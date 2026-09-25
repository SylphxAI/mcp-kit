//! `setup`: register an MCP server with the clients installed on this
//! machine, and optionally add a Claude Code hook. Every change is idempotent
//! and reported.

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The server to register: `npx -y <package> <args…>` under `name`.
#[derive(Debug, Clone)]
pub struct Server {
    pub name: String,
    pub package: String,
    pub args: Vec<String>,
}

impl Server {
    /// The launch command. On Windows, `npx` runs through `cmd /c`.
    pub fn launch(&self) -> (String, Vec<String>) {
        let mut base = vec!["-y".to_string(), self.package.clone()];
        base.extend(self.args.iter().cloned());
        if cfg!(windows) {
            let mut a = vec!["/c".to_string(), "npx".to_string()];
            a.extend(base);
            ("cmd".into(), a)
        } else {
            ("npx".into(), base)
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub dry_run: bool,
    pub remove: bool,
    /// Only these client ids (e.g. `cursor`, `codex`); otherwise every detected client.
    pub clients: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    /// `{ "mcpServers": { name: {command, args} } }`
    McpServers,
    /// VS Code: `{ "servers": { name: {type, command, args} } }`
    VsCode,
    /// Claude Code user config: mcpServers with `type: stdio`
    ClaudeJson,
    /// Codex `config.toml`: `[mcp_servers.name]`
    CodexToml,
}

struct Client {
    id: &'static str,
    label: &'static str,
    detect: PathBuf,
    config: PathBuf,
    format: Format,
}

/// Whether `bin` is on PATH.
pub fn on_path(bin: &str) -> bool {
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat", ""] } else { &[""] };
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| exts.iter().any(|e| d.join(format!("{bin}{e}")).is_file())))
}

/// Whether `bin` is installed to stay: npx puts a temporary shim on PATH,
/// which must never be written into a client's settings.
pub fn installed(bin: &str) -> bool {
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat", ""] } else { &[""] };
    std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p).any(|d| {
            let s = d.to_string_lossy();
            !s.contains("_npx") && !s.contains("npm-cache") && !s.contains("/.npm/") && exts.iter().any(|e| d.join(format!("{bin}{e}")).is_file())
        })
    })
}

fn clients() -> Vec<Client> {
    let home = dirs::home_dir().unwrap_or_default();
    let config = dirs::config_dir().unwrap_or_else(|| home.join(".config"));
    let c = |id, label, detect: PathBuf, config: PathBuf, format| Client { id, label, detect, config, format };
    vec![
        c("claude-code", "Claude Code", home.join(".claude"), home.join(".claude.json"), Format::ClaudeJson),
        // Codex may be installed without a config dir yet.
        c("codex", "Codex", if on_path("codex") { home.clone() } else { home.join(".codex") }, home.join(".codex/config.toml"), Format::CodexToml),
        c("cursor", "Cursor", home.join(".cursor"), home.join(".cursor/mcp.json"), Format::McpServers),
        c("vscode", "VS Code", config.join("Code/User"), config.join("Code/User/mcp.json"), Format::VsCode),
        c("vscode-insiders", "VS Code Insiders", config.join("Code - Insiders/User"), config.join("Code - Insiders/User/mcp.json"), Format::VsCode),
        c("claude-desktop", "Claude Desktop", config.join("Claude"), config.join("Claude/claude_desktop_config.json"), Format::McpServers),
        c("windsurf", "Windsurf", home.join(".codeium/windsurf"), home.join(".codeium/windsurf/mcp_config.json"), Format::McpServers),
        c("gemini", "Gemini CLI", home.join(".gemini"), home.join(".gemini/settings.json"), Format::McpServers),
    ]
}

#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    Unchanged,
    Wrote(&'static str),
}

/// Register (or remove) `server` in every detected client, printing one line each.
/// Returns how many files changed.
pub fn run(server: &Server, opts: &Options) -> Result<usize> {
    let (cmd, args) = server.launch();
    let mut touched = 0;
    let mut found = 0;
    for c in clients() {
        match &opts.clients {
            Some(only) if !only.iter().any(|o| o == c.id) => continue,
            None if !c.detect.exists() => continue,
            _ => {}
        }
        found += 1;
        let res = if c.id == "claude-code" && on_path("claude") {
            claude_cli(&server.name, &cmd, &args, opts)
        } else if c.format == Format::CodexToml {
            edit_codex(&c.config, &server.name, &cmd, &args, opts)
        } else {
            edit_json(&c.config, c.format, &server.name, &cmd, &args, opts)
        };
        match res {
            Ok(Change::Unchanged) => println!("  = {:<17} already configured ({})", c.label, c.config.display()),
            Ok(Change::Wrote(what)) => {
                touched += 1;
                println!("  + {:<17} {} {}", c.label, describe(what, opts.dry_run), c.config.display());
            }
            Err(e) => println!("  ! {:<17} {e}", c.label),
        }
    }
    if found == 0 {
        println!("  No MCP clients detected. Add this to your client's MCP config:");
        println!("  {{\"mcpServers\": {{\"{}\": {{\"command\": \"{cmd}\", \"args\": {}}}}}}}", server.name, serde_json::to_string(&args)?);
    }
    Ok(touched)
}

