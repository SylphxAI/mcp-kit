//! Which directory a tool call works on.

use std::path::{Path, PathBuf};

/// Decode a `file://` URI into a path. Works byte by byte, so malformed
/// percent escapes next to non-ASCII text cannot split a character.
pub fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let b = rest.as_bytes();
    let mut bytes = Vec::with_capacity(b.len());
    let mut i = 0;
    let hex = |c: u8| (c as char).to_digit(16);
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(x), Some(y)) = (hex(b[i + 1]), hex(b[i + 2])) {
                bytes.push((x * 16 + y) as u8);
                i += 3;
                continue;
            }
        }
        bytes.push(b[i]);
        i += 1;
    }
    let s = String::from_utf8(bytes).ok()?;
    // file:///C:/x on Windows
    let s = if s.len() > 3 && s.as_bytes()[0] == b'/' && s.as_bytes()[2] == b':' { s[1..].to_string() } else { s };
    Some(PathBuf::from(s))
}

/// Where a call's root may come from, in order of precedence.
#[derive(Debug, Default, Clone)]
pub struct Sources<'a> {
    /// The call's own `root` argument.
    pub explicit: Option<PathBuf>,
    /// Environment variables to try, e.g. `["REPOMAP_ROOT"]`.
    pub env: &'a [&'a str],
    /// A root given when the server was launched (`--root`).
    pub default: Option<PathBuf>,
    /// The client's roots (MCP `roots/list`); ones that do not exist here are skipped.
    pub client: &'a [PathBuf],
}

/// Pick the root: explicit, then env, then the launch default, then the first
/// client root that exists, then the working directory. The working directory
/// is refused when it is `/` or the home directory, which usually means the
/// client did not start the server in a project.
pub fn pick(src: &Sources) -> Result<PathBuf, String> {
    if let Some(r) = &src.explicit {
        return dir(r.clone());
    }
    for var in src.env {
        if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            return dir(PathBuf::from(v));
        }
    }
    if let Some(r) = &src.default {
        return dir(r.clone());
    }
    if let Some(r) = src.client.iter().find(|p| p.is_dir()) {
        return Ok(r.clone());
    }
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    if cwd.parent().is_none() || Some(&cwd) == dirs::home_dir().as_ref() {
        return Err("No project selected: pass `root` (absolute path), or start the server from the project directory.".into());
    }
    Ok(cwd)
}

fn dir(p: PathBuf) -> Result<PathBuf, String> {
    if Path::new(&p).is_dir() {
        Ok(p)
    } else {
        Err(format!("root `{}` is not a directory", p.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uris() {
        assert_eq!(file_uri_to_path("file:///a%20b/c"), Some(PathBuf::from("/a b/c")));
        assert_eq!(file_uri_to_path("file:///C:/x"), Some(PathBuf::from("C:/x")));
        assert_eq!(file_uri_to_path("https://x"), None);
        for u in ["file:///%aé", "file:///%", "file:///%é%", "file:///中%2", "file:///%zz"] {
            let _ = file_uri_to_path(u);
        }
    }

    #[test]
    fn precedence() {
        let tmp = std::env::temp_dir();
        let missing = tmp.join("mcp-kit-missing-dir");
        // Client roots that do not exist here are skipped.
        let client = [missing.clone(), tmp.clone()];
        assert_eq!(pick(&Sources { client: &client, ..Default::default() }).unwrap(), tmp);
        // The explicit root wins, and must exist.
        assert!(pick(&Sources { explicit: Some(missing), client: &client, ..Default::default() }).is_err());
        assert_eq!(pick(&Sources { default: Some(tmp.clone()), ..Default::default() }).unwrap(), tmp);
    }
}
