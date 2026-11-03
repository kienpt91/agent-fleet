//! Read models of the message channels: the documents `msg.list` and `msg.show` return.
//!
//! Every message is shown with its label (`from task <id> (unverified peer message)`), its text
//! already sanitized and redacted by the domain. The documents change nothing.

use fleet_domain::approval::Approval;
use fleet_domain::fleet::FleetState;
use fleet_domain::ids::{ChannelName, RunId, TaskId};
use fleet_domain::msg::board::MessageBoard;
use fleet_domain::msg::channel::{Channel, Member, MuteNote};
use fleet_domain::msg::message::{DeliveryState, Message, Sender};
use fleet_domain::status::{ApprovalStatus, MessageStatus};
use fleet_domain::value::Value;

/// Messages one `msg.list` answers with, at most.
pub const LIST_LIMIT: usize = 500;
pub const DEFAULT_LIST_LIMIT: usize = 100;

fn opt_str(s: Option<&str>) -> Value {
    s.map_or(Value::Null, Value::str)
}

fn approval_pending(state: &FleetState, m: &Message, now_ms: u64) -> bool {
    m.approval
        .as_ref()
        .and_then(|a| state.approval(a).ok())
        .is_some_and(|a: &Approval| a.status_at(now_ms) == ApprovalStatus::Pending)
}

pub fn status_of(state: &FleetState, m: &Message, now_ms: u64) -> MessageStatus {
    m.status(approval_pending(state, m, now_ms))
}

/// Counts of one channel's messages.
fn counts(
    board: &MessageBoard,
    state: &FleetState,
    run: &RunId,
    name: &ChannelName,
    now: u64,
) -> Value {
    let (mut queued, mut delivered, mut expired, mut blocked, mut waiting) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    for m in board
        .messages()
        .filter(|m| &m.run == run && m.channel == name.as_str())
    {
        match status_of(state, m, now) {
            MessageStatus::Queued => queued += 1,
            MessageStatus::AwaitingApproval => waiting += 1,
            MessageStatus::Delivered => delivered += 1,
            MessageStatus::Expired => expired += 1,
            MessageStatus::Blocked => blocked += 1,
        }
    }
    Value::object([
        ("queued", Value::int(queued)),
        ("awaiting_approval", Value::int(waiting)),
        ("delivered", Value::int(delivered)),
        ("expired", Value::int(expired)),
        ("blocked", Value::int(blocked)),
    ])
}

pub fn channel_value(c: &Channel, board: &MessageBoard, state: &FleetState, now_ms: u64) -> Value {
    let def = &c.def;
    let (reason, by, gate) = match &c.muted {
        Some(n) => (
            Value::str(n.reason.as_str()),
            Value::str(&n.by),
            n.gate
                .as_ref()
                .map_or(Value::Null, |g| Value::str(g.as_str())),
        ),
        None => (Value::Null, Value::Null, Value::Null),
    };
    Value::object([
        ("run", Value::str(c.run.as_str())),
        ("name", Value::str(def.name.as_str())),
        (
            "members",
            Value::array(def.members.iter().map(|m| Value::str(m.text()))),
        ),
        (
            "lead",
            def.lead
                .as_ref()
                .map_or(Value::Null, |l: &Member| Value::str(l.text())),
        ),
        ("mode", Value::str(def.mode.as_str())),
        ("direction", Value::str(def.direction.as_str())),
        ("requires_approval", Value::Bool(def.requires_approval)),
        (
            "max_message_bytes",
            Value::int(def.max_message_bytes as u64),
        ),
        (
            "max_messages_per_task",
            Value::int(def.max_messages_per_task),
        ),
        (
            "max_messages_per_minute",
            Value::int(def.max_messages_per_minute),
        ),
        ("ttl_secs", Value::int(def.ttl_ms / 1000)),
        ("muted", Value::Bool(c.muted.is_some())),
        ("mute_reason", reason),
        ("mute_by", by),
        ("mute_gate", gate),
        ("messages", counts(board, state, &c.run, &def.name, now_ms)),
    ])
}

fn recipient_value(r: &fleet_domain::msg::message::Recipient) -> Value {
    let (state, at, reason) = match &r.state {
        DeliveryState::Pending => ("pending", Value::Null, Value::Null),
        DeliveryState::Delivered { at_ms, .. } => ("delivered", Value::int(*at_ms), Value::Null),
        DeliveryState::Expired { reason, at_ms } => {
            ("expired", Value::int(*at_ms), Value::str(reason.as_str()))
        }
    };
    Value::object([
        ("task", Value::str(r.task.as_str())),
        ("state", Value::str(state)),
        ("at_ms", at),
        ("reason", reason),
    ])
}

