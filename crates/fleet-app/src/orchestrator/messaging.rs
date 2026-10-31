//! Messages between agents: reading outboxes, deciding, delivering, muting, and the operator's
//! commands. The daemon is the only party: a worker writes a file, the daemon reads it with the
//! care it gives every worker file, decides under the policy of the channel and the daemon,
//! records the decision and writes accepted messages into the inbox of a running recipient. Nothing
//! is typed into a terminal unless the operator turned `notify_input` on.
//!
//! The text of a message is untrusted peer content. It is stored sanitized and redacted, shown
//! with its label and kept out of everything the daemon treats as true (approvals name the
//! message and its digest, never its words; gates and logs name ids and numbers).

use super::{Key, Orchestrator};
use crate::api::Reply;
use crate::msg::{list_value, show_value, status_of, ListFilter};
use crate::ports::{InboxReport, OutboxItem, OutboxRead};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::events::DomainEvent;
use fleet_domain::ids::{ChannelName, MessageId, RunId, TaskId};
use fleet_domain::msg::board::{
    Accepted, Context, Due, Refused, Submission, TripInfo, Verdict, GATE_KEEP, GATE_UNMUTE,
};
use fleet_domain::msg::channel::{ChannelDef, Member, RosterEntry};
use fleet_domain::msg::inbox::{notice_line, render_item};
use fleet_domain::msg::limits::MAX_OUTBOX_PER_SCAN;
use fleet_domain::msg::message::{OutboxMessage, Sender};
use fleet_domain::status::{ApprovalStatus, BlockReason, MessageKind, MuteReason};
use fleet_domain::value::Value;
use std::collections::BTreeSet;

const OPERATOR: &str = "operator";
const AUTO: &str = "auto";

fn data(schema: &str, pairs: Vec<(&str, Value)>) -> Reply {
    let mut map: Vec<(String, Value)> = vec![("schema".to_string(), Value::str(schema))];
    map.extend(pairs.into_iter().map(|(k, v)| (k.to_string(), v)));
    Reply::Data(Value::object(map))
}

/// What `fleet msg mute` and `unmute` name.
enum Target {
    Channel(RunId, ChannelName),
    Task(RunId, TaskId),
}

impl Orchestrator {
    /// The tasks of a run as the channels see them.
    pub(crate) fn roster(&self, run: &RunId) -> Vec<RosterEntry> {
        let Ok(r) = self.state.run(run) else {
            return Vec::new();
        };
        r.tasks()
            .iter()
            .map(|t| RosterEntry {
                task: t.id.clone(),
                worker: t.worker.clone().or_else(|| self.cfg.default_worker.clone()),
                status: t.status,
                live: self.live.contains_key(&(run.clone(), t.id.clone())),
            })
            .collect()
    }

    // ---- the tick -------------------------------------------------------------------------

    /// One round of message work: channels with nobody left are muted, outboxes are read,
    /// deliveries and expiries happen. Cheap when no channel exists.
    pub(crate) fn messages(&mut self) -> Result<()> {
        if !self.board.has_channels() {
            return Ok(());
        }
        self.mute_dead_channels();
        self.read_outboxes();
        self.deliver_due();
        Ok(())
    }

    fn mute_dead_channels(&mut self) {
        let now = self.now();
        let dead = self.board.dead_channels(&|run| self.roster(run));
        let events: Vec<DomainEvent> = dead
            .iter()
            .filter_map(|(run, name)| {
                self.board
                    .mute_channel(run, name, MuteReason::EndpointEnded, AUTO, None, now)
                    .ok()
            })
            .collect();
        if let Err(e) = self.commit(events) {
            self.warn(e.message);
        }
    }

    // ---- reading outboxes -------------------------------------------------------------------

