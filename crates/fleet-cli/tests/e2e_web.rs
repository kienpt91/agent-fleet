//! End-to-end tests of the web dashboard: the real `fleet daemon --web` binary, scripted worker
//! processes and a small raw-TCP HTTP client (no curl, no crates, one request per connection).
//!
//! The web token only ever exists on the daemon's stderr, which these tests drain into memory the
//! way an operator's terminal would show it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod common;

use common::*;
use fleet_domain::value::Value;
use fleet_infra::testutil::{wait_until, TempDir};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}
/// The header that carries a dashboard session id.
const SESSION: &str = "X-Fleet-Session";

fn has_hex64_run(text: &str) -> bool {
    let mut run = 0usize;
    for c in text.chars() {
        run = if c.is_ascii_hexdigit() { run + 1 } else { 0 };
        if run >= 64 {
            return true;
        }
    }
    false
}

fn has_bytes(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

// ---- a tiny HTTP/1.1 client ------------------------------------------------------------------------

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> Value {
        parse_json(&self.text())
    }

    /// `error.code` of a `fleet.error/1` document.
    fn code(&self) -> String {
        let doc = self.json();
        assert_eq!(jstr(&doc, "schema"), "fleet.error/1", "{}", self.text());
        jstr(doc.get("error").unwrap(), "code").to_string()
    }

    /// `error.subject` of a `fleet.error/1` document.
    fn subject(&self) -> String {
        let doc = self.json();
        jstr(doc.get("error").unwrap(), "subject").to_string()
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn connect(port: u16) -> TcpStream {
    let s = TcpStream::connect(("127.0.0.1", port)).expect("connect to the dashboard");
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(30))).unwrap();
    s
}

