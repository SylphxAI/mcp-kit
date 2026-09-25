//! The server end to end: an rmcp client talks to a kit server over a pipe.

use mcp_kit::rmcp::model::CallToolRequestParams;
use mcp_kit::rmcp::ServiceExt;
use mcp_kit::server::{serve, App, Call, Info};
use serde_json::{json, Value};

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
    assert_eq!(info.server_info.name, "echo");
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
