#!/usr/bin/env python3
"""Build wheels from a caller's tagged native release, or verify them on PyPI."""

import argparse
import base64
import csv
import hashlib
import io
import json
import os
import re
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.request
import venv
import zipfile
from pathlib import Path, PurePosixPath

PLATFORMS = {
    "darwin-arm64": "macosx_11_0_arm64",
    "darwin-x64": "macosx_10_12_x86_64",
    "linux-x64-gnu": "manylinux_2_17_x86_64.manylinux2014_x86_64",
    "linux-arm64-gnu": "manylinux_2_17_aarch64.manylinux2014_aarch64",
    "win32-x64-msvc": "win_amd64",
}
VERIFY_DELAYS = (0, 5, 10, 20, 30)
SHA256SUMS = "SHA256SUMS"
PACKAGE_NAME = re.compile(r"[A-Za-z0-9](?:[A-Za-z0-9._-]*[A-Za-z0-9])?")


def input_value(name, default=""):
    return os.environ.get(f"WHEELS_{name.upper().replace('-', '_')}", "") or default


def source_path(value):
    path = PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts or not path.parts:
        raise ValueError(f"expected a relative source path: {value!r}")
    return path.as_posix()


def version_from(contents):
    version = json.loads(contents)["version"]
    # The Git tag and PyPI JSON key must refer to the same canonical version.
    number = r"(?:0|[1-9][0-9]*)"
    if not isinstance(version, str) or not re.fullmatch(
        rf"{number}\.{number}\.{number}(?:(?:a|b|rc){number})?(?:\.post{number})?(?:\.dev{number})?",
        version,
    ):
        raise ValueError(f"package.json version is not a canonical PyPI version: {version!r}")
    return version


def normalized_name(name):
    return re.sub(r"[-_.]+", "_", name).lower()


def wheel_stem(name, version):
    return f"{normalized_name(name)}-{version}"


def wheel_name(name, version, tag):
    return f"{wheel_stem(name, version)}-py3-none-{tag}.whl"


def pypi_state(name, version):
    """Return missing and yanked filenames for the exact version, not latest."""
    expected = {wheel_name(name, version, tag) for tag in PLATFORMS.values()}
    missing = expected.copy()
    yanked = set()
    request = urllib.request.Request(f"https://pypi.org/pypi/{name}/json", headers={"Cache-Control": "no-cache"})
    try:
        with urllib.request.urlopen(request, timeout=15) as response:
            files = json.load(response)["releases"].get(version, [])
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return missing, yanked
        raise
    if not isinstance(files, list):
        raise ValueError("invalid PyPI release file list")
    seen = set()
    for file in files:
        filename = file["filename"]
        if filename not in expected:
            continue
        if (filename in seen or file["packagetype"] != "bdist_wheel"
                or type(file["yanked"]) is not bool
                or not re.fullmatch(r"[0-9a-f]{64}", file["digests"]["sha256"])):
            raise ValueError(f"invalid PyPI wheel entry: {filename}")
        seen.add(filename)
        if file["yanked"]:
            yanked.add(filename)
        else:
            missing.remove(filename)
    return missing, yanked


def verify_pypi(name, version):
    problem = ""
    for delay in VERIFY_DELAYS:
        if delay:
            time.sleep(delay)
        try:
            missing, _ = pypi_state(name, version)
        except urllib.error.HTTPError as error:
            if error.code != 429 and not 500 <= error.code < 600:
                raise
            problem = str(error)
        except (urllib.error.URLError, TimeoutError) as error:
            problem = str(error)
        else:
            if not missing:
                print(f"Verified all five wheels for {name} {version} on PyPI.")
                return
            problem = "missing or yanked: " + ", ".join(sorted(missing))
    raise ValueError(f"PyPI did not confirm all five wheels for {name} {version}: {problem}")


def release_assets(repo, tag):
    release = json.loads(subprocess.check_output(["gh", "api", f"repos/{repo}/releases/tags/{tag}"]))
    if release["tag_name"] != tag or release["draft"] is not False:
        raise ValueError(f"wrong or draft GitHub release: {tag}")
    assets = {}
    for asset in release["assets"]:
        if asset["name"] in assets:
            raise ValueError(f"duplicate release asset: {asset['name']}")
        assets[asset["name"]] = asset
    return assets


