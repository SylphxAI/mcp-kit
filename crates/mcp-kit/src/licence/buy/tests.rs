use super::*;
use ed25519_dalek::{Signer, SigningKey};
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const GOOD: &str = r#"{"plan":"pro","issuedAt":1,"product":"demo","email":"private@example.com"}"#;
const SECRET: &str = "clm_private_checkout_secret";
const SID: &str = "cs_test_1234567890abcdef";
const LONG: Duration = Duration::from_secs(60);
type Scripted = (u16, Vec<(&'static str, String)>, String);

#[derive(Clone)]
struct Seen {
    route: String,
    headers: String,
    body: Value,
}

struct Fake {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn fake(replies: Vec<Scripted>) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (log, done) = (seen.clone(), stop.clone());
    std::thread::spawn(move || {
        let mut index = 0;
        while !done.load(Ordering::Relaxed) {
            let (mut stream, _) = match listener.accept() {
                Ok(stream) => stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(_) => return,
            };
            // macOS accepts sockets with the listener's nonblocking flag;
            // restore blocking mode before the scripted request reads.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut route = String::new();
            reader.read_line(&mut route).unwrap();
            let mut len = 0;
            let mut headers = String::new();
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim().is_empty() {
                    break;
                }
                if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                headers += &header;
            }
            let mut bytes = vec![0; len];
            reader.read_exact(&mut bytes).unwrap();
            log.lock().unwrap().push(Seen {
                route: route.trim().to_string(),
                headers: headers.to_ascii_lowercase(),
                body: serde_json::from_slice(&bytes).unwrap(),
            });
            let (status, headers, body) = &replies[index.min(replies.len() - 1)];
            index += 1;
            if *status == 0 {
                continue;
            } // Server received the request but the reply was lost.
            let mut head = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n", body.len());
            for (k, v) in headers {
                head += &format!("{k}: {v}\r\n");
            }
            let _ = stream.write_all(format!("{head}\r\n{body}").as_bytes());
        }
    });
    Fake { base, seen, stop }
}

fn create() -> Scripted {
    (200, vec![], json!({"processor_session_id":SID,"url":"https://pay.example.com/session","claim_secret":SECRET}).to_string())
}
fn reason(reason: &str) -> Scripted {
    (
        412,
        vec![],
        json!({"details":[{"reason":reason}]}).to_string(),
    )
}
fn paid_reply(tokens: &[&str]) -> Scripted {
    (200,vec![],json!({"paid":true,"licence_tokens":tokens.iter().map(|t| json!({"token":t})).collect::<Vec<_>>(),"grant_token":"never-print-this-grant"}).to_string())
}
fn signed(seed: u8, payload: &str) -> String {
    let key = SigningKey::from_bytes(&[seed; 32]);
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(payload),
        URL_SAFE_NO_PAD.encode(key.sign(payload.as_bytes()).to_bytes())
    )
}

