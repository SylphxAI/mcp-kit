# mcp-kit

Shared parts of the Sylphx MCP servers ([repomap](https://github.com/SylphxAI/repomap), [lockdocs](https://github.com/SylphxAI/lockdocs), [anymd](https://github.com/SylphxAI/anymd)), so each server keeps only its own tools.

| Part | What it does |
|---|---|
| Rust crate `sylphx-mcp-kit` | Runs an MCP server over stdio on [rmcp](https://github.com/modelcontextprotocol/rust-sdk). Picks the directory a call works on (argument, env var, `--root`, the client's roots, the working directory). Registers the server with MCP clients (`setup`) and adds a Claude Code hook. |
| `npm/launcher.js` | The `bin` script of an npm package. It runs the native binary for this platform from an optional dependency. |
| `.github/workflows/release.yml` | A reusable release workflow. It builds 5 native binaries, publishes to npm with trusted publishing, smoke-tests with `npx`, and creates the GitHub release and MCP Registry entry. Optionally it pushes a GHCR image and retires old registry names. |

MIT licensed.

## Server

```rust
use mcp_kit::server::{run_stdio, App, Call, Info};
use serde_json::{json, Value};

struct Echo;

impl App for Echo {
    fn info(&self) -> Info {
        Info { name: "echo".into(), title: "Echo".into(), version: "1.0.0".into(),
               website: "https://example.com".into(), instructions: "Echoes text.".into() }
    }
    fn tools(&self) -> Vec<Value> {
        vec![json!({"name": "echo", "description": "Echo `text`.",
                    "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}})]
    }
    fn call(&self, name: &str, args: &Value, call: &Call) -> Result<String, String> {
        // call.client_roots: the client's workspace folders that exist here.
        Ok(args["text"].as_str().unwrap_or_default().to_string())
    }
}

fn main() -> anyhow::Result<()> {
    run_stdio(Echo)
}
```

- `call` runs on a blocking thread. `Err(text)` goes back to the agent as a readable tool error.
- `warm` (optional) runs once after the client connects. Use it to fill a cache.
- `roots::pick` chooses the working directory from the call's arguments and `call.client_roots`.

### Why rmcp

The servers first used about 200 lines of hand-written JSON-RPC each. rmcp is the official Rust SDK, kept in step with the MCP spec. It handles what the hand-written loops did not:
- protocol version negotiation over every published version
- cancellation, progress and logging
- pagination and result caching fields
- tasks, and structured and error results with the right shape for each protocol version

The binary cost is measured in each migration PR.

## Setup

```rust
use mcp_kit::setup::{run, Options, Server};

let server = Server { name: "repomap".into(), package: "@sylphx/repomap".into(), args: vec!["mcp".into()] };
run(&server, &Options { dry_run: false, remove: false, clients: None })?;
```

Supported clients:

| Client | Where the entry is written |
|---|---|
| Claude Code | `claude mcp add --scope user`, or `~/.claude.json` |
| Codex | `~/.codex/config.toml` |
| Cursor | `~/.cursor/mcp.json` |
| VS Code (and Insiders) | the user `mcp.json` |
| Claude Desktop | its config |
| Windsurf | its config |
| Gemini CLI | its config |

Every change is safe to repeat and is printed. `remove: true` undoes it. `setup::claude_hook` adds, updates or removes a Claude Code hook identified by a marker string.

## npm package

Copy `npm/launcher.js` to `packages/<name>/bin/<name>.js`, and declare one optional dependency per platform:

```json
{
  "name": "@sylphx/tool",
  "bin": { "tool": "bin/tool.js" },
  "optionalDependencies": {
    "@sylphx/tool-darwin-arm64": "1.0.0", "@sylphx/tool-darwin-x64": "1.0.0",
    "@sylphx/tool-linux-x64-gnu": "1.0.0", "@sylphx/tool-linux-arm64-gnu": "1.0.0",
    "@sylphx/tool-win32-x64-msvc": "1.0.0"
  }
}
```

Each platform package (`packages/npm/<platform>/package.json`) sets `os`, `cpu` and, on Linux, `libc`, and ships the binary. `TOOL_BIN=/path` overrides the binary.

## Release workflow

```yaml
# .github/workflows/release.yml in the server's repo
name: release
on:
  push: { branches: [main] }
  workflow_dispatch:
jobs:
  release:
    uses: SylphxAI/mcp-kit/.github/workflows/release.yml@v0
    permissions: { contents: write, id-token: write, packages: write }
    with:
      name: tool
      npm-package: '@sylphx/tool'
      mcp-name: io.github.SylphxAI/tool
```

A release happens when `packages/<name>/package.json` has a version that is not on npm yet. All manifests must carry that version, including `server.json` and the platform packages. The workflow checks this first.

npm trusted publishing checks the **calling** workflow file. So every npm package trusts `<owner>/<repo>` with the file `release.yml`:

```bash
npm trust github @sylphx/tool --file release.yml --repo SylphxAI/tool --allow-publish --otp <code>
```

Inputs: `alias-dirs`, `smoke`, `docker-image`, `retired-mcp-names`, `retired-message` and `major-tag`. They are documented in the workflow file.
