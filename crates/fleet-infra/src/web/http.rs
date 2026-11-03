//! A minimal HTTP/1.1 reader and writer on `std::net`, just enough for the local dashboard.
//!
//! Deliberately small: one request per connection (every response says `Connection: close`, so
//! there is no keep-alive, no pipelining and no request smuggling surface), no chunked bodies, no
//! `Expect`, no upgrades. Every size is bounded before anything is buffered, the whole request has
//! one deadline (not a timeout per read, so a client that trickles bytes cannot hold a slot) and
//! malformed input is answered with a status code, never a panic.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::{Duration, Instant};

/// Everything that is bounded in the reader. The defaults are what the daemon uses; tests shrink
/// them to exercise the bounds quickly.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Longest request target (path and query).
    pub max_target: usize,
    /// Request line plus headers, including the blank line.
    pub max_head: usize,
    pub max_headers: usize,
    pub max_body: usize,
    /// The whole request (head and body) must arrive within this long of the accept.
    pub request_deadline: Duration,
    /// The first byte must arrive within this long of the accept: a connection that sends nothing
    /// holds a slot and a thread for no more than this.
    pub first_byte_deadline: Duration,
    /// A client that does not read its response is dropped after this long (per write).
    pub write_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            // a repository path of 1024 bytes, percent-encoded, plus the other parameters
            max_target: 4096,
            max_head: 8 * 1024,
            max_headers: 40,
            max_body: 16 * 1024,
            request_deadline: Duration::from_secs(10),
            first_byte_deadline: Duration::from_secs(3),
            write_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }
}

/// A parsed request. Header names are lower-case; values are visible ASCII.
#[derive(Clone, Debug)]
pub struct Request {
    pub method: Method,
    /// The path without the query, exactly as sent (never percent-decoded).
    pub path: String,
    /// The raw query string (without `?`), empty when there is none.
    pub query: String,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    #[cfg(test)]
    pub fn for_test(method: Method, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Request {
        Request {
            method,
            path: path.to_string(),
            query: String::new(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_ascii_lowercase(), (*v).to_string()))
                .collect(),
            body: body.to_vec(),
        }
    }
}

/// Why a request was not accepted, with the status it is answered with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpError {
    pub status: u16,
    pub message: &'static str,
}

impl HttpError {
    fn new(status: u16, message: &'static str) -> HttpError {
        HttpError { status, message }
    }
}

/// Headers that must appear at most once: a second copy is how requests get smuggled or how a
/// proxy and a server end up disagreeing about who the caller is.
const SINGLETON_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "content-type",
    "authorization",
    "x-fleet-session",
    "origin",
    "sec-fetch-site",
    "x-csrf-token",
    "last-event-id",
    "transfer-encoding",
    "expect",
];

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// One read under the request deadline: the timeout shrinks as the deadline approaches.
fn read_until_deadline(
    stream: &mut TcpStream,
    buf: &mut [u8],
    deadline: Instant,
) -> Result<usize, HttpError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(HttpError::new(408, "the request was not complete in time"));
    }
    if stream.set_read_timeout(Some(remaining)).is_err() {
        return Err(HttpError::new(400, "the connection cannot be read"));
    }
    match stream.read(buf) {
        Ok(n) => Ok(n),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            Err(HttpError::new(408, "the request was not complete in time"))
        }
        Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(usize::MAX),
        Err(_) => Err(HttpError::new(400, "the connection failed")),
    }
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Reads one request. `accepted` is when the connection was accepted: the first byte is due
/// `first_byte_deadline` after it and the whole request `request_deadline` after it.
pub fn read_request(
    stream: &mut TcpStream,
    limits: &Limits,
    accepted: Instant,
) -> Result<Request, HttpError> {
    let deadline = accepted + limits.request_deadline;
    let first_byte = (accepted + limits.first_byte_deadline).min(deadline);
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(pos) = find_head_end(&buf) {
            break pos;
        }
        // A request line that is already too long is refused before the rest arrives.
        if buf.len() > limits.max_target.saturating_add(32) && !buf.windows(2).any(|w| w == b"\r\n")
        {
            return Err(HttpError::new(414, "the request target is too long"));
        }
        if buf.len() > limits.max_head {
            return Err(HttpError::new(431, "the request head is too large"));
        }
        let want = chunk.len().min(
            limits
                .max_head
                .saturating_add(4)
                .saturating_sub(buf.len())
                .max(1),
        );
        let due = if buf.is_empty() { first_byte } else { deadline };
        let n = read_until_deadline(stream, chunk.get_mut(..want).unwrap_or(&mut []), due)?;
        if n == usize::MAX {
            continue;
        }
        if n == 0 {
            return Err(HttpError::new(
                400,
                "the request ended before it was complete",
            ));
        }
        buf.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
    };
    if head_end.saturating_add(4) > limits.max_head {
        return Err(HttpError::new(431, "the request head is too large"));
    }
    let head = buf.get(..head_end).unwrap_or(&[]);
    let mut request = parse_head(head, limits)?;
    let content_length = body_length(&request, limits)?;

    let mut body = buf
        .get(head_end.saturating_add(4)..)
        .unwrap_or(&[])
        .to_vec();
    if body.len() > content_length {
        return Err(HttpError::new(
            400,
            "extra data after the request (pipelining is not supported)",
        ));
    }
    while body.len() < content_length {
        let want = chunk.len().min(content_length - body.len());
        let n = read_until_deadline(stream, chunk.get_mut(..want).unwrap_or(&mut []), deadline)?;
        if n == usize::MAX {
            continue;
        }
        if n == 0 {
            return Err(HttpError::new(400, "the body ended before it was complete"));
        }
        body.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
    }
    request.body = body;
    Ok(request)
}

