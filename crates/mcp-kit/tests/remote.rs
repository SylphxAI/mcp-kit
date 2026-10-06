//! The remote server end to end: a real HTTP listener, an authorization
//! server that publishes its keys through RFC 8414 discovery, and signed
//! ES256 access tokens.

use axum::routing::get;
use axum::Router;
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use mcp_kit::remote::{router, Principal, Remote};
use mcp_kit::rmcp::model::{CallToolResult, ContentBlock};
use mcp_kit::server::{App, Call, Info};
use p256::pkcs8::EncodePrivateKey;
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

struct WhoAmI;

impl App for WhoAmI {
    fn info(&self) -> Info {
        Info { name: "whoami".into(), title: "Who am I".into(), version: "1.0.0".into(), website: "https://example.com".into(), instructions: "Says who called.".into() }
    }
    fn tools(&self) -> Vec<Value> {
        let schema = json!({"type": "object"});
        vec![json!({"name": "whoami", "inputSchema": schema}), json!({"name": "make", "inputSchema": schema})]
    }
    fn call(&self, _name: &str, _args: &Value, _call: &Call) -> Result<String, String> {
        Err("a remote call always has a caller".into())
    }
    fn call_as(&self, name: &str, _args: &Value, _call: &Call, p: &Principal) -> CallToolResult {
        CallToolResult::success(vec![ContentBlock::text(format!("{name} {}", p.subject))])
    }
}

struct Signer {
    key: EncodingKey,
    kid: &'static str,
}

impl Signer {
    fn new(seed: u8, kid: &'static str) -> Self {
        let sk = p256::SecretKey::from_slice(&[seed; 32]).unwrap();
        Self { key: EncodingKey::from_ec_der(sk.to_pkcs8_der().unwrap().as_bytes()), kid }
    }
    fn jwk(&self) -> Jwk {
        let mut jwk = Jwk::from_encoding_key(&self.key, Algorithm::ES256).unwrap();
        jwk.common.key_id = Some(self.kid.into());
        jwk
    }
    fn sign(&self, claims: Value) -> String {
        let mut h = Header::new(Algorithm::ES256);
        h.kid = Some(self.kid.into());
        h.typ = Some("at+jwt".into());
        jsonwebtoken::encode(&h, &claims, &self.key).unwrap()
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

struct Env {
    base: String,
    resource: String,
    issuer: String,
    signer: Signer,
}

impl Env {
    fn claims(&self, scope: &str) -> Value {
        json!({"iss": self.issuer, "aud": self.resource, "sub": "user-1", "scope": scope, "iat": now(), "exp": now() + 600})
    }
}

/// One listener serves the MCP resource and, at its root, the authorization
/// server (RFC 8414 metadata and the JWKS). Only `signer` is published.
async fn start() -> Env {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let resource = format!("{base}/mcp");
    let issuer = base.clone();
    let signer = Signer::new(7, "k1");
    let jwks = serde_json::to_value(JwkSet { keys: vec![signer.jwk()] }).unwrap();
    let as_meta = json!({"issuer": issuer, "jwks_uri": format!("{base}/jwks")});
    let remote = Remote::new(resource.clone(), issuer.clone())
        .scopes(["creations:read", "creations:write"])
        .require(["creations:read"])
        .tool_scopes("make", ["creations:write"])
        .describe("Test", None);
    let app = router(WhoAmI, remote)
        .merge(Router::new()
            .route("/.well-known/oauth-authorization-server", get(move || async move { axum::Json(as_meta) }))
            .route("/jwks", get(move || async move { axum::Json(jwks) })));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Env { base, resource, issuer, signer }
}

struct Reply {
    status: u16,
    challenge: String,
    body: Value,
}

async fn send(url: String, token: Option<String>, body: Option<Value>) -> Reply {
    tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
        let mut res = match body {
            Some(b) => {
                let mut req = agent
                    .post(&url)
                    .header("content-type", "application/json")
                    .header("accept", "application/json, text/event-stream")
                    .header("mcp-protocol-version", "2025-06-18");
                if let Some(t) = &token {
                    req = req.header("authorization", &format!("Bearer {t}"));
                }
                req.send(b.to_string()).unwrap()
            }
            None => agent.get(&url).call().unwrap(),
        };
        let challenge = res.headers().get("www-authenticate").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        let text = res.body_mut().read_to_string().unwrap();
        Reply { status: res.status().as_u16(), challenge, body: serde_json::from_str(&text).unwrap_or(Value::String(text)) }
    })
    .await
    .unwrap()
}

