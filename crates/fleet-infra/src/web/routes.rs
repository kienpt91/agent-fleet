//! Routing, request checks and the JSON API of the dashboard.
//!
//! Every `/api` route maps onto a command the control socket already has (`status`, `events`,
//! `task.tail`, `task.send`, `gate.resolve`, `approval.decide`, `merge`), so validation, the
//! digest binding of approvals and the approval flow of merges are the daemon's own, not a second
//! implementation.

use super::auth::{self, ClientKey, Presented, Verdict, SESSION_HEADER};
use super::http::{Method, Request, Response};
use super::{assets, host_allowed, repo, url_host, Shared};
use crate::eventlog::VerifyReport;
use crate::json;
use crate::statedir::tokens_equal;
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::ids::is_invisible;
use fleet_domain::limits::{MAX_ANSWER, MAX_SEND_BYTES};
use fleet_domain::value::Value;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) enum Outcome {
    Reply(Response),
    Stream(StreamPlan),
}

/// A validated request for an event stream. The slot keeps the stream counted while it lives.
pub(crate) struct StreamPlan {
    /// The session the stream was opened with; the stream ends when it does.
    session: Option<String>,
    task: String,
    since: Option<u64>,
    first: Value,
    _slot: StreamSlot,
}

struct StreamSlot(Arc<Shared>);

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.0.streams_active.fetch_sub(1, Ordering::AcqRel);
    }
}

// ---- responses ----

fn schema_error(code: ErrorCode, message: &str, subject: Option<&str>) -> String {
    let doc = Value::object([
        ("schema", Value::str("fleet.error/1")),
        (
            "error",
            Value::object([
                ("code", Value::str(code.as_str())),
                ("message", Value::str(message)),
                ("subject", subject.map_or(Value::Null, Value::str)),
            ]),
        ),
    ]);
    json_text(&doc)
}

/// Compact JSON with `<`, `>` and `&` written as escapes. A JSON string never needs them raw, and
/// without them the document cannot be mistaken for markup if some client ever embeds it.
pub(crate) fn json_text(v: &Value) -> String {
    let line = json::to_line(v);
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        match c {
            '<' | '>' | '&' => push_u_escape(&mut out, c),
            // Controls and characters that change how text is displayed (bidi overrides,
            // zero-width characters) never travel raw: the page shows them as plain text anyway,
            // and a log or a terminal that prints the document cannot be steered by them.
            c if c.is_control() || fleet_domain::ids::is_invisible(c) => push_u_escape(&mut out, c),
            other => out.push(other),
        }
    }
    out
}

/// `\uXXXX` (a surrogate pair above the basic plane), valid inside a JSON string.
fn push_u_escape(out: &mut String, c: char) {
    let mut units = [0u16; 2];
    for unit in c.encode_utf16(&mut units) {
        out.push_str(&format!("\\u{unit:04x}"));
    }
}

pub(crate) fn error_response(status: u16, code: ErrorCode, message: &str) -> Response {
    Response::json(status, schema_error(code, message, None))
}

fn status_for(code: ErrorCode) -> u16 {
    match code {
        ErrorCode::Usage | ErrorCode::Invalid => 400,
        ErrorCode::NotFound => 404,
        ErrorCode::ApprovalRequired | ErrorCode::Conflict => 409,
        ErrorCode::Limit => 413,
        ErrorCode::Forbidden => 403,
        ErrorCode::Unavailable | ErrorCode::DaemonDown | ErrorCode::Unhealthy => 503,
        ErrorCode::Timeout => 504,
        ErrorCode::Internal | ErrorCode::Tampered => 500,
    }
}

pub(super) fn fleet_error_response(e: &FleetError) -> Response {
    Response::json(
        status_for(e.code),
        schema_error(e.code, &e.message, e.subject.as_deref()),
    )
}

pub(super) fn ok_value(v: &Value) -> Response {
    let text = json_text(v);
    if text.len() > crate::protocol::MAX_RESPONSE_BYTES {
        return error_response(500, ErrorCode::Limit, "the reply is too large");
    }
    Response::json(200, text)
}

pub(crate) fn log_document(result: Result<VerifyReport>) -> Value {
    let (ok, records, head, error) = match result {
        Ok(r) => (
            true,
            Value::int(r.records),
            Value::str(r.head_hash),
            Value::Null,
        ),
        Err(e) => (false, Value::Null, Value::Null, Value::str(e.message)),
    };
    Value::object([
        ("schema", Value::str("fleet.web.log/1")),
        ("ok", Value::Bool(ok)),
        ("records", records),
        ("head_hash", head),
        ("error", error),
    ])
}

