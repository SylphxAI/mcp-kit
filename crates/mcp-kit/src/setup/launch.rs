use super::Server;
use std::path::{Path, PathBuf};

const EXTS: &[&str] = if cfg!(windows) {
    &[".exe", ".cmd", ".bat", ""]
} else {
    &[""]
};

pub(super) fn path_entries() -> Vec<PathBuf> {
    std::env::var_os("PATH").map_or_else(Vec::new, |path| std::env::split_paths(&path).collect())
}

pub(super) fn uv_cache() -> Option<PathBuf> {
    let path = std::env::var_os("UV_CACHE_DIR").filter(|p| !p.is_empty())?;
    let absolute = std::path::absolute(path).ok()?;
    Some(absolute.canonicalize().unwrap_or(absolute))
}

pub(super) fn candidates<'a>(
    bin: &'a str,
    dirs: &'a [PathBuf],
) -> impl Iterator<Item = PathBuf> + 'a {
    dirs.iter()
        .flat_map(move |dir| EXTS.iter().map(move |ext| dir.join(format!("{bin}{ext}"))))
}

#[derive(PartialEq, Eq, Debug)]
enum Origin {
    Native,
    Npm,
    NpmCache,
    UvCache,
}

fn origin(path: &Path, cache: Option<&Path>) -> Origin {
    // Normalize both separators and case on Windows. Match directory names, not substrings.
    let spelling = path.to_string_lossy();
    let spelling = if cfg!(windows) {
        spelling.to_ascii_lowercase().replace('\\', "/").into()
    } else {
        spelling
    };
    let mut dirs = spelling.rsplit('/').skip(1);
    let mut child = "";
    let mut found = Origin::Native;
    for dir in &mut dirs {
        match dir {
            "_npx" | "npm-cache" | ".npm" => return Origin::NpmCache,
            "node_modules" => found = Origin::Npm,
            "uv" if child == "cache" && found != Origin::Npm => found = Origin::UvCache,
            // uv puts temporary tool environments in versioned buckets, even
            // under --cache-dir or --no-cache (a randomly named cache root).
            _ if found != Origin::Npm
                && ["archive-v", "environments-v"].iter().any(|prefix| {
                    dir.strip_prefix(prefix)
                        .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
                }) =>
            {
                found = Origin::UvCache
            }
            _ => {}
        }
        child = dir;
    }
    if found == Origin::Native && cache.is_some_and(|root| path.starts_with(root)) {
        Origin::UvCache
    } else {
        found
    }
}

pub(super) fn persistent(path: &Path, cache: Option<&Path>) -> bool {
    let stable = |p: &Path| matches!(origin(p, cache), Origin::Native | Origin::Npm);
    stable(path) && path.canonicalize().is_ok_and(|p| stable(&p))
}

fn native_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
            && (!cfg!(windows)
                || path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("exe")))
    }
}

fn matches_binary(exe: &Path, binary: &str) -> bool {
    exe.file_name().is_some_and(|name| {
        if cfg!(windows) {
            name.to_str().is_some_and(|name| {
                name.eq_ignore_ascii_case(binary)
                    || name
                        .to_ascii_lowercase()
                        .strip_suffix(".exe")
                        .is_some_and(|stem| stem.eq_ignore_ascii_case(binary))
            })
        } else {
            name == binary
        }
    })
}