/// "wrote" becomes "would be written to" in a dry run.
pub fn describe(what: &str, dry_run: bool) -> String {
    if dry_run {
        format!("would be {}", what.replace("wrote", "written to"))
    } else {
        what.to_string()
    }
}

fn entry(fmt: Format, cmd: &str, args: &[String]) -> Value {
    match fmt {
        Format::McpServers => json!({"command": cmd, "args": args}),
        _ => json!({"type": "stdio", "command": cmd, "args": args}),
    }
}

fn read_json(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let txt = std::fs::read_to_string(path)?;
    if txt.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&txt).map_err(|e| anyhow!("cannot parse {} ({e}); edit it by hand", path.display()))
}

fn write_json(path: &Path, v: &Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("mcp-kit-tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(v)? + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn edit_json(path: &Path, fmt: Format, name: &str, cmd: &str, args: &[String], opts: &Options) -> Result<Change> {
    let mut root = read_json(path)?;
    let key = if fmt == Format::VsCode { "servers" } else { "mcpServers" };
    let obj = root.as_object_mut().ok_or_else(|| anyhow!("{} is not a JSON object", path.display()))?;
    let servers = obj.entry(key).or_insert_with(|| Value::Object(Map::new()));
    let servers = servers.as_object_mut().ok_or_else(|| anyhow!("`{key}` is not an object"))?;
    let want = entry(fmt, cmd, args);
    if opts.remove {
        if servers.remove(name).is_none() {
            return Ok(Change::Unchanged);
        }
    } else {
        if servers.get(name) == Some(&want) {
            return Ok(Change::Unchanged);
        }
        servers.insert(name.into(), want);
    }
    if !opts.dry_run {
        write_json(path, &root)?;
    }
    Ok(Change::Wrote(if opts.remove { "removed from" } else { "wrote" }))
}

fn edit_codex(path: &Path, name: &str, cmd: &str, args: &[String], opts: &Options) -> Result<Change> {
    use toml_edit::{value, Array, DocumentMut, Item, Table};
    let txt = if path.exists() { std::fs::read_to_string(path)? } else { String::new() };
    let mut doc: DocumentMut = txt.parse().map_err(|e| anyhow!("cannot parse {}: {e}", path.display()))?;
    if !doc.contains_key("mcp_servers") {
        let mut t = Table::new();
        t.set_implicit(true);
        doc["mcp_servers"] = Item::Table(t);
    }
    let servers = doc["mcp_servers"].as_table_mut().ok_or_else(|| anyhow!("mcp_servers is not a table"))?;
    if opts.remove {
        if servers.remove(name).is_none() {
            return Ok(Change::Unchanged);
        }
    } else {
        let same = servers.get(name).and_then(|t| t.as_table()).is_some_and(|t| {
            t.get("command").and_then(|c| c.as_str()) == Some(cmd)
                && t.get("args").and_then(|a| a.as_array()).map(|a| a.iter().filter_map(|x| x.as_str()).map(String::from).collect::<Vec<_>>())
                    == Some(args.to_vec())
        });
        if same {
            return Ok(Change::Unchanged);
        }
        let mut t = Table::new();
        t["command"] = value(cmd);
        t["args"] = value(args.iter().map(String::as_str).collect::<Array>());
        servers.insert(name, Item::Table(t));
    }
    if !opts.dry_run {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, doc.to_string())?;
    }
    Ok(Change::Wrote(if opts.remove { "removed from" } else { "wrote" }))
}

fn claude_cli(name: &str, cmd: &str, args: &[String], opts: &Options) -> Result<Change> {
    let exists = Command::new("claude").args(["mcp", "get", name]).output().is_ok_and(|o| o.status.success());
    if opts.remove {
        if !exists {
            return Ok(Change::Unchanged);
        }
        if !opts.dry_run {
            run_ok(Command::new("claude").args(["mcp", "remove", "--scope", "user", name]))?;
        }
        return Ok(Change::Wrote("removed (claude mcp remove) from"));
    }
    if exists {
        return Ok(Change::Unchanged);
    }
    if !opts.dry_run {
        run_ok(Command::new("claude").args(["mcp", "add", "--scope", "user", name, "--", cmd]).args(args))?;
    }
    Ok(Change::Wrote("added (claude mcp add --scope user) to"))
}

