//! Cache roots with caller-owned overrides, layout and fallback policy.

use std::ffi::OsString;
use std::path::PathBuf;

/// How to find a cache when the override is absent.
#[derive(Clone, Copy)]
pub enum Fallback {
    /// The OS cache directory, or the temporary directory.
    Temp,
    /// The OS cache directory, or no cache.
    None,
    /// Environment-only platform paths, or no cache. On Windows the product
    /// directory contains a `cache` child; relative XDG_CACHE_HOME is ignored.
    Environment,
}

/// How to interpret the override variable.
#[derive(Clone, Copy)]
pub enum Override {
    /// Accept any nonempty OS string.
    NonEmpty,
    /// Accept UTF-8 strings, including empty strings (a relative root).
    Utf8,
}

/// Select a cache root. An override is already a product root and is not
/// suffixed with `product_dir`. Callers own subdirectories and retention.
pub fn root(env: &str, product_dir: &str, fallback: Fallback, override_policy: Override) -> Option<PathBuf> {
    root_with_env(env, product_dir, fallback, override_policy, &|key| std::env::var_os(key))
}

/// Select a root using a supplied environment for the override and the
/// environment-only fallback. OS and temporary fallbacks use the process.
pub fn root_with_env(
    env: &str,
    product_dir: &str,
    fallback: Fallback,
    override_policy: Override,
    get: &dyn Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    root_from(env, product_dir, fallback, override_policy, get, dirs::cache_dir, std::env::temp_dir)
}

fn root_from(
    env: &str,
    product_dir: &str,
    fallback: Fallback,
    override_policy: Override,
    get: &dyn Fn(&str) -> Option<OsString>,
    platform: impl FnOnce() -> Option<PathBuf>,
    temp: impl FnOnce() -> PathBuf,
) -> Option<PathBuf> {
    let value = get(env).filter(|v| match override_policy {
        Override::NonEmpty => !v.is_empty(),
        Override::Utf8 => v.to_str().is_some(),
    });
    if let Some(value) = value {
        return Some(value.into());
    }
    match fallback {
        Fallback::Temp => Some(platform().unwrap_or_else(temp).join(product_dir)),
        Fallback::None => platform().map(|p| p.join(product_dir)),
        Fallback::Environment => environment_root(product_dir, get),
    }
}

/// Environment-only cache selection, also useful for callers with a supplied
/// environment (for example a sandbox or a test).
pub fn environment_root(product_dir: &str, get: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let var = |key: &str| get(key).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        var("LOCALAPPDATA").map(|p| p.join(product_dir).join("cache"))
    } else if cfg!(target_os = "macos") {
        var("HOME").map(|p| p.join("Library").join("Caches").join(product_dir))
    } else {
        var("XDG_CACHE_HOME")
            .filter(|p| p.is_absolute())
            .or_else(|| var("HOME").map(|p| p.join(".cache")))
            .map(|p| p.join(product_dir))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_and_fallbacks() {
        let pick = |value: Option<&str>, fallback, policy| {
            root_from("CACHE", "tool", fallback, policy, &|_| value.map(OsString::from), || Some(PathBuf::from("os")), || PathBuf::from("tmp"))
        };
        assert_eq!(pick(Some("custom"), Fallback::Temp, Override::NonEmpty), Some("custom".into()));
        assert_eq!(pick(Some(""), Fallback::Temp, Override::NonEmpty), Some(PathBuf::from("os").join("tool")));
        assert_eq!(pick(Some(""), Fallback::Temp, Override::Utf8), Some("".into()));
        assert_eq!(root_from("CACHE", "tool", Fallback::Temp, Override::NonEmpty, &|_| None, || None, || "tmp".into()), Some(PathBuf::from("tmp").join("tool")));
        assert_eq!(root_from("CACHE", "tool", Fallback::None, Override::NonEmpty, &|_| None, || None, || "tmp".into()), None);
    }

    #[test]
    fn environment_layout() {
        let get = |key: &str| match key {
            "HOME" => Some(OsString::from("home")),
            "LOCALAPPDATA" => Some(OsString::from("local")),
            "XDG_CACHE_HOME" => Some(OsString::from("relative")),
            _ => None,
        };
        let expected = if cfg!(windows) {
            PathBuf::from("local").join("tool").join("cache")
        } else if cfg!(target_os = "macos") {
            PathBuf::from("home").join("Library").join("Caches").join("tool")
        } else {
            PathBuf::from("home").join(".cache").join("tool")
        };
        assert_eq!(environment_root("tool", &get), Some(expected));
        assert_eq!(environment_root("tool", &|_| None), None);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_override() {
        use std::os::unix::ffi::OsStringExt;
        let get = |_: &str| Some(OsString::from_vec(vec![255]));
        assert!(root_from("CACHE", "tool", Fallback::None, Override::NonEmpty, &get, || None, || "tmp".into()).is_some());
        assert!(root_from("CACHE", "tool", Fallback::None, Override::Utf8, &get, || None, || "tmp".into()).is_none());
    }
}
