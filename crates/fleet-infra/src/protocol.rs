//! Wire format of the control socket.
//!
//! Requests and replies are single JSON lines. After a successful `task.attach` the connection
//! switches to binary frames. Everything read from the socket is untrusted: lines and frames are
//! bounded before they are buffered, and error text from the peer is sanitized before it can
//! reach a terminal.

use crate::json;
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::limits::{MAX_FRAME_PAYLOAD, MAX_MESSAGE_BYTES};
use fleet_domain::sanitize::{head_str, sanitize_text};
use fleet_domain::value::Value;
use std::io::{self, BufRead, Read, Write};

/// Protocol version carried in every message.
pub const VERSION: i128 = 1;

/// Longest accepted reply line. Requests stay at `MAX_MESSAGE_BYTES`; a `status` reply grows
/// with the number of tasks and is produced by the daemon itself, so it gets a larger bound.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Longest command name.
const MAX_CMD_LEN: usize = 64;
/// Error messages received from a peer are cut to this many bytes.
const MAX_ERROR_MESSAGE: usize = 2 * 1024;
const MAX_ERROR_SUBJECT: usize = 128;

pub const KIND_INPUT: u8 = 1;
pub const KIND_RESIZE: u8 = 2;
pub const KIND_DETACH: u8 = 3;

pub const KIND_OUTPUT: u8 = 1;
pub const KIND_EXIT: u8 = 2;
pub const KIND_NOTICE: u8 = 3;

/// Reads one line of at most `max` bytes (without the newline).
///
/// `None` means end of input with nothing read. A longer line is `InvalidData` and the rest of
/// it is left unread: at most `max + 1` bytes are ever buffered.
pub fn read_line_bounded(r: &mut impl BufRead, max: usize) -> io::Result<Option<String>> {
    let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    let mut buf: Vec<u8> = Vec::new();
    let read = r.by_ref().take(limit).read_until(b'\n', &mut buf)?;
    if read == 0 {
        return Ok(None);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    } else if buf.len() > max {
        // The limit cut the line before its newline: it is too long.
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("line is longer than {max} bytes"),
        ));
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "line is not valid UTF-8"))
}

/// One request line (no newline). A null `args` is sent as an empty object. `token` is the
/// daemon's per-run capability token (see `statedir`); `None` leaves the field out.
pub fn encode_request(cmd: &str, args: &Value, token: Option<&str>) -> String {
    let args = if args.is_null() {
        Value::object(std::iter::empty::<(String, Value)>())
    } else {
        args.clone()
    };
    let mut fields = vec![
        ("v", Value::Int(VERSION)),
        ("cmd", Value::str(cmd)),
        ("args", args),
    ];
    if let Some(token) = token {
        fields.push(("token", Value::str(token)));
    }
    json::to_line(&Value::object(fields))
}

fn bad(message: &str) -> FleetError {
    FleetError::invalid(message)
}

fn check_version(doc: &Value) -> Result<()> {
    match doc.get("v") {
        Some(Value::Int(n)) if *n == VERSION => Ok(()),
        Some(_) => Err(bad("unsupported protocol version")),
        None => Err(bad("message has no protocol version")),
    }
}

/// One validated request line.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub cmd: String,
    /// Always an object.
    pub args: Value,
    /// The capability token the client presented, if any. Not validated here: the server
    /// compares it against its own.
    pub token: Option<String>,
}

