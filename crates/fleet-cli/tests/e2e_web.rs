//! End-to-end tests of the web dashboard: the real `fleet daemon --web` binary, scripted worker
//! processes and a small raw-TCP HTTP client (no curl, no crates, one request per connection).
//!
//! The web token only ever exists on the daemon's stderr, which these tests drain into memory the
//! way an operator's terminal would show it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod common;

use common::*;
use fleet_domain::value::Value;
use fleet_infra::testutil::wait_until;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
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
        let st = self.wait(&format!("{task} to be {wanted}"), |st| {
            find_task(st, "r1", task).is_some_and(|t| jstr(t, "status") == wanted)
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
        let mut s = connect(port);
        let bearer = format!("Bearer {token}");
        s.write_all(
            request_bytes(
                port,
                "GET",
                path,
                &[
                    ("Authorization", bearer.as_str()),
                    ("Accept", "text/event-stream"),
                ],
                None,
            )
            .as_bytes(),
        )
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
    let cookies = login.all("set-cookie");
    assert_eq!(cookies.len(), 1, "{:?}", login.headers);
    let set_cookie = cookies[0];
    for attr in ["HttpOnly", "SameSite=Strict", "Path=/"] {
        assert!(
            set_cookie.contains(attr),
            "{attr} missing from {set_cookie}"
        );
    }
    let session_id = set_cookie
        .strip_prefix("fleet_session=")
        .and_then(|r| r.split(';').next())
        .unwrap_or_else(|| panic!("unexpected cookie {set_cookie}"))
        .to_string();
    assert!(!session_id.is_empty());
    assert_ne!(session_id, info.token);
    assert!(
        !set_cookie.contains(&info.token),
        "the cookie carries the token"
    );
    let session = login.json();
    assert_eq!(jstr(&session, "schema"), "fleet.web.session/1");
    let csrf = jstr(&session, "csrf").to_string();
    assert!(!csrf.is_empty() && csrf != session_id && csrf != info.token);
    assert!(!login.text().contains(&info.token));

    // the cookie reads
    let cookie = format!("fleet_session={session_id}");
    let by_cookie = http(port, "GET", "/api/status", &[("Cookie", &cookie)], None);
    assert_eq!(by_cookie.status, 200, "{}", by_cookie.text());
    assert_eq!(jstr(&by_cookie.json(), "schema"), "fleet.status/1");
    let sess = http(port, "GET", "/api/session", &[("Cookie", &cookie)], None);
    assert_eq!(jstr(&sess.json(), "csrf"), csrf);

    // a cookie POST needs the CSRF token and the dashboard's own Origin
    let ask = Value::object([("id", Value::str("g99")), ("answer", Value::str("x"))]).canonical();
    let no_csrf = http(
        port,
        "POST",
        "/api/gate/resolve",
        &[("Cookie", &cookie), ("Origin", &origin)],
        Some(&ask),
    );
    assert_eq!(no_csrf.status, 403, "{}", no_csrf.text());
    assert_eq!(no_csrf.code(), "forbidden");
    let wrong_csrf = http(
        port,
        "POST",
        "/api/gate/resolve",
        &[
            ("Cookie", &cookie),
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
        &[("Cookie", &cookie), ("X-CSRF-Token", &csrf)],
        Some(&ask),
    );
    assert_eq!(no_origin.status, 403, "{}", no_origin.text());
    let accepted = http(
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

    // logout ends the session
    let out = http(
        port,
        "POST",
        "/api/logout",
        &[
            ("Cookie", &cookie),
            ("Origin", &origin),
            ("X-CSRF-Token", &csrf),
        ],
        Some("{}"),
    );
    assert_eq!(out.status, 200, "{}", out.text());
    assert!(out
        .all("set-cookie")
        .iter()
        .any(|c| c.contains("fleet_session=;") && c.contains("Max-Age=0")));
    let after = http(port, "GET", "/api/status", &[("Cookie", &cookie)], None);
    assert_eq!(after.status, 401, "{}", after.text());
    // the token still works
    assert_eq!(web.get("/api/status").status, 200);
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
    let (f, info) = web_fleet("echo first-line\nread line\necho \"got:$line\"\n");
    let web = Web::new(&info);
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_live("r1", "t");

    let mut sse = Sse::open(web.port, "/api/stream?task=r1/t", &web.token);
    let first = sse.output_until(Duration::from_secs(15), |t| t.contains("first-line"));
    assert!(!first.contains("got:"), "{first:?}");

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
