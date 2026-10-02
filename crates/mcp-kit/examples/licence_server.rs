//! A minimal server with one free tool and one Pro tool.
//!
//! `cargo run --example licence_server --features licence,server -- licence status`
use mcp_kit::licence::{require, required_result, run_cli, LicencePolicy};
use mcp_kit::rmcp::model::{CallToolResult, ContentBlock};
use mcp_kit::server::{run_stdio, App, Call, Info};
use serde_json::{json, Value};

const POLICY: LicencePolicy = LicencePolicy {
    product: "example",
    require_product: true,
    accepted_plans: &["pro", "team"],
    // Replace with your product's public key (base64url raw Ed25519).
    public_keys: &["REPLACE_WITH_YOUR_BASE64URL_ED25519_PUBLIC_KEY"],
    env_var: "EXAMPLE_LICENCE_TOKEN",
    file_name: "licence",
    upgrade_url: "https://example.com/pro",
};

struct Example;

impl App for Example {
    fn info(&self) -> Info {
        Info {
            name: "example".into(),
            title: "Example".into(),
            version: "1.0.0".into(),
            website: "https://example.com".into(),
            instructions: "A free tool and a Pro tool.".into(),
        }
    }
    fn tools(&self) -> Vec<Value> {
        ["free_tool", "pro_tool"]
            .map(|n| json!({"name": n, "description": n, "inputSchema": {"type": "object"}}))
            .to_vec()
    }
    fn call_result(&self, name: &str, args: &Value, call: &Call) -> CallToolResult {
        if name == "pro_tool" {
            if let Err(required) = require(&POLICY, "The Pro tool") {
                return required_result(&required);
            }
        }
        match self.call(name, args, call) {
            Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
            Err(text) => CallToolResult::error(vec![ContentBlock::text(text)]),
        }
    }
    fn call(&self, name: &str, _args: &Value, _call: &Call) -> Result<String, String> {
        match name {
            "free_tool" => Ok("free result".into()),
            // Not an error: the agent relays the notice. For the structured
            // `pro_required` field, override `App::call_result` (see below).
            "pro_tool" => match require(&POLICY, "The Pro tool") {
                Ok(_) => Ok("pro result".into()),
                Err(required) => Ok(required.to_string()),
            },
            _ => Err(format!("unknown tool {name}")),
        }
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("licence") {
        std::process::exit(run_cli(&POLICY, &args[1..]));
    }
    run_stdio(Example)
}