fn config(base: &str) -> MoneyCheckout {
    MoneyCheckout {
        api_origin: base.into(),
        parent: "orgs/org_demo/projects/prj_demo/envs/test".into(),
        publishable_key: "sylphx_pk_test_public".into(),
        product: "demo_seat_year".into(),
        checkout_enabled: true,
        prices: vec![
            super::super::MoneyPriceOption {
                id: "one".into(),
                key: "seat".into(),
                label: "1 seat".into(),
                quantity: 1,
            },
            super::super::MoneyPriceOption {
                id: "five".into(),
                key: "seat".into(),
                label: "5 seats".into(),
                quantity: 5,
            },
        ],
        consent_version: "v1".into(),
        consent_text:
            "I ask for immediate supply. I understand I lose my right to cancel once supplied."
                .into(),
        success_url: "https://example.com/paid?session_id={CHECKOUT_SESSION_ID}".into(),
        cancel_url: "https://example.com/cancel".into(),
        recovery_url: "https://example.com/recover".into(),
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    file: PathBuf,
}
struct Run {
    code: i32,
    out: String,
    err: String,
    delays: Vec<Duration>,
    opened: Vec<String>,
    prompts: Vec<String>,
}
impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("demo/licence");
        Self { _dir: dir, file }
    }
    fn pending(&self) -> PathBuf {
        let mut p = self.file.as_os_str().to_os_string();
        p.push(".pending-checkout.json");
        PathBuf::from(p)
    }
    fn go(
        &self,
        checkout: Option<&MoneyCheckout>,
        args: &[&str],
        wait: Duration,
        answers: Option<Vec<&str>>,
    ) -> Run {
        let public =
            URL_SAFE_NO_PAD.encode(SigningKey::from_bytes(&[1; 32]).verifying_key().to_bytes());
        let keys = [public.as_str()];
        let policy = LicencePolicy {
            product: "demo",
            require_product: true,
            accepted_plans: &["pro"],
            public_keys: &keys,
            env_var: "DEMO_LICENCE_TOKEN_UNSET_IN_TESTS",
            file_name: "licence",
            upgrade_url: "https://example.com/pro",
            tier: "Pro",
            checkout_base: Some("https://legacy.invalid"),
        };
        let opts = parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let delays = RefCell::new(Vec::new());
        let opened = RefCell::new(Vec::new());
        let prompts = RefCell::new(Vec::new());
        let sleep = |d| {
            delays.borrow_mut().push(d);
            std::thread::sleep(Duration::from_millis(1));
        };
        let open = |s: &str| opened.borrow_mut().push(s.to_string());
        let interactive = answers.is_some();
        let mut answers = answers.unwrap_or_default().into_iter();
        let mut prompt = |s: &str| {
            prompts.borrow_mut().push(s.to_string());
            answers.next().map(str::to_string)
        };
        let code = flow(
            &policy,
            checkout,
            &opts,
            &mut Env {
                token_path: Some(self.file.clone()),
                open: Some(&open),
                prompt: interactive.then_some(&mut prompt),
                sleep: &sleep,
                wait,
                out: &mut out,
                err: &mut err,
            },
        );
        Run {
            code,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
            delays: delays.into_inner(),
            opened: opened.into_inner(),
            prompts: prompts.into_inner(),
        }
    }
    fn buy(&self, checkout: &MoneyCheckout, extra: &[&str], wait: Duration) -> Run {
        let mut args = vec!["--email", "private@example.com", "--accept-terms"];
        args.extend_from_slice(extra);
        self.go(Some(checkout), &args, wait, None)
    }
}