fn run_ok(c: &mut Command) -> Result<()> {
    let out = c.output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// A Claude Code hook: run `command` before the tools in `matcher`.
#[derive(Debug, Clone)]
pub struct Hook {
    /// Hook event, e.g. `PreToolUse`.
    pub event: String,
    /// Tool matcher, e.g. `Grep|Glob`.
    pub matcher: String,
    pub command: String,
    /// Text that identifies this hook's command (for updates and removal), e.g. `repomap hook`.
    pub marker: String,
    pub timeout_secs: u64,
}

/// `~/.claude/settings.json`
pub fn claude_settings() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".claude").join("settings.json")
}

/// Add, update or remove `hook` in a Claude Code settings file.
pub fn claude_hook(path: &Path, hook: &Hook, opts: &Options) -> Result<Change> {
    let mut root = read_json(path)?;
    let obj = root.as_object_mut().ok_or_else(|| anyhow!("{} is not a JSON object", path.display()))?;
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().ok_or_else(|| anyhow!("`hooks` is not an object"))?;
    let groups = hooks.entry(hook.event.clone()).or_insert_with(|| json!([]));
    let groups = groups.as_array_mut().ok_or_else(|| anyhow!("`hooks.{}` is not an array", hook.event))?;
    let ours = |g: &Value| {
        g.get("hooks").and_then(|h| h.as_array()).is_some_and(|hs| {
            hs.iter().any(|h| h.get("command").and_then(|c| c.as_str()).is_some_and(|c| c.contains(&hook.marker)))
        })
    };
    let want = json!({"matcher": hook.matcher, "hooks": [{"type": "command", "command": hook.command, "timeout": hook.timeout_secs}]});
    let mine: Vec<usize> = groups.iter().enumerate().filter(|(_, g)| ours(g)).map(|(i, _)| i).collect();
    if opts.remove && mine.is_empty() {
        return Ok(Change::Unchanged);
    }
    if !opts.remove && mine.len() == 1 && groups[mine[0]] == want {
        return Ok(Change::Unchanged);
    }
    for i in mine.into_iter().rev() {
        groups.remove(i);
    }
    if !opts.remove {
        groups.push(want);
    }
    if groups.is_empty() {
        hooks.remove(&hook.event);
    }
    if hooks.is_empty() {
        obj.remove("hooks");
    }
    if !opts.dry_run {
        write_json(path, &root)?;
    }
    Ok(Change::Wrote(if opts.remove { "removed from" } else { "wrote" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mcp-kit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn json_and_toml_edits_are_idempotent() {
        let d = tmp("edit");
        let args = vec!["-y".to_string(), "@x/y".to_string(), "mcp".to_string()];
        let opts = Options::default();
        let json_path = d.join("mcp.json");
        std::fs::write(&json_path, r#"{"mcpServers":{"other":{"command":"x"}}}"#).unwrap();
        assert!(matches!(edit_json(&json_path, Format::McpServers, "y", "npx", &args, &opts).unwrap(), Change::Wrote(_)));
        assert_eq!(edit_json(&json_path, Format::McpServers, "y", "npx", &args, &opts).unwrap(), Change::Unchanged);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        let toml = d.join("config.toml");
        std::fs::write(&toml, "model = \"o3\"\n").unwrap();
        assert!(matches!(edit_codex(&toml, "y", "npx", &args, &opts).unwrap(), Change::Wrote(_)));
        assert_eq!(edit_codex(&toml, "y", "npx", &args, &opts).unwrap(), Change::Unchanged);
        assert!(std::fs::read_to_string(&toml).unwrap().contains("model = \"o3\""));
        let remove = Options { remove: true, ..Default::default() };
        assert!(matches!(edit_codex(&toml, "y", "npx", &args, &remove).unwrap(), Change::Wrote(_)));
        assert!(!std::fs::read_to_string(&toml).unwrap().contains("[mcp_servers.y]"));
    }

    #[test]
    fn hook_is_idempotent_and_removable() {
        let d = tmp("hook");
        let path = d.join("settings.json");
        std::fs::write(&path, r#"{"model":"opus","hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"x"}]}]}}"#).unwrap();
        let hook = |cmd: &str| Hook { event: "PreToolUse".into(), matcher: "Grep|Glob".into(), command: cmd.into(), marker: "y hook".into(), timeout_secs: 10 };
        let opts = Options::default();
        assert!(matches!(claude_hook(&path, &hook("y hook"), &opts).unwrap(), Change::Wrote(_)));
        assert_eq!(claude_hook(&path, &hook("y hook"), &opts).unwrap(), Change::Unchanged);
        // A new command replaces the entry instead of adding a second one.
        assert!(matches!(claude_hook(&path, &hook("npx -y y hook"), &opts).unwrap(), Change::Wrote(_)));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert_eq!(v["model"], "opus");
        let remove = Options { remove: true, ..Default::default() };
        assert!(matches!(claude_hook(&path, &hook("y hook"), &remove).unwrap(), Change::Wrote(_)));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert_eq!(describe("wrote", true), "would be written to");
    }
}
