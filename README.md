# mcp-kit

<p align="center">
  <img src="https://mark.sylphx.com/api/v1/mark/hero.svg?type=aurora&theme=grape&text=mcp-kit&desc=Shared%20parts%20of%20the%20Sylphx%20MCP%20servers" alt="mcp-kit" width="100%" />
</p>

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
sylphx-mcp-kit = "0.5"
# only the embeddings, without the server and setup parts:
# sylphx-mcp-kit = { version = "0.5", default-features = false, features = ["embed"] }
```

Features: `server` and `setup` (default), `embed`, `search`, `licence`. Cache roots and CLI hints need no feature.

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

rmcp is the official Rust SDK, kept in step with the MCP spec, so a server needs no hand-written JSON-RPC loop. It handles:
- protocol version negotiation over every published version
- cancellation, progress and logging
- pagination and result caching fields
- tasks, and structured and error results with the right shape for each protocol version

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

## Search and CLI helpers

- `search` owns identifier splitting, `tokenize`, `path_terms`, `chunk_terms`
  and UTF-8-safe byte limits (`floor_char`). Ranking stays in each tool.
- `star_hint::after_success(message, opt_out_env, state_dir, mcp)` prints once,
  after the fifth successful interactive CLI run. It never counts or prints
  in MCP mode, with non-TTY stderr, in CI, or when opted out. It keeps the
  existing `star-hint` counter format; callers choose their cache root.
- `cache::root(env, product_dir, fallback, override_policy)` preserves the
  caller's cache rules: OS cache with temp/no fallback, or environment-only
  platform paths. Overrides are either nonempty OS strings or UTF-8 strings
  including empty strings. Callers keep their own layout and retention.
- `embed::ensure_at` and `Model::load_dir` support existing model caches and
  full model URL overrides. `Tokenization::Identifiers` keeps the original
  identifier-aware, untruncated vectors; the default model2vec behavior is
  unchanged. The `model.q8`, `vocab.txt` and serialized `Vec8` formats are
  unchanged, so existing embedding indexes stay readable.

## Selling a Pro tier with licence

The `licence` feature (off by default) lets a server keep its core free and unlock extra tools with an offline-verified token. The kit holds no product logic: a server describes itself in one `LicencePolicy`.

```rust
use mcp_kit::licence::{require, run_cli, LicencePolicy};

const POLICY: LicencePolicy = LicencePolicy {
    product: "lockdocs",
    require_product: true,                            // false only for anymd back-compat
    accepted_plans: &["pro", "team"],
    public_keys: &["<base64url Ed25519 public key>"], // a list, so rotation is additive
    env_var: "LOCKDOCS_LICENCE_TOKEN",                // read first
    file_name: "licence",                             // then <config dir>/lockdocs/licence
    upgrade_url: "https://example.com/pro",         // also the renewal link in expiry warnings
    tier: "Pro",                                      // what users see: "lockdocs Pro", or "Team"
    checkout_base: Some("https://checkout.example.com"), // enables `licence buy`; None prints upgrade_url
};

// In a Pro tool: the licence, or a polite notice the agent relays.
match require(&POLICY, "Team reports") {
    Ok(licence) => { /* licence.plan, .seats, .expires_at, ... */ }
    Err(required) => return Ok(required.to_string()),
}
```

- Token: `base64url(payloadJSON).base64url(Ed25519 signature over the payload bytes)`, payload `{"plan", "email"?, "issuedAt" (ms), "product"?, "order"?, "grant"?, "seats"?, "expiresAt"? (ms)}`. Unknown fields are ignored. It is byte-compatible with the anymd and GPDT verifiers.
- `policy.verify(token)` (or `licence::verify(&policy, token)`) checks the signature against any key, then the plan, the `product` (a token naming another product is refused; one naming none is refused when `require_product` is true), and `expiresAt`.
- `require` reads the env var first, even when it holds a bad token (no silent fallback), then the token file.
- An unlicensed call is not an error. `required_result_json(&required)` returns the MCP result: text "<feature> is part of <product> <tier>. Learn more and get it: <url>" plus `structuredContent` `{"pro_required": {"feature", "product", "tier", "url"}}` so an agent can act on it. With the `server` feature, `required_result` returns the rmcp type: override `App::call_result` in a gated tool to return it. A Pro tool must not declare an `outputSchema` without `pro_required`.
- `run_cli(&POLICY, args)` is the `licence status | activate <token>` subcommand to mount in the server binary; `activate` verifies the token and writes the file with 0600 permissions. `status` also prints where the token was read (env var or file) and why it is invalid, and during the last 30 days of a licence it warns "expires in N days" with `upgrade_url` as the renewal link. `licence.expires_soon(Duration)` gives the same check to your own code, e.g. to add a line to a Pro answer; it is false for a licence with no `expiresAt`, and true when expiry is within the window (inclusive) or past.
- `licence buy [--pack <id>] [--qty <n>] [--no-browser] [--json]` (in `run_cli`) sells through the shared checkout service at `checkout_base` (https only; `None` prints `upgrade_url`). It creates a claim (`POST {checkout_base}/api/v1/claims`), prints the browser URL and opens it unless `--no-browser`, `--json`, no display or non-interactive, then polls the claim (`Retry-After` honoured, backoff capped at 5 s, 30 minutes at most). When paid it verifies the returned tokens, saves the first valid one the way `activate` does and prints the `status` report; on expiry or timeout it points at `{checkout_base}/recover`. Nothing is saved before a token verifies, so Ctrl+C leaves no state.
- For agents, `--json` prints a line with `claim_id` and `browser_url` as soon as the claim exists (show the link to your user) and a final `{"event":"result","status":...}` line. Exit codes: 0 paid, 1 failed, 2 usage, 3 expired, 4 timed out.

A runnable example is in `crates/mcp-kit/examples/licence_server.rs`.

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

The workflow checks every requested delivery channel: all five native npm packages, the launcher and aliases, the GitHub release assets (including requested bundles), the exact MCP Registry version, and the optional GHCR image. A fresh run finishes missing channels without republishing delivered packages or overwriting existing release assets. HTTP 404 alone means absent; authentication, throttling, server and transport errors stop the run rather than authorizing publication.

### Homebrew and Scoop

Opt in with one input each; the release then writes `Formula/<name>.rb` (macOS and Linux, arm64 and x86_64) to the tap and `bucket/<name>.json` (Windows x64, with `checkver` and `autoupdate`) to the bucket, from the digests of the release archives. It runs after the release, and again on a no-op run, so a missed update heals. Unchanged files are not committed.

```yaml
    with:
      homebrew: true   # and/or scoop: true
    secrets:
      TAP_APP_ID: ${{ secrets.TAP_APP_ID }}
      TAP_APP_PRIVATE_KEY: ${{ secrets.TAP_APP_PRIVATE_KEY }}