fn private_mode(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[test]
fn public_create_redeem_first_valid_activation_and_headers() {
    let wrong_key = signed(2, GOOD);
    let wrong_product = signed(1, r#"{"plan":"pro","issuedAt":1,"product":"other"}"#);
    let good = signed(1, GOOD);
    let f = fake(vec![
        create(),
        reason("purchase_pending"),
        paid_reply(&[&wrong_key, &wrong_product, &good]),
    ]);
    let h = Harness::new();
    let c = config(&f.base);
    let r = h.buy(&c, &["--pack", "five", "--qty", "3"], LONG);
    assert_eq!(r.code, 0, "{} {}", r.out, r.err);
    assert_eq!(std::fs::read_to_string(&h.file).unwrap(), good);
    private_mode(&h.file);
    assert!(!h.pending().exists());
    assert_eq!(r.opened, ["https://pay.example.com/session"]);
    assert_eq!(r.delays, [Duration::from_secs(1)]);
    assert!(r.out.contains("demo Pro: active"));
    for secret in [
        &good,
        &wrong_key,
        &wrong_product,
        SECRET,
        "never-print-this-grant",
        "private@example.com",
    ] {
        assert!(!r.out.contains(secret) && !r.err.contains(secret));
    }
    let seen = f.seen.lock().unwrap();
    assert!(seen[0].route.starts_with(
        "POST /v1/money/orgs/org_demo/projects/prj_demo/envs/test/checkout_sessions:clientCreate "
    ));
    assert!(seen[0]
        .headers
        .contains("authorization: bearer sylphx_pk_test_public"));
    assert!(seen[0].headers.contains("idempotency-key: mcp-checkout-"));
    assert_eq!(seen[0].body["product"], "demo_seat_year");
    assert_eq!(seen[0].body["price"], "seat");
    assert_eq!(seen[0].body["quantity"], 3);
    assert_eq!(seen[0].body["consent"]["terms_text"], c.consent_text);
    assert_eq!(seen[0].body["consent"]["terms_accepted"], true);
    assert!(seen[1]
        .route
        .starts_with("POST /v1/money/checkout_sessions:redeem "));
    assert_eq!(
        seen[1].body,
        json!({"checkout_session":SID,"claim_secret":SECRET})
    );
    assert!(
        !seen[1].headers.contains("authorization:")
            && !seen[1].headers.contains("idempotency-key:")
    );
}

#[test]
fn invalid_signature_and_product_preserve_recoverable_receipt() {
    for token in [
        signed(2, GOOD),
        signed(1, r#"{"plan":"pro","issuedAt":1,"product":"other"}"#),
    ] {
        let f = fake(vec![create(), paid_reply(&[&token])]);
        let h = Harness::new();
        let r = h.buy(&config(&f.base), &["--json"], LONG);
        assert_eq!(r.code, 1);
        assert!(!h.file.exists());
        assert!(h.pending().exists());
        assert!(!r.out.contains(&token) && !r.out.contains(SECRET));
        let result: Value = serde_json::from_str(r.out.lines().last().unwrap()).unwrap();
        assert_eq!(result["status"], "invalid");
        assert_eq!(result["recover"], "https://example.com/recover");
        assert!(!std::fs::read_to_string(h.pending())
            .unwrap()
            .contains(&token));
    }
}

#[test]
fn json_no_browser_and_explicit_automation_consent() {
    let good = signed(1, GOOD);
    let f = fake(vec![create(), paid_reply(&[&good])]);
    let c = config(&f.base);
    for args in [
        vec!["--json"],
        vec!["--email", "private@example.com", "--json"],
        vec!["--accept-terms", "--json"],
        vec!["--email", "private@example.com"],
        vec![],
    ] {
        let h = Harness::new();
        let r = h.go(Some(&c), &args, LONG, None);
        assert_eq!(r.code, 1);
        assert!(!h.pending().exists());
        assert!(f.seen.lock().unwrap().is_empty());
    }
    let h = Harness::new();
    let r = h.buy(&c, &["--json"], LONG);
    assert_eq!(r.code, 0, "{} {}", r.out, r.err);
    let lines: Vec<Value> = r
        .out
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(lines[0]["event"], "checkout");
    assert_eq!(lines[0]["checkout_session"], SID);
    assert_eq!(lines[1]["activated"], true);
    assert!(r.opened.is_empty() && r.prompts.is_empty());
    assert!(!r.out.contains("private@example.com") && !r.out.contains(SECRET));
    let h = Harness::new();
    let r = h.buy(&c, &["--no-browser"], LONG);
    assert!(r.opened.is_empty());
}

#[test]
fn interactive_prompt_shows_exact_terms_and_requires_yes() {
    let good = signed(1, GOOD);
    let f = fake(vec![create(), paid_reply(&[&good])]);
    let c = config(&f.base);
    let h = Harness::new();
    let r = h.go(Some(&c), &[], LONG, Some(vec!["private@example.com", "no"]));
    assert_eq!(r.code, 1);
    assert!(r.err.contains(&c.consent_text));
    assert!(f.seen.lock().unwrap().is_empty());
    let r = h.go(
        Some(&c),
        &[],
        LONG,
        Some(vec!["private@example.com", "yes"]),
    );
    assert_eq!(r.code, 0);
    assert!(r.err.contains(&c.consent_text));
    assert_eq!(r.prompts.len(), 2);
}

#[test]
fn expired_clears_receipt_but_pending_timeout_preserves_it_and_resumes() {
    let f = fake(vec![
        create(),
        reason("purchase_pending"),
        reason("checkout_expired"),
    ]);
    let c = config(&f.base);
    let h = Harness::new();
    let r = h.buy(&c, &[], Duration::ZERO);
    assert_eq!(r.code, 4);
    assert!(h.pending().exists());
    private_mode(&h.pending());
    let record = std::fs::read_to_string(h.pending()).unwrap();
    assert!(record.contains(SECRET));
    assert!(!record.contains("licence_tokens"));
    let r = h.buy(&c, &[], LONG);
    assert_eq!(r.code, 3);
    assert!(!h.pending().exists());
    let seen = f.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert!(seen[2].route.contains(":redeem"));
    assert!(r.err.contains("https://example.com/recover"));
}

#[test]
fn restart_after_create_failure_does_not_replay_or_create_duplicate() {
    let f = fake(vec![(503, vec![], "internal secret must not print".into())]);
    let c = config(&f.base);
    let h = Harness::new();
    let r = h.buy(&c, &[], LONG);
    assert_eq!(r.code, 1);
    assert!(h.pending().exists());
    let first = std::fs::read_to_string(h.pending()).unwrap();
    let parsed: Value = serde_json::from_str(&first).unwrap();
    assert!(parsed["session"].is_null());
    assert_eq!(parsed["create_attempted"], true);
    assert!(!r.err.contains("internal secret"));
    let r = h.buy(&c, &[], LONG);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("automatic replay is refused"), "{}", r.err);
    assert_eq!(std::fs::read_to_string(h.pending()).unwrap(), first);
    assert_eq!(f.seen.lock().unwrap().len(), 1, "no second create request");
}

#[test]
fn lost_create_response_is_not_replayed_without_an_upstream_receipt() {
    let f = fake(vec![(0, vec![], String::new())]);
    let c = config(&f.base);
    let h = Harness::new();
    assert_eq!(h.buy(&c, &[], LONG).code, 1);
    assert_eq!(f.seen.lock().unwrap().len(), 1);
    let r = h.buy(&c, &[], LONG);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("automatic replay is refused"), "{}", r.err);
    assert_eq!(
        f.seen.lock().unwrap().len(),
        1,
        "the lost create is not resent"
    );
    assert!(h.pending().exists());
}

#[test]
fn create_refusals_keep_the_original_key_and_never_follow_redirects() {
    for status in [302, 404, 429, 503, 409] {
        let f = fake(vec![(
            status,
            vec![("location", "https://evil.example.com/".into())],
            "sensitive server body".into(),
        )]);
        let c = config(&f.base);
        let h = Harness::new();
        let r = h.buy(&c, &["--json"], LONG);
        assert_eq!(r.code, 1);
        assert!(h.pending().exists());
        assert!(r.opened.is_empty());
        assert!(!r.out.contains("sensitive server body"));
        assert_eq!(f.seen.lock().unwrap().len(), 1);
    }
}

#[test]
fn unverified_paid_false_missing_or_oversized_tokens_never_activate() {
    let good = signed(1, GOOD);
    for body in [
        json!({"paid":false,"licence_tokens":[{"token":good}]}).to_string(),
        json!({"paid":true,"licence_tokens":[]}).to_string(),
        json!({"paid":true,"licence_tokens":[{"token":"malformed"}]}).to_string(),
        format!(
            "{}{}",
            " ".repeat((MAX_BODY + 1) as usize),
            json!({"paid":true,"licence_tokens":[{"token":good}]})
        ),
    ] {
        let f = fake(vec![create(), (200, vec![], body)]);
        let h = Harness::new();
        let r = h.buy(&config(&f.base), &["--json"], Duration::ZERO);
        assert_eq!(r.code, 1);
        assert!(!h.file.exists());
        assert!(h.pending().exists());
        assert!(!r.out.contains(&good) && !r.out.contains(SECRET));
    }
}

#[test]
fn redemption_transport_error_retries_without_exposing_secrets() {
    let good = signed(1, GOOD);
    let f = fake(vec![
        create(),
        (0, vec![], String::new()),
        paid_reply(&[&good]),
    ]);
    let h = Harness::new();
    let r = h.buy(&config(&f.base), &["--json"], LONG);
    assert_eq!(r.code, 0);
    assert_eq!(r.delays, [Duration::from_secs(1)]);
    assert!(!r.out.contains(SECRET));
}

#[test]
fn conflicting_pending_request_or_rotated_key_never_creates_another_order() {
    let f = fake(vec![create(), reason("purchase_pending")]);
    let c = config(&f.base);
    let h = Harness::new();
    assert_eq!(h.buy(&c, &[], Duration::ZERO).code, 4);
    for extra in [
        vec!["--qty", "2"],
        vec!["--email", "other@example.com"],
        vec!["--pack", "five"],
    ] {
        let r = h.buy(&c, &extra, LONG);
        assert_eq!(r.code, 1);
        assert!(r.err.contains("conflicting pending"));
    }
    let mut rotated = c.clone();
    rotated.publishable_key = "pk_rotated".into();
    assert_eq!(h.buy(&rotated, &[], LONG).code, 1);
    assert_eq!(f.seen.lock().unwrap().len(), 2);
}

#[test]
fn unresolved_create_older_than_money_replay_window_is_not_retried() {
    let f = fake(vec![(503, vec![], String::new())]);
    let c = config(&f.base);
    let h = Harness::new();
    assert_eq!(h.buy(&c, &[], LONG).code, 1);
    {
        let store = Store::lock(&h.file).unwrap();
        let mut p = store.load().unwrap().unwrap();
        p.created_at_ms = now_ms() - CREATE_REPLAY_WINDOW_MS;
        store.save(&p).unwrap();
    }
    let r = h.buy(&c, &[], LONG);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("safe retry window"));
    assert_eq!(f.seen.lock().unwrap().len(), 1);
}

