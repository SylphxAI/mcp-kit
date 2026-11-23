"""Offline wheel, recovery, trust, and registry checks."""

import base64
import csv
import hashlib
import importlib.util
import io
import json
import os
import stat
import subprocess
import sys
import tarfile
import tempfile
import unittest
import urllib.error
import zipfile
from email import policy
from email.parser import BytesParser
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("pypi_wheels", Path(__file__).with_name("pypi-wheels.py"))
wheels = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(wheels)

NAME = "sample"
VERSION = "1.2.3"
REPO = "example/sample"
COMMIT = "b" * 40
PACKAGE_JSON = "packages/sample/package.json"
NATIVE_FILES = {
    "sample-darwin-arm64.tar.gz": "sample-1.2.3-py3-none-macosx_11_0_arm64.whl",
    "sample-darwin-x64.tar.gz": "sample-1.2.3-py3-none-macosx_10_12_x86_64.whl",
    "sample-linux-x64-gnu.tar.gz": "sample-1.2.3-py3-none-manylinux_2_17_x86_64.manylinux2014_x86_64.whl",
    "sample-linux-arm64-gnu.tar.gz": "sample-1.2.3-py3-none-manylinux_2_17_aarch64.manylinux2014_aarch64.whl",
    "sample-win32-x64-msvc.zip": "sample-1.2.3-py3-none-win_amd64.whl",
}
EXPECTED = set(NATIVE_FILES.values())
LINUX_X64 = NATIVE_FILES["sample-linux-x64-gnu.tar.gz"]


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def zip_bytes(exe, binary, mode=stat.S_IFREG | 0o644):
    stream = io.BytesIO()
    with zipfile.ZipFile(stream, "w") as archive:
        member = zipfile.ZipInfo(exe)
        member.external_attr = mode << 16
        archive.writestr(member, binary)
    return stream.getvalue()


def tar_bytes(exe, binary, member_type=tarfile.REGTYPE):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w:gz") as archive:
        member = tarfile.TarInfo(exe)
        member.type = member_type
        member.size = len(binary)
        archive.addfile(member, io.BytesIO(binary))
    return stream.getvalue()


def registry(files):
    return io.BytesIO(json.dumps({"info": {"version": "9.0.0"}, "releases": {VERSION: files}}).encode())


def wheel_entry(filename, yanked=False):
    return {"filename": filename, "packagetype": "bdist_wheel", "yanked": yanked,
            "digests": {"sha256": sha256(filename.encode())}}