// ---- routing ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Session,
    Login,
    Logout,
    Status,
    Events,
    Log,
    Health,
    Tail,
    Goals,
    Goal,
    Remaining,
    VerifyStatus,
    Stream,
    GateResolve,
    ApprovalDecide,
    Send,
    Merge,
    RepoTree,
    RepoFile,
    RepoBlob,
    RepoMarkdown,
    RepoDiff,
}

fn route_for(path: &str) -> Option<(Route, Method)> {
    Some(match path {
        "/api/session" => (Route::Session, Method::Get),
        "/api/login" => (Route::Login, Method::Post),
        "/api/logout" => (Route::Logout, Method::Post),
        "/api/status" => (Route::Status, Method::Get),
        "/api/events" => (Route::Events, Method::Get),
        "/api/log" => (Route::Log, Method::Get),
        "/api/health" => (Route::Health, Method::Get),
        "/api/tail" => (Route::Tail, Method::Get),
        "/api/goals" => (Route::Goals, Method::Get),
        "/api/goal" => (Route::Goal, Method::Get),
        "/api/remaining" => (Route::Remaining, Method::Get),
        "/api/verify" => (Route::VerifyStatus, Method::Get),
        "/api/stream" => (Route::Stream, Method::Get),
        "/api/gate/resolve" => (Route::GateResolve, Method::Post),
        "/api/approval/decide" => (Route::ApprovalDecide, Method::Post),
        "/api/send" => (Route::Send, Method::Post),
        "/api/merge" => (Route::Merge, Method::Post),
        "/api/repo/tree" => (Route::RepoTree, Method::Get),
        "/api/repo/file" => (Route::RepoFile, Method::Get),
        "/api/repo/blob" => (Route::RepoBlob, Method::Get),
        "/api/repo/markdown" => (Route::RepoMarkdown, Method::Get),
        "/api/repo/diff" => (Route::RepoDiff, Method::Get),
        _ => return None,
    })
}

fn refuse(status: u16, code: ErrorCode, message: &str) -> Outcome {
    Outcome::Reply(error_response(status, code, message))
}

/// Handles one parsed request.
pub(crate) fn handle(
    shared: &Arc<Shared>,
    req: &Request,
    client: ClientKey,
    now: Instant,
) -> Outcome {
    // 1. Host: the only defence against DNS rebinding, so it runs before anything else.
    let Some(host) = req.header("host") else {
        return refuse(421, ErrorCode::Forbidden, "a Host header is required");
    };
    if !host_allowed(host, shared.bound, shared.allow_remote) {
        return refuse(
            421,
            ErrorCode::Forbidden,
            "this Host is not served here (use the address the daemon printed)",
        );
    }

    // 2. static files
    if !req.path.starts_with("/api/") {
        return match assets::lookup(&req.path) {
            None => refuse(404, ErrorCode::NotFound, "not found"),
            Some(_) if req.method != Method::Get => Outcome::Reply(
                error_response(405, ErrorCode::Usage, "method not allowed")
                    .with_header("Allow", "GET"),
            ),
            Some((content_type, body)) => {
                Outcome::Reply(Response::new(200, content_type, body.as_bytes().to_vec()))
            }
        };
    }

    // 3. the API
    let Some((route, method)) = route_for(&req.path) else {
        return refuse(404, ErrorCode::NotFound, "not found");
    };
    if req.method != method {
        return Outcome::Reply(
            error_response(405, ErrorCode::Usage, "method not allowed")
                .with_header("Allow", method.as_str()),
        );
    }
    let site = req.header("sec-fetch-site");
    if let Some(site) = site {
        // A page of this origin is `same-origin`; typing an address is `none` (reads only).
        let fine = site == "same-origin" || (site == "none" && method == Method::Get);
        if !fine {
            return refuse(
                403,
                ErrorCode::Forbidden,
                "cross-site requests to the dashboard are refused",
            );
        }
    }

    if route == Route::Login {
        return login(shared, req, host, site.is_some(), client, now);
    }

    // authentication
    // Credentials travel in headers only. A `Cookie` header is never read: cookies are not
    // isolated by port, so a browser sends them to every server on this host.
    let presented = match (req.header("authorization"), req.header(SESSION_HEADER)) {
        (Some(h), _) => Presented::Bearer(auth::bearer_token(h).unwrap_or("")),
        (None, Some(s)) => auth::session_id(s).map_or(Presented::Nothing, Presented::Session),
        (None, None) => Presented::Nothing,
    };
    let verdict = shared.auth.check(presented, client, now);
    let csrf = match &verdict {
        Verdict::Bearer => None,
        Verdict::Session { csrf } => Some(csrf.as_str()),
        Verdict::Missing => {
            return refuse(
                401,
                ErrorCode::Forbidden,
                "not logged in: open the login URL the daemon printed, or send the token as a bearer token",
            )
        }
        Verdict::Rejected => {
            return refuse(
                401,
                ErrorCode::Forbidden,
                "the credentials were not accepted (a session that ended needs a new login)",
            )
        }
        Verdict::Locked { retry_after_secs } => return locked(*retry_after_secs),
    };

    if method == Method::Post {
        if let Err(r) = check_state_change(req, host, csrf) {
            return Outcome::Reply(r);
        }
    }

    match run_route(shared, route, req, &verdict) {
        Ok(Reply::Response(r)) => Outcome::Reply(r),
        Ok(Reply::Stream(plan)) => Outcome::Stream(plan),
        Err(r) => Outcome::Reply(r),
    }
}