fn body_length(request: &Request, limits: &Limits) -> Result<usize, HttpError> {
    if request.header("transfer-encoding").is_some() {
        return Err(HttpError::new(501, "chunked bodies are not supported"));
    }
    if request.header("expect").is_some() {
        return Err(HttpError::new(417, "Expect is not supported"));
    }
    let declared = match request.header("content-length") {
        None => None,
        Some(v) => {
            if v.is_empty() || v.len() > 10 || !v.bytes().all(|b| b.is_ascii_digit()) {
                return Err(HttpError::new(400, "Content-Length is not a number"));
            }
            Some(
                v.parse::<usize>()
                    .map_err(|_| HttpError::new(400, "Content-Length is not a number"))?,
            )
        }
    };
    match (request.method, declared) {
        (Method::Get, None | Some(0)) => Ok(0),
        (Method::Get, Some(_)) => Err(HttpError::new(400, "a GET request must not have a body")),
        (Method::Post, None) => Err(HttpError::new(411, "Content-Length is required")),
        (Method::Post, Some(n)) if n > limits.max_body => {
            Err(HttpError::new(413, "the body is too large"))
        }
        (Method::Post, Some(n)) => Ok(n),
    }
}

fn parse_head(head: &[u8], limits: &Limits) -> Result<Request, HttpError> {
    if head.iter().any(|b| *b >= 0x80 || *b == 0) {
        return Err(HttpError::new(400, "the request head must be ASCII"));
    }
    let text = std::str::from_utf8(head)
        .map_err(|_| HttpError::new(400, "the request head must be ASCII"))?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    if request_line.contains(['\r', '\n']) {
        return Err(HttpError::new(400, "bare line breaks are not allowed"));
    }
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(HttpError::new(400, "malformed request line"));
    };
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(HttpError::new(
            505,
            "only HTTP/1.1 and HTTP/1.0 are supported",
        ));
    }
    if method.is_empty() || !method.bytes().all(is_token_char) {
        return Err(HttpError::new(400, "malformed method"));
    }
    let method = match method {
        "GET" => Method::Get,
        "POST" => Method::Post,
        _ => return Err(HttpError::new(405, "method not allowed")),
    };
    if target.len() > limits.max_target {
        return Err(HttpError::new(414, "the request target is too long"));
    }
    if !target.starts_with('/')
        || !target
            .bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && b != b'#')
    {
        return Err(HttpError::new(400, "malformed request target"));
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };

    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if headers.len() >= limits.max_headers {
            return Err(HttpError::new(431, "too many headers"));
        }
        if line.contains(['\r', '\n']) {
            return Err(HttpError::new(400, "bare line breaks are not allowed"));
        }
        if line.starts_with([' ', '\t']) {
            return Err(HttpError::new(400, "folded headers are not supported"));
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HttpError::new(400, "malformed header"));
        };
        if name.is_empty() || !name.bytes().all(is_token_char) {
            return Err(HttpError::new(400, "malformed header name"));
        }
        let value = value.trim_matches([' ', '\t']);
        if !value
            .bytes()
            .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b))
        {
            return Err(HttpError::new(400, "malformed header value"));
        }
        let name = name.to_ascii_lowercase();
        if SINGLETON_HEADERS.contains(&name.as_str()) && headers.iter().any(|(k, _)| *k == name) {
            return Err(HttpError::new(
                400,
                "a header that must be unique was repeated",
            ));
        }
        headers.push((name, value.to_string()));
    }
    Ok(Request {
        method,
        path: path.to_string(),
        query: query.to_string(),
        headers,
        body: Vec::new(),
    })
}

