//! The answer of `gh api --include`: a status line, headers, a blank line and the body, and what
//! each failure means. Everything here works on text; nothing is trusted, everything is capped.

use crate::limits::MAX_HEADER_BYTES;
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::sanitize::{flatten_line, head_str};

const MAX_HEADERS: usize = 100;

/// A parsed answer. `status` is `None` when the output carries no header block (an error that
/// happened before any answer was received, or a `gh` that does not print one).
#[derive(Debug, PartialEq, Eq)]
pub struct Response<'a> {
    pub status: Option<u16>,
    /// Lower-cased names, trimmed values, at most 100 of them.
    pub headers: Vec<(String, String)>,
    pub body: &'a [u8],
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Splits `stdout` into status line, headers and body.
pub fn parse_response(stdout: &[u8]) -> Result<Response<'_>> {
    if !stdout.starts_with(b"HTTP/") {
        return Ok(Response {
            status: None,
            headers: Vec::new(),
            body: stdout,
        });
    }
    let window = stdout
        .get(..MAX_HEADER_BYTES.min(stdout.len()))
        .unwrap_or(stdout);
    let (head_end, body_start) = match (find(window, b"\r\n\r\n"), find(window, b"\n\n")) {
        (Some(a), Some(b)) if a < b => (a, a + 4),
        (Some(a), None) => (a, a + 4),
        (_, Some(b)) => (b, b + 2),
        (None, None) => {
            return Err(FleetError::invalid(
                "the answer of gh starts like an HTTP answer but its header block does not end",
            ))
        }
    };
    let head = String::from_utf8_lossy(stdout.get(..head_end).unwrap_or_default());
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|s| (100..=599).contains(s))
        .ok_or_else(|| FleetError::invalid("the status line of the answer of gh is not valid"))?;
    let mut headers = Vec::new();
    for line in lines.take(MAX_HEADERS) {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((
                name.trim().to_ascii_lowercase(),
                head_str(value.trim(), 512).to_string(),
            ));
        }
    }
    Ok(Response {
        status: Some(status),
        headers,
        body: stdout.get(body_start..).unwrap_or_default(),
    })
}

impl Response<'_> {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn rate(&self) -> RateHeaders {
        let num = |name: &str| self.header(name).and_then(|v| v.parse::<u64>().ok());
        RateHeaders {
            limit: num("x-ratelimit-limit"),
            remaining: num("x-ratelimit-remaining"),
            reset_epoch_s: num("x-ratelimit-reset"),
            retry_after_s: num("retry-after"),
        }
    }

    /// True when the `Link` header offers a next page.
    pub fn has_next_page(&self) -> bool {
        self.header("link")
            .is_some_and(|l| l.split(',').any(|part| part.contains("rel=\"next\"")))
    }
}

/// What the answer says about the rate limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RateHeaders {
    pub limit: Option<u64>,
    pub remaining: Option<u64>,
    /// Epoch seconds at which the window resets.
    pub reset_epoch_s: Option<u64>,
    pub retry_after_s: Option<u64>,
}

/// Why a call did not give an answer to use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    NotAuthenticated,
    /// The network or the host cannot be reached; the text is one cleaned line from `gh`.
    NetworkDown(String),
    RateLimited(RateHeaders),
    NotFound,
    Forbidden(String),
    /// The forge refused the request as it was written (validation, conflict).
    Rejected(String),
    Server(u16),
    Other(String),
}

const NETWORK_MARKERS: [&str; 11] = [
    "dial tcp",
    "no such host",
    "connection refused",
    "network is unreachable",
    "i/o timeout",
    "could not resolve",
    "error connecting",
    "tls handshake",
    "context deadline exceeded",
    "timeout awaiting",
    "connection reset",
];

fn http_status_in(stderr: &str) -> Option<u16> {
    let at = stderr.rfind("(HTTP ")?;
    let rest = stderr.get(at + 6..)?;
    rest.split(')').next()?.trim().parse::<u16>().ok()
}