#[test]
fn concurrent_run_and_corrupt_state_fail_before_network() {
    let f = fake(vec![create()]);
    let c = config(&f.base);
    let h = Harness::new();
    let store = Store::lock(&h.file).unwrap();
    let r = h.buy(&c, &[], LONG);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("already running"));
    drop(store);
    super::super::write_private(&h.pending(), b"corrupt").unwrap();
    assert_eq!(h.buy(&c, &[], LONG).code, 1);
    assert!(f.seen.lock().unwrap().is_empty());
}

#[test]
fn errors_redirects_404_and_unknown_412_keep_pending_and_do_not_follow() {
    for status in [302, 404, 403, 412] {
        let f = fake(vec![
            create(),
            (
                status,
                vec![("location", "https://evil.example.com/".into())],
                json!({"details":[{"reason":"unknown"}]}).to_string(),
            ),
        ]);
        let c = config(&f.base);
        let h = Harness::new();
        let r = h.buy(&c, &["--json"], LONG);
        assert_eq!(r.code, 1);
        assert!(h.pending().exists());
        assert!(!h.file.exists());
        assert!(r.out.contains("https://example.com/recover"));
        assert!(!r.out.contains(SECRET));
        assert_eq!(f.seen.lock().unwrap().len(), 2);
    }
}