fn request_bytes(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> String {
    let has = |name: &str| headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
    let mut req = format!("{method} {path} HTTP/1.1\r\n");
    if !has("host") {
        req.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(b) = body {
        if !has("content-type") {
            req.push_str("Content-Type: application/json\r\n");
        }
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("Connection: close\r\n\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }
    req
}

fn parse_response(raw: &[u8]) -> Resp {
    let end = find(raw, b"\r\n\r\n").unwrap_or_else(|| {
        panic!(
            "no complete response head: {:?}",
            String::from_utf8_lossy(raw)
        )
    });
    let head = String::from_utf8_lossy(&raw[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap();
    let status: u16 = status_line
        .strip_prefix("HTTP/1.1 ")
        .and_then(|r| r.split(' ').next())
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("bad status line {status_line:?}"));
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Resp {
        status,
        headers,
        body: raw[end + 4..].to_vec(),
    }
}

/// One request on its own connection; reads the response to the end of the stream.
fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&str>) -> Resp {
    let mut s = connect(port);
    s.write_all(request_bytes(port, method, path, headers, body).as_bytes())
        .unwrap();
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            // a signal (a child process ended) interrupts the read: not the end of the response
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    parse_response(&raw)
}

/// A dashboard reached with the bearer token.
struct Web {
    port: u16,
    token: String,
}

impl Web {
    fn new(info: &WebInfo) -> Web {
        Web {
            port: info.port,
            token: info.token.clone(),
        }
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }

    fn get(&self, path: &str) -> Resp {
        http(
            self.port,
            "GET",
            path,
            &[("Authorization", &self.bearer())],
            None,
        )
    }

    fn post(&self, path: &str, body: &Value) -> Resp {
        http(
            self.port,
            "POST",
            path,
            &[("Authorization", &self.bearer())],
            Some(&body.canonical()),
        )
    }

    fn post_raw(&self, path: &str, body: &str) -> Resp {
        http(
            self.port,
            "POST",
            path,
            &[("Authorization", &self.bearer())],
            Some(body),
        )
    }

    fn status(&self) -> Value {
        let r = self.get("/api/status");
        assert_eq!(r.status, 200, "{}", r.text());
        r.json()
    }

    /// Polls `GET /api/status` until `pred` holds.
    fn wait(&self, what: &str, mut pred: impl FnMut(&Value) -> bool) -> Value {
        let start = Instant::now();
        loop {
            let st = self.status();
            if pred(&st) {
                return st;
            }
            if start.elapsed() > Duration::from_millis(WAIT_MS) {
                panic!("timed out waiting for {what}: {}", st.canonical());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_task(&self, task: &str, wanted: &str) -> Value {
        self.wait_task_in(task, &[wanted])
    }

    fn wait_task_in(&self, task: &str, wanted: &[&str]) -> Value {
        let st = self.wait(&format!("{task} to be one of {wanted:?}"), |st| {
            find_task(st, "r1", task).is_some_and(|t| wanted.contains(&jstr(t, "status")))
        });
        find_task(&st, "r1", task).unwrap().clone()
    }
}

fn approval<'a>(st: &'a Value, id: &str) -> Option<&'a Value> {
    jarr(st, "approvals").iter().find(|a| jstr(a, "id") == id)
}

// ---- server-sent events ----------------------------------------------------------------------------

enum Fill {
    Data,
    Idle,
    Closed,
}

#[derive(Debug)]
struct Event {
    name: String,
    data: String,
}

struct Sse {
    stream: TcpStream,
    buf: Vec<u8>,
    /// Every byte received after the response head.
    body_raw: Vec<u8>,
    head: String,
}

impl Sse {
    fn open(port: u16, path: &str, token: &str) -> Sse {
        let bearer = format!("Bearer {token}");
        Sse::open_with(port, path, &[("Authorization", bearer.as_str())])
    }

    /// Opens a stream with the given credential headers.
    fn open_with(port: u16, path: &str, creds: &[(&str, &str)]) -> Sse {
        let mut s = connect(port);
        let mut headers = vec![("Accept", "text/event-stream")];
        headers.extend_from_slice(creds);
        s.write_all(request_bytes(port, "GET", path, &headers, None).as_bytes())
            .unwrap();
        s.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut sse = Sse {
            stream: s,
            buf: Vec::new(),
            body_raw: Vec::new(),
            head: String::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(i) = find(&sse.buf, b"\r\n\r\n") {
                sse.head = String::from_utf8_lossy(&sse.buf[..i]).into_owned();
                sse.buf.drain(..i + 4);
                sse.body_raw = sse.buf.clone();
                break;
            }
            match sse.fill() {
                Fill::Closed => panic!("the stream closed before its head: {:?}", sse.buf),
                Fill::Idle if Instant::now() > deadline => panic!("no response head"),
                _ => {}
            }
        }
        assert!(sse.head.starts_with("HTTP/1.1 200"), "{}", sse.head);
        assert!(
            sse.head.to_ascii_lowercase().contains("text/event-stream"),
            "{}",
            sse.head
        );
        sse
    }

    fn fill(&mut self) -> Fill {
        let mut chunk = [0u8; 4096];
        match self.stream.read(&mut chunk) {
            Ok(0) => Fill::Closed,
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                if !self.head.is_empty() {
                    self.body_raw.extend_from_slice(&chunk[..n]);
                }
                Fill::Data
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                Fill::Idle
            }
            Err(_) => Fill::Closed,
        }
    }

    /// The next event; `None` when the server closed the connection. Comment lines (keep-alives)
    /// are skipped. Panics when nothing arrives within `within`.
    fn next(&mut self, within: Duration) -> Option<Event> {
        let deadline = Instant::now() + within;
        loop {
            while let Some(i) = find(&self.buf, b"\n\n") {
                let block: Vec<u8> = self.buf.drain(..i + 2).collect();
                let text = String::from_utf8_lossy(&block[..i]).into_owned();
                if text.lines().all(|l| l.starts_with(':')) {
                    continue;
                }
                let mut name = String::new();
                let mut data = String::new();
                for l in text.lines() {
                    if let Some(v) = l.strip_prefix("event: ") {
                        name = v.to_string();
                    } else if let Some(v) = l.strip_prefix("data: ") {
                        data = v.to_string();
                    }
                }
                return Some(Event { name, data });
            }
            match self.fill() {
                Fill::Closed => return None,
                Fill::Idle if Instant::now() > deadline => {
                    panic!(
                        "no event within {within:?}; unparsed bytes: {:?}",
                        String::from_utf8_lossy(&self.buf)
                    )
                }
                _ => {}
            }
        }
    }

    /// Collects the text of `output` events until `pred` holds for it. Panics when the stream
    /// ends first.
    fn output_until(&mut self, within: Duration, pred: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + within;
        let mut text = String::new();
        while !pred(&text) {
            let left = deadline.saturating_duration_since(Instant::now());
            let ev = self
                .next(left.max(Duration::from_millis(1)))
                .unwrap_or_else(|| panic!("the stream closed; output so far: {text:?}"));
            assert_eq!(
                ev.name, "output",
                "unexpected event {ev:?}; output {text:?}"
            );
            let doc = parse_json(&ev.data);
            text.push_str(jstr(&doc, "text"));
        }
        text
    }

    /// Reads every remaining event until the server closes the connection.
    fn drain_to_close(&mut self, within: Duration) -> Vec<Event> {
        let deadline = Instant::now() + within;
        let mut out = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.next(left.max(Duration::from_millis(1))) {
                Some(ev) => out.push(ev),
                None => return out,
            }
        }
    }
}

// ---- worker script helpers -----------------------------------------------------------------------

/// A fleet with one worker script, started with the dashboard.
fn web_fleet(body: &str) -> (Fleet, WebInfo) {
    let mut f = Fleet::new();
    let s = f.script("w.sh", body);
    f.write_cfg(&simple_cfg(&s, ""));
    let info = f.start_web(&[]);
    (f, info)
}

// ---- 1. login ------------------------------------------------------------------------------------------

#[test]
fn login_flow() {
    let mut f = Fleet::new();
    let info = f.start_web(&[]);
    let web = Web::new(&info);
    let port = web.port;
    let origin = format!("http://127.0.0.1:{port}");
    let bearer = web.bearer();
    assert!(is_hex64(&info.token), "{}", info.token.len());
    assert!(
        info.stderr_lines().iter().all(|l| !l.contains("WARNING")),
        "a loopback dashboard printed a warning"
    );

    // the page itself is public, the API is not
    let page = http(port, "GET", "/", &[], None);
    assert_eq!(page.status, 200);
    assert!(page
        .header("content-type")
        .is_some_and(|c| c.starts_with("text/html")));
    let nothing = http(port, "GET", "/api/status", &[], None);
    assert_eq!(nothing.status, 401, "{}", nothing.text());
    assert_eq!(nothing.code(), "forbidden");

    // bearer
    let ok = web.get("/api/status");
    assert_eq!(ok.status, 200, "{}", ok.text());
    assert_eq!(jstr(&ok.json(), "schema"), "fleet.status/1");

    // a wrong Host is refused before anything else, credentials or not
    for host in ["evil.example", "evil.example:80", "127.0.0.1:1"] {
        let r = http(
            port,
            "GET",
            "/api/status",
            &[("Host", host), ("Authorization", &bearer)],
            None,
        );
        assert_eq!(r.status, 421, "Host {host}: {}", r.text());
    }
    let r = http(port, "GET", "/", &[("Host", "evil.example")], None);
    assert_eq!(r.status, 421);

    // the token is exchanged for a session
    let login_body = Value::object([("token", Value::str(&info.token))]).canonical();
    let login = http(
        port,
        "POST",
        "/api/login",
        &[("Origin", &origin)],
        Some(&login_body),
    );
    assert_eq!(login.status, 200, "{}", login.text());
    assert!(
        login.all("set-cookie").is_empty(),
        "login set a cookie: {:?}",
        login.headers
    );
    let session = login.json();
    assert_eq!(jstr(&session, "schema"), "fleet.web.session/1");
    let session_id = jstr(&session, "session").to_string();
    assert!(!session_id.is_empty());
    assert_ne!(session_id, info.token);
    let csrf = jstr(&session, "csrf").to_string();
    assert!(!csrf.is_empty() && csrf != session_id && csrf != info.token);
    assert!(!login.text().contains(&info.token));

    // the session travels in a header and works for reads
    let by_session = http(port, "GET", "/api/status", &[(SESSION, &session_id)], None);
    assert_eq!(by_session.status, 200, "{}", by_session.text());
    assert_eq!(jstr(&by_session.json(), "schema"), "fleet.status/1");
    // the id is a credential and is never echoed again; the CSRF token is
    let sess = http(port, "GET", "/api/session", &[(SESSION, &session_id)], None);
    assert_eq!(sess.status, 200, "{}", sess.text());
    assert!(sess.json().get("session").is_some_and(Value::is_null));
    assert_eq!(jstr(&sess.json(), "csrf"), csrf);
    assert!(!sess.text().contains(&session_id));

    // a cookie is no credential at all: a browser sends cookies to every server on the host,
    // whatever the port, so the dashboard never reads one
    let cookie = format!("fleet_session={session_id}");
    let ask = Value::object([("id", Value::str("g99")), ("answer", Value::str("x"))]).canonical();
    let by_cookie = http(port, "GET", "/api/status", &[("Cookie", &cookie)], None);
    assert_eq!(by_cookie.status, 401, "{}", by_cookie.text());
    assert_eq!(by_cookie.code(), "forbidden");
    let cookie_post = http(
        port,
        "POST",
        "/api/gate/resolve",
        &[
            ("Cookie", &cookie),
            ("Origin", &origin),
            ("X-CSRF-Token", &csrf),
        ],
        Some(&ask),
    );
    assert_eq!(cookie_post.status, 401, "{}", cookie_post.text());
    // not even next to a valid session header
    let both = http(
        port,
        "GET",
        "/api/status",
        &[("Cookie", "fleet_session=nonsense"), (SESSION, &session_id)],
        None,
    );
    assert_eq!(both.status, 200, "{}", both.text());

    // a session POST needs the CSRF token and the dashboard's own Origin
    let no_csrf = http(
        port,
        "POST",
        "/api/gate/resolve",
        &[(SESSION, &session_id), ("Origin", &origin)],
        Some(&ask),
    );
    assert_eq!(no_csrf.status, 403, "{}", no_csrf.text());
    assert_eq!(no_csrf.code(), "forbidden");
    let wrong_csrf = http(
        port,
        "POST",
        "/api/gate/resolve",
        &[
            (SESSION, &session_id),
            ("Origin", &origin),
            ("X-CSRF-Token", &"0".repeat(64)),
        ],
        Some(&ask),
    );
    assert_eq!(wrong_csrf.status, 403, "{}", wrong_csrf.text());
    let no_origin = http(
        port,
        "POST",
        "/api/gate/resolve",
        &[(SESSION, &session_id), ("X-CSRF-Token", &csrf)],
        Some(&ask),
    );
    assert_eq!(no_origin.status, 403, "{}", no_origin.text());
    let accepted = http(
        port,
        "POST",
        "/api/gate/resolve",
        &[
            (SESSION, &session_id),
            ("Origin", &origin),
            ("X-CSRF-Token", &csrf),
        ],
        Some(&ask),
    );
    // the request got through every browser-facing check and reached the daemon, which has no
    // such question
    assert_eq!(accepted.status, 404, "{}", accepted.text());
    assert_eq!(accepted.code(), "not_found");

    // other origins and other sites
    let evil = "http://evil.example";
    let r = http(
        port,
        "POST",
        "/api/login",
        &[("Origin", evil)],
        Some(&login_body),
    );
    assert_eq!(r.status, 403, "{}", r.text());
    assert!(r.all("set-cookie").is_empty());
    let r = http(
        port,
        "POST",
        "/api/gate/resolve",
        &[("Authorization", &bearer), ("Origin", evil)],
        Some(&ask),
    );
    assert_eq!(r.status, 403, "{}", r.text());
    for site in ["cross-site", "same-site"] {
        let r = http(
            port,
            "GET",
            "/api/status",
            &[("Authorization", &bearer), ("Sec-Fetch-Site", site)],
            None,
        );
        assert_eq!(r.status, 403, "Sec-Fetch-Site {site}: {}", r.text());
    }
    let r = http(
        port,
        "GET",
        "/api/status",
        &[
            ("Authorization", &bearer),
            ("Sec-Fetch-Site", "same-origin"),
        ],
        None,
    );
    assert_eq!(r.status, 200, "{}", r.text());
    // a body that is not JSON is not accepted either
    let r = http(
        port,
        "POST",
        "/api/gate/resolve",
        &[("Authorization", &bearer), ("Content-Type", "text/plain")],
        Some(&ask),
    );
    assert_eq!(r.status, 415, "{}", r.text());

    // wrong credentials
    let r = http(
        port,
        "POST",
        "/api/login",
        &[("Origin", &origin)],
        Some(&Value::object([("token", Value::str("0".repeat(64)))]).canonical()),
    );
    assert_eq!(r.status, 401, "{}", r.text());
    assert!(r.all("set-cookie").is_empty());

    // logout ends the session: it stops working, and nothing is set in the browser's cookie jar
    let out = http(
        port,
        "POST",
        "/api/logout",
        &[
            (SESSION, &session_id),
            ("Origin", &origin),
            ("X-CSRF-Token", &csrf),
        ],
        Some("{}"),
    );
    assert_eq!(out.status, 200, "{}", out.text());
    assert!(out.all("set-cookie").is_empty(), "{:?}", out.headers);
    let after = http(port, "GET", "/api/status", &[(SESSION, &session_id)], None);
    assert_eq!(after.status, 401, "{}", after.text());
    // an unknown or ended session is refused without costing the token holder anything
    for _ in 0..8 {
        let r = http(port, "GET", "/api/status", &[(SESSION, &session_id)], None);
        assert_eq!(r.status, 401, "{}", r.text());
    }
    // the token still works
    assert_eq!(web.get("/api/status").status, 200);
    // and no response of this test ever set a cookie
    assert!(http(port, "GET", "/", &[], None)
        .all("set-cookie")
        .is_empty());
}

// ---- 2. questions, approvals and merges over HTTP ------------------------------------------------------

#[test]
fn full_decision_flow_over_http() {
    let body = "case \"$FLEET_TASK\" in\n  q)\n    if grep -q 'Operator says: ' \"$FLEET_BRIEF_FILE\"; then echo answered > q.txt; exit 0; fi\n    printf '{\"version\":1,\"kind\":\"question\",\"text\":\"which?\",\"choices\":[\"a\",\"b\"]}' > .fleet/question.json ;;\n  *) echo \"$FLEET_TASK\" > \"$FLEET_TASK.txt\" ;;\nesac\n";
    let (f, info) = web_fleet(body);
    let web = Web::new(&info);
    f.plan_and_start("r1", 3, &[t("q"), t("d1"), t("d2")]);

    // the question shows up in the status ...
    let st = web.wait("the question", |st| !jarr(st, "gates").is_empty());
    let gate = &jarr(&st, "gates")[0];
    let gate_id = jstr(gate, "id").to_string();
    assert_eq!(jstr(gate, "text"), "which?");
    assert_eq!(jstr(gate, "task"), "q");
    assert_eq!(jarr(gate, "choices").len(), 2);
    assert_eq!(
        jstr(find_task(&st, "r1", "q").unwrap(), "status"),
        "blocked"
    );
    // ... a malformed or unknown answer is refused ...
    let r = web.post(
        "/api/gate/resolve",
        &Value::object([("id", Value::str("g999")), ("answer", Value::str("a"))]),
    );
    assert_eq!(r.status, 404, "{}", r.text());
    let r = web.post_raw("/api/gate/resolve", "{\"id\":\"g1\"}");
    assert_eq!(r.status, 400, "{}", r.text());
    // ... and the answer unblocks the task
    let r = web.post(
        "/api/gate/resolve",
        &Value::object([("id", Value::str(&gate_id)), ("answer", Value::str("a"))]),
    );
    assert_eq!(r.status, 200, "{}", r.text());
    web.wait_task("q", "completed");
    web.wait_task("d1", "completed");
    web.wait_task("d2", "completed");
    assert_eq!(
        fs::read_to_string(f.worktree("r1", "q").join("q.txt"))
            .unwrap()
            .trim(),
        "answered"
    );

    // a merge needs an approval: the first request is refused and names it
    let merge = |task: &str| web.post("/api/merge", &Value::object([("task", Value::str(task))]));
    let first = merge("d1");
    assert_eq!(first.status, 409, "{}", first.text());
    assert_eq!(first.code(), "approval_required");
    let id1 = first.subject();
    assert!(!id1.is_empty());
    assert!(
        git_branches(&f).iter().all(|b| b != "fleet/r1/integration"),
        "the merge went ahead"
    );

    // the status shows exactly what would be merged, bound to a digest
    let sha = safe_git(&f.repo, &["rev-parse", "fleet/r1/d1"]);
    let st = web.status();
    let shown = approval(&st, &id1).unwrap_or_else(|| panic!("no approval {id1}"));
    assert_eq!(jstr(shown, "status"), "pending");
    assert_eq!(jstr(shown, "purpose"), "merge");
    assert_eq!(jstr(shown, "task"), "d1");
    assert_eq!(
        jstr(shown, "display"),
        format!("git: \"merge\" \"fleet/r1/integration\" \"fleet/r1/d1\" \"{sha}\"")
    );
    let digest = jstr(shown, "digest").to_string();
    assert!(is_hex64(&digest), "{digest}");
    let cli = f.approvals();
    assert_eq!(jstr(&cli[0], "digest"), digest, "the CLI disagrees");
    assert_eq!(jstr(&cli[0], "display"), jstr(shown, "display"));
    // asking again names the same approval instead of opening another
    let again = merge("d1");
    assert_eq!(
        (again.status, again.code()),
        (409, "approval_required".into())
    );
    assert_eq!(again.subject(), id1);
    assert_eq!(f.approvals().len(), 1);

    let decide = |id: &str, approve: bool, digest: Option<&str>| {
        let mut pairs = vec![("id", Value::str(id)), ("approve", Value::Bool(approve))];
        if let Some(d) = digest {
            pairs.push(("digest", Value::str(d)));
        }
        web.post("/api/approval/decide", &Value::object(pairs))
    };
    // a digest that is not the approval's own is refused and nothing changes
    let wrong = decide(&id1, true, Some(&"0".repeat(64)));
    assert_eq!(wrong.status, 400, "{}", wrong.text());
    assert_eq!(wrong.code(), "invalid");
    let flipped = format!(
        "{}{}",
        &digest[..63],
        if digest.ends_with('0') { '1' } else { '0' }
    );
    let near = decide(&id1, true, Some(&flipped));
    assert_eq!(near.status, 400, "{}", near.text());
    // a digest is mandatory, and must look like one
    let missing = decide(&id1, true, None);
    assert_eq!(missing.status, 400, "{}", missing.text());
    let short = decide(&id1, true, Some("abc123"));
    assert_eq!(short.status, 400, "{}", short.text());
    let st = web.status();
    assert_eq!(jstr(approval(&st, &id1).unwrap(), "status"), "pending");
    assert_eq!(merge("d1").status, 409, "merged without an approval");

    // the right digest approves, and the merge then goes through exactly once
    let ok = decide(&id1, true, Some(&digest));
    assert_eq!(ok.status, 200, "{}", ok.text());
    assert_eq!(
        jstr(approval(&web.status(), &id1).unwrap(), "status"),
        "approved"
    );
    let merged = merge("d1");
    assert_eq!(merged.status, 200, "{}", merged.text());
    assert_eq!(jstr(&merged.json(), "schema"), "fleet.merge/1");
    let log = safe_git(&f.repo, &["log", "--format=%s", "fleet/r1/integration"]);
    assert!(
        log.contains("fleet: merge d1 into fleet/r1/integration"),
        "{log}"
    );
    let tree = safe_git(
        &f.repo,
        &["ls-tree", "-r", "--name-only", "fleet/r1/integration"],
    );
    assert!(tree.lines().any(|l| l == "d1.txt"), "{tree}");
    assert!(!tree.lines().any(|l| l == "d2.txt"), "{tree}");
    assert_eq!(
        jstr(approval(&web.status(), &id1).unwrap(), "status"),
        "consumed"
    );
    assert_eq!(
        decide(&id1, true, Some(&digest)).status,
        409,
        "an approval was used twice"
    );

    // the other task: a denial keeps the merge refused
    let second = merge("d2");
    assert_eq!(second.status, 409, "{}", second.text());
    assert_eq!(second.code(), "approval_required");
    let id2 = second.subject();
    assert_ne!(id2, id1);
    let st = web.status();
    let digest2 = jstr(approval(&st, &id2).unwrap(), "digest").to_string();
    assert!(is_hex64(&digest2) && digest2 != digest);
    // the first approval's digest does not open the second
    assert_eq!(decide(&id2, false, Some(&digest)).status, 400);
    let denied = decide(&id2, false, Some(&digest2));
    assert_eq!(denied.status, 200, "{}", denied.text());
    assert_eq!(
        jstr(approval(&web.status(), &id2).unwrap(), "status"),
        "denied"
    );
    let after = merge("d2");
    assert_eq!(after.status, 409, "{}", after.text());
    assert_eq!(after.code(), "approval_required");
    assert_ne!(after.subject(), id2, "a denied approval was reused");
    let tree = safe_git(
        &f.repo,
        &["ls-tree", "-r", "--name-only", "fleet/r1/integration"],
    );
    assert!(!tree.lines().any(|l| l == "d2.txt"), "{tree}");
    assert!(jstr(&f.task("r1", "d2"), "merged").is_empty());
    assert!(!jstr(&f.task("r1", "d1"), "merged").is_empty());
}

fn git_branches(f: &Fleet) -> Vec<String> {
    safe_git(&f.repo, &["branch", "--format=%(refname:short)"])
        .lines()
        .map(|l| l.trim().to_string())
        .collect()
}

// ---- 3. live output -------------------------------------------------------------------------------------------

#[test]
fn live_output_sse_and_send_input() {
    let (f, info) = web_fleet(
        "echo first-line\nread line\nprintf '%s' \"$line\" > got.txt\necho \"got:$line\"\n",
    );
    let web = Web::new(&info);
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_live("r1", "t");

    let mut sse = Sse::open(web.port, "/api/stream?task=r1/t", &web.token);
    let first = sse.output_until(Duration::from_secs(15), |t| t.contains("first-line"));
    assert!(!first.contains("got:"), "{first:?}");

    // the stream works with a session as well, and ends when that session does
    let origin = format!("http://127.0.0.1:{}", web.port);
    let login = http(
        web.port,
        "POST",
        "/api/login",
        &[("Origin", &origin)],
        Some(&Value::object([("token", Value::str(&web.token))]).canonical()),
    );
    assert_eq!(login.status, 200, "{}", login.text());
    let session_id = jstr(&login.json(), "session").to_string();
    let csrf = jstr(&login.json(), "csrf").to_string();
    let mut by_session = Sse::open_with(
        web.port,
        "/api/stream?task=r1/t",
        &[(SESSION, session_id.as_str())],
    );
    by_session.output_until(Duration::from_secs(15), |t| t.contains("first-line"));
    let out = http(
        web.port,
        "POST",
        "/api/logout",
        &[
            (SESSION, session_id.as_str()),
            ("Origin", &origin),
            ("X-CSRF-Token", &csrf),
        ],
        Some("{}"),
    );
    assert_eq!(out.status, 200, "{}", out.text());
    let rest = by_session.drain_to_close(Duration::from_secs(10));
    let last = rest.last().expect("the stream ended without an event");
    assert_eq!(last.name, "end", "{rest:?}");
    let reason = parse_json(&last.data);
    assert!(
        jstr(&reason, "reason").contains("login has ended"),
        "{}",
        last.data
    );
    // the worker is still running: the stream ended because of the logout, not the task
    assert!(jbool(&f.task("r1", "t"), "live"));
    // and the session no longer opens one
    let again = http(
        web.port,
        "GET",
        "/api/stream?task=r1/t",
        &[(SESSION, session_id.as_str())],
        None,
    );
    assert_eq!(again.status, 401, "{}", again.text());

    // typing is text: escape sequences, carriage returns and invisible characters are refused
    // and nothing of them is sent
    for (what, text) in [
        ("ESC", "a\u{1b}[31mred"),
        ("CR", "one\rtwo"),
        ("bidi", "evil\u{202e}text"),
        ("BEL", "ding\u{7}"),
    ] {
        let r = web.post(
            "/api/send",
            &Value::object([
                ("task", Value::str("r1/t")),
                ("text", Value::str(text)),
                ("newline", Value::Bool(true)),
            ]),
        );
        assert_eq!(r.status, 400, "{what}: {}", r.text());
        assert_eq!(r.code(), "invalid", "{what}");
    }
    assert!(jbool(&f.task("r1", "t"), "live"), "the worker took input");
    // input typed in the browser reaches the worker; the markup in it stays text
    let sent = web.post(
        "/api/send",
        &Value::object([
            ("task", Value::str("r1/t")),
            ("text", Value::str("hello <b>x</b>")),
            ("newline", Value::Bool(true)),
        ]),
    );
    assert_eq!(sent.status, 200, "{}", sent.text());
    let output = sse.output_until(Duration::from_secs(15), |t| {
        t.contains("got:hello <b>x</b>")
    });
    assert!(output.contains("got:hello <b>x</b>"), "{output:?}");

    // the worker ends: an `end` event, then the server closes the connection
    let rest = sse.drain_to_close(Duration::from_secs(15));
    let last = rest.last().expect("no event after the output");
    assert_eq!(last.name, "end", "{rest:?}");
    for ev in &rest[..rest.len() - 1] {
        assert_eq!(ev.name, "output", "{rest:?}");
    }
    // the stream never carries markup: `<`, `>` and `&` are JSON escapes
    for b in [b'<', b'>', b'&'] {
        assert!(
            !sse.body_raw.contains(&b),
            "raw {:?} in the stream: {:?}",
            b as char,
            String::from_utf8_lossy(&sse.body_raw)
        );
    }
    assert!(has_bytes(
        &sse.body_raw,
        b"\\u003cb\\u003ex\\u003c/b\\u003e"
    ));

    // the stored output is the same text
    f.wait_task("r1", "t", &["completed"]);
    // the worker received exactly the one legitimate line: none of the refused text got to it
    assert_eq!(
        fs::read_to_string(f.worktree("r1", "t").join("got.txt")).unwrap(),
        "hello <b>x</b>"
    );
    let tail = web.get("/api/tail?task=r1/t");
    assert_eq!(tail.status, 200, "{}", tail.text());
    let doc = tail.json();
    assert_eq!(jstr(&doc, "schema"), "fleet.tail/1");
    let text = jstr(&doc, "text");
    assert!(text.contains("first-line"), "{text:?}");
    assert!(text.contains("got:hello <b>x</b>"), "{text:?}");
    assert!(!tail.body.contains(&b'<') && !tail.body.contains(&b'>'));
    // an unknown task is an error document, not a crash
    assert_eq!(web.get("/api/tail?task=r1/nosuch").status, 404);
    assert_eq!(web.get("/api/tail").status, 400);
}

// ---- 4. the token stays out of workers and files -----------------------------------------------------------

#[test]
fn web_token_never_reaches_workers_or_disk() {
    let body = r#"{
  echo "== env"; env
  echo "== self cmdline"; tr '\000' ' ' < /proc/$$/cmdline; echo
  echo "== child cmdline"; cat /proc/self/cmdline | tr '\000' ' '; echo
  echo "== parent cmdline"; tr '\000' ' ' < /proc/$PPID/cmdline 2>/dev/null; echo
  echo "== parent environ"; tr '\000' '\n' < /proc/$PPID/environ 2>/dev/null
  echo "== fds"; ls -l /proc/$$/fd 2>/dev/null
  echo "== pwd"; pwd
  echo "== ls"; ls -la
  echo "== find worktree"; find "$FLEET_WORKTREE" 2>/dev/null
  echo "== find fleet dir"; find "$FLEET_WORKTREE/.fleet" 2>/dev/null
  echo "== brief"; cat "$FLEET_BRIEF_FILE"
} > dump.txt
cat dump.txt
"#;
    let (f, info) = web_fleet(body);
    let web = Web::new(&info);
    let token = info.token.clone();
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_task("r1", "t", &["completed"]);
    let daemon = f.daemon_pid();

    // the test means something: the worker really dumped what it can see, and it can see the
    // daemon's own command line
    let wt = f.worktree("r1", "t");
    let dump = fs::read_to_string(wt.join("dump.txt")).unwrap();
    for part in [
        "FLEET_TASK=t",
        "== find fleet dir",
        "== ls",
        "dump.txt",
        "--web 127.0.0.1:0",
    ] {
        assert!(dump.contains(part), "{part} missing from the dump:\n{dump}");
    }
    assert!(has_bytes(
        &fs::read(format!("/proc/{daemon}/cmdline")).unwrap(),
        b"--web"
    ));

    // the token was shown once, on the operator's terminal
    let shown = info
        .stderr_lines()
        .iter()
        .filter(|l| l.contains(&token))
        .count();
    assert_eq!(shown, 1, "{:?}", info.stderr_lines().len());
    assert_eq!(web.get("/api/status").status, 200);

    // and nowhere else
    assert!(!dump.contains(&token), "the worker saw the token");
    assert_eq!(tree_contains(&wt, &token), None, "the worktree has it");
    assert_eq!(
        tree_contains(&f.state, &token),
        None,
        "the state directory has it"
    );
    assert_eq!(
        tree_contains(&f.state.join("logs"), &token),
        None,
        "the output logs have it"
    );
    assert!(!f.events_raw().contains(&token), "the event log has it");
    assert_eq!(
        tree_contains(&f.repo, &token),
        None,
        "the repository has it"
    );
    let committed = safe_git(&f.repo, &["log", "-p", "--all", "--format=%H %s"]);
    assert!(committed.contains("dump.txt"), "the dump was not committed");
    assert!(!committed.contains(&token), "a commit has it");
    assert!(
        !has_bytes(
            &fs::read(format!("/proc/{daemon}/cmdline")).unwrap(),
            token.as_bytes()
        ),
        "the daemon's argv has it"
    );
    assert!(
        !has_bytes(
            &fs::read(format!("/proc/{daemon}/environ")).unwrap(),
            token.as_bytes()
        ),
        "the daemon's environment has it"
    );
    assert!(
        tree_contains(&f.tmp.path().join("h"), &token).is_none(),
        "the home directory has it"
    );
    // the web is not in the CLI's output either
    for args in [vec!["status", "--json"], vec!["events", "--json"]] {
        let out = f.ok(&args);
        assert!(!out.stdout.contains(&token) && !out.stderr.contains(&token));
    }
}

// ---- 5. a worker cannot use the dashboard, even with the token --------------------------------------------

const BASH_REQ: &str = r#"
req() {
  local method=$1 path=$2 body=$3 out=$4 line
  echo "no reply" > "$out"
  exec 3<>/dev/tcp/127.0.0.1/$PORT || return 1
  if [ -n "$body" ]; then
    printf '%s %s HTTP/1.1\r\nHost: 127.0.0.1:%s\r\nAuthorization: Bearer %s\r\nContent-Type: application/json\r\nContent-Length: %s\r\nConnection: close\r\n\r\n%s' "$method" "$path" "$PORT" "$TOKEN" "${#body}" "$body" >&3
  else
    printf '%s %s HTTP/1.1\r\nHost: 127.0.0.1:%s\r\nAuthorization: Bearer %s\r\nConnection: close\r\n\r\n' "$method" "$path" "$PORT" "$TOKEN" >&3
  fi
  while IFS= read -r -u 3 line || [ -n "$line" ]; do printf '%s\n' "$line"; done > "$out"
  exec 3<&-
}
"#;

fn assert_refused_as_worker(dir: &Path, file: &str) {
    let text = fs::read_to_string(dir.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
    assert!(
        text.starts_with("HTTP/1.1 403"),
        "{file}: the worker was not refused:\n{text}"
    );
    assert!(text.contains("\"code\":\"forbidden\""), "{file}: {text}");
    assert!(text.contains("worker processes"), "{file}: {text}");
    for secret in [
        "fleet.status/1",
        "fleet.merge/1",
        "fleet.web.session/1",
        "csrf",
        "\"approvals\"",
        "\"runs\"",
    ] {
        assert!(!text.contains(secret), "{file} got {secret}:\n{text}");
    }
}

#[test]
fn a_worker_cannot_use_the_web_ui_even_with_the_token() {
    let mut f = Fleet::new();
    let out = fleet_infra::testutil::TempDir::new("o");
    // the child that makes the request is a separate process started by the worker
    let child = f.tmp.script(
        "webreq.sh",
        &format!("#!/bin/bash\n{BASH_REQ}\nPORT=$1\nTOKEN=$2\nreq \"$3\" \"$4\" \"$5\" \"$6\"\n"),
    );
    let worker = fill(
        &format!(
            r#"#!/bin/bash
{BASH_REQ}
if [ "$FLEET_TASK" = a ]; then echo a > a.txt; exit 0; fi
PORT=; TOKEN=
while IFS= read -r l; do
  [[ $l =~ WEBPORT=([0-9]+) ]] && PORT=${{BASH_REMATCH[1]}}
  [[ $l =~ WEBTOKEN=([0-9a-f]+) ]] && TOKEN=${{BASH_REMATCH[1]}}
done < "$FLEET_BRIEF_FILE"
req GET /api/status "" result-get.txt
( req GET /api/status "" result-sub.txt )
bash @CHILD@ "$PORT" "$TOKEN" GET /api/status "" result-child.txt
while [ ! -s @OUT@/decide.txt ]; do sleep 0.1; done
read -r ID DIGEST < @OUT@/decide.txt
DECIDE="{{\"id\":\"$ID\",\"approve\":true,\"digest\":\"$DIGEST\"}}"
req POST /api/approval/decide "$DECIDE" result-decide.txt
( req POST /api/approval/decide "$DECIDE" result-decide-sub.txt )
bash @CHILD@ "$PORT" "$TOKEN" POST /api/approval/decide "$DECIDE" result-decide-child.txt
bash -c 'exec bash @CHILD@ "$0" "$1" POST /api/approval/decide "$2" result-decide-deep.txt' "$PORT" "$TOKEN" "$DECIDE"
echo finished > b.txt
"#
        ),
        &[("CHILD", &child), ("OUT", out.path())],
    );
    let s = f.tmp.script("w.sh", &worker);
    f.write_cfg(&simple_cfg(&s, "timeout_secs = 90"));
    let info = f.start_web(&[]);
    let web = Web::new(&info);
    // the token is handed to the worker on purpose, as if it had leaked
    let spec = format!("WEBPORT={} WEBTOKEN={}", info.port, info.token);
    f.plan_and_start("r1", 2, &[t("a"), t("b").spec(&spec)]);

    // an approval exists; the worker is told its id and digest as well
    web.wait_task("a", "completed");
    let merge = web.post("/api/merge", &Value::object([("task", Value::str("a"))]));
    assert_eq!(merge.status, 409, "{}", merge.text());
    let id = merge.subject();
    let st = web.status();
    let digest = jstr(approval(&st, &id).unwrap(), "digest").to_string();
    assert!(is_hex64(&digest));
    fs::write(out.join("decide.tmp"), format!("{id} {digest}\n")).unwrap();
    fs::rename(out.join("decide.tmp"), out.join("decide.txt")).unwrap();

    web.wait_task("b", "completed");
    let dir = f.worktree("r1", "b");
    assert!(
        dir.join("b.txt").exists(),
        "the worker script did not finish"
    );
    for file in [
        "result-get.txt",
        "result-sub.txt",
        "result-child.txt",
        "result-decide.txt",
        "result-decide-sub.txt",
        "result-decide-child.txt",
        "result-decide-deep.txt",
    ] {
        assert_refused_as_worker(&dir, file);
    }

    // nothing was decided
    let st = web.status();
    let a = approval(&st, &id).unwrap();
    assert_eq!(jstr(a, "status"), "pending", "{}", a.canonical());
    assert_eq!(jstr(&f.approvals()[0], "status"), "pending");
    let tree_has_merge = git_branches(&f).iter().any(|b| b == "fleet/r1/integration");
    assert!(!tree_has_merge, "a merge happened");
    // the same request from this process is fine
    let ok = web.get("/api/status");
    assert_eq!(ok.status, 200, "{}", ok.text());
    assert_eq!(jstr(&ok.json(), "schema"), "fleet.status/1");
    // and no worker connection used up the failed-login budget
    for _ in 0..3 {
        assert_eq!(web.get("/api/health").status, 200);
    }
}

// ---- 6. flags --------------------------------------------------------------------------------------------------

#[test]
fn web_flag_policy() {
    let mut f = Fleet::new();

    // a non-loopback address needs --web-allow-remote, and nothing is started without it
    let refused = f.daemon_refused_with(&["--web", "0.0.0.0:0"]);
    assert_eq!(refused.code, 7, "{refused:?}");
    assert!(refused.stderr.contains("--web-allow-remote"), "{refused:?}");
    assert!(
        !f.state.join("control/fleet.sock").exists(),
        "a daemon was started"
    );
    assert_eq!(f.cli(&["status"]).code, 3, "something answers");

    // --web-allow-remote only makes sense with --web
    let usage = f.daemon_refused_with(&["--web-allow-remote"]);
    assert_eq!(usage.code, 2, "{usage:?}");
    assert!(usage.stderr.contains("--web"), "{usage:?}");
    // so does --web-print-login-url: there is no URL to print without a dashboard
    let usage = f.daemon_refused_with(&["--web-print-login-url"]);
    assert_eq!(usage.code, 2, "{usage:?}");
    assert!(usage.stderr.contains("--web"), "{usage:?}");
    assert!(
        !f.state.join("control/fleet.sock").exists(),
        "a daemon was started"
    );
    // a name is not an address
    let name = f.daemon_refused_with(&["--web", "example.com:80"]);
    assert_eq!(name.code, 2, "{name:?}");

    // allowed on loopback, where it changes nothing and says nothing
    let info = f.start_web(&["--web-allow-remote"]);
    let lines = info.stderr_lines();
    assert!(
        lines
            .iter()
            .all(|l| !l.contains("WARNING") && !l.contains("no TLS")),
        "{}",
        lines.len()
    );
    assert_eq!(Web::new(&info).get("/api/status").status, 200);
    assert_eq!(f.shutdown(), 0);

    // on every interface it works, and warns that nothing is encrypted
    let info = f.start_web_at("0.0.0.0:0", &["--web-allow-remote"]);
    let lines = info.stderr_lines();
    let warning = lines
        .iter()
        .find(|l| l.contains("WARNING"))
        .unwrap_or_else(|| panic!("no warning in {} lines", lines.len()));
    assert!(warning.contains("no TLS"), "{warning}");
    assert!(warning.contains("0.0.0.0"), "{warning}");
    assert_eq!(Web::new(&info).get("/api/status").status, 200);
    assert_eq!(f.shutdown(), 0);
}

// ---- 7. shutdown ------------------------------------------------------------------------------------------------

#[test]
fn shutdown_closes_the_web_port_and_the_daemon_exits_cleanly() {
    let (mut f, info) = web_fleet("echo started\nread line\n");
    let web = Web::new(&info);
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_live("r1", "t");
    let mut sse = Sse::open(web.port, "/api/stream?task=r1/t", &web.token);
    sse.output_until(Duration::from_secs(15), |t| t.contains("started"));
    assert!(TcpStream::connect(("127.0.0.1", web.port)).is_ok());

    f.ok(&["shutdown"]);
    // the open stream is closed, not left hanging
    let events = sse.drain_to_close(Duration::from_secs(10));
    for ev in &events {
        assert!(
            ["output", "end", "limit"].contains(&ev.name.as_str()),
            "{events:?}"
        );
    }
    let code = f.wait_daemon_exit(40_000);
    assert_eq!(code, Some(0), "the daemon did not exit cleanly");
    assert!(
        wait_until(5_000, || TcpStream::connect(("127.0.0.1", web.port))
            .is_err()),
        "the port is still open"
    );
    // the control socket is gone with it
    assert_eq!(f.cli(&["status"]).code, 3);
}

// ---- 8. hostile text -----------------------------------------------------------------------------------------------

const HOSTILE: &str = "<script>alert(1)</script><img src=x onerror=alert(1)> & \"quotes\"";

#[test]
fn hostile_task_text_is_inert_in_the_json() {
    let body = r#"case "$FLEET_TASK" in
  s)
    printf 'tail:\033[31mred\033[0m\342\200\256x\n'
    cat > .fleet/result.json <<'EOF'
{"version":1,"status":"done","summary":"pre-marker \u001b[31mred\u001b[0m \u001b]0;pwned\u0007 \u202e rtl post-marker"}
EOF
    ;;
  *) echo x > x.txt ;;
esac
"#;
    let (f, info) = web_fleet(body);
    let web = Web::new(&info);

    // control characters and bidi overrides in a title are refused when the plan is loaded
    for (what, title) in [
        ("ESC", r"red \u001b[31mx"),
        ("OSC", r"osc \u001b]0;pwned\u0007 x"),
        ("bidi", r"bidi \u202eevil"),
    ] {
        let plan = plan_toml(&f.repo, "r1", 1, &[t("h")])
            .replace("title = \"Task h\"", &format!("title = \"{title}\""));
        let file = f.tmp.write(&format!("plan-bad-{what}.toml"), &plan);
        let out = f.cli(&["plan", file.to_str().unwrap()]);
        assert_eq!(out.code, 7, "{what}: {out:?}");
        assert!(!out.stderr.is_empty(), "{what}: no message");
    }
    assert!(jarr(&web.status(), "runs").is_empty(), "a bad plan loaded");

    // markup is allowed in a title; it only has to stay data
    let plan = plan_toml(&f.repo, "r1", 2, &[t("h"), t("s")])
        .replace("title = \"Task h\"", &format!("title = {}", q(HOSTILE)));
    let file = f.tmp.write("plan-hostile.toml", &plan);
    f.ok(&["plan", file.to_str().unwrap()]);
    f.ok(&["start", "--run", "r1"]);
    web.wait_task("h", "completed");
    let done = web.wait_task("s", "completed");

    let resp = web.get("/api/status");
    assert_eq!(resp.status, 200);
    for b in [b'<', b'>', b'&'] {
        assert!(
            !resp.body.contains(&b),
            "raw {:?} in the status document",
            b as char
        );
    }
    assert!(resp
        .text()
        .contains("\\u003cscript\\u003ealert(1)\\u003c/script\\u003e"));
    let st = resp.json();
    assert_eq!(jstr(find_task(&st, "r1", "h").unwrap(), "title"), HOSTILE);

    // the recent events (which repeat the title) are escaped as well
    let events = web.get("/api/events?limit=100");
    assert_eq!(events.status, 200, "{}", events.text());
    assert!(!events.body.contains(&b'<') && !events.body.contains(&b'>'));

    // a worker's own words are sanitized before they are served
    let summary = jstr(&done, "summary").to_string();
    assert!(summary.contains("pre-marker"), "{summary:?}");
    assert!(summary.contains("post-marker"), "{summary:?}");
    assert!(
        !summary.chars().any(|c| c.is_control() || c == '\u{202e}'),
        "{summary:?}"
    );
    for doc in [&resp, &events] {
        assert!(!doc.body.contains(&0x1b), "ESC in a served document");
        assert!(!has_bytes(&doc.body, "\u{202e}".as_bytes()));
        assert!(!has_bytes(&doc.body, b"\\u202e"));
        assert!(!has_bytes(&doc.body, b"\\u001b"));
    }
    let tail = web.get("/api/tail?task=r1/s");
    assert_eq!(tail.status, 200, "{}", tail.text());
    let text = jstr(&tail.json(), "text").to_string();
    assert!(text.contains("tail:") && text.contains("red"), "{text:?}");
    assert!(
        !text.contains('\u{1b}') && !text.contains('\u{202e}'),
        "{text:?}"
    );
    assert!(!tail.body.contains(&0x1b));
}

// ---- 9. health and log -------------------------------------------------------------------------------------------------

#[test]
fn health_and_log_panels() {
    let (f, info) = web_fleet("echo x > x.txt\n");
    let web = Web::new(&info);
    f.plan_and_start("r1", 1, &[t("t")]);
    web.wait_task("t", "completed");

    let health = web.get("/api/health");
    assert_eq!(health.status, 200, "{}", health.text());
    let doc = health.json();
    assert_eq!(jstr(&doc, "schema"), "fleet.doctor/1");
    assert!(jbool(&doc, "ok"), "{}", health.text());
    let checks = jarr(&doc, "checks");
    let log_check = checks
        .iter()
        .find(|c| jstr(c, "name") == "event log")
        .unwrap_or_else(|| panic!("no event log check in {}", health.text()));
    assert_ne!(jstr(log_check, "status"), "fail");

    let log = web.get("/api/log");
    assert_eq!(log.status, 200, "{}", log.text());
    let doc = log.json();
    assert_eq!(jstr(&doc, "schema"), "fleet.web.log/1");
    assert!(jbool(&doc, "ok"), "{}", log.text());
    assert!(jnum(&doc, "records") >= 1, "{}", log.text());
    assert!(is_hex64(jstr(&doc, "head_hash")), "{}", log.text());
    assert!(doc.get("error").is_some_and(Value::is_null));

    // the audit log is tampered with while the daemon runs: once the cache lapses, the
    // dashboard says so
    let mut raw = fs::OpenOptions::new()
        .append(true)
        .open(f.events_log_path())
        .unwrap();
    raw.write_all(b"{\"this\":\"is not a record\"}\n").unwrap();
    drop(raw);
    let mut last = String::new();
    let seen = wait_until(8_000, || {
        let r = web.get("/api/log");
        last = r.text();
        r.status == 200 && !jbool(&r.json(), "ok")
    });
    assert!(seen, "the log never reported tampering: {last}");
    let doc = parse_json(&last);
    assert!(!jstr(&doc, "error").is_empty(), "no error text in {last}");
    assert!(doc.get("head_hash").is_some_and(Value::is_null));
}

// ---- 10. the login URL stays off non-terminals ------------------------------------------------------------------------

#[test]
fn login_url_is_not_printed_when_stderr_is_not_a_terminal() {
    let mut f = Fleet::new();
    let info = f.start_web_quiet();
    // the dashboard is up, and nobody was told how to log in
    let page = http(info.port, "GET", "/", &[], None);
    assert_eq!(page.status, 200);
    assert_eq!(http(info.port, "GET", "/api/status", &[], None).status, 401);
    assert_eq!(f.shutdown(), 0);
    assert!(
        wait_until(10_000, || info.stderr_closed()),
        "the daemon's stderr never closed"
    );
    let text = info.stderr_lines().join("\n");
    assert!(text.contains("dashboard on http://127.0.0.1:"), "{text}");
    assert!(text.contains("--web-print-login-url"), "{text}");
    assert!(!text.contains("#token="), "{text}");
    assert!(!text.contains("shown only now"), "{text}");
    assert!(
        !has_hex64_run(&text),
        "a 64 digit hex string was printed: {text}"
    );
}

// ---- 11. everything the daemon starts is refused, not only workers -----------------------------------------------------

/// Bash functions shared by the probe scripts. `attack TAG` makes every request a hostile
/// descendant could try, each into its own file in `$OUT`; `run_modes` runs it as the process
/// itself, as a subshell, as a separate `bash` child and as a grandchild that is orphaned (adopted
/// by init) but keeps the process group: only the daemon's registry of what it started, not the
/// parent chain, can still tell where that one came from.
const ATTACK_FUNCS: &str = r#"
attack() {
  local tag=$1
  req POST /api/login "{\"token\":\"$TOKEN\"}" "$OUT/$tag-login.txt"
  req GET /api/status "" "$OUT/$tag-status.txt"
  req GET /api/session "" "$OUT/$tag-session.txt"
  req POST /api/approval/decide "{\"id\":\"$ID\",\"approve\":true,\"digest\":\"$DIGEST\"}" "$OUT/$tag-decide.txt"
  req POST /api/merge '{"task":"a"}' "$OUT/$tag-merge.txt"
  "$FLEETBIN" --state-dir "$STATE" status > "$OUT/$tag-socket.txt" 2>&1
  echo "exit=$?" >> "$OUT/$tag-socket.txt"
}
run_modes() {
  attack self
  ( attack sub )
  bash -c 'source "$LIB"; attack child'
  rm -f "$OUT/bg.done"
  ( ( sleep 1; attack bg; : > "$OUT/bg.done" ) & )
  for i in $(seq 300); do [ -e "$OUT/bg.done" ] && break; sleep 0.1; done
}
"#;

const MODES: [&str; 4] = ["self", "sub", "child", "bg"];

fn attack_lib() -> String {
    format!("{BASH_REQ}{ATTACK_FUNCS}")
}

/// A merge request that stays pending: returns the approval id and its digest.
fn pending_merge_approval(web: &Web, task: &str) -> (String, String) {
    let r = web.post("/api/merge", &Value::object([("task", Value::str(task))]));
    assert_eq!(r.status, 409, "{}", r.text());
    assert_eq!(r.code(), "approval_required");
    let id = r.subject();
    let digest = jstr(
        approval(&web.status(), &id).unwrap_or_else(|| panic!("no approval {id}")),
        "digest",
    )
    .to_string();
    assert!(is_hex64(&digest));
    (id, digest)
}

/// Hands the probe scripts the approval they are going to try to decide (written in one step so
/// that a script that polls for it never reads half of it).
fn publish_decision(out: &TempDir, id: &str, digest: &str) {
    fs::write(out.join("decide.tmp"), format!("{id} {digest}\n")).unwrap();
    fs::rename(out.join("decide.tmp"), out.join("decide.txt")).unwrap();
}

/// Every probe request was answered 403 `forbidden` by the admission rule, whatever it was
/// (including a valid login with the real token), and the control socket refused as well.
fn assert_every_probe_refused(out: &Path, token: &str) {
    for tag in MODES {
        for name in ["login", "status", "session", "decide", "merge"] {
            let file = format!("{tag}-{name}.txt");
            assert_refused_as_worker(out, &file);
            let text = fs::read_to_string(out.join(&file)).unwrap();
            assert!(!text.contains(token), "{file} echoes the token");
            assert!(!text.contains("\"session\":\""), "{file}: {text}");
        }
        let file = format!("{tag}-socket.txt");
        let text = fs::read_to_string(out.join(&file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        assert!(text.contains("worker processes"), "{file}: {text}");
        assert!(
            text.lines()
                .any(|l| l.starts_with("exit=") && l != "exit=0"),
            "{file}: the control socket answered: {text}"
        );
        assert!(!text.contains("r1/"), "{file} shows the runs: {text}");
    }
}

/// The operator side is untouched while the daemon's own processes are being refused, and the very
/// same probe script, run from this process (which the daemon did not start), is answered: it is
/// the refusal of the daemon's processes that the tests above saw, not a broken script.
fn assert_operator_unaffected(f: &Fleet, web: &Web, lib: &Path) {
    let ok = web.get("/api/status");
    assert_eq!(ok.status, 200, "{}", ok.text());
    assert_eq!(jstr(&ok.json(), "schema"), "fleet.status/1");
    assert_eq!(jstr(&f.status(), "schema"), "fleet.status/1");

    let dir = TempDir::new("op");
    let ran = std::process::Command::new("bash")
        .env_clear()
        .env("PATH", system_path())
        .env("PORT", web.port.to_string())
        .env("TOKEN", &web.token)
        .env("OUT", dir.path())
        .env("STATE", &f.state)
        .env("FLEETBIN", FLEET_BIN)
        .env("LIB", lib)
        // an approval that does not exist: the operator's decide must not touch a real one
        .env("ID", "a999")
        .env("DIGEST", "0".repeat(64))
        .args(["-c", "source \"$LIB\"; attack operator"])
        .status()
        .unwrap();
    assert!(ran.success());
    let read = |name: &str| fs::read_to_string(dir.join(&format!("operator-{name}.txt"))).unwrap();
    for (name, status, schema) in [
        ("login", "200", "fleet.web.session/1"),
        ("status", "200", "fleet.status/1"),
        ("session", "200", "fleet.web.session/1"),
        ("decide", "404", "fleet.error/1"),
        ("merge", "409", "fleet.error/1"),
    ] {
        let text = read(name);
        assert!(
            text.starts_with(&format!("HTTP/1.1 {status}")),
            "operator {name}: {text}"
        );
        assert!(text.contains(schema), "operator {name}: {text}");
    }
    assert!(read("socket").lines().any(|l| l == "exit=0"));
}

const GATE_PROBE: &str = r#"#!/bin/bash
grep -q 'XPORT=' .fleet/brief.md || exit 0
while IFS= read -r l; do
  [[ $l =~ XPORT=([0-9]+) ]] && PORT=${BASH_REMATCH[1]}
  [[ $l =~ XTOKEN=([0-9a-f]+) ]] && TOKEN=${BASH_REMATCH[1]}
  [[ $l =~ XBIN=([^ ]+) ]] && FLEETBIN=${BASH_REMATCH[1]}
  [[ $l =~ XSTATE=([^ ]+) ]] && STATE=${BASH_REMATCH[1]}
  [[ $l =~ XOUT=([^ ]+) ]] && OUT=${BASH_REMATCH[1]}
  [[ $l =~ XLIB=([^ ]+) ]] && LIB=${BASH_REMATCH[1]}
done < .fleet/brief.md
export PORT TOKEN FLEETBIN STATE OUT LIB
source "$LIB"
for i in $(seq 400); do [ -s "$OUT/decide.txt" ] && break; sleep 0.1; done
[ -s "$OUT/decide.txt" ] || exit 3
read -r ID DIGEST < "$OUT/decide.txt"
export ID DIGEST
run_modes
# the gate passes exactly when every request was refused
bad=0
for f in "$OUT"/*-login.txt "$OUT"/*-status.txt "$OUT"/*-session.txt "$OUT"/*-decide.txt "$OUT"/*-merge.txt; do
  grep -q '^HTTP/1.1 403' "$f" || bad=1
done
exit $bad
"#;

/// Covers admission of a quality gate. A gate is code the daemon runs on the agent's behalf in the
/// task's worktree, so what it could read (the brief, here carrying the web token on purpose) is
/// what a prompt-injected agent could plant there. The earlier rule refused only the descendants
/// of the pty workers, so a gate, a child of it, or a leftover of it that was adopted by init could
/// log in with the token, read the status and decide the approval it can see. Each of those
/// requests, and the control socket, must now be refused while the gate runs, and the daemon must
/// go on working (the task completes and the gate reports that it passed).
#[test]
fn a_quality_gate_child_cannot_use_the_web_ui_or_the_socket() {
    let mut f = Fleet::new();
    let out = TempDir::new("o");
    let lib = f.tmp.write("attack-lib.sh", &attack_lib());
    let gate = f.tmp.script("gate.sh", GATE_PROBE);
    let worker = f.script("w.sh", "echo \"$FLEET_TASK\" > \"$FLEET_TASK.txt\"");
    f.write_cfg(&simple_cfg(&worker, "").set("max_retries", "0").gate(
        "probe",
        &gate,
        &[],
        "timeout_secs = 90",
    ));
    let info = f.start_web(&[]);
    let web = Web::new(&info);
    // the secrets are handed over on purpose, as if the brief had leaked them
    let spec = format!(
        "XPORT={} XTOKEN={} XBIN={} XSTATE={} XOUT={} XLIB={}",
        info.port,
        info.token,
        FLEET_BIN,
        f.state.display(),
        out.path().display(),
        lib.display()
    );
    f.plan_and_start("r1", 2, &[t("a"), t("b").spec(&spec)]);

    // a pending approval exists while the gate of `b` runs
    web.wait_task("a", "completed");
    let (id, digest) = pending_merge_approval(&web, "a");
    publish_decision(&out, &id, &digest);

    let b = web.wait_task_in("b", &["completed", "failed"]);
    assert_eq!(jstr(&b, "status"), "completed", "{}", b.canonical());
    let gates = jarr(&b, "gates");
    assert_eq!(gates.len(), 1, "{}", b.canonical());
    assert!(jbool(&gates[0], "passed"), "{}", b.canonical());
    assert_every_probe_refused(out.path(), &info.token);

    // nothing was decided or merged
    let st = web.status();
    assert_eq!(jstr(approval(&st, &id).unwrap(), "status"), "pending");
    assert_eq!(jstr(&f.approvals()[0], "status"), "pending");
    assert!(
        git_branches(&f).iter().all(|b| b != "fleet/r1/integration"),
        "a merge happened"
    );
    assert_operator_unaffected(&f, &web, &lib);
}

/// Covers admission of a process started by an approved command. The command runs outside any
/// worker session, so it was the second hole of the earlier rule: it, a child of it and an orphaned
/// grandchild (which sleeps first, so the parent chain no longer leads anywhere) could use the
/// token the way the operator does. They must all be refused, the approval they can see must stay
/// pending, and the operator's own requests keep working.
#[test]
fn a_process_started_by_an_approved_command_cannot_use_the_web_ui() {
    let mut f = Fleet::new();
    let out = TempDir::new("o");
    let lib = f.tmp.write("attack-lib.sh", &attack_lib());
    let ext = f.tmp.join("ext.sh");
    let worker = f.script(
        "w.sh",
        &fill(
            "case \"$FLEET_TASK\" in\n  c)\n    if grep -q -e 'The approved command was started' -e 'An operator denied' \"$FLEET_BRIEF_FILE\"; then exit 0; fi\n    printf '{\"version\":1,\"kind\":\"approval\",\"text\":\"need the helper\",\"action\":{\"command\":\"@EXT@\",\"args\":[]}}' > .fleet/question.json ;;\n  *) echo \"$FLEET_TASK\" > \"$FLEET_TASK.txt\" ;;\nesac\n",
            &[("EXT", &ext)],
        ),
    );
    f.write_cfg(&simple_cfg(&worker, ""));
    let info = f.start_web(&[]);
    let web = Web::new(&info);
    // the helper holds the token on purpose, as if it had leaked to it
    f.tmp.script(
        "ext.sh",
        &format!(
            "#!/bin/bash\nexport PORT={} TOKEN={} OUT={} STATE={} FLEETBIN={} LIB={}\nread -r ID DIGEST < \"$OUT/decide.txt\"\nexport ID DIGEST\nsource \"$LIB\"\nrun_modes\n: > \"$OUT/ext.done\"\n",
            info.port,
            info.token,
            out.path().display(),
            f.state.display(),
            FLEET_BIN,
            lib.display()
        ),
    );
    f.plan_and_start("r1", 2, &[t("a"), t("c")]);

    // `a` finishes (its merge approval stays pending for the helper to try), `c` asks to run the
    // helper
    web.wait_task("a", "completed");
    let st = web.wait("the helper's approval", |st| {
        jarr(st, "approvals")
            .iter()
            .any(|a| jstr(a, "task") == "c" && jstr(a, "status") == "pending")
    });
    let helper = jarr(&st, "approvals")
        .iter()
        .find(|a| jstr(a, "task") == "c")
        .unwrap()
        .clone();
    assert_eq!(jstr(&helper, "purpose"), "external-command");
    assert_eq!(jstr(&helper, "command"), ext.to_str().unwrap());
    let (merge_id, merge_digest) = pending_merge_approval(&web, "a");
    publish_decision(&out, &merge_id, &merge_digest);

    // the operator approves the helper over the web, with the digest the dashboard shows
    let helper_id = jstr(&helper, "id").to_string();
    let helper_digest = jstr(&helper, "digest").to_string();
    let ok = web.post(
        "/api/approval/decide",
        &Value::object([
            ("id", Value::str(&helper_id)),
            ("approve", Value::Bool(true)),
            ("digest", Value::str(&helper_digest)),
        ]),
    );
    assert_eq!(ok.status, 200, "{}", ok.text());

    wait_file(&out.join("ext.done"));
    web.wait_task("c", "completed");
    assert_every_probe_refused(out.path(), &info.token);

    let st = web.status();
    assert_eq!(
        jstr(approval(&st, &helper_id).unwrap(), "status"),
        "consumed"
    );
    assert_eq!(jstr(approval(&st, &merge_id).unwrap(), "status"), "pending");
    assert!(
        git_branches(&f).iter().all(|b| b != "fleet/r1/integration"),
        "a merge happened"
    );
    assert_operator_unaffected(&f, &web, &lib);
}

// ---- 12. goals and evidence, served read-only -----------------------------------------------------------------------

fn is_hex40(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// A fleet with one worker script, started with the dashboard, whose retries are on.
fn goal_fleet(worker_body: &str) -> (Fleet, WebInfo) {
    let mut f = Fleet::new();
    let s = f.script("w.sh", worker_body);
    f.write_cfg(&simple_cfg(&s, "").set("max_retries", "2"));
    let info = f.start_web(&[]);
    (f, info)
}

/// Loads a plan with the single task `r1/a` (not started).
fn load_plan_a(f: &Fleet) {
    let file = f
        .tmp
        .write("plan.toml", &plan_toml(&f.repo, "r1", 1, &[t("a")]));
    f.ok(&["plan", file.to_str().unwrap()]);
}

/// `fleet goals add` with a contract file, naming `r1/a`.
fn add_feature_goal(f: &Fleet, files: &TempDir, contract: &str) {
    let path = files.write("contract.json", contract);
    f.ok(&[
        "goals",
        "add",
        "--id",
        "feature",
        "--title",
        "The answer is ready",
        "--intent",
        "answer.txt says ready",
        "--task",
        "r1/a",
        "--contract",
        path.to_str().unwrap(),
    ]);
}

/// Polls a `GET` document until `pred` holds for it.
fn poll(web: &Web, path: &str, what: &str, mut pred: impl FnMut(&Value) -> bool) -> Value {
    let start = Instant::now();
    loop {
        let r = web.get(path);
        assert_eq!(r.status, 200, "{path}: {}", r.text());
        let doc = r.json();
        if pred(&doc) {
            return doc;
        }
        if start.elapsed() > Duration::from_millis(WAIT_MS) {
            panic!("timed out waiting for {what}: {}", doc.canonical());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn goal_row<'a>(doc: &'a Value, id: &str) -> &'a Value {
    jarr(doc, "goals")
        .iter()
        .find(|g| jstr(g, "id") == id)
        .unwrap_or_else(|| panic!("no goal {id} in {}", doc.canonical()))
}

fn task_verification_of(doc: &Value, id: &str) -> String {
    jstr(&jarr(goal_row(doc, id), "tasks")[0], "verification").to_string()
}

#[test]
fn goals_and_evidence_are_served_read_only() {
    let out = TempDir::new("o");
    let files = TempDir::new("files");
    // attempt 1 writes the wrong answer; a retry whose brief quotes the failed check writes the
    // right one. The worker waits for `go`, so the states in between can be looked at.
    let worker = fill(
        "set -eu\nwhile [ ! -f @OUT@/go ]; do sleep 0.1; done\nif grep -q 'verify feature' \"$FLEET_BRIEF_FILE\"; then echo ready > answer.txt; else echo wrong > answer.txt; fi\nprintf '{\"version\":1,\"status\":\"done\",\"summary\":\"all done\"}\\n' > .fleet/result.json\n",
        &[("OUT", out.path())],
    );
    let (f, info) = goal_fleet(&worker);
    let web = Web::new(&info);
    load_plan_a(&f);
    // the last check waits for `verify-go`, so that the task can be seen between "the worker
    // said done" and "fleet's own run finished"
    let contract = fill(
        r#"{"version":1,"checks":[
  {"id":"answer","kind":"file","path":"answer.txt","assert":"contains","value":"ready"},
  {"id":"exists","kind":"command","program":"/bin/sh","args":["-c","test -f answer.txt"]},
  {"id":"slow","kind":"command","program":"/bin/sh","args":["-c","while [ ! -f @OUT@/verify-go ]; do sleep 0.1; done"]}
]}"#,
        &[("OUT", out.path())],
    );
    add_feature_goal(&f, &files, &contract);

    // before anything runs: the goal is open, its task pending, and it is the next thing to do
    let list = web.get("/api/goals");
    assert_eq!(list.status, 200, "{}", list.text());
    let doc = list.json();
    assert_eq!(jstr(&doc, "schema"), "fleet.goals.list/1");
    assert_eq!(jnum(&doc, "total"), 1);
    let row = goal_row(&doc, "feature");
    assert_eq!(jstr(row, "status"), "open", "{}", row.canonical());
    assert_eq!(jstr(row, "contract"), "pending");
    assert_eq!(task_verification_of(&doc, "feature"), "pending");
    let rem = web.get("/api/remaining");
    assert_eq!(rem.status, 200, "{}", rem.text());
    let rem = rem.json();
    assert_eq!(jstr(&rem, "schema"), "fleet.goals.remaining/1");
    assert_eq!(jarr(&rem, "open_leaves").len(), 1, "{}", rem.canonical());
    assert_eq!(jstr(&jarr(&rem, "open_leaves")[0], "id"), "feature");
    let next = jarr(&rem, "next_ready");
    assert_eq!(next.len(), 1, "{}", rem.canonical());
    assert_eq!(jstr(&next[0], "task"), "r1/a");
    assert_eq!(jstr(&next[0], "goal"), "feature");
    assert!(jarr(&rem, "blockers").is_empty());
    assert_eq!(jnum(&rem, "verified"), 0);

    // running, the worker has not said anything: still pending
    f.ok(&["start", "--run", "r1"]);
    f.wait_live("r1", "a");
    let doc = web.get("/api/goals").json();
    assert_eq!(task_verification_of(&doc, "feature"), "pending");
    assert_eq!(
        jstr(&jarr(goal_row(&doc, "feature"), "tasks")[0], "status"),
        "running"
    );
    let rem = web.get("/api/remaining").json();
    assert!(
        jarr(&rem, "next_ready").is_empty(),
        "a running task is not next: {}",
        rem.canonical()
    );

    // the worker claims success with the wrong answer; fleet's own run is not finished, so the
    // claim is "unverified" and nothing is completed
    fs::write(out.join("go"), "").unwrap();
    let doc = poll(&web, "/api/goals", "the task to leave pending", |d| {
        task_verification_of(d, "feature") != "pending"
    });
    assert_eq!(
        task_verification_of(&doc, "feature"),
        "unverified",
        "{}",
        doc.canonical()
    );
    let tasks = jarr(goal_row(&doc, "feature"), "tasks");
    assert_eq!(jstr(&tasks[0], "status"), "review");
    assert_eq!(jstr(goal_row(&doc, "feature"), "status"), "open");
    let first_commit = safe_git(&f.repo, &["rev-parse", "fleet/r1/a"]);
    assert!(is_hex40(&first_commit), "{first_commit}");
    let unverified = web.get("/api/verify?target=r1/a");
    assert_eq!(unverified.status, 200, "{}", unverified.text());
    let doc = unverified.json();
    assert_eq!(jstr(&doc, "schema"), "fleet.verify/1");
    assert_eq!(jstr(&doc, "verification"), "unverified");
    assert!(!jbool(&doc, "passed"));

    // fleet's run finishes: the wrong answer fails its contract, the retry fixes it
    fs::write(out.join("verify-go"), "").unwrap();
    let doc = poll(&web, "/api/goals", "the goal to be verified", |d| {
        jstr(goal_row(d, "feature"), "status") == "verified"
    });
    assert_eq!(jstr(goal_row(&doc, "feature"), "contract"), "passed");
    assert_eq!(task_verification_of(&doc, "feature"), "verified");
    assert_eq!(jnum(&doc, "verified"), 1);
    f.wait_task("r1", "a", &["completed"]);

    let head = safe_git(&f.repo, &["rev-parse", "fleet/r1/a"]);
    let tree = safe_git(&f.repo, &["rev-parse", &format!("{head}^{{tree}}")]);
    assert!(is_hex40(&head) && is_hex40(&tree));
    assert_ne!(head, first_commit, "the retry committed new work");

    let shown = web.get("/api/goal?id=feature");
    assert_eq!(shown.status, 200, "{}", shown.text());
    let goal = shown.json();
    assert_eq!(jstr(&goal, "schema"), "fleet.goals.show/1");
    assert_eq!(jstr(&goal, "id"), "feature");
    assert_eq!(jstr(&goal, "status"), "verified");
    assert_eq!(jstr(&goal, "contract"), "passed");
    assert_eq!(jstr(&goal, "intent"), "answer.txt says ready");
    // the newest evidence of every check names the commit and the tree that were checked, and
    // counts: it is what the task's review recorded
    let checks = jarr(&goal, "checks");
    assert_eq!(
        checks.iter().map(|c| jstr(c, "check")).collect::<Vec<_>>(),
        ["answer", "exists", "slow"]
    );
    for c in checks {
        let latest = c
            .get("latest")
            .filter(|l| !l.is_null())
            .unwrap_or_else(|| panic!("no evidence for {}", c.canonical()));
        assert_eq!(jstr(latest, "verdict"), "passed", "{}", c.canonical());
        assert_eq!(jstr(latest, "commit"), head, "{}", c.canonical());
        assert_eq!(jstr(latest, "tree"), tree, "{}", c.canonical());
        assert!(jbool(latest, "counts"), "{}", c.canonical());
        assert!(!jbool(latest, "dirty"), "{}", c.canonical());
        assert_eq!(jstr(latest, "task"), "a");
    }
    // the history is newest first and keeps the failed run of attempt 1, bound to the commit
    // that run checked
    let history = jarr(&goal, "history");
    assert_eq!(history.len(), 6, "{}", goal.canonical());
    let times: Vec<u64> = history.iter().map(|e| jnum(e, "at_ms")).collect();
    assert!(times.windows(2).all(|w| w[0] >= w[1]), "{times:?}");
    assert_eq!(jstr(&history[0], "verdict"), "passed");
    assert_eq!(jstr(&history[0], "commit"), head);
    let failed: Vec<&Value> = history
        .iter()
        .filter(|e| jstr(e, "verdict") == "failed")
        .collect();
    assert!(!failed.is_empty(), "{}", goal.canonical());
    for e in &failed {
        assert_eq!(jstr(e, "check"), "answer");
        assert_eq!(jstr(e, "commit"), first_commit, "{}", e.canonical());
        assert_eq!(
            jstr(e, "tree"),
            safe_git(&f.repo, &["rev-parse", &format!("{first_commit}^{{tree}}")])
        );
        assert!(!jbool(e, "counts"), "a stale run counts: {}", e.canonical());
    }
    for e in history {
        assert!(is_hex40(jstr(e, "commit")) && is_hex40(jstr(e, "tree")));
    }

    // nothing is left to do
    let rem = web.get("/api/remaining").json();
    assert!(jarr(&rem, "open_leaves").is_empty(), "{}", rem.canonical());
    assert!(jarr(&rem, "next_ready").is_empty());
    assert!(jarr(&rem, "blockers").is_empty());
    assert_eq!(jnum(&rem, "verified"), 1);
    // the state of the verification of the task, and of the goal
    for target in ["r1/a", "feature"] {
        let v = web.get(&format!("/api/verify?target={target}"));
        assert_eq!(v.status, 200, "{}", v.text());
        let v = v.json();
        assert_eq!(jstr(&v, "schema"), "fleet.verify/1");
        assert!(
            jbool(&v, "passed") && !jbool(&v, "running"),
            "{}",
            v.canonical()
        );
        let goals = jarr(&v, "goals");
        assert_eq!(goals.len(), 1);
        assert_eq!(jstr(&goals[0], "goal"), "feature");
        assert_eq!(jstr(&goals[0], "contract"), "passed");
    }
    // the dashboard and the command line tell the same story
    let cli = f.json(&["goals", "show", "feature"]);
    assert_eq!(cli.canonical(), goal.canonical());
    // an unknown goal is a 404 document
    let none = web.get("/api/goal?id=nosuch");
    assert_eq!(none.status, 404, "{}", none.text());
    assert_eq!(web.get("/api/goal").status, 400);
    assert_eq!(web.get("/api/verify").status, 400);
}

// ---- 13. the goal documents cannot be changed over HTTP ---------------------------------------------------------------

#[test]
fn goal_documents_cannot_be_changed_over_http() {
    let files = TempDir::new("files");
    let (f, info) = goal_fleet("exit 0");
    let web = Web::new(&info);
    load_plan_a(&f);
    add_feature_goal(
        &f,
        &files,
        r#"{"version":1,"checks":[{"id":"answer","kind":"file","path":"answer.txt","assert":"exists"}]}"#,
    );
    let goals_before = f.json(&["goals", "list"]);
    let events_before = f.events_raw();
    let status_before = f.status();
    let body = Value::object([
        ("id", Value::str("injected")),
        ("title", Value::str("injected")),
        ("goal", Value::str("feature")),
        ("status", Value::str("abandoned")),
        ("target", Value::str("feature")),
    ])
    .canonical();

    // the four documents have a GET and nothing else
    for path in ["/api/goals", "/api/goal", "/api/remaining", "/api/verify"] {
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            let r = http(
                web.port,
                method,
                path,
                &[("Authorization", &web.bearer())],
                (method != "DELETE").then_some(body.as_str()),
            );
            assert!(
                r.status == 405 || r.status == 404,
                "{method} {path} -> {} {}",
                r.status,
                r.text()
            );
            if method == "POST" {
                assert_eq!(r.status, 405, "{path}: {}", r.text());
                assert_eq!(r.header("allow"), Some("GET"), "{path}");
            }
        }
    }
    // and there is no route that could add, change or verify anything
    for path in [
        "/api/goals/add",
        "/api/goals/attach",
        "/api/goals/status",
        "/api/goal/add",
        "/api/goal/status",
        "/api/goal/attach",
        "/api/verify/start",
        "/api/verify/run",
        "/api/goals/verify",
        "/api/command",
        "/api/goal.add",
    ] {
        let r = web.post_raw(path, &body);
        assert_eq!(r.status, 404, "POST {path}: {}", r.text());
        let r = web.get(path);
        assert_eq!(r.status, 404, "GET {path}: {}", r.text());
    }
    // a command name in the query is not a thing the dashboard understands: the request is
    // refused rather than ignored, and it changes nothing
    for path in [
        "/api/goals?cmd=goal.add&title=injected",
        "/api/goals?command=verify",
        "/api/goal?id=feature&cmd=goal.status",
        "/api/goal?id=feature&status=abandoned",
        "/api/remaining?cmd=verify",
        "/api/verify?target=feature&cmd=verify",
        "/api/verify?target=feature&run=1",
        "/api/verify?target=feature&command=goal.add",
        "/api/status?cmd=shutdown",
        "/api/events?command=shutdown",
    ] {
        let r = web.get(path);
        assert_eq!(r.status, 400, "{path}: {}", r.text());
        assert_eq!(r.code(), "invalid", "{path}");
    }
    // reading the verification state starts nothing
    for _ in 0..2 {
        for path in ["/api/verify?target=feature", "/api/verify?target=r1/a"] {
            let r = web.get(path);
            assert_eq!(r.status, 200, "{path}: {}", r.text());
            let v = r.json();
            assert!(
                !jbool(&v, "running") && !jbool(&v, "passed"),
                "{}",
                v.canonical()
            );
        }
    }
    // an unauthenticated attempt gets nowhere either
    let r = http(web.port, "GET", "/api/goals", &[], None);
    assert_eq!(r.status, 401);

    // nothing changed: the ledger, the audit log and the runs are what they were
    let goals_after = f.json(&["goals", "list"]);
    assert_eq!(goals_after.canonical(), goals_before.canonical());
    assert_eq!(jarr(&goals_after, "goals").len(), 1);
    assert_eq!(jstr(&jarr(&goals_after, "goals")[0], "status"), "open");
    assert_eq!(f.events_raw(), events_before, "something was recorded");
    let status_after = f.status();
    assert_eq!(
        jarr(&status_after, "runs")[0].canonical(),
        jarr(&status_before, "runs")[0].canonical()
    );
    assert_eq!(jstr(&f.task("r1", "a"), "status"), "ready");
    // and the daemon is still there
    assert_eq!(web.get("/api/goals").status, 200);
}

// ---- 14. hostile goal text and evidence --------------------------------------------------------------------------------

#[test]
fn hostile_goal_text_is_inert_over_http() {
    let files = TempDir::new("files");
    let (f, info) = goal_fleet("echo ready > answer.txt\nprintf '{\"version\":1,\"status\":\"done\",\"summary\":\"d\"}\\n' > .fleet/result.json\n");
    let web = Web::new(&info);
    load_plan_a(&f);
    let title = "<script>alert(1)</script> & \"quotes\" <img src=x onerror=alert(1)>";
    let intent = "intent <b>bold</b> & more";
    f.ok(&[
        "goals", "add", "--id", "hostile", "--title", title, "--intent", intent,
    ]);

    // control characters and bidi overrides in a title are refused, so they cannot be served
    for (what, bad) in [
        ("ESC", "red \u{1b}[31mx"),
        ("OSC", "osc \u{1b}]0;pwned\u{7} x"),
        ("bidi", "bidi \u{202e}evil"),
    ] {
        let out = f.cli(&["goals", "add", "--id", "bad", "--title", bad]);
        assert_eq!(out.code, 7, "{what}: {out:?}");
        let out = f.cli(&[
            "goals", "add", "--id", "bad", "--title", "ok", "--intent", bad,
        ]);
        assert_eq!(out.code, 7, "{what} (intent): {out:?}");
    }
    assert_eq!(jarr(&web.get("/api/goals").json(), "goals").len(), 1);

    // a check whose output holds escape sequences, markup and a token-shaped string (assembled by
    // the check itself, so that no file of this test holds one)
    let token = format!("ghp_{}", "a1B2".repeat(10));
    let noisy = r#"{"version":1,"checks":[
  {"id":"present","kind":"file","path":"answer.txt","assert":"exists"},
  {"id":"noisy","kind":"command","mandatory":false,"program":"/bin/sh","args":["-c","t=ghp_; for i in 1 2 3 4 5 6 7 8 9 10; do t=\"${t}a1B2\"; done; printf '\\033]0;owned\\007\\033[31mred\\033[0m token=%s <b>x</b> & done\\n' \"$t\"; exit 3"]}
]}"#;
    add_feature_goal(&f, &files, noisy);
    f.ok(&["start", "--run", "r1"]);
    poll(&web, "/api/goals", "the goal to be verified", |d| {
        jstr(goal_row(d, "feature"), "status") == "verified"
    });

    let documents = [
        web.get("/api/goals"),
        web.get("/api/goal?id=hostile"),
        web.get("/api/goal?id=feature"),
        web.get("/api/remaining"),
        web.get("/api/verify?target=feature"),
        web.get("/api/events?limit=100"),
        web.get("/api/status"),
    ];
    for doc in &documents {
        assert_eq!(doc.status, 200, "{}", doc.text());
        for b in [b'<', b'>', b'&'] {
            assert!(
                !doc.body.contains(&b),
                "raw {:?} in {}",
                b as char,
                doc.text()
            );
        }
        assert!(
            doc.body.iter().all(|b| *b >= 0x20 && *b != 0x7f),
            "a control byte in {}",
            doc.text()
        );
        assert!(!has_bytes(&doc.body, "\u{202e}".as_bytes()));
        assert!(!has_bytes(&doc.body, b"\\u001b") && !has_bytes(&doc.body, b"\\u0007"));
        assert!(
            !doc.text().contains(&token),
            "the token-shaped string was served"
        );
    }
    // the strings are data: parsed back they are exactly what was entered
    let list = documents[0].json();
    assert_eq!(jstr(goal_row(&list, "hostile"), "title"), title);
    assert!(documents[0]
        .text()
        .contains("\\u003cscript\\u003ealert(1)\\u003c/script\\u003e"));
    let shown = documents[1].json();
    assert_eq!(jstr(&shown, "title"), title);
    assert_eq!(jstr(&shown, "intent"), intent);
    let rem = documents[3].json();
    let leaf = jarr(&rem, "open_leaves")
        .iter()
        .find(|g| jstr(g, "id") == "hostile")
        .unwrap_or_else(|| panic!("{}", rem.canonical()));
    assert_eq!(jstr(leaf, "title"), title);

    // the evidence output is sanitized and redacted before it is served
    let feature = documents[2].json();
    let noisy_evidence = jarr(&feature, "history")
        .iter()
        .find(|e| jstr(e, "check") == "noisy")
        .unwrap_or_else(|| panic!("{}", feature.canonical()))
        .clone();
    assert_eq!(jstr(&noisy_evidence, "verdict"), "failed");
    let output = jstr(&noisy_evidence, "output");
    assert!(output.contains("red token="), "{output:?}");
    assert!(output.contains("<b>x</b> & done"), "{output:?}");
    assert!(output.contains("[REDACTED"), "{output:?}");
    assert!(!output.contains(&token) && !output.contains(&"a1B2".repeat(10)));
    // line breaks are text; every other control character and every bidi override is gone
    assert!(
        !output
            .chars()
            .any(|c| (c.is_control() && c != '\n') || c == '\u{202e}'),
        "{output:?}"
    );
    assert!(documents[2]
        .text()
        .contains("\\u003cb\\u003ex\\u003c/b\\u003e"));
    // the optional check failed, the mandatory one holds: the goal is verified
    assert_eq!(jstr(&feature, "status"), "verified");
    assert!(
        !f.events_raw().contains(&token),
        "the audit log holds the secret"
    );
}

// ---- 15. a verification check cannot use the dashboard ----------------------------------------------------------------

/// The requests a check process tries: log in with the token, read the goals, and ask the control
/// socket. `vrun` makes them as the check itself, from a subshell, from a separate `bash`, and
/// from a grandchild that leaves the process group and the session (`setsid`). The grandchild's
/// parent stays alive long enough for the daemon to see the chain and then exits, so the
/// grandchild is orphaned while the check is still running and keeps trying (`vorphan`).
const VERIFY_FUNCS: &str = r#"
vattack() {
  local tag=$1
  req POST /api/login "{\"token\":\"$TOKEN\"}" "$OUT/$tag-login.txt"
  req GET /api/goals "" "$OUT/$tag-goals.txt"
  "$FLEETBIN" --state-dir "$STATE" status > "$OUT/$tag-socket.txt" 2>&1
  echo "exit=$?" >> "$OUT/$tag-socket.txt"
}
vorphan() {
  local i=0 n=0
  echo $$ > "$OUT/detached.pid"
  while [ $i -lt 100 ]; do
    i=$((i+1))
    if kill -0 "$MID" 2>/dev/null; then echo alive > "$OUT/d$i.parent"; else echo gone > "$OUT/d$i.parent"; n=$((n+1)); fi
    vattack "d$i"
    : > "$OUT/d$i.fin"
    [ $n -ge 2 ] && : > "$OUT/orphaned.enough"
    sleep 0.3
  done
}
vrun() {
  vattack self
  ( vattack sub )
  bash -c 'source "$LIB"; vattack child'
  # the middle process stays for at least a second (the daemon looks every 200 ms) and until the
  # grandchild has made its first attempt, then exits and orphans it
  bash -c 'MID=$$; export MID; setsid bash -c "source \"$LIB\"; vorphan" < /dev/null > /dev/null 2>&1 & n=0; while { [ ! -e "$OUT/d1.fin" ] || [ $n -lt 10 ]; } && [ $n -lt 300 ]; do sleep 0.1; n=$((n+1)); done' &
  # the check lives until the orphan has made two attempts of its own (bounded)
  n=0
  while [ ! -e "$OUT/orphaned.enough" ] && [ $n -lt 300 ]; do sleep 0.1; n=$((n+1)); done
}
"#;

const VERIFY_MODES: [&str; 3] = ["self", "sub", "child"];

/// Every request of a probe tag was refused as coming from the daemon's own processes.
fn assert_probe_refused(out: &Path, tag: &str, token: &str) {
    for name in ["login", "goals"] {
        let file = format!("{tag}-{name}.txt");
        assert_refused_as_worker(out, &file);
        let text = fs::read_to_string(out.join(&file)).unwrap();
        assert!(!text.contains("fleet.goals"), "{file}: {text}");
        assert!(!text.contains(token), "{file} echoes the token");
        assert!(!text.contains("\"session\":\""), "{file}: {text}");
    }
    let file = format!("{tag}-socket.txt");
    let text = fs::read_to_string(out.join(&file)).unwrap_or_else(|e| panic!("{file}: {e}"));
    assert!(text.contains("worker processes"), "{file}: {text}");
    assert!(
        text.lines()
            .any(|l| l.starts_with("exit=") && l != "exit=0"),
        "{file}: the control socket answered: {text}"
    );
    assert!(!text.contains("r1/"), "{file} shows the runs: {text}");
}

/// Covers the origin rule for verification: the checks of a contract are programs the daemon runs
/// for a task, with paths and arguments that an agent's work can influence. The token is handed to
/// this one on purpose (in the contract's arguments). The check, a subshell and a child of it, and a
/// grandchild that left its process group and session (and, once its parent exited, has no
/// ancestor to give it away) must all be refused on the dashboard and on the control socket, while
/// the verification itself completes and the same requests from this process work.
#[test]
fn a_verification_check_process_cannot_use_the_web_ui() {
    let out = TempDir::new("o");
    let files = TempDir::new("files");
    let (mut f, info) = {
        let mut f = Fleet::new();
        let s = f.script(
            "w.sh",
            "echo ready > answer.txt\nprintf '{\"version\":1,\"status\":\"done\",\"summary\":\"d\"}\\n' > .fleet/result.json\n",
        );
        f.write_cfg(&simple_cfg(&s, ""));
        let info = f.start_web(&[]);
        (f, info)
    };
    let web = Web::new(&info);
    let lib = f
        .tmp
        .write("verify-lib.sh", &format!("{BASH_REQ}{VERIFY_FUNCS}"));
    let check = f.tmp.script(
        "vcheck.sh",
        "#!/bin/bash\nexport PATH=/usr/local/bin:/usr/bin:/bin\nPORT=$1 TOKEN=$2 FLEETBIN=$3 STATE=$4 OUT=$5 LIB=$6\nexport PORT TOKEN FLEETBIN STATE OUT LIB\nsource \"$LIB\"\nvrun\nexit 0\n",
    );
    load_plan_a(&f);
    let contract = format!(
        r#"{{"version":1,"checks":[{{"id":"spy","kind":"command","timeout_ms":120000,"program":{},"args":[{},{},{},{},{},{}]}}]}}"#,
        q(&check.to_string_lossy()),
        q(&info.port.to_string()),
        q(&info.token),
        q(FLEET_BIN),
        q(&f.state.to_string_lossy()),
        q(&out.path().to_string_lossy()),
        q(&lib.to_string_lossy()),
    );
    add_feature_goal(&f, &files, &contract);
    f.ok(&["start", "--run", "r1"]);

    // the verification completes, whatever its check's processes were refused
    let doc = poll(&web, "/api/goals", "the goal to be verified", |d| {
        jstr(goal_row(d, "feature"), "status") == "verified"
    });
    assert_eq!(jstr(goal_row(&doc, "feature"), "contract"), "passed");

    for tag in VERIFY_MODES {
        assert_probe_refused(out.path(), tag, &info.token);
    }
    // the grandchild that left the process group and the session kept trying for as long as the
    // check lived: first while its parent was alive, then orphaned (only what the daemon
    // remembers about the processes of its jobs can tell where that one came from)
    let mut attempts: Vec<String> = fs::read_dir(out.path())
        .unwrap()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let tag = name.strip_suffix(".fin")?;
            tag.starts_with('d').then(|| tag.to_string())
        })
        .collect();
    attempts.sort();
    assert!(attempts.len() >= 3, "too few attempts: {attempts:?}");
    let mut orphaned = 0;
    for tag in &attempts {
        assert_probe_refused(out.path(), tag, &info.token);
        if out.read(&format!("{tag}.parent")).trim() == "gone" {
            orphaned += 1;
        }
    }
    assert!(
        orphaned >= 2,
        "too few attempts after the grandchild was orphaned: {attempts:?}"
    );
    // and it did not outlive the check
    let pid: u32 = out.read("detached.pid").trim().parse().unwrap();
    assert!(
        wait_until(10_000, || !pid_alive(pid)),
        "the detached grandchild {pid} outlived its check"
    );

    // the same script from this process, which the daemon did not start, is answered
    let dir = TempDir::new("op");
    let ran = std::process::Command::new("bash")
        .env_clear()
        .env("PATH", system_path())
        .env("PORT", web.port.to_string())
        .env("TOKEN", &web.token)
        .env("OUT", dir.path())
        .env("STATE", &f.state)
        .env("FLEETBIN", FLEET_BIN)
        .env("LIB", &lib)
        .args(["-c", "source \"$LIB\"; vattack operator"])
        .status()
        .unwrap();
    assert!(ran.success());
    let read = |name: &str| fs::read_to_string(dir.join(&format!("operator-{name}.txt"))).unwrap();
    let login = read("login");
    assert!(login.starts_with("HTTP/1.1 200"), "{login}");
    assert!(login.contains("fleet.web.session/1"), "{login}");
    let goals = read("goals");
    assert!(goals.starts_with("HTTP/1.1 200"), "{goals}");
    assert!(goals.contains("fleet.goals.list/1"), "{goals}");
    assert!(read("socket").lines().any(|l| l == "exit=0"));
    assert_eq!(jstr(&f.status(), "schema"), "fleet.status/1");
    assert_eq!(web.get("/api/goals").status, 200);

    // the ledger is as the verification left it: one goal, verified, nothing injected
    let list = f.json(&["goals", "list"]);
    assert_eq!(jarr(&list, "goals").len(), 1, "{}", list.canonical());
    assert_eq!(jstr(&jarr(&list, "goals")[0], "status"), "verified");
    f.shutdown();
}

// ---- 14. the review workbench: the repository at recorded commits, read only ------------------------------------------

const WORKBENCH_WORKER: &str = r##"#!/bin/bash
set -e
mkdir -p src docs img .fleet
if [ "$FLEET_TASK" = a ]; then
  mv src/lib.txt src/renamed.txt
  printf 'one\ntwo\nthree\nfour\n' > src/renamed.txt
  printf 'fn main() {}\n' > src/main.rs
  printf '# Title\n\nSee [guide](docs/guide.md), [site](https://example.com/x) and [evil](javascript:alert(1)).\n\n![logo](img/logo.png) ![remote](https://tracker.example/p.png)\n\n<script>alert(1)</script>\n\n| a | b |\n|---|---|\n| 1 | 2 |\n' > README.md
  printf '# Guide\n' > docs/guide.md
  echo 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==' | base64 -d > img/logo.png
  printf 'a\r\nb\r\n' > crlf.txt
  printf '\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\x9c\x40\0\0\x9c\x40\x08\x02\0\0\0\0\0\0\0' > img/huge.png
  printf 'GIF89a\xff\xff\xff\xff\0\0\0' > img/huge.gif
  printf 'bare\rcr\n' > lone-cr.txt
  printf 'back\\slash\\u{202e}\n' > 'lit\u{202e}eral\\name.txt'
  printf 'caf\xe9\n' > latin1.txt
  printf '\0\1\2\0' > bin.dat
  printf '<svg xmlns="x" onload="alert(1)"><script>alert(2)</script></svg>\n' > evil.svg
  ln -s ../../../../etc/passwd src/up
  ln -s /etc/passwd abs
  ln -s src dirlink
  printf '#!/bin/sh\n' > run.sh; chmod +x run.sh
  printf x > $'\xc3\xa9.txt'
  : > $'we\nird\x1b[31mname.txt'
  : > $'bidi\xe2\x80\xaetxt.exe'
  : > 'sp ace.txt'
  head -c 3000000 /dev/zero | tr '\0' x > big.txt
else
  printf 'other\n' > other.txt
fi
printf '{"version":1,"status":"done","summary":"wrote files"}' > .fleet/result.json
"##;

/// A daemon with the dashboard on and two finished tasks, `a` (many kinds of files) and `b`.
fn workbench_fleet() -> (Fleet, Web) {
    let mut f = Fleet::new();
    let s = f.tmp.script("w.sh", WORKBENCH_WORKER);
    f.write_cfg(&simple_cfg(&s, ""));
    let info = f.start_web(&[]);
    let web = Web::new(&info);
    f.plan_and_start("r1", 2, &[t("a"), t("b")]);
    web.wait_task("a", "completed");
    web.wait_task("b", "completed");
    (f, web)
}

fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn repo_url(kind: &str, task: &str, commit: &str, path: &str) -> String {
    format!(
        "/api/repo/{kind}?task={}&commit={}&path={}",
        enc(task),
        enc(commit),
        enc(path)
    )
}

const CSP_PAGE: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

/// Every response of the workbench carries the security headers; JSON is escaped.
fn assert_workbench_response(r: &Resp, what: &str) {
    assert_eq!(
        r.header("x-content-type-options"),
        Some("nosniff"),
        "{what}"
    );
    assert_eq!(r.header("cache-control"), Some("no-store"), "{what}");
    assert_eq!(r.header("referrer-policy"), Some("no-referrer"), "{what}");
    assert_eq!(r.header("x-frame-options"), Some("DENY"), "{what}");
    assert_eq!(
        r.header("cross-origin-resource-policy"),
        Some("same-origin"),
        "{what}"
    );
    assert_eq!(r.all("content-security-policy").len(), 1, "{what}");
    let csp = r.header("content-security-policy").unwrap();
    if r.header("content-type").unwrap_or("").starts_with("image/") {
        assert_eq!(csp, "default-src 'none'; sandbox", "{what}");
    } else {
        assert_eq!(csp, CSP_PAGE, "{what}");
        for b in [b'<', b'>', b'&', 0x1b, 0x00, 0x07] {
            assert!(
                !r.body.contains(&b),
                "{what}: raw byte {b:#x} in the document"
            );
        }
        for bad in ["\u{202e}", "\u{2028}", "\u{200b}", "\u{feff}"] {
            assert!(!has_bytes(&r.body, bad.as_bytes()), "{what}: raw {bad:?}");
        }
    }
}

fn get_ok(web: &Web, path: &str) -> Value {
    let r = web.get(path);
    assert_eq!(r.status, 200, "{path}: {}", r.text());
    assert_workbench_response(&r, path);
    r.json()
}

fn get_err(web: &Web, path: &str, status: u16, code: &str) {
    let r = web.get(path);
    assert_eq!(r.status, status, "{path}: {}", r.text());
    assert_eq!(r.code(), code, "{path}: {}", r.text());
    assert_workbench_response(&r, path);
}

fn ids(web: &Web, task: &str) -> (String, String) {
    let st = web.status();
    let run = jarr(&st, "runs")[0].clone();
    let base = jstr(&run, "base_sha").to_string();
    let head = jstr(find_task(&st, "r1", task).unwrap(), "head").to_string();
    assert!(is_hex40(&base) && is_hex40(&head), "{base} {head}");
    (base, head)
}

fn entry<'a>(doc: &'a Value, name: &str) -> &'a Value {
    jarr(doc, "entries")
        .iter()
        .find(|e| jstr(e, "name") == name)
        .unwrap_or_else(|| panic!("no entry {name:?} in {}", doc.canonical()))
}

fn file_of<'a>(doc: &'a Value, path: &str) -> &'a Value {
    jarr(doc, "files")
        .iter()
        .find(|f| jstr(f, "path") == path)
        .unwrap_or_else(|| panic!("no file {path:?} in the diff"))
}

#[test]
fn the_workbench_shows_the_recorded_commit_of_a_task_from_the_real_repository() {
    let (_f, web) = workbench_fleet();
    let (base, head) = ids(&web, "a");

    // ---- the tree at the head commit, at the base commit and below a directory
    let root = get_ok(&web, &repo_url("tree", "r1/a", "head", ""));
    assert_eq!(jstr(&root, "schema"), "fleet.repo.tree/1");
    assert_eq!(jstr(&root, "task"), "r1/a");
    assert_eq!(jstr(&root, "at"), "head");
    assert_eq!(jstr(&root, "commit"), head);
    assert!(!jbool(&root, "truncated"));
    assert_eq!(jstr(entry(&root, "src"), "kind"), "dir");
    assert_eq!(jstr(entry(&root, "run.sh"), "mode"), "executable");
    assert_eq!(jstr(entry(&root, "abs"), "kind"), "symlink");
    assert_eq!(jstr(entry(&root, "dirlink"), "kind"), "symlink");
    assert_eq!(jstr(entry(&root, "big.txt"), "kind"), "file");
    assert_eq!(jnum(entry(&root, "big.txt"), "size"), 3_000_000);
    // directories come first
    assert_eq!(jstr(&jarr(&root, "entries")[0], "kind"), "dir");
    // hostile names are listed, escaped, and cannot be requested back
    let evil = jarr(&root, "entries")
        .iter()
        .find(|e| jstr(e, "name").contains("\\u{1b}"))
        .expect("the name with an escape sequence is listed");
    assert!(
        jstr(evil, "name").contains("\\u{1b}") && jstr(evil, "name").contains("\\u{a}"),
        "{}",
        jstr(evil, "name")
    );
    assert!(evil.get("path").unwrap().is_null());
    let bidi = jarr(&root, "entries")
        .iter()
        .find(|e| jstr(e, "name").contains("txt.exe"))
        .unwrap();
    assert!(jstr(bidi, "name").contains("\\u{202e}"));
    assert!(bidi.get("path").unwrap().is_null());
    assert_eq!(jstr(entry(&root, "sp ace.txt"), "path"), "sp ace.txt");

    let at_base = get_ok(&web, &repo_url("tree", "r1/a", "base", ""));
    assert_eq!(jstr(&at_base, "commit"), base);
    let names: Vec<&str> = jarr(&at_base, "entries")
        .iter()
        .map(|e| jstr(e, "name"))
        .collect();
    assert_eq!(
        names,
        ["src", "README.md"],
        "the base commit has the fixture's files only"
    );
    let by_sha = get_ok(&web, &repo_url("tree", "r1/a", &head, "src"));
    assert_eq!(jstr(&by_sha, "at"), head);
    let names: Vec<&str> = jarr(&by_sha, "entries")
        .iter()
        .map(|e| jstr(e, "name"))
        .collect();
    assert_eq!(names, ["main.rs", "renamed.txt", "up"]);
    // the task may also be named by its bare id
    assert_eq!(
        jstr(&get_ok(&web, &repo_url("tree", "a", "head", "")), "task"),
        "r1/a"
    );

    // ---- files
    let readme = get_ok(&web, &repo_url("file", "r1/a", "head", "README.md"));
    assert_eq!(jstr(&readme, "schema"), "fleet.repo.file/1");
    assert_eq!(
        (jstr(&readme, "view"), jstr(&readme, "class")),
        ("text", "text")
    );
    assert!(jbool(&readme, "markdown"));
    assert_eq!(jstr(&jarr(&readme, "lines")[0], ""), "");
    let lines: Vec<&str> = jarr(&readme, "lines")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(lines[0], "# Title");
    assert!(
        lines.contains(&"<script>alert(1)</script>"),
        "markup is text: {lines:?}"
    );
    let old = get_ok(&web, &repo_url("file", "r1/a", "base", "README.md"));
    assert_eq!(jarr(&old, "lines")[0].as_str(), Some("# fixture"));
    let crlf = get_ok(&web, &repo_url("file", "r1/a", "head", "crlf.txt"));
    assert!(jbool(&crlf, "crlf"));
    assert_eq!(jarr(&crlf, "lines")[0].as_str(), Some("a"));
    let latin1 = get_ok(&web, &repo_url("file", "r1/a", "head", "latin1.txt"));
    assert!(jbool(&latin1, "lossy"));
    assert_eq!(jarr(&latin1, "lines")[0].as_str(), Some("caf\u{fffd}"));
    let bin = get_ok(&web, &repo_url("file", "r1/a", "head", "bin.dat"));
    assert_eq!(
        (jstr(&bin, "view"), jstr(&bin, "class")),
        ("binary", "binary")
    );
    assert_eq!(jnum(&bin, "size"), 4);
    assert!(jarr(&bin, "lines").is_empty());
    let big = get_ok(&web, &repo_url("file", "r1/a", "head", "big.txt"));
    assert_eq!(jstr(&big, "view"), "too_large");
    assert_eq!(jnum(&big, "size"), 3_000_000);
    assert!(jarr(&big, "lines").is_empty());
    let svg = get_ok(&web, &repo_url("file", "r1/a", "head", "evil.svg"));
    assert_eq!(
        jstr(&svg, "view"),
        "text",
        "an SVG is shown as source, never as an image"
    );
    assert!(
        jstr(&jarr(&svg, "lines")[0], "").is_empty()
            || jarr(&svg, "lines")[0].as_str().unwrap().contains("onload")
    );
    let png = get_ok(&web, &repo_url("file", "r1/a", "head", "img/logo.png"));
    assert_eq!((jstr(&png, "view"), jstr(&png, "image")), ("image", "png"));
    // links are described, not followed
    let up = get_ok(&web, &repo_url("file", "r1/a", "head", "src/up"));
    assert_eq!(jstr(&up, "view"), "symlink");
    assert_eq!(jstr(&up, "target"), "../../../../etc/passwd");
    assert!(up.get("target_path").unwrap().is_null());
    let abs = get_ok(&web, &repo_url("file", "r1/a", "head", "abs"));
    assert_eq!(jstr(&abs, "target"), "/etc/passwd");
    assert!(abs.get("target_path").unwrap().is_null());
    let dirlink = get_ok(&web, &repo_url("file", "r1/a", "head", "dirlink"));
    assert_eq!(jstr(&dirlink, "target"), "src");
    assert_eq!(jstr(&dirlink, "target_path"), "src");
    get_err(
        &web,
        &repo_url("file", "r1/a", "head", "dirlink/main.rs"),
        404,
        "not_found",
    );
    get_err(
        &web,
        &repo_url("tree", "r1/a", "head", "dirlink"),
        400,
        "invalid",
    );
    get_err(
        &web,
        &repo_url("file", "r1/a", "head", "src"),
        400,
        "invalid",
    );
    get_err(
        &web,
        &repo_url("file", "r1/a", "head", "nope.txt"),
        404,
        "not_found",
    );
    // a name is exactly the bytes it has: composed and decomposed forms are different names
    let composed = get_ok(&web, &repo_url("file", "r1/a", "head", "\u{e9}.txt"));
    assert_eq!(jarr(&composed, "lines")[0].as_str(), Some("x"));
    get_err(
        &web,
        &repo_url("file", "r1/a", "head", "e\u{301}.txt"),
        404,
        "not_found",
    );

    // ---- images: the bytes, with the headers that keep them inert
    let img = web.get(&repo_url("blob", "r1/a", "head", "img/logo.png"));
    assert_eq!(img.status, 200, "{}", img.text());
    assert_workbench_response(&img, "blob");
    assert_eq!(img.header("content-type"), Some("image/png"));
    assert_eq!(img.header("content-disposition"), Some("inline"));
    assert!(img.body.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert_eq!(img.body.len(), 70);
    assert_eq!(img.header("x-image-width"), Some("1"));
    assert_eq!(img.header("x-image-height"), Some("1"));
    // an image that declares a canvas of 40000 x 40000 pixels (a 33 byte file) is never served,
    // and its file document is a placeholder with the size and the limits
    for huge in ["img/huge.png", "img/huge.gif"] {
        let r = web.get(&repo_url("blob", "r1/a", "head", huge));
        assert_eq!(r.status, 413, "{huge}: {}", r.text());
        assert_workbench_response(&r, huge);
        assert!(r
            .header("content-type")
            .unwrap()
            .starts_with("application/json"));
        assert!(r.header("x-image-width").is_none());
        let d = get_ok(&web, &repo_url("file", "r1/a", "head", huge));
        assert_eq!(jstr(&d, "view"), "image_too_large", "{huge}");
        assert_eq!(jnum(&d, "max_side"), 8192);
        assert_eq!(jnum(&d, "max_pixels"), 32_000_000);
        assert!(jnum(&d, "width") >= 40_000 || huge.ends_with("gif"));
    }
    // a bare carriage return stays in the text for the page to make visible; a literal
    // backslash is doubled in names so it cannot pass for an escape
    let cr = get_ok(&web, &repo_url("file", "r1/a", "head", "lone-cr.txt"));
    assert_eq!(jarr(&cr, "lines")[0].as_str(), Some("bare\rcr"));
    let root_names: Vec<String> = jarr(&root, "entries")
        .iter()
        .map(|e| jstr(e, "name").to_string())
        .collect();
    assert!(
        root_names
            .iter()
            .any(|n| n.contains("\\\\u{202e}eral\\\\\\\\name")),
        "{root_names:?}"
    );
    for (path, status) in [
        ("evil.svg", 415),
        ("README.md", 415),
        ("bin.dat", 415),
        ("abs", 415),
        ("src", 400),
        ("big.txt", 415),
        ("nope.png", 404),
    ] {
        let r = web.get(&repo_url("blob", "r1/a", "head", path));
        assert_eq!(r.status, status, "{path}: {}", r.text());
        assert_workbench_response(&r, path);
        assert!(
            r.header("content-type")
                .unwrap()
                .starts_with("application/json"),
            "{path}"
        );
    }

    // ---- Markdown: a closed tree, links classified, nothing remote loaded
    let md = get_ok(&web, &repo_url("markdown", "r1/a", "head", "README.md"));
    assert_eq!(jstr(&md, "schema"), "fleet.repo.markdown/1");
    let text = md.canonical();
    assert!(text.contains("\"kind\":\"external\"") && text.contains("https://example.com/x"));
    assert!(text.contains("\"path\":\"docs/guide.md\""));
    assert!(text.contains("\"path\":\"img/logo.png\""));
    assert!(text.contains("remote images are not loaded"));
    assert!(!text.contains("\"url\":\"javascript:"), "{text}");
    assert!(
        text.contains("<script>alert(1)</script>"),
        "raw markup is text: {text}"
    );
    assert!(text.contains("\"t\":\"table\""));
    get_err(
        &web,
        &repo_url("markdown", "r1/a", "head", "crlf.txt"),
        400,
        "invalid",
    );
    get_err(
        &web,
        &repo_url("markdown", "r1/a", "head", "big.txt"),
        400,
        "invalid",
    );

    // ---- the diff of the task's branch against the run's base commit
    let d = get_ok(&web, &format!("/api/repo/diff?task={}", enc("r1/a")));
    assert_eq!(jstr(&d, "schema"), "fleet.repo.diff/1");
    assert_eq!(
        (jstr(&d, "base"), jstr(&d, "head")),
        (base.as_str(), head.as_str())
    );
    assert!(!jbool(&d, "truncated"), "{:?}", d.get("notice"));
    let readme = file_of(&d, "README.md");
    assert_eq!(jstr(readme, "status"), "modified");
    let renamed = file_of(&d, "src/renamed.txt");
    assert_eq!(jstr(renamed, "status"), "renamed");
    assert_eq!(jstr(renamed, "old_path"), "src/lib.txt");
    assert!(jnum(renamed, "similarity") >= 50);
    assert_eq!(jstr(file_of(&d, "src/main.rs"), "status"), "added");
    let logo = file_of(&d, "img/logo.png");
    assert!(jbool(logo, "binary") && jarr(logo, "hunks").is_empty());
    assert!(jbool(file_of(&d, "bin.dat"), "binary"));
    assert_eq!(jstr(file_of(&d, "run.sh"), "new_mode"), "100755");
    assert_eq!(jstr(file_of(&d, "abs"), "new_mode"), "120000");
    let crlf_file = file_of(&d, "crlf.txt");
    let first = &jarr(&jarr(crlf_file, "hunks")[0], "lines")[0];
    assert!(jbool(first, "cr"), "{}", first.canonical());
    // the one very long line is clipped with a marker
    let big_file = file_of(&d, "big.txt");
    let big_line = jstr(&jarr(&jarr(big_file, "hunks")[0], "lines")[0], "t");
    assert!(
        big_line.contains("more characters]") && big_line.len() < 2100,
        "{}",
        big_line.len()
    );
    // hostile file names are shown escaped and are not offered to the viewer
    let weird = jarr(&d, "files")
        .iter()
        .find(|f| jstr(f, "path").contains("\\u{1b}"))
        .unwrap();
    assert!(!jbool(weird, "path_ok") && jstr(weird, "path").contains("\\u{1b}"));
    let stats = d.get("stats").unwrap();
    assert_eq!(jnum(stats, "files"), jarr(&d, "files").len() as u64);
    assert!(jnum(stats, "binary") >= 2 && jnum(stats, "additions") > 10);
    // the other direction, an explicit context and a commit by id
    let back = get_ok(
        &web,
        &format!(
            "/api/repo/diff?task={}&base=head&head=base&context=0",
            enc("r1/a")
        ),
    );
    assert_eq!(jstr(file_of(&back, "src/main.rs"), "status"), "deleted");
    assert_eq!(jnum(&back, "context"), 0);
    let by_id = get_ok(
        &web,
        &format!(
            "/api/repo/diff?task={}&base={base}&head={head}",
            enc("r1/a")
        ),
    );
    assert_eq!(jarr(&by_id, "files").len(), jarr(&d, "files").len());
    // task b's diff is its own
    let b = get_ok(&web, &format!("/api/repo/diff?task={}", enc("r1/b")));
    assert_eq!(jarr(&b, "files").len(), 1);
    assert_eq!(jstr(&jarr(&b, "files")[0], "path"), "other.txt");
}

#[test]
fn the_workbench_reads_the_repository_never_the_worktree() {
    let (f, web) = workbench_fleet();
    let (_, head) = ids(&web, "a");
    let wt = f.worktree("r1", "a");
    // a worker (or anything else with write access to its worktree) changes what is there...
    fs::write(wt.join("README.md"), "FORGED in the worktree\n").unwrap();
    fs::write(wt.join("src/main.rs"), "forged\n").unwrap();
    fs::write(wt.join("untracked-secret.txt"), "not committed\n").unwrap();
    // ...and repoints the worktree's own `.git` file at a repository of its own
    let forged = f.tmp.join("forged");
    fs::create_dir_all(&forged).unwrap();
    safe_git(&forged, &["init", "-q", "-b", "main"]);
    fs::write(forged.join("README.md"), "FORGED repository\n").unwrap();
    safe_git(&forged, &["add", "-A"]);
    safe_git(&forged, &["commit", "-q", "-m", "forged"]);
    fs::write(
        wt.join(".git"),
        format!("gitdir: {}\n", forged.join(".git").display()),
    )
    .unwrap();
    // what is served is what the repository holds at the recorded commit
    let readme = get_ok(&web, &repo_url("file", "r1/a", "head", "README.md"));
    assert_eq!(jarr(&readme, "lines")[0].as_str(), Some("# Title"));
    let main = get_ok(&web, &repo_url("file", "r1/a", "head", "src/main.rs"));
    assert_eq!(jarr(&main, "lines")[0].as_str(), Some("fn main() {}"));
    get_err(
        &web,
        &repo_url("file", "r1/a", "head", "untracked-secret.txt"),
        404,
        "not_found",
    );
    let root = get_ok(&web, &repo_url("tree", "r1/a", "head", ""));
    assert_eq!(jstr(&root, "commit"), head);
    assert!(jarr(&root, "entries")
        .iter()
        .all(|e| jstr(e, "name") != "untracked-secret.txt"));
    // and the worktree may be gone altogether
    fs::remove_dir_all(&wt).unwrap();
    let again = get_ok(&web, &repo_url("file", "r1/a", "head", "docs/guide.md"));
    assert_eq!(jarr(&again, "lines")[0].as_str(), Some("# Guide"));
    let d = get_ok(&web, &format!("/api/repo/diff?task={}", enc("r1/a")));
    assert_eq!(jstr(file_of(&d, "src/main.rs"), "status"), "added");
}

#[test]
fn clients_cannot_name_paths_commits_or_tasks_the_daemon_did_not_record() {
    let (f, web) = workbench_fleet();
    let (base, head) = ids(&web, "a");
    let (_, head_b) = ids(&web, "b");
    let events_before = web.get("/api/events?limit=500").text();

    // paths: every form of leaving the tree, or of naming something that is not a plain entry
    for (path, status, code) in [
        ("..", 400, "invalid"),
        ("../README.md", 400, "invalid"),
        ("src/../README.md", 400, "invalid"),
        ("src/./main.rs", 400, "invalid"),
        ("./README.md", 400, "invalid"),
        ("/etc/passwd", 400, "invalid"),
        ("/README.md", 400, "invalid"),
        ("//", 400, "invalid"),
        ("src//main.rs", 400, "invalid"),
        ("src/", 400, "invalid"),
        ("src\\main.rs", 400, "invalid"),
        ("..\\README.md", 400, "invalid"),
        (".git/config", 400, "invalid"),
        (".GIT/HEAD", 400, "invalid"),
        ("src/.git/x", 400, "invalid"),
        ("a\nb", 400, "invalid"),
        ("a\rb", 400, "invalid"),
        ("a\tb", 400, "invalid"),
        ("a\u{1b}[31mb", 400, "invalid"),
        ("a\u{202e}b", 400, "invalid"),
        ("a\u{200b}b", 400, "invalid"),
        ("a\u{7f}", 400, "invalid"),
        // spelled like traversal but just names (full-width dots, a division slash)
        ("\u{ff0e}\u{ff0e}/README.md", 404, "not_found"),
        ("src\u{2215}main.rs", 404, "not_found"),
        // a double-encoded `..` is the literal text `%2e%2e`
        ("%2e%2e/README.md", 404, "not_found"),
        ("README.md ", 404, "not_found"),
        ("readme.md", 404, "not_found"),
    ] {
        for kind in ["file", "tree", "markdown", "blob"] {
            let url = repo_url(kind, "r1/a", "head", path);
            let r = web.get(&url);
            assert_eq!(r.status, status, "{kind} {path:?}: {}", r.text());
            assert_eq!(r.code(), code, "{kind} {path:?}: {}", r.text());
            assert!(r
                .header("content-type")
                .unwrap()
                .starts_with("application/json"));
            assert_workbench_response(&r, &format!("{kind} {path:?}"));
        }
    }
    // raw (not percent-decoded by us) escapes in the target
    for raw in [
        "/api/repo/file?task=r1%2Fa&path=%2e%2e%2f%2e%2e%2fetc%2fpasswd",
        "/api/repo/file?task=r1%2Fa&path=%2E%2E",
        "/api/repo/file?task=r1%2Fa&path=a%00b",
        "/api/repo/file?task=r1%2Fa&path=%5c..%5c",
        "/api/repo/file?task=r1%2Fa&path=%ff%fe",
    ] {
        let r = web.get(raw);
        assert!(matches!(r.status, 400), "{raw}: {} {}", r.status, r.text());
        assert_workbench_response(&r, raw);
    }
    // overlong: path, component and depth
    for path in ["a".repeat(1025), "b/".repeat(40) + "c", "x".repeat(300)] {
        get_err(
            &web,
            &repo_url("file", "r1/a", "head", &path),
            400,
            "invalid",
        );
    }

    // commits: only `head`, `base` and ids the daemon recorded for this very task
    for bad in [
        "main",
        "master",
        "HEAD",
        "refs/heads/main",
        "fleet/r1/a",
        "fleet/r1/integration",
        "HEAD~1",
        "head^",
        "head~1",
        "@{u}",
        "--output=/tmp/x",
        "-1",
        "",
        " ",
        "HEAD:README.md",
    ] {
        get_err(
            &web,
            &repo_url("file", "r1/a", bad, "README.md"),
            400,
            "invalid",
        );
    }
    for bad in [
        &head[..7],
        &head[..39],
        &head.to_uppercase(),
        &format!("{head}0"),
    ] {
        get_err(
            &web,
            &repo_url("file", "r1/a", bad, "README.md"),
            400,
            "invalid",
        );
    }
    // a well-formed id nobody recorded, and the head of a different task
    get_err(
        &web,
        &repo_url("file", "r1/a", &"1".repeat(40), "README.md"),
        404,
        "not_found",
    );
    get_err(
        &web,
        &repo_url("file", "r1/a", &head_b, "other.txt"),
        404,
        "not_found",
    );
    assert_eq!(
        jstr(
            &get_ok(&web, &repo_url("file", "r1/b", "head", "other.txt")),
            "commit"
        ),
        head_b
    );
    get_err(
        &web,
        &repo_url("file", "r1/b", &head, "README.md"),
        404,
        "not_found",
    );
    // the base commit of the run belongs to every task of it
    assert_eq!(
        jstr(
            &get_ok(&web, &repo_url("file", "r1/b", &base, "README.md")),
            "at"
        ),
        base
    );
    get_err(
        &web,
        &format!("/api/repo/diff?task=r1%2Fa&head={}", "2".repeat(40)),
        404,
        "not_found",
    );
    get_err(&web, "/api/repo/diff?task=r1%2Fa&base=main", 400, "invalid");

    // tasks and parameters
    for task in ["nope", "r9/a", "r1/nope"] {
        get_err(&web, &repo_url("tree", task, "head", ""), 404, "not_found");
    }
    for task in [
        "../a",
        "r1/a/../b",
        "a b",
        "r1//a",
        "r1/",
        "/a",
        "A",
        "a\nb",
    ] {
        let r = web.get(&repo_url("tree", task, "head", ""));
        assert!(
            matches!(r.status, 400 | 404),
            "{task:?}: {} {}",
            r.status,
            r.text()
        );
        assert_workbench_response(&r, task);
    }
    for q in [
        "/api/repo/tree?task=r1%2Fa&ref=main",
        "/api/repo/tree?task=r1%2Fa&repo=%2Ftmp",
        "/api/repo/tree?task=r1%2Fa&worktree=x",
        "/api/repo/tree?task=r1%2Fa&path=a&path=b",
        "/api/repo/tree?commit=head",
        "/api/repo/diff?task=r1%2Fa&context=-1",
        "/api/repo/diff?task=r1%2Fa&context=abc",
    ] {
        let r = web.get(q);
        assert_eq!(r.status, 400, "{q}: {}", r.text());
    }
    // a large context is clamped, not an error
    assert_eq!(
        jnum(
            &get_ok(
                &web,
                &format!("/api/repo/diff?task={}&context=99999", enc("r1/a"))
            ),
            "context"
        ),
        25
    );

    // the surface is read only
    for path in [
        "/api/repo/tree",
        "/api/repo/file",
        "/api/repo/diff",
        "/api/repo/blob",
        "/api/repo/markdown",
    ] {
        let r = web.post_raw(&format!("{path}?task=r1%2Fa"), "{}");
        assert!(
            matches!(r.status, 405 | 415 | 403),
            "POST {path}: {}",
            r.status
        );
        assert_ne!(r.status, 200);
    }
    for path in [
        "/api/repo/write",
        "/api/repo/checkout",
        "/api/repo/exec",
        "/api/repo/refs",
        "/api/repo",
    ] {
        assert_eq!(web.get(path).status, 404, "{path}");
    }
    // none of this changed anything: no events, no new branches, no state
    assert_eq!(web.get("/api/events?limit=500").text(), events_before);
    assert!(!git_branches(&f).iter().any(|b| b == "fleet/r1/integration"));
    assert_eq!(jarr(&web.status(), "approvals").len(), 0);
}

#[test]
fn hostile_repository_content_is_inert_in_every_document() {
    let (_f, web) = workbench_fleet();
    let mut seen = 0usize;
    for url in [
        repo_url("tree", "r1/a", "head", ""),
        repo_url("file", "r1/a", "head", "README.md"),
        repo_url("file", "r1/a", "head", "evil.svg"),
        repo_url("file", "r1/a", "head", "src/up"),
        repo_url("markdown", "r1/a", "head", "README.md"),
        format!("/api/repo/diff?task={}", enc("r1/a")),
    ] {
        let r = web.get(&url);
        assert_eq!(r.status, 200, "{url}: {}", r.text());
        assert_workbench_response(&r, &url);
        assert_eq!(
            r.header("content-type"),
            Some("application/json; charset=utf-8")
        );
        seen += 1;
    }
    assert_eq!(seen, 6);
    // the SVG with script is only ever text
    let svg = web.get(&repo_url("blob", "r1/a", "head", "evil.svg"));
    assert_eq!(svg.status, 415);
    assert!(
        !svg.text().contains("onload"),
        "the content is not echoed into an error"
    );
    // the dashboard's other commands are unaffected: nothing a repository holds can name one
    let st = web.status();
    assert_eq!(jstr(&st, "schema"), "fleet.status/1");
}

#[test]
fn a_worker_cannot_read_the_repository_through_the_workbench() {
    let mut f = Fleet::new();
    let child = f.tmp.script(
        "webreq.sh",
        &format!("#!/bin/bash\n{BASH_REQ}\nPORT=$1\nTOKEN=$2\nreq \"$3\" \"$4\" \"$5\" \"$6\"\n"),
    );
    let worker = fill(
        &format!(
            r#"#!/bin/bash
{BASH_REQ}
if [ "$FLEET_TASK" = a ]; then echo a > a.txt; exit 0; fi
PORT=; TOKEN=
while IFS= read -r l; do
  [[ $l =~ WEBPORT=([0-9]+) ]] && PORT=${{BASH_REMATCH[1]}}
  [[ $l =~ WEBTOKEN=([0-9a-f]+) ]] && TOKEN=${{BASH_REMATCH[1]}}
done < "$FLEET_BRIEF_FILE"
n=0
for p in "/api/repo/tree?task=r1%2Fa" "/api/repo/file?task=r1%2Fa&path=a.txt" "/api/repo/blob?task=r1%2Fa&path=a.txt" "/api/repo/markdown?task=r1%2Fa&path=a.txt" "/api/repo/diff?task=r1%2Fa"; do
  n=$((n+1))
  req GET "$p" "" repo-get-$n.txt
  ( req GET "$p" "" repo-sub-$n.txt )
  bash @CHILD@ "$PORT" "$TOKEN" GET "$p" "" repo-child-$n.txt
done
echo finished > b.txt
"#
        ),
        &[("CHILD", &child)],
    );
    let s = f.tmp.script("w.sh", &worker);
    f.write_cfg(&simple_cfg(&s, "timeout_secs = 90"));
    let info = f.start_web(&[]);
    let web = Web::new(&info);
    let spec = format!("WEBPORT={} WEBTOKEN={}", info.port, info.token);
    f.plan_and_start("r1", 2, &[t("a"), t("b").spec(&spec)]);
    web.wait_task("a", "completed");
    web.wait_task("b", "completed");
    let dir = f.worktree("r1", "b");
    assert!(
        dir.join("b.txt").exists(),
        "the worker script did not finish"
    );
    for n in 1..=5 {
        for kind in ["get", "sub", "child"] {
            let file = format!("repo-{kind}-{n}.txt");
            let text =
                fs::read_to_string(dir.join(&file)).unwrap_or_else(|e| panic!("{file}: {e}"));
            assert!(
                text.starts_with("HTTP/1.1 403"),
                "{file}: the worker was not refused:\n{text}"
            );
            assert!(
                text.contains("\"code\":\"forbidden\"") && text.contains("worker processes"),
                "{file}: {text}"
            );
            for secret in [
                "fleet.repo.",
                "\"entries\"",
                "\"lines\"",
                "\"files\"",
                "a.txt\"",
            ] {
                assert!(!text.contains(secret), "{file} got {secret}:\n{text}");
            }
        }
    }
    // the same request from this process is fine
    let ok = web.get(&repo_url("file", "r1/a", "head", "a.txt"));
    assert_eq!(ok.status, 200, "{}", ok.text());
}

#[test]
fn many_large_diffs_and_files_keep_memory_flat_and_every_cap_holds() {
    let mut f = Fleet::new();
    let worker = f.script(
        "w.sh",
        r##"#!/bin/bash
set -e
mkdir -p .fleet many
# one file of about 24 MiB with 600000 lines, 60 files of 2000 lines, a directory of 3000 files
yes 'a line of ordinary text that is long enough to take some space in the diff' | head -n 400000 > huge.txt
for i in $(seq 1 60); do yes "file $i line text" | head -n 2000 > many/f$i.txt; done
mkdir -p wide
for i in $(seq 1 3000); do : > wide/w$i.txt; done
printf '{"version":1,"status":"done","summary":"big"}' > .fleet/result.json
"##,
    );
    f.write_cfg(&simple_cfg(&worker, "timeout_secs = 120"));
    let info = f.start_web(&[]);
    let web = Web::new(&info);
    f.plan_and_start("r1", 1, &[t("a")]);
    web.wait_task("a", "completed");
    let rss = |web: &Web| -> u64 {
        let st = web.status();
        st.get("daemon")
            .and_then(|d| d.get("rss_kb"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let url = format!("/api/repo/diff?task={}", enc("r1/a"));
    let first = web.get(&url);
    assert_eq!(
        first.status,
        200,
        "{}",
        &first.text()[..first.text().len().min(300)]
    );
    assert!(
        first.body.len() < 8 * 1024 * 1024,
        "{} bytes",
        first.body.len()
    );
    let d = first.json();
    let shown: usize = jarr(&d, "files")
        .iter()
        .flat_map(|f| jarr(f, "hunks"))
        .map(|h| jarr(h, "lines").len())
        .sum();
    assert!(shown <= 30_000, "{shown} lines");
    assert!(
        jarr(&d, "files").iter().any(|f| jbool(f, "truncated")),
        "the caps must show"
    );
    assert!(jarr(&d, "files").len() <= 300);
    let huge = file_of(&d, "huge.txt");
    assert_eq!(jnum(huge, "additions"), 400_000, "totals are the real ones");
    assert!(jbool(huge, "truncated"));
    // the wide directory is cut at its cap and says so
    let wide = get_ok(&web, &repo_url("tree", "r1/a", "head", "wide"));
    assert_eq!(jarr(&wide, "entries").len(), 2000);
    assert!(jbool(&wide, "truncated"));

    // Two identical phases of reads, three at a time. A read builds a document of a few megabytes
    // (bounded by the caps), so memory rises to a plateau during the first phase; the plateau is
    // bounded and the second phase must not raise it: nothing grows with the number of reads.
    let port = info.port;
    let token = info.token.clone();
    let paths: Vec<String> = vec![
        url.clone(),
        format!("{url}&context=25"),
        format!("{url}&context=0"),
        format!("{url}&base=head&head=base"),
        repo_url("tree", "r1/a", "head", "wide"),
        repo_url("tree", "r1/a", "head", "many"),
        repo_url("file", "r1/a", "head", "huge.txt"),
        repo_url("file", "r1/a", "head", "many/f7.txt"),
    ];
    let phase = |rounds: usize| -> usize {
        let workers: Vec<_> = (0..4)
            .map(|w| {
                let paths = paths.clone();
                let token = token.clone();
                std::thread::spawn(move || {
                    let mut busy = 0;
                    for round in 0..rounds {
                        let p = &paths[(round + w) % paths.len()];
                        let r = http(
                            port,
                            "GET",
                            p,
                            &[("Authorization", &format!("Bearer {token}"))],
                            None,
                        );
                        match r.status {
                            200 => assert!(r.body.len() < 8 * 1024 * 1024),
                            // the number of reads that run at once is capped
                            503 => busy += 1,
                            other => {
                                panic!("{p}: {other} {}", &r.text()[..r.text().len().min(200)])
                            }
                        }
                    }
                    busy
                })
            })
            .collect();
        workers.into_iter().map(|h| h.join().unwrap()).sum()
    };
    let start = Instant::now();
    let baseline = rss(&web);
    let busy1 = phase(48);
    let plateau = rss(&web);
    let busy2 = phase(48);
    let after = rss(&web);
    println!(
        "workbench memory: baseline {baseline} kB, after the first phase {plateau} kB, after the second {after} kB ({} reads in {:?}, {} turned away)",
        2 * 4 * 48,
        start.elapsed(),
        busy1 + busy2
    );
    assert!(busy1 + busy2 < 2 * 4 * 48, "every read was refused as busy");
    assert!(
        plateau < baseline + 256 * 1024,
        "the first phase raised the daemon's memory from {baseline} kB to {plateau} kB"
    );
    assert!(
        after < plateau + 32 * 1024,
        "memory grew from {plateau} kB to {after} kB over a second identical phase ({:?}; {busy1} and {busy2} reads were turned away)",
        start.elapsed()
    );
    // the daemon is still answering everything else
    assert_eq!(web.get("/api/health").status, 200);
}
