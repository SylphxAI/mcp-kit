//! Sell a Pro tier with an offline-verified licence token.
//!
//! Token: `base64url(payloadJSON) + "." + base64url(Ed25519 signature over the
//! payload bytes)`, payload `{"plan", "email"?, "issuedAt" (ms), "product"?,
//! "order"?, "grant"?, "seats"?, "expiresAt"? (ms)}`. Unknown fields are
//! ignored. This is the format anymd and GPDT already verify. The token is
//! never logged or printed.
//!
//! A server describes itself once with a [`LicencePolicy`] (data, no product
//! code here), then gates a Pro feature with [`require`] and answers an
//! unlicensed call with [`required_result_json`], a normal (non-error) MCP
//! tool result the agent relays to the user. [`run_cli`] is the generic
//! `licence status | activate <token>` subcommand.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

mod buy;

/// What one server sells: who signs, which plans unlock it, where the token lives.
#[derive(Debug, Clone, Copy)]
pub struct LicencePolicy<'a> {
    /// Product name shown to the user ("lockdocs"). A token that names a
    /// different `product` is refused; see `require_product` for one without.
    pub product: &'a str,
    /// Refuse a token that does not name this product. True for any product
    /// launched on the Money issuer; false only for anymd back-compat.
    pub require_product: bool,
    /// Plans that unlock the Pro features ("pro", "team").
    pub accepted_plans: &'a [&'a str],
    /// Trusted public keys (base64url raw Ed25519). A list so rotation is additive.
    pub public_keys: &'a [&'a str],
    /// Env var holding the token (wins over the token file).
    pub env_var: &'a str,
    /// Token file name inside `<config dir>/<product>/`.
    pub file_name: &'a str,
    /// Where Pro is explained and sold, and where an expiring licence is renewed.
    pub upgrade_url: &'a str,
    /// The tier name users see: "Pro", "Team". Used in the notice
    /// ("<feature> is part of <product> <tier>"), `licence status` and the
    /// `pro_required` structured content.
    pub tier: &'a str,
    /// Base URL (https only) of the shared checkout service. With it,
    /// `licence buy` starts a purchase and activates the licence when paid;
    /// with `None`, `buy` prints `upgrade_url` instead.
    pub checkout_base: Option<&'a str>,
}

/// A verified licence.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Licence {
    pub plan: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(rename = "issuedAt")]
    pub issued_at: i64,
    #[serde(default)]
    pub product: Option<String>,
    #[serde(default)]
    pub order: Option<String>,
    #[serde(default)]
    pub grant: Option<String>,
    #[serde(default)]
    pub seats: Option<u32>,
    #[serde(default, rename = "expiresAt")]
    pub expires_at: Option<i64>,
}

impl Licence {
    /// True when the licence expires within `within` from now, or has already
    /// expired. A licence with no `expiresAt` never expires, so it is false.
    /// The window is inclusive: expiring exactly `within` from now is soon.
    pub fn expires_soon(&self, within: Duration) -> bool {
        self.expires_soon_at(within, now_ms())
    }

    fn expires_soon_at(&self, within: Duration, now: i64) -> bool {
        let window = i64::try_from(within.as_millis()).unwrap_or(i64::MAX);
        self.expires_at.is_some_and(|at| at.saturating_sub(now) <= window)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicenceError {
    Malformed,
    BadSignature,
    WrongPlan,
    WrongProduct,
    Expired,
}

impl fmt::Display for LicenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Malformed => "the licence token is malformed",
            Self::BadSignature => "the licence token signature is not valid",
            Self::WrongPlan => "the licence token is not for a plan that unlocks this feature",
            Self::WrongProduct => "the licence token is for a different product",
            Self::Expired => "the licence token has expired",
        })
    }
}