def download_asset(repo, asset, path):
    asset_id = asset["id"]
    if type(asset_id) is not int or asset_id <= 0 or asset["state"] != "uploaded":
        raise ValueError(f"release asset is not uploaded: {path.name}")
    with path.open("wb") as output:
        subprocess.run(["gh", "api", f"repos/{repo}/releases/assets/{asset_id}",
                        "-H", "Accept: application/octet-stream"], stdout=output, check=True)
    with path.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    if asset.get("digest") is not None and asset["digest"] != f"sha256:{digest}":
        raise ValueError(f"GitHub asset digest mismatch: {path.name}")
    if path.stat().st_size != asset["size"] or not asset["size"]:
        raise ValueError(f"GitHub asset size mismatch: {path.name}")
    return digest


def parse_checksums(path):
    sums = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        match = re.fullmatch(r"([a-fA-F0-9]{64}) [ *](.+)", line)
        if not match or match[2] in sums:
            raise ValueError("invalid or duplicate entry in SHA256SUMS")
        sums[match[2]] = match[1].lower()
    return sums


def native_archives(repo, name, version, work, require_sums, workflow, commit):
    tag = f"v{version}"
    assets = release_assets(repo, tag)
    if require_sums and SHA256SUMS not in assets:
        raise ValueError(f"{tag} is missing SHA256SUMS")
    sums = None
    if SHA256SUMS in assets:
        manifest = work / SHA256SUMS
        download_asset(repo, assets[SHA256SUMS], manifest)
        sums = parse_checksums(manifest)
    for platform, wheel_tag in PLATFORMS.items():
        windows = platform.startswith("win32-")
        exe = f"{name}.exe" if windows else name
        path = work / f"{name}-{platform}.{'zip' if windows else 'tar.gz'}"
        if path.name not in assets:
            raise ValueError(f"{tag} is missing {path.name}")
        digest = download_asset(repo, assets[path.name], path)
        if sums is not None and sums.get(path.name) != digest:
            raise ValueError(f"SHA256SUMS mismatch or missing entry: {path.name}")
        if workflow:
            subprocess.run(["gh", "attestation", "verify", str(path), "--repo", repo,
                            "--signer-workflow", f"{repo}/{workflow}", "--source-digest", commit], check=True)
        yield wheel_tag, exe, path


def read_binary(archive, exe):
    """Read just the regular root executable; never extract archive paths."""
    names = (exe, f"./{exe}")
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as bundle:
            members = [member for member in bundle.infolist() if member.filename in names]
            if (len(members) != 1 or members[0].is_dir()
                    or stat.S_IFMT(members[0].external_attr >> 16) not in (0, stat.S_IFREG)):
                raise ValueError(f"expected one regular {exe} in {archive.name}")
            content = bundle.read(members[0])
    else:
        with tarfile.open(archive, "r:gz") as bundle:
            members = [member for member in bundle if member.name in names]
            if len(members) != 1 or not members[0].isfile():
                raise ValueError(f"expected one regular {exe} in {archive.name}")
            content = bundle.extractfile(members[0]).read()
    if not content:
        raise ValueError(f"empty executable in {archive.name}")
    return content


def tagged_source(repo, version, paths, work):
    """Use the exact tag, not release.target_commitish or files from main."""
    subprocess.run(["git", "fetch", "--no-tags", "--depth=1", f"https://github.com/{repo}.git",
                    f"refs/tags/v{version}"], check=True)
    commit = subprocess.check_output(["git", "rev-parse", "--verify", "FETCH_HEAD^{commit}"], text=True).strip()
    with (work / "source.tar").open("wb+") as archive:
        subprocess.run(["git", "--literal-pathspecs", "archive", commit, "--", *dict.fromkeys(paths)],
                       stdout=archive, check=True)
        archive.seek(0)
        with tarfile.open(fileobj=archive) as tar:
            files = {member.name: tar.extractfile(member).read() for member in tar if member.isfile()}
    return commit, files


