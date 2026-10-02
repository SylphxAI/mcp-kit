//! `licence buy`: start a checkout claim at the shared checkout service, show
//! the browser URL, poll until it is paid, then save the licence token through
//! the same path as `activate`. Nothing is written until a token verifies, so
//! Ctrl+C at any point leaves no partial state. Tokens are never printed.

use super::{activate_at, now_ms, status_report, LicencePolicy};
use serde_json::{json, Value};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: licence buy [--pack <id>] [--qty <n>] [--no-browser] [--json]";
/// Give up waiting for payment after this long.
const WAIT: Duration = Duration::from_secs(30 * 60);
/// Backoff between polls when the server sends no Retry-After, capped here.
const BACKOFF_CAP: Duration = Duration::from_secs(5);
/// A server Retry-After is honoured within this range.
const RETRY_AFTER_MIN: Duration = Duration::from_secs(1);
const RETRY_AFTER_MAX: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BODY: u64 = 1 << 20;
/// Consecutive failed polls tolerated before giving up.
const MAX_POLL_ERRORS: u32 = 5;

#[derive(Debug, Default, PartialEq, Eq)]
struct Opts {
    pack: Option<String>,
    qty: Option<u32>,
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
            "--pack" => {
                let v = it
                    .next()
                    .filter(|v| !v.is_empty())
                    .ok_or("--pack needs a pack id")?;
                opts.pack = Some(v.clone());
            }
            "--qty" => {
                let n: u32 = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|n| *n >= 1)
                    .ok_or("--qty needs a whole number of at least 1")?;
                opts.qty = Some(n);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(opts)
}

/// Everything the flow touches outside the policy, so tests can fake it.
struct Env<'a> {
    allow_http: bool,
    token_path: Option<PathBuf>,
    open: Option<&'a dyn Fn(&str)>,
    sleep: &'a dyn Fn(Duration),
    wait: Duration,
    out: &'a mut dyn Write,
    err: &'a mut dyn Write,
}

pub(super) fn run(policy: &LicencePolicy, args: &[String]) -> i32 {
    let opts = match parse(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{} licence buy: {e}\n{USAGE}", policy.product);
            return 2;
        }
    };
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let open: &dyn Fn(&str) = &open_browser;
    let sleep: &dyn Fn(Duration) = &std::thread::sleep;
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    let mut env = Env {
        allow_http: false,
        token_path: policy.token_path(),
        open: (!opts.no_browser && !opts.json && interactive && has_display()).then_some(open),
        sleep,
        wait: WAIT,
        out: &mut out,
        err: &mut err,
    };
    flow(policy, &opts, &mut env)
}

fn has_display() -> bool {
    if cfg!(any(target_os = "macos", windows)) {
        return true;
    }
    ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|d| !d.is_empty()))
}

/// Open a URL with the platform opener. Failure is silent: the URL is printed anyway.
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

/// Print one line to the stream; a closed pipe must not abort a paid flow.
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

fn fail(policy: &LicencePolicy, env: &mut Env, opts: &Opts, message: &str) -> i32 {
    if opts.json {
        say(
            env.out,
            &json!({"event": "result", "status": "error", "error": message}).to_string(),
        );
    } else {
        say(
            env.err,
            &format!("{} licence buy: {message}", policy.product),
        );
    }
    1
}

