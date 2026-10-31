//! The web adapter driven over real loopback sockets with a scripted backend: authentication,
//! Host/Origin/CSRF checks, malformed and oversized requests, slow clients, connection and stream
//! caps, server-sent events and shutdown. The daemon is not involved: the backend is a fake, so
//! every request the adapter lets through is visible in `Fake::calls`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_domain::error::{FleetError, Result};
use fleet_domain::value::Value;
use fleet_infra::eventlog::VerifyReport;
use fleet_infra::json;
use fleet_infra::server::PeerInfo;
use fleet_infra::web::auth::AuthLimits;
use fleet_infra::web::http::Limits;
use fleet_infra::web::{WebBackend, WebLimits, WebOptions, WebServer};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// ---------- the fake backend ----------

type Handler = Box<dyn FnMut(&str, &Value) -> Result<Value> + Send>;

struct Fake {
    calls: Mutex<Vec<(String, Value)>>,
    handler: Mutex<Handler>,
    deny: AtomicBool,
    admitted_pids: Mutex<Vec<u32>>,
    health_computed: AtomicUsize,
    log: Mutex<Result<VerifyReport>>,
}

impl Fake {
    fn new() -> Arc<Fake> {
        Arc::new(Fake {
            calls: Mutex::new(Vec::new()),
            handler: Mutex::new(Box::new(|cmd, _| {
                Ok(Value::object([(
                    "schema",
                    Value::str(format!("fake.{cmd}/1")),
                )]))
            })),
            deny: AtomicBool::new(false),
            admitted_pids: Mutex::new(Vec::new()),
            health_computed: AtomicUsize::new(0),
            log: Mutex::new(Ok(VerifyReport {
                records: 7,
                head_hash: "ab".repeat(32),
            })),
        })
    }

    fn on(&self, f: impl FnMut(&str, &Value) -> Result<Value> + Send + 'static) {
        *self.handler.lock().unwrap() = Box::new(f);
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }

    fn call_names(&self) -> Vec<String> {
        self.calls().into_iter().map(|(n, _)| n).collect()
    }
}

impl WebBackend for Fake {
    fn admit(&self, peer: &PeerInfo) -> Result<()> {
        self.admitted_pids
            .lock()
            .unwrap()
            .push(peer.pid.unwrap_or(0));
        if self.deny.load(Ordering::SeqCst) {
            return Err(FleetError::forbidden(
                "requests from worker processes are refused; approvals and decisions are made by a person",
            ));
        }
        Ok(())
    }

    fn call(&self, cmd: &str, args: Value) -> Result<Value> {
        self.calls
            .lock()
            .unwrap()
            .push((cmd.to_string(), args.clone()));
        (self.handler.lock().unwrap())(cmd, &args)
    }

    fn process_alive(&self, _pid: u32) -> bool {
        true
    }

    fn log_status(&self) -> Result<VerifyReport> {
        self.log.lock().unwrap().clone()
    }

    fn health(&self) -> Value {
        self.health_computed.fetch_add(1, Ordering::SeqCst);
        Value::object([
            ("schema", Value::str("fleet.doctor/1")),
            ("ok", Value::Bool(true)),
            ("checks", Value::array(Vec::new())),
        ])
    }
}

// ---------- a running server ----------

struct Running {
    fake: Arc<Fake>,
    server: Option<Arc<WebServer>>,
    addr: SocketAddr,
    token: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

fn quiet_auth() -> AuthLimits {
    AuthLimits {
        max_failures: 10_000,
        ..AuthLimits::default()
    }
}

fn fast_limits() -> WebLimits {
    WebLimits {
        http: Limits {
            request_deadline: Duration::from_secs(5),
            ..Limits::default()
        },
        auth: quiet_auth(),
        stream_poll: Duration::from_millis(20),
        ..WebLimits::default()
    }
}

impl Running {
    fn start() -> Running {
        Running::with(fast_limits())
    }

    fn with(limits: WebLimits) -> Running {
        let fake = Fake::new();
        let mut opts = WebOptions::new("127.0.0.1:0".parse().unwrap(), false);
        opts.limits = limits;
        let server = Arc::new(WebServer::bind(&opts, fake.clone()).unwrap());
        let addr = server.local_addr();
        let token = server.token().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let (s, st) = (Arc::clone(&server), Arc::clone(&stop));
        let thread = std::thread::spawn(move || s.serve(&st));
        Running {
            fake,
            server: Some(server),
            addr,
            token,
            stop,
            thread: Some(thread),
        }
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.addr.port())
    }

    fn origin(&self) -> String {
        format!("http://{}", self.host())
    }

    fn bearer(&self) -> (String, String) {
        (
            "Authorization".to_string(),
            format!("Bearer {}", self.token),
        )
    }

    fn get(&self, path: &str, headers: &[(&str, &str)]) -> Resp {
        self.request("GET", path, headers, None)
    }

    fn get_bearer(&self, path: &str) -> Resp {
        let (k, v) = self.bearer();
        self.get(path, &[(&k, &v)])
    }