def metadata(name, version, readme):
    headers = []

    def field(key, value):
        # Headers cannot introduce another field, a body separator, or a control character.
        if not value or any(ord(ch) < 32 or ord(ch) == 127 for ch in value):
            raise ValueError(f"missing or invalid PyPI metadata field: {key}")
        headers.append(f"{key}: {value}\n")

    field("Metadata-Version", "2.4")
    field("Name", name)
    field("Version", version)
    field("Summary", input_value("summary"))
    field("Project-URL", "Homepage, " + input_value("homepage"))
    field("Project-URL", "Source, " + input_value("source-url"))
    if not input_value("homepage") or not input_value("source-url"):
        raise ValueError("build requires homepage and source-url")
    license_expression = input_value("license-expression")
    if license_expression:
        field("License-Expression", license_expression)
    field("License-File", "LICENSE")
    field("Requires-Python", ">=3.8")
    if input_value("keywords"):
        field("Keywords", input_value("keywords"))
    for classifier in input_value("classifiers").splitlines():
        if classifier.strip():
            field("Classifier", classifier.strip())
    extras = set()
    for line in input_value("extras").splitlines():
        if not line.strip():
            continue
        extra, _, requirement = line.strip().partition("=")
        extra, requirement = extra.strip(), requirement.strip()
        if not re.fullmatch(r"[a-z0-9]+(?:-[a-z0-9]+)*", extra) or not requirement or ";" in requirement:
            raise ValueError(f"expected extra=requirement (no environment marker): {line!r}")
        if extra not in extras:
            field("Provides-Extra", extra)
            extras.add(extra)
        field("Requires-Dist", f'{requirement}; extra == "{extra}"')
    field("Description-Content-Type", "text/markdown")
    return ("".join(headers) + "\n" + readme.decode("utf-8")).encode("utf-8")


def package_files(files, directory, module):
    prefix = f"{directory}/{module}/"
    package = {path.removeprefix(f"{directory}/"): data for path, data in files.items()
               if path.startswith(prefix) and (path.endswith(".py") or path == f"{prefix}py.typed")}
    if f"{module}/__init__.py" not in package:
        raise ValueError(f"tagged release is missing {prefix}__init__.py")
    package.setdefault(f"{module}/py.typed", b"")
    return package


def write_wheel(out, name, version, tag, exe, binary, metadata_bytes, license_bytes, package):
    stem = wheel_stem(name, version)
    info = f"{stem}.dist-info"
    script = f"{stem}.data/scripts/{exe}"
    tags = "".join(f"Tag: py3-none-{platform}\n" for platform in tag.split("."))
    files = {
        script: binary,
        f"{info}/METADATA": metadata_bytes,
        f"{info}/WHEEL": ("Wheel-Version: 1.0\nGenerator: mcp-kit\nRoot-Is-Purelib: false\n" + tags).encode(),
        f"{info}/licenses/LICENSE": license_bytes,
        **package,
    }
    record = io.StringIO(newline="")
    rows = csv.writer(record, lineterminator="\n")
    path = out / wheel_name(name, version, tag)
    with zipfile.ZipFile(path, "w") as wheel:
        def add(filename, data, mode):
            entry = zipfile.ZipInfo(filename, date_time=(2020, 1, 1, 0, 0, 0))
            entry.create_system = 3  # Preserve Unix executable bits regardless of the build host.
            entry.external_attr = (stat.S_IFREG | mode) << 16
            wheel.writestr(entry, data, compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)

        for filename, data in sorted(files.items()):
            add(filename, data, 0o755 if filename == script else 0o644)
            digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode()
            rows.writerow((filename, f"sha256={digest}", len(data)))
        rows.writerow((f"{info}/RECORD", "", ""))
        add(f"{info}/RECORD", record.getvalue().encode(), 0o644)
    return path


def smoke_wheels(work, paths, linux_wheel, name, version, command):
    smoke = work / "venv"
    venv.create(smoke, with_pip=True)
    env = {key: value for key, value in os.environ.items()
           if key not in {"GH_TOKEN", "GITHUB_TOKEN", "ACTIONS_ID_TOKEN_REQUEST_TOKEN", "ACTIONS_ID_TOKEN_REQUEST_URL"}}
    env.update(PATH=f"{smoke / 'bin'}{os.pathsep}{os.environ.get('PATH', os.defpath)}",
               VIRTUAL_ENV=str(smoke), WHEEL_VERSION=version)
    python = str(smoke / "bin/python")
    subprocess.run([python, "-m", "pip", "install", "--no-index", "--no-deps", str(linux_wheel)], env=env, check=True)
    if not os.access(smoke / "bin" / name, os.X_OK):
        raise ValueError(f"installed wheel lacks executable: {name}")
    subprocess.run(["bash", "-euo", "pipefail", "-c", command], env=env, check=True)
    subprocess.run([python, "-m", "pip", "install", "--quiet", "--only-binary=:all:", "twine==6.2.0"], env=env, check=True)
    subprocess.run([python, "-m", "twine", "check", "--strict", *map(str, paths)], env=env, check=True)