    fn read_outboxes(&mut self) {
        let keys: Vec<Key> = self.live.keys().cloned().collect();
        for key in keys {
            let Some(live) = self.live.get(&key) else {
                continue;
            };
            let (worktree, attempt) = (live.worktree.clone(), live.attempt);
            let worker = self.roster(&key.0).into_iter().find(|e| e.task == key.1);
            // a task that belongs to no channel cannot send anything: its outbox is never opened
            let member = self
                .board
                .channels_of(
                    &key.0,
                    &key.1,
                    worker.as_ref().and_then(|w| w.worker.as_ref()),
                )
                .is_empty();
            if member || self.board.task_muted(&key.0, &key.1).is_some() {
                continue;
            }
            let scan = self.ports.outbox.scan(&worktree, MAX_OUTBOX_PER_SCAN);
            if let Some(why) = scan.refused {
                self.warn(format!(
                    "{}/{}: the outbox was not read: {why}",
                    key.0, key.1
                ));
                continue;
            }
            for item in scan.items {
                self.take_outbox_file(&key, attempt, &worktree, item);
            }
        }
    }

    fn take_outbox_file(&mut self, key: &Key, attempt: u32, worktree: &str, item: OutboxItem) {
        let sender = Sender::Task(key.1.clone());
        let source = format!("outbox:{attempt}:{}", item.name);
        if self.board.seen_source(&key.0, &sender, &source) {
            // read before (a restart, a rename that failed): it is one message
            let _ = self.ports.outbox.settle(worktree, &item.name, true);
            return;
        }
        let now = self.now();
        let (events, accepted) = match item.read {
            OutboxRead::NotReady => return,
            OutboxRead::Invalid(reason, detail) => {
                let r = self.board.refuse_file(
                    &key.0,
                    &sender,
                    reason,
                    &detail,
                    Some(source),
                    self.ports.digest.as_ref(),
                    now,
                );
                (self.refusal_events(r, now), false)
            }
            OutboxRead::Valid(m) => self.judge(&key.0, &sender, m, source, now),
        };
        match self.commit(events) {
            Ok(()) => {
                if let Err(e) = self.ports.outbox.settle(worktree, &item.name, accepted) {
                    self.warn(format!("{}/{}: {}", key.0, key.1, e.message));
                }
            }
            // not recorded, not settled: the file is read again on the next tick
            Err(e) => self.warn(format!(
                "{}/{}: could not record a message: {}",
                key.0, key.1, e.message
            )),
        }
    }

    /// Submits one message to the board and turns the verdict into events.
    fn judge(
        &mut self,
        run: &RunId,
        sender: &Sender,
        m: OutboxMessage,
        source: String,
        now: u64,
    ) -> (Vec<DomainEvent>, bool) {
        let verdict = self.submit(run, sender, &m, Some(source));
        self.settle_verdict(run, sender, verdict, now)
    }

    fn submit(
        &self,
        run: &RunId,
        sender: &Sender,
        m: &OutboxMessage,
        source: Option<String>,
    ) -> Verdict {
        let roster = self.roster(run);
        let redactor = &self.ports.redact;
        let redact = |text: &str| redactor.redact(text);
        let sub = Submission {
            run,
            from: sender.clone(),
            channel: m.channel.as_str(),
            to: m.to.as_ref(),
            kind: m.kind,
            subject: &m.subject,
            body: &m.body,
            in_reply_to: m.in_reply_to.as_ref(),
            source,
        };
        self.board.submit(
            &sub,
            &Context {
                roster: &roster,
                limits: &self.cfg.messaging,
                now_ms: self.now(),
                hasher: self.ports.digest.as_ref(),
                redact: &redact,
            },
        )
    }

    /// The events a verdict calls for, and whether the message was accepted.
    fn settle_verdict(
        &mut self,
        run: &RunId,
        sender: &Sender,
        verdict: Verdict,
        now: u64,
    ) -> (Vec<DomainEvent>, bool) {
        match verdict {
            Verdict::Refused(r) => (self.refusal_events(r, now), false),
            Verdict::Accepted(a) => match self.accepted_events(run, sender, a, now) {
                Ok(events) => (events, true),
                Err(r) => (self.refusal_events(*r, now), false),
            },
        }
    }

