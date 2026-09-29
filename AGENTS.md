# mcp-kit

The shared parts of the Sylphx MCP servers (repomap, lockdocs, anymd): a Rust
crate, an npm launcher and a reusable release workflow. Each server then keeps
only its own tools, and a fix here reaches all of them.

Layout: `crates/mcp-kit/src` (`server`, `setup`, `roots`, `embed`),
`npm/launcher.js`, `scripts/mcpb.mjs`, `.github/workflows/release.yml`
(reusable), `publish.yml` (releases the kit).

## Hard lines

- Keep the public API of `sylphx-mcp-kit` compatible within `0.x` minor
  versions: three servers build against it, and a break stops all of their
  releases.
- Changes to `release.yml` reach the servers only through a version bump
  (`publish.yml` moves the `v0` tag), so bump `version` in `Cargo.toml` with
  them.
- Embedding vectors must equal `model2vec` output: the servers' stored indexes
  depend on it.
- No product-specific logic here: a server's tools stay in its own repository.
- Follow the [owner standards](https://github.com/SylphxAI/owner/tree/main/standards).

## Judged by

CI (`ci.yml`): `cargo clippy` with `-D warnings` for all features and for
`embed` alone, `cargo test --all-features` on Linux, macOS and Windows,
"Embeddings match model2vec", the launcher exit-code test, `node --test
scripts/mcpb.test.mjs`, `plain-language` and `identifiers`.