#[test]
fn retry_429_503_backoff_retry_after_then_paid() {
    let good = signed(1, GOOD);
    let mut pending = reason("purchase_pending");
    pending.1.push(("retry-after", "3".into()));
    let f = fake(vec![
        create(),
        pending,
        (429, vec![("retry-after", "0".into())], String::new()),
        (503, vec![], String::new()),
        reason("purchase_pending"),
        reason("purchase_pending"),
        paid_reply(&[&good]),
    ]);
    let h = Harness::new();
    let r = h.buy(&config(&f.base), &[], Duration::from_secs(100));
    assert_eq!(r.code, 0);
    assert_eq!(r.delays, [3, 1, 4, 5, 5].map(Duration::from_secs));
}

#[test]
fn exhausted_budget_on_service_errors_returns_error_not_timeout() {
    let f = fake(vec![create(), (503, vec![], String::new())]);
    let h = Harness::new();
    let r = h.buy(&config(&f.base), &[], Duration::ZERO);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("503"));
    assert!(h.pending().exists());
}

#[test]
fn unsafe_urls_or_incomplete_receipts_are_never_opened_or_redeemed() {
    for url in [
        "file:///etc/passwd",
        "http://evil.example.com/",
        "https://user:pass@pay.example.com/",
        "https://pay.example.com/a b",
        "https://pay.example.com/a\nb",
        "https://pay.example.com/a\"b",
        "https:\\evil.example.com",
        "https://pay.example.com/clm_private_checkout_secret",
    ] {
        let reply = (
            200,
            vec![],
            json!({"processor_session_id":SID,"url":url,"claim_secret":SECRET}).to_string(),
        );
        let f = fake(vec![reply]);
        let h = Harness::new();
        let r = h.buy(&config(&f.base), &[], LONG);
        assert_eq!(r.code, 1, "{url}");
        assert!(r.opened.is_empty());
        assert_eq!(f.seen.lock().unwrap().len(), 1);
        assert!(!r.err.contains(SECRET));
    }
    let f = fake(vec![(
        200,
        vec![],
        json!({"processor_session_id":SID,"url":"https://pay.example.com/"}).to_string(),
    )]);
    assert_eq!(Harness::new().buy(&config(&f.base), &[], LONG).code, 1);
    let token = signed(1, GOOD);
    let f = fake(vec![(
        200,
        vec![],
        json!({"processor_session_id":SID,"url":"https://pay.example.com/","claim_secret":token})
            .to_string(),
    )]);
    let h = Harness::new();
    assert_eq!(h.buy(&config(&f.base), &[], LONG).code, 1);
    assert!(!std::fs::read_to_string(h.pending())
        .unwrap()
        .contains(&token));
}