/// Headers sent with every response, including errors and static files.
pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

const SECURITY_HEADERS: &[(&str, &str)] = &[
    ("Content-Security-Policy", CSP),
    ("X-Content-Type-Options", "nosniff"),
    ("Referrer-Policy", "no-referrer"),
    ("Cache-Control", "no-store"),
    ("X-Frame-Options", "DENY"),
    ("Cross-Origin-Opener-Policy", "same-origin"),
    ("Cross-Origin-Resource-Policy", "same-origin"),
];

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    extra: Vec<(&'static str, String)>,
    csp: Option<&'static str>,
    pub body: Vec<u8>,
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        417 => "Expectation Failed",
        421 => "Misdirected Request",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "Unknown",
    }
}

impl Response {
    pub fn new(status: u16, content_type: &'static str, body: Vec<u8>) -> Response {
        Response {
            status,
            content_type,
            extra: Vec::new(),
            csp: None,
            body,
        }
    }

    pub fn json(status: u16, body: String) -> Response {
        Response::new(status, "application/json; charset=utf-8", body.into_bytes())
    }

    /// Replaces the page's content security policy of this response by `csp`, which must be
    /// stricter than [`CSP`] (a response of raw bytes, such as an image, gets
    /// `default-src 'none'; sandbox`). The header is never omitted.
    pub fn with_csp(mut self, csp: &'static str) -> Response {
        self.csp = Some(csp);
        self
    }

    /// An extra header. The value is reduced to visible ASCII so no input can add a line.
    pub fn with_header(mut self, name: &'static str, value: &str) -> Response {
        let safe: String = value
            .chars()
            .map(|c| if (' '..='~').contains(&c) { c } else { '?' })
            .collect();
        self.extra.push((name, safe));
        self
    }

    fn head(&self, length: usize, streaming: bool) -> String {
        let mut out = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nConnection: close\r\n",
            self.status,
            reason(self.status),
            self.content_type
        );
        if !streaming {
            out.push_str(&format!("Content-Length: {length}\r\n"));
        }
        for (k, v) in SECURITY_HEADERS {
            // a response may replace the policy by a stricter one, never drop it
            let v = if *k == "Content-Security-Policy" {
                self.csp.unwrap_or(v)
            } else {
                v
            };
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        for (k, v) in &self.extra {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str("\r\n");
        out
    }

    /// Writes the response in one go.
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        let mut bytes = self.head(self.body.len(), false).into_bytes();
        bytes.extend_from_slice(&self.body);
        w.write_all(&bytes)?;
        w.flush()
    }

    /// Writes only the head of a response whose body is streamed (server-sent events).
    pub fn write_stream_head(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(self.head(0, true).as_bytes())?;
        w.flush()
    }
}