```

| Input | Default |
| --- | --- |
| `homebrew`, `scoop` | `false` |
| `homebrew-tap` | `SylphxAI/homebrew-tap` |
| `scoop-bucket` | `SylphxAI/scoop-bucket` |
| `description`, `homepage`, `license` | the main npm package's, then the GitHub repository for `homepage` |

Writing to another repository uses a GitHub App, never a token: create an org app with **Contents: read and write** on the tap and bucket repositories only, install it on those two, and store its ID and private key as org secrets `TAP_APP_ID` and `TAP_APP_PRIVATE_KEY` (visible to the server repositories). Pass them by name as above and never use `secrets: inherit`: it would hand the reusable workflow every caller secret, including publish tokens. Without the secrets the job skips with a notice and the release is unaffected. Tap and bucket must share one owner. Winget is not generated.

Users: `brew install SylphxAI/tap/<name>`, `scoop bucket add sylphx https://github.com/SylphxAI/scoop-bucket && scoop install <name>`. The rendering lives in `scripts/package-managers.mjs`.

The selected Cargo package, npm manifests and `server.json` must carry the same version and publication identity. Executable native targets must report that exact version. Every native artifact carries its platform, version, source and binary SHA-256; staging checks these before publishing, including cross-compiled targets.

A version has one canonical repository commit, established from original platform identities and published native source revisions; conflicting sources stop the run. Recovery preserves the original platform identity and verifies its binary digest, version and canonical source. GitHub archives require a digest-verified original identity asset; npm fallback requires registry integrity, the embedded manifest and original identity, with a matching registry source revision. Already-complete legacy releases remain verified no-ops: every native, launcher and alias must share an immutable npm source revision, the exact MCP Registry version must be active, and all five archives and requested bundles must exist. No new sidecars or build provenance are manufactured. A requested legacy GHCR image must have both digest-verified Linux architectures and an established GitHub Packages version record binding the exact version tag to its index digest; this verifies existing delivery, not native-source provenance. Any conflicting version label fails. When recovery is needed, missing GitHub sidecars are not proof of a legacy release: the planner first verifies available npm tarball integrity, embedded manifests and original native identities against the shared canonical source. Interrupted modern deliveries resume using those original identities. Legacy partial releases without original identities fail closed when a requested channel needs recovery, rather than relabelling or overwriting old bytes. Missing natives compile from the canonical commit, not a later same-version main commit. GHCR readback verifies digest-addressed child manifests and configs for both architectures, checking version, canonical revision, source and native digests; an existing stale version tag is an error. Authorized GitHub package metadata confirms first-image absence; token denials never mean absent. Old MCP Registry names are retired only after the replacement's exact version is verified available, including otherwise complete no-op runs.

`kit-ref` supplies the release helpers as well as the bundle builder. When pinning the reusable workflow to a commit, pin `kit-ref` to that same commit. The release publishes npm packages and then waits per package (natives, main, aliases) with `npm view name@V version`, backing off from 5 s to 30 s for up to 10 minutes and retrying only ETARGET, 404 and "No matching version" (`scripts/npm-settle.sh`) before the `npx` smoke; a caller's `smoke` can `. "$SETTLE_LIB"` and run checks through `settle`. This change bumps the compatible kit version to 0.3.3. Changes on main do not reach `@v0` callers until the normal kit publication updates that tag; do not move it while a workflow change is still under review.

The success gates follow [GitHub Actions dependency and status-check semantics](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax#jobsjob_idneeds). Publisher authentication follows [npm trusted publishing](https://docs.npmjs.com/trusted-publishers/); authentication does not replace artifact identity or delivery readback.

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

Bump `version` in `Cargo.toml` and merge. `publish.yml` publishes the crate to crates.io, tags `vX.Y.Z` and moves `v0`, which the servers' release workflows use. A change to the release workflow reaches the servers only with a version bump.