    /// An accepted message, and the approval it waits for when its channel asks for one.
    fn accepted_events(
        &mut self,
        run: &RunId,
        sender: &Sender,
        accepted: Accepted,
        now: u64,
    ) -> std::result::Result<Vec<DomainEvent>, Box<Refused>> {
        let (Some(action), Sender::Task(task)) = (accepted.action.clone(), sender) else {
            return Ok(vec![accepted.event]);
        };
        let ttl_ms = match &accepted.event {
            DomainEvent::MessageSubmitted {
                at_ms, expires_ms, ..
            } => expires_ms.saturating_sub(*at_ms).max(1),
            _ => return Ok(vec![accepted.event]),
        };
        match self.state.request_message_approval(
            run,
            task,
            &action,
            ttl_ms,
            now,
            self.ports.digest.as_ref(),
        ) {
            Ok((id, approval_event)) => {
                let accepted = accepted.with_approval(id);
                Ok(vec![approval_event, accepted.event])
            }
            Err(e) => Err(Box::new(self.board.refuse_file(
                run,
                sender,
                BlockReason::Quota,
                &e.message,
                None,
                self.ports.digest.as_ref(),
                now,
            ))),
        }
    }

    /// The events of a refusal; one that crossed a loop or storm limit also mutes and asks a
    /// person.
    fn refusal_events(&mut self, refused: Refused, now: u64) -> Vec<DomainEvent> {
        let mut events = refused.events;
        if let Some(trip) = refused.trip {
            events.extend(self.trip_events(&trip, now));
        }
        events
    }

    /// Mutes what tripped a limit and opens a decision gate for a person. Nothing a peer wrote is
    /// in the gate: ids and numbers only.
    fn trip_events(&mut self, trip: &TripInfo, now: u64) -> Vec<DomainEvent> {
        let what = match (&trip.channel, &trip.task) {
            (Some(c), _) => format!("Channel {c} of run {}", trip.run),
            (None, Some(t)) => format!("Task {}/{t}", trip.run),
            (None, None) => return Vec::new(),
        };
        let look = match &trip.channel {
            Some(c) => format!("fleet msg list --channel {}/{c}", trip.run),
            None => format!("fleet msg list --task {}/{}", trip.run, trip.sender),
        };
        let text = format!(
            "{what} was muted automatically: {}. Read what was sent with `{look}` (peer text, unverified). Answer unmute to let it carry messages again (its counters start afresh) or keep muted.",
            trip.text
        );
        let mut events = Vec::new();
        let gate = match self.state.raise_channel_gate(
            &trip.run,
            &trip.sender,
            &text,
            &[GATE_UNMUTE, GATE_KEEP],
        ) {
            Ok((id, event)) => {
                events.push(event);
                Some(id)
            }
            Err(e) => {
                self.warn(format!(
                    "no gate could be opened for a muted channel: {}",
                    e.message
                ));
                None
            }
        };
        let mute = match (&trip.channel, &trip.task) {
            (Some(c), _) => self
                .board
                .mute_channel(&trip.run, c, trip.reason, AUTO, gate, now),
            (None, Some(t)) => self
                .board
                .mute_task(&trip.run, t, trip.reason, AUTO, gate, now),
            (None, None) => return Vec::new(),
        };
        match mute {
            Ok(event) => {
                events.push(event);
                events
            }
            // already muted: the gate that was opened for nothing is not recorded either
            Err(_) => Vec::new(),
        }
    }

    // ---- delivering -------------------------------------------------------------------------