/// Ends a connection politely: the write side is closed first (the client reads the response
/// and then end of stream) and what the client still sends is read and dropped for a moment, so
/// closing a socket with unread data cannot turn into a reset that destroys the response.
pub fn finish(stream: &mut TcpStream) {
    let _ = stream.shutdown(Shutdown::Write);
    let deadline = Instant::now() + Duration::from_millis(250);
    let mut drained = 0usize;
    let mut buf = [0u8; 4096];
    while drained < 64 * 1024 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || stream.set_read_timeout(Some(remaining)).is_err() {
            break;
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained = drained.saturating_add(n),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn parse(raw: &[u8], limits: Limits) -> Result<Request, HttpError> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let data = raw.to_vec();
        let writer = std::thread::spawn(move || {
            let mut c = TcpStream::connect(addr).unwrap();
            c.write_all(&data).unwrap();
            // keep the connection open so a short request hits the deadline, not end of stream
            std::thread::sleep(Duration::from_millis(400));
        });
        let (mut s, _) = listener.accept().unwrap();
        let limits = Limits {
            request_deadline: Duration::from_millis(300),
            first_byte_deadline: Duration::from_millis(300),
            ..limits
        };
        let r = read_request(&mut s, &limits, Instant::now());
        writer.join().unwrap();
        r
    }

    fn ok(raw: &str) -> Request {
        parse(raw.as_bytes(), Limits::default()).unwrap()
    }

    fn err(raw: &str) -> u16 {
        parse(raw.as_bytes(), Limits::default()).unwrap_err().status
    }

    #[test]
    fn a_simple_get_is_parsed() {
        let r = ok("GET /api/status?run=r1 HTTP/1.1\r\nHost: 127.0.0.1:1\r\nX-A: b\r\n\r\n");
        assert_eq!(r.method, Method::Get);
        assert_eq!(r.path, "/api/status");
        assert_eq!(r.query, "run=r1");
        assert_eq!(r.header("host"), Some("127.0.0.1:1"));
        assert_eq!(r.header("x-a"), Some("b"));
        assert!(r.body.is_empty());
    }

    #[test]
    fn a_post_body_is_read_to_its_length() {
        let r = ok("POST /x HTTP/1.1\r\nHost: h\r\nContent-Length: 7\r\n\r\n{\"a\":1}");
        assert_eq!(r.body, b"{\"a\":1}");
    }

    #[test]
    fn malformed_requests_get_a_status_and_never_panic() {
        assert_eq!(err("GET\r\n\r\n"), 400);
        assert_eq!(err("GET / HTTP/1.1 extra\r\n\r\n"), 400);
        assert_eq!(err("GET  / HTTP/1.1\r\n\r\n"), 400);
        assert_eq!(err("GET / HTTP/2.0\r\n\r\n"), 505);
        assert_eq!(err("GET /a b HTTP/1.1\r\n\r\n"), 400);
        assert_eq!(err("GET a HTTP/1.1\r\n\r\n"), 400);
        assert_eq!(err("GET /#frag HTTP/1.1\r\n\r\n"), 400);
        assert_eq!(err("GET * HTTP/1.1\r\n\r\n"), 400);
        assert_eq!(err("GET http://evil/ HTTP/1.1\r\n\r\n"), 400);
        assert_eq!(err("PUT / HTTP/1.1\r\n\r\n"), 405);
        assert_eq!(err("DELETE / HTTP/1.1\r\n\r\n"), 405);
        assert_eq!(err("G\x01T / HTTP/1.1\r\n\r\n"), 400);
        assert_eq!(err("GET / HTTP/1.1\r\nno-colon\r\n\r\n"), 400);
        assert_eq!(err("GET / HTTP/1.1\r\nBad Name: x\r\n\r\n"), 400);
        assert_eq!(err("GET / HTTP/1.1\r\nHost : x\r\n\r\n"), 400);
        assert_eq!(err("GET / HTTP/1.1\r\nX: a\x01b\r\n\r\n"), 400);
        assert_eq!(err("GET / HTTP/1.1\r\nX: caf\u{e9}\r\n\r\n"), 400);
        assert_eq!(err("GET / HTTP/1.1\r\nX: a\r\n folded\r\n\r\n"), 400);
        assert_eq!(
            err("GET / HTTP/1.1\nHost: x\n\n"),
            408,
            "bare LF never completes"
        );
        assert_eq!(err("GET / HTTP/1.1\r\nHost: a\nX: b\r\n\r\n"), 400);
        assert_eq!(err("GET /\0 HTTP/1.1\r\n\r\n"), 400);
    }

    #[test]
    fn duplicate_security_relevant_headers_are_refused() {
        for name in [
            "Host",
            "Origin",
            "X-Fleet-Session",
            "Authorization",
            "Content-Length",
        ] {
            let raw = format!("GET / HTTP/1.1\r\n{name}: a\r\n{name}: b\r\n\r\n");
            assert_eq!(err(&raw), 400, "{name}");
        }
    }

    #[test]
    fn body_rules() {
        assert_eq!(err("POST / HTTP/1.1\r\nHost: h\r\n\r\n"), 411);
        assert_eq!(err("POST / HTTP/1.1\r\nContent-Length: abc\r\n\r\n"), 400);
        assert_eq!(err("POST / HTTP/1.1\r\nContent-Length: -1\r\n\r\n"), 400);
        assert_eq!(
            err("POST / HTTP/1.1\r\nContent-Length: 99999999999\r\n\r\n"),
            400
        );
        assert_eq!(err("POST / HTTP/1.1\r\nContent-Length: 16385\r\n\r\n"), 413);
        assert_eq!(err("GET / HTTP/1.1\r\nContent-Length: 3\r\n\r\nabc"), 400);
        assert_eq!(
            err("POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"),
            501
        );
        assert_eq!(
            err("POST / HTTP/1.1\r\nContent-Length: 1\r\nExpect: 100-continue\r\n\r\n"),
            417
        );
        assert_eq!(
            err("POST / HTTP/1.1\r\nContent-Length: 1\r\n\r\nabc"),
            400,
            "pipelined data"
        );
        assert_eq!(
            err("POST / HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc"),
            408,
            "a short body hits the deadline"
        );
    }

    #[test]
    fn head_bounds() {
        let small = Limits {
            max_head: 256,
            max_target: 64,
            max_headers: 3,
            ..Limits::default()
        };
        let long_target = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(100));
        assert_eq!(
            parse(long_target.as_bytes(), small).unwrap_err().status,
            414
        );
        let endless = format!("GET / HTTP/1.1\r\nX: {}", "a".repeat(600));
        assert_eq!(parse(endless.as_bytes(), small).unwrap_err().status, 431);
        let many = "GET / HTTP/1.1\r\nA: 1\r\nB: 2\r\nC: 3\r\nD: 4\r\n\r\n";
        assert_eq!(parse(many.as_bytes(), small).unwrap_err().status, 431);
        let no_end_of_line = format!("GET /{}", "a".repeat(400));
        assert_eq!(
            parse(no_end_of_line.as_bytes(), small).unwrap_err().status,
            414
        );
    }

    #[test]
    fn a_cookie_header_is_not_special_and_may_repeat() {
        let r = ok("GET / HTTP/1.1\r\nHost: h\r\nCookie: a=1\r\nCookie: b=2\r\n\r\n");
        assert_eq!(r.method, Method::Get);
    }

    #[test]
    fn a_connection_that_sends_nothing_times_out_at_the_first_byte_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _idle = TcpStream::connect(addr).unwrap();
        let (mut s, _) = listener.accept().unwrap();
        let limits = Limits {
            first_byte_deadline: Duration::from_millis(150),
            ..Limits::default()
        };
        let started = Instant::now();
        let e = read_request(&mut s, &limits, Instant::now()).unwrap_err();
        assert_eq!(e.status, 408);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_request_that_never_completes_times_out() {
        assert_eq!(err("GET / HTTP/1.1\r\nHost: h\r\n"), 408);
    }

    #[test]
    fn responses_carry_the_security_headers_and_cannot_be_split() {
        let r = Response::json(200, "{}".to_string()).with_header("X-T", "a\r\nInjected: 1");
        let mut out = Vec::new();
        r.write_to(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains(&format!("Content-Security-Policy: {CSP}\r\n")));
        for h in [
            "X-Content-Type-Options: nosniff\r\n",
            "Referrer-Policy: no-referrer\r\n",
            "Cache-Control: no-store\r\n",
            "Content-Length: 2\r\n",
            "Connection: close\r\n",
        ] {
            assert!(text.contains(h), "{h}");
        }
        assert!(!text.contains("\r\nInjected:"));
        assert!(text.ends_with("\r\n\r\n{}"));
    }
}
