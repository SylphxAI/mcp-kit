//! Public Money checkout → claim-secret redeem → offline-verified activation.
//! Persist before creating anything; preserve the receipt after interruption.

use super::checkout::safe_url;
use super::pending::{Pending, Session, Store};
use super::{activate_at, now_ms, status_report, LicencePolicy, MoneyCheckout};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: licence buy [--pack <id>] [--qty <n>] [--email <address>] [--accept-terms] [--no-browser] [--json]";
const WAIT: Duration = Duration::from_secs(30 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BODY: u64 = 1 << 20;
// Money forgets idempotency keys after 24h. Never replay an unknown receipt then.
const CREATE_REPLAY_WINDOW_MS: i64 = 23 * 60 * 60 * 1000;

#[derive(Debug, Default, PartialEq, Eq)]
struct Opts {
    pack: Option<String>,
    qty: Option<u32>,
    email: Option<String>,
    accept_terms: bool,
    no_browser: bool,
    json: bool,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut opts = Opts::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--no-browser" => opts.no_browser = true,
            "--json" => opts.json = true,
            "--accept-terms" => opts.accept_terms = true,
            "--pack" => {
                opts.pack = Some(
                    it.next()
                        .filter(|v| !v.is_empty() && !v.starts_with("--"))
                        .ok_or("--pack needs a pack id")?
                        .clone(),
                )
            }
            "--email" => {
                opts.email = Some(
                    it.next()
                        .filter(|v| !v.starts_with("--"))
                        .ok_or("--email needs an address")?
                        .clone(),
                )
            }
            "--qty" => {
                opts.qty = Some(
                    it.next()
                        .and_then(|v| v.parse().ok())
                        .filter(|n| (1..=10_000).contains(n))
                        .ok_or("--qty needs a whole number between 1 and 10000")?,
                )
            }
            _ => return Err("unknown licence buy argument".into()),
        }
    }
    Ok(opts)
}

type Prompt<'a> = &'a mut dyn FnMut(&str) -> Option<String>;

struct Env<'a> {
    token_path: Option<PathBuf>,
    open: Option<&'a dyn Fn(&str)>,
    prompt: Option<Prompt<'a>>,
    sleep: &'a dyn Fn(Duration),
    wait: Duration,
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
}

pub(super) fn run(
    policy: &LicencePolicy,
    checkout: Option<&MoneyCheckout>,
    args: &[String],
) -> i32 {
    let opts = match parse(args) {
        Ok(opts) => opts,
        Err(e) => {
            eprintln!("{} licence buy: {e}\n{USAGE}", policy.product);
            return 2;
        }
    };
    let interactive =
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal() && !opts.json;
    let mut prompt = |message: &str| {
        eprint!("{message}");
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .ok()
            .filter(|n| *n > 0)
            .map(|_| answer.trim().to_string())
    };
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    if std::env::var(policy.env_var).is_ok_and(|v| !v.trim().is_empty()) {
        eprintln!(
            "warning: {} overrides the saved licence file; unset it to use the licence saved here.",
            policy.env_var
        );
    }
    flow(
        policy,
        checkout,
        &opts,
        &mut Env {
            token_path: policy.token_path(),
            open: (!opts.no_browser && interactive && has_display())
                .then_some(&open_browser as &dyn Fn(&str)),
            prompt: interactive.then_some(&mut prompt as &mut dyn FnMut(&str) -> Option<String>),
            sleep: &std::thread::sleep,
            wait: WAIT,
            out: &mut out,
            err: &mut err,
        },
    )
}

fn has_display() -> bool {
    cfg!(any(target_os = "macos", windows))
        || ["DISPLAY", "WAYLAND_DISPLAY"]
            .iter()
            .any(|v| std::env::var_os(v).is_some_and(|d| !d.is_empty()))
}

