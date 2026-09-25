//! A stdio MCP server on rmcp. The app lists its tools as JSON and answers
//! calls with text; the kit does the protocol, runs each call on a blocking
//! thread, and fetches the client's roots when the client offers them.

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How the server introduces itself.
#[derive(Debug, Clone)]
pub struct Info {
    pub name: String,
    pub title: String,
    pub version: String,
    pub website: String,
    /// Shown to the agent: when and how to use the tools.
    pub instructions: String,
}

/// What a tool call gets besides its arguments.
#[derive(Debug, Clone, Default)]
pub struct Call {
    /// The client's roots that exist on this machine (MCP `roots/list`).
    pub client_roots: Vec<PathBuf>,
}

/// An MCP server: its identity, its tools, and how it answers a call.
pub trait App: Send + Sync + 'static {
    fn info(&self) -> Info;
    /// Tool definitions, each `{name, description, inputSchema, ...}` as in the MCP spec.
    fn tools(&self) -> Vec<Value>;
    /// Answer one call. Runs on a blocking thread. `Err` becomes a tool error
    /// the agent can read (`isError: true`).
    fn call(&self, name: &str, args: &Value, call: &Call) -> Result<String, String>;
    /// Called once the client is ready, on a blocking thread; e.g. to warm a cache.
    fn warm(&self, _call: &Call) {}
}

struct Handler<A: App> {
    app: Arc<A>,
    /// Client roots, fetched on first use and dropped when the client says they changed.
    roots: Arc<Mutex<Option<Vec<PathBuf>>>>,
}

impl<A: App> Handler<A> {
    // MCP roots are deprecated by SEP-2577 but are still how today's clients
    // (Claude Code, Cursor, VS Code) say which folder is open.
    #[allow(deprecated)]
    async fn client_roots(&self, peer: &rmcp::Peer<RoleServer>) -> Vec<PathBuf> {
        if let Some(r) = self.roots.lock().unwrap().clone() {
            return r;
        }
        let offered = peer.peer_info().is_some_and(|i| i.capabilities.roots.is_some());
        if !offered {
            return Vec::new();
        }
        let roots: Vec<PathBuf> = match tokio::time::timeout(Duration::from_secs(3), peer.list_roots()).await {
            Ok(Ok(res)) => res.roots.iter().filter_map(|r| crate::roots::file_uri_to_path(&r.uri)).collect(),
            _ => return Vec::new(),
        };
        *self.roots.lock().unwrap() = Some(roots.clone());
        roots
    }
}

/// Parse the app's JSON tool definitions into rmcp tools.
pub fn parse_tools(defs: Vec<Value>) -> Result<Vec<Tool>, serde_json::Error> {
    defs.into_iter().map(serde_json::from_value).collect()
}

impl<A: App> ServerHandler for Handler<A> {
    fn get_info(&self) -> ServerConfig {
        let i = self.app.info();
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(i.name, i.version).with_title(i.title).with_website_url(i.website))
            .with_instructions(i.instructions)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = parse_tools(self.app.tools()).map_err(|e| McpError::internal_error(format!("bad tool definition: {e}"), None))?;
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let call = Call { client_roots: self.client_roots(&context.peer).await };
        let app = self.app.clone();
        let name = request.name.to_string();
        let args = Value::Object(request.arguments.unwrap_or_default());
        let out = tokio::task::spawn_blocking(move || app.call(&name, &args, &call))
            .await
            .map_err(|e| McpError::internal_error(format!("tool call failed: {e}"), None))?;
        Ok(CallToolResponse::Complete(match out {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(text) => CallToolResult::error(vec![ContentBlock::text(text)]),
        }))
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        let call = Call { client_roots: self.client_roots(&context.peer).await };
        let app = self.app.clone();
        tokio::task::spawn_blocking(move || app.warm(&call));
    }

    async fn on_roots_list_changed(&self, _context: NotificationContext<RoleServer>) {
        *self.roots.lock().unwrap() = None;
    }
}

/// Serve `app` over any transport (stdio in production, a pipe in tests).
pub async fn serve<A, T, E, M>(app: A, transport: T) -> anyhow::Result<()>
where
    A: App,
    T: rmcp::transport::IntoTransport<RoleServer, E, M>,
    E: std::error::Error + Send + Sync + 'static,
{
    let handler = Handler { app: Arc::new(app), roots: Arc::new(Mutex::new(None)) };
    let running = handler.serve(transport).await?;
    running.waiting().await?;
    Ok(())
}

/// Serve `app` on stdin/stdout until the client disconnects.
pub fn run_stdio<A: App>(app: A) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(serve(app, rmcp::transport::stdio()))
}