impl std::error::Error for LicenceError {}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl LicencePolicy<'_> {
    /// Verify a token: signature against any key, then plan, product and expiry.
    pub fn verify(&self, token: &str) -> Result<Licence, LicenceError> {
        self.verify_at(token, now_ms())
    }

    fn verify_at(&self, token: &str, now: i64) -> Result<Licence, LicenceError> {
        let (payload_b64, sig_b64) = token.trim().split_once('.').ok_or(LicenceError::Malformed)?;
        let payload = URL_SAFE_NO_PAD.decode(payload_b64).map_err(|_| LicenceError::Malformed)?;
        let sig = URL_SAFE_NO_PAD.decode(sig_b64).map_err(|_| LicenceError::Malformed)?;
        let signature = Signature::from_slice(&sig).map_err(|_| LicenceError::Malformed)?;
        let licence: Licence = serde_json::from_slice(&payload).map_err(|_| LicenceError::Malformed)?;
        let verified = self.public_keys.iter().any(|key| {
            URL_SAFE_NO_PAD
                .decode(key)
                .ok()
                .and_then(|raw| <[u8; 32]>::try_from(raw).ok())
                .and_then(|raw| VerifyingKey::from_bytes(&raw).ok())
                .is_some_and(|key| key.verify(&payload, &signature).is_ok())
        });
        if !verified {
            return Err(LicenceError::BadSignature);
        }
        if !self.accepted_plans.contains(&licence.plan.as_str()) {
            return Err(LicenceError::WrongPlan);
        }
        match licence.product.as_deref() {
            Some(p) if p == self.product => {}
            None if !self.require_product => {}
            _ => return Err(LicenceError::WrongProduct),
        }
        if licence.expires_at.is_some_and(|at| at <= now) {
            return Err(LicenceError::Expired);
        }
        Ok(licence)
    }

    /// The token file: `<config dir>/<product>/<file_name>`.
    pub fn token_path(&self) -> Option<PathBuf> {
        dirs::config_dir().map(|dir| dir.join(self.product).join(self.file_name))
    }

    /// The configured token: the env var first, else the token file.
    pub fn find_token(&self) -> Option<String> {
        self.find_token_source().map(|(token, _)| token)
    }

    /// Like [`find_token`](Self::find_token), also saying where it was read: the env var name or the file path.
    pub fn find_token_source(&self) -> Option<(String, String)> {
        let env = std::env::var(self.env_var).ok();
        let path = self.token_path();
        let from_env = env.as_deref().is_some_and(|t| !t.trim().is_empty());
        let source = if from_env {
            self.env_var.to_string()
        } else {
            path.as_ref()?.display().to_string()
        };
        find_token(env, path).map(|token| (token, source))
    }

    /// The active licence, or `None` when no valid token is configured.
    pub fn current(&self) -> Option<Licence> {
        self.verify(&self.find_token()?).ok()
    }
}

/// Verify a token against a policy.
pub fn verify(policy: &LicencePolicy, token: &str) -> Result<Licence, LicenceError> {
    policy.verify(token)
}

/// Env first (even when invalid: no silent fallback), else the file.
fn find_token(env: Option<String>, file: Option<PathBuf>) -> Option<String> {
    if let Some(token) = env.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
        return Some(token);
    }
    let text = std::fs::read_to_string(file?).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Returned by [`require`] when a Pro feature is used without a licence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProRequired {
    pub feature: String,
    pub product: String,
    pub tier: String,
    pub url: String,
}

impl fmt::Display for ProRequired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is part of {} {}. Learn more and get it: {}",
            self.feature, self.product, self.tier, self.url
        )
    }
}

impl std::error::Error for ProRequired {}

/// Gate for a Pro feature: the licence, or a polite, agent-relayable [`ProRequired`].
pub fn require(policy: &LicencePolicy, feature: &str) -> Result<Licence, ProRequired> {
    policy.current().ok_or_else(|| required(policy, feature))
}

fn required(policy: &LicencePolicy, feature: &str) -> ProRequired {
    ProRequired {
        feature: feature.to_string(),
        product: policy.product.to_string(),
        tier: policy.tier.to_string(),
        url: policy.upgrade_url.to_string(),
    }
}

/// The MCP `CallToolResult` JSON for an unlicensed call. It is NOT an error
/// (`isError` false), so the agent relays the text; `structuredContent` lets
/// it act on `pro_required`.
pub fn required_result_json(required: &ProRequired) -> Value {
    json!({
        "content": [{"type": "text", "text": required.to_string()}],
        "structuredContent": {"pro_required": {
            "feature": required.feature, "product": required.product, "tier": required.tier,
            "url": required.url}},
        "isError": false
    })
}

/// The same result as an rmcp type (needs the `server` feature too).
#[cfg(feature = "server")]
pub fn required_result(required: &ProRequired) -> rmcp::model::CallToolResult {
    let mut result =
        rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(required.to_string())]);
    result.structured_content = required_result_json(required).get("structuredContent").cloned();
    result
}