fn locked(retry_after_secs: u64) -> Outcome {
    Outcome::Reply(
        error_response(
            429,
            ErrorCode::Limit,
            "too many failed attempts; authentication is locked for a while",
        )
        .with_header("Retry-After", &retry_after_secs.to_string()),
    )
}

/// What a state-changing request must show: it came from the dashboard's own origin, it is JSON
/// (which a cross-site form cannot send without a preflight) and, when it is authenticated by the
/// session, it carries the session's CSRF token.
fn check_state_change(
    req: &Request,
    host: &str,
    csrf: Option<&str>,
) -> std::result::Result<(), Response> {
    let forbidden = |m: &str| Err(error_response(403, ErrorCode::Forbidden, m));
    let expected = format!("http://{host}");
    match req.header("origin") {
        Some(origin) if origin.eq_ignore_ascii_case(&expected) => {}
        Some(_) => return forbidden("the Origin of this request is not the dashboard"),
        // Browsers always send Origin on a POST; a request without one is a script with the
        // token in hand, which is fine, but never a browser session.
        None if csrf.is_none() && req.header("sec-fetch-site").is_none() => {}
        None => return forbidden("the Origin header is required"),
    }
    if let Some(expected_csrf) = csrf {
        match req.header("x-csrf-token") {
            Some(t) if tokens_equal(t, expected_csrf) => {}
            _ => return forbidden("the CSRF token is missing or wrong"),
        }
    }
    if !is_json(req) {
        return Err(error_response(
            415,
            ErrorCode::Invalid,
            "the body must be application/json",
        ));
    }
    Ok(())
}

fn is_json(req: &Request) -> bool {
    let Some(ct) = req.header("content-type") else {
        return false;
    };
    let mut parts = ct.split(';').map(str::trim);
    if !parts
        .next()
        .is_some_and(|m| m.eq_ignore_ascii_case("application/json"))
    {
        return false;
    }
    parts.all(|p| {
        let p = p.to_ascii_lowercase();
        p == "charset=utf-8" || p == "charset=\"utf-8\""
    })
}

// ---- body and query validation ----

enum Kind {
    Str(usize),
    Bool,
    Hex64,
}

struct Field {
    key: &'static str,
    kind: Kind,
    required: bool,
}

const fn req_str(key: &'static str, max: usize) -> Field {
    Field {
        key,
        kind: Kind::Str(max),
        required: true,
    }
}