    fn deliver_due(&mut self) {
        let now = self.now();
        let due = self.board.due(now, &|run| self.roster(run), &|id| {
            self.state.approval(id).ok().map(|a| a.status_at(now))
        });
        for step in due {
            match step {
                Due::Expire { id, to, reason } => {
                    let ev = DomainEvent::MessageExpired {
                        id,
                        to,
                        reason,
                        at_ms: now,
                    };
                    if let Err(e) = self.commit(vec![ev]) {
                        self.warn(e.message);
                    }
                }
                Due::Deny { id } => self.deny_message(&id, now),
                Due::Deliver { id, to } => self.deliver(&id, &to, now),
            }
        }
    }

    fn deny_message(&mut self, id: &MessageId, now: u64) {
        let Some(m) = self.board.message(id) else {
            return;
        };
        let ev = DomainEvent::MessageBlocked {
            id: id.clone(),
            run: m.run.clone(),
            channel: m.channel.clone(),
            from: m.from.text(),
            reason: BlockReason::Denied,
            detail: "a person denied the approval".to_string(),
            digest: m.digest.clone(),
            bytes: m.bytes,
            at_ms: now,
            source: None,
        };
        if let Err(e) = self.commit(vec![ev]) {
            self.warn(e.message);
        }
    }

    /// Writes one message into the inbox of a running recipient, then records that it arrived.
    /// The file is written first: a crash between the two redelivers the same file under the
    /// same name, and the log still says "delivered" once.
    fn deliver(&mut self, id: &MessageId, to: &TaskId, now: u64) {
        let Some(m) = self.board.message(id).cloned() else {
            return;
        };
        let key: Key = (m.run.clone(), to.clone());
        let Some(live) = self.live.get(&key) else {
            return;
        };
        let (worktree, session) = (live.worktree.clone(), live.session);
        let mut events = Vec::new();
        if let Some(approval_id) = &m.approval {
            let Ok(approval) = self.state.approval(approval_id) else {
                return;
            };
            if approval.status_at(now) == ApprovalStatus::Approved {
                // the one use of the approval: it must still be the approval of this exact message
                match approval.consume(&m.approval_action(), now, self.ports.digest.as_ref()) {
                    Ok(ev) => events.push(ev),
                    Err(e) => {
                        self.warn(format!("message {id} was not delivered: {}", e.message));
                        return;
                    }
                }
            }
        }
        let hasher = self.ports.digest.as_ref();
        let seq = self.board.next_inbox_seq(&m.run, to);
        let mut items = self.board.inbox_items(&m.run, to, hasher);
        items.push(render_item(&m, seq, hasher));
        let report = match self.ports.inbox.sync(&worktree, &items) {
            Ok(r) => r,
            Err(e) => {
                self.warn(format!(
                    "{}/{}: message {id} was not delivered: {}",
                    m.run, to, e.message
                ));
                return;
            }
        };
        self.report_inbox(&key, &report);
        events.push(DomainEvent::MessageDelivered {
            id: id.clone(),
            to: to.clone(),
            at_ms: now,
        });
        match self.commit(events) {
            Ok(()) => {
                if self.cfg.messaging.notify_input {
                    // best effort: a worker that does not read its terminal is not waited for
                    let _ = self.ports.sessions.notify(session, &notice_line(id));
                }
            }
            Err(e) => self.warn(e.message),
        }
    }

    fn report_inbox(&mut self, key: &Key, report: &InboxReport) {
        if !report.removed.is_empty() {
            self.warn(format!(
                "{}/{}: {} file(s) in the inbox were not written by fleet and were removed",
                key.0,
                key.1,
                report.removed.len()
            ));
        }
    }

    // ---- starting a task ------------------------------------------------------------------

    /// Makes the message directories of a task that belongs to a channel, empties its outbox and
    /// puts what was delivered to it before back in its inbox. The notes for its brief.
    pub(crate) fn prepare_messages(
        &mut self,
        run: &RunId,
        task: &TaskId,
        worktree: &str,
    ) -> Result<()> {
        let roster = self.roster(run);
        let worker = roster
            .iter()
            .find(|e| &e.task == task)
            .and_then(|e| e.worker.clone());
        if self
            .board
            .channels_of(run, task, worker.as_ref())
            .is_empty()
        {
            return Ok(());
        }
        let items = self
            .board
            .inbox_items(run, task, self.ports.digest.as_ref());
        let report = self.ports.inbox.prepare(worktree, &items)?;
        self.report_inbox(&(run.clone(), task.clone()), &report);
        Ok(())
    }