fn initialize() -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}})
}

fn call(tool: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": tool, "arguments": {}}})
}

#[tokio::test(flavor = "multi_thread")]
async fn metadata_names_the_resource_and_its_issuer() {
    let env = start().await;
    for path in ["/.well-known/oauth-protected-resource/mcp", "/.well-known/oauth-protected-resource"] {
        let r = send(format!("{}{path}", env.base), None, None).await;
        assert_eq!(r.status, 200, "{path}");
        assert_eq!(r.body["resource"], env.resource);
        assert_eq!(r.body["authorization_servers"], json!([env.issuer]));
        assert_eq!(r.body["scopes_supported"], json!(["creations:read", "creations:write"]));
        assert_eq!(r.body["bearer_methods_supported"], json!(["header"]));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn no_token_is_401_with_the_metadata_url() {
    let env = start().await;
    let r = send(env.resource.clone(), None, Some(initialize())).await;
    assert_eq!(r.status, 401);
    assert_eq!(
        r.challenge,
        format!(r#"Bearer resource_metadata="{}/.well-known/oauth-protected-resource/mcp", scope="creations:read""#, env.base)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_for_another_audience_is_401() {
    let env = start().await;
    let mut claims = env.claims("creations:read");
    claims["aud"] = json!("https://other.example/mcp");
    let r = send(env.resource.clone(), Some(env.signer.sign(claims)), Some(initialize())).await;
    assert_eq!(r.status, 401);
    assert!(r.challenge.starts_with(r#"Bearer error="invalid_token""#), "{}", r.challenge);
    assert!(r.challenge.contains("resource_metadata="), "{}", r.challenge);
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_issuer_expiry_and_unknown_keys_are_401() {
    let env = start().await;
    let mut wrong_iss = env.claims("creations:read");
    wrong_iss["iss"] = json!("https://evil.example");
    let mut expired = env.claims("creations:read");
    expired["exp"] = json!(now() - 3600);
    let stranger = Signer::new(9, "k2").sign(env.claims("creations:read"));
    let forged = Signer::new(9, "k1").sign(env.claims("creations:read"));
    for token in [env.signer.sign(wrong_iss), env.signer.sign(expired), stranger, forged, "not.a.jwt".into()] {
        let r = send(env.resource.clone(), Some(token), Some(initialize())).await;
        assert_eq!(r.status, 401);
        assert!(r.challenge.starts_with(r#"Bearer error="invalid_token""#), "{}", r.challenge);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_scope_is_403_insufficient_scope() {
    let env = start().await;
    let r = send(env.resource.clone(), Some(env.signer.sign(env.claims("account:read"))), Some(initialize())).await;
    assert_eq!(r.status, 403);
    assert_eq!(r.body["error"], "insufficient_scope");
    assert!(r.challenge.starts_with(r#"Bearer error="insufficient_scope""#), "{}", r.challenge);
    assert!(r.challenge.ends_with(r#"scope="creations:read""#), "{}", r.challenge);

    // A tool that needs more than the base scope: step-up, not a tool error.
    let r = send(env.resource.clone(), Some(env.signer.sign(env.claims("creations:read"))), Some(call("make"))).await;
    assert_eq!(r.status, 403);
    assert!(r.challenge.ends_with(r#"scope="creations:write""#), "{}", r.challenge);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_valid_token_reaches_the_app_as_its_caller() {
    let env = start().await;
    let token = env.signer.sign(env.claims("creations:read creations:write"));
    let r = send(env.resource.clone(), Some(token.clone()), Some(initialize())).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["result"]["serverInfo"]["name"], "whoami");

    for (tool, text) in [("whoami", "whoami user-1"), ("make", "make user-1")] {
        let r = send(env.resource.clone(), Some(token.clone()), Some(call(tool))).await;
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(r.body["result"]["content"][0]["text"], text);
    }
}