/// A JSON object with exactly the known keys, each of the right type and size. Nothing else is
/// accepted: unknown keys, nulls, numbers where text belongs and nested values are errors.
fn parse_body(req: &Request, fields: &[Field]) -> std::result::Result<Value, Response> {
    let bad = |m: String| Err(error_response(400, ErrorCode::Invalid, &m));
    let doc = match json::parse_value(&req.body, req.body.len().max(2)) {
        Ok(v) => v,
        Err(e) => return bad(format!("the body is not valid JSON: {e}")),
    };
    let Some(map) = doc.as_object() else {
        return bad("the body must be a JSON object".to_string());
    };
    for key in map.keys() {
        if !fields.iter().any(|f| f.key == key) {
            return bad(format!("unknown field {}", fleet_domain::ids::show(key)));
        }
    }
    for f in fields {
        let Some(v) = map.get(f.key) else {
            if f.required {
                return bad(format!("`{}` is required", f.key));
            }
            continue;
        };
        let ok = match (&f.kind, v) {
            (Kind::Str(max), Value::Str(s)) => !s.is_empty() && s.len() <= *max,
            (Kind::Bool, Value::Bool(_)) => true,
            (Kind::Hex64, Value::Str(s)) => {
                s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
            }
            _ => false,
        };
        if !ok {
            let what = match f.kind {
                Kind::Str(max) => format!("a non-empty string of at most {max} bytes"),
                Kind::Bool => "true or false".to_string(),
                Kind::Hex64 => "a 64 character hexadecimal digest".to_string(),
            };
            return bad(format!("`{}` must be {what}", f.key));
        }
    }
    Ok(doc)
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            if !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).ok().filter(|t| !t.contains('\0'))
}

/// `a=1&b=x%2Fy` into a map. Repeated keys, keys that are not in `allowed`, bad escapes and
/// values longer than 256 bytes are errors.
pub(super) fn parse_query(
    query: &str,
    allowed: &[&str],
) -> std::result::Result<BTreeMap<String, String>, Response> {
    parse_query_limited(query, allowed, 256)
}

/// [`parse_query`] with another longest value (a repository path may be longer than an id).
pub(super) fn parse_query_limited(
    query: &str,
    allowed: &[&str],
    max_value: usize,
) -> std::result::Result<BTreeMap<String, String>, Response> {
    let bad = |m: &str| Err(error_response(400, ErrorCode::Invalid, m));
    let mut out = BTreeMap::new();
    if query.is_empty() {
        return Ok(out);
    }
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let (Some(k), Some(v)) = (percent_decode(k), percent_decode(v)) else {
            return bad("malformed query string");
        };
        if !allowed.contains(&k.as_str()) {
            return bad("unknown query parameter");
        }
        if v.len() > max_value || out.insert(k, v).is_some() {
            return bad("malformed query string");
        }
    }
    Ok(out)
}

pub(super) fn query_u64(
    q: &BTreeMap<String, String>,
    key: &str,
) -> std::result::Result<Option<u64>, Response> {
    match q.get(key) {
        None => Ok(None),
        Some(v) if !v.is_empty() && v.len() <= 20 && v.bytes().all(|b| b.is_ascii_digit()) => {
            v.parse().map(Some).map_err(|_| {
                error_response(400, ErrorCode::Invalid, &format!("`{key}` is not a number"))
            })
        }
        Some(_) => Err(error_response(
            400,
            ErrorCode::Invalid,
            &format!("`{key}` must be a non-negative integer"),
        )),
    }
}

// ---- the routes ----

enum Reply {
    Response(Response),
    Stream(StreamPlan),
}

fn call(shared: &Shared, cmd: &str, args: Value) -> std::result::Result<Value, Response> {
    shared
        .backend
        .call(cmd, args)
        .map_err(|e| fleet_error_response(&e))
}