/// Validates one request line.
pub fn parse_request(line: &str) -> Result<Request> {
    let doc = json::parse_value(line.as_bytes(), MAX_MESSAGE_BYTES)
        .map_err(|e| FleetError::invalid(format!("malformed request: {e}")))?;
    if doc.as_object().is_none() {
        return Err(bad("a request must be a JSON object"));
    }
    check_version(&doc)?;
    let cmd = doc
        .get("cmd")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("request has no command"))?;
    if cmd.is_empty()
        || cmd.len() > MAX_CMD_LEN
        || !cmd
            .chars()
            .all(|c| c.is_ascii_lowercase() || matches!(c, '.' | '_' | '-'))
    {
        return Err(bad("the command name is not valid"));
    }
    let args = match doc.get("args") {
        None | Some(Value::Null) => Value::object(std::iter::empty::<(String, Value)>()),
        Some(v @ Value::Object(_)) => v.clone(),
        Some(_) => return Err(bad("args must be an object")),
    };
    let token = match doc.get("token") {
        None | Some(Value::Null) => None,
        Some(Value::Str(s)) => Some(s.clone()),
        Some(_) => return Err(bad("token must be a string")),
    };
    Ok(Request {
        cmd: cmd.to_string(),
        args,
        token,
    })
}

pub fn encode_ok(data: &Value) -> String {
    json::to_line(&Value::object([
        ("v", Value::Int(VERSION)),
        ("ok", Value::Bool(true)),
        ("data", data.clone()),
    ]))
}

pub fn encode_err(e: &FleetError) -> String {
    let message = head_str(&e.message, MAX_ERROR_MESSAGE);
    let subject = match &e.subject {
        Some(s) => Value::str(head_str(s, MAX_ERROR_SUBJECT)),
        None => Value::Null,
    };
    json::to_line(&Value::object([
        ("v", Value::Int(VERSION)),
        ("ok", Value::Bool(false)),
        (
            "error",
            Value::object([
                ("code", Value::str(e.code.as_str())),
                ("message", Value::str(message)),
                ("subject", subject),
            ]),
        ),
    ]))
}

/// Interprets one reply line: the data object of an `ok` reply, or the error of a failed one.
pub fn parse_response(line: &str) -> Result<Value> {
    let doc = json::parse_value(line.as_bytes(), MAX_RESPONSE_BYTES)
        .map_err(|e| FleetError::internal(format!("malformed reply from the daemon: {e}")))?;
    check_version(&doc)
        .map_err(|_| FleetError::internal("the daemon speaks a different protocol version"))?;
    match doc.get("ok") {
        Some(Value::Bool(true)) => match doc.get("data") {
            Some(data @ Value::Object(_)) => Ok(data.clone()),
            _ => Err(FleetError::internal("the reply has no data object")),
        },
        Some(Value::Bool(false)) => Err(error_from_reply(doc.get("error"))),
        _ => Err(FleetError::internal("the reply has no ok flag")),
    }
}

fn error_from_reply(err: Option<&Value>) -> FleetError {
    let code = err
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .and_then(ErrorCode::parse)
        .unwrap_or(ErrorCode::Internal);
    let message = err
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(|m| sanitize_text(head_str(m, MAX_ERROR_MESSAGE).as_bytes()))
        .unwrap_or_else(|| "the daemon reported an error without a message".to_string());
    let subject = err
        .and_then(|e| e.get("subject"))
        .and_then(Value::as_str)
        .map(|s| sanitize_text(head_str(s, MAX_ERROR_SUBJECT).as_bytes()));
    FleetError {
        code,
        message,
        subject,
    }
}

/// Writes one frame as a single `write_all`, so frames from one writer never interleave.
pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame payload is too large",
        ));
    }
    let len = u16::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame payload is too large"))?;
    let mut buf = Vec::with_capacity(payload.len().saturating_add(3));
    buf.push(kind);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf)
}

/// Reads one frame. `None` is a clean end of stream at a frame boundary; a stream that ends
/// inside a frame is `UnexpectedEof`. The length is checked before the payload is allocated.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut kind = [0u8; 1];
    loop {
        match r.read(&mut kind) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    let mut len = [0u8; 2];
    r.read_exact(&mut len)?;
    let len = usize::from(u16::from_be_bytes(len));
    if len > MAX_FRAME_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame payload is too large",
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some((kind[0], payload)))
}

pub fn encode_resize(cols: u16, rows: u16) -> [u8; 4] {
    let c = cols.to_be_bytes();
    let r = rows.to_be_bytes();
    [c[0], c[1], r[0], r[1]]
}

