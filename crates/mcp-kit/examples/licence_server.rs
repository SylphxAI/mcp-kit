//! A minimal server with one free tool and one Pro tool.
//!
//! `cargo run --example licence_server --features licence,server -- licence status`
use mcp_kit::licence::{require, run_cli, LicencePolicy};
use mcp_kit::server::{run_stdio, App, Call, Info};
use serde_json::{json, Value};

const POLICY: LicencePolicy = LicencePolicy {
    product: "example",
    accepted_plans: &["pro", "team"],
    // Replace with your product's public key (base64url raw Ed25519).
    public_keys: &["xO9jSvEq5nsVPMk9x62Egr0_n5WPpWCF8yCYmrwzH3Y"],
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
    fn call(&self, name: &str, _args: &Value, _call: &Call) -> Result<String, String> {
        match name {
            "free_tool" => Ok("free result".into()),
            // Not an error: the agent relays the notice. `App::call` returns text, so
            // return the notice text; a server that builds its own rmcp results can
            // return `licence::required_result` to add the structured `pro_required` field.
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