/// One message. `body` is included only when `full`: a list shows the subject.
pub fn message_value(m: &Message, status: MessageStatus, full: bool) -> Value {
    let (reason, detail) = match &m.refused {
        Some(r) => (Value::str(r.reason.as_str()), Value::str(&r.detail)),
        None => (Value::Null, Value::Null),
    };
    let mut pairs = vec![
        ("id", Value::str(m.id.as_str())),
        ("run", Value::str(m.run.as_str())),
        ("channel", Value::str(&m.channel)),
        ("from", Value::str(m.from.text())),
        ("label", Value::str(m.label())),
        ("kind", Value::str(m.kind.as_str())),
        ("subject", Value::str(&m.subject)),
        ("status", Value::str(status.as_str())),
        ("reason", reason),
        ("detail", detail),
        ("bytes", Value::int(m.bytes)),
        (
            "in_reply_to",
            opt_str(m.in_reply_to.as_ref().map(|i| i.as_str())),
        ),
        ("depth", Value::int(m.depth)),
        ("at_ms", Value::int(m.submitted_ms)),
        ("expires_ms", Value::int(m.expires_ms)),
        ("approval", opt_str(m.approval.as_ref().map(|a| a.as_str()))),
        ("to", Value::array(m.recipients.iter().map(recipient_value))),
    ];
    if full {
        pairs.push(("body", Value::str(&m.body)));
        pairs.push(("digest", Value::str(&m.digest)));
        pairs.push(("source", opt_str(m.source.as_deref())));
    }
    Value::object(pairs)
}

/// What `msg.list` can be narrowed to.
#[derive(Clone, Debug, Default)]
pub struct ListFilter {
    pub channel: Option<(RunId, ChannelName)>,
    pub task: Option<(RunId, TaskId)>,
    pub limit: usize,
}

fn involves(m: &Message, run: &RunId, task: &TaskId) -> bool {
    &m.run == run
        && (m.from == Sender::Task(task.clone()) || m.recipients.iter().any(|r| &r.task == task))
}

/// The channels and the newest messages, newest first.
pub fn list_value(
    board: &MessageBoard,
    state: &FleetState,
    filter: &ListFilter,
    now_ms: u64,
) -> Value {
    let channels: Vec<Value> = board
        .channels()
        .filter(|c| {
            filter
                .channel
                .as_ref()
                .is_none_or(|(r, n)| *r == c.run && *n == c.def.name)
                && filter.task.as_ref().is_none_or(|(r, _)| *r == c.run)
        })
        .map(|c| channel_value(c, board, state, now_ms))
        .collect();
    let mut matching: Vec<&Message> = board
        .messages()
        .filter(|m| {
            filter
                .channel
                .as_ref()
                .is_none_or(|(r, n)| *r == m.run && m.channel == n.as_str())
                && filter.task.as_ref().is_none_or(|(r, t)| involves(m, r, t))
        })
        .collect();
    matching.sort_by_key(|m| std::cmp::Reverse(m.id.seq()));
    let total = matching.len();
    let limit = filter.limit.clamp(1, LIST_LIMIT);
    let shown: Vec<Value> = matching
        .into_iter()
        .take(limit)
        .map(|m| message_value(m, status_of(state, m, now_ms), false))
        .collect();
    let mute_row = |run: &str, task: &str, channel: Option<&str>, note: &MuteNote| {
        Value::object([
            ("run", Value::str(run)),
            ("task", Value::str(task)),
            // set when the task is muted on that channel only
            ("channel", channel.map_or(Value::Null, Value::str)),
            ("reason", Value::str(note.reason.as_str())),
            ("by", Value::str(&note.by)),
            (
                "gate",
                note.gate
                    .as_ref()
                    .map_or(Value::Null, |g| Value::str(g.as_str())),
            ),
        ])
    };
    let mut muted_tasks: Vec<Value> = board
        .muted_tasks()
        .map(|((run, task), note)| mute_row(run.as_str(), task.as_str(), None, note))
        .collect();
    muted_tasks.extend(board.muted_members().map(|((run, channel, task), note)| {
        mute_row(run.as_str(), task.as_str(), Some(channel.as_str()), note)
    }));
    Value::object([
        ("schema", Value::str("fleet.msg.list/1")),
        ("channels", Value::array(channels)),
        ("muted_tasks", Value::array(muted_tasks)),
        ("messages", Value::array(shown)),
        ("total", Value::int(total as u64)),
        ("truncated", Value::Bool(total > limit)),
    ])
}

pub fn show_value(m: &Message, status: MessageStatus) -> Value {
    Value::object([
        ("schema", Value::str("fleet.msg.show/1")),
        ("message", message_value(m, status, true)),
    ])
}

const _: () = assert!(DEFAULT_LIST_LIMIT <= LIST_LIMIT);