pub fn decode_resize(payload: &[u8]) -> Option<(u16, u16)> {
    let b: [u8; 4] = payload.try_into().ok()?;
    Some((
        u16::from_be_bytes([b[0], b[1]]),
        u16::from_be_bytes([b[2], b[3]]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_domain::error::ALL_CODES;
    use std::io::Cursor;

    fn obj(pairs: &[(&str, Value)]) -> Value {
        Value::object(pairs.iter().map(|(k, v)| (k.to_string(), v.clone())))
    }

    #[test]
    fn request_round_trip() {
        let args = obj(&[("task", Value::str("r1/t1")), ("max", Value::int(5))]);
        let line = encode_request("task.tail", &args, None);
        assert!(!line.contains('\n'));
        let req = parse_request(&line).unwrap();
        assert_eq!(req.cmd, "task.tail");
        assert_eq!(req.args, args);
        assert_eq!(req.token, None);
    }

    #[test]
    fn the_token_travels_as_a_top_level_field_not_inside_args() {
        let args = obj(&[("task", Value::str("r1/t1"))]);
        let token = "ab".repeat(32);
        let line = encode_request("approve", &args, Some(&token));
        let req = parse_request(&line).unwrap();
        assert_eq!(req.token.as_deref(), Some(token.as_str()));
        assert_eq!(
            req.args, args,
            "the token must not leak into the command arguments"
        );
        // a token that is not a string is a malformed request, not a missing token
        let err = parse_request(r#"{"v":1,"cmd":"ping","token":7}"#).unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);
        let req = parse_request(r#"{"v":1,"cmd":"ping","token":null}"#).unwrap();
        assert_eq!(req.token, None);
    }

    #[test]
    fn a_command_name_may_hold_a_hyphen_like_issue_to_task() {
        let line = encode_request("issue.to-task", &Value::Null, None);
        assert_eq!(parse_request(&line).unwrap().cmd, "issue.to-task");
        for bad in ["Issue.to-task", "issue to-task", "issue.to+task"] {
            let line = encode_request(bad, &Value::Null, None);
            assert!(parse_request(&line).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn null_or_missing_args_become_an_empty_object() {
        let line = encode_request("ping", &Value::Null, None);
        let args = parse_request(&line).unwrap().args;
        assert_eq!(args, obj(&[]));
        let args = parse_request(r#"{"v":1,"cmd":"ping"}"#).unwrap().args;
        assert_eq!(args, obj(&[]));
        let args = parse_request(r#"{"v":1,"cmd":"ping","args":null}"#)
            .unwrap()
            .args;
        assert_eq!(args, obj(&[]));
    }

    #[test]
    fn hostile_requests_are_rejected() {
        let long = "a".repeat(65);
        let cases = [
            "".to_string(),
            "not json".to_string(),
            "[1,2]".to_string(),
            r#"{"cmd":"ping"}"#.to_string(),
            r#"{"v":2,"cmd":"ping"}"#.to_string(),
            r#"{"v":"1","cmd":"ping"}"#.to_string(),
            r#"{"v":1}"#.to_string(),
            r#"{"v":1,"cmd":7}"#.to_string(),
            r#"{"v":1,"cmd":""}"#.to_string(),
            r#"{"v":1,"cmd":"Ping"}"#.to_string(),
            r#"{"v":1,"cmd":"pi ng"}"#.to_string(),
            r#"{"v":1,"cmd":"../etc"}"#.to_string(),
            r#"{"v":1,"cmd":"task\u0000"}"#.to_string(),
            format!(r#"{{"v":1,"cmd":"{long}"}}"#),
            r#"{"v":1,"cmd":"ping","args":[1]}"#.to_string(),
            r#"{"v":1,"cmd":"ping","args":"x"}"#.to_string(),
            r#"{"v":1,"cmd":"ping"} trailing"#.to_string(),
        ];
        for case in cases {
            let err = parse_request(&case).expect_err(&case);
            assert_eq!(err.code, ErrorCode::Invalid, "{case}");
        }
        let exact = "a".repeat(64);
        assert!(parse_request(&format!(r#"{{"v":1,"cmd":"{exact}"}}"#)).is_ok());
    }

    #[test]
    fn ok_reply_round_trip_and_data_must_be_an_object() {
        let data = obj(&[("schema", Value::str("fleet.ping/1")), ("n", Value::int(3))]);
        assert_eq!(parse_response(&encode_ok(&data)).unwrap(), data);
        let err = parse_response(r#"{"v":1,"ok":true,"data":[1]}"#).unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);
        let err = parse_response(r#"{"v":1,"ok":true}"#).unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);
    }

    #[test]
    fn every_error_code_round_trips_with_message_and_subject() {
        for code in ALL_CODES {
            let e = FleetError::new(code, "something failed").with_subject("a1");
            let got = parse_response(&encode_err(&e)).unwrap_err();
            assert_eq!(got.code, code);
            assert_eq!(got.message, "something failed");
            assert_eq!(got.subject.as_deref(), Some("a1"));
            let no_subject = parse_response(&encode_err(&FleetError::new(code, "x"))).unwrap_err();
            assert_eq!(no_subject.subject, None);
        }
    }

    #[test]
    fn unknown_code_becomes_internal() {
        let line = r#"{"v":1,"ok":false,"error":{"code":"made_up","message":"m","subject":null}}"#;
        let got = parse_response(line).unwrap_err();
        assert_eq!(got.code, ErrorCode::Internal);
        assert_eq!(got.message, "m");
    }

    #[test]
    fn error_text_from_the_peer_is_sanitized_and_clipped() {
        let hostile = "bad\u{1b}]52;c;SGVsbG8=\u{7}\u{1b}[2J\u{202e}end\r\n";
        let line = encode_err(&FleetError::new(ErrorCode::Conflict, hostile));
        let got = parse_response(&line).unwrap_err();
        for forbidden in ['\u{1b}', '\u{7}', '\u{202e}', '\r'] {
            assert!(!got.message.contains(forbidden), "{:?}", got.message);
        }
        assert!(got.message.starts_with("bad"));

        // A hostile daemon can also send an oversized message and a hostile subject.
        let big = "x".repeat(10_000);
        let line = format!(
            r#"{{"v":1,"ok":false,"error":{{"code":"limit","message":"{big}","subject":"a\u001b[31m1"}}}}"#
        );
        let got = parse_response(&line).unwrap_err();
        assert_eq!(got.code, ErrorCode::Limit);
        assert!(got.message.len() <= MAX_ERROR_MESSAGE);
        assert!(!got.subject.unwrap_or_default().contains('\u{1b}'));
    }

    #[test]
    fn malformed_replies_are_internal_errors() {
        for line in [
            "",
            "garbage",
            r#"{"v":2,"ok":true,"data":{}}"#,
            r#"{"ok":true,"data":{}}"#,
            r#"{"v":1,"ok":"yes","data":{}}"#,
            r#"{"v":1}"#,
        ] {
            let err = parse_response(line).expect_err(line);
            assert_eq!(err.code, ErrorCode::Internal, "{line}");
        }
        // an error reply without an error object still yields an error, never a panic
        let err = parse_response(r#"{"v":1,"ok":false}"#).unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);
    }

    #[test]
    fn encode_err_clips_long_messages() {
        let e = FleetError::invalid("y".repeat(50_000));
        let line = encode_err(&e);
        assert!(line.len() < 4 * 1024);
    }

    #[test]
    fn line_reader_strips_newlines_and_signals_eof() {
        let mut r = Cursor::new(b"one\ntwo\r\nthree".to_vec());
        assert_eq!(
            read_line_bounded(&mut r, 100).unwrap().as_deref(),
            Some("one")
        );
        assert_eq!(
            read_line_bounded(&mut r, 100).unwrap().as_deref(),
            Some("two")
        );
        assert_eq!(
            read_line_bounded(&mut r, 100).unwrap().as_deref(),
            Some("three")
        );
        assert_eq!(read_line_bounded(&mut r, 100).unwrap(), None);
        let mut empty = Cursor::new(Vec::new());
        assert_eq!(read_line_bounded(&mut empty, 10).unwrap(), None);
        let mut blank = Cursor::new(b"\n".to_vec());
        assert_eq!(
            read_line_bounded(&mut blank, 10).unwrap().as_deref(),
            Some("")
        );
    }

    #[test]
    fn line_reader_accepts_exactly_max_and_rejects_longer() {
        let mut ok = Cursor::new(b"12345\nrest\n".to_vec());
        assert_eq!(
            read_line_bounded(&mut ok, 5).unwrap().as_deref(),
            Some("12345")
        );
        let mut long = Cursor::new(b"123456\n".to_vec());
        let err = read_line_bounded(&mut long, 5).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // no newline at all and too long
        let mut long = Cursor::new(vec![b'a'; 100]);
        let err = read_line_bounded(&mut long, 5).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn line_reader_does_not_buffer_an_endless_line() {
        /// Yields 'a' forever and counts how much was handed out.
        struct Endless(u64);
        impl Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                buf.fill(b'a');
                self.0 += buf.len() as u64;
                Ok(buf.len())
            }
        }
        let mut r = io::BufReader::with_capacity(1024, Endless(0));
        let err = read_line_bounded(&mut r, 4096).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(r.get_ref().0 <= 4096 + 2048, "read {} bytes", r.get_ref().0);
    }

    #[test]
    fn line_reader_rejects_invalid_utf8() {
        let mut r = Cursor::new(vec![b'a', 0xff, 0xfe, b'\n']);
        let err = read_line_bounded(&mut r, 100).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn frame_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, KIND_INPUT, b"hello").unwrap();
        write_frame(&mut buf, KIND_DETACH, b"").unwrap();
        let max = vec![7u8; MAX_FRAME_PAYLOAD];
        write_frame(&mut buf, KIND_RESIZE, &max).unwrap();
        let mut r = Cursor::new(buf);
        assert_eq!(
            read_frame(&mut r).unwrap(),
            Some((KIND_INPUT, b"hello".to_vec()))
        );
        assert_eq!(read_frame(&mut r).unwrap(), Some((KIND_DETACH, Vec::new())));
        assert_eq!(read_frame(&mut r).unwrap(), Some((KIND_RESIZE, max)));
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn oversize_payload_is_refused_when_writing() {
        let mut buf = Vec::new();
        let err =
            write_frame(&mut buf, KIND_OUTPUT, &vec![0u8; MAX_FRAME_PAYLOAD + 1]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(buf.is_empty());
    }

    #[test]
    fn oversize_frame_header_is_refused_before_allocating() {
        // Claims 65535 bytes but only a few follow: the length alone must be rejected.
        let mut r = Cursor::new(vec![KIND_INPUT, 0xff, 0xff, 1, 2, 3]);
        let err = read_frame(&mut r).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let over = u16::try_from(MAX_FRAME_PAYLOAD + 1).unwrap().to_be_bytes();
        let mut r = Cursor::new(vec![KIND_INPUT, over[0], over[1]]);
        assert_eq!(
            read_frame(&mut r).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn truncated_frames_are_errors_not_eof() {
        for bytes in [
            vec![KIND_INPUT],
            vec![KIND_INPUT, 0],
            vec![KIND_INPUT, 0, 5, b'a', b'b'],
        ] {
            let mut r = Cursor::new(bytes);
            let err = read_frame(&mut r).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        }
    }

    #[test]
    fn resize_codec() {
        assert_eq!(decode_resize(&encode_resize(120, 40)), Some((120, 40)));
        assert_eq!(decode_resize(&encode_resize(0, 65535)), Some((0, 65535)));
        assert_eq!(decode_resize(&[1, 2, 3]), None);
        assert_eq!(decode_resize(&[1, 2, 3, 4, 5]), None);
        assert_eq!(decode_resize(&[]), None);
    }
}
