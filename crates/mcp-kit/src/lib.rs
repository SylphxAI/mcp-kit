//! Shared parts of the Sylphx MCP servers (repomap, lockdocs, anymd).
//!
//! - [`server`]: run an MCP server over stdio. You describe tools as JSON and
//!   answer calls; the kit handles the protocol through [rmcp], the official
//!   Rust MCP SDK.
//! - [`roots`]: choose which directory a call works on, from an explicit
//!   argument, environment variables, a launch default, or the client's roots.
//! - [`setup`]: register the server with Claude Code, Codex, Cursor, VS Code,
//!   Claude Desktop, Windsurf and Gemini CLI, and optionally add a Claude Code hook.
//! - [`licence`] (feature `licence`): sell a Pro tier with an offline-verified
//!   Ed25519 token, a polite upgrade notice and a `licence` CLI.
//! - [`remote`] (feature `remote`): serve the same app over Streamable HTTP as
//!   an OAuth resource server (RFC 9728 metadata, audience-bound JWT bearer
//!   check, RFC 6750 challenges).
//! - [`embed`] (feature `embed`): local embeddings with a small static model,
//!   downloaded once and shared by every Sylphx tool.

#[cfg(feature = "embed")]
pub mod embed;
#[cfg(feature = "licence")]
pub mod licence;
pub mod cache;
pub mod star_hint;
#[cfg(feature = "remote")]
pub mod remote;
#[cfg(feature = "search")]
pub mod search;
pub mod roots;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "setup")]
pub mod setup;

#[cfg(feature = "server")]
pub use rmcp;
