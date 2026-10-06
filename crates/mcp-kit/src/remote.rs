//! A remote MCP server: Streamable HTTP on rmcp, protected as an OAuth 2.1
//! resource server the way the MCP authorization spec asks.
//!
//! - `POST <path>` is the MCP endpoint, served statelessly by rmcp's
//!   Streamable HTTP service (every request stands alone, so any replica
//!   answers it).
//! - `GET /.well-known/oauth-protected-resource<path>` (and the bare
//!   `/.well-known/oauth-protected-resource`) is the RFC 9728 metadata: the
//!   resource, its authorization server and its scopes.
//! - Every MCP request needs `Authorization: Bearer <JWT access token>`
//!   (RFC 9068) signed by a key in the issuer's JWKS, issued by the configured
//!   issuer, and with the resource URL in `aud` (RFC 8707). A missing or bad
//!   token is 401 and a missing scope is 403 `insufficient_scope`, each with
//!   an RFC 6750 `WWW-Authenticate` challenge that names the metadata URL.
//! - The token's subject and scopes reach the app as a [`Principal`] in
//!   [`App::call_as`](crate::server::App::call_as).
//!
//! ```no_run
//! # use mcp_kit::server::{App, Call, Info};
//! # struct MyApp;
//! # impl App for MyApp {
//! #   fn info(&self) -> Info { unimplemented!() }
//! #   fn tools(&self) -> Vec<serde_json::Value> { vec![] }
//! #   fn call(&self, _: &str, _: &serde_json::Value, _: &Call) -> Result<String, String> { Ok(String::new()) }
//! # }
//! use mcp_kit::remote::{serve, Remote};
//!
//! # async fn run() -> anyhow::Result<()> {
//! let remote = Remote::new("https://example.com/mcp", "https://auth.example.com")
//!     .scopes(["things:read", "things:write"])
//!     .require(["things:read"])
//!     .tool_scopes("make_thing", ["things:write"]);
//! serve(MyApp, remote, "0.0.0.0:8080").await
//! # }
//! ```

use crate::server::App;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Signature algorithms accepted on an access token. `none` and HMAC are
/// never accepted: a resource server holds no shared secret.
const ALGORITHMS: &[Algorithm] = &[
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::EdDSA,
];
/// Clock skew allowed on `exp` and `nbf`.
const LEEWAY_SECS: u64 = 60;
/// Fetched keys are refreshed after this long; the last good set stays in use
/// while a refresh fails.
const KEYS_TTL: Duration = Duration::from_secs(600);
/// An unknown `kid` refetches the keys at most this often.
const REFETCH_MIN: Duration = Duration::from_secs(30);
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FETCH_BODY: u64 = 1024 * 1024;
/// Largest MCP request body the scope check reads (rmcp's own default).
const MAX_BODY: usize = 4 * 1024 * 1024;

/// Who called: read from a verified access token.
#[derive(Debug, Clone, PartialEq)]
pub struct Principal {
    /// The token's `sub`.
    pub subject: String,
    /// The granted scopes (`scope`, or `scp` for issuers that use it).
    pub scopes: Vec<String>,
    /// Every claim of the token, for app-specific ones such as `org_id`.
    pub claims: Value,
}

impl Principal {
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

/// How a remote MCP server is protected. Build it with [`Remote::new`].
#[derive(Debug, Clone)]
pub struct Remote {
    resource: String,
    issuer: String,
    jwks_uri: Option<String>,
    keys: Option<JwkSet>,
    scopes: Vec<String>,
    required: Vec<String>,
    tool_scopes: BTreeMap<String, Vec<String>>,
    name: Option<String>,
    documentation: Option<String>,
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
}

impl Remote {
    /// `resource` is the MCP endpoint's public URL (for example
    /// `https://example.com/mcp`): it is the token audience and the RFC 9728
    /// resource id, and its path is where the server listens. `issuer` is the
    /// authorization server whose keys sign the tokens.
    pub fn new(resource: impl Into<String>, issuer: impl Into<String>) -> Self {
        let resource = resource.into();
        let allowed_hosts = resource
            .parse::<Uri>()
            .ok()
            .and_then(|u| u.authority().map(|a| vec![a.host().to_string(), a.as_str().to_string()]))
            .unwrap_or_default();
        Self {
            resource,
            issuer: issuer.into(),
            jwks_uri: None,
            keys: None,
            scopes: Vec::new(),
            required: Vec::new(),
            tool_scopes: BTreeMap::new(),
            name: None,
            documentation: None,
            allowed_hosts,
            allowed_origins: Vec::new(),
        }
    }