    // ---- commands ---------------------------------------------------------------------------

    fn resolve_target(&self, text: &str) -> Result<Target> {
        let channel = |t: &str| {
            self.board
                .find_channel(t)
                .map(|(r, c)| Target::Channel(r, c))
        };
        let task = |t: &str| self.state.find_task(t).map(|(r, k)| Target::Task(r, k));
        if let Some(rest) = text.strip_prefix("channel:") {
            return channel(rest);
        }
        if let Some(rest) = text.strip_prefix("task:") {
            return task(rest);
        }
        match (channel(text), task(text)) {
            (Ok(c), Err(_)) => Ok(c),
            (Err(_), Ok(t)) => Ok(t),
            (Ok(_), Ok(_)) => Err(FleetError::invalid(format!(
                "{} names a channel and a task; write channel:{0} or task:{0}",
                fleet_domain::ids::show(text)
            ))),
            (Err(c), Err(t)) => {
                // a name that is malformed or ambiguous says so; one that is simply unknown too
                if c.code == fleet_domain::error::ErrorCode::NotFound
                    && t.code == fleet_domain::error::ErrorCode::NotFound
                {
                    Err(FleetError::not_found(format!(
                        "no channel or task named {}",
                        fleet_domain::ids::show(text)
                    )))
                } else {
                    Err(c)
                }
            }
        }
    }

    pub(crate) fn msg_mute(&mut self, target: &str, mute: bool) -> Result<Reply> {
        let target = self.resolve_target(target)?;
        let now = self.now();
        let mut events = Vec::new();
        let (kind, name) = match &target {
            Target::Channel(run, name) => {
                if mute {
                    events.push(self.board.mute_channel(
                        run,
                        name,
                        MuteReason::Operator,
                        OPERATOR,
                        None,
                        now,
                    )?);
                } else {
                    events.push(self.board.unmute_channel(run, name, OPERATOR, now)?);
                    let gate = self
                        .board
                        .channel(run, name)
                        .and_then(|c| c.muted.as_ref())
                        .and_then(|n| n.gate.clone());
                    events.extend(self.close_gate(gate));
                }
                ("channel", format!("{run}/{name}"))
            }
            Target::Task(run, task) => {
                if mute {
                    events.push(self.board.mute_task(
                        run,
                        task,
                        MuteReason::Operator,
                        OPERATOR,
                        None,
                        now,
                    )?);
                } else {
                    events.push(self.board.unmute_task(run, task, OPERATOR, now)?);
                    let gate = self
                        .board
                        .task_muted(run, task)
                        .and_then(|n| n.gate.clone());
                    events.extend(self.close_gate(gate));
                }
                ("task", format!("{run}/{task}"))
            }
        };
        self.commit(events)?;
        Ok(data(
            "fleet.msg.mute/1",
            vec![
                ("kind", Value::str(kind)),
                ("target", Value::str(name)),
                ("muted", Value::Bool(mute)),
            ],
        ))
    }