/// `licence status | activate <token> | buy [--pack <id>] [--qty <n>]
/// [--no-browser] [--json]`; mount it under any subcommand name and pass the
/// arguments after it. Returns the exit code.
pub fn run_cli(policy: &LicencePolicy, arguments: &[String]) -> i32 {
    let product = policy.product;
    match arguments.first().map(String::as_str) {
        Some("status") if arguments.len() == 1 => {
            print!("{}", status_report(policy, policy.find_token_source(), now_ms()));
            0
        }
        Some("activate") if arguments.len() == 2 => match activate(policy, &arguments[1]) {
            Ok(path) => {
                println!("{product} {} activated ({})", policy.tier, path.display());
                0
            }
            Err(message) => {
                eprintln!("{product} licence activate: {message}");
                1
            }
        },
        Some("buy") => buy::run(policy, &arguments[1..]),
        _ => {
            eprintln!("usage: licence status | licence activate <token> | licence buy [--pack <id>] [--qty <n>] [--no-browser] [--json]");
            2
        }
    }
}

/// How long before expiry `licence status` starts warning.
const EXPIRY_WARNING: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The `licence status` text for a token found as `(token, source)`, at `now` (ms).
fn status_report(policy: &LicencePolicy, found: Option<(String, String)>, now: i64) -> String {
    let (product, tier) = (policy.product, policy.tier);
    let verified = found.as_ref().map(|(token, _)| policy.verify_at(token, now));
    let mut out = String::new();
    if let Some((_, source)) = &found {
        out += &format!("token source: {source}\n");
    }
    match verified {
        Some(Ok(licence)) => {
            out += &format!("{product} {tier}: active\nplan: {}\nissuedAt: {}\n", licence.plan, licence.issued_at);
            if let Some(at) = licence.expires_at {
                out += &format!("expiresAt: {at}\n");
                if licence.expires_soon_at(EXPIRY_WARNING, now) {
                    // Round up, so the last day reads "1 day" until it has expired.
                    let days = (at - now + 86_399_999) / 86_400_000;
                    let unit = if days == 1 { "day" } else { "days" };
                    out += &format!("warning: this licence expires in {days} {unit}. Renew it: {}\n", policy.upgrade_url);
                }
            }
            if let Some(seats) = licence.seats {
                out += &format!("seats: {seats}\n");
            }
        }
        other => {
            out += &format!("{product} {tier}: inactive\n");
            if let Some(Err(error)) = other {
                out += &format!("token error: {error}\n");
            }
            out += &format!("Learn more and get it: {}\n", policy.upgrade_url);
        }
    }
    out
}

fn activate(policy: &LicencePolicy, token: &str) -> Result<PathBuf, String> {
    let path = policy.token_path().ok_or("no config directory on this machine")?;
    activate_at(policy, token, &path)?;
    Ok(path)
}

/// Verify a token, then save it at `path`. The one save path for `activate` and `buy`.
fn activate_at(policy: &LicencePolicy, token: &str, path: &Path) -> Result<(), String> {
    policy.verify(token).map_err(|e| e.to_string())?;
    write_token(path, token.trim()).map_err(|e| format!("cannot write the token file: {e}"))
}