#[test]
fn legacy_run_and_closed_offer_do_not_contact_checkout() {
    let f = fake(vec![create()]);
    let h = Harness::new();
    let r = h.go(None, &["--json"], LONG, None);
    assert_eq!(r.code, 0);
    assert!(r.out.contains("https://example.com/pro"));
    let mut c = config(&f.base);
    c.checkout_enabled = false;
    c.prices.clear();
    c.consent_text.clear();
    c.success_url.clear();
    let r = h.go(Some(&c), &[], LONG, None);
    assert_eq!(r.code, 0);
    assert!(r.out.contains(&c.recovery_url));
    assert!(f.seen.lock().unwrap().is_empty());
}

#[test]
fn parses_arguments_and_pack_quantity_semantics() {
    let args = |a: &[&str]| parse(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    let o = args(&[
        "--pack",
        "five",
        "--qty",
        "2",
        "--email",
        "a@b.com",
        "--accept-terms",
        "--json",
        "--no-browser",
    ])
    .unwrap();
    assert!(o.json && o.no_browser && o.accept_terms);
    assert_eq!(o.qty, Some(2));
    for bad in [
        vec!["--qty", "0"],
        vec!["--qty", "10001"],
        vec!["--qty"],
        vec!["--pack"],
        vec!["--email"],
        vec!["--nope"],
    ] {
        assert!(args(&bad).is_err());
    }
    let c = config("https://api.example.com");
    assert_eq!(c.selection(Some("five"), None).unwrap(), ("seat", 5));
    assert_eq!(c.selection(Some("five"), Some(3)).unwrap(), ("seat", 3));
    assert!(c.selection(Some("seat"), None).is_err());
    assert!(c.selection(Some("missing"), None).is_err());
}

#[test]
fn safe_configuration_rejects_nonpublic_keys_and_unsafe_origins() {
    for base in [
        "https://api.example.com/path",
        "https://api.example.com?x=1",
        "http://api.example.com",
        "http://localhost@evil.com",
        "http://localhost.evil.com",
        "http://127.1",
        "http://0x7f000001",
        "http://localhost:",
        "https://@api.example.com",
        "https://user@api.example.com",
    ] {
        assert!(config(base).validate().is_err(), "{base}");
    }
    for base in [
        "https://api.example.com",
        "http://localhost:3000",
        "http://127.0.0.1:99",
        "http://[::1]:1",
    ] {
        assert!(config(base).validate().is_ok(), "{base}");
    }
    for key in [
        "sylphx_sk_private",
        "sk_private",
        "rk_private",
        "pk_",
        "pk_bad\nkey",
    ] {
        let mut c = config("https://api.example.com");
        c.publishable_key = key.into();
        assert!(c.validate().is_err());
    }
}

#[test]
fn docs_toml_is_the_single_source_and_ignores_presentation_fields() {
    let docs = r#"
title = "Storefront"
[money]
parent = "orgs/org_demo/projects/prj_demo/envs/live"
publishable_key = "sylphx_pk_public"
[money.trader]
name = "Example"
[money.offers.pro]
product = "demo_seat_year"
token_product = "demo"
title = "Buy Pro"
consent_version = "v1"
consent_text = "Exact immediate-supply terms."
success_url = "https://example.com/paid?session_id={CHECKOUT_SESSION_ID}"
cancel_url = "https://example.com/cancel"
recovery_url = "https://example.com/recover"
[[money.offers.pro.prices]]
id = "one"
key = "seat"
label = "One seat"
quantity = 1
[[money.offers.pro.prices]]
id = "five"
key = "seat"
label = "Five seats"
quantity = 5
[money.offers.closed]
product = "legacy"
checkout_enabled = false
recovery_url = "https://example.com/recover"
"#;
    let c = MoneyCheckout::from_docs_toml(docs, "pro").unwrap();
    assert_eq!(c.api_origin, "https://api.sylphx.com");
    assert_eq!(c.consent_text, "Exact immediate-supply terms.");
    assert_eq!(c.selection(Some("five"), None).unwrap(), ("seat", 5));
    assert!(
        !MoneyCheckout::from_docs_toml(docs, "closed")
            .unwrap()
            .checkout_enabled
    );
    assert!(MoneyCheckout::from_docs_toml(docs, "missing").is_err());
    assert!(MoneyCheckout::from_docs_toml("secret invalid toml", "pro").is_err());
    let no_ids = docs
        .replace("id = \"one\"", "")
        .replace("id = \"five\"", "");
    let c = MoneyCheckout::from_docs_toml(&no_ids, "pro").unwrap();
    assert_eq!(c.selection(Some("seat:5"), None).unwrap(), ("seat", 5));
    assert!(MoneyCheckout::from_docs_toml(
        &docs.replace("sylphx_pk_public", "sylphx_sk_private"),
        "pro"
    )
    .is_err());
    assert!(
        MoneyCheckout::from_docs_toml(&docs.replace("quantity = 5", "quantity = 0"), "pro")
            .is_err()
    );
    let defaults = docs
        .replace("quantity = 1", "")
        .replace("id = \"one\"", "id = \" one \"");
    let c = MoneyCheckout::from_docs_toml(&defaults, "pro").unwrap();
    assert_eq!(c.selection(Some("one"), None).unwrap(), ("seat", 1));
    assert!(
        MoneyCheckout::from_docs_toml(&docs.replace("id = \"five\"", "id = \"one\""), "pro")
            .is_err()
    );
}