pub(super) fn detect(
    server: &Server,
    binary: &str,
    pypi_package: Option<&str>,
    exe: Option<&Path>,
    path: &[PathBuf],
    cache: Option<&Path>,
) -> (String, Vec<String>) {
    let resolved = exe.and_then(|p| p.canonicalize().ok());
    let source = exe.map_or(Origin::Native, |p| origin(p, cache));
    let target = resolved
        .as_deref()
        .map_or(Origin::Native, |p| origin(p, cache));
    if matches!(source, Origin::Npm | Origin::NpmCache)
        || matches!(target, Origin::Npm | Origin::NpmCache)
    {
        return server.launch();
    }
    if source == Origin::UvCache || target == Origin::UvCache {
        if let Some(package) = pypi_package {
            let mut args = if package == binary {
                vec![]
            } else {
                vec!["--from".into(), package.into()]
            };
            args.push(binary.into());
            args.extend(server.args.iter().cloned());
            return ("uvx".into(), args);
        }
        return server.launch();
    }

    let current = exe.filter(|p| p.is_absolute() && matches_binary(p, binary));
    let installed = candidates(binary, path).find_map(|p| {
        // Preserve stable symlinks (e.g. Homebrew's bin rather than a versioned
        // Cellar path), but only if they point to the same executable.
        let p = std::path::absolute(p).ok()?;
        if !native_executable(&p) || origin(&p, cache) != Origin::Native {
            return None;
        }
        let target = p.canonicalize().ok()?;
        (origin(&target, cache) == Origin::Native
            && (current.is_none() || Some(&target) == resolved.as_ref()))
        .then_some(p)
    });

    // Clients started outside the shell may not inherit its PATH (especially
    // for pipx, Cargo, Homebrew, and activated virtual environments).
    if let Some(command) = installed.as_deref().or(current).and_then(Path::to_str) {
        return (command.to_owned(), server.args.clone());
    }
    server.launch()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        Server {
            name: "app".into(),
            package: "@example/app".into(),
            args: vec!["mcp".into(), "--flag".into()],
        }
    }

    fn file(root: &Path, bin_dir: &str) -> PathBuf {
        let path = bin_dir
            .split('/')
            .fold(root.to_path_buf(), |path, dir| path.join(dir))
            .join(format!("app{}", std::env::consts::EXE_SUFFIX));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn persistent_install_uses_an_absolute_command_with_or_without_path() {
        let temp = tempfile::tempdir().unwrap();
        // Persistent uv tool installs are distinct from uv's tool-run cache.
        for dir in [
            ".local/bin",
            ".cargo/bin",
            "opt/homebrew/bin",
            "project/.venv/bin",
            "pipx/venvs/app/Scripts",
            ".local/share/uv/tools/app/bin",
        ] {
            let exe = file(temp.path(), dir);
            let other = file(temp.path(), "other/bin");
            for dirs in [
                vec![],
                vec![other.parent().unwrap().into(), exe.parent().unwrap().into()],
            ] {
                assert_eq!(
                    detect(&server(), "app", Some("app"), Some(&exe), &dirs, None),
                    (exe.to_str().unwrap().to_owned(), server().args)
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn prefers_stable_homebrew_symlink_even_when_current_exe_is_resolved() {
        let temp = tempfile::tempdir().unwrap();
        let cellar = file(temp.path(), "homebrew/Cellar/app/1.2.3/bin");
        let link = temp.path().join("homebrew/bin/app");
        std::fs::create_dir(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&cellar, &link).unwrap();
        assert_eq!(
            detect(
                &server(),
                "app",
                None,
                Some(&cellar),
                &[link.parent().unwrap().into()],
                None
            ),
            (link.to_str().unwrap().to_owned(), server().args)
        );
    }

    #[test]
    fn uvx_recreates_the_environment_instead_of_pointing_into_it() {
        let temp = tempfile::tempdir().unwrap();
        let other = file(temp.path(), "other/bin");
        for dir in [
            ".cache/uv/archive-v0/abc/bin",
            "Library/Caches/uv/archive-v0/abc/bin",
            "AppData/Local/uv/cache/environments-v2/abc/Scripts",
            "AppData/Local/uv/cache/other/Scripts",
            "custom-temp-root/archive-v0/abc/bin",
            "custom-cache/environments-v1/abc/bin",
        ] {
            let exe = file(temp.path(), dir);
            let dirs = [exe.parent().unwrap().into(), other.parent().unwrap().into()];
            assert!(!persistent(&exe, None), "{dir}");
            assert_eq!(
                detect(&server(), "app", Some("app"), Some(&exe), &dirs, None),
                (
                    "uvx".into(),
                    vec!["app".into(), "mcp".into(), "--flag".into()]
                )
            );
            assert_eq!(
                detect(
                    &server(),
                    "app",
                    Some("example-app"),
                    Some(&exe),
                    &dirs,
                    None
                ),
                (
                    "uvx".into(),
                    ["--from", "example-app", "app", "mcp", "--flag"]
                        .map(String::from)
                        .to_vec()
                )
            );
            assert_eq!(
                detect(&server(), "app", None, Some(&exe), &dirs, None),
                server().launch()
            );
        }
    }

    #[test]
    fn explicit_uv_cache_and_symlink_do_not_count_as_installed() {
        let temp = tempfile::tempdir().unwrap();
        let exe = file(temp.path(), "custom/other/bin");
        // Same normalization as uv_cache(), including macOS /var -> /private/var.
        let cache = temp.path().join("custom").canonicalize().unwrap();
        assert!(!persistent(&exe, Some(&cache)));
        assert_eq!(
            detect(&server(), "app", Some("app"), Some(&exe), &[], Some(&cache)).0,
            "uvx"
        );
        #[cfg(unix)]
        {
            let link = temp.path().join("app");
            std::os::unix::fs::symlink(&exe, &link).unwrap();
            assert!(!persistent(&link, Some(&cache)));
            assert_eq!(
                detect(
                    &server(),
                    "app",
                    Some("app"),
                    Some(&link),
                    &[],
                    Some(&cache)
                )
                .0,
                "uvx"
            );
            assert_eq!(
                detect(
                    &server(),
                    "app",
                    Some("app"),
                    None,
                    &[temp.path().into()],
                    Some(&cache)
                ),
                server().launch()
            );
        }
    }

    #[test]
    fn npm_invocations_keep_the_existing_npx_command_even_with_a_native_install() {
        let temp = tempfile::tempdir().unwrap();
        let native = file(temp.path(), "persistent/bin");
        for dir in [
            "_npx/id/node_modules/@example/app",
            "AppData/Local/npm-cache/id",
            ".npm/bin",
            "lib/node_modules/@example/app",
        ] {
            let exe = file(temp.path(), dir);
            assert_eq!(
                detect(
                    &server(),
                    "app",
                    Some("app"),
                    Some(&exe),
                    &[native.parent().unwrap().into()],
                    None
                ),
                server().launch()
            );
        }
        let result = if cfg!(windows) {
            (
                "cmd",
                vec!["/c", "npx", "-y", "@example/app", "mcp", "--flag"],
            )
        } else {
            ("npx", vec!["-y", "@example/app", "mcp", "--flag"])
        };
        assert_eq!(
            server().launch(),
            (
                result.0.into(),
                result.1.into_iter().map(String::from).collect()
            )
        );
        assert_eq!(
            detect(&server(), "app", Some("app"), None, &[], None),
            server().launch()
        );
    }

    #[test]
    fn path_selection_skips_transient_and_missing_entries() {
        let temp = tempfile::tempdir().unwrap();
        let uv = file(temp.path(), "uv/archive-v0/x/bin");
        let npm = file(temp.path(), "npm-cache/bin");
        let native = file(temp.path(), "my_npx_project/uv/tools/bin");
        assert!(!persistent(&npm, None));
        assert!(persistent(&native, None));
        let dirs = [
            temp.path().join("missing"),
            uv.parent().unwrap().into(),
            npm.parent().unwrap().into(),
            native.parent().unwrap().into(),
        ];
        assert_eq!(
            detect(&server(), "app", None, None, &dirs, None),
            (native.to_str().unwrap().to_owned(), server().args)
        );
    }
}