/// Reads a failed call. `message` is the `message` field of an error body, already cleaned (it
/// may be empty); `stderr` is what `gh` printed; `exit_code` is its exit status.
pub fn classify(
    exit_code: Option<i32>,
    status: Option<u16>,
    rate: RateHeaders,
    message: &str,
    stderr: &str,
) -> Failure {
    let low_err = stderr.to_ascii_lowercase();
    let low_msg = message.to_ascii_lowercase();
    let status = status.or_else(|| http_status_in(stderr));
    let not_signed_in = status == Some(401)
        || exit_code == Some(4)
        || low_err.contains("gh auth login")
        || low_err.contains("not logged in")
        || low_err.contains("authentication token")
        || low_msg.contains("bad credentials");
    if not_signed_in {
        return Failure::NotAuthenticated;
    }
    let limited_text = low_msg.contains("rate limit") || low_err.contains("rate limit");
    let limited = status == Some(429)
        || (status == Some(403)
            && (limited_text || rate.remaining == Some(0) || rate.retry_after_s.is_some()));
    if limited {
        return Failure::RateLimited(rate);
    }
    let line = |s: &str| flatten_line(s, 160);
    match status {
        Some(404) => Failure::NotFound,
        Some(403) => Failure::Forbidden(line(if message.is_empty() { stderr } else { message })),
        Some(s @ (400 | 409 | 410 | 422)) => Failure::Rejected(format!(
            "HTTP {s}: {}",
            line(if message.is_empty() { stderr } else { message })
        )),
        Some(s) if s >= 500 => Failure::Server(s),
        Some(s) => Failure::Other(format!("HTTP {s}: {}", line(message))),
        None => {
            if NETWORK_MARKERS.iter().any(|m| low_err.contains(m)) {
                Failure::NetworkDown(line(stderr))
            } else {
                Failure::Other(line(stderr))
            }
        }
    }
}