    /// The issuer's JWKS URL. Without it the URL is discovered from the
    /// issuer's RFC 8414 metadata (then OpenID Connect discovery).
    pub fn jwks_uri(mut self, url: impl Into<String>) -> Self {
        self.jwks_uri = Some(url.into());
        self
    }

    /// Fixed verification keys instead of fetching them (tests, pinned keys).
    pub fn keys(mut self, keys: JwkSet) -> Self {
        self.keys = Some(keys);
        self
    }

    /// The scopes advertised in the metadata (`scopes_supported`).
    pub fn scopes<S: Into<String>>(mut self, scopes: impl IntoIterator<Item = S>) -> Self {
        self.scopes = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// Scopes every MCP request must hold.
    pub fn require<S: Into<String>>(mut self, scopes: impl IntoIterator<Item = S>) -> Self {
        self.required = scopes.into_iter().map(Into::into).collect();
        self
    }

    /// Extra scopes a call of the tool `tool` must hold. A call without them
    /// is 403 `insufficient_scope`, so the client can ask for them (step-up).
    pub fn tool_scopes<S: Into<String>>(mut self, tool: impl Into<String>, scopes: impl IntoIterator<Item = S>) -> Self {
        self.tool_scopes.insert(tool.into(), scopes.into_iter().map(Into::into).collect());
        self
    }

    /// `resource_name` and `resource_documentation` in the metadata.
    pub fn describe(mut self, name: impl Into<String>, documentation: Option<String>) -> Self {
        self.name = Some(name.into());
        self.documentation = documentation;
        self
    }

    /// `Host` values accepted (DNS-rebinding defence). The default is the
    /// resource URL's host.
    pub fn allowed_hosts<S: Into<String>>(mut self, hosts: impl IntoIterator<Item = S>) -> Self {
        self.allowed_hosts = hosts.into_iter().map(Into::into).collect();
        self
    }

    /// Browser origins accepted, for MCP clients that run in a web page. A
    /// request with another `Origin` is refused; the default accepts any.
    pub fn allowed_origins<S: Into<String>>(mut self, origins: impl IntoIterator<Item = S>) -> Self {
        self.allowed_origins = origins.into_iter().map(Into::into).collect();
        self
    }

    fn path(&self) -> String {
        let path = self.resource.parse::<Uri>().map(|u| u.path().to_string()).unwrap_or_default();
        match path.trim_end_matches('/') {
            "" => "/".into(),
            p => p.into(),
        }
    }

    /// The RFC 9728 metadata URL: the well-known prefix inserted before the
    /// resource's path.
    pub fn metadata_url(&self) -> String {
        let (origin, path) = match self.resource.parse::<Uri>() {
            Ok(u) => (
                format!("{}://{}", u.scheme_str().unwrap_or("https"), u.authority().map(|a| a.as_str()).unwrap_or_default()),
                self.path(),
            ),
            Err(_) => (self.resource.clone(), "/".into()),
        };
        let path = if path == "/" { String::new() } else { path };
        format!("{origin}/.well-known/oauth-protected-resource{path}")
    }

    /// The RFC 9728 metadata document.
    pub fn metadata(&self) -> Value {
        let mut doc = json!({
            "resource": self.resource,
            "authorization_servers": [self.issuer],
            "bearer_methods_supported": ["header"],
        });
        let mut scopes: Vec<&String> = self.scopes.iter().chain(&self.required).chain(self.tool_scopes.values().flatten()).collect();
        let mut seen = BTreeSet::new();
        scopes.retain(|s| seen.insert(*s));
        if !scopes.is_empty() {
            doc["scopes_supported"] = json!(scopes);
        }
        if let Some(name) = &self.name {
            doc["resource_name"] = json!(name);
        }
        if let Some(docs) = &self.documentation {
            doc["resource_documentation"] = json!(docs);
        }
        doc
    }
}

/// Why a bearer was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No bearer token: 401 with the plain challenge.
    Missing,
    /// Bad signature, issuer, audience or lifetime: 401 `invalid_token`.
    Invalid(String),
    /// Valid, but without these scopes: 403 `insufficient_scope`.
    InsufficientScope(Vec<String>),
    /// The issuer's keys could not be read and none are cached: 503.
    Unavailable(String),
}

/// Verifies access tokens for one resource against the issuer's keys.
pub struct Verifier {
    remote: Remote,
    keys: Mutex<Keys>,
}

#[derive(Default)]
struct Keys {
    set: Option<JwkSet>,
    fetched: Option<Instant>,
    attempted: Option<Instant>,
}

impl Verifier {
    pub fn new(remote: Remote) -> Self {
        let keys = Keys { set: remote.keys.clone(), ..Keys::default() };
        Self { remote, keys: Mutex::new(keys) }
    }