    fn post_bearer(&self, path: &str, body: &str) -> Resp {
        let (k, v) = self.bearer();
        self.request(
            "POST",
            path,
            &[(&k, &v), ("Content-Type", "application/json")],
            Some(body),
        )
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Resp {
        let mut text = format!("{method} {path} HTTP/1.1\r\n");
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            text.push_str(&format!("Host: {}\r\n", self.host()));
        }
        for (k, v) in headers {
            text.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some(b) = body {
            text.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
        text.push_str("Connection: close\r\n\r\n");
        if let Some(b) = body {
            text.push_str(b);
        }
        self.raw(text.as_bytes())
    }

    fn raw(&self, bytes: &[u8]) -> Resp {
        let mut s = TcpStream::connect(self.addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let _ = s.write_all(bytes);
        read_response(&mut s)
    }

    /// Logs in with the token the way the page does; returns (session id, csrf).
    fn login(&self) -> (String, String) {
        let resp = self.request(
            "POST",
            "/api/login",
            &[
                ("Origin", &self.origin()),
                ("Content-Type", "application/json"),
            ],
            Some(&format!("{{\"token\":\"{}\"}}", self.token)),
        );
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert!(resp.header("set-cookie").is_none(), "no cookie is ever set");
        let doc = parse(&resp.body);
        let session = doc
            .get("session")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        let csrf = doc.get("csrf").and_then(Value::as_str).unwrap().to_string();
        (session, csrf)
    }

    /// A POST the way the page sends it: session header, Origin, CSRF header, JSON.
    fn post_session(&self, path: &str, session: &str, csrf: &str, body: &str) -> Resp {
        self.request(
            "POST",
            path,
            &[
                ("X-Fleet-Session", session),
                ("Origin", &self.origin()),
                ("X-CSRF-Token", csrf),
                ("Content-Type", "application/json"),
            ],
            Some(body),
        )
    }

    fn active(&self) -> usize {
        self.server.as_ref().map_or(0, |s| s.active_connections())
    }

    fn wait_active(&self, n: usize) {
        let start = Instant::now();
        while self.active() != n {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "active connections: {} (wanted {n})",
                self.active()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn error_code(&self) -> String {
        parse(&self.body)
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }
}

fn parse(text: &str) -> Value {
    json::parse_value(text.as_bytes(), 8 * 1024 * 1024)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

fn read_response(s: &mut TcpStream) -> Resp {
    let mut raw = Vec::new();
    let _ = s.read_to_end(&mut raw);
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Resp {
    let text = String::from_utf8_lossy(raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Resp {
        status,
        headers,
        body: body.to_string(),
    }
}

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

fn assert_hardened(r: &Resp) {
    assert_eq!(r.header("content-security-policy"), Some(CSP));
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(r.header("referrer-policy"), Some("no-referrer"));
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(r.header("connection"), Some("close"));
    assert!(r.header("access-control-allow-origin").is_none());
}

// ---------- static files and headers ----------

#[test]
fn static_files_carry_the_security_headers_and_have_no_inline_code() {
    let r = Running::start();
    for (path, ctype) in [
        ("/", "text/html; charset=utf-8"),
        ("/app.js", "text/javascript; charset=utf-8"),
        ("/app.css", "text/css; charset=utf-8"),
    ] {
        let resp = r.get(path, &[]);
        assert_eq!(resp.status, 200, "{path}");
        assert_eq!(resp.header("content-type"), Some(ctype), "{path}");
        assert_hardened(&resp);
        assert!(!resp.body.is_empty());
    }
    let html = r.get("/", &[]).body;
    assert!(html.contains("<script src=\"/app.js\""));
    assert!(!html.contains("onclick") && !html.contains("style="));
    assert!(
        r.fake.calls().is_empty(),
        "static files never reach the daemon"
    );
}

#[test]
fn errors_and_unknown_paths_are_hardened_too() {
    let r = Running::start();
    for resp in [
        r.get("/nope", &[]),
        r.get("/api/nope", &[]),
        r.get("/api/status", &[]),
        r.request("DELETE", "/", &[], None),
        r.get("/", &[("Host", "evil.example")]),
    ] {
        assert_hardened(&resp);
        assert!(resp
            .header("content-type")
            .unwrap()
            .starts_with("application/json"));
    }
    assert_eq!(r.get("/nope", &[]).status, 404);
    assert_eq!(r.get("/../app.js", &[]).status, 404);
    assert_eq!(r.get("/app.js/", &[]).status, 404);
    assert_eq!(
        r.get("/%61pp.js", &[]).status,
        404,
        "paths are never percent-decoded"
    );
}

#[test]
fn methods_other_than_get_and_post_and_no_cors() {
    let r = Running::start();
    for m in [
        "PUT", "DELETE", "OPTIONS", "HEAD", "PATCH", "TRACE", "CONNECT",
    ] {
        let resp = r.request(m, "/api/status", &[], None);
        assert_eq!(resp.status, 405, "{m}");
        assert_hardened(&resp);
    }
    // a wrong method for a known route names the right one
    let resp = r.request("POST", "/api/status", &[], Some("{}"));
    assert_eq!(resp.status, 405);
    assert_eq!(resp.header("allow"), Some("GET"));
    assert_eq!(r.get("/api/send", &[]).status, 405);
    // a preflight is never answered with permission
    let pre = r.request(
        "OPTIONS",
        "/api/send",
        &[
            ("Origin", "http://evil.example"),
            ("Access-Control-Request-Method", "POST"),
        ],
        None,
    );
    assert!(pre.header("access-control-allow-origin").is_none());
    assert!(r.fake.calls().is_empty());
}

// ---------- Host ----------

#[test]
fn a_wrong_host_is_refused_before_authentication() {
    let r = Running::start();
    let (k, v) = r.bearer();
    let port = r.addr.port();
    for host in [
        "evil.example".to_string(),
        format!("evil.example:{port}"),
        format!("localhost.evil.example:{port}"),
        format!("127.0.0.1.nip.io:{port}"),
        format!("127.0.0.1:{}", port.wrapping_add(1)),
        "127.0.0.1".to_string(),
        format!("[::1]:{port}"),
        format!("0.0.0.0:{port}"),
        String::new(),
    ] {
        let resp = r.get("/api/status", &[("Host", &host), (&k, &v)]);
        assert_eq!(resp.status, 421, "Host {host:?}");
        assert_eq!(resp.error_code(), "forbidden");
        let page = r.get("/", &[("Host", &host)]);
        assert_eq!(page.status, 421, "page with Host {host:?}");
    }
    assert!(r.fake.calls().is_empty());
    // the names the daemon printed work
    for host in [format!("127.0.0.1:{port}"), format!("localhost:{port}")] {
        assert_eq!(
            r.get("/api/status", &[("Host", &host), (&k, &v)]).status,
            200
        );
    }
    // HTTP/1.1 without a Host header at all
    let resp = r.raw(b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n");
    assert_eq!(resp.status, 421);
}

// ---------- authentication ----------

#[test]
fn the_api_needs_credentials_and_the_bearer_token_works() {
    let r = Running::start();
    assert_eq!(r.get("/api/status", &[]).status, 401);
    assert_eq!(r.get("/api/session", &[]).status, 401);
    for bad in [
        "Bearer nope".to_string(),
        format!("Bearer {}x", r.token),
        format!("Basic {}", r.token),
        r.token.clone(),
        "Bearer ".to_string(),
        format!("Bearer {} extra", r.token),
    ] {
        let resp = r.get("/api/status", &[("Authorization", &bad)]);
        assert_eq!(resp.status, 401, "{bad}");
        assert_eq!(resp.error_code(), "forbidden");
    }
    assert!(r.fake.calls().is_empty());
    let ok = r.get_bearer("/api/status");
    assert_eq!(ok.status, 200, "{}", ok.body);
    assert_hardened(&ok);
    assert_eq!(r.fake.call_names(), vec!["status"]);
    // the token is never accepted in the URL
    let resp = r.get(&format!("/api/status?token={}", r.token), &[]);
    assert_eq!(resp.status, 401);
    let resp = r.get_bearer(&format!("/api/status?token={}", r.token));
    assert_eq!(resp.status, 400, "and not even as an extra parameter");
    assert_eq!(r.fake.call_names(), vec!["status"]);
}

#[test]
fn failed_credentials_lock_authentication_and_the_lock_applies_to_the_right_token_too() {
    let r = Running::with(WebLimits {
        auth: AuthLimits {
            max_failures: 3,
            failure_window: Duration::from_secs(60),
            lockout: Duration::from_secs(2),
            ..AuthLimits::default()
        },
        ..fast_limits()
    });
    for _ in 0..3 {
        assert_eq!(
            r.get("/api/status", &[("Authorization", "Bearer wrong")])
                .status,
            401
        );
    }
    let locked = r.get_bearer("/api/status");
    assert_eq!(locked.status, 429, "{}", locked.body);
    let retry: u64 = locked.header("retry-after").unwrap().parse().unwrap();
    assert!((1..=3).contains(&retry), "{retry}");
    assert_eq!(locked.error_code(), "limit");
    // a login with the right token is locked out as well, and static files still work
    let login = r.request(
        "POST",
        "/api/login",
        &[
            ("Origin", &r.origin()),
            ("Content-Type", "application/json"),
        ],
        Some(&format!("{{\"token\":\"{}\"}}", r.token)),
    );
    assert_eq!(login.status, 429);
    assert_eq!(r.get("/", &[]).status, 200);
    assert!(r.fake.calls().is_empty());
    // after the lock the right token works again
    std::thread::sleep(Duration::from_millis(2200));
    assert_eq!(r.get_bearer("/api/status").status, 200);
}

#[test]
fn a_lockout_does_not_cut_off_a_logged_in_session_and_unknown_sessions_are_not_failures() {
    let r = Running::with(WebLimits {
        auth: AuthLimits {
            max_failures: 3,
            failure_window: Duration::from_secs(60),
            lockout: Duration::from_secs(30),
            ..AuthLimits::default()
        },
        ..fast_limits()
    });
    let (session, csrf) = r.login();
    // a stale tab, a restarted daemon or a reload with an old id is not an attack
    for _ in 0..10 {
        let resp = r.get("/api/status", &[("X-Fleet-Session", &"0".repeat(64))]);
        assert_eq!(resp.status, 401);
    }
    assert_eq!(
        r.get_bearer("/api/status").status,
        200,
        "not locked by unknown sessions"
    );
    // wrong tokens (guessing) do lock the token paths ...
    for _ in 0..3 {
        assert_eq!(
            r.get("/api/status", &[("Authorization", "Bearer wrong")])
                .status,
            401
        );
    }
    assert_eq!(r.get_bearer("/api/status").status, 429);
    // ... but the operator's session keeps working: reads and decisions
    assert_eq!(
        r.get("/api/status", &[("X-Fleet-Session", &session)])
            .status,
        200
    );
    let resp = r.post_session("/api/merge", &session, &csrf, "{\"task\":\"r1/a\"}");
    assert_eq!(resp.status, 200, "{}", resp.body);
}

#[test]
fn login_issues_a_session_id_in_the_body_and_never_a_cookie() {
    let r = Running::start();
    let resp = r.request(
        "POST",
        "/api/login",
        &[
            ("Origin", &r.origin()),
            ("Content-Type", "application/json"),
        ],
        Some(&format!("{{\"token\":\"{}\"}}", r.token)),
    );
    assert_eq!(resp.status, 200);
    assert_hardened(&resp);
    assert!(
        resp.header("set-cookie").is_none(),
        "a cookie would travel to every other port of this host"
    );
    assert!(!resp.body.contains(&r.token));
    let doc = parse(&resp.body);
    assert_eq!(
        doc.get("schema").and_then(Value::as_str),
        Some("fleet.web.session/1")
    );
    let csrf = doc.get("csrf").and_then(Value::as_str).unwrap();
    let id = doc.get("session").and_then(Value::as_str).unwrap();
    for v in [csrf, id] {
        assert_eq!(v.len(), 64);
        assert!(v.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(v, r.token);
    }
    assert_ne!(csrf, id);
    // the session header authenticates reads without any other header
    let (session, _) = r.login();
    let status = r.get("/api/status", &[("X-Fleet-Session", &session)]);
    assert_eq!(status.status, 200);
    let doc = r.get("/api/session", &[("X-Fleet-Session", &session)]);
    assert_eq!(doc.status, 200);
    let doc = parse(&doc.body);
    assert_eq!(
        doc.get("csrf").and_then(Value::as_str).map(str::len),
        Some(64)
    );
    assert!(
        doc.get("session").unwrap().is_null(),
        "the id is not echoed"
    );
    assert_eq!(resp.body.matches(&r.token).count(), 0);
}

#[test]
fn a_session_id_sent_as_a_cookie_is_not_a_credential() {
    let r = Running::start();
    let (session, csrf) = r.login();
    let cookie = format!("fleet_session={session}");
    // reads
    let resp = r.get("/api/status", &[("Cookie", &cookie)]);
    assert_eq!(resp.status, 401, "{}", resp.body);
    assert_eq!(r.get("/api/session", &[("Cookie", &cookie)]).status, 401);
    assert_eq!(
        r.get("/api/stream?task=r1%2Fa", &[("Cookie", &cookie)])
            .status,
        401
    );
    // and writes, even with the right CSRF token and Origin, which is all a program that got hold
    // of an old-style cookie could add
    let resp = r.request(
        "POST",
        "/api/send",
        &[
            ("Cookie", &cookie),
            ("Origin", &r.origin()),
            ("X-CSRF-Token", &csrf),
            ("Content-Type", "application/json"),
        ],
        Some("{\"task\":\"r1/a\",\"text\":\"x\"}"),
    );
    assert_eq!(resp.status, 401, "{}", resp.body);
    // the session itself is still good, as a header
    assert_eq!(
        r.get("/api/status", &[("X-Fleet-Session", &session)])
            .status,
        200
    );
    assert!(
        r.fake.call_names().iter().all(|n| n == "status"),
        "{:?}",
        r.fake.call_names()
    );
    assert_eq!(r.fake.calls().len(), 1);
}

#[test]
fn login_rejects_wrong_tokens_bad_origins_and_bad_bodies() {
    let r = Running::start();
    let json_header = ("Content-Type", "application/json");
    let origin = r.origin();
    let wrong = r.request(
        "POST",
        "/api/login",
        &[("Origin", &origin), json_header],
        Some(&format!("{{\"token\":\"{}\"}}", "0".repeat(64))),
    );
    assert_eq!(wrong.status, 401);
    assert!(wrong.header("set-cookie").is_none());
    let body = format!("{{\"token\":\"{}\"}}", r.token);
    for hdrs in [
        vec![("Origin", "http://evil.example"), json_header],
        vec![("Origin", "null"), json_header],
        vec![("Origin", "https://127.0.0.1"), json_header],
    ] {
        let resp = r.request("POST", "/api/login", &hdrs, Some(&body));
        assert_eq!(resp.status, 403, "{hdrs:?}");
        assert!(resp.header("set-cookie").is_none());
    }
    // a browser (it sends Sec-Fetch-Site) must also send Origin
    let resp = r.request(
        "POST",
        "/api/login",
        &[("Sec-Fetch-Site", "same-origin"), json_header],
        Some(&body),
    );
    assert_eq!(resp.status, 403);
    let resp = r.request(
        "POST",
        "/api/login",
        &[
            ("Origin", &origin),
            ("Sec-Fetch-Site", "cross-site"),
            json_header,
        ],
        Some(&body),
    );
    assert_eq!(resp.status, 403);
    // a script without Origin and without fetch metadata may log in
    let resp = r.request("POST", "/api/login", &[json_header], Some(&body));
    assert_eq!(resp.status, 200);
    // wrong content type, unknown fields, wrong types
    let resp = r.request(
        "POST",
        "/api/login",
        &[("Origin", &origin), ("Content-Type", "text/plain")],
        Some(&body),
    );
    assert_eq!(resp.status, 415);
    for bad in [
        "{}".to_string(),
        format!("{{\"token\":\"{}\",\"extra\":1}}", r.token),
        "{\"token\":1}".to_string(),
        "{\"token\":null}".to_string(),
        "[\"x\"]".to_string(),
        "nope".to_string(),
        String::new(),
        format!("{{\"token\":\"{}\"}}", "a".repeat(200)),
    ] {
        let resp = r.request(
            "POST",
            "/api/login",
            &[("Origin", &origin), json_header],
            Some(&bad),
        );
        assert_eq!(resp.status, 400, "{bad}");
    }
}

#[test]
fn logout_ends_the_session() {
    let r = Running::start();
    let (session, csrf) = r.login();
    assert_eq!(
        r.get("/api/status", &[("X-Fleet-Session", &session)])
            .status,
        200
    );
    let out = r.post_session("/api/logout", &session, &csrf, "{}");
    assert_eq!(out.status, 200);
    assert!(out.header("set-cookie").is_none());
    assert_eq!(
        r.get("/api/status", &[("X-Fleet-Session", &session)])
            .status,
        401
    );
}

// ---------- CSRF, Origin, Sec-Fetch-Site, content type ----------

#[test]
fn state_changing_requests_need_origin_csrf_token_and_json() {
    let r = Running::start();
    let (session, csrf) = r.login();
    let body = "{\"task\":\"r1/a\",\"text\":\"hi\"}";
    let origin = r.origin();
    let json_header = ("Content-Type", "application/json");

    // no CSRF header
    let resp = r.request(
        "POST",
        "/api/send",
        &[
            ("X-Fleet-Session", &session),
            ("Origin", &origin),
            json_header,
        ],
        Some(body),
    );
    assert_eq!(resp.status, 403);
    // wrong CSRF header
    let resp = r.post_session("/api/send", &session, &"0".repeat(64), body);
    assert_eq!(resp.status, 403);
    // the bearer token is not a CSRF token
    let resp = r.post_session("/api/send", &session, &r.token, body);
    assert_eq!(resp.status, 403);
    // no Origin with a session
    let resp = r.request(
        "POST",
        "/api/send",
        &[
            ("X-Fleet-Session", &session),
            ("X-CSRF-Token", &csrf),
            json_header,
        ],
        Some(body),
    );
    assert_eq!(resp.status, 403);
    // foreign origins
    for bad in [
        "http://evil.example",
        "null",
        "https://127.0.0.1",
        &format!("http://127.0.0.1:{}", r.addr.port() + 1),
        &format!("{origin}.evil.example"),
    ] {
        let resp = r.request(
            "POST",
            "/api/send",
            &[
                ("X-Fleet-Session", &session),
                ("Origin", bad),
                ("X-CSRF-Token", &csrf),
                json_header,
            ],
            Some(body),
        );
        assert_eq!(resp.status, 403, "Origin {bad}");
    }
    // fetch metadata of a cross-site or same-site page
    for site in ["cross-site", "same-site", "none", "bogus"] {
        let resp = r.request(
            "POST",
            "/api/send",
            &[
                ("X-Fleet-Session", &session),
                ("Origin", &origin),
                ("X-CSRF-Token", &csrf),
                ("Sec-Fetch-Site", site),
                json_header,
            ],
            Some(body),
        );
        assert_eq!(resp.status, 403, "Sec-Fetch-Site {site}");
    }
    // not JSON: what a cross-site HTML form can send
    for ct in [
        "text/plain",
        "application/x-www-form-urlencoded",
        "multipart/form-data; boundary=x",
        "application/jsonx",
        "application/json; charset=latin1",
    ] {
        let resp = r.request(
            "POST",
            "/api/send",
            &[
                ("X-Fleet-Session", &session),
                ("Origin", &origin),
                ("X-CSRF-Token", &csrf),
                ("Content-Type", ct),
            ],
            Some(body),
        );
        assert_eq!(resp.status, 415, "{ct}");
    }
    let resp = r.request(
        "POST",
        "/api/send",
        &[
            ("X-Fleet-Session", &session),
            ("Origin", &origin),
            ("X-CSRF-Token", &csrf),
        ],
        Some(body),
    );
    assert_eq!(resp.status, 415);
    assert!(
        r.fake.calls().is_empty(),
        "nothing above reached the daemon"
    );

    // the real thing
    let resp = r.post_session("/api/send", &session, &csrf, body);
    assert_eq!(resp.status, 200, "{}", resp.body);
    let resp = r.request(
        "POST",
        "/api/send",
        &[
            ("X-Fleet-Session", &session),
            ("Origin", &origin),
            ("X-CSRF-Token", &csrf),
            ("Sec-Fetch-Site", "same-origin"),
            ("Content-Type", "application/json; charset=utf-8"),
        ],
        Some(body),
    );
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert_eq!(r.fake.call_names(), vec!["task.send", "task.send"]);
}

#[test]
fn cross_site_fetch_metadata_is_refused_on_reads_too() {
    let r = Running::start();
    let (k, v) = r.bearer();
    for site in ["cross-site", "same-site"] {
        let resp = r.get("/api/status", &[(&k, &v), ("Sec-Fetch-Site", site)]);
        assert_eq!(resp.status, 403, "{site}");
    }
    for site in ["same-origin", "none"] {
        assert_eq!(
            r.get("/api/status", &[(&k, &v), ("Sec-Fetch-Site", site)])
                .status,
            200,
            "{site}"
        );
    }
    assert_eq!(r.fake.call_names(), vec!["status", "status"]);
}

#[test]
fn a_token_bearing_script_needs_no_origin_but_a_foreign_one_is_refused() {
    let r = Running::start();
    let body = "{\"task\":\"r1/a\"}";
    assert_eq!(r.post_bearer("/api/merge", body).status, 200);
    let (k, v) = r.bearer();
    let resp = r.request(
        "POST",
        "/api/merge",
        &[
            (&k, &v),
            ("Content-Type", "application/json"),
            ("Origin", "http://evil.example"),
        ],
        Some(body),
    );
    assert_eq!(resp.status, 403);
    assert_eq!(r.fake.call_names(), vec!["merge"]);
}

// ---------- strict JSON bodies ----------

#[test]
fn bodies_are_validated_strictly_and_rejected_before_the_daemon_sees_them() {
    let r = Running::start();
    let digest = "ab".repeat(32);
    let big_text = "x".repeat(5000);
    let cases: Vec<(&str, String)> = vec![
        ("/api/send", "{\"task\":\"r1/a\"}".into()),
        ("/api/send", "{\"task\":\"r1/a\",\"text\":\"\"}".into()),
        ("/api/send", "{\"task\":\"r1/a\",\"text\":5}".into()),
        (
            "/api/send",
            "{\"task\":\"r1/a\",\"text\":\"x\",\"newline\":\"yes\"}".into(),
        ),
        (
            "/api/send",
            "{\"task\":\"r1/a\",\"text\":\"x\",\"extra\":true}".into(),
        ),
        (
            "/api/send",
            format!("{{\"task\":\"r1/a\",\"text\":\"{big_text}\"}}"),
        ),
        ("/api/send", "{\"task\":null,\"text\":\"x\"}".into()),
        ("/api/send", "[]".into()),
        ("/api/send", "\"text\"".into()),
        ("/api/send", "{\"task\":\"r1/a\",\"text\":\"x\"".into()),
        (
            "/api/send",
            "{\"task\":\"r1/a\",\"text\":\"\u{1}\u{ff}\"}x".into(),
        ),
        ("/api/merge", "{}".into()),
        ("/api/merge", "{\"task\":[\"r1/a\"]}".into()),
        ("/api/gate/resolve", "{\"id\":\"g1\"}".into()),
        (
            "/api/gate/resolve",
            "{\"id\":\"g1\",\"answer\":\"\"}".into(),
        ),
        ("/api/gate/resolve", "{\"id\":7,\"answer\":\"x\"}".into()),
        (
            "/api/approval/decide",
            "{\"id\":\"a1\",\"approve\":true}".to_string(),
        ),
        (
            "/api/approval/decide",
            format!("{{\"id\":\"a1\",\"digest\":\"{digest}\"}}"),
        ),
        (
            "/api/approval/decide",
            format!("{{\"id\":\"a1\",\"approve\":\"true\",\"digest\":\"{digest}\"}}"),
        ),
        (
            "/api/approval/decide",
            "{\"id\":\"a1\",\"approve\":true,\"digest\":\"abc\"}".into(),
        ),
        (
            "/api/approval/decide",
            format!(
                "{{\"id\":\"a1\",\"approve\":true,\"digest\":\"{}\"}}",
                "g".repeat(64)
            ),
        ),
        (
            "/api/approval/decide",
            format!("{{\"id\":\"a1\",\"approve\":true,\"digest\":\"{digest}\",\"run\":\"r1\"}}"),
        ),
        ("/api/logout", "{\"x\":1}".into()),
    ];
    for (path, body) in &cases {
        let resp = r.post_bearer(path, body);
        assert_eq!(resp.status, 400, "{path} {body}");
        assert_eq!(resp.error_code(), "invalid");
    }
    assert!(r.fake.calls().is_empty(), "{:?}", r.fake.calls());
    // well formed ones go through with exactly the validated fields
    let resp = r.post_bearer(
        "/api/approval/decide",
        &format!("{{\"id\":\"a1\",\"approve\":true,\"digest\":\"{digest}\"}}"),
    );
    assert_eq!(resp.status, 200);
    let calls = r.fake.calls();
    let (name, args) = &calls[0];
    assert_eq!(name, "approval.decide");
    assert_eq!(args.get("id").and_then(Value::as_str), Some("a1"));
    assert_eq!(args.get("approve").and_then(Value::as_bool), Some(true));
    assert_eq!(
        args.get("digest").and_then(Value::as_str),
        Some(digest.as_str())
    );
    assert_eq!(args.as_object().unwrap().len(), 3);
}

#[test]
fn the_goal_ledger_and_verification_are_served_read_only_through_the_daemons_own_commands() {
    let r = Running::start();
    let resp = r.get_bearer("/api/goals");
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert_eq!(r.get_bearer("/api/remaining").status, 200);
    assert_eq!(r.get_bearer("/api/goal?id=ship").status, 200);
    assert_eq!(
        r.get_bearer("/api/verify?target=r1%2Fa&task=r1%2Fa&ticket=7")
            .status,
        200
    );
    assert_eq!(
        r.get_bearer("/api/verify?target=ship").status,
        200,
        "task and ticket are optional"
    );
    let calls = r.fake.calls();
    let names: Vec<&str> = calls.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "goal.list",
            "goal.remaining",
            "goal.show",
            "verify.status",
            "verify.status"
        ]
    );
    assert_eq!(calls[2].1.get("goal").and_then(Value::as_str), Some("ship"));
    let v = &calls[3].1;
    assert_eq!(v.get("target").and_then(Value::as_str), Some("r1/a"));
    assert_eq!(v.get("task").and_then(Value::as_str), Some("r1/a"));
    assert_eq!(v.get("ticket").and_then(Value::as_u64), Some(7));
    assert!(calls[4].1.get("task").is_none() && calls[4].1.get("ticket").is_none());
    // they need credentials like everything else
    for path in [
        "/api/goals",
        "/api/remaining",
        "/api/goal?id=g",
        "/api/verify?target=g",
    ] {
        assert_eq!(r.get(path, &[]).status, 401, "{path}");
    }
    // nothing here changes anything: no way to add, attach, set a status or start a verification
    for (m, path) in [
        ("POST", "/api/goals"),
        ("POST", "/api/goal"),
        ("POST", "/api/verify"),
        ("POST", "/api/goal/add"),
        ("POST", "/api/goal/attach"),
        ("POST", "/api/goal/status"),
    ] {
        let resp = r.request(m, path, &[], Some("{}"));
        assert!(
            matches!(resp.status, 404 | 405 | 401),
            "{m} {path} -> {}",
            resp.status
        );
    }
    let (session, csrf) = r.login();
    for path in ["/api/goals", "/api/goal", "/api/verify", "/api/remaining"] {
        let resp = r.post_session(path, &session, &csrf, "{}");
        assert_eq!(resp.status, 405, "{path}");
    }
    assert_eq!(r.fake.calls().len(), 5, "no write reached the daemon");
}

#[test]
fn goal_and_verification_queries_are_validated() {
    let r = Running::start();
    for q in [
        "/api/goal",
        "/api/goal?id=",
        "/api/goal?id=a&id=b",
        "/api/goal?id=a&x=1",
        "/api/goals?x=1",
        "/api/remaining?x=1",
        "/api/verify",
        "/api/verify?target=",
        "/api/verify?target=a&ticket=-1",
        "/api/verify?target=a&ticket=x",
        "/api/verify?target=a&bogus=1",
        "/api/goal?id=%ZZ",
    ] {
        let resp = r.get_bearer(q);
        assert_eq!(resp.status, 400, "{q}: {}", resp.body);
    }
    assert!(r.fake.calls().is_empty());
}

#[test]
fn agent_messages_are_served_read_only_through_msg_list_and_msg_show() {
    let r = Running::start();
    let resp = r.get_bearer("/api/messages");
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert_eq!(
        r.get_bearer("/api/messages?channel=dev&task=r1%2Fa&limit=20")
            .status,
        200
    );
    assert_eq!(r.get_bearer("/api/message?id=m1").status, 200);
    let calls = r.fake.calls();
    let names: Vec<&str> = calls.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["msg.list", "msg.list", "msg.show"]);
    assert!(calls[0].1.get("channel").is_none() && calls[0].1.get("limit").is_none());
    let v = &calls[1].1;
    assert_eq!(v.get("channel").and_then(Value::as_str), Some("dev"));
    assert_eq!(v.get("task").and_then(Value::as_str), Some("r1/a"));
    assert_eq!(v.get("limit").and_then(Value::as_u64), Some(20));
    assert_eq!(calls[2].1.get("id").and_then(Value::as_str), Some("m1"));
    // they need credentials like everything else
    for path in ["/api/messages", "/api/message?id=m1"] {
        assert_eq!(r.get(path, &[]).status, 401, "{path}");
    }
    assert_eq!(r.fake.calls().len(), 3);
}

#[test]
fn message_queries_are_validated() {
    let r = Running::start();
    for q in [
        "/api/message",
        "/api/message?id=",
        "/api/message?id=a&id=b",
        "/api/message?id=a&x=1",
        "/api/message?id=%ZZ",
        "/api/messages?x=1",
        "/api/messages?limit=-1",
        "/api/messages?limit=x",
        "/api/messages?channel=a&channel=b",
    ] {
        let resp = r.get_bearer(q);
        assert_eq!(resp.status, 400, "{q}: {}", resp.body);
    }
    assert!(r.fake.calls().is_empty());
}

#[test]
fn sending_muting_and_unmuting_messages_has_no_web_route() {
    let r = Running::start();
    let paths = [
        "/api/messages",
        "/api/message",
        "/api/msg/send",
        "/api/msg/mute",
        "/api/msg/unmute",
        "/api/msg",
        "/api/messages/send",
        "/api/message/send",
    ];
    for path in paths {
        let resp = r.request("POST", path, &[], Some("{}"));
        assert!(
            matches!(resp.status, 404 | 405 | 401),
            "POST {path} -> {}",
            resp.status
        );
        let resp = r.post_bearer(path, "{\"id\":\"m1\",\"task\":\"r1/a\",\"text\":\"x\"}");
        assert!(
            matches!(resp.status, 404 | 405),
            "authenticated POST {path} -> {}",
            resp.status
        );
    }
    let (session, csrf) = r.login();
    for path in paths {
        let resp = r.post_session(path, &session, &csrf, "{}");
        assert!(
            matches!(resp.status, 404 | 405),
            "session POST {path} -> {}",
            resp.status
        );
    }
    // a command name in a query or a body is never run either
    for q in [
        "/api/messages?cmd=msg.send",
        "/api/message?id=m&cmd=msg.mute",
    ] {
        assert_eq!(r.get_bearer(q).status, 400, "{q}");
    }
    for name in r.fake.call_names() {
        assert!(
            !matches!(name.as_str(), "msg.send" | "msg.mute" | "msg.unmute"),
            "{name} reached the daemon"
        );
    }
    assert!(r.fake.calls().is_empty(), "no write reached the daemon");
}

#[test]
fn hostile_message_text_stays_inert_in_the_served_documents() {
    let r = Running::start();
    let nasty = "<img src=x onerror=alert(1)>\u{1b}]8;;http://evil\u{7}x\u{202e}y";
    let expected = nasty.to_string();
    r.fake.on(move |cmd, _| {
        Ok(Value::object([
            ("schema", Value::str(format!("fleet.{cmd}/1"))),
            ("subject", Value::str(nasty)),
            ("body", Value::str(nasty)),
            (
                "label",
                Value::str("from task r1/a (unverified peer message)"),
            ),
        ]))
    });
    for path in ["/api/messages", "/api/message?id=m1"] {
        let resp = r.get_bearer(path);
        assert_eq!(resp.status, 200, "{path}");
        for c in ['<', '>', '&', '\u{1b}', '\u{202e}', '\u{7}'] {
            assert!(!resp.body.contains(c), "{path}: raw {c:?}");
        }
        let doc = parse(&resp.body);
        assert_eq!(
            doc.get("subject").and_then(Value::as_str),
            Some(expected.as_str())
        );
    }
}

#[test]
fn hostile_goal_text_and_evidence_output_stay_inert_in_the_served_documents() {
    let r = Running::start();
    let nasty = "<img src=x onerror=alert(1)>\u{1b}]8;;http://evil\u{7}x\u{202e}y";
    let expected = nasty.to_string();
    r.fake.on(move |cmd, _| {
        Ok(Value::object([
            ("schema", Value::str(format!("fleet.{cmd}/1"))),
            ("title", Value::str(nasty)),
            (
                "history",
                Value::array([Value::object([
                    ("output", Value::str(nasty)),
                    ("commit", Value::str("a".repeat(40))),
                ])]),
            ),
        ]))
    });
    for path in [
        "/api/goals",
        "/api/goal?id=g",
        "/api/remaining",
        "/api/verify?target=g",
    ] {
        let resp = r.get_bearer(path);
        assert_eq!(resp.status, 200, "{path}");
        for c in ['<', '>', '&', '\u{1b}', '\u{202e}', '\u{7}'] {
            assert!(!resp.body.contains(c), "{path}: raw {c:?}");
        }
        let doc = parse(&resp.body);
        assert_eq!(
            doc.get("title").and_then(Value::as_str),
            Some(expected.as_str())
        );
    }
}

#[test]
fn text_sent_to_a_worker_may_not_carry_terminal_control_characters() {
    let r = Running::start();
    // what a pasted escape sequence, a carriage return or a bidi override could do to a terminal
    // program that reads the input
    for text in [
        "a\u{1b}[31mb",
        "a\u{1b}]0;title\u{7}",
        "line\rtwo",
        "a\u{3}b",
        "a\u{8}b",
        "a\u{7f}b",
        "a\u{202e}b",
        "a\u{200b}b",
        "a\u{85}b",
    ] {
        let body = format!(
            "{{\"task\":\"r1/a\",\"text\":{}}}",
            fleet_infra::json::to_line(&Value::str(text))
        );
        let resp = r.post_bearer("/api/send", &body);
        assert_eq!(resp.status, 400, "{text:?}");
        assert_eq!(resp.error_code(), "invalid");
    }
    assert!(r.fake.calls().is_empty());
    // ordinary text, tabs, line breaks and non-ASCII letters are fine
    let ok = "ls -la\tdone\nnaïve 日本語";
    let body = format!(
        "{{\"task\":\"r1/a\",\"text\":{}}}",
        fleet_infra::json::to_line(&Value::str(ok))
    );
    assert_eq!(r.post_bearer("/api/send", &body).status, 200);
    let calls = r.fake.calls();
    assert_eq!(calls[0].1.get("text").and_then(Value::as_str), Some(ok));
}

#[test]
fn query_strings_are_validated_too() {
    let r = Running::start();
    for q in [
        "/api/tail",
        "/api/tail?task=",
        "/api/tail?task=r1%2Fa&since=-1",
        "/api/tail?task=r1%2Fa&since=1x",
        "/api/tail?task=r1%2Fa&since=99999999999999999999999",
        "/api/tail?task=r1%2Fa&task=r1%2Fb",
        "/api/tail?task=r1%2Fa&bogus=1",
        "/api/tail?task=%ZZ",
        "/api/tail?task=%FF",
        "/api/tail?task=a%00b",
        "/api/events?limit=abc",
        "/api/status?x=1",
    ] {
        let resp = r.get_bearer(q);
        assert_eq!(resp.status, 400, "{q}: {}", resp.body);
    }
    assert!(r.fake.calls().is_empty());
    let ok = r.get_bearer("/api/tail?task=r1%2Fa&since=5&max=100");
    assert_eq!(ok.status, 200);
    let calls = r.fake.calls();
    assert_eq!(calls[0].1.get("task").and_then(Value::as_str), Some("r1/a"));
    assert_eq!(calls[0].1.get("since").and_then(Value::as_u64), Some(5));
    assert_eq!(calls[0].1.get("max").and_then(Value::as_u64), Some(100));
}

// ---------- backend results ----------

#[test]
fn backend_errors_become_http_statuses_with_the_same_error_document() {
    use fleet_domain::error::ErrorCode;
    let r = Running::start();
    let table = [
        (ErrorCode::NotFound, 404),
        (ErrorCode::Invalid, 400),
        (ErrorCode::Conflict, 409),
        (ErrorCode::ApprovalRequired, 409),
        (ErrorCode::Forbidden, 403),
        (ErrorCode::Unavailable, 503),
        (ErrorCode::Timeout, 504),
        (ErrorCode::Limit, 413),
        (ErrorCode::Internal, 500),
    ];
    for (code, status) in table {
        r.fake
            .on(move |_, _| Err(FleetError::new(code, "boom <b>").with_subject("a7")));
        let resp = r.post_bearer("/api/merge", "{\"task\":\"r1/a\"}");
        assert_eq!(resp.status, status, "{code:?}");
        let doc = parse(&resp.body);
        assert_eq!(
            doc.get("schema").and_then(Value::as_str),
            Some("fleet.error/1")
        );
        let e = doc.get("error").unwrap();
        assert_eq!(e.get("code").and_then(Value::as_str), Some(code.as_str()));
        assert_eq!(e.get("subject").and_then(Value::as_str), Some("a7"));
        assert_eq!(e.get("message").and_then(Value::as_str), Some("boom <b>"));
        assert!(!resp.body.contains('<'), "raw markup in {}", resp.body);
        assert_hardened(&resp);
    }
}

#[test]
fn hostile_text_in_replies_is_json_escaped_and_survives_a_round_trip() {
    let r = Running::start();
    let nasty = "<script>alert(1)</script><img src=x onerror=alert(1)> & \"q\" \u{1b}[31m \u{1b}]0;t\u{7} \u{202e}evil \u{2028} </body>";
    let expected = nasty.to_string();
    r.fake.on(move |_, _| {
        Ok(Value::object([
            ("schema", Value::str("fleet.status/1")),
            ("title", Value::str(nasty)),
            ("list", Value::array([Value::str(nasty)])),
        ]))
    });
    let resp = r.get_bearer("/api/status");
    assert_eq!(resp.status, 200);
    for c in ['<', '>', '&', '\u{1b}', '\u{202e}', '\u{2028}'] {
        assert!(!resp.body.contains(c), "raw {c:?} in the body");
    }
    assert_eq!(
        resp.header("content-type"),
        Some("application/json; charset=utf-8")
    );
    let doc = parse(&resp.body);
    assert_eq!(
        doc.get("title").and_then(Value::as_str),
        Some(expected.as_str())
    );
}

#[test]
fn health_and_log_documents_are_cached_and_report_tampering() {
    let r = Running::start();
    for _ in 0..4 {
        let resp = r.get_bearer("/api/health");
        assert_eq!(resp.status, 200);
        assert_eq!(
            parse(&resp.body).get("schema").and_then(Value::as_str),
            Some("fleet.doctor/1")
        );
    }
    assert_eq!(
        r.fake.health_computed.load(Ordering::SeqCst),
        1,
        "one computation serves a burst"
    );
    let log = parse(&r.get_bearer("/api/log").body);
    assert_eq!(log.get("ok").and_then(Value::as_bool), Some(true));
    assert_eq!(log.get("records").and_then(Value::as_u64), Some(7));
    assert_eq!(
        log.get("head_hash").and_then(Value::as_str),
        Some("ab".repeat(32).as_str())
    );
    assert!(log.get("error").unwrap().is_null());

    // a tampered log is reported as such (a fresh server: the first answer was cached)
    let r = Running::start();
    *r.fake.log.lock().unwrap() = Err(FleetError::new(
        fleet_domain::error::ErrorCode::Tampered,
        "record 3 does not match its hash",
    ));
    let resp = r.get_bearer("/api/log");
    assert_eq!(resp.status, 200);
    let log = parse(&resp.body);
    assert_eq!(log.get("ok").and_then(Value::as_bool), Some(false));
    assert_eq!(
        log.get("error").and_then(Value::as_str),
        Some("record 3 does not match its hash")
    );
    assert!(log.get("records").unwrap().is_null());
}

// ---------- admission (worker origin) ----------

#[test]
fn a_refused_process_gets_403_and_its_request_never_reaches_the_daemon() {
    let r = Running::start();
    r.fake.deny.store(true, Ordering::SeqCst);
    // the token is right, the Host is right: the process itself is what is refused
    let resp = r.get_bearer("/api/status");
    assert_eq!(resp.status, 403);
    assert_eq!(resp.error_code(), "forbidden");
    assert!(resp.body.contains("worker processes"), "{}", resp.body);
    assert_hardened(&resp);
    assert_eq!(
        r.post_bearer("/api/merge", "{\"task\":\"r1/a\"}").status,
        403
    );
    assert_eq!(r.get("/", &[]).status, 403, "even the static page");
    // a login with the right token gets no session
    let login = r.request(
        "POST",
        "/api/login",
        &[
            ("Origin", &r.origin()),
            ("Content-Type", "application/json"),
        ],
        Some(&format!("{{\"token\":\"{}\"}}", r.token)),
    );
    assert_eq!(login.status, 403);
    assert!(!login.body.contains("\"session\""), "{}", login.body);
    assert!(r.fake.calls().is_empty());
    // the pid the backend was asked about is this test process (the client end of the socket)
    let pids = r.fake.admitted_pids.lock().unwrap().clone();
    assert!(
        !pids.is_empty() && pids.iter().all(|p| *p == std::process::id()),
        "{pids:?}"
    );
    // and the same process is served once it is no longer refused
    r.fake.deny.store(false, Ordering::SeqCst);
    assert_eq!(r.get_bearer("/api/status").status, 200);
}

#[test]
fn a_session_id_carried_to_a_refused_process_is_useless_there() {
    // admission runs on every connection, whatever credential it carries
    let r = Running::start();
    let (session, _) = r.login();
    assert_eq!(
        r.get("/api/status", &[("X-Fleet-Session", &session)])
            .status,
        200
    );
    r.fake.deny.store(true, Ordering::SeqCst);
    let resp = r.get("/api/status", &[("X-Fleet-Session", &session)]);
    assert_eq!(resp.status, 403);
    assert_eq!(r.fake.call_names(), vec!["status"]);
}

#[test]
fn a_connection_that_sends_nothing_costs_no_lookup_and_is_cut_off_quickly() {
    let r = Running::with(WebLimits {
        http: Limits {
            first_byte_deadline: Duration::from_millis(300),
            ..fast_limits().http
        },
        ..fast_limits()
    });
    let started = Instant::now();
    let mut idle: Vec<TcpStream> = (0..5)
        .map(|_| TcpStream::connect(r.addr).unwrap())
        .collect();
    for s in &mut idle {
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let resp = read_response(s);
        assert_eq!(resp.status, 408);
    }
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert!(
        r.fake.admitted_pids.lock().unwrap().is_empty(),
        "the process table was never read for connections that sent nothing"
    );
    r.wait_active(0);
    // a client that sends its first byte in time gets the longer deadline for the rest
    let mut s = TcpStream::connect(r.addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"GET / HT").unwrap();
    std::thread::sleep(Duration::from_millis(500));
    s.write_all(format!("TP/1.1\r\nHost: {}\r\n\r\n", r.host()).as_bytes())
        .unwrap();
    assert_eq!(read_response(&mut s).status, 200);
}

// ---------- malformed and oversized input ----------

#[test]
fn malformed_requests_get_an_error_and_the_server_keeps_serving() {
    let r = Running::start();
    let host = r.host();
    let long_target = format!("GET /{} HTTP/1.1\r\nHost: {host}\r\n\r\n", "a".repeat(3000));
    let big_header = format!(
        "GET / HTTP/1.1\r\nHost: {host}\r\nX-Big: {}\r\n\r\n",
        "a".repeat(20_000)
    );
    let many_headers = format!(
        "GET / HTTP/1.1\r\nHost: {host}\r\n{}\r\n",
        (0..60).map(|i| format!("X-{i}: v\r\n")).collect::<String>()
    );
    let cases: Vec<(&str, Vec<u8>, u16)> = vec![
        ("garbage", b"\x00\x01\x02garbage\r\n\r\n".to_vec(), 400),
        ("no version", b"GET /\r\n\r\n".to_vec(), 400),
        ("http 2 preface", b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec(), 505),
        ("bad version", format!("GET / HTTP/9.9\r\nHost: {host}\r\n\r\n").into_bytes(), 505),
        ("absolute target", format!("GET http://{host}/ HTTP/1.1\r\nHost: {host}\r\n\r\n").into_bytes(), 400),
        ("fragment", format!("GET /#x HTTP/1.1\r\nHost: {host}\r\n\r\n").into_bytes(), 400),
        ("long target", long_target.into_bytes(), 414),
        ("huge header", big_header.into_bytes(), 431),
        ("many headers", many_headers.into_bytes(), 431),
        ("non ascii header", format!("GET / HTTP/1.1\r\nHost: {host}\r\nX: caf\u{e9}\r\n\r\n").into_bytes(), 400),
        ("duplicate host", format!("GET / HTTP/1.1\r\nHost: {host}\r\nHost: {host}\r\n\r\n").into_bytes(), 400),
        ("duplicate session header", format!("GET / HTTP/1.1\r\nHost: {host}\r\nX-Fleet-Session: a\r\nX-Fleet-Session: b\r\n\r\n").into_bytes(), 400),
        ("bad content length", format!("POST /api/send HTTP/1.1\r\nHost: {host}\r\nContent-Length: ten\r\n\r\n").into_bytes(), 400),
        ("two content lengths", format!("POST /api/send HTTP/1.1\r\nHost: {host}\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n{{}}").into_bytes(), 400),
        ("chunked", format!("POST /api/send HTTP/1.1\r\nHost: {host}\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n").into_bytes(), 501),
        ("chunked with length", format!("POST /api/send HTTP/1.1\r\nHost: {host}\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n{{}}").into_bytes(), 501),
        ("no length", format!("POST /api/send HTTP/1.1\r\nHost: {host}\r\n\r\n").into_bytes(), 411),
        ("expect", format!("POST /api/send HTTP/1.1\r\nHost: {host}\r\nContent-Length: 2\r\nExpect: 100-continue\r\n\r\n{{}}").into_bytes(), 417),
        ("get with body", format!("GET / HTTP/1.1\r\nHost: {host}\r\nContent-Length: 2\r\n\r\n{{}}").into_bytes(), 400),
        ("pipelined", format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\nGET / HTTP/1.1\r\nHost: {host}\r\n\r\n").into_bytes(), 400),
        ("bare lf", format!("GET / HTTP/1.1\nHost: {host}\n\n").into_bytes(), 408),
    ];
    for (name, bytes, want) in cases {
        let mut s = TcpStream::connect(r.addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let _ = s.write_all(&bytes);
        if want == 408 {
            // never completes: the deadline answers
            let resp = read_response(&mut s);
            assert_eq!(resp.status, want, "{name}");
            continue;
        }
        let _ = s.shutdown(Shutdown::Write);
        let resp = read_response(&mut s);
        assert_eq!(resp.status, want, "{name}: {}", resp.body);
        assert_hardened(&resp);
    }
    assert!(r.fake.calls().is_empty());
    assert_eq!(r.get("/", &[]).status, 200, "still serving");
}

#[test]
fn oversize_bodies_are_refused_from_the_declared_length_without_reading_them() {
    let r = Running::start();
    let (k, v) = r.bearer();
    let mut s = TcpStream::connect(r.addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let head = format!(
        "POST /api/send HTTP/1.1\r\nHost: {}\r\n{k}: {v}\r\nContent-Type: application/json\r\nContent-Length: 10000000\r\n\r\n",
        r.host()
    );
    s.write_all(head.as_bytes()).unwrap();
    // the body is not even sent: the length alone is enough
    let resp = read_response(&mut s);
    assert_eq!(resp.status, 413);
    assert_eq!(resp.error_code(), "limit");
    // a body just over the limit is refused as well, and exactly the limit is read
    let limit = Limits::default().max_body;
    let over = format!("{{\"task\":\"r1/a\",\"text\":\"{}\"}}", "x".repeat(limit));
    assert_eq!(r.post_bearer("/api/send", &over).status, 413);
    assert!(r.fake.calls().is_empty());
}

// ---------- slow and many clients ----------

#[test]
fn a_client_that_trickles_bytes_is_cut_off_by_one_deadline_for_the_whole_request() {
    let r = Running::with(WebLimits {
        http: Limits {
            request_deadline: Duration::from_millis(600),
            ..Limits::default()
        },
        ..fast_limits()
    });
    let mut slow = TcpStream::connect(r.addr).unwrap();
    slow.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let started = Instant::now();
    let head = format!("GET / HTTP/1.1\r\nHost: {}\r\nX-Slow: ", r.host());
    let feeder = {
        let mut w = slow.try_clone().unwrap();
        std::thread::spawn(move || {
            let _ = w.write_all(head.as_bytes());
            // a byte every 100 ms would keep a per-read timeout alive forever
            for _ in 0..40 {
                std::thread::sleep(Duration::from_millis(100));
                if w.write_all(b"a").is_err() {
                    break;
                }
            }
        })
    };
    // meanwhile the server answers everybody else at once
    let t = Instant::now();
    assert_eq!(r.get("/", &[]).status, 200);
    assert!(t.elapsed() < Duration::from_millis(500));
    let resp = read_response(&mut slow);
    let waited = started.elapsed();
    assert_eq!(resp.status, 408, "{}", resp.body);
    assert!(
        waited >= Duration::from_millis(500) && waited < Duration::from_millis(2500),
        "cut off after {waited:?}"
    );
    feeder.join().unwrap();
    r.wait_active(0);
}

#[test]
fn a_body_that_never_arrives_times_out_too() {
    let r = Running::with(WebLimits {
        http: Limits {
            request_deadline: Duration::from_millis(500),
            ..Limits::default()
        },
        ..fast_limits()
    });
    let (k, v) = r.bearer();
    let mut s = TcpStream::connect(r.addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let head = format!(
        "POST /api/send HTTP/1.1\r\nHost: {}\r\n{k}: {v}\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{{\"task\"",
        r.host()
    );
    s.write_all(head.as_bytes()).unwrap();
    let resp = read_response(&mut s);
    assert_eq!(resp.status, 408);
    assert!(r.fake.calls().is_empty());
}

#[test]
fn the_connection_cap_refuses_extra_clients_at_once_and_recovers() {
    let r = Running::with(WebLimits {
        max_connections: 3,
        http: Limits {
            request_deadline: Duration::from_secs(20),
            ..Limits::default()
        },
        ..fast_limits()
    });
    let idle: Vec<TcpStream> = (0..3)
        .map(|_| TcpStream::connect(r.addr).unwrap())
        .collect();
    r.wait_active(3);
    let started = Instant::now();
    let resp = r.get("/", &[]);
    assert_eq!(resp.status, 503, "{}", resp.body);
    assert_eq!(resp.error_code(), "limit");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "refused at once"
    );
    drop(idle);
    r.wait_active(0);
    assert_eq!(r.get("/", &[]).status, 200);
}

// ---------- server-sent events ----------

fn tail_reply(text: &str, next: u64, live: bool, lost: bool) -> Value {
    Value::object([
        ("schema", Value::str("fleet.tail/1")),
        ("task", Value::str("r1/a")),
        ("live", Value::Bool(live)),
        ("text", Value::str(text)),
        ("next", Value::int(next)),
        ("lost", Value::Bool(lost)),
    ])
}

/// A backend whose `task.tail` answers come from a script; when it runs dry the task stays live
/// and quiet at the last offset.
fn script_tail(r: &Running, replies: Vec<Value>) {
    let queue = Mutex::new(VecDeque::from(replies));
    let last = Mutex::new(0u64);
    r.fake.on(move |cmd, args| {
        if cmd != "task.tail" {
            return Ok(Value::Null);
        }
        if let Some(next) = queue.lock().unwrap().pop_front() {
            *last.lock().unwrap() = next.get("next").and_then(Value::as_u64).unwrap_or(0);
            return Ok(next);
        }
        let _ = args;
        Ok(tail_reply("", *last.lock().unwrap(), true, false))
    });
}

struct Sse {
    stream: TcpStream,
    buf: String,
}

impl Sse {
    /// Opens a stream authenticated with the bearer token.
    fn open(r: &Running, path: &str, extra: &[(&str, &str)]) -> Sse {
        let (k, v) = r.bearer();
        let mut headers = vec![(k.as_str(), v.as_str())];
        headers.extend_from_slice(extra);
        Sse::open_as(r, path, &headers)
    }

    /// Opens a stream with exactly the given headers as credentials.
    fn open_as(r: &Running, path: &str, extra: &[(&str, &str)]) -> Sse {
        let mut s = TcpStream::connect(r.addr).unwrap();
        s.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut req = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\n",
            r.host()
        );
        for (a, b) in extra {
            req.push_str(&format!("{a}: {b}\r\n"));
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes()).unwrap();
        Sse {
            stream: s,
            buf: String::new(),
        }
    }

    /// Reads until `done(buffer)` or the stream ends or the timeout passes; true when `done`.
    fn read_until(&mut self, timeout: Duration, done: impl Fn(&str) -> bool) -> bool {
        let end = Instant::now() + timeout;
        let mut chunk = [0u8; 4096];
        while Instant::now() < end {
            if done(&self.buf) {
                return true;
            }
            match self.stream.read(&mut chunk) {
                Ok(0) => return done(&self.buf),
                Ok(n) => self.buf.push_str(&String::from_utf8_lossy(&chunk[..n])),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return done(&self.buf),
            }
        }
        done(&self.buf)
    }

    /// True when the server closed the connection within the timeout.
    fn closed_within(&mut self, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        let mut chunk = [0u8; 4096];
        while Instant::now() < end {
            match self.stream.read(&mut chunk) {
                Ok(0) => return true,
                Ok(n) => self.buf.push_str(&String::from_utf8_lossy(&chunk[..n])),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return true,
            }
        }
        false
    }

    fn events(&self) -> Vec<(String, Value)> {
        let body = self.buf.split_once("\r\n\r\n").map_or("", |(_, b)| b);
        body.split("\n\n")
            .filter_map(|block| {
                let name = block.lines().find_map(|l| l.strip_prefix("event: "))?;
                let data = block.lines().find_map(|l| l.strip_prefix("data: "))?;
                Some((name.to_string(), parse(data)))
            })
            .collect()
    }
}

#[test]
fn an_event_stream_delivers_output_then_end_and_closes() {
    let r = Running::start();
    script_tail(
        &r,
        vec![
            tail_reply("first <b>line</b>\n", 18, true, false),
            tail_reply("second \u{1b}? line\n", 36, true, false),
            tail_reply("", 36, false, false),
        ],
    );
    let mut sse = Sse::open(&r, "/api/stream?task=r1%2Fa", &[]);
    assert!(
        sse.read_until(Duration::from_secs(5), |b| b.contains("event: end")),
        "{}",
        sse.buf
    );
    assert!(sse.closed_within(Duration::from_secs(2)));
    let head = sse
        .buf
        .split_once("\r\n\r\n")
        .unwrap()
        .0
        .to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 200"));
    assert!(head.contains("content-type: text/event-stream"));
    assert!(head.contains("cache-control: no-store"));
    assert!(head.contains("content-security-policy:"));
    assert!(!head.contains("content-length"));
    let events = sse.events();
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["output", "output", "end"], "{}", sse.buf);
    assert_eq!(
        events[0].1.get("text").and_then(Value::as_str),
        Some("first <b>line</b>\n")
    );
    assert_eq!(events[1].1.get("next").and_then(Value::as_u64), Some(36));
    assert!(
        !sse.buf.contains('<'),
        "markup is escaped inside the event data"
    );
    assert!(sse.buf.contains("id: 18\n") && sse.buf.contains("id: 36\n"));
    // later polls asked for the offset after what was already sent
    let sinces: Vec<Option<u64>> = r
        .fake
        .calls()
        .iter()
        .map(|(_, a)| a.get("since").and_then(Value::as_u64))
        .collect();
    assert_eq!(sinces[0], None, "the first read is the snapshot");
    assert_eq!(sinces[1], Some(18));
    assert_eq!(sinces[2], Some(36));
    // each read asks for a bounded chunk
    for (_, a) in r.fake.calls() {
        assert_eq!(a.get("max").and_then(Value::as_u64), Some(16 * 1024));
    }
    r.wait_active(0);
}

#[test]
fn a_stream_opened_with_a_session_ends_when_the_session_is_revoked() {
    let r = Running::start();
    script_tail(&r, vec![tail_reply("a", 1, true, false)]);
    let (session, csrf) = r.login();
    let mut sse = Sse::open_as(
        &r,
        "/api/stream?task=r1%2Fa",
        &[("X-Fleet-Session", &session)],
    );
    assert!(
        sse.read_until(Duration::from_secs(5), |b| b.contains("event: output")),
        "{}",
        sse.buf
    );
    let out = r.post_session("/api/logout", &session, &csrf, "{}");
    assert_eq!(out.status, 200);
    assert!(
        sse.read_until(Duration::from_secs(5), |b| b.contains("event: end")),
        "{}",
        sse.buf
    );
    assert!(sse.closed_within(Duration::from_secs(2)));
    let end = sse.events().pop().unwrap();
    assert_eq!(end.0, "end");
    assert!(end
        .1
        .get("reason")
        .and_then(Value::as_str)
        .unwrap()
        .contains("login has ended"));
    r.wait_active(0);
}

#[test]
fn a_stream_ends_when_its_session_expires() {
    let r = Running::with(WebLimits {
        auth: AuthLimits {
            session_lifetime: Duration::from_millis(600),
            ..quiet_auth()
        },
        ..fast_limits()
    });
    script_tail(&r, vec![tail_reply("a", 1, true, false)]);
    let (session, _) = r.login();
    let mut sse = Sse::open_as(
        &r,
        "/api/stream?task=r1%2Fa",
        &[("X-Fleet-Session", &session)],
    );
    assert!(
        sse.read_until(Duration::from_secs(5), |b| b.contains("event: end")),
        "{}",
        sse.buf
    );
    assert!(sse.closed_within(Duration::from_secs(2)));
}

#[test]
fn a_reconnecting_browser_resumes_from_last_event_id() {
    let r = Running::start();
    script_tail(
        &r,
        vec![
            tail_reply("", 500, true, false),
            tail_reply("more\n", 505, false, false),
        ],
    );
    let mut sse = Sse::open(
        &r,
        "/api/stream?task=r1%2Fa&since=3",
        &[("Last-Event-ID", "500")],
    );
    assert!(sse.read_until(Duration::from_secs(5), |b| b.contains("event: end")));
    let first = r.fake.calls()[0].1.clone();
    assert_eq!(
        first.get("since").and_then(Value::as_u64),
        Some(500),
        "the header wins over the URL"
    );
    // a malformed id is a client error
    let (k, v) = r.bearer();
    let resp = r.get(
        "/api/stream?task=r1%2Fa",
        &[(&k, &v), ("Last-Event-ID", "abc")],
    );
    assert_eq!(resp.status, 400);
}

#[test]
fn a_finished_task_streams_its_stored_output_once_and_not_again_on_reconnect() {
    let r = Running::start();
    script_tail(&r, vec![tail_reply("stored tail\n", 0, false, false)]);
    let mut sse = Sse::open(&r, "/api/stream?task=r1%2Fa", &[]);
    assert!(sse.read_until(Duration::from_secs(5), |b| b.contains("event: end")));
    assert_eq!(sse.events().len(), 2);
    script_tail(&r, vec![tail_reply("stored tail\n", 0, false, false)]);
    let mut again = Sse::open(&r, "/api/stream?task=r1%2Fa&since=11", &[]);
    assert!(again.read_until(Duration::from_secs(5), |b| b.contains("event: end")));
    let names: Vec<String> = again.events().into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, vec!["end"]);
}

#[test]
fn a_dropped_output_notice_is_forwarded() {
    let r = Running::start();
    script_tail(
        &r,
        vec![
            tail_reply("", 9000, true, true),
            tail_reply("x", 9001, false, false),
        ],
    );
    let mut sse = Sse::open(&r, "/api/stream?task=r1%2Fa", &[]);
    assert!(sse.read_until(Duration::from_secs(5), |b| b.contains("event: end")));
    assert_eq!(
        sse.events()[0].1.get("lost").and_then(Value::as_bool),
        Some(true)
    );
}

#[test]
fn streams_need_credentials_and_an_existing_task_and_report_errors_before_the_stream_starts() {
    let r = Running::start();
    assert_eq!(r.get("/api/stream?task=r1%2Fa", &[]).status, 401);
    assert_eq!(r.get_bearer("/api/stream").status, 400);
    r.fake
        .on(|_, _| Err(FleetError::not_found("there is no task r1/zz")));
    let resp = r.get_bearer("/api/stream?task=r1%2Fzz");
    assert_eq!(resp.status, 404);
    assert!(resp
        .header("content-type")
        .unwrap()
        .starts_with("application/json"));
    r.wait_active(0);
    // the failed attempt did not leak a stream slot
    script_tail(&r, vec![tail_reply("x", 1, false, false)]);
    let mut sse = Sse::open(&r, "/api/stream?task=r1%2Fa", &[]);
    assert!(sse.read_until(Duration::from_secs(5), |b| b.contains("event: end")));
}

#[test]
fn a_stream_ends_at_its_lifetime_and_at_its_byte_budget() {
    let r = Running::with(WebLimits {
        stream_lifetime: Duration::from_millis(400),
        ..fast_limits()
    });
    script_tail(&r, vec![tail_reply("a", 1, true, false)]);
    let mut sse = Sse::open(&r, "/api/stream?task=r1%2Fa", &[]);
    assert!(
        sse.read_until(Duration::from_secs(5), |b| b.contains("event: limit")),
        "{}",
        sse.buf
    );
    assert!(sse.closed_within(Duration::from_secs(2)));
    let last = sse.events().pop().unwrap();
    assert_eq!(last.0, "limit");
    assert_eq!(
        last.1.get("next").and_then(Value::as_u64),
        Some(1),
        "tells where to resume"
    );

    let r = Running::with(WebLimits {
        stream_bytes: 200,
        ..fast_limits()
    });
    let big = "x".repeat(300);
    script_tail(
        &r,
        vec![
            tail_reply(&big, 300, true, false),
            tail_reply(&big, 600, true, false),
        ],
    );
    let mut sse = Sse::open(&r, "/api/stream?task=r1%2Fa", &[]);
    assert!(sse.read_until(Duration::from_secs(5), |b| b.contains("event: limit")));
    assert!(sse.closed_within(Duration::from_secs(2)));
    let outputs = sse.events().iter().filter(|(n, _)| n == "output").count();
    assert_eq!(
        outputs, 1,
        "the byte budget stops the stream after the event that used it up"
    );
}

#[test]
fn the_number_of_open_streams_is_capped_and_a_closed_stream_frees_its_slot() {
    let r = Running::with(WebLimits {
        max_streams: 2,
        ..fast_limits()
    });
    script_tail(&r, vec![tail_reply("a", 1, true, false)]);
    let mut one = Sse::open(&r, "/api/stream?task=r1%2Fa", &[]);
    assert!(one.read_until(Duration::from_secs(5), |b| b.contains("event: output")));
    let mut two = Sse::open(&r, "/api/stream?task=r1%2Fa", &[]);
    assert!(two.read_until(Duration::from_secs(5), |b| b.contains("HTTP/1.1 200")));
    let third = r.get_bearer("/api/stream?task=r1%2Fa");
    assert_eq!(third.status, 503);
    assert_eq!(third.error_code(), "limit");
    // the client leaves without saying anything: the server notices and frees the slot
    drop(one);
    let start = Instant::now();
    loop {
        let mut again = Sse::open(&r, "/api/stream?task=r1%2Fa", &[]);
        if again.read_until(Duration::from_millis(300), |b| b.contains("HTTP/1.1 200")) {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "the slot was never freed"
        );
    }
    drop(two);
}

#[test]
fn a_quiet_stream_sends_keepalives_and_notices_a_vanished_client() {
    let r = Running::with(WebLimits {
        keepalive: Duration::from_millis(100),
        ..fast_limits()
    });
    script_tail(&r, vec![tail_reply("", 0, true, false)]);
    let mut sse = Sse::open(&r, "/api/stream?task=r1%2Fa", &[]);
    assert!(
        sse.read_until(Duration::from_secs(5), |b| b.contains(": keep-alive")),
        "{}",
        sse.buf
    );
    drop(sse);
    r.wait_active(0);
}

#[test]
fn a_backend_failure_ends_the_stream_with_a_reason() {
    let r = Running::start();
    let calls = AtomicUsize::new(0);
    r.fake.on(move |_, _| {
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(tail_reply("x", 1, true, false))
        } else {
            Err(FleetError::conflict("the daemon is shutting down"))
        }
    });
    let mut sse = Sse::open(&r, "/api/stream?task=r1%2Fa", &[]);
    assert!(sse.read_until(Duration::from_secs(5), |b| b.contains("event: end")));
    let end = sse.events().pop().unwrap();
    assert_eq!(
        end.1.get("reason").and_then(Value::as_str),
        Some("the daemon is shutting down")
    );
}

// ---------- shutdown ----------

#[test]
fn shutdown_closes_open_streams_and_stops_listening() {
    let mut r = Running::start();
    script_tail(&r, vec![tail_reply("a", 1, true, false)]);
    let mut sse = Sse::open(&r, "/api/stream?task=r1%2Fa", &[]);
    assert!(sse.read_until(Duration::from_secs(5), |b| b.contains("event: output")));
    let started = Instant::now();
    r.stop.store(true, Ordering::SeqCst);
    r.thread.take().unwrap().join().unwrap();
    // in the daemon the server is owned by its thread and is gone now: drop our handle as well
    r.server = None;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "serve returned after {:?}",
        started.elapsed()
    );
    assert!(
        sse.closed_within(Duration::from_secs(2)),
        "the open stream was closed"
    );
    assert!(
        TcpStream::connect_timeout(&r.addr, Duration::from_millis(500)).is_err(),
        "nothing listens any more"
    );
    assert_eq!(r.active(), 0);
}
