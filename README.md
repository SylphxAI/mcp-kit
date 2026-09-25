# mcp-kit

Shared parts of the Sylphx MCP servers ([repomap](https://github.com/SylphxAI/repomap), [lockdocs](https://github.com/SylphxAI/lockdocs), [anymd](https://github.com/SylphxAI/anymd)), so each server keeps only its own tools.

| Part | What it does |
|---|---|
| Rust crate [`sylphx-mcp-kit`](https://crates.io/crates/sylphx-mcp-kit) | Runs an MCP server over stdio on [rmcp](https://github.com/modelcontextprotocol/rust-sdk). Picks the directory a call works on (argument, env var, `--root`, the client's roots, the working directory). Registers the server with MCP clients (`setup`) and adds a Claude Code hook. With the `embed` feature: local embeddings from a small static model. |
| `npm/launcher.js` | The `bin` script of an npm package. It runs the native binary for this platform from an optional dependency. |
| `.github/workflows/release.yml` | A reusable release workflow. It builds 5 native binaries, publishes to npm with trusted publishing, smoke-tests with `npx`, and creates the GitHub release and MCP Registry entry. Optionally it attaches MCP Bundles (`.mcpb`), pushes a GHCR image and retires old registry names. |

MIT licensed.

## Install

```toml
[dependencies]
sylphx-mcp-kit = "0.2"
# only the embeddings, without the server and setup parts:
# sylphx-mcp-kit = { version = "0.2", default-features = false, features = ["embed"] }
```

Features: `server` and `setup` (default), `embed`. The crate was first used as a git dependency (`git = "https://github.com/SylphxAI/mcp-kit", tag = "v0.1.0"`); those tags stay, and the crates.io release is the same code.

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

## Embeddings

```rust
use mcp_kit::embed::{self, Model, POTION_CODE_16M};

embed::ensure(&POTION_CODE_16M, "tool", "Set TOOL_EMBED=0 to stay keyword-only.")?; // once, 33 MB
let model = Model::load(&POTION_CODE_16M)?;
let v = model.embed("where are failed requests retried").unwrap(); // unit length, 256 numbers
```

- Models: `POTION_CODE_16M` ([potion-code-16M-v2](https://huggingface.co/minishlab/potion-code-16M-v2), code search) and `POTION_RETRIEVAL_32M` (English text). Both are MIT licensed [model2vec](https://github.com/MinishLab/model2vec) static models: an embedding is the mean of the token vectors, so a CPU embeds a large repository in about a second.
- `ensure` downloads the pinned revision from Hugging Face once, checks its SHA-256, and stores it as int8 in `~/.cache/sylphx/models` (or `SYLPHX_MODEL_DIR`), shared by every tool. It prints one line before downloading. After a failed download it waits an hour before trying again.
- The tokenizer matches the model's own (BERT normalization and WordPiece). CI checks tokens and vectors against `model2vec` itself.
- `Vec8`, `quantize` and `cosine` store and compare vectors as int8.

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

Inputs: `alias-dirs`, `smoke`, `docker-image`, `retired-mcp-names`, `retired-message`, `major-tag`, `mcpb` and `mcpb-icon`. They are documented in the workflow file.

### MCP Bundles

`mcpb: true` attaches [MCP Bundles](https://github.com/modelcontextprotocol/mcpb) to the GitHub release, for one-click install in Claude Desktop and other hosts:

- `<name>-<version>.mcpb`: every platform's binary and a small Node launcher (hosts such as Claude Desktop ship Node).
- `<name>-<version>-<platform>.mcpb`: one binary each, about a fifth of the size.

The manifest comes from `server.json` (title, description, website, arguments) and the npm package (license, keywords). The tool list comes from starting the server once. An optional `mcpb.json` at the repository root is merged into every manifest, for example to ask for a project folder:

```json
{
  "server": { "mcp_config": { "env": { "TOOL_ROOT": "${user_config.project}" } } },
  "user_config": { "project": { "type": "directory", "title": "Project folder", "description": "…", "required": true } }
}
```

`mcpb-icon` points at a 512×512 PNG. `scripts/mcpb.mjs` builds the bundles with the official `mcpb` CLI; `scripts/mcpb.test.mjs` checks them.

## Releasing the kit

Bump `version` in `Cargo.toml` and merge. `publish.yml` publishes the crate to crates.io, tags `vX.Y.Z` and moves `v0`, which the servers' release workflows use.