    /// An unmute by hand answers the gate the automatic mute opened, if it is still open.
    fn close_gate(&self, gate: Option<fleet_domain::ids::DecisionId>) -> Vec<DomainEvent> {
        let Some(id) = gate else {
            return Vec::new();
        };
        match self.state.decision(&id) {
            Ok(d) if d.is_open() => self
                .state
                .resolve_decision(&id, GATE_UNMUTE)
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    pub(crate) fn msg_list(
        &self,
        channel: Option<&str>,
        task: Option<&str>,
        limit: usize,
    ) -> Result<Reply> {
        let filter = ListFilter {
            channel: channel.map(|c| self.board.find_channel(c)).transpose()?,
            task: task.map(|t| self.state.find_task(t)).transpose()?,
            limit,
        };
        Ok(Reply::Data(list_value(
            &self.board,
            &self.state,
            &filter,
            self.now(),
        )))
    }

    pub(crate) fn msg_show(&self, id: &MessageId) -> Result<Reply> {
        let m = self
            .board
            .message(id)
            .ok_or_else(|| FleetError::not_found(format!("message {id} does not exist")))?;
        Ok(Reply::Data(show_value(
            m,
            status_of(&self.state, m, self.now()),
        )))
    }

    /// A message from the operator into a channel.
    pub(crate) fn msg_send(
        &mut self,
        channel: &str,
        to: Option<&str>,
        kind: MessageKind,
        subject: &str,
        body: &str,
        in_reply_to: Option<&MessageId>,
    ) -> Result<Reply> {
        let (run, name) = self.board.find_channel(channel)?;
        let to_task = match to {
            Some(t) => {
                let (to_run, task) = self.state.find_task(t)?;
                if to_run != run {
                    return Err(FleetError::invalid(format!(
                        "task {to_run}/{task} is not in run {run}, the run of channel {name}"
                    )));
                }
                Some(task)
            }
            None => None,
        };
        let m = OutboxMessage {
            channel: name,
            to: to_task,
            kind,
            subject: subject.to_string(),
            body: body.to_string(),
            in_reply_to: in_reply_to.cloned(),
        };
        let now = self.now();
        let sender = Sender::Operator;
        let verdict = self.submit(&run, &sender, &m, None);
        let (events, accepted) = self.settle_verdict(&run, &sender, verdict, now);
        let id = events.iter().find_map(|e| match e {
            DomainEvent::MessageSubmitted { id, .. } | DomainEvent::MessageBlocked { id, .. } => {
                Some(id.clone())
            }
            _ => None,
        });
        let refusal = events.iter().find_map(|e| match e {
            DomainEvent::MessageBlocked { reason, detail, .. } => {
                Some(format!("{reason}: {detail}"))
            }
            _ => None,
        });
        self.commit(events)?;
        match (accepted, id.as_ref()) {
            (true, Some(id)) => Ok(data(
                "fleet.msg.send/1",
                vec![
                    ("id", Value::str(id.as_str())),
                    ("channel", Value::str(format!("{run}/{}", m.channel))),
                    ("status", Value::str("queued")),
                ],
            )),
            _ => Err(FleetError::invalid(format!(
                "the message was refused ({}); it was recorded as {}",
                refusal.unwrap_or_default(),
                id.as_ref().map_or_else(String::new, |i| i.to_string())
            ))),
        }
    }

    // ---- plans ----------------------------------------------------------------------------

    /// Checks the channels of a plan against the tasks the run will have and the workers the
    /// daemon is configured with, before anything is recorded.
    pub(crate) fn check_channels(
        &self,
        run: &RunId,
        task_ids: &BTreeSet<TaskId>,
        channels: &[ChannelDef],
    ) -> Result<()> {
        for c in channels {
            for m in c.members.iter().chain(c.lead.iter()) {
                match m {
                    Member::Task(t) if !task_ids.contains(t) => {
                        return Err(FleetError::invalid(format!(
                            "channel {}: member task:{t} is not a task of run {run}",
                            c.name
                        )))
                    }
                    Member::Role(w) if !self.cfg.workers.contains_key(w) => {
                        return Err(FleetError::invalid(format!(
                            "channel {}: member role:{w} is not a worker in the configuration",
                            c.name
                        )))
                    }
                    _ => {}
                }
            }
        }
        self.board.open_channels(run, channels).map(|_| ())
    }

    /// The events that open the channels of a plan.
    pub(crate) fn channel_events(
        &self,
        run: &RunId,
        channels: &[ChannelDef],
    ) -> Result<Vec<DomainEvent>> {
        self.board.open_channels(run, channels)
    }
}
