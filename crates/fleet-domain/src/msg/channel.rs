//! Channels: who may talk to whom. Declared by the operator in a plan, never by a worker. A task
//! that is not a member of any channel can neither send nor receive anything.
//!
//! ```toml
//! [[channels]]
//! name = "review"
//! members = ["task:api", "role:reviewer"]   # tasks by id, workers by the name in fleet.toml
//! mode = "direct"                           # or "broadcast"
//! direction = "both"                        # or "to_lead" (then `lead` names the receiving member)
//! max_message_bytes = 2048                  # at most 8192
//! max_messages_per_task = 20
//! max_messages_per_minute = 10
//! ttl_secs = 900
//! requires_approval = false
//! ```

use crate::error::{FleetError, Result};
use crate::events::DomainEvent;
use crate::ids::{show, ChannelName, RunId, TaskId, WorkerName};
use crate::msg::limits::*;
use crate::status::{ChannelDirection, ChannelMode, MuteReason, TaskStatus};
use crate::value::Value;
use std::collections::BTreeMap;

/// One entry of a channel's member list: a task by id or every task a worker runs.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Member {
    Task(TaskId),
    /// Every task whose worker (the one it names, or the default) is this one.
    Role(WorkerName),
}

impl Member {
    /// `task:<id>` or `role:<worker>`. A bare word is refused: whether it names a task or a
    /// worker decides who may talk, so it is never guessed.
    pub fn parse(text: &str) -> Result<Member> {
        if text.len() > MAX_MEMBER_TEXT {
            return Err(FleetError::invalid("a channel member entry is too long"));
        }
        if let Some(id) = text.strip_prefix("task:") {
            return Ok(Member::Task(TaskId::parse(id)?));
        }
        if let Some(name) = text.strip_prefix("role:") {
            return Ok(Member::Role(WorkerName::parse(name)?));
        }
        Err(FleetError::invalid(format!(
            "channel member {} must be written task:<id> or role:<worker>",
            show(text)
        )))
    }

    pub fn text(&self) -> String {
        match self {
            Member::Task(t) => format!("task:{t}"),
            Member::Role(w) => format!("role:{w}"),
        }
    }

    pub fn matches(&self, task: &TaskId, worker: Option<&WorkerName>) -> bool {
        match self {
            Member::Task(t) => t == task,
            Member::Role(w) => worker == Some(w),
        }
    }
}

/// What a channel is, as the operator wrote it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelDef {
    pub name: ChannelName,
    pub members: Vec<Member>,
    /// The member that receives in a `to_lead` channel.
    pub lead: Option<Member>,
    pub mode: ChannelMode,
    pub direction: ChannelDirection,
    pub max_message_bytes: usize,
    /// Messages one task may send on this channel, in total.
    pub max_messages_per_task: u32,
    /// Messages one task may send on this channel in one minute.
    pub max_messages_per_minute: u32,
    pub ttl_ms: u64,
    /// Every message waits for a person's approval before it is delivered.
    pub requires_approval: bool,
}

const KEYS: [&str; 10] = [
    "name",
    "members",
    "lead",
    "mode",
    "direction",
    "max_message_bytes",
    "max_messages_per_task",
    "max_messages_per_minute",
    "ttl_secs",
    "requires_approval",
];

fn bounded_u64(v: &Value, key: &str, min: u64, max: u64, default: u64) -> Result<u64> {
    match v.get(key) {
        None => Ok(default),
        Some(n) => match n.as_u64() {
            Some(n) if (min..=max).contains(&n) => Ok(n),
            _ => Err(FleetError::invalid(format!(
                "channel {key} must be a whole number between {min} and {max}"
            ))),
        },
    }
}