fn pass(map: Vec<(&'static str, Value)>) -> Value {
    Value::object(map)
}

/// `session` is given only in the reply to a login: the id is a credential and is not echoed.
fn session_document(shared: &Shared, session: Option<&str>, csrf: Option<&str>) -> Value {
    Value::object([
        ("schema", Value::str("fleet.web.session/1")),
        ("session", session.map_or(Value::Null, Value::str)),
        ("csrf", csrf.map_or(Value::Null, Value::str)),
        ("version", Value::str(crate::daemon::VERSION)),
        ("bind", Value::str(url_host(shared.bound))),
    ])
}

fn login(
    shared: &Arc<Shared>,
    req: &Request,
    host: &str,
    browser: bool,
    client: ClientKey,
    now: Instant,
) -> Outcome {
    // A browser sends Origin on every POST; a script with the token may leave it out.
    let origin = req.header("origin");
    let expected = format!("http://{host}");
    match origin {
        Some(o) if o.eq_ignore_ascii_case(&expected) => {}
        Some(_) => {
            return refuse(
                403,
                ErrorCode::Forbidden,
                "the Origin of this request is not the dashboard",
            )
        }
        None if browser => {
            return refuse(403, ErrorCode::Forbidden, "the Origin header is required")
        }
        None => {}
    }
    if !is_json(req) {
        return refuse(415, ErrorCode::Invalid, "the body must be application/json");
    }
    let body = match parse_body(req, &[req_str("token", 128)]) {
        Ok(b) => b,
        Err(r) => return Outcome::Reply(r),
    };
    let token = body.get("token").and_then(Value::as_str).unwrap_or("");
    match shared.auth.login(token, client, now) {
        Ok((id, csrf)) => {
            Outcome::Reply(ok_value(&session_document(shared, Some(&id), Some(&csrf))))
        }
        Err(Verdict::Locked { retry_after_secs }) => locked(retry_after_secs),
        Err(_) => refuse(401, ErrorCode::Forbidden, "the token was not accepted"),
    }
}

fn run_route(
    shared: &Arc<Shared>,
    route: Route,
    req: &Request,
    verdict: &Verdict,
) -> std::result::Result<Reply, Response> {
    let respond = |v: Value| Ok(Reply::Response(ok_value(&v)));
    match route {
        Route::Login => Err(error_response(500, ErrorCode::Internal, "unreachable")),
        Route::Session => {
            let csrf = match verdict {
                Verdict::Session { csrf } => Some(csrf.as_str()),
                _ => None,
            };
            respond(session_document(shared, None, csrf))
        }
        Route::Logout => {
            parse_body(req, &[])?;
            if let Some(id) = req.header(SESSION_HEADER).and_then(auth::session_id) {
                shared.auth.logout(id);
            }
            respond(Value::object([
                ("schema", Value::str("fleet.web.logout/1")),
                ("ok", Value::Bool(true)),
            ]))
        }
        Route::Status => {
            let q = parse_query(&req.query, &["run"])?;
            let args = match q.get("run") {
                Some(run) => pass(vec![("run", Value::str(run))]),
                None => Value::Null,
            };
            respond(call(shared, "status", args)?)
        }
        Route::Events => {
            let q = parse_query(&req.query, &["limit"])?;
            let args = match query_u64(&q, "limit")? {
                Some(n) => pass(vec![("limit", Value::int(n))]),
                None => Value::Null,
            };
            respond(call(shared, "events", args)?)
        }
        Route::Log => respond(shared.cached_log()),
        Route::Health => respond(shared.cached_health()),
        Route::Tail => {
            let q = parse_query(&req.query, &["task", "since", "max"])?;
            let task = q
                .get("task")
                .filter(|t| !t.is_empty())
                .ok_or_else(|| error_response(400, ErrorCode::Invalid, "`task` is required"))?;
            let mut args = vec![("task", Value::str(task))];
            if let Some(n) = query_u64(&q, "since")? {
                args.push(("since", Value::int(n)));
            }
            if let Some(n) = query_u64(&q, "max")? {
                args.push(("max", Value::int(n)));
            }
            respond(call(shared, "task.tail", pass(args))?)
        }
        // The goal ledger and verification, read only. Adding goals, attaching contracts and
        // running `verify` (which starts processes) stay on the control socket.
        Route::Goals => {
            parse_query(&req.query, &[])?;
            respond(call(shared, "goal.list", Value::Null)?)
        }
        Route::Goal => {
            let q = parse_query(&req.query, &["id"])?;
            let id = q
                .get("id")
                .filter(|t| !t.is_empty())
                .ok_or_else(|| error_response(400, ErrorCode::Invalid, "`id` is required"))?;
            respond(call(
                shared,
                "goal.show",
                pass(vec![("goal", Value::str(id))]),
            )?)
        }
        Route::Remaining => {
            parse_query(&req.query, &[])?;
            respond(call(shared, "goal.remaining", Value::Null)?)
        }
        Route::VerifyStatus => {
            let q = parse_query(&req.query, &["target", "task", "ticket"])?;
            let target = q
                .get("target")
                .filter(|t| !t.is_empty())
                .ok_or_else(|| error_response(400, ErrorCode::Invalid, "`target` is required"))?;
            let mut args = vec![("target", Value::str(target))];
            if let Some(task) = q.get("task").filter(|t| !t.is_empty()) {
                args.push(("task", Value::str(task)));
            }
            if let Some(n) = query_u64(&q, "ticket")? {
                args.push(("ticket", Value::int(n)));
            }
            respond(call(shared, "verify.status", pass(args))?)
        }
        Route::Stream => open_stream(shared, req, verdict).map(Reply::Stream),
        Route::GateResolve => {
            let body = parse_body(req, &[req_str("id", 40), req_str("answer", MAX_ANSWER)])?;
            respond(call(shared, "gate.resolve", body)?)
        }
        Route::ApprovalDecide => {
            let body = parse_body(
                req,
                &[
                    req_str("id", 40),
                    Field {
                        key: "approve",
                        kind: Kind::Bool,
                        required: true,
                    },
                    Field {
                        key: "digest",
                        kind: Kind::Hex64,
                        required: true,
                    },
                ],
            )?;
            respond(call(shared, "approval.decide", body)?)
        }
        Route::Send => {
            let body = parse_body(
                req,
                &[
                    req_str("task", 128),
                    req_str("text", MAX_SEND_BYTES),
                    Field {
                        key: "newline",
                        kind: Kind::Bool,
                        required: false,
                    },
                ],
            )?;
            // What the operator types goes to a terminal program as keystrokes. Escape sequences,
            // carriage returns, backspaces and invisible characters are not typing: they would
            // let pasted text drive the terminal or hide what it does.
            let text = body.get("text").and_then(Value::as_str).unwrap_or("");
            if text
                .chars()
                .any(|c| (c.is_control() && c != '\n' && c != '\t') || is_invisible(c))
            {
                return Err(error_response(
                    400,
                    ErrorCode::Invalid,
                    "`text` contains control or invisible characters (only tab and line breaks are allowed)",
                ));
            }
            respond(call(shared, "task.send", body)?)
        }
        Route::Merge => {
            let body = parse_body(req, &[req_str("task", 128)])?;
            respond(call(shared, "merge", body)?)
        }
        Route::RepoTree => repo::handle(shared, repo::RepoRoute::Tree, req).map(Reply::Response),
        Route::RepoFile => repo::handle(shared, repo::RepoRoute::File, req).map(Reply::Response),
        Route::RepoBlob => repo::handle(shared, repo::RepoRoute::Blob, req).map(Reply::Response),
        Route::RepoMarkdown => {
            repo::handle(shared, repo::RepoRoute::Markdown, req).map(Reply::Response)
        }
        Route::RepoDiff => repo::handle(shared, repo::RepoRoute::Diff, req).map(Reply::Response),
    }
}

// ---- server-sent events ----

fn open_stream(
    shared: &Arc<Shared>,
    req: &Request,
    verdict: &Verdict,
) -> std::result::Result<StreamPlan, Response> {
    let q = parse_query(&req.query, &["task", "since"])?;
    let task = q
        .get("task")
        .filter(|t| !t.is_empty())
        .ok_or_else(|| error_response(400, ErrorCode::Invalid, "`task` is required"))?
        .clone();
    // On reconnect the browser repeats the URL and adds the id of the last event it saw.
    let since = match req.header("last-event-id") {
        Some(v) if !v.is_empty() && v.len() <= 20 && v.bytes().all(|b| b.is_ascii_digit()) => {
            v.parse().ok()
        }
        Some(_) => {
            return Err(error_response(
                400,
                ErrorCode::Invalid,
                "malformed Last-Event-ID",
            ))
        }
        None => query_u64(&q, "since")?,
    };
    let prior = shared.streams_active.fetch_add(1, Ordering::AcqRel);
    let slot = StreamSlot(Arc::clone(shared));
    if prior >= shared.limits.max_streams {
        return Err(error_response(
            503,
            ErrorCode::Limit,
            "too many open output streams",
        ));
    }
    let mut args = vec![
        ("task", Value::str(&task)),
        ("max", Value::int(shared.limits.tail_chunk as u64)),
    ];
    if let Some(n) = since {
        args.push(("since", Value::int(n)));
    }
    let first = call(shared, "task.tail", pass(args))?;
    let session = match verdict {
        Verdict::Session { .. } => req
            .header(SESSION_HEADER)
            .and_then(auth::session_id)
            .map(str::to_string),
        _ => None,
    };
    Ok(StreamPlan {
        session,
        task,
        since,
        first,
        _slot: slot,
    })
}

fn sse_event(
    w: &mut impl Write,
    name: &str,
    id: Option<u64>,
    data: &Value,
) -> std::io::Result<usize> {
    let mut text = String::new();
    if let Some(id) = id {
        text.push_str(&format!("id: {id}\n"));
    }
    text.push_str(&format!("event: {name}\ndata: {}\n\n", json_text(data)));
    w.write_all(text.as_bytes())?;
    w.flush()?;
    Ok(text.len())
}

fn output_event(text: &str, next: u64, lost: bool) -> Value {
    Value::object([
        ("text", Value::str(text)),
        ("next", Value::int(next)),
        ("lost", Value::Bool(lost)),
    ])
}

/// True when the client closed its end (or sent something it must not).
fn client_gone(stream: &mut TcpStream) -> bool {
    if stream
        .set_read_timeout(Some(Duration::from_millis(1)))
        .is_err()
    {
        return true;
    }
    let mut b = [0u8; 1];
    match stream.read(&mut b) {
        // End of stream, or bytes the client has no business sending on an event stream.
        Ok(0) | Ok(1..) => true,
        Err(e) => !matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::Interrupted
        ),
    }
}

