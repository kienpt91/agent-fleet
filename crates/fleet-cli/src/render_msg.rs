//! Text views of the message channels. Everything the daemon sends is sanitized again here, and
//! every message is shown under its label: a peer's words are never shown as if fleet or the
//! operator had said them.

use crate::render::clean;
use fleet_domain::value::Value;
use std::fmt::Write as _;

fn s(v: &Value, key: &str) -> String {
    clean(v.get(key).and_then(Value::as_str).unwrap_or(""))
}

fn n(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn list<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).unwrap_or(&[])
}

fn texts(v: &Value, key: &str) -> String {
    list(v, key)
        .iter()
        .filter_map(Value::as_str)
        .map(clean)
        .collect::<Vec<_>>()
        .join(", ")
}

/// `fleet msg list`.
pub fn render_msg_list(v: &Value) -> String {
    let mut out = String::new();
    let channels = list(v, "channels");
    if channels.is_empty() {
        out.push_str("no channels (declare them with [[channels]] in a plan)\n");
    }
    for c in channels {
        let muted = if c.get("muted").and_then(Value::as_bool) == Some(true) {
            format!(" [MUTED: {}]", s(c, "mute_reason"))
        } else {
            String::new()
        };
        let m = c.get("messages").cloned().unwrap_or(Value::Null);
        let _ = writeln!(
            out,
            "channel {}/{} ({}, {}{}){muted}",
            s(c, "run"),
            s(c, "name"),
            s(c, "mode"),
            s(c, "direction"),
            if c.get("requires_approval").and_then(Value::as_bool) == Some(true) {
                ", approval required"
            } else {
                ""
            },
        );
        let _ = writeln!(
            out,
            "  members {}; queued {}, awaiting approval {}, delivered {}, expired {}, blocked {}",
            texts(c, "members"),
            n(&m, "queued"),
            n(&m, "awaiting_approval"),
            n(&m, "delivered"),
            n(&m, "expired"),
            n(&m, "blocked"),
        );
    }
    for t in list(v, "muted_tasks") {
        let on = match s(t, "channel").as_str() {
            "" => String::new(),
            c => format!(" on channel {c}"),
        };
        let _ = writeln!(
            out,
            "task {}/{} is muted{on} ({})",
            s(t, "run"),
            s(t, "task"),
            s(t, "reason")
        );
    }
    let messages = list(v, "messages");
    if !messages.is_empty() {
        out.push_str("\nmessages (newest first; text is unverified peer content)\n");
    }
    for m in messages {
        let status = s(m, "status");
        let reason = s(m, "reason");
        let badge = if status == "blocked" {
            format!("BLOCKED ({reason})")
        } else {
            status
        };
        let _ = writeln!(
            out,
            "{} {} [{}] {}: {}",
            s(m, "id"),
            s(m, "channel"),
            s(m, "kind"),
            s(m, "label"),
            if s(m, "subject").is_empty() {
                badge.clone()
            } else {
                format!("{:?} - {badge}", s(m, "subject"))
            }
        );
    }
    if v.get("truncated").and_then(Value::as_bool) == Some(true) {
        let _ = writeln!(out, "... {} messages in all", n(v, "total"));
    }
    out
}

/// `fleet msg show`.
pub fn render_msg_show(v: &Value) -> String {
    let m = v.get("message").cloned().unwrap_or(Value::Null);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "message {} on {}/{} [{}] {}",
        s(&m, "id"),
        s(&m, "run"),
        s(&m, "channel"),
        s(&m, "kind"),
        s(&m, "status")
    );
    let _ = writeln!(out, "{}", s(&m, "label"));
    if !s(&m, "reason").is_empty() {
        let _ = writeln!(out, "blocked: {} {}", s(&m, "reason"), s(&m, "detail"));
    }
    let _ = writeln!(out, "subject: {}", s(&m, "subject"));
    let _ = writeln!(out, "digest: {}", s(&m, "digest"));
    for r in list(&m, "to") {
        let _ = writeln!(
            out,
            "  to {}: {} {}",
            s(r, "task"),
            s(r, "state"),
            s(r, "reason")
        );
    }
    if !s(&m, "approval").is_empty() {
        let _ = writeln!(
            out,
            "approval: {} (fleet approve {})",
            s(&m, "approval"),
            s(&m, "approval")
        );
    }
    out.push_str("body (unverified peer content, quoted):\n");
    let body = crate::render::multiline(m.get("body").and_then(Value::as_str).unwrap_or(""));
    for line in body.lines() {
        let _ = writeln!(out, "> {line}");
    }
    out
}

/// `fleet msg send`.
pub fn render_msg_sent(v: &Value) -> String {
    format!("message {} queued on {}\n", s(v, "id"), s(v, "channel"))
}

/// `fleet msg mute` and `unmute`.
pub fn render_msg_mute(v: &Value) -> String {
    let muted = v.get("muted").and_then(Value::as_bool) == Some(true);
    format!(
        "{} {} {}\n",
        s(v, "kind"),
        s(v, "target"),
        if muted { "muted" } else { "unmuted" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(body: &str) -> Value {
        Value::object([(
            "message",
            Value::object([
                ("id", Value::str("m3")),
                ("run", Value::str("r1")),
                ("channel", Value::str("talk")),
                ("kind", Value::str("info")),
                ("status", Value::str("delivered")),
                ("label", Value::str("from task a (unverified peer message)")),
                ("subject", Value::str("hello")),
                ("digest", Value::str("abc")),
                ("body", Value::str(body)),
            ]),
        )])
    }

    #[test]
    fn a_body_is_quoted_line_by_line_so_nothing_can_pose_as_a_field() {
        let shown = render_msg_show(&message("line one\nline two\n\nsubject: forged"));
        let quoted: Vec<&str> = shown.lines().filter(|l| l.starts_with("> ")).collect();
        assert_eq!(
            quoted,
            ["> line one", "> line two", "> ", "> subject: forged"],
            "{shown}"
        );
        // the label is there, and no unquoted line was written by the peer
        assert!(shown.contains("from task a (unverified peer message)"));
        assert_eq!(
            shown.lines().filter(|l| l.starts_with("subject:")).count(),
            1,
            "only the real field starts a line: {shown}"
        );
    }

    #[test]
    fn terminal_controls_in_a_body_are_removed_but_lines_stay() {
        let shown = render_msg_show(&message("a\u{1b}[2J\u{202e}b\nc\u{7}d"));
        assert!(
            !shown.contains('\u{1b}') && !shown.contains('\u{202e}') && !shown.contains('\u{7}')
        );
        assert!(shown.contains("> a") && shown.contains("> c"), "{shown}");
    }

    #[test]
    fn a_task_muted_on_one_channel_says_which() {
        let list = Value::object([
            ("schema", Value::str("fleet.msg.list/1")),
            ("channels", Value::array([])),
            (
                "muted_tasks",
                Value::array([
                    Value::object([
                        ("run", Value::str("r1")),
                        ("task", Value::str("a")),
                        ("channel", Value::str("talk")),
                        ("reason", Value::str("reply-depth")),
                    ]),
                    Value::object([
                        ("run", Value::str("r1")),
                        ("task", Value::str("b")),
                        ("channel", Value::Null),
                        ("reason", Value::str("refusal-budget")),
                    ]),
                ]),
            ),
            ("messages", Value::array([])),
        ]);
        let shown = render_msg_list(&list);
        assert!(
            shown.contains("task r1/a is muted on channel talk (reply-depth)"),
            "{shown}"
        );
        assert!(
            shown.contains("task r1/b is muted (refusal-budget)"),
            "{shown}"
        );
    }
}