impl ChannelDef {
    /// Parses and validates a channel from a plan. Unknown keys are errors.
    pub fn from_value(v: &Value) -> Result<ChannelDef> {
        let obj = v
            .as_object()
            .ok_or_else(|| FleetError::invalid("a channel must be a table"))?;
        if let Some(key) = obj.keys().find(|k| !KEYS.contains(&k.as_str())) {
            return Err(FleetError::invalid(format!(
                "channel: unknown key {}",
                show(key)
            )));
        }
        let name = ChannelName::parse(
            v.get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| FleetError::invalid("channel: name is required"))?,
        )?;
        let members = match v.get("members") {
            Some(Value::Array(items)) => items
                .iter()
                .map(|i| match i.as_str() {
                    Some(s) => Member::parse(s),
                    None => Err(FleetError::invalid("channel members must be strings")),
                })
                .collect::<Result<Vec<_>>>()?,
            _ => return Err(FleetError::invalid("channel: members must be a list")),
        };
        let lead = match v.get("lead") {
            None | Some(Value::Null) => None,
            Some(Value::Str(s)) => Some(Member::parse(s)?),
            Some(_) => return Err(FleetError::invalid("channel lead must be a string")),
        };
        let mode = match v.get("mode") {
            None => ChannelMode::Direct,
            Some(m) => m
                .as_str()
                .and_then(ChannelMode::parse)
                .ok_or_else(|| FleetError::invalid("channel mode must be broadcast or direct"))?,
        };
        let direction = match v.get("direction") {
            None => ChannelDirection::Both,
            Some(d) => d
                .as_str()
                .and_then(ChannelDirection::parse)
                .ok_or_else(|| FleetError::invalid("channel direction must be both or to_lead"))?,
        };
        let requires_approval = match v.get("requires_approval") {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(FleetError::invalid(
                    "channel requires_approval must be true or false",
                ))
            }
        };
        let def = ChannelDef {
            name,
            members,
            lead,
            mode,
            direction,
            max_message_bytes: bounded_u64(
                v,
                "max_message_bytes",
                MIN_MESSAGE_BYTES as u64,
                HARD_MESSAGE_BYTES as u64,
                DEFAULT_MESSAGE_BYTES as u64,
            )? as usize,
            max_messages_per_task: bounded_u64(
                v,
                "max_messages_per_task",
                1,
                u64::from(HARD_PER_TASK),
                u64::from(DEFAULT_PER_TASK),
            )? as u32,
            max_messages_per_minute: bounded_u64(
                v,
                "max_messages_per_minute",
                1,
                u64::from(HARD_PER_MINUTE),
                u64::from(DEFAULT_PER_MINUTE),
            )? as u32,
            ttl_ms: bounded_u64(v, "ttl_secs", MIN_TTL_SECS, MAX_TTL_SECS, DEFAULT_TTL_SECS)?
                .saturating_mul(1000),
            requires_approval,
        };
        def.validate()?;
        Ok(def)
    }

    /// Rules that hold for any channel, however it was built.
    pub fn validate(&self) -> Result<()> {
        let n = self.members.len();
        if n == 0 || n > MAX_MEMBERS {
            return Err(FleetError::invalid(format!(
                "channel {}: it needs between 1 and {MAX_MEMBERS} members",
                self.name
            )));
        }
        let mut seen: Vec<&Member> = Vec::new();
        for m in &self.members {
            if seen.contains(&m) {
                return Err(FleetError::invalid(format!(
                    "channel {}: member {} is listed twice",
                    self.name,
                    m.text()
                )));
            }
            seen.push(m);
        }
        let has_role = self.members.iter().any(|m| matches!(m, Member::Role(_)));
        if n < 2 && !has_role {
            return Err(FleetError::invalid(format!(
                "channel {}: a channel of tasks needs at least two members",
                self.name
            )));
        }
        match (&self.direction, &self.lead) {
            (ChannelDirection::ToLead, None) => {
                return Err(FleetError::invalid(format!(
                    "channel {}: direction to_lead needs a lead",
                    self.name
                )))
            }
            (ChannelDirection::ToLead, Some(l)) if !self.members.contains(l) => {
                return Err(FleetError::invalid(format!(
                    "channel {}: the lead {} is not a member",
                    self.name,
                    l.text()
                )))
            }
            (ChannelDirection::Both, Some(_)) => {
                return Err(FleetError::invalid(format!(
                    "channel {}: a lead only means something with direction to_lead",
                    self.name
                )))
            }
            _ => {}
        }
        if !(MIN_MESSAGE_BYTES..=HARD_MESSAGE_BYTES).contains(&self.max_message_bytes) {
            return Err(FleetError::invalid(format!(
                "channel {}: max_message_bytes must be between {MIN_MESSAGE_BYTES} and {HARD_MESSAGE_BYTES}",
                self.name
            )));
        }
        Ok(())
    }

    /// The record of this channel opening for `run`.
    pub fn opened_event(&self, run: &RunId) -> DomainEvent {
        DomainEvent::ChannelOpened {
            run: run.clone(),
            channel: self.name.clone(),
            members: self.members.iter().map(Member::text).collect(),
            lead: self.lead.as_ref().map(Member::text),
            mode: self.mode,
            direction: self.direction,
            max_message_bytes: self.max_message_bytes as u64,
            max_messages_per_task: u64::from(self.max_messages_per_task),
            max_messages_per_minute: u64::from(self.max_messages_per_minute),
            ttl_secs: self.ttl_ms / 1000,
            requires_approval: self.requires_approval,
        }
    }

    /// Rebuilds a channel from its event; `None` for a record that is not a valid channel.
    pub fn from_event(ev: &DomainEvent) -> Option<(RunId, ChannelDef)> {
        let DomainEvent::ChannelOpened {
            run,
            channel,
            members,
            lead,
            mode,
            direction,
            max_message_bytes,
            max_messages_per_task,
            max_messages_per_minute,
            ttl_secs,
            requires_approval,
        } = ev
        else {
            return None;
        };
        let def = ChannelDef {
            name: channel.clone(),
            members: members
                .iter()
                .map(|m| Member::parse(m))
                .collect::<Result<Vec<_>>>()
                .ok()?,
            lead: match lead {
                Some(l) => Some(Member::parse(l).ok()?),
                None => None,
            },
            mode: *mode,
            direction: *direction,
            max_message_bytes: usize::try_from(*max_message_bytes).ok()?,
            max_messages_per_task: u32::try_from(*max_messages_per_task).ok()?,
            max_messages_per_minute: u32::try_from(*max_messages_per_minute).ok()?,
            ttl_ms: ttl_secs.saturating_mul(1000),
            requires_approval: *requires_approval,
        };
        def.validate().ok()?;
        Some((run.clone(), def))
    }

    pub fn is_member(&self, task: &TaskId, worker: Option<&WorkerName>) -> bool {
        self.members.iter().any(|m| m.matches(task, worker))
    }

    pub fn is_lead(&self, task: &TaskId, worker: Option<&WorkerName>) -> bool {
        self.lead.as_ref().is_some_and(|l| l.matches(task, worker))
    }
}