def build(name, pypi_name, version, package_json):
    missing, yanked = pypi_state(pypi_name, version)
    if not missing:
        print(f"All five wheels for {pypi_name} {version} already exist on PyPI.")
        return False
    if yanked:
        raise ValueError("PyPI filenames cannot be reused; unyank or release a new version: " + ", ".join(sorted(yanked)))
    repo = os.environ["GITHUB_REPOSITORY"]
    if not re.fullmatch(r"[A-Za-z0-9-]+/[A-Za-z0-9_.-]+", repo) or repo.split("/")[1] in {".", ".."}:
        raise ValueError("invalid GITHUB_REPOSITORY")
    require_sums = input_value("require-sha256sums", "false")
    if require_sums not in ("true", "false"):
        raise ValueError("require-sha256sums must be true or false")
    workflow = input_value("attestation-workflow")
    if workflow and not re.fullmatch(r"\.github/workflows/[A-Za-z0-9_.-]+\.ya?ml", workflow):
        raise ValueError("attestation-workflow must be a caller .github/workflows/*.yml path")
    readme = source_path(input_value("readme", "README.md"))
    license_file = source_path(input_value("license-file", "LICENSE"))
    directory = input_value("python-package-dir")
    if directory:
        directory = source_path(directory)
    out = Path(input_value("out-dir", "dist"))
    if out.is_symlink() or (out.exists() and (not out.is_dir() or any(out.iterdir()))):
        raise ValueError(f"output directory must be empty: {out}")
    paths = [package_json, readme, license_file] + ([directory] if directory else [])
    with tempfile.TemporaryDirectory(prefix="pypi-wheels-") as temp:
        work = Path(temp)
        commit, files = tagged_source(repo, version, paths, work)
        if version_from(files[package_json]) != version:
            raise ValueError("tagged package.json version does not match caller version")
        print(f"Packaging {repo}@{commit} (v{version}).", flush=True)
        metadata_bytes = metadata(pypi_name, version, files[readme])
        package = package_files(files, directory, normalized_name(name)) if directory else {}
        wheels = [write_wheel(work, pypi_name, version, tag, exe, read_binary(path, exe),
                              metadata_bytes, files[license_file], package)
                  for tag, exe, path in native_archives(repo, name, version, work, require_sums == "true", workflow, commit)]
        linux_wheel = work / wheel_name(pypi_name, version, PLATFORMS["linux-x64-gnu"])
        smoke_wheels(work, wheels, linux_wheel, name, version, input_value("smoke-command", f"{name} --version"))
        out.mkdir(parents=True, exist_ok=True)
        # Exclusive writes avoid overwriting an existing caller file.
        for wheel in wheels:
            with (out / wheel.name).open("xb") as target:
                target.write(wheel.read_bytes())
    return True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("build", "verify"))
    args = parser.parse_args()
    try:
        name = input_value("name")
        pypi_name = input_value("pypi-name", name)
        if not PACKAGE_NAME.fullmatch(name):
            raise ValueError("invalid executable name")
        if not PACKAGE_NAME.fullmatch(pypi_name):
            raise ValueError("invalid PyPI distribution name")
        package_json = source_path(input_value("package-json"))
        version = version_from(Path(package_json).read_bytes())
        outputs = {"version": version}
        if args.mode == "build":
            outputs["built"] = str(build(name, pypi_name, version, package_json)).lower()
        else:
            verify_pypi(pypi_name, version)
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
            output.writelines(f"{key}={value}\n" for key, value in outputs.items())
    except (OSError, ValueError, KeyError, TypeError, tarfile.TarError, zipfile.BadZipFile, subprocess.SubprocessError) as error:
        print(f"PyPI wheels: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
