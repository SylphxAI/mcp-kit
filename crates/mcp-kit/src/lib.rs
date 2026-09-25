//! Shared parts of the Sylphx MCP servers (repomap, lockdocs, anymd).
//!
//! - [`server`]: run an MCP server over stdio. You describe tools as JSON and
//!   answer calls; the kit handles the protocol through [rmcp], the official
//!   Rust MCP SDK.
//! - [`roots`]: choose which directory a call works on, from an explicit
//!   argument, environment variables, a launch default, or the client's roots.
//! - [`setup`]: register the server with Claude Code, Codex, Cursor, VS Code,
//!   Claude Desktop, Windsurf and Gemini CLI, and optionally add a Claude Code hook.

pub mod roots;
pub mod server;
pub mod setup;

pub use rmcp;