class BuildTest(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.out = self.root / "dist"
        self.source = {PACKAGE_JSON: b'{"version":"1.2.3"}', "README.md": b"# Tagged README\n", "LICENSE": b"License text"}
        self.assets = {
            name: (zip_bytes("sample.exe", name.encode()) if name.endswith(".zip") else tar_bytes("./sample", name.encode()))
            for name in NATIVE_FILES
        }
        self.ids = {name: idx for idx, name in enumerate([*NATIVE_FILES, "SHA256SUMS"], 1)}
        self.sums = "".join(f"{sha256(data)}  {name}\n" for name, data in self.assets.items())
        self.repo_env = {"GITHUB_REPOSITORY": REPO, "WHEELS_OUT_DIR": str(self.out),
                         "WHEELS_SUMMARY": "Fixture CLI", "WHEELS_HOMEPAGE": "https://example.com",
                         "WHEELS_SOURCE_URL": "https://github.com/example/sample", "WHEELS_LICENSE_EXPRESSION": "MIT"}
        self.env = patch.dict(os.environ, self.repo_env, clear=True)
        self.env.start()
        self.addCleanup(self.env.stop)
        self.api = self.use_patch(wheels.subprocess, "check_output", side_effect=self.release_json)
        self.commands = self.use_patch(wheels.subprocess, "run", side_effect=self.download)
        self.tagged = self.use_patch(wheels, "tagged_source", return_value=(COMMIT, self.source))
        self.registry = self.use_patch(wheels.urllib.request, "urlopen", side_effect=self.not_found)
        self.smoke = self.use_patch(wheels, "smoke_wheels", side_effect=self.check_smoke_input)

    def use_patch(self, owner, attribute, **kwargs):
        mock = patch.object(owner, attribute, **kwargs)
        self.addCleanup(mock.stop)
        return mock.start()

    def release_json(self, args):
        self.assertEqual(args, ["gh", "api", f"repos/{REPO}/releases/tags/v{VERSION}"])
        return json.dumps({"tag_name": "v1.2.3", "draft": False, "assets": [
            {"id": self.ids[name], "name": name, "digest": f"sha256:{sha256(data)}",
             "size": len(data), "state": "uploaded"} for name, data in self.assets.items()
        ]})

    def download(self, args, **kwargs):
        if args[1:3] == ["attestation", "verify"]:
            self.assertEqual(args[4:], ["--repo", REPO, "--signer-workflow",
                                         f"{REPO}/.github/workflows/release.yml", "--source-digest", COMMIT])
            return
        name = kwargs["stdout"].name.rsplit(os.sep, 1)[-1]
        self.assertEqual(args, ["gh", "api", f"repos/{REPO}/releases/assets/{self.ids[name]}",
                                "-H", "Accept: application/octet-stream"])
        self.assertTrue(kwargs["check"])
        kwargs["stdout"].write(self.assets[name])

    def not_found(self, request, timeout):
        self.assertEqual(request.full_url, "https://pypi.org/pypi/sample/json")
        self.assertGreater(timeout, 0)
        raise urllib.error.HTTPError(request.full_url, 404, "not found", {}, None)

    def check_smoke_input(self, work, paths, linux_wheel, name, version, command):
        self.assertEqual((name, version), (NAME, VERSION))
        self.assertEqual({p.name for p in paths}, EXPECTED)
        self.assertTrue(all(p.is_file() for p in paths))
        self.assertEqual(linux_wheel, work / LINUX_X64)
        self.assertEqual(command, os.environ.get("WHEELS_SMOKE_COMMAND", "sample --version"))
        self.assertFalse(self.out.exists(), "publish directory must not be ready before smoke")

    def build(self):
        return wheels.build(NAME, NAME, VERSION, PACKAGE_JSON)

    def test_distribution_alias_keeps_native_archive_executable_and_import_names(self):
        self.source["python/sample/__init__.py"] = b"# import sample"
        expected = {filename.replace("sample-", "vendor_sample-", 1) for filename in EXPECTED}
        linux_wheel = LINUX_X64.replace("sample-", "vendor_sample-", 1)
        self.registry.side_effect = None
        self.registry.return_value = registry([wheel_entry(linux_wheel)])
        result = self.root / "outputs"
        real_read_bytes = Path.read_bytes
        with patch.dict(os.environ, {"WHEELS_NAME": NAME, "WHEELS_PYPI_NAME": "vendor-sample",
                                     "WHEELS_PACKAGE_JSON": PACKAGE_JSON, "WHEELS_PYTHON_PACKAGE_DIR": "python",
                                     "GITHUB_OUTPUT": str(result)}), \
             patch.object(sys, "argv", ["script", "build"]), \
             patch.object(Path, "read_bytes", autospec=True,
                          side_effect=lambda path: self.source[PACKAGE_JSON] if path == Path(PACKAGE_JSON) else real_read_bytes(path)):
            self.smoke.side_effect = None
            self.assertEqual(wheels.main(), 0)
        self.assertEqual(result.read_text(encoding="utf-8"), "version=1.2.3\nbuilt=true\n")
        self.assertEqual(self.registry.call_args.args[0].full_url, "https://pypi.org/pypi/vendor-sample/json")
        work, paths, linux, name, version, command = self.smoke.call_args.args
        self.assertEqual({p.name for p in paths}, expected)
        self.assertEqual((linux, name, version, command), (work / linux_wheel, NAME, VERSION, "sample --version"))
        self.assertEqual({p.name for p in self.out.iterdir()}, expected)
        for archive, original in NATIVE_FILES.items():
            filename = original.replace("sample-", "vendor_sample-", 1)
            with self.subTest(filename=filename), zipfile.ZipFile(self.out / filename) as wheel:
                exe = "sample.exe" if filename.endswith("win_amd64.whl") else "sample"
                self.assertEqual(wheel.read(f"vendor_sample-1.2.3.data/scripts/{exe}"), archive.encode())
                meta = BytesParser().parsebytes(wheel.read("vendor_sample-1.2.3.dist-info/METADATA"))
                self.assertEqual(meta["Name"], "vendor-sample")
                self.assertIn("sample/__init__.py", wheel.namelist())

    def test_cli_only_reproducible_wheels(self):
        self.assertTrue(self.build())
        self.tagged.assert_called_once()
        self.assertEqual(self.tagged.call_args.args[:3], (REPO, VERSION, [PACKAGE_JSON, "README.md", "LICENSE"]))
        self.smoke.assert_called_once()
        self.assertEqual({p.name for p in self.out.iterdir()}, EXPECTED)
        for archive, filename in NATIVE_FILES.items():
            with self.subTest(filename=filename), zipfile.ZipFile(self.out / filename) as wheel:
                exe = "sample.exe" if filename.endswith("win_amd64.whl") else "sample"
                script = f"sample-1.2.3.data/scripts/{exe}"
                info = "sample-1.2.3.dist-info"
                record = f"{info}/RECORD"
                self.assertEqual(wheel.read(script), archive.encode())
                self.assertEqual(wheel.read(f"{info}/licenses/LICENSE"), b"License text")
                self.assertCountEqual(wheel.namelist(), [script, record, f"{info}/METADATA", f"{info}/WHEEL",
                                                         f"{info}/licenses/LICENSE"])
                for entry in wheel.infolist():
                    self.assertEqual(entry.date_time, (2020, 1, 1, 0, 0, 0))
                    self.assertEqual(entry.create_system, 3)
                    self.assertEqual((entry.external_attr >> 16) & 0o777, 0o755 if entry.filename == script else 0o644)
                meta = BytesParser(policy=policy.default).parsebytes(wheel.read(f"{info}/METADATA"))
                self.assertEqual(meta["Metadata-Version"], "2.4")
                self.assertEqual(meta["License-Expression"], "MIT")
                self.assertEqual(meta["License-File"], "LICENSE")
                self.assertEqual(meta["Description-Content-Type"], "text/markdown")
                self.assertEqual(meta.get_payload(), "# Tagged README\n")
                header = BytesParser().parsebytes(wheel.read(f"{info}/WHEEL"))
                self.assertEqual(header["Root-Is-Purelib"], "false")
                self.assertEqual(header.get_all("Tag"), [f"py3-none-{tag}" for tag in filename[:-4].split("-none-")[1].split(".")])
                rows = list(csv.reader(io.StringIO(wheel.read(record).decode())))
                self.assertCountEqual([path for path, _, _ in rows], wheel.namelist())
                for path, digest, size in rows:
                    if path == record:
                        self.assertEqual((digest, size), ("", ""))
                    else:
                        algorithm, encoded = digest.split("=", 1)
                        self.assertEqual(algorithm, "sha256")
                        self.assertNotIn("=", encoded)
                        decoded = base64.urlsafe_b64decode(encoded + "=").hex()
                        self.assertEqual((decoded, int(size)), (sha256(wheel.read(path)), len(wheel.read(path))))
        initial = {p.name: sha256(p.read_bytes()) for p in self.out.iterdir()}
        self.out = self.root / "dist-again"
        os.environ["WHEELS_OUT_DIR"] = str(self.out)
        self.assertTrue(self.build())
        self.assertEqual({p.name: sha256(p.read_bytes()) for p in self.out.iterdir()}, initial)

    def test_api_extras_and_tagged_attestation(self):
        self.source.update({"python/sample/__init__.py": b"from .sub import api\n", "python/sample/sub/api.py": b"tagged API",
                            "python/sample/py.typed": b"partial\n", "python/sample/cache.pyc": b"ignore",
                            "python/tests/test_api.py": b"ignore"})
        self.assets["SHA256SUMS"] = self.sums.encode()
        with patch.dict(os.environ, {"WHEELS_PYTHON_PACKAGE_DIR": "python", "WHEELS_REQUIRE_SHA256SUMS": "true",
                                     "WHEELS_ATTESTATION_WORKFLOW": ".github/workflows/release.yml",
                                     "WHEELS_CLASSIFIERS": "Programming Language :: Python\nTopic :: Utilities",
                                     "WHEELS_EXTRAS": "integration=backend>=0.3,<2\nintegration=common==1.0\nother=helper",
                                     "WHEELS_KEYWORDS": "cli,native", "WHEELS_SMOKE_COMMAND": "sample version; python -c 'import sample'"}):
            self.assertTrue(self.build())
        self.assertEqual(sum(c.args[0][1:3] == ["attestation", "verify"] for c in self.commands.call_args_list), 5)
        with zipfile.ZipFile(self.out / LINUX_X64) as wheel:
            self.assertEqual(wheel.read("sample/sub/api.py"), b"tagged API")
            self.assertEqual(wheel.read("sample/py.typed"), b"partial\n")
            self.assertNotIn("sample/cache.pyc", wheel.namelist())
            self.assertNotIn("tests/test_api.py", wheel.namelist())
            meta = BytesParser().parsebytes(wheel.read("sample-1.2.3.dist-info/METADATA"))
            self.assertEqual(meta["Keywords"], "cli,native")
            self.assertEqual(meta.get_all("Classifier"), ["Programming Language :: Python", "Topic :: Utilities"])
            self.assertEqual(meta.get_all("Provides-Extra"), ["integration", "other"])
            self.assertEqual(meta.get_all("Requires-Dist"), ['backend>=0.3,<2; extra == "integration"',
                                                            'common==1.0; extra == "integration"', 'helper; extra == "other"'])

    def test_complete_version_skips_with_no_release_or_outputs(self):
        self.registry.side_effect = None
        self.registry.return_value = registry([wheel_entry(filename) for filename in EXPECTED])
        self.assertFalse(self.build())
        self.tagged.assert_not_called()
        self.api.assert_not_called()
        self.smoke.assert_not_called()
        self.assertFalse(self.out.exists())

    def test_partial_recovery_produces_full_set_for_skip_existing_publish(self):
        self.registry.side_effect = None
        self.registry.return_value = registry([wheel_entry(LINUX_X64)])
        self.assertTrue(self.build())
        self.assertEqual({p.name for p in self.out.iterdir()}, EXPECTED)

    def test_yanked_filename_refuses_without_building(self):
        self.registry.side_effect = None
        self.registry.return_value = registry([wheel_entry(LINUX_X64, yanked=True)])
        with self.assertRaisesRegex(ValueError, "unyank or release a new version"):
            self.build()
        self.tagged.assert_not_called()
        self.smoke.assert_not_called()

    def test_absent_required_checksums(self):
        with patch.dict(os.environ, {"WHEELS_REQUIRE_SHA256SUMS": "true"}), \
             self.assertRaisesRegex(ValueError, "missing SHA256SUMS"):
            self.build()
        self.commands.assert_not_called()
        self.smoke.assert_not_called()

    def test_present_manifest_always_checked(self):
        corrupt = self.sums.replace(sha256(self.assets["sample-win32-x64-msvc.zip"]), "f" * 64)
        truncated = "\n".join(corrupt.splitlines()[:-1])
        for content in (corrupt, truncated):
            with self.subTest(content=content):
                self.assets["SHA256SUMS"] = content.encode()
                with self.assertRaisesRegex(ValueError, "SHA256SUMS mismatch or missing entry.*win32"):
                    self.build()
        self.smoke.assert_not_called()
        self.assertFalse(self.out.exists())

    def test_missing_native_release_asset_refuses(self):
        del self.assets["sample-win32-x64-msvc.zip"]
        with self.assertRaisesRegex(ValueError, "missing sample-win32-x64-msvc.zip"):
            self.build()
        self.smoke.assert_not_called()
        self.assertFalse(self.out.exists())

    def test_download_changed_since_api_read_refuses(self):
        original = self.release_json(["gh", "api", f"repos/{REPO}/releases/tags/v{VERSION}"])
        self.api.side_effect = None
        self.api.return_value = original
        self.assets["sample-win32-x64-msvc.zip"] = b"not the released bytes"
        with self.assertRaisesRegex(ValueError, "GitHub asset digest mismatch.*win32"):
            self.build()
        self.smoke.assert_not_called()

    def test_smoke_failure_leaves_no_upload_set(self):
        self.smoke.side_effect = subprocess.CalledProcessError(1, "intentional smoke failure")
        with self.assertRaises(subprocess.CalledProcessError):
            self.build()
        self.assertFalse(self.out.exists())

    def test_output_from_existing_run_is_not_reused(self):
        self.out.mkdir()
        stale = self.out / LINUX_X64
        stale.write_bytes(b"existing data")
        with self.assertRaisesRegex(ValueError, "output directory must be empty"):
            self.build()
        self.assertEqual(stale.read_bytes(), b"existing data")
        self.tagged.assert_not_called()

    def test_rejects_bad_tagged_source_or_missing_python_api(self):
        with patch.dict(os.environ, {"WHEELS_PYTHON_PACKAGE_DIR": "python"}):
            with self.assertRaisesRegex(ValueError, "tagged release is missing python/sample/__init__.py"):
                self.build()
        self.source[PACKAGE_JSON] = b'{"version":"1.0.0"}'
        with self.assertRaisesRegex(ValueError, "tagged package.json version does not match"):
            self.build()
        self.api.assert_not_called()


class ArchiveTest(unittest.TestCase):
    def test_ignores_non_root_names_and_refuses_link_or_empty_binary(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for filename, exe, data in [
                ("bad.tar.gz", "sample", tar_bytes("../sample", b"other")),
                ("bad.tar.gz", "sample", tar_bytes("sample", b"", tarfile.SYMTYPE)),
                ("bad.zip", "sample.exe", zip_bytes("sample.exe", b"target", stat.S_IFLNK | 0o777)),
                ("bad.zip", "sample.exe", zip_bytes("sample.exe", b"")),
            ]:
                with self.subTest(filename=filename, data=data[:30]):
                    archive = root / filename
                    archive.write_bytes(data)
                    with self.assertRaises(ValueError):
                        wheels.read_binary(archive, exe)


class VerifyTest(unittest.TestCase):
    def test_passes_after_propagation_and_retryable_error(self):
        error = urllib.error.HTTPError("https://pypi.org", 503, "temporarily unavailable", {}, None)
        with patch.object(wheels.urllib.request, "urlopen", side_effect=[
            registry([wheel_entry(LINUX_X64)]), error, registry([wheel_entry(name) for name in EXPECTED])
        ]) as probe, patch.object(wheels.time, "sleep") as sleep:
            wheels.verify_pypi(NAME, VERSION)
        self.assertEqual(probe.call_count, 3)
        self.assertEqual([call.args[0] for call in sleep.call_args_list], [5, 10])

    def test_fails_after_bounded_wait_when_yanked(self):
        files = [wheel_entry(name, yanked=name == LINUX_X64) for name in EXPECTED]
        with patch.object(wheels.urllib.request, "urlopen", side_effect=lambda *a, **k: registry(files)) as probe, \
             patch.object(wheels.time, "sleep"), self.assertRaisesRegex(ValueError, "missing or yanked.*x86_64"):
            wheels.verify_pypi(NAME, VERSION)
        self.assertEqual(probe.call_count, 5)

    def test_malformed_registry_wheel_fails_immediately(self):
        entry = wheel_entry(LINUX_X64) | {"digests": {"sha256": "invalid"}}
        with patch.object(wheels.urllib.request, "urlopen", return_value=registry([entry])), \
             patch.object(wheels.time, "sleep") as sleep, self.assertRaisesRegex(ValueError, "invalid PyPI wheel entry"):
            wheels.verify_pypi(NAME, VERSION)
        sleep.assert_not_called()

    def test_verify_outputs_version_with_default_or_aliased_distribution(self):
        for alias, dist, prefix in ((None, "sample", "sample"), ("vendor-sample", "vendor-sample", "vendor_sample")):
            with self.subTest(alias=alias), tempfile.TemporaryDirectory() as temp:
                package_json = Path(temp, "package.json")
                package_json.write_text('{"version":"1.2.3"}', encoding="utf-8")
                result = Path(temp, "outputs")
                env = {"WHEELS_NAME": NAME, "WHEELS_PACKAGE_JSON": "package.json", "GITHUB_OUTPUT": str(result)}
                if alias is not None:
                    env["WHEELS_PYPI_NAME"] = alias
                files = [wheel_entry(name.replace("sample-", prefix + "-", 1)) for name in EXPECTED]
                with patch.dict(os.environ, env, clear=True), patch.object(sys, "argv", ["script", "verify"]), \
                     patch.object(Path, "read_bytes", return_value=package_json.read_bytes()), \
                     patch.object(wheels.urllib.request, "urlopen", return_value=registry(files)) as probe:
                    self.assertEqual(wheels.main(), 0)
                probe.assert_called_once()
                self.assertEqual(probe.call_args.args[0].full_url, f"https://pypi.org/pypi/{dist}/json")
                self.assertEqual(result.read_text(encoding="utf-8"), "version=1.2.3\n")

    def test_invalid_alias_is_rejected_before_registry_or_release_io(self):
        env = {"WHEELS_NAME": NAME, "WHEELS_PYPI_NAME": "../wrong", "WHEELS_PACKAGE_JSON": PACKAGE_JSON}
        with patch.dict(os.environ, env, clear=True), patch.object(sys, "argv", ["script", "build"]), \
             patch.object(sys, "stderr", new_callable=io.StringIO) as stderr, \
             patch.object(wheels.urllib.request, "urlopen") as probe:
            self.assertEqual(wheels.main(), 1)
        probe.assert_not_called()
        self.assertIn("invalid PyPI distribution name", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