/// Write the token file with 0600 permissions on Unix.
pub fn write_token(path: &Path, token: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(token.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }
    fn public(key: &SigningKey) -> String {
        URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes())
    }
    fn token(key: &SigningKey, payload: &str) -> String {
        let sig = key.sign(payload.as_bytes());
        format!("{}.{}", URL_SAFE_NO_PAD.encode(payload), URL_SAFE_NO_PAD.encode(sig.to_bytes()))
    }
    fn policy<'a>(keys: &'a [&'a str]) -> LicencePolicy<'a> {
        LicencePolicy {
            product: "demo",
            require_product: false,
            accepted_plans: &["pro", "team"],
            public_keys: keys,
            env_var: "DEMO_LICENCE_TOKEN_UNSET_IN_TESTS",
            file_name: "licence",
            upgrade_url: "https://example.com/pro",
            tier: "Pro",
            checkout_base: None,
        }
    }
    const PRO: &str = r#"{"plan":"pro","email":"a@example.com","issuedAt":1700000000}"#;

    #[test]
    fn valid() {
        let k = key(1);
        let pk = public(&k);
        let keys = ["bogus", pk.as_str()];
        let l = policy(&keys).verify(&token(&k, PRO)).unwrap();
        assert_eq!((l.plan.as_str(), l.issued_at), ("pro", 1_700_000_000));
        assert_eq!(l.email.as_deref(), Some("a@example.com"));
        let full = r#"{"plan":"team","issuedAt":5,"product":"demo","order":"pi_1","grant":"g/0","seats":5,"expiresAt":9999999999999}"#;
        let l = verify(&policy(&keys), &token(&k, full)).unwrap();
        assert_eq!((l.seats, l.order.as_deref(), l.grant.as_deref()), (Some(5), Some("pi_1"), Some("g/0")));
    }

    #[test]
    fn bad_signature() {
        let (k, other) = (key(1), key(2));
        let pk = public(&k);
        let keys = [pk.as_str()];
        assert_eq!(policy(&keys).verify(&token(&other, PRO)), Err(LicenceError::BadSignature));
        let sig = token(&k, PRO).split_once('.').unwrap().1.to_string();
        let forged = format!("{}.{sig}", URL_SAFE_NO_PAD.encode(r#"{"plan":"pro","issuedAt":1}"#));
        assert_eq!(policy(&keys).verify(&forged), Err(LicenceError::BadSignature));
    }

    #[test]
    fn wrong_plan_and_product() {
        let k = key(1);
        let pk = public(&k);
        let keys = [pk.as_str()];
        assert_eq!(
            policy(&keys).verify(&token(&k, r#"{"plan":"free","issuedAt":1}"#)),
            Err(LicenceError::WrongPlan)
        );
        assert_eq!(
            policy(&keys).verify(&token(&k, r#"{"plan":"pro","issuedAt":1,"product":"other"}"#)),
            Err(LicenceError::WrongProduct)
        );
    }

    #[test]
    fn require_product_refuses_productless_token() {
        let k = key(1);
        let pk = public(&k);
        let keys = [pk.as_str()];
        let strict = LicencePolicy { require_product: true, ..policy(&keys) };
        assert_eq!(strict.verify(&token(&k, PRO)), Err(LicenceError::WrongProduct));
        let named = token(&k, r#"{"plan":"pro","issuedAt":1,"product":"demo"}"#);
        assert!(strict.verify(&named).is_ok());
        assert!(policy(&keys).verify(&token(&k, PRO)).is_ok());
    }

    #[test]
    fn malformed() {
        let pk = public(&key(1));
        let keys = [pk.as_str()];
        for t in ["", "abc", "a.b", "!!.!!", "e30.AAAA"] {
            assert_eq!(policy(&keys).verify(t), Err(LicenceError::Malformed), "{t}");
        }
    }

    #[test]
    fn expired() {
        let k = key(1);
        let pk = public(&k);
        let keys = [pk.as_str()];
        let t = token(&k, r#"{"plan":"pro","issuedAt":1,"expiresAt":1000}"#);
        assert_eq!(policy(&keys).verify_at(&t, 1000), Err(LicenceError::Expired));
        assert!(policy(&keys).verify_at(&t, 999).is_ok());
        assert_eq!(policy(&keys).verify(&t), Err(LicenceError::Expired));
    }

    #[test]
    fn unknown_fields_ignored() {
        let k = key(1);
        let pk = public(&k);
        let keys = [pk.as_str()];
        let t = token(&k, r#"{"plan":"pro","issuedAt":1,"future":{"a":[1]},"x":null}"#);
        assert!(policy(&keys).verify(&t).is_ok());
    }

    #[test]
    fn env_beats_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("licence");
        let good = token(&key(1), PRO);
        write_token(&file, &good).unwrap();
        // Env wins even when invalid: no silent fallback to the file.
        assert_eq!(find_token(Some("junk".into()), Some(file.clone())).as_deref(), Some("junk"));
        assert_eq!(find_token(None, Some(file.clone())).as_deref(), Some(good.as_str()));
        assert_eq!(find_token(Some("  ".into()), Some(file.clone())).as_deref(), Some(good.as_str()));
        let other = dir.path().join("none");
        assert_eq!(find_token(Some("env".into()), Some(other.clone())).as_deref(), Some("env"));
        assert_eq!(find_token(None, Some(other)), None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn required_notice() {
        let keys: [&str; 0] = [];
        let p = policy(&keys);
        let r = required(&p, "Cite-check");
        assert_eq!(r.to_string(), "Cite-check is part of demo Pro. Learn more and get it: https://example.com/pro");
        let v = required_result_json(&r);
        assert_eq!(v["isError"], false);
        assert_eq!(v["content"][0]["text"], r.to_string());
        assert_eq!(
            v["structuredContent"]["pro_required"],
            json!({"feature": "Cite-check", "product": "demo", "tier": "Pro", "url": "https://example.com/pro"})
        );
        let team = LicencePolicy { tier: "Team", ..p };
        let r = required(&team, "Reviews");
        assert_eq!(r.to_string(), "Reviews is part of demo Team. Learn more and get it: https://example.com/pro");
        let v = required_result_json(&r);
        assert_eq!(v["content"][0]["text"], r.to_string());
        assert_eq!(v["structuredContent"]["pro_required"]["tier"], "Team");
        assert_eq!(require(&p, "Cite-check").unwrap_err(), required(&p, "Cite-check"));
    }

    fn licence_expiring(at: Option<i64>) -> Licence {
        Licence {
            plan: "pro".into(),
            email: None,
            issued_at: 1,
            product: None,
            order: None,
            grant: None,
            seats: None,
            expires_at: at,
        }
    }

    #[test]
    fn expires_soon_edges() {
        let day = Duration::from_secs(86_400);
        let now = 1_000_000_000;
        let ms = 86_400_000;
        // No expiry: never soon.
        assert!(!licence_expiring(None).expires_soon_at(day, now));
        assert!(!licence_expiring(None).expires_soon(Duration::MAX));
        // Outside, exactly on, and inside the window.
        assert!(!licence_expiring(Some(now + ms + 1)).expires_soon_at(day, now));
        assert!(licence_expiring(Some(now + ms)).expires_soon_at(day, now));
        assert!(licence_expiring(Some(now + 1)).expires_soon_at(day, now));
        // Already expired still warns; a zero window is true only at or past expiry.
        assert!(licence_expiring(Some(now - 1)).expires_soon_at(Duration::ZERO, now));
        assert!(licence_expiring(Some(now)).expires_soon_at(Duration::ZERO, now));
        assert!(!licence_expiring(Some(now + 1)).expires_soon_at(Duration::ZERO, now));
        // A huge window does not overflow.
        assert!(licence_expiring(Some(i64::MAX)).expires_soon_at(Duration::MAX, now));
        // The wall-clock entry point.
        assert!(licence_expiring(Some(now_ms() + 1000)).expires_soon(Duration::from_secs(60)));
        assert!(!licence_expiring(Some(now_ms() + 10 * 86_400_000)).expires_soon(Duration::from_secs(60)));
    }

    #[test]
    fn status_shows_tier_and_expiry_warning() {
        let k = key(1);
        let pk = public(&k);
        let keys = [pk.as_str()];
        let p = LicencePolicy { tier: "Team", ..policy(&keys) };
        let now = 1_700_000_000_000;
        let ms = 86_400_000;
        let status = |extra: &str| {
            let t = token(&k, &format!(r#"{{"plan":"team","issuedAt":1{extra}}}"#));
            status_report(&p, Some((t, "ENV".to_string())), now)
        };
        let expires = |at: i64| status(&format!(r#","expiresAt":{at}"#));

        let soon = expires(now + 12 * ms + 5);
        assert!(soon.contains("demo Team: active"), "{soon}");
        assert!(soon.contains("warning: this licence expires in 13 days. Renew it: https://example.com/pro"), "{soon}");
        assert!(expires(now + 1).contains("expires in 1 day."));
        assert!(expires(now + 30 * ms).contains("expires in 30 days"));
        let far = expires(now + 30 * ms + 1);
        assert!(far.contains("demo Team: active") && !far.contains("warning"), "{far}");
        let none = status("");
        assert!(none.contains("demo Team: active") && !none.contains("warning"), "{none}");
        let expired = expires(now - 1);
        assert!(expired.contains("demo Team: inactive") && expired.contains("expired"), "{expired}");
        let missing = status_report(&p, None, now);
        assert!(missing.contains("demo Team: inactive") && missing.contains("https://example.com/pro"), "{missing}");
    }


    /// A token in the exact shape anymd's tests sign (`SigningKey::from_bytes(&[seed; 32])`,
    /// payload `{"plan":"pro","email":..,"issuedAt":..}`) verifies the same.
    #[test]
    fn anymd_compat() {
        let k = key(1);
        let pk = public(&k);
        let keys = [pk.as_str()];
        let plans = ["pro"];
        let p = LicencePolicy { accepted_plans: &plans, ..policy(&keys) };
        let l = p
            .verify(&token(&k, r#"{"plan":"pro","email":"buyer@example.com","issuedAt":1700000000}"#))
            .unwrap();
        assert_eq!((l.plan.as_str(), l.issued_at, l.expires_at, l.seats), ("pro", 1_700_000_000, None, None));
        // The Money issuer's payload adds product/order/grant; the extra fields verify too.
        let money = r#"{"plan":"pro","email":"b@example.com","issuedAt":1,"product":"demo","order":"pi_x","grant":"cs/line_items/0"}"#;
        assert!(p.verify(&token(&k, money)).is_ok());
    }
}