fn flow(policy: &LicencePolicy, opts: &Opts, env: &mut Env) -> i32 {
    let (product, tier) = (policy.product, policy.tier);
    let Some(base) = policy.checkout_base else {
        let human = format!("Get {product} {tier}: {}", policy.upgrade_url);
        event(
            env,
            opts,
            json!({"event": "result", "status": "unavailable", "url": policy.upgrade_url}),
            &human,
        );
        return 0;
    };
    let base = base.trim_end_matches('/');
    if !https_ok(base, env.allow_http) {
        return fail(
            policy,
            env,
            opts,
            "the checkout address is not an https URL",
        );
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .http_status_as_error(false)
        .user_agent(concat!("sylphx-mcp-kit/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();

    let mut body = json!({"product": product});
    if let Some(pack) = &opts.pack {
        body["pack"] = json!(pack);
    }
    if let Some(qty) = opts.qty {
        body["qty"] = json!(qty);
    }
    let claim = match http(
        &agent,
        Some(&body.to_string()),
        &format!("{base}/api/v1/claims"),
    ) {
        Ok(r) if (200..300).contains(&r.status) => r,
        Ok(r) => {
            let m = format!(
                "the checkout service answered {} to the purchase request",
                r.status
            );
            return fail(policy, env, opts, &m);
        }
        Err(e) => {
            return fail(
                policy,
                env,
                opts,
                &format!("cannot reach the checkout service: {e}"),
            )
        }
    };
    let claim: Value = serde_json::from_str(&claim.body).unwrap_or(Value::Null);
    let text = |k: &str| claim.get(k).and_then(Value::as_str).map(str::to_string);
    let (Some(claim_id), Some(browser_url), Some(poll_url)) =
        (text("claim_id"), text("browser_url"), text("poll_url"))
    else {
        return fail(
            policy,
            env,
            opts,
            "the checkout service sent an unexpected answer",
        );
    };
    // The poll URL must stay on the checkout origin; the browser URL must be https.
    if !https_ok(&browser_url, env.allow_http) || !poll_url.starts_with(&origin(base)) {
        return fail(
            policy,
            env,
            opts,
            "the checkout service sent an address that is not trusted",
        );
    }
    let expires_at = claim.get("expires_at").cloned().unwrap_or(Value::Null);

    event(
        env,
        opts,
        json!({"event": "claim", "claim_id": claim_id, "browser_url": browser_url, "expires_at": expires_at}),
        &format!("Buy {product} {tier}. Open this link to pay:\n\n  {browser_url}\n\nWaiting for payment (Ctrl+C to stop)..."),
    );
    if let Some(open) = env.open {
        open(&browser_url);
    }

    let started = Instant::now();
    let (mut attempt, mut errors) = (0u32, 0u32);
    loop {
        let retry_after = match http(&agent, None, &poll_url) {
            Ok(r) if (200..300).contains(&r.status) => {
                errors = 0;
                let v: Value = serde_json::from_str(&r.body).unwrap_or(Value::Null);
                match v.get("status").and_then(Value::as_str) {
                    Some("paid") => return paid(policy, opts, env, &claim_id, &v),
                    Some("expired") => {
                        return stopped(
                            policy,
                            opts,
                            env,
                            &claim_id,
                            "expired",
                            "this checkout expired before it was paid",
                        )
                    }
                    Some("pending") => {}
                    _ => {
                        return fail(
                            policy,
                            env,
                            opts,
                            "the checkout service sent an unexpected status",
                        )
                    }
                }
                r.retry_after
            }
            Ok(r) if (400..500).contains(&r.status) && r.status != 429 => {
                let m = format!("the checkout service answered {} while waiting", r.status);
                return fail(policy, env, opts, &m);
            }
            other => {
                errors += 1;
                if errors >= MAX_POLL_ERRORS {
                    let why = other.map_or_else(|e| e, |r| format!("status {}", r.status));
                    return fail(
                        policy,
                        env,
                        opts,
                        &format!("lost contact with the checkout service ({why})"),
                    );
                }
                None
            }
        };
        if started.elapsed() >= env.wait {
            return stopped(
                policy,
                opts,
                env,
                &claim_id,
                "timeout",
                "no payment was seen within 30 minutes",
            );
        }
        let delay = match retry_after {
            Some(d) => d.clamp(RETRY_AFTER_MIN, RETRY_AFTER_MAX),
            None => Duration::from_secs(1u64 << attempt.min(3)).min(BACKOFF_CAP),
        };
        attempt += 1;
        (env.sleep)(delay);
    }
}

fn paid(policy: &LicencePolicy, opts: &Opts, env: &mut Env, claim_id: &str, v: &Value) -> i32 {
    let tokens: Vec<&str> = v
        .get("licence_tokens")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| t.get("token").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let Some(path) = env.token_path.clone() else {
        return refused(
            policy,
            opts,
            env,
            claim_id,
            "no config directory on this machine",
        );
    };
    let mut last_error = String::from("the payment went through but no licence token came back");
    for token in tokens {
        match activate_at(policy, token, &path) {
            Ok(()) => {
                let report = status_report(
                    policy,
                    Some((token.trim().to_string(), path.display().to_string())),
                    now_ms(),
                );
                if opts.json {
                    let v = json!({"event": "result", "status": "paid", "claim_id": claim_id, "activated": true,
                                   "path": path.display().to_string(), "report": report.trim_end()});
                    say(env.out, &v.to_string());
                } else {
                    say(
                        env.out,
                        &format!(
                            "{} {} activated ({})",
                            policy.product,
                            policy.tier,
                            path.display()
                        ),
                    );
                    let _ = write!(env.out, "{report}");
                }
                return 0;
            }
            Err(e) => last_error = e,
        }
    }
    refused(policy, opts, env, claim_id, &last_error)
}

/// Paid, but nothing usable could be saved.
fn refused(
    policy: &LicencePolicy,
    opts: &Opts,
    env: &mut Env,
    claim_id: &str,
    message: &str,
) -> i32 {
    let hint = recover_hint(policy);
    if opts.json {
        let v = json!({"event": "result", "status": "invalid", "claim_id": claim_id, "activated": false,
                       "error": message, "recover": hint});
        say(env.out, &v.to_string());
    } else {
        say(
            env.err,
            &format!(
                "{} licence buy: {message}. Nothing was saved.",
                policy.product
            ),
        );
        say(env.err, &format!("Recover a paid licence: {hint}"));
    }
    1
}

/// Exit 3: the checkout expired. Exit 4: gave up waiting.
fn stopped(
    policy: &LicencePolicy,
    opts: &Opts,
    env: &mut Env,
    claim_id: &str,
    status: &str,
    message: &str,
) -> i32 {
    let hint = recover_hint(policy);
    if opts.json {
        say(
            env.out,
            &json!({"event": "result", "status": status, "claim_id": claim_id, "recover": hint})
                .to_string(),
        );
    } else {
        say(
            env.err,
            &format!("{} licence buy: {message}.", policy.product),
        );
        say(
            env.err,
            &format!("If you did pay, recover your licence: {hint}"),
        );
    }
    if status == "expired" {
        3
    } else {
        4
    }
}

fn recover_hint(policy: &LicencePolicy) -> String {
    format!(
        "{}/recover",
        policy
            .checkout_base
            .unwrap_or_default()
            .trim_end_matches('/')
    )
}

fn https_ok(url: &str, allow_http: bool) -> bool {
    url.starts_with("https://") || (allow_http && url.starts_with("http://"))
}

/// `scheme://host[:port]/` of a URL, for same-origin checks.
fn origin(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", ""));
    format!("{scheme}://{}/", rest.split('/').next().unwrap_or(""))
}

struct Reply {
    status: u16,
    retry_after: Option<Duration>,
    body: String,
}

/// POST `body` as JSON, or GET when there is none. Every request has a timeout.
fn http(agent: &ureq::Agent, body: Option<&str>, url: &str) -> Result<Reply, String> {
    let result = match body {
        Some(body) => agent
            .post(url)
            .header("content-type", "application/json")
            .send(body),
        None => agent.get(url).call(),
    };
    let mut res = result.map_err(|e| e.to_string())?;
    let retry_after = res
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let status = res.status().as_u16();
    let body = res
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .map_err(|e| e.to_string())?;
    Ok(Reply {
        status,
        retry_after,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};
    use std::cell::RefCell;
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// `(status, extra headers, body)`; `{base}` in a body becomes the server's address.
    type Scripted = (u16, Vec<(&'static str, String)>, String);

    struct Fake {
        base: String,
        seen: Arc<Mutex<Vec<String>>>,
    }

    /// Serves the scripted replies in order, repeating the last. Records `METHOD path body`.
    fn fake(replies: Vec<Scripted>) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (log, own) = (seen.clone(), base.clone());
        std::thread::spawn(move || {
            for (i, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut len = 0;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                let mut parts = line.split_whitespace();
                let (m, p) = (parts.next().unwrap(), parts.next().unwrap());
                log.lock()
                    .unwrap()
                    .push(format!("{m} {p} {}", String::from_utf8_lossy(&body)));
                let (status, headers, body) = &replies[i.min(replies.len() - 1)];
                let body = body.replace("{base}", &own);
                let mut head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                    body.len()
                );
                for (k, v) in headers {
                    head += &format!("{k}: {v}\r\n");
                }
                let _ = stream.write_all(format!("{head}\r\n{body}").as_bytes());
            }
        });
        Fake { base, seen }
    }

    fn claim() -> Scripted {
        let b = r#"{"claim_id":"c1","browser_url":"https://pay.example.com/c1","poll_url":"{base}/api/v1/claims/c1","expires_at":9}"#;
        (201, vec![], b.to_string())
    }

    fn signed(seed: u8, payload: &str) -> String {
        let k = SigningKey::from_bytes(&[seed; 32]);
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(payload),
            URL_SAFE_NO_PAD.encode(k.sign(payload.as_bytes()).to_bytes())
        )
    }

    fn paid_reply(tokens: &[&str]) -> Scripted {
        let t: Vec<Value> = tokens
            .iter()
            .map(|t| json!({"line": "pro", "token": t}))
            .collect();
        (
            200,
            vec![],
            json!({"status": "paid", "licence_tokens": t}).to_string(),
        )
    }

    fn status_reply(status: &str, retry: Option<&str>) -> Scripted {
        let h = retry
            .map(|r| vec![("retry-after", r.to_string())])
            .unwrap_or_default();
        (200, h, json!({"status": status}).to_string())
    }

    struct Run {
        code: i32,
        out: String,
        err: String,
        delays: Vec<Duration>,
        opened: Vec<String>,
        file: PathBuf,
        _dir: tempfile::TempDir,
    }

    fn go(f: &Fake, with_base: bool, args: &[&str], wait: Duration, open: bool) -> Run {
        let pk =
            URL_SAFE_NO_PAD.encode(SigningKey::from_bytes(&[1; 32]).verifying_key().to_bytes());
        let keys = [pk.as_str()];
        let base: &'static str = Box::leak(f.base.clone().into_boxed_str());
        let policy = LicencePolicy {
            product: "demo",
            require_product: true,
            accepted_plans: &["pro"],
            public_keys: &keys,
            env_var: "DEMO_LICENCE_TOKEN_UNSET_IN_TESTS",
            file_name: "licence",
            upgrade_url: "https://example.com/pro",
            tier: "Pro",
            checkout_base: with_base.then_some(base),
        };
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("demo").join("licence");
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let delays = RefCell::new(Vec::new());
        let opened = RefCell::new(Vec::new());
        let sleep = |d: Duration| {
            delays.borrow_mut().push(d);
            std::thread::sleep(Duration::from_millis(15));
        };
        let opener = |u: &str| opened.borrow_mut().push(u.to_string());
        let opts = parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
        let code = {
            let mut env = Env {
                allow_http: true,
                token_path: Some(file.clone()),
                open: open.then_some(&opener as &dyn Fn(&str)),
                sleep: &sleep,
                wait,
                out: &mut out,
                err: &mut err,
            };
            flow(&policy, &opts, &mut env)
        };
        Run {
            code,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
            delays: delays.into_inner(),
            opened: opened.into_inner(),
            file,
            _dir: dir,
        }
    }

    const GOOD: &str = r#"{"plan":"pro","issuedAt":1,"product":"demo"}"#;
    const LONG: Duration = Duration::from_secs(60);

    #[test]
    fn happy_path_saves_the_first_valid_token() {
        let wrong_key = signed(2, GOOD);
        let wrong_product = signed(1, r#"{"plan":"pro","issuedAt":1,"product":"other"}"#);
        let good = signed(1, GOOD);
        let f = fake(vec![
            claim(),
            status_reply("pending", None),
            paid_reply(&[&wrong_key, &wrong_product, &good]),
        ]);
        let r = go(&f, true, &["--pack", "big", "--qty", "3"], LONG, true);
        assert_eq!(r.code, 0, "{} {}", r.out, r.err);
        assert_eq!(std::fs::read_to_string(&r.file).unwrap(), good);
        assert!(
            r.out.contains("https://pay.example.com/c1") && r.out.contains("demo Pro activated"),
            "{}",
            r.out
        );
        assert!(r.out.contains("demo Pro: active"), "{}", r.out);
        assert!(
            !r.out.contains(&good) && !r.err.contains(&good),
            "the token must never be printed"
        );
        assert_eq!(r.opened, ["https://pay.example.com/c1"]);
        assert_eq!(r.delays, [Duration::from_secs(1)]);
        let seen = f.seen.lock().unwrap();
        assert!(seen[0].starts_with("POST /api/v1/claims "));
        assert!(
            seen[0].contains(r#""pack":"big""#)
                && seen[0].contains(r#""qty":3"#)
                && seen[0].contains(r#""product":"demo""#)
        );
        assert!(seen[1].starts_with("GET /api/v1/claims/c1"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&r.file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn invalid_tokens_are_refused_and_not_saved() {
        let wrong_key = signed(2, GOOD);
        let wrong_product = signed(1, r#"{"plan":"pro","issuedAt":1,"product":"other"}"#);
        let f = fake(vec![claim(), paid_reply(&[&wrong_key, &wrong_product])]);
        let r = go(&f, true, &[], LONG, false);
        assert_eq!(r.code, 1);
        assert!(!r.file.exists(), "nothing may be saved");
        assert!(
            r.err.contains("Nothing was saved") && r.err.contains("/recover"),
            "{}",
            r.err
        );
        assert!(!r.err.contains(&wrong_key) && !r.out.contains(&wrong_key));
    }

    #[test]
    fn expired_says_so_and_hints_recovery() {
        let f = fake(vec![claim(), status_reply("expired", None)]);
        let r = go(&f, true, &[], LONG, false);
        assert_eq!(r.code, 3);
        assert!(!r.file.exists());
        assert!(
            r.err.contains("expired") && r.err.contains(&format!("{}/recover", f.base)),
            "{}",
            r.err
        );
    }

    #[test]
    fn retry_after_is_respected_and_backoff_is_capped() {
        let f = fake(vec![
            claim(),
            status_reply("pending", Some("3")),
            status_reply("pending", None),
            status_reply("pending", None),
            status_reply("pending", None),
            status_reply("pending", None),
            status_reply("pending", None),
            status_reply("expired", None),
        ]);
        let r = go(&f, true, &[], LONG, false);
        let s = Duration::from_secs;
        // Retry-After first, then 2,4,5 (cap),5 once the backoff has grown.
        assert_eq!(r.delays, [s(3), s(2), s(4), s(5), s(5), s(5)]);
    }

    #[test]
    fn gives_up_after_the_wait() {
        let f = fake(vec![claim(), status_reply("pending", None)]);
        let r = go(&f, true, &[], Duration::from_millis(40), false);
        assert_eq!(r.code, 4);
        assert!(
            r.err.contains("30 minutes") && r.err.contains("/recover"),
            "{}",
            r.err
        );
    }

    #[test]
    fn no_checkout_base_prints_the_upgrade_url() {
        let f = fake(vec![claim()]);
        let r = go(&f, false, &[], LONG, true);
        assert_eq!(r.code, 0);
        assert!(r.out.contains("https://example.com/pro"), "{}", r.out);
        assert!(
            f.seen.lock().unwrap().is_empty(),
            "no request without a checkout base"
        );
        assert!(r.opened.is_empty());
    }

    #[test]
    fn json_events_and_no_browser() {
        let good = signed(1, GOOD);
        let f = fake(vec![claim(), paid_reply(&[&good])]);
        let r = go(&f, true, &["--json"], LONG, false);
        assert_eq!(r.code, 0);
        let lines: Vec<Value> = r
            .out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["event"], "claim");
        assert_eq!(lines[0]["claim_id"], "c1");
        assert_eq!(lines[0]["browser_url"], "https://pay.example.com/c1");
        assert_eq!(lines[1]["status"], "paid");
        assert_eq!(lines[1]["activated"], true);
        assert!(!r.out.contains(&good));
        assert!(r.opened.is_empty());
    }

    #[test]
    fn untrusted_addresses_are_refused() {
        let off_origin = (201, vec![], r#"{"claim_id":"c","browser_url":"https://p.example.com/c","poll_url":"https://evil.example.com/p"}"#.to_string());
        let f = fake(vec![off_origin]);
        assert_eq!(go(&f, true, &[], LONG, false).code, 1);
        let bad_scheme = (
            201,
            vec![],
            r#"{"claim_id":"c","browser_url":"file:///etc/passwd","poll_url":"{base}/p"}"#
                .to_string(),
        );
        let f = fake(vec![bad_scheme]);
        let r = go(&f, true, &[], LONG, true);
        assert_eq!(r.code, 1);
        assert!(r.opened.is_empty());
        assert!(
            !https_ok("http://x.example.com", false) && https_ok("https://x.example.com", false)
        );
    }

    #[test]
    fn arguments() {
        let a = |v: &[&str]| parse(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            a(&["--pack", "x", "--qty", "2", "--no-browser", "--json"]).unwrap(),
            Opts {
                pack: Some("x".into()),
                qty: Some(2),
                no_browser: true,
                json: true
            }
        );
        assert!(
            a(&["--qty", "0"]).is_err()
                && a(&["--qty"]).is_err()
                && a(&["--pack"]).is_err()
                && a(&["--nope"]).is_err()
        );
    }
}