/// A task as the channels see it: who it is, which worker runs it and how far along it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RosterEntry {
    pub task: TaskId,
    /// The worker the task runs (the one it names, or the configured default).
    pub worker: Option<WorkerName>,
    pub status: TaskStatus,
    /// A worker session is attached right now.
    pub live: bool,
}

impl RosterEntry {
    /// Completed or failed: it will not take part in anything again unless an operator retries it.
    pub fn ended(&self) -> bool {
        self.status.is_terminal()
    }
}

pub fn roster_find<'a>(roster: &'a [RosterEntry], task: &TaskId) -> Option<&'a RosterEntry> {
    roster.iter().find(|e| &e.task == task)
}

/// Why a channel or a task is muted, who did it, and (for an automatic mute) the gate a person
/// answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MuteNote {
    pub reason: MuteReason,
    pub by: String,
    pub gate: Option<crate::ids::DecisionId>,
}

/// A channel as it stands: its definition and whether it is muted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Channel {
    pub run: RunId,
    pub def: ChannelDef,
    pub muted: Option<MuteNote>,
}

impl Channel {
    /// The tasks of the roster that belong to this channel.
    pub fn member_tasks<'a>(&self, roster: &'a [RosterEntry]) -> Vec<&'a RosterEntry> {
        roster
            .iter()
            .filter(|e| self.def.is_member(&e.task, e.worker.as_ref()))
            .collect()
    }

    /// Whether fewer than two members can still take part, or the lead is gone: nobody is left
    /// to talk to, so the channel is muted.
    pub fn is_dead(&self, roster: &[RosterEntry]) -> bool {
        let members = self.member_tasks(roster);
        let alive = members.iter().filter(|e| !e.ended()).count();
        if alive < 2 {
            return true;
        }
        self.def.direction == ChannelDirection::ToLead
            && !members
                .iter()
                .any(|e| !e.ended() && self.def.is_lead(&e.task, e.worker.as_ref()))
    }
}