impl Failure {
    /// The error a command reports. Every one of them says that nothing was changed.
    pub fn into_error(self) -> FleetError {
        match self {
            Failure::NotAuthenticated => FleetError::unavailable(
                "GitHub: gh is not signed in. Run `gh auth login` yourself; fleet never sees or stores a token. Nothing was changed.",
            ),
            Failure::NetworkDown(why) => FleetError::unavailable(format!(
                "GitHub: the network or the host cannot be reached ({why}). Nothing was changed."
            )),
            Failure::RateLimited(_) => FleetError::unavailable(
                "GitHub: the rate limit was reached. Nothing was changed.",
            ),
            Failure::NotFound => FleetError::not_found(
                "GitHub: not found (or the signed-in account may not see it)",
            ),
            Failure::Forbidden(why) => FleetError::forbidden(format!("GitHub refused: {why}")),
            Failure::Rejected(why) => FleetError::new(
                ErrorCode::Invalid,
                format!("GitHub rejected the request: {why}"),
            ),
            Failure::Server(code) => FleetError::unavailable(format!(
                "GitHub answered with a server error (HTTP {code}). Nothing was changed."
            )),
            Failure::Other(why) => FleetError::unavailable(format!("gh failed: {why}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_and_body_are_split_for_both_line_endings() {
        let crlf = b"HTTP/2.0 200 OK\r\nContent-Type: application/json\r\nX-RateLimit-Remaining: 4990\r\nLink: <https://x/y?page=2>; rel=\"next\", <https://x/y?page=9>; rel=\"last\"\r\n\r\n[1,2]";
        let r = parse_response(crlf).unwrap();
        assert_eq!(r.status, Some(200));
        assert_eq!(r.body, b"[1,2]");
        assert_eq!(r.rate().remaining, Some(4990));
        assert!(r.has_next_page());
        let lf = b"HTTP/1.1 404 Not Found\nretry-after: 7\n\n{\"message\":\"Not Found\"}";
        let r = parse_response(lf).unwrap();
        assert_eq!(r.status, Some(404));
        assert_eq!(r.rate().retry_after_s, Some(7));
        assert!(!r.has_next_page());
        assert_eq!(r.body, b"{\"message\":\"Not Found\"}");
    }

    #[test]
    fn output_without_a_header_block_is_all_body() {
        let r = parse_response(b"{\"a\":1}").unwrap();
        assert_eq!(r.status, None);
        assert_eq!(r.body, b"{\"a\":1}");
        let r = parse_response(b"").unwrap();
        assert_eq!(r.status, None);
        assert!(r.body.is_empty());
    }

    #[test]
    fn a_broken_header_block_is_an_error() {
        for bad in [
            &b"HTTP/2.0 200 OK\r\nno end of headers"[..],
            &b"HTTP/2.0 abc OK\r\n\r\n"[..],
            &b"HTTP/2.0 99 OK\r\n\r\n"[..],
            &b"HTTP/2.0 700 OK\r\n\r\n"[..],
            &b"HTTP/2.0\r\n\r\n"[..],
        ] {
            assert!(
                parse_response(bad).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        let mut huge = b"HTTP/2.0 200 OK\r\n".to_vec();
        huge.extend(std::iter::repeat_n(b'a', MAX_HEADER_BYTES * 2));
        assert!(parse_response(&huge).is_err());
    }

    #[test]
    fn a_flood_of_headers_is_cut() {
        let mut text = String::from("HTTP/2.0 200 OK\r\n");
        for i in 0..300 {
            text.push_str(&format!("h{i}: v\r\n"));
        }
        text.push_str("\r\n{}");
        let r = parse_response(text.as_bytes()).unwrap();
        assert_eq!(r.headers.len(), MAX_HEADERS);
        assert_eq!(r.body, b"{}");
    }

    fn none() -> RateHeaders {
        RateHeaders::default()
    }

    #[test]
    fn failures_are_told_apart() {
        assert_eq!(
            classify(
                Some(4),
                None,
                none(),
                "",
                "gh: To get started with GitHub CLI, please run:  gh auth login"
            ),
            Failure::NotAuthenticated
        );
        assert_eq!(
            classify(Some(1), Some(401), none(), "Bad credentials", ""),
            Failure::NotAuthenticated
        );
        assert!(matches!(
            classify(
                Some(1),
                Some(403),
                RateHeaders {
                    remaining: Some(0),
                    ..none()
                },
                "",
                ""
            ),
            Failure::RateLimited(_)
        ));
        assert!(matches!(
            classify(
                Some(1),
                Some(403),
                none(),
                "You have exceeded a secondary rate limit",
                ""
            ),
            Failure::RateLimited(_)
        ));
        assert!(matches!(
            classify(Some(1), Some(429), none(), "", ""),
            Failure::RateLimited(_)
        ));
        assert!(matches!(
            classify(
                Some(1),
                None,
                none(),
                "",
                "gh: API rate limit exceeded for user ID 1. (HTTP 403)"
            ),
            Failure::RateLimited(_)
        ));
        assert_eq!(
            classify(Some(1), Some(404), none(), "Not Found", ""),
            Failure::NotFound
        );
        assert!(matches!(
            classify(Some(1), Some(403), none(), "Resource not accessible", ""),
            Failure::Forbidden(_)
        ));
        assert!(matches!(
            classify(Some(1), Some(422), none(), "Validation Failed", ""),
            Failure::Rejected(_)
        ));
        assert_eq!(
            classify(Some(1), Some(502), none(), "", ""),
            Failure::Server(502)
        );
        assert!(matches!(
            classify(
                Some(1),
                None,
                none(),
                "",
                "error connecting to api.github.com"
            ),
            Failure::NetworkDown(_)
        ));
        assert!(matches!(
            classify(
                Some(1),
                None,
                none(),
                "",
                "Get \"https://api.github.com/x\": dial tcp: lookup api.github.com: no such host"
            ),
            Failure::NetworkDown(_)
        ));
        assert!(matches!(
            classify(Some(1), None, none(), "", "something odd"),
            Failure::Other(_)
        ));
    }

    #[test]
    fn every_failure_becomes_an_error_that_says_nothing_changed_where_that_matters() {
        for (failure, code) in [
            (Failure::NotAuthenticated, ErrorCode::Unavailable),
            (Failure::NetworkDown("x".into()), ErrorCode::Unavailable),
            (
                Failure::RateLimited(RateHeaders::default()),
                ErrorCode::Unavailable,
            ),
            (Failure::NotFound, ErrorCode::NotFound),
            (Failure::Forbidden("x".into()), ErrorCode::Forbidden),
            (Failure::Rejected("x".into()), ErrorCode::Invalid),
            (Failure::Server(503), ErrorCode::Unavailable),
            (Failure::Other("x".into()), ErrorCode::Unavailable),
        ] {
            let e = failure.clone().into_error();
            assert_eq!(e.code, code, "{failure:?}");
        }
        assert!(Failure::NotAuthenticated
            .into_error()
            .message
            .contains("never sees or stores a token"));
        assert!(Failure::NetworkDown("x".into())
            .into_error()
            .message
            .contains("Nothing was changed"));
    }

    #[test]
    fn error_text_is_one_clean_line() {
        let f = classify(
            Some(1),
            Some(422),
            none(),
            "line one\n\u{1b}[31mred\u{202e}",
            "",
        );
        let Failure::Rejected(text) = f else { panic!() };
        assert!(
            !text.contains('\n') && !text.contains('\u{1b}') && !text.contains('\u{202e}'),
            "{text:?}"
        );
    }
}
