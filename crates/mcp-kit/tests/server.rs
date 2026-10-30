//! The server end to end: an rmcp client talks to a kit server over a pipe.

use mcp_kit::rmcp::model::CallToolRequestParams;
use mcp_kit::rmcp::ServiceExt;
use mcp_kit::server::{serve, App, Call, Info};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

struct Echo;

impl App for Echo {
    fn info(&self) -> Info {
        Info { name: "echo".into(), title: "Echo".into(), version: "1.0.0".into(), website: "https://example.com".into(), instructions: "Echoes.".into() }
    }
    fn tools(&self) -> Vec<Value> {
        vec![json!({"name": "echo", "description": "Echo `text`.", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}})]
    }
    fn call(&self, name: &str, args: &Value, _call: &Call) -> Result<String, String> {
        match (name, args.get("text").and_then(|t| t.as_str())) {
            ("echo", Some(t)) => Ok(t.to_string()),
            ("echo", None) => Err("`text` is required".into()),
            _ => Err(format!("unknown tool `{name}`")),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn lists_and_calls_tools() {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(serve(Echo, server_io));
    let client = ().serve(client_io).await.expect("client connects");
    let info = client.peer_info().expect("server info");
    assert_eq!(info.server_info.as_ref().map(|s| s.name.as_str()), Some("echo"));
    assert_eq!(info.instructions.as_deref(), Some("Echoes."));

    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");

    let ok = client.call_tool(CallToolRequestParams::new("echo").with_arguments(json!({"text": "hi"}).as_object().unwrap().clone())).await.unwrap();
    assert_eq!(ok.is_error, Some(false));
    assert_eq!(ok.content[0].as_text().unwrap().text, "hi");

    let err = client.call_tool(CallToolRequestParams::new("echo")).await.unwrap();
    assert_eq!(err.is_error, Some(true));
    client.cancel().await.unwrap();
}

async fn exchange(io: &mut BufReader<DuplexStream>, request: Value) -> Value {
    io.get_mut().write_all(format!("{request}\n").as_bytes()).await.unwrap();
    let mut line = String::new();
    tokio::time::timeout(std::time::Duration::from_secs(5), io.read_line(&mut line)).await.unwrap().unwrap();
    serde_json::from_str(&line).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn modern_list_has_required_private_uncached_hints() {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(serve(Echo, server_io));
    let mut io = BufReader::new(client_io);
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name": "wire-test", "version": "1.0.0"}
    });
    let discovery = exchange(&mut io, json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {"_meta": meta}})).await;
    assert!(discovery["result"]["supportedVersions"].as_array().unwrap().contains(&json!("2026-07-28")));
    let list = exchange(&mut io, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {"_meta": meta}})).await;
    assert_eq!(list["result"]["resultType"], "complete");
    assert_eq!(list["result"]["ttlMs"], 0);
    assert_eq!(list["result"]["cacheScope"], "private");
    assert_eq!(list["result"]["tools"][0]["name"], "echo");

    let unsupported = exchange(&mut io, json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {"_meta": {
        "io.modelcontextprotocol/protocolVersion": "2099-01-01",
        "io.modelcontextprotocol/clientCapabilities": {}
    }}})).await;
    assert_eq!(unsupported["error"]["code"], -32022);
    drop(io);
    server.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_list_keeps_its_original_wire_shape() {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(serve(Echo, server_io));
    let mut io = BufReader::new(client_io);
    let init = exchange(&mut io, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "wire-test", "version": "1.0.0"}
    }})).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    io.get_mut().write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n").await.unwrap();
    let list = exchange(&mut io, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).await;
    assert_eq!(list["result"]["tools"][0]["name"], "echo");
    for field in ["resultType", "ttlMs", "cacheScope"] {
        assert!(list["result"].get(field).is_none(), "legacy list gained {field}");
    }
    drop(io);
    server.await.unwrap().unwrap();
}
