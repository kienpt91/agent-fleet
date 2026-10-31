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
        let _ = writeln!(
            out,
            "task {}/{} is muted ({})",
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
    for line in s(&m, "body").lines() {
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