    /// Check a bearer token: signature, issuer, audience, lifetime and the
    /// scopes every request needs.
    pub async fn verify(&self, token: &str) -> Result<Principal, Refusal> {
        let head = jsonwebtoken::decode_header(token).map_err(|e| Refusal::Invalid(format!("malformed token: {e}")))?;
        if !ALGORITHMS.contains(&head.alg) {
            return Err(Refusal::Invalid(format!("algorithm {:?} is not accepted", head.alg)));
        }
        let jwk = self.key_for(head.kid.as_deref()).await?;
        let key = DecodingKey::from_jwk(&jwk).map_err(|e| Refusal::Invalid(format!("unusable key: {e}")))?;
        if let Some(alg) = jwk.common.key_algorithm {
            if format!("{alg:?}") != format!("{:?}", head.alg) {
                return Err(Refusal::Invalid("the token's algorithm does not match its key".into()));
            }
        }
        let mut v = Validation::new(head.alg);
        v.leeway = LEEWAY_SECS;
        v.validate_nbf = true;
        v.set_issuer(&[&self.remote.issuer]);
        v.set_audience(&[&self.remote.resource]);
        v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let claims = jsonwebtoken::decode::<Value>(token, &key, &v)
            .map_err(|e| Refusal::Invalid(e.to_string()))?
            .claims;
        let principal = Principal {
            subject: claims["sub"].as_str().unwrap_or_default().to_string(),
            scopes: scopes_of(&claims),
            claims,
        };
        let missing = missing(&principal, &self.remote.required);
        if !missing.is_empty() {
            return Err(Refusal::InsufficientScope(missing));
        }
        Ok(principal)
    }

    async fn key_for(&self, kid: Option<&str>) -> Result<jsonwebtoken::jwk::Jwk, Refusal> {
        let pick = |set: &JwkSet| match kid {
            Some(k) => set.find(k).cloned(),
            None if set.keys.len() == 1 => set.keys.first().cloned(),
            None => None,
        };
        let fixed = self.remote.keys.is_some();
        let (found, refetch) = {
            let k = self.keys.lock().unwrap();
            let found = k.set.as_ref().and_then(pick);
            let stale = k.fetched.is_none_or(|t| t.elapsed() > KEYS_TTL);
            let may_try = k.attempted.is_none_or(|t| t.elapsed() > REFETCH_MIN);
            (found.clone(), !fixed && may_try && (found.is_none() || stale))
        };
        if refetch {
            self.keys.lock().unwrap().attempted = Some(Instant::now());
            match self.fetch_keys().await {
                Ok(set) => {
                    let mut k = self.keys.lock().unwrap();
                    let hit = pick(&set);
                    k.set = Some(set);
                    k.fetched = Some(Instant::now());
                    if let Some(hit) = hit {
                        return Ok(hit);
                    }
                    return Err(Refusal::Invalid("no key of the issuer matches the token".into()));
                }
                Err(e) => {
                    if found.is_none() && self.keys.lock().unwrap().set.is_none() {
                        return Err(Refusal::Unavailable(e));
                    }
                }
            }
        }
        found.ok_or_else(|| Refusal::Invalid("no key of the issuer matches the token".into()))
    }