/// Sorted by name, for stable output.
pub fn channels_by_name<'a>(
    channels: impl Iterator<Item = &'a Channel>,
) -> BTreeMap<(RunId, ChannelName), &'a Channel> {
    channels
        .map(|c| ((c.run.clone(), c.def.name.clone()), c))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;

    fn val(pairs: Vec<(&str, Value)>) -> Value {
        Value::object(pairs)
    }

    fn members(list: &[&str]) -> Value {
        Value::array(list.iter().map(|s| Value::str(*s)))
    }

    fn minimal() -> Vec<(&'static str, Value)> {
        vec![
            ("name", Value::str("review")),
            ("members", members(&["task:a", "task:b"])),
        ]
    }

    fn t(s: &str) -> TaskId {
        TaskId::parse(s).unwrap()
    }

    fn w(s: &str) -> WorkerName {
        WorkerName::parse(s).unwrap()
    }

    fn entry(task: &str, worker: &str, status: TaskStatus) -> RosterEntry {
        RosterEntry {
            task: t(task),
            worker: Some(w(worker)),
            status,
            live: status == TaskStatus::Running,
        }
    }

    #[test]
    fn a_minimal_channel_gets_the_documented_defaults() {
        let c = ChannelDef::from_value(&val(minimal())).unwrap();
        assert_eq!(c.mode, ChannelMode::Direct);
        assert_eq!(c.direction, ChannelDirection::Both);
        assert_eq!(c.max_message_bytes, 2048);
        assert_eq!(c.max_messages_per_task, 20);
        assert_eq!(c.max_messages_per_minute, DEFAULT_PER_MINUTE);
        assert_eq!(c.ttl_ms, 900_000);
        assert!(
            !c.requires_approval,
            "low-risk text channels need no approval"
        );
        assert!(c.lead.is_none());
    }

    #[test]
    fn every_key_is_read_and_the_event_round_trips() {
        let c = ChannelDef::from_value(&val(vec![
            ("name", Value::str("lead-ch")),
            ("members", members(&["task:a", "role:reviewer", "task:c"])),
            ("lead", Value::str("role:reviewer")),
            ("mode", Value::str("broadcast")),
            ("direction", Value::str("to_lead")),
            ("max_message_bytes", Value::int(4096)),
            ("max_messages_per_task", Value::int(5)),
            ("max_messages_per_minute", Value::int(3)),
            ("ttl_secs", Value::int(60)),
            ("requires_approval", Value::Bool(true)),
        ]))
        .unwrap();
        assert_eq!(c.mode, ChannelMode::Broadcast);
        assert_eq!(c.direction, ChannelDirection::ToLead);
        assert_eq!(c.lead, Some(Member::Role(w("reviewer"))));
        assert_eq!(c.max_message_bytes, 4096);
        assert_eq!(c.ttl_ms, 60_000);
        assert!(c.requires_approval);
        let run = RunId::parse("r1").unwrap();
        let ev = c.opened_event(&run);
        let (r, back) = ChannelDef::from_event(&ev).unwrap();
        assert_eq!(r, run);
        assert_eq!(back, c);
        // and through the log's own encoding
        let decoded = DomainEvent::from_value(&ev.to_value()).unwrap();
        assert_eq!(decoded, ev);
    }

    #[test]
    fn unknown_keys_and_wrong_types_are_errors() {
        let mut bad = minimal();
        bad.push(("colour", Value::str("red")));
        assert!(ChannelDef::from_value(&val(bad)).is_err());
        for (key, value) in [
            ("mode", Value::str("multicast")),
            ("direction", Value::str("sideways")),
            ("max_message_bytes", Value::str("big")),
            ("max_message_bytes", Value::int(63)),
            ("max_message_bytes", Value::int(8193)),
            ("max_messages_per_task", Value::int(0)),
            ("max_messages_per_task", Value::int(201)),
            ("max_messages_per_minute", Value::int(121)),
            ("ttl_secs", Value::int(9)),
            ("ttl_secs", Value::int(86_401)),
            ("requires_approval", Value::str("yes")),
            ("lead", Value::int(1)),
        ] {
            let mut doc = minimal();
            doc.push((key, value.clone()));
            assert!(
                ChannelDef::from_value(&val(doc)).is_err(),
                "{key} = {value:?}"
            );
        }
        assert!(ChannelDef::from_value(&Value::str("review")).is_err());
        assert!(ChannelDef::from_value(&val(vec![("members", members(&["task:a"]))])).is_err());
    }

    #[test]
    fn members_must_say_whether_they_are_tasks_or_roles() {
        assert!(Member::parse("task:api").is_ok());
        assert!(Member::parse("role:reviewer").is_ok());
        for bad in [
            "api",
            "task:",
            "role:",
            "team:api",
            "task:A",
            "task:a b",
            &"task:".repeat(20),
        ] {
            assert!(Member::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Member::parse("task:api").unwrap().text(), "task:api");
        let mut doc = minimal();
        doc.retain(|(k, _)| *k != "members");
        doc.push(("members", members(&["a", "b"])));
        assert!(ChannelDef::from_value(&val(doc)).is_err());
    }

    #[test]
    fn member_lists_are_checked() {
        let with = |list: &[&str]| {
            let mut doc = minimal();
            doc.retain(|(k, _)| *k != "members");
            doc.push(("members", members(list)));
            ChannelDef::from_value(&val(doc))
        };
        assert!(with(&[]).is_err());
        assert!(with(&["task:a"]).is_err(), "one task cannot talk to itself");
        assert!(
            with(&["role:worker"]).is_ok(),
            "a role may stand for several tasks"
        );
        assert!(with(&["task:a", "task:a"]).is_err());
        let many: Vec<String> = (0..=MAX_MEMBERS).map(|i| format!("task:t{i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(with(&many).is_err());
    }

    #[test]
    fn to_lead_needs_a_lead_that_is_a_member_and_both_forbids_one() {
        let with = |direction: &str, lead: Option<&str>| {
            let mut doc = minimal();
            doc.push(("direction", Value::str(direction)));
            if let Some(l) = lead {
                doc.push(("lead", Value::str(l)));
            }
            ChannelDef::from_value(&val(doc))
        };
        assert!(with("to_lead", Some("task:a")).is_ok());
        assert!(with("to_lead", None).is_err());
        assert!(with("to_lead", Some("task:z")).is_err());
        assert!(with("both", Some("task:a")).is_err());
        assert!(with("both", None).is_ok());
    }

    #[test]
    fn membership_follows_task_ids_and_roles() {
        let c = ChannelDef::from_value(&val(vec![
            ("name", Value::str("x")),
            ("members", members(&["task:a", "role:rev"])),
        ]))
        .unwrap();
        assert!(c.is_member(&t("a"), Some(&w("other"))));
        assert!(c.is_member(&t("z"), Some(&w("rev"))));
        assert!(!c.is_member(&t("z"), Some(&w("other"))));
        assert!(!c.is_member(&t("z"), None));
    }

    #[test]
    fn a_channel_is_dead_when_fewer_than_two_members_can_take_part() {
        let def = ChannelDef::from_value(&val(vec![
            ("name", Value::str("x")),
            ("members", members(&["task:a", "task:b", "task:c"])),
        ]))
        .unwrap();
        let ch = Channel {
            run: RunId::parse("r1").unwrap(),
            def,
            muted: None,
        };
        let roster = |a, b, c| vec![entry("a", "w", a), entry("b", "w", b), entry("c", "w", c)];
        use TaskStatus::*;
        assert!(!ch.is_dead(&roster(Running, Running, Pending)));
        assert!(!ch.is_dead(&roster(Completed, Running, Running)));
        assert!(ch.is_dead(&roster(Completed, Failed, Running)));
        assert!(ch.is_dead(&roster(Completed, Completed, Completed)));
        // a task that is only waiting still counts
        assert!(!ch.is_dead(&roster(Completed, Pending, Ready)));
    }

    #[test]
    fn a_to_lead_channel_is_dead_when_the_lead_is_gone() {
        let def = ChannelDef::from_value(&val(vec![
            ("name", Value::str("x")),
            ("members", members(&["task:a", "task:b", "task:c"])),
            ("direction", Value::str("to_lead")),
            ("lead", Value::str("task:a")),
        ]))
        .unwrap();
        let ch = Channel {
            run: RunId::parse("r1").unwrap(),
            def,
            muted: None,
        };
        use TaskStatus::*;
        let alive = vec![
            entry("a", "w", Running),
            entry("b", "w", Running),
            entry("c", "w", Running),
        ];
        assert!(!ch.is_dead(&alive));
        let lead_gone = vec![
            entry("a", "w", Completed),
            entry("b", "w", Running),
            entry("c", "w", Running),
        ];
        assert!(ch.is_dead(&lead_gone));
        assert_eq!(ch.member_tasks(&alive).len(), 3);
    }

    #[test]
    fn channels_are_listed_by_run_and_name() {
        let mk = |run: &str, name: &str| Channel {
            run: RunId::parse(run).unwrap(),
            def: ChannelDef::from_value(&val(vec![
                ("name", Value::str(name)),
                ("members", members(&["task:a", "task:b"])),
            ]))
            .unwrap(),
            muted: None,
        };
        let all = [mk("r2", "b"), mk("r1", "z"), mk("r1", "a")];
        let names: Vec<String> = channels_by_name(all.iter())
            .keys()
            .map(|(r, n)| format!("{r}/{n}"))
            .collect();
        assert_eq!(names, ["r1/a", "r1/z", "r2/b"]);
    }
}