/// Streams a task's output until the session ends, the client leaves, the stream's lifetime or
/// byte budget is used up, or the daemon stops. Everything is bounded; a slow client is dropped by
/// the write timeout.
pub(crate) fn stream_task(shared: &Shared, stream: &mut TcpStream, plan: &StreamPlan) {
    let head = Response::new(200, "text/event-stream; charset=utf-8", Vec::new())
        .with_header("X-Accel-Buffering", "no");
    if head.write_stream_head(stream).is_err() {
        return;
    }
    let limits = &shared.limits;
    let started = Instant::now();
    let mut sent = 0usize;
    let mut last_write = Instant::now();
    let mut next = plan.since.unwrap_or(0);
    let mut reply = plan.first.clone();
    loop {
        let live = reply.get("live").and_then(Value::as_bool).unwrap_or(false);
        let text = reply.get("text").and_then(Value::as_str).unwrap_or("");
        let lost = reply.get("lost").and_then(Value::as_bool).unwrap_or(false);
        let server_next = reply.get("next").and_then(Value::as_u64).unwrap_or(next);
        // A finished task has no offsets: its stored output is sent once, and not again to a
        // client that reconnects with an offset.
        let skip = !live && plan.since.is_some();
        if (!text.is_empty() && !skip) || lost {
            let shown = if skip { "" } else { text };
            match sse_event(
                stream,
                "output",
                Some(server_next),
                &output_event(shown, server_next, lost),
            ) {
                Ok(n) => {
                    sent = sent.saturating_add(n);
                    last_write = Instant::now();
                }
                Err(_) => return,
            }
        }
        if live {
            next = server_next;
        }
        if !live {
            let _ = sse_event(
                stream,
                "end",
                None,
                &Value::object([("reason", Value::str("the worker has finished"))]),
            );
            return;
        }
        // A stream is authenticated when it opens; it must not outlive its session.
        if let Some(id) = &plan.session {
            if !shared.auth.session_alive(id, Instant::now()) {
                let _ = sse_event(
                    stream,
                    "end",
                    None,
                    &Value::object([(
                        "reason",
                        Value::str("your dashboard login has ended; log in again"),
                    )]),
                );
                return;
            }
        }
        if shared.stop.load(Ordering::Acquire)
            || started.elapsed() >= limits.stream_lifetime
            || sent >= limits.stream_bytes
        {
            let _ = sse_event(
                stream,
                "limit",
                None,
                &Value::object([("next", Value::int(next))]),
            );
            return;
        }
        // Catch up without pausing when the last read was full.
        if text.len() < limits.tail_chunk {
            let wake = Instant::now() + limits.stream_poll;
            while Instant::now() < wake {
                if shared.stop.load(Ordering::Acquire) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20).min(limits.stream_poll));
            }
        }
        if client_gone(stream) {
            return;
        }
        if last_write.elapsed() >= limits.keepalive {
            if stream
                .write_all(b": keep-alive\n\n")
                .and_then(|()| stream.flush())
                .is_err()
            {
                return;
            }
            last_write = Instant::now();
        }
        let args = Value::object([
            ("task", Value::str(&plan.task)),
            ("since", Value::int(next)),
            ("max", Value::int(limits.tail_chunk as u64)),
        ]);
        reply = match shared.backend.call("task.tail", args) {
            Ok(v) => v,
            Err(e) => {
                let _ = sse_event(
                    stream,
                    "end",
                    None,
                    &Value::object([("reason", Value::str(e.message))]),
                );
                return;
            }
        };
    }
}