    async fn fetch_keys(&self) -> Result<JwkSet, String> {
        let issuer = self.remote.issuer.clone();
        let jwks_uri = self.remote.jwks_uri.clone();
        tokio::task::spawn_blocking(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(FETCH_TIMEOUT))
                .http_status_as_error(false)
                .user_agent(concat!("sylphx-mcp-kit/", env!("CARGO_PKG_VERSION")))
                .build()
                .into();
            let uri = match jwks_uri {
                Some(u) => u,
                None => discover_jwks_uri(&agent, &issuer)?,
            };
            serde_json::from_value(get_json(&agent, &uri)?).map_err(|e| format!("{uri}: not a JWK set: {e}"))
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

fn get_json(agent: &ureq::Agent, url: &str) -> Result<Value, String> {
    let mut res = agent.get(url).call().map_err(|e| format!("{url}: {e}"))?;
    if res.status() != 200 {
        return Err(format!("{url}: HTTP {}", res.status().as_u16()));
    }
    let body = res
        .body_mut()
        .with_config()
        .limit(MAX_FETCH_BODY)
        .read_to_string()
        .map_err(|e| format!("{url}: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("{url}: {e}"))
}

/// RFC 8414 discovery (well-known prefix before the issuer's path), then
/// OpenID Connect discovery. The document must name the same issuer.
fn discover_jwks_uri(agent: &ureq::Agent, issuer: &str) -> Result<String, String> {
    let trimmed = issuer.trim_end_matches('/');
    let mut urls = Vec::new();
    if let Ok(u) = issuer.parse::<Uri>() {
        let origin = format!("{}://{}", u.scheme_str().unwrap_or("https"), u.authority().map(|a| a.as_str()).unwrap_or_default());
        let path = u.path().trim_end_matches('/');
        urls.push(format!("{origin}/.well-known/oauth-authorization-server{path}"));
    }
    urls.push(format!("{trimmed}/.well-known/openid-configuration"));
    let mut last = String::from("no discovery URL");
    for url in urls {
        match get_json(agent, &url) {
            Ok(doc) => {
                if doc["issuer"].as_str().map(|i| i.trim_end_matches('/')) != Some(trimmed) {
                    return Err(format!("{url}: names another issuer"));
                }
                return doc["jwks_uri"].as_str().map(str::to_string).ok_or_else(|| format!("{url}: no jwks_uri"));
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn scopes_of(claims: &Value) -> Vec<String> {
    match (&claims["scope"], &claims["scp"]) {
        (Value::String(s), _) | (_, Value::String(s)) => s.split_whitespace().map(str::to_string).collect(),
        (_, Value::Array(a)) => a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

fn missing(p: &Principal, needed: &[String]) -> Vec<String> {
    needed.iter().filter(|s| !p.has_scope(s)).cloned().collect()
}

/// Escape a value for a quoted-string in a `WWW-Authenticate` parameter.
fn quoted(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect::<String>().replace('\\', "\\\\").replace('"', "\\\"")
}

/// The RFC 6750 response for a refusal, with the MCP `resource_metadata`
/// parameter (RFC 9728 section 5.1).
pub fn refusal_response(remote: &Remote, refusal: &Refusal) -> Response {
    let meta = quoted(&remote.metadata_url());
    let scope = |s: &[String]| if s.is_empty() { String::new() } else { format!(r#", scope="{}""#, quoted(&s.join(" "))) };
    let (status, challenge, body) = match refusal {
        Refusal::Missing => (
            StatusCode::UNAUTHORIZED,
            format!(r#"Bearer resource_metadata="{meta}"{}"#, scope(&remote.required)),
            json!({"error": "invalid_request", "error_description": "a bearer access token is required"}),
        ),
        Refusal::Invalid(why) => (
            StatusCode::UNAUTHORIZED,
            format!(r#"Bearer error="invalid_token", error_description="{}", resource_metadata="{meta}"{}"#, quoted(why), scope(&remote.required)),
            json!({"error": "invalid_token", "error_description": why}),
        ),
        Refusal::InsufficientScope(needed) => (
            StatusCode::FORBIDDEN,
            format!(r#"Bearer error="insufficient_scope", resource_metadata="{meta}"{}"#, scope(needed)),
            json!({"error": "insufficient_scope", "error_description": format!("requires scope: {}", needed.join(" "))}),
        ),
        Refusal::Unavailable(why) => (
            StatusCode::SERVICE_UNAVAILABLE,
            String::new(),
            json!({"error": "temporarily_unavailable", "error_description": why}),
        ),
    };
    let mut r = (status, axum::Json(body)).into_response();
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(v) = HeaderValue::from_str(&challenge) {
        if !challenge.is_empty() {
            r.headers_mut().insert(header::WWW_AUTHENTICATE, v);
        }
    }
    r
}

/// The bearer of an `Authorization` header (scheme is case-insensitive).
fn bearer(req: &Request) -> Option<&str> {
    let v = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = v.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

/// The tools a JSON-RPC body calls (`tools/call` requests, single or batch).
fn called_tools(body: &[u8]) -> Vec<String> {
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return Vec::new() };
    let msgs = match v {
        Value::Array(a) => a,
        one => vec![one],
    };
    msgs.iter()
        .filter(|m| m["method"] == "tools/call")
        .filter_map(|m| m["params"]["name"].as_str().map(str::to_string))
        .collect()
}

async fn guard(State(verifier): State<Arc<Verifier>>, req: Request, next: Next) -> Response {
    let remote = &verifier.remote;
    let Some(token) = bearer(&req) else {
        return refusal_response(remote, &Refusal::Missing);
    };
    let principal = match verifier.verify(token).await {
        Ok(p) => p,
        Err(r) => return refusal_response(remote, &r),
    };
    let (parts, body) = req.into_parts();
    let mut req = if remote.tool_scopes.is_empty() {
        Request::from_parts(parts, body)
    } else {
        let bytes: Bytes = match axum::body::to_bytes(body, MAX_BODY).await {
            Ok(b) => b,
            Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        };
        let mut needed: Vec<String> = called_tools(&bytes)
            .iter()
            .filter_map(|t| remote.tool_scopes.get(t))
            .flat_map(|s| missing(&principal, s))
            .collect();
        needed.dedup();
        if !needed.is_empty() {
            return refusal_response(remote, &Refusal::InsufficientScope(needed));
        }
        Request::from_parts(parts, Body::from(bytes))
    };
    req.extensions_mut().insert(principal);
    next.run(req).await
}

/// The router: the MCP endpoint behind the bearer check, and the metadata.
pub fn router<A: App>(app: A, remote: Remote) -> Router {
    let app = Arc::new(app);
    let mut config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_allowed_hosts(remote.allowed_hosts.clone());
    if !remote.allowed_origins.is_empty() {
        config = config.with_allowed_origins(remote.allowed_origins.clone());
    }
    let mcp = StreamableHttpService::new(
        move || Ok(crate::server::remote_handler(app.clone())),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    let path = remote.path();
    let doc = Arc::new(remote.metadata());
    let metadata = get(move || {
        let doc = doc.clone();
        async move {
            let mut r = axum::Json((*doc).clone()).into_response();
            r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=3600"));
            r.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
            r
        }
    });
    let well_known = "/.well-known/oauth-protected-resource";
    let verifier = Arc::new(Verifier::new(remote));
    let guarded = Router::new()
        .route_service(&path, mcp)
        .layer(axum::middleware::from_fn_with_state(verifier, guard));
    let mut r = Router::new().merge(guarded).route(well_known, metadata.clone());
    if path != "/" {
        r = r.route(&format!("{well_known}{path}"), metadata);
    }
    r
}

/// Serve `app` over Streamable HTTP on `addr` until the process stops.
pub async fn serve<A: App>(app: A, remote: Remote, addr: &str) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router(app, remote)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_url_inserts_the_well_known_prefix() {
        assert_eq!(Remote::new("https://example.com/mcp", "i").metadata_url(), "https://example.com/.well-known/oauth-protected-resource/mcp");
        assert_eq!(Remote::new("https://x.dev/", "i").metadata_url(), "https://x.dev/.well-known/oauth-protected-resource");
        assert_eq!(Remote::new("https://x.dev", "i").path(), "/");
    }

    #[test]
    fn scopes_come_from_scope_or_scp() {
        assert_eq!(scopes_of(&json!({"scope": "a b"})), ["a", "b"]);
        assert_eq!(scopes_of(&json!({"scp": ["a"]})), ["a"]);
        assert!(scopes_of(&json!({})).is_empty());
    }

    #[test]
    fn called_tools_reads_single_and_batch() {
        assert_eq!(called_tools(br#"{"method":"tools/call","params":{"name":"x"}}"#), ["x"]);
        assert_eq!(called_tools(br#"[{"method":"tools/list"},{"method":"tools/call","params":{"name":"y"}}]"#), ["y"]);
        assert!(called_tools(b"not json").is_empty());
    }

    #[test]
    fn challenge_values_are_quoted_safely() {
        assert_eq!(quoted("a\"b\\c\n"), "a\\\"b\\\\c");
    }
}