fn open_browser(url: &str) {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else if cfg!(windows) {
        let mut c = std::process::Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    let _ = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn say(w: &mut dyn Write, line: &str) {
    let _ = writeln!(w, "{line}");
    let _ = w.flush();
}

fn event(env: &mut Env, opts: &Opts, value: Value, human: &str) {
    say(
        env.out,
        &if opts.json {
            value.to_string()
        } else {
            human.to_string()
        },
    );
}

fn fail(
    policy: &LicencePolicy,
    checkout: Option<&MoneyCheckout>,
    env: &mut Env,
    opts: &Opts,
    message: &str,
) -> i32 {
    result(policy, checkout, env, opts, "error", message, None)
}

fn result(
    policy: &LicencePolicy,
    checkout: Option<&MoneyCheckout>,
    env: &mut Env,
    opts: &Opts,
    status: &str,
    message: &str,
    session: Option<&str>,
) -> i32 {
    let recovery = checkout.map_or(policy.upgrade_url, |c| c.recovery_url.as_str());
    if opts.json {
        let mut value =
            json!({"event":"result", "status":status, "error":message, "recover":recovery});
        if let Some(id) = session {
            value["checkout_session"] = json!(id);
        }
        if status == "invalid" {
            value["activated"] = json!(false);
        }
        say(env.out, &value.to_string());
    } else {
        say(
            env.err,
            &format!("{} licence buy: {message}.", policy.product),
        );
        say(
            env.err,
            &format!("If you paid, recover your licence: {recovery}"),
        );
    }
    match status {
        "expired" => 3,
        "timeout" => 4,
        _ => 1,
    }
}

fn request(checkout: &MoneyCheckout, opts: &Opts, env: &mut Env) -> Result<String, String> {
    let (price, quantity) = checkout.selection(opts.pack.as_deref(), opts.qty)?;
    let email = match &opts.email {
        Some(email) => email.clone(),
        None if !opts.json => env
            .prompt
            .as_mut()
            .and_then(|p| p("Buyer email (for your licence and receipt): "))
            .ok_or("--email is required for noninteractive purchases")?,
        None => return Err("--email is required for JSON purchases".into()),
    }
    .trim()
    .to_ascii_lowercase();
    if email.len() > 254
        || email.chars().any(|c| c.is_whitespace() || c.is_control())
        || !email.split_once('@').is_some_and(|(name, host)| {
            !name.is_empty()
                && host.contains('.')
                && !host.contains('@')
                && !host.starts_with('.')
                && !host.ends_with('.')
        })
    {
        return Err("--email needs a valid buyer email address".into());
    }
    if !opts.accept_terms {
        if opts.json {
            return Err("--accept-terms is required for JSON purchases".into());
        }
        say(env.err, &checkout.consent_text);
        let answer = env
            .prompt
            .as_mut()
            .and_then(|p| p("Accept these immediate-supply terms? Type yes to continue: "));
        if !answer.is_some_and(|v| v.eq_ignore_ascii_case("yes")) {
            return Err("explicit consent is required; use --accept-terms only after reading the storefront terms".into());
        }
    }
    Ok(json!({"product":checkout.product, "price":price, "quantity":quantity, "buyer_email":email,
        "consent":{"terms_accepted":true,"terms_version":checkout.consent_version,"terms_text":checkout.consent_text},
        "success_url":checkout.success_url,"cancel_url":checkout.cancel_url}).to_string())
}

fn valid_session(session: &Session) -> bool {
    let id = &session.processor_session_id;
    (id.starts_with("cs_test_") || id.starts_with("cs_live_"))
        && (16..=255).contains(&id.len())
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && session
            .claim_secret
            .strip_prefix("clm_")
            .is_some_and(|secret| {
                !secret.is_empty()
                    && secret.len() <= 128
                    && secret
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            })
        && safe_url(&session.url, false).is_some()
        && !session.url.contains(&session.claim_secret)
}

fn flow(
    policy: &LicencePolicy,
    checkout: Option<&MoneyCheckout>,
    opts: &Opts,
    env: &mut Env,
) -> i32 {
    let Some(checkout) = checkout else {
        event(
            env,
            opts,
            json!({"event":"result","status":"unavailable","url":policy.upgrade_url}),
            &format!(
                "Get {} {}: {}",
                policy.product, policy.tier, policy.upgrade_url
            ),
        );
        return 0;
    };
    if let Err(e) = checkout.validate() {
        return fail(policy, Some(checkout), env, opts, &e);
    }
    if !checkout.checkout_enabled {
        event(
            env,
            opts,
            json!({"event":"result","status":"unavailable","url":policy.upgrade_url,"recover":checkout.recovery_url}),
            &format!(
                "New purchases are closed. Learn more: {}\nRecover an existing licence: {}",
                policy.upgrade_url, checkout.recovery_url
            ),
        );
        return 0;
    }
    let body = match request(checkout, opts, env) {
        Ok(body) => body,
        Err(e) => return fail(policy, Some(checkout), env, opts, &e),
    };
    let Some(path) = env.token_path.clone() else {
        return fail(
            policy,
            Some(checkout),
            env,
            opts,
            "no config directory on this machine",
        );
    };
    let store = match Store::lock(&path) {
        Ok(s) => s,
        Err(e) => return fail(policy, Some(checkout), env, opts, &e),
    };
    let endpoint = format!(
        "{}/v1/money/{}/checkout_sessions:clientCreate",
        checkout.api_origin.trim_end_matches('/'),
        checkout.parent
    );
    let mut pending = match store.load() {
        Ok(Some(record)) => {
            if record.endpoint != endpoint
                || record.publishable_key != checkout.publishable_key
                || record.body != body
            {
                return fail(policy, Some(checkout), env, opts, "a conflicting pending purchase exists; resume the original request or recover it before starting another");
            }
            record
        }
        Ok(None) => {
            let mut random = [0u8; 32];
            if getrandom::getrandom(&mut random).is_err() {
                return fail(
                    policy,
                    Some(checkout),
                    env,
                    opts,
                    "cannot generate a secure idempotency key",
                );
            }
            let record = Pending {
                version: 1,
                created_at_ms: now_ms(),
                endpoint,
                publishable_key: checkout.publishable_key.clone(),
                body,
                idempotency_key: format!("mcp-checkout-{}", URL_SAFE_NO_PAD.encode(random)),
                create_attempted: false,
                session: None,
            };
            if let Err(e) = store.save(&record) {
                return fail(policy, Some(checkout), env, opts, &e);
            }
            record
        }
        Err(e) => return fail(policy, Some(checkout), env, opts, &e),
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .http_status_as_error(false)
        .https_only(!checkout.api_origin.starts_with("http://"))
        .max_redirects(0)
        .user_agent(concat!("sylphx-mcp-kit/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    if pending.session.is_none() {
        let age = now_ms().saturating_sub(pending.created_at_ms);
        if !(0..CREATE_REPLAY_WINDOW_MS).contains(&age) {
            return fail(policy, Some(checkout), env, opts, "the unresolved purchase is outside Money's safe retry window; use recovery before starting another");
        }
        if pending.create_attempted {
            return fail(
                policy,
                Some(checkout),
                env,
                opts,
                "checkout creation was already attempted; its effect is uncertain, so automatic replay is refused until Money confirms it; use recovery before another purchase",
            );
        }
        // Persist the attempt before any bytes can reach Money. A crash from
        // here on cannot make a later invocation create a duplicate session.
        pending.create_attempted = true;
        if let Err(e) = store.save(&pending) {
            return fail(policy, Some(checkout), env, opts, &e);
        }
        let reply = match post(
            &agent,
            &pending.endpoint,
            &pending.body,
            Some((&pending.publishable_key, &pending.idempotency_key)),
        ) {
            Ok(reply) => reply,
            Err(()) => return fail(
                policy,
                Some(checkout),
                env,
                opts,
                "cannot reach Money; pending purchase retained, rerun the same command to resume",
            ),
        };
        if !(200..300).contains(&reply.status) {
            return fail(
                policy,
                Some(checkout),
                env,
                opts,
                &format!(
                    "Money answered {} to the purchase request; pending purchase retained",
                    reply.status
                ),
            );
        }
        let value: Value = serde_json::from_str(&reply.body).unwrap_or(Value::Null);
        let text = |k: &str| value[k].as_str().unwrap_or_default().to_string();
        let session = Session {
            processor_session_id: text("processor_session_id"),
            url: text("url"),
            claim_secret: text("claim_secret"),
        };
        if !valid_session(&session) {
            return fail(policy, Some(checkout), env, opts, "Money sent an invalid checkout receipt or unsafe payment URL; pending purchase retained");
        }
        pending.session = Some(session);
        if let Err(e) = store.save(&pending) {
            return fail(policy, Some(checkout), env, opts, &e);
        }
    }
    let session = pending.session.as_ref().expect("session was saved above");
    if !valid_session(session) {
        return fail(
            policy,
            Some(checkout),
            env,
            opts,
            "invalid pending checkout receipt; use recovery",
        );
    }
    event(env, opts, json!({"event":"checkout","checkout_session":session.processor_session_id,"browser_url":session.url}),
        &format!("Buy {} {}. Open this link to pay:\n\n  {}\n\nWaiting for payment (Ctrl+C to stop; rerun the same command to resume)...", policy.product, policy.tier, session.url));
    if !opts.no_browser && !opts.json {
        if let Some(open) = env.open {
            open(&session.url);
        }
    }
    let redeem_url = format!(
        "{}/v1/money/checkout_sessions:redeem",
        checkout.api_origin.trim_end_matches('/')
    );
    let body = json!({"checkout_session":session.processor_session_id,"claim_secret":session.claim_secret}).to_string();
    let started = Instant::now();
    let mut attempt = 0u32;
    loop {
        let (retry_after, error) = match post(&agent, &redeem_url, &body, None) {
            Ok(r) if (200..300).contains(&r.status) => {
                let value: Value = serde_json::from_str(&r.body).unwrap_or(Value::Null);
                if value["paid"] == true {
                    return paid(policy, checkout, opts, env, session, &store, &value);
                }
                return result(
                    policy,
                    Some(checkout),
                    env,
                    opts,
                    "error",
                    "Money sent an unexpected payment answer; pending purchase retained",
                    Some(&session.processor_session_id),
                );
            }
            Ok(r) if r.status == 412 => {
                let value: Value = serde_json::from_str(&r.body).unwrap_or(Value::Null);
                match value["details"][0]["reason"].as_str() {
                    Some("purchase_pending") => (r.retry_after, None),
                    Some("checkout_expired") => {
                        if let Err(e) = store.clear() {
                            return fail(policy, Some(checkout), env, opts, &e);
                        }
                        return result(
                            policy,
                            Some(checkout),
                            env,
                            opts,
                            "expired",
                            "this checkout expired unpaid",
                            Some(&session.processor_session_id),
                        );
                    }
                    _ => {
                        return result(
                            policy,
                            Some(checkout),
                            env,
                            opts,
                            "error",
                            "Money refused this purchase; pending purchase retained",
                            Some(&session.processor_session_id),
                        )
                    }
                }
            }
            Ok(r) if r.status == 429 || r.status >= 500 => (
                r.retry_after,
                Some(format!(
                    "Money answered {} while waiting; pending purchase retained",
                    r.status
                )),
            ),
            Ok(r) => {
                return result(
                    policy,
                    Some(checkout),
                    env,
                    opts,
                    "error",
                    &format!(
                        "Money answered {} while waiting; pending purchase retained",
                        r.status
                    ),
                    Some(&session.processor_session_id),
                )
            }
            Err(()) => (
                None,
                Some("lost contact with Money; pending purchase retained".into()),
            ),
        };
        if started.elapsed() >= env.wait {
            return match error {
                Some(e) => result(
                    policy,
                    Some(checkout),
                    env,
                    opts,
                    "error",
                    &e,
                    Some(&session.processor_session_id),
                ),
                None => result(
                    policy,
                    Some(checkout),
                    env,
                    opts,
                    "timeout",
                    "no payment was seen within 30 minutes; rerun the same command to resume",
                    Some(&session.processor_session_id),
                ),
            };
        }
        let delay = retry_after
            .map(|d| d.clamp(Duration::from_secs(1), Duration::from_secs(60)))
            .unwrap_or_else(|| {
                Duration::from_secs(1u64 << attempt.min(3)).min(Duration::from_secs(5))
            })
            .min(env.wait.saturating_sub(started.elapsed()));
        attempt = attempt.saturating_add(1);
        (env.sleep)(delay);
    }
}

fn paid(
    policy: &LicencePolicy,
    checkout: &MoneyCheckout,
    opts: &Opts,
    env: &mut Env,
    session: &Session,
    store: &Store,
    value: &Value,
) -> i32 {
    let path = env.token_path.as_ref().expect("checked before checkout");
    let mut error =
        String::from("payment verified but no usable licence token came back; nothing was saved");
    for token in value["licence_tokens"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v["token"].as_str())
    {
        match activate_at(policy, token, path) {
            Ok(()) => {
                if let Err(e) = store.clear() {
                    return fail(
                        policy,
                        Some(checkout),
                        env,
                        opts,
                        &format!("licence activated, but {e}; check licence status before another purchase"),
                    );
                }
                let report = status_report(
                    policy,
                    Some((token.trim().to_string(), path.display().to_string())),
                    now_ms(),
                );
                event(
                    env,
                    opts,
                    json!({"event":"result","status":"paid","checkout_session":session.processor_session_id,"activated":true,"path":path.display().to_string(),"report":report.trim_end()}),
                    &format!(
                        "{} {} activated ({})\n{report}",
                        policy.product,
                        policy.tier,
                        path.display()
                    ),
                );
                return 0;
            }
            Err(e) => error = e,
        }
    }
    result(
        policy,
        Some(checkout),
        env,
        opts,
        "invalid",
        &format!("{error}; no returned licence was saved, pending purchase retained"),
        Some(&session.processor_session_id),
    )
}

struct Reply {
    status: u16,
    retry_after: Option<Duration>,
    body: String,
}

/// All requests are POST, bounded, with redirects disabled. Never expose server bodies/errors.
fn post(
    agent: &ureq::Agent,
    url: &str,
    body: &str,
    auth: Option<(&str, &str)>,
) -> Result<Reply, ()> {
    let mut request = agent
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json");
    if let Some((key, idempotency)) = auth {
        request = request
            .header("authorization", &format!("Bearer {key}"))
            .header("idempotency-key", idempotency);
    }
    let mut response = request.send(body).map_err(|_| ())?;
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .map_err(|_| ())?;
    Ok(Reply {
        status,
        retry_after,
        body,
    })
}

#[cfg(test)]
mod tests;
