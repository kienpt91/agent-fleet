//! The message board: every channel, every message and the counters that keep them in bounds. A
//! projection of the event log like the fleet state, so it survives a restart: undelivered
//! messages are still waiting afterwards and delivered ones are never delivered twice.
//!
//! Decisions are pure functions of the board, the roster of the run and an injected clock.
//! [`MessageBoard::submit`] decides what happens to a message (accept it or refuse it with a
//! reason), [`MessageBoard::due`] decides what is to be delivered or has expired, and
//! [`MessageBoard::apply`] changes the board when an event is recorded.

use crate::approval::{Action, Digest};
use crate::error::{FleetError, Result};
use crate::events::DomainEvent;
use crate::ids::{show, ApprovalId, ChannelName, DecisionId, MessageId, RunId, TaskId};
use crate::msg::channel::{roster_find, Channel, ChannelDef, MuteNote, RosterEntry};
use crate::msg::inbox::{render_item, ChannelNote, InboxItem, MessageNotes, WaitingNote};
use crate::msg::limits::*;
use crate::msg::loops::{Exchanges, Trip};
use crate::msg::message::{content_digest, DeliveryState, Message, Recipient, Refusal, Sender};
use crate::msg::ratelimit::{SlidingWindow, MINUTE_MS};
use crate::msg::secrets::find_secret_shape;
use crate::sanitize::{flatten_line, head_str, sanitize_text};
use crate::status::{
    ApprovalStatus, BlockReason, ChannelDirection, ChannelMode, ExpireReason, MessageKind,
    MuteReason,
};
use std::collections::{BTreeMap, BTreeSet};

/// What the board decides, once, for a message that was submitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Accepted(Accepted),
    Refused(Refused),
}

/// A message that passed every check. `action` is set when a person has to approve it first:
/// the caller requests that approval and gives its id to [`Accepted::with_approval`] before the
/// event is recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    pub id: MessageId,
    pub event: DomainEvent,
    pub action: Option<Action>,
}

impl Accepted {
    pub fn with_approval(mut self, approval: ApprovalId) -> Accepted {
        if let DomainEvent::MessageSubmitted { approval: a, .. } = &mut self.event {
            *a = Some(approval);
        }
        self
    }
}

/// A message the board refused. Recording `events` makes it visible; `trip` says that the
/// refusal also crossed a loop or storm limit, so the caller must mute and open a gate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub id: MessageId,
    pub events: Vec<DomainEvent>,
    pub reason: BlockReason,
    pub detail: String,
    pub trip: Option<TripInfo>,
}

/// What a tripped limit asks of the caller: mute `channel` (or, for a flood, `task`), and put a
/// decision gate in front of a person. The text names ids and numbers only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TripInfo {
    pub run: RunId,
    pub reason: MuteReason,
    pub channel: Option<ChannelName>,
    pub task: Option<TaskId>,
    /// The task the gate is about: the sender of the message that crossed the limit.
    pub sender: TaskId,
    pub text: String,
}

/// What a message is submitted with.
#[derive(Clone, Debug)]
pub struct Submission<'a> {
    pub run: &'a RunId,
    pub from: Sender,
    /// The channel as the sender wrote it.
    pub channel: &'a str,
    pub to: Option<&'a TaskId>,
    pub kind: MessageKind,
    pub subject: &'a str,
    pub body: &'a str,
    pub in_reply_to: Option<&'a MessageId>,
    /// Where it was read from, so a file that is read twice is one message.
    pub source: Option<String>,
}

/// What a decision needs from the outside world.
pub struct Context<'a> {
    pub roster: &'a [RosterEntry],
    pub limits: &'a MessagingLimits,
    pub now_ms: u64,
    pub hasher: &'a dyn Digest,
    /// Redacts a text; run after the secret scan, so a key is refused, not quietly masked.
    pub redact: &'a dyn Fn(&str) -> String,
}

/// One step the board wants taken for a waiting message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Due {
    Deliver {
        id: MessageId,
        to: TaskId,
    },
    Expire {
        id: MessageId,
        to: TaskId,
        reason: ExpireReason,
    },
    /// A person denied the approval the message waited for.
    Deny {
        id: MessageId,
    },
}

/// What is muted, found by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MuteTarget {
    Channel(RunId, ChannelName),
    Task(RunId, TaskId),
}

#[derive(Clone, Debug, Default)]
struct SenderCounters {
    total: u32,
    window: SlidingWindow,
}

#[derive(Debug, Default)]
pub struct MessageBoard {
    channels: BTreeMap<(RunId, ChannelName), Channel>,
    messages: BTreeMap<MessageId, Message>,
    seq: u64,
    muted_tasks: BTreeMap<(RunId, TaskId), MuteNote>,
    channel_totals: BTreeMap<(RunId, ChannelName), u32>,
    senders: BTreeMap<(RunId, ChannelName, Sender), SenderCounters>,
    task_windows: BTreeMap<(RunId, TaskId), SlidingWindow>,
    refused_windows: BTreeMap<(RunId, TaskId), SlidingWindow>,
    exchanges: BTreeMap<(RunId, ChannelName), Exchanges>,
    digests: BTreeMap<(RunId, ChannelName, String), u64>,
    sources: BTreeSet<(RunId, String, String)>,
    accepted: usize,
    inbox_seq: BTreeMap<(RunId, TaskId), u64>,
    gates: BTreeMap<DecisionId, MuteTarget>,
}

/// A refusal in the making: the reason, the detail, and whether it tripped a limit.
struct Failure {
    reason: BlockReason,
    detail: String,
    trip: Option<(MuteReason, Trip)>,
}

fn fail<T>(reason: BlockReason, detail: impl Into<String>) -> std::result::Result<T, Failure> {
    Err(Failure {
        reason,
        detail: detail.into(),
        trip: None,
    })
}

/// A message that is ready to be accepted.
struct Plan {
    channel: ChannelName,
    recipients: Vec<TaskId>,
    depth: u32,
    subject: String,
    body: String,
    digest: String,
    approval: bool,
}

/// A shortened, escaped copy of something a worker wrote, for the log's `detail` field.
fn shown(s: &str) -> String {
    show(head_str(s, 48))
}

impl MessageBoard {
    pub fn new() -> MessageBoard {
        MessageBoard::default()
    }

    // ---- queries --------------------------------------------------------------------------

    pub fn channels(&self) -> impl Iterator<Item = &Channel> {
        self.channels.values()
    }

    pub fn channel(&self, run: &RunId, name: &ChannelName) -> Option<&Channel> {
        self.channels.get(&(run.clone(), name.clone()))
    }

    pub fn has_channels(&self) -> bool {
        !self.channels.is_empty()
    }

    pub fn messages(&self) -> impl Iterator<Item = &Message> {
        self.messages.values()
    }

    pub fn message(&self, id: &MessageId) -> Option<&Message> {
        self.messages.get(id)
    }

    /// Accepted messages, refused ones excluded.
    pub fn accepted_count(&self) -> usize {
        self.accepted
    }

    pub fn task_muted(&self, run: &RunId, task: &TaskId) -> Option<&MuteNote> {
        self.muted_tasks.get(&(run.clone(), task.clone()))
    }

    pub fn muted_tasks(&self) -> impl Iterator<Item = (&(RunId, TaskId), &MuteNote)> {
        self.muted_tasks.iter()
    }

    /// Messages sent on `channel` by `task`, over its whole life.
    pub fn sent_total(&self, run: &RunId, channel: &ChannelName, from: &Sender) -> u32 {
        self.senders
            .get(&(run.clone(), channel.clone(), from.clone()))
            .map_or(0, |c| c.total)
    }

    /// The channels of `run` a task belongs to, given the worker it runs.
    pub fn channels_of(
        &self,
        run: &RunId,
        task: &TaskId,
        worker: Option<&crate::ids::WorkerName>,
    ) -> Vec<&Channel> {
        self.channels
            .iter()
            .filter(|((r, _), c)| r == run && c.def.is_member(task, worker))
            .map(|(_, c)| c)
            .collect()
    }

    /// Resolves `<run>/<name>` or a channel name that exists in one run only.
    pub fn find_channel(&self, reference: &str) -> Result<(RunId, ChannelName)> {
        if let Some((run, name)) = reference.split_once('/') {
            let key = (RunId::parse(run)?, ChannelName::parse(name)?);
            return if self.channels.contains_key(&key) {
                Ok(key)
            } else {
                Err(FleetError::not_found(format!(
                    "channel {reference} does not exist"
                )))
            };
        }
        let name = ChannelName::parse(reference)?;
        let matches: Vec<&(RunId, ChannelName)> =
            self.channels.keys().filter(|(_, n)| *n == name).collect();
        match matches.as_slice() {
            [] => Err(FleetError::not_found(format!(
                "channel {reference} does not exist"
            ))),
            [one] => Ok((*one).clone()),
            many => Err(FleetError::invalid(format!(
                "channel {reference} exists in several runs ({}); write it as <run>/<channel>",
                many.iter()
                    .map(|(r, _)| r.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// The messages waiting for `task` to start, oldest first.
    pub fn pending_for(&self, run: &RunId, task: &TaskId) -> Vec<&Message> {
        self.messages
            .values()
            .filter(|m| &m.run == run && m.is_pending_for(task))
            .collect()
    }

    /// The messages delivered to `task`, in the order they were delivered.
    pub fn delivered_to(&self, run: &RunId, task: &TaskId) -> Vec<(u64, &Message)> {
        let mut out: Vec<(u64, &Message)> = self
            .messages
            .values()
            .filter(|m| &m.run == run)
            .filter_map(|m| match m.recipient(task).map(|r| &r.state) {
                Some(DeliveryState::Delivered { seq, .. }) => Some((*seq, m)),
                _ => None,
            })
            .collect();
        out.sort_by_key(|(seq, _)| *seq);
        out
    }

    /// Everything that belongs in the inbox of `task`, rendered.
    pub fn inbox_items(&self, run: &RunId, task: &TaskId, hasher: &dyn Digest) -> Vec<InboxItem> {
        self.delivered_to(run, task)
            .into_iter()
            .map(|(seq, m)| render_item(m, seq, hasher))
            .collect()
    }

    /// The seq the next delivery to `task` gets.
    pub fn next_inbox_seq(&self, run: &RunId, task: &TaskId) -> u64 {
        self.inbox_seq
            .get(&(run.clone(), task.clone()))
            .copied()
            .unwrap_or(0)
            .saturating_add(1)
    }

    /// What a brief says about the messages of `task`.
    pub fn notes_for(
        &self,
        run: &RunId,
        task: &TaskId,
        worker: Option<&crate::ids::WorkerName>,
    ) -> MessageNotes {
        let channels = self
            .channels_of(run, task, worker)
            .into_iter()
            .map(|c| ChannelNote {
                name: c.def.name.clone(),
                mode: c.def.mode.as_str(),
                direction: c.def.direction.as_str(),
                max_message_bytes: c.def.max_message_bytes,
                requires_approval: c.def.requires_approval,
                muted: c.muted.is_some(),
            })
            .collect();
        MessageNotes {
            channels,
            in_inbox: self.delivered_to(run, task).len(),
            waiting: self
                .pending_for(run, task)
                .into_iter()
                .map(|m| WaitingNote {
                    id: m.id.clone(),
                    from: m.from.clone(),
                    subject: m.subject.clone(),
                })
                .collect(),
        }
    }

    /// The channel or task a gate was opened for.
    pub fn gate_target(&self, gate: &DecisionId) -> Option<&MuteTarget> {
        self.gates.get(gate)
    }

    // ---- state ----------------------------------------------------------------------------

    pub fn apply(&mut self, ev: &DomainEvent) {
        match ev {
            DomainEvent::ChannelOpened { .. } => {
                if let Some((run, def)) = ChannelDef::from_event(ev) {
                    let key = (run.clone(), def.name.clone());
                    self.channels.entry(key).or_insert(Channel {
                        run,
                        def,
                        muted: None,
                    });
                }
            }
            DomainEvent::MessageSubmitted { .. } => self.apply_submitted(ev),
            DomainEvent::MessageDelivered { id, to, at_ms } => {
                let Some(m) = self.messages.get_mut(id) else {
                    return;
                };
                let key = (m.run.clone(), to.clone());
                let seq = self
                    .inbox_seq
                    .get(&key)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(1);
                if m.is_pending_for(to) {
                    m.mark_delivered(to, *at_ms, seq);
                    self.inbox_seq.insert(key, seq);
                }
            }
            DomainEvent::MessageExpired {
                id,
                to,
                reason,
                at_ms,
            } => {
                if let Some(m) = self.messages.get_mut(id) {
                    m.mark_expired(to, *reason, *at_ms);
                }
            }
            DomainEvent::MessageBlocked { .. } => self.apply_blocked(ev),
            DomainEvent::ChannelMuted {
                run,
                channel,
                task,
                reason,
                by,
                gate,
                ..
            } => {
                let note = MuteNote {
                    reason: *reason,
                    by: by.clone(),
                    gate: gate.clone(),
                };
                match (channel, task) {
                    (Some(c), _) => {
                        if let Some(ch) = self.channels.get_mut(&(run.clone(), c.clone())) {
                            ch.muted = Some(note);
                            if let Some(g) = gate {
                                self.gates
                                    .insert(g.clone(), MuteTarget::Channel(run.clone(), c.clone()));
                            }
                        }
                    }
                    (None, Some(t)) => {
                        self.muted_tasks.insert((run.clone(), t.clone()), note);
                        if let Some(g) = gate {
                            self.gates
                                .insert(g.clone(), MuteTarget::Task(run.clone(), t.clone()));
                        }
                    }
                    (None, None) => {}
                }
            }
            DomainEvent::ChannelUnmuted {
                run, channel, task, ..
            } => match (channel, task) {
                (Some(c), _) => {
                    let key = (run.clone(), c.clone());
                    if let Some(ch) = self.channels.get_mut(&key) {
                        ch.muted = None;
                    }
                    if let Some(ex) = self.exchanges.get_mut(&key) {
                        ex.reset();
                    }
                }
                (None, Some(t)) => {
                    let key = (run.clone(), t.clone());
                    self.muted_tasks.remove(&key);
                    self.refused_windows.remove(&key);
                }
                (None, None) => {}
            },
            _ => {}
        }
    }

    fn apply_submitted(&mut self, ev: &DomainEvent) {
        let Some(m) = Message::from_event(ev) else {
            return;
        };
        let Ok(name) = ChannelName::parse(&m.channel) else {
            return;
        };
        let key = (m.run.clone(), name);
        self.seq = self.seq.max(m.id.seq());
        self.accepted = self.accepted.saturating_add(1);
        let counters = self
            .senders
            .entry((key.0.clone(), key.1.clone(), m.from.clone()))
            .or_default();
        counters.total = counters.total.saturating_add(1);
        counters
            .window
            .record(m.submitted_ms, MINUTE_MS, HARD_PER_MINUTE as usize + 1);
        let total = self.channel_totals.entry(key.clone()).or_insert(0);
        *total = total.saturating_add(1);
        if let Sender::Task(t) = &m.from {
            self.task_windows
                .entry((m.run.clone(), t.clone()))
                .or_default()
                .record(m.submitted_ms, MINUTE_MS, HARD_PER_MINUTE as usize + 1);
            let to: Vec<TaskId> = m.recipients.iter().map(|r| r.task.clone()).collect();
            self.exchanges
                .entry(key.clone())
                .or_default()
                .record(t, &to, m.depth);
            self.sources.insert((
                m.run.clone(),
                m.from.text(),
                m.source.clone().unwrap_or_default(),
            ));
        }
        if m.from != Sender::Operator {
            self.digests.insert(
                (key.0.clone(), key.1.clone(), m.digest.clone()),
                m.submitted_ms,
            );
            if self.digests.len() > MAX_DIGESTS_KEPT {
                let oldest = self
                    .digests
                    .iter()
                    .min_by_key(|(_, at)| **at)
                    .map(|(k, _)| k.clone());
                if let Some(k) = oldest {
                    self.digests.remove(&k);
                }
            }
        }
        self.messages.insert(m.id.clone(), m);
        self.prune();
    }

    fn apply_blocked(&mut self, ev: &DomainEvent) {
        let DomainEvent::MessageBlocked {
            id,
            reason,
            detail,
            at_ms,
            ..
        } = ev
        else {
            return;
        };
        if let Some(existing) = self.messages.get_mut(id) {
            // an accepted message a person then denied
            existing.refuse(Refusal {
                reason: *reason,
                detail: detail.clone(),
            });
            return;
        }
        let Some(m) = Message::from_event(ev) else {
            return;
        };
        self.seq = self.seq.max(m.id.seq());
        if let Sender::Task(t) = &m.from {
            self.refused_windows
                .entry((m.run.clone(), t.clone()))
                .or_default()
                .record(*at_ms, MINUTE_MS, MAX_BLOCKED_PER_MINUTE + 1);
            self.sources.insert((
                m.run.clone(),
                m.from.text(),
                m.source.clone().unwrap_or_default(),
            ));
        }
        self.messages.insert(m.id.clone(), m);
        self.prune();
    }

    /// Refused messages are the first to go when too many are kept.
    fn prune(&mut self) {
        while self.messages.len() > MESSAGES_KEPT {
            let oldest = self
                .messages
                .iter()
                .find(|(_, m)| m.is_refused())
                .map(|(id, _)| id.clone());
            match oldest {
                Some(id) => {
                    self.messages.remove(&id);
                }
                None => break,
            }
        }
    }

    // ---- commands -------------------------------------------------------------------------

    /// The events that open the channels of a plan for `run`. Names must be new in the run, and
    /// the numbers of channels are bounded.
    pub fn open_channels(&self, run: &RunId, defs: &[ChannelDef]) -> Result<Vec<DomainEvent>> {
        let in_run = self.channels.keys().filter(|(r, _)| r == run).count();
        if in_run + defs.len() > MAX_CHANNELS_PER_RUN {
            return Err(FleetError::limit(format!(
                "a run may have at most {MAX_CHANNELS_PER_RUN} channels"
            )));
        }
        if self.channels.len() + defs.len() > MAX_CHANNELS_TOTAL {
            return Err(FleetError::limit(format!(
                "at most {MAX_CHANNELS_TOTAL} channels are kept across all runs"
            )));
        }
        let mut seen: BTreeSet<&ChannelName> = BTreeSet::new();
        for d in defs {
            d.validate()?;
            if !seen.insert(&d.name) || self.channels.contains_key(&(run.clone(), d.name.clone())) {
                return Err(FleetError::conflict(format!(
                    "channel {} is defined twice in run {run}",
                    d.name
                )));
            }
        }
        Ok(defs.iter().map(|d| d.opened_event(run)).collect())
    }

    /// Decides what happens to one submitted message.
    pub fn submit(&self, sub: &Submission<'_>, cx: &Context<'_>) -> Verdict {
        let bytes = sub.body.len() as u64;
        match self.plan(sub, cx) {
            Ok(plan) => {
                let id = MessageId::from_seq(self.seq.saturating_add(1));
                let expires_ms = cx.now_ms.saturating_add(
                    self.channels
                        .get(&(sub.run.clone(), plan.channel.clone()))
                        .map_or(0, |c| c.def.ttl_ms),
                );
                let message = Message {
                    id: id.clone(),
                    run: sub.run.clone(),
                    channel: plan.channel.to_string(),
                    from: sub.from.clone(),
                    kind: sub.kind,
                    subject: plan.subject.clone(),
                    body: plan.body.clone(),
                    in_reply_to: sub.in_reply_to.cloned(),
                    depth: plan.depth,
                    digest: plan.digest.clone(),
                    submitted_ms: cx.now_ms,
                    expires_ms,
                    recipients: plan
                        .recipients
                        .iter()
                        .map(|t| Recipient {
                            task: t.clone(),
                            state: DeliveryState::Pending,
                        })
                        .collect(),
                    approval: None,
                    source: sub.source.clone(),
                    bytes,
                    refused: None,
                };
                let event = DomainEvent::MessageSubmitted {
                    id: id.clone(),
                    run: sub.run.clone(),
                    channel: plan.channel,
                    from: sub.from.text(),
                    to: plan.recipients,
                    kind: sub.kind,
                    subject: plan.subject,
                    body: plan.body,
                    in_reply_to: sub.in_reply_to.cloned(),
                    depth: plan.depth,
                    digest: plan.digest,
                    at_ms: cx.now_ms,
                    expires_ms,
                    approval: None,
                    source: sub.source.clone(),
                };
                let action = plan.approval.then(|| message.approval_action());
                Verdict::Accepted(Accepted { id, event, action })
            }
            Err(f) => Verdict::Refused(self.refusal(sub, f, bytes, cx.hasher, cx.now_ms)),
        }
    }

    fn refusal(
        &self,
        sub: &Submission<'_>,
        f: Failure,
        bytes: u64,
        hasher: &dyn Digest,
        now_ms: u64,
    ) -> Refused {
        let id = MessageId::from_seq(self.seq.saturating_add(1));
        let channel = flatten_line(sub.channel, 40);
        let digest = hasher.sha256_hex(sub.body.as_bytes());
        let events = vec![DomainEvent::MessageBlocked {
            id: id.clone(),
            run: sub.run.clone(),
            channel,
            from: sub.from.text(),
            reason: f.reason,
            detail: f.detail.clone(),
            digest,
            bytes,
            at_ms: now_ms,
            source: sub.source.clone(),
        }];
        let trip = f.trip.and_then(|(mute, trip)| {
            let sender = sub.from.task()?.clone();
            let name = ChannelName::parse(sub.channel).ok();
            let (channel, task) = match mute {
                MuteReason::Flood => (None, Some(sender.clone())),
                _ => (name, None),
            };
            Some(TripInfo {
                run: sub.run.clone(),
                reason: mute,
                channel,
                task,
                sender,
                text: trip.describe(),
            })
        });
        Refused {
            id,
            events,
            reason: f.reason,
            detail: f.detail,
            trip,
        }
    }

    /// A file that is not a message at all (it did not parse, it is too large): refused like any
    /// other, and counted towards a flood.
    #[allow(clippy::too_many_arguments)]
    pub fn refuse_file(
        &self,
        run: &RunId,
        from: &Sender,
        reason: BlockReason,
        detail: &str,
        source: Option<String>,
        hasher: &dyn Digest,
        now_ms: u64,
    ) -> Refused {
        let sub = Submission {
            run,
            from: from.clone(),
            channel: "",
            to: None,
            kind: MessageKind::Info,
            subject: "",
            body: "",
            in_reply_to: None,
            source,
        };
        let failure = self.flood(run, from, now_ms).unwrap_or(Failure {
            reason,
            detail: flatten_line(detail, 160),
            trip: None,
        });
        self.refusal(&sub, failure, 0, hasher, now_ms)
    }

    /// Whether this exact file of this sender was dealt with already (accepted or refused), so a
    /// file that is read twice, after a restart or a failed rename, is one message.
    pub fn seen_source(&self, run: &RunId, from: &Sender, source: &str) -> bool {
        self.sources
            .contains(&(run.clone(), from.text(), source.to_string()))
    }

    /// A task that keeps sending messages that are refused is a flood.
    fn flood(&self, run: &RunId, from: &Sender, now_ms: u64) -> Option<Failure> {
        let Sender::Task(t) = from else {
            return None;
        };
        let flooding = self
            .refused_windows
            .get(&(run.clone(), t.clone()))
            .is_some_and(|w| w.count(now_ms, MINUTE_MS) >= MAX_BLOCKED_PER_MINUTE);
        flooding.then(|| Failure {
            reason: BlockReason::Rate,
            detail: "too many refused messages in a minute".to_string(),
            trip: Some((MuteReason::Flood, flood_trip(t, MAX_BLOCKED_PER_MINUTE))),
        })
    }

    /// Every check a message must pass, in order. The first failure is the reason.
    fn plan(&self, sub: &Submission<'_>, cx: &Context<'_>) -> std::result::Result<Plan, Failure> {
        let now = cx.now_ms;
        let run = sub.run;
        if let Some(f) = self.flood(run, &sub.from, now) {
            return Err(f);
        }
        let Some(name) = ChannelName::parse(sub.channel).ok() else {
            return fail(BlockReason::UnknownChannel, shown(sub.channel));
        };
        let Some(channel) = self.channels.get(&(run.clone(), name.clone())) else {
            return fail(BlockReason::UnknownChannel, shown(sub.channel));
        };
        let def = &channel.def;
        if let Sender::Task(t) = &sub.from {
            let worker = roster_find(cx.roster, t).and_then(|e| e.worker.as_ref());
            if roster_find(cx.roster, t).is_none() || !def.is_member(t, worker) {
                return fail(
                    BlockReason::NotMember,
                    format!("task {t} is not in this channel"),
                );
            }
            if self.muted_tasks.contains_key(&(run.clone(), t.clone())) {
                return fail(BlockReason::TaskMuted, format!("task {t} is muted"));
            }
        }
        if channel.muted.is_some() {
            return fail(BlockReason::ChannelMuted, "the channel is muted");
        }
        if sub.body.len() > def.max_message_bytes {
            return fail(
                BlockReason::TooLarge,
                format!(
                    "the body is {} bytes; this channel allows {}",
                    sub.body.len(),
                    def.max_message_bytes
                ),
            );
        }
        // what would be stored and delivered
        let subject_clean = flatten_line(&sanitize_text(sub.subject.as_bytes()), MAX_SUBJECT * 4);
        let body_clean = sanitize_text(sub.body.as_bytes()).trim().to_string();
        if subject_clean.is_empty() {
            return fail(BlockReason::Invalid, "the subject is empty");
        }
        if body_clean.is_empty() {
            return fail(BlockReason::Invalid, "the body is empty");
        }
        let secret = [sub.subject, sub.body, &subject_clean, &body_clean]
            .iter()
            .find_map(|t| find_secret_shape(t));
        if let Some(rule) = secret {
            return fail(BlockReason::SecretShape, rule);
        }
        let recipients = self.recipients(def, sub, cx)?;
        if recipients.len() > cx.limits.max_fan_out {
            return fail(
                BlockReason::FanOut,
                format!(
                    "{} recipients; at most {} are allowed",
                    recipients.len(),
                    cx.limits.max_fan_out
                ),
            );
        }
        let explicit_depth = match sub.in_reply_to {
            None => 0,
            Some(parent) => match self.messages.get(parent) {
                Some(p) if !p.is_refused() && p.run == *run && p.channel == name.as_str() => {
                    p.depth.saturating_add(1)
                }
                _ => {
                    return fail(
                        BlockReason::UnknownReply,
                        format!("no message {parent} here"),
                    )
                }
            },
        };
        // after the scan: masks what the redactor knows (configured literals, env values)
        let subject = flatten_line(&(cx.redact)(&subject_clean), MAX_SUBJECT * 4);
        let body = head_str(&(cx.redact)(&body_clean), def.max_message_bytes).to_string();
        let digest = content_digest(
            name.as_str(),
            &sub.from,
            &recipients,
            sub.kind,
            &subject,
            &body,
            cx.hasher,
        );
        let mut depth = explicit_depth;
        if let Sender::Task(from) = &sub.from {
            if let Some(last) = self
                .digests
                .get(&(run.clone(), name.clone(), digest.clone()))
            {
                if now.saturating_sub(*last) < def.ttl_ms {
                    return fail(
                        BlockReason::Duplicate,
                        "the same message was sent a moment ago",
                    );
                }
            }
            if let Some(ex) = self.exchanges.get(&(run.clone(), name.clone())) {
                for t in &recipients {
                    depth = depth.max(ex.implicit_depth(from, t));
                }
                if let Some(trip) = ex.check(from, &recipients, depth, cx.limits) {
                    let mute = match trip {
                        Trip::ReplyDepth { .. } => MuteReason::ReplyDepth,
                        Trip::PairBudget { .. } => MuteReason::PairBudget,
                        Trip::Flood { .. } => MuteReason::Flood,
                    };
                    let reason = match mute {
                        MuteReason::ReplyDepth => BlockReason::ReplyDepth,
                        _ => BlockReason::PairBudget,
                    };
                    return Err(Failure {
                        reason,
                        detail: trip.describe(),
                        trip: Some((mute, trip)),
                    });
                }
            } else if depth > cx.limits.max_reply_depth {
                let trip = Trip::ReplyDepth {
                    depth,
                    limit: cx.limits.max_reply_depth,
                    pair: crate::msg::loops::pair_key(from, recipients.first().unwrap_or(from)),
                };
                return Err(Failure {
                    reason: BlockReason::ReplyDepth,
                    detail: trip.describe(),
                    trip: Some((MuteReason::ReplyDepth, trip)),
                });
            }
            let counters = self
                .senders
                .get(&(run.clone(), name.clone(), sub.from.clone()));
            let sent = counters.map_or(0, |c| c.total);
            if sent >= def.max_messages_per_task {
                return fail(
                    BlockReason::Quota,
                    format!("task {from} already sent {sent} messages on this channel"),
                );
            }
            let members = u32::try_from(channel.member_tasks(cx.roster).len()).unwrap_or(u32::MAX);
            let channel_cap = def.max_messages_per_task.saturating_mul(members.max(1));
            let channel_sent = self
                .channel_totals
                .get(&(run.clone(), name.clone()))
                .copied()
                .unwrap_or(0);
            if channel_sent >= channel_cap {
                return fail(
                    BlockReason::Quota,
                    format!("the channel already carried {channel_sent} messages"),
                );
            }
            let in_minute = counters.map_or(0, |c| c.window.count(now, MINUTE_MS));
            if in_minute >= def.max_messages_per_minute as usize {
                return fail(
                    BlockReason::Rate,
                    format!("{in_minute} messages in the last minute on this channel"),
                );
            }
            let all_channels = self
                .task_windows
                .get(&(run.clone(), from.clone()))
                .map_or(0, |w| w.count(now, MINUTE_MS));
            if all_channels >= cx.limits.max_per_task_per_minute as usize {
                return fail(
                    BlockReason::Rate,
                    format!("{all_channels} messages in the last minute over all channels"),
                );
            }
        }
        if self.accepted >= cx.limits.max_total_messages {
            return fail(
                BlockReason::GlobalCap,
                format!("{} messages were accepted in all", self.accepted),
            );
        }
        let approval = def.requires_approval && matches!(sub.from, Sender::Task(_));
        Ok(Plan {
            channel: name,
            recipients,
            depth,
            subject,
            body,
            digest,
            approval,
        })
    }

    /// Who the message is for.
    fn recipients(
        &self,
        def: &ChannelDef,
        sub: &Submission<'_>,
        cx: &Context<'_>,
    ) -> std::result::Result<Vec<TaskId>, Failure> {
        let sender = sub.from.task();
        let from_task = matches!(sub.from, Sender::Task(_));
        let lead_only = from_task && def.direction == ChannelDirection::ToLead;
        if lead_only {
            let is_lead = sender.is_some_and(|t| {
                let worker = roster_find(cx.roster, t).and_then(|e| e.worker.as_ref());
                def.is_lead(t, worker)
            });
            if is_lead {
                return fail(
                    BlockReason::Direction,
                    "this channel only carries messages to its lead",
                );
            }
        }
        let candidates: Vec<&RosterEntry> = cx
            .roster
            .iter()
            .filter(|e| def.is_member(&e.task, e.worker.as_ref()))
            .filter(|e| Some(&e.task) != sender)
            .filter(|e| !lead_only || def.is_lead(&e.task, e.worker.as_ref()))
            .collect();
        let chosen: Vec<&RosterEntry> = match sub.to {
            Some(to) => match candidates.iter().find(|e| &e.task == to) {
                Some(e) => vec![*e],
                None => {
                    return fail(
                        BlockReason::BadRecipient,
                        format!("task {to} cannot receive on this channel"),
                    )
                }
            },
            None if def.mode == ChannelMode::Direct => {
                return fail(BlockReason::BadRecipient, "a direct channel needs `to`")
            }
            None => candidates,
        };
        let live: Vec<TaskId> = chosen
            .into_iter()
            .filter(|e| !e.ended())
            .map(|e| e.task.clone())
            .collect();
        if live.is_empty() {
            return fail(
                BlockReason::BadRecipient,
                "no recipient can take part any more",
            );
        }
        Ok(live)
    }

    /// The deliveries and expiries that are due. `roster` gives the tasks of a run;
    /// `approval` the state of an approval (`None` when it is gone). At most
    /// [`MAX_DUE_PER_TICK`] steps are returned.
    pub fn due(
        &self,
        now_ms: u64,
        roster: &dyn Fn(&RunId) -> Vec<RosterEntry>,
        approval: &dyn Fn(&ApprovalId) -> Option<ApprovalStatus>,
    ) -> Vec<Due> {
        let mut out = Vec::new();
        let mut rosters: BTreeMap<RunId, Vec<RosterEntry>> = BTreeMap::new();
        for m in self.messages.values().filter(|m| m.has_pending()) {
            let entries = rosters
                .entry(m.run.clone())
                .or_insert_with(|| roster(&m.run));
            let approved = match &m.approval {
                None => Some(true),
                Some(a) => match approval(a) {
                    Some(ApprovalStatus::Approved | ApprovalStatus::Consumed) => Some(true),
                    Some(ApprovalStatus::Pending) => Some(false),
                    Some(ApprovalStatus::Denied) => {
                        out.push(Due::Deny { id: m.id.clone() });
                        continue;
                    }
                    Some(ApprovalStatus::Expired) | None => None,
                },
            };
            let channel = ChannelName::parse(&m.channel)
                .ok()
                .and_then(|n| self.channels.get(&(m.run.clone(), n)));
            let silenced = channel.is_some_and(|c| c.muted.is_some())
                || m.from
                    .task()
                    .is_some_and(|t| self.muted_tasks.contains_key(&(m.run.clone(), t.clone())));
            for r in m
                .recipients
                .iter()
                .filter(|r| r.state == DeliveryState::Pending)
            {
                if out.len() >= MAX_DUE_PER_TICK {
                    return out;
                }
                let expire = |reason| Due::Expire {
                    id: m.id.clone(),
                    to: r.task.clone(),
                    reason,
                };
                let entry = roster_find(entries, &r.task);
                if now_ms >= m.expires_ms {
                    out.push(expire(ExpireReason::Ttl));
                } else if approved.is_none() {
                    out.push(expire(ExpireReason::ApprovalExpired));
                } else if entry.is_none_or(RosterEntry::ended) {
                    out.push(expire(ExpireReason::RecipientEnded));
                } else if approved == Some(true)
                    && !silenced
                    && !self
                        .muted_tasks
                        .contains_key(&(m.run.clone(), r.task.clone()))
                    && entry.is_some_and(|e| e.live)
                {
                    if self.delivered_to(&m.run, &r.task).len() >= MAX_INBOX_ENTRIES {
                        out.push(expire(ExpireReason::InboxFull));
                    } else {
                        out.push(Due::Deliver {
                            id: m.id.clone(),
                            to: r.task.clone(),
                        });
                    }
                }
            }
        }
        out
    }

    /// Channels that have nobody left to talk to and are not muted yet.
    pub fn dead_channels(
        &self,
        roster: &dyn Fn(&RunId) -> Vec<RosterEntry>,
    ) -> Vec<(RunId, ChannelName)> {
        let mut out = Vec::new();
        let mut rosters: BTreeMap<RunId, Vec<RosterEntry>> = BTreeMap::new();
        for ((run, name), c) in &self.channels {
            if c.muted.is_some() {
                continue;
            }
            let entries = rosters.entry(run.clone()).or_insert_with(|| roster(run));
            if c.is_dead(entries) {
                out.push((run.clone(), name.clone()));
            }
        }
        out
    }

    /// Mutes one channel.
    pub fn mute_channel(
        &self,
        run: &RunId,
        name: &ChannelName,
        reason: MuteReason,
        by: &str,
        gate: Option<DecisionId>,
        now_ms: u64,
    ) -> Result<DomainEvent> {
        let c = self
            .channel(run, name)
            .ok_or_else(|| FleetError::not_found(format!("channel {run}/{name} does not exist")))?;
        if c.muted.is_some() {
            return Err(FleetError::conflict(format!(
                "channel {run}/{name} is already muted"
            )));
        }
        Ok(DomainEvent::ChannelMuted {
            run: run.clone(),
            channel: Some(name.clone()),
            task: None,
            reason,
            by: by.to_string(),
            gate,
            at_ms: now_ms,
        })
    }

    pub fn unmute_channel(
        &self,
        run: &RunId,
        name: &ChannelName,
        by: &str,
        now_ms: u64,
    ) -> Result<DomainEvent> {
        let c = self
            .channel(run, name)
            .ok_or_else(|| FleetError::not_found(format!("channel {run}/{name} does not exist")))?;
        if c.muted.is_none() {
            return Err(FleetError::conflict(format!(
                "channel {run}/{name} is not muted"
            )));
        }
        Ok(DomainEvent::ChannelUnmuted {
            run: run.clone(),
            channel: Some(name.clone()),
            task: None,
            by: by.to_string(),
            at_ms: now_ms,
        })
    }

    pub fn mute_task(
        &self,
        run: &RunId,
        task: &TaskId,
        reason: MuteReason,
        by: &str,
        gate: Option<DecisionId>,
        now_ms: u64,
    ) -> Result<DomainEvent> {
        if self.task_muted(run, task).is_some() {
            return Err(FleetError::conflict(format!(
                "task {run}/{task} is already muted"
            )));
        }
        Ok(DomainEvent::ChannelMuted {
            run: run.clone(),
            channel: None,
            task: Some(task.clone()),
            reason,
            by: by.to_string(),
            gate,
            at_ms: now_ms,
        })
    }

    pub fn unmute_task(
        &self,
        run: &RunId,
        task: &TaskId,
        by: &str,
        now_ms: u64,
    ) -> Result<DomainEvent> {
        if self.task_muted(run, task).is_none() {
            return Err(FleetError::conflict(format!(
                "task {run}/{task} is not muted"
            )));
        }
        Ok(DomainEvent::ChannelUnmuted {
            run: run.clone(),
            channel: None,
            task: Some(task.clone()),
            by: by.to_string(),
            at_ms: now_ms,
        })
    }

    /// The unmuting a person's answer to a mute gate asks for (`unmute`); none for `keep muted`
    /// or when the target is no longer muted.
    pub fn answer_gate(&self, gate: &DecisionId, answer: &str, now_ms: u64) -> Vec<DomainEvent> {
        if answer != GATE_UNMUTE {
            return Vec::new();
        }
        let by = format!("gate {gate}");
        let event = match self.gates.get(gate) {
            Some(MuteTarget::Channel(run, name)) => self.unmute_channel(run, name, &by, now_ms),
            Some(MuteTarget::Task(run, task)) => self.unmute_task(run, task, &by, now_ms),
            None => return Vec::new(),
        };
        event.into_iter().collect()
    }
}

/// The answer of a mute gate that lets the channel speak again.
pub const GATE_UNMUTE: &str = "unmute";
/// The answer that keeps it muted.
pub const GATE_KEEP: &str = "keep muted";

fn flood_trip(task: &TaskId, refused: usize) -> Trip {
    Trip::Flood {
        task: task.clone(),
        refused: u32::try_from(refused).unwrap_or(u32::MAX),
        limit: MAX_BLOCKED_PER_MINUTE as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::channel::ChannelDef;
    use crate::status::TaskStatus;
    use crate::value::Value;

    struct Toy;
    impl Digest for Toy {
        fn sha256_hex(&self, data: &[u8]) -> String {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            data.hash(&mut h);
            format!("{:016x}", h.finish())
        }
    }

    fn t(s: &str) -> TaskId {
        TaskId::parse(s).unwrap()
    }

    fn run() -> RunId {
        RunId::parse("r1").unwrap()
    }

    fn name(s: &str) -> ChannelName {
        ChannelName::parse(s).unwrap()
    }

    fn entry(task: &str, status: TaskStatus) -> RosterEntry {
        RosterEntry {
            task: t(task),
            worker: Some(crate::ids::WorkerName::parse("agent").unwrap()),
            status,
            live: status == TaskStatus::Running,
        }
    }

    fn roster() -> Vec<RosterEntry> {
        vec![
            entry("a", TaskStatus::Running),
            entry("b", TaskStatus::Running),
            entry("c", TaskStatus::Running),
            entry("outsider", TaskStatus::Running),
        ]
    }

    fn channel_def(extra: Vec<(&str, Value)>) -> ChannelDef {
        let mut pairs = vec![
            ("name", Value::str("ch")),
            (
                "members",
                Value::array(["task:a", "task:b", "task:c"].map(Value::str)),
            ),
        ];
        pairs.extend(extra);
        ChannelDef::from_value(&Value::object(pairs)).unwrap()
    }

    fn board_with(def: ChannelDef) -> MessageBoard {
        let mut b = MessageBoard::new();
        for ev in b.open_channels(&run(), &[def]).unwrap() {
            b.apply(&ev);
        }
        b
    }

    fn limits() -> MessagingLimits {
        MessagingLimits::default()
    }

    struct Env {
        roster: Vec<RosterEntry>,
        limits: MessagingLimits,
        now: u64,
    }

    impl Env {
        fn new() -> Env {
            Env {
                roster: roster(),
                limits: limits(),
                now: 1_000_000,
            }
        }
    }

    fn redact_none(s: &str) -> String {
        s.to_string()
    }

    fn send(
        b: &MessageBoard,
        env: &Env,
        from: &str,
        to: Option<&str>,
        subject: &str,
        body: &str,
    ) -> Verdict {
        let to = to.map(t);
        let run = run();
        let sub = Submission {
            run: &run,
            from: Sender::Task(t(from)),
            channel: "ch",
            to: to.as_ref(),
            kind: MessageKind::Info,
            subject,
            body,
            in_reply_to: None,
            source: None,
        };
        let redact: &dyn Fn(&str) -> String = &redact_none;
        b.submit(
            &sub,
            &Context {
                roster: &env.roster,
                limits: &env.limits,
                now_ms: env.now,
                hasher: &Toy,
                redact,
            },
        )
    }

    fn accept(b: &mut MessageBoard, v: Verdict) -> MessageId {
        match v {
            Verdict::Accepted(a) => {
                b.apply(&a.event);
                a.id
            }
            Verdict::Refused(r) => panic!("refused: {:?} {}", r.reason, r.detail),
        }
    }

    fn refuse(b: &mut MessageBoard, v: Verdict) -> Refused {
        match v {
            Verdict::Refused(r) => {
                for ev in &r.events {
                    b.apply(ev);
                }
                r
            }
            Verdict::Accepted(a) => panic!("accepted {:?}", a.event),
        }
    }

    /// Sends a message and records that it was accepted.
    fn ok(
        b: &mut MessageBoard,
        env: &Env,
        from: &str,
        to: Option<&str>,
        subject: &str,
        body: &str,
    ) -> MessageId {
        let v = send(b, env, from, to, subject, body);
        accept(b, v)
    }

    /// Sends a message and records that it was refused.
    fn no(
        b: &mut MessageBoard,
        env: &Env,
        from: &str,
        to: Option<&str>,
        subject: &str,
        body: &str,
    ) -> Refused {
        let v = send(b, env, from, to, subject, body);
        refuse(b, v)
    }

    #[test]
    fn a_member_can_send_to_a_member_and_the_message_is_labelled_and_bounded() {
        let mut b = board_with(channel_def(vec![]));
        let env = Env::new();
        let id = ok(&mut b, &env, "a", Some("b"), "hi", "hello there");
        let m = b.message(&id).unwrap();
        assert_eq!(m.label(), "from task a (unverified peer message)");
        assert_eq!(m.recipients.len(), 1);
        assert_eq!(m.expires_ms, env.now + 900_000);
        assert_eq!(b.accepted_count(), 1);
        assert_eq!(b.sent_total(&run(), &name("ch"), &Sender::Task(t("a"))), 1);
    }

    #[test]
    fn a_task_that_is_not_a_member_cannot_send_and_nothing_is_stored() {
        let mut b = board_with(channel_def(vec![]));
        let env = Env::new();
        let r = no(&mut b, &env, "outsider", Some("b"), "hi", "hello there");
        assert_eq!(r.reason, BlockReason::NotMember);
        let m = b.message(&r.id).unwrap();
        assert!(m.is_refused() && m.body.is_empty() && m.subject.is_empty());
        assert_eq!(b.accepted_count(), 0);
        // an unknown task is not a member either
        let r = no(&mut b, &env, "ghost", Some("b"), "hi", "hello there");
        assert_eq!(r.reason, BlockReason::NotMember);
    }

    #[test]
    fn recipients_must_be_members_and_not_the_sender() {
        let mut b = board_with(channel_def(vec![]));
        let env = Env::new();
        for to in ["outsider", "a", "ghost"] {
            let r = no(&mut b, &env, "a", Some(to), "hi", "hello there");
            assert_eq!(r.reason, BlockReason::BadRecipient, "{to}");
        }
        // a direct channel needs `to`
        let r = no(&mut b, &env, "a", None, "hi", "hello there");
        assert_eq!(r.reason, BlockReason::BadRecipient);
    }

    #[test]
    fn a_broadcast_reaches_every_other_member_and_respects_fan_out() {
        let mut b = board_with(channel_def(vec![("mode", Value::str("broadcast"))]));
        let mut env = Env::new();
        let id = ok(&mut b, &env, "a", None, "hi", "to everybody");
        let to: Vec<String> = b
            .message(&id)
            .unwrap()
            .recipients
            .iter()
            .map(|r| r.task.to_string())
            .collect();
        assert_eq!(to, ["b", "c"]);
        // a message to one member only is allowed on a broadcast channel
        let id = ok(&mut b, &env, "a", Some("c"), "hi", "just you");
        assert_eq!(b.message(&id).unwrap().recipients.len(), 1);
        env.limits.max_fan_out = 1;
        let r = no(&mut b, &env, "a", None, "hi", "too many");
        assert_eq!(r.reason, BlockReason::FanOut);
    }

    #[test]
    fn a_to_lead_channel_carries_messages_to_the_lead_only() {
        let mut b = board_with(channel_def(vec![
            ("direction", Value::str("to_lead")),
            ("lead", Value::str("task:a")),
            ("mode", Value::str("broadcast")),
        ]));
        let mut env = Env::new();
        // a worker writes to the lead, with or without `to`
        let id = ok(&mut b, &env, "b", None, "report", "i am done");
        assert_eq!(b.message(&id).unwrap().recipients[0].task, t("a"));
        env.now += 1;
        ok(&mut b, &env, "c", Some("a"), "report", "so am i");
        // ... not to another worker
        let r = no(&mut b, &env, "b", Some("c"), "psst", "collude with me");
        assert_eq!(r.reason, BlockReason::BadRecipient);
        // and the lead does not write to the workers
        let r = no(&mut b, &env, "a", Some("b"), "order", "do this");
        assert_eq!(r.reason, BlockReason::Direction);
        let r = no(&mut b, &env, "a", None, "order", "do this");
        assert_eq!(r.reason, BlockReason::Direction);
    }

    #[test]
    fn size_caps_apply_to_the_raw_body() {
        let mut b = board_with(channel_def(vec![("max_message_bytes", Value::int(100))]));
        let env = Env::new();
        let r = no(&mut b, &env, "a", Some("b"), "hi", &"x".repeat(101));
        assert_eq!(r.reason, BlockReason::TooLarge);
        ok(&mut b, &env, "a", Some("b"), "hi", &"x".repeat(100));
    }

    #[test]
    fn a_body_that_looks_like_a_secret_is_refused_and_never_stored() {
        let mut b = board_with(channel_def(vec![]));
        let env = Env::new();
        let key = format!("{}{}", "ghp_", "a".repeat(36));
        let r = no(
            &mut b,
            &env,
            "a",
            Some("b"),
            "hi",
            &format!("use {key} to push"),
        );
        assert_eq!(r.reason, BlockReason::SecretShape);
        assert_eq!(r.detail, "github-token");
        for ev in &r.events {
            let text = format!("{ev:?}");
            assert!(!text.contains(&key), "{text}");
            assert!(!text.contains("to push"), "{text}");
        }
        // the subject is scanned as well
        let r = no(&mut b, &env, "a", Some("b"), &key, "innocent");
        assert_eq!(r.reason, BlockReason::SecretShape);
        assert!(!b.messages().any(|m| m.body.contains(&key)));
        assert_eq!(b.accepted_count(), 0);
    }

    #[test]
    fn text_is_sanitized_before_it_is_stored() {
        let mut b = board_with(channel_def(vec![]));
        let env = Env::new();
        let id = ok(
            &mut b,
            &env,
            "a",
            Some("b"),
            "subject\n\u{1b}[31mred",
            "body\u{1b}]0;title\u{7}text \u{202e}rtl",
        );
        let m = b.message(&id).unwrap();
        assert!(
            !m.subject.contains('\n') && !m.subject.contains('\u{1b}'),
            "{:?}",
            m.subject
        );
        assert!(
            !m.body.contains('\u{1b}') && !m.body.contains('\u{202e}'),
            "{:?}",
            m.body
        );
        assert!(m.body.contains("text"));
    }

    #[test]
    fn empty_subjects_and_bodies_are_invalid() {
        let mut b = board_with(channel_def(vec![]));
        let env = Env::new();
        for (s, body) in [("", "x"), ("s", ""), ("  ", "x"), ("s", " \n ")] {
            let r = no(&mut b, &env, "a", Some("b"), s, body);
            assert_eq!(r.reason, BlockReason::Invalid, "{s:?} {body:?}");
        }
    }

    #[test]
    fn the_redactor_runs_after_the_scan() {
        let b = board_with(channel_def(vec![]));
        let env = Env::new();
        let run = run();
        let to = t("b");
        let sub = Submission {
            run: &run,
            from: Sender::Task(t("a")),
            channel: "ch",
            to: Some(&to),
            kind: MessageKind::Info,
            subject: "s",
            body: "the password is hunter2",
            in_reply_to: None,
            source: None,
        };
        let redact = |s: &str| s.replace("hunter2", "[REDACTED]");
        let v = b.submit(
            &sub,
            &Context {
                roster: &env.roster,
                limits: &env.limits,
                now_ms: env.now,
                hasher: &Toy,
                redact: &redact,
            },
        );
        match v {
            Verdict::Accepted(a) => match a.event {
                DomainEvent::MessageSubmitted { body, .. } => {
                    assert_eq!(body, "the password is [REDACTED]")
                }
                other => panic!("{other:?}"),
            },
            Verdict::Refused(r) => panic!("{r:?}"),
        }
    }

    #[test]
    fn duplicates_are_suppressed_for_the_lifetime_of_the_message() {
        let mut b = board_with(channel_def(vec![]));
        let mut env = Env::new();
        ok(&mut b, &env, "a", Some("b"), "hi", "same words");
        env.now += 1000;
        let r = no(&mut b, &env, "a", Some("b"), "hi", "same words");
        assert_eq!(r.reason, BlockReason::Duplicate);
        // other recipients, other words or other senders are not duplicates
        ok(&mut b, &env, "a", Some("c"), "hi", "same words");
        ok(&mut b, &env, "a", Some("b"), "hi", "other words");
        ok(&mut b, &env, "c", Some("b"), "hi", "same words");
        // after the ttl the same words may be sent again
        env.now += 900_000;
        ok(&mut b, &env, "a", Some("b"), "hi", "same words");
    }

    #[test]
    fn rate_limits_use_the_clock_and_count_per_sender() {
        let mut b = board_with(channel_def(vec![
            ("max_messages_per_minute", Value::int(3)),
            ("max_messages_per_task", Value::int(100)),
        ]));
        let mut env = Env::new();
        for i in 0..3 {
            ok(&mut b, &env, "a", Some("b"), "hi", &format!("message {i}"));
        }
        let r = no(&mut b, &env, "a", Some("b"), "hi", "message 4");
        assert_eq!(r.reason, BlockReason::Rate);
        // another sender has its own allowance
        ok(&mut b, &env, "c", Some("b"), "hi", "message c");
        // a minute later the first sender may send again
        env.now += 60_000;
        ok(&mut b, &env, "a", Some("b"), "hi", "message 5");
    }

    #[test]
    fn totals_per_task_and_per_channel_and_the_global_cap_are_enforced() {
        let mut b = board_with(channel_def(vec![
            ("max_messages_per_task", Value::int(2)),
            ("max_messages_per_minute", Value::int(100)),
        ]));
        let mut env = Env::new();
        env.limits.max_per_task_per_minute = 100;
        for i in 0..2 {
            ok(&mut b, &env, "a", Some("b"), "hi", &format!("m{i}"));
        }
        let r = no(&mut b, &env, "a", Some("b"), "hi", "m2");
        assert_eq!(r.reason, BlockReason::Quota, "per task");
        for i in 0..2 {
            ok(&mut b, &env, "b", Some("a"), "hi", &format!("n{i}"));
        }
        for i in 0..2 {
            ok(&mut b, &env, "c", Some("a"), "hi", &format!("o{i}"));
        }
        // 2 per task over 3 members: the channel total (6) is used up
        let r = no(&mut b, &env, "c", Some("b"), "hi", "o2");
        assert_eq!(r.reason, BlockReason::Quota);

        let mut b = board_with(channel_def(vec![(
            "max_messages_per_minute",
            Value::int(100),
        )]));
        env.limits.max_total_messages = 2;
        ok(&mut b, &env, "a", Some("b"), "hi", "g1");
        ok(&mut b, &env, "b", Some("c"), "hi", "g2");
        let r = no(&mut b, &env, "c", Some("a"), "hi", "g3");
        assert_eq!(r.reason, BlockReason::GlobalCap);
    }

    #[test]
    fn a_task_wide_rate_limit_spans_channels() {
        let mut b = board_with(channel_def(vec![
            ("max_messages_per_minute", Value::int(100)),
            ("max_messages_per_task", Value::int(100)),
        ]));
        let mut env = Env::new();
        env.limits.max_per_task_per_minute = 2;
        ok(&mut b, &env, "a", Some("b"), "hi", "one");
        ok(&mut b, &env, "a", Some("c"), "hi", "two");
        let r = no(&mut b, &env, "a", Some("b"), "hi", "three");
        assert_eq!(r.reason, BlockReason::Rate);
    }

    #[test]
    fn a_reply_must_answer_a_message_of_this_channel() {
        let mut b = board_with(channel_def(vec![]));
        let env = Env::new();
        let first = ok(&mut b, &env, "a", Some("b"), "hi", "question");
        let run = run();
        let to = t("a");
        let mk = |parent: &MessageId, env: &Env, b: &MessageBoard| {
            let sub = Submission {
                run: &run,
                from: Sender::Task(t("b")),
                channel: "ch",
                to: Some(&to),
                kind: MessageKind::Info,
                subject: "re",
                body: "answer",
                in_reply_to: Some(parent),
                source: None,
            };
            b.submit(
                &sub,
                &Context {
                    roster: &env.roster,
                    limits: &env.limits,
                    now_ms: env.now,
                    hasher: &Toy,
                    redact: &redact_none,
                },
            )
        };
        let v = mk(&first, &env, &b);
        let id = accept(&mut b, v);
        assert_eq!(b.message(&id).unwrap().depth, 1);
        let v = mk(&MessageId::from_seq(99), &env, &b);
        let r = refuse(&mut b, v);
        assert_eq!(r.reason, BlockReason::UnknownReply);
    }

    #[test]
    fn a_ping_pong_trips_the_depth_limit_without_in_reply_to_and_asks_for_a_mute() {
        let mut b = board_with(channel_def(vec![
            ("max_messages_per_minute", Value::int(100)),
            ("max_messages_per_task", Value::int(100)),
        ]));
        let mut env = Env::new();
        env.limits.max_reply_depth = 3;
        env.limits.max_pair_exchanges = 100;
        env.limits.max_per_task_per_minute = 100;
        let mut tripped = None;
        for i in 0..10 {
            env.now += 10;
            let (from, to) = if i % 2 == 0 { ("a", "b") } else { ("b", "a") };
            match send(&b, &env, from, Some(to), "ping", &format!("round {i}")) {
                Verdict::Accepted(a) => b.apply(&a.event),
                Verdict::Refused(r) => {
                    tripped = Some((i, r));
                    break;
                }
            }
        }
        let (i, r) = tripped.expect("the loop must be stopped");
        assert_eq!(i, 4, "depths 0..=3 pass, the fifth message is depth 4");
        assert_eq!(r.reason, BlockReason::ReplyDepth);
        let trip = r.trip.unwrap();
        assert_eq!(trip.reason, MuteReason::ReplyDepth);
        assert_eq!(trip.channel, Some(name("ch")));
        assert_eq!(trip.sender, t("a"));
        assert!(trip.text.contains("depth 4") && trip.text.contains("limit is 3"));
    }

    #[test]
    fn the_pair_budget_stops_exchanges_that_avoid_replying() {
        let mut b = board_with(channel_def(vec![
            ("max_messages_per_minute", Value::int(100)),
            ("max_messages_per_task", Value::int(100)),
        ]));
        let mut env = Env::new();
        env.limits.max_pair_exchanges = 5;
        env.limits.max_per_task_per_minute = 100;
        env.limits.max_reply_depth = 100;
        let mut count = 0;
        loop {
            env.now += 10;
            // a talks to b in one direction only: no reply depth builds up
            match send(&b, &env, "a", Some("b"), "x", &format!("m{count}")) {
                Verdict::Accepted(a) => {
                    b.apply(&a.event);
                    count += 1;
                }
                Verdict::Refused(r) => {
                    assert_eq!(r.reason, BlockReason::PairBudget);
                    assert_eq!(r.trip.unwrap().reason, MuteReason::PairBudget);
                    break;
                }
            }
            assert!(count <= 5);
        }
        assert_eq!(count, 5);
    }

    #[test]
    fn muted_channels_and_tasks_accept_nothing_until_unmuted_and_unmuting_resets_the_budget() {
        let mut b = board_with(channel_def(vec![]));
        let env = Env::new();
        let ev = b
            .mute_channel(
                &run(),
                &name("ch"),
                MuteReason::Operator,
                "operator",
                None,
                env.now,
            )
            .unwrap();
        b.apply(&ev);
        assert!(b.channel(&run(), &name("ch")).unwrap().muted.is_some());
        let r = no(&mut b, &env, "a", Some("b"), "hi", "hello");
        assert_eq!(r.reason, BlockReason::ChannelMuted);
        assert!(b
            .mute_channel(
                &run(),
                &name("ch"),
                MuteReason::Operator,
                "operator",
                None,
                env.now
            )
            .is_err());
        let ev = b
            .unmute_channel(&run(), &name("ch"), "operator", env.now)
            .unwrap();
        b.apply(&ev);
        ok(&mut b, &env, "a", Some("b"), "hi", "hello again");
        assert!(b
            .unmute_channel(&run(), &name("ch"), "operator", env.now)
            .is_err());
        // a muted task cannot send, on any channel
        let ev = b
            .mute_task(
                &run(),
                &t("a"),
                MuteReason::Operator,
                "operator",
                None,
                env.now,
            )
            .unwrap();
        b.apply(&ev);
        let r = no(&mut b, &env, "a", Some("b"), "hi", "hello once more");
        assert_eq!(r.reason, BlockReason::TaskMuted);
        ok(&mut b, &env, "c", Some("b"), "hi", "from someone else");
        let ev = b.unmute_task(&run(), &t("a"), "operator", env.now).unwrap();
        b.apply(&ev);
        assert!(b.task_muted(&run(), &t("a")).is_none());
        assert!(b
            .mute_channel(&run(), &name("nope"), MuteReason::Operator, "o", None, 0)
            .is_err());
    }

    #[test]
    fn a_task_that_floods_with_refused_messages_is_muted() {
        let mut b = board_with(channel_def(vec![]));
        let mut env = Env::new();
        let mut flood = None;
        for i in 0..(MAX_BLOCKED_PER_MINUTE + 5) {
            env.now += 10;
            // an unknown recipient is refused every time
            let r = no(&mut b, &env, "a", Some("ghost"), "x", &format!("m{i}"));
            if let Some(trip) = &r.trip {
                flood = Some((i, trip.clone()));
                break;
            }
        }
        let (i, trip) = flood.expect("a flood must be noticed");
        assert_eq!(i, MAX_BLOCKED_PER_MINUTE);
        assert_eq!(trip.reason, MuteReason::Flood);
        assert_eq!(trip.task, Some(t("a")));
        assert_eq!(trip.channel, None);
    }

    #[test]
    fn an_operator_message_is_allowed_on_a_to_lead_channel_and_is_labelled() {
        let mut b = board_with(channel_def(vec![
            ("direction", Value::str("to_lead")),
            ("lead", Value::str("task:a")),
        ]));
        let env = Env::new();
        let run = run();
        let to = t("b");
        let sub = Submission {
            run: &run,
            from: Sender::Operator,
            channel: "ch",
            to: Some(&to),
            kind: MessageKind::Request,
            subject: "please stop",
            body: "wrap up",
            in_reply_to: None,
            source: None,
        };
        let cx = Context {
            roster: &env.roster,
            limits: &env.limits,
            now_ms: env.now,
            hasher: &Toy,
            redact: &redact_none,
        };
        let v = b.submit(&sub, &cx);
        let id = accept(&mut b, v);
        assert_eq!(b.message(&id).unwrap().label(), "from operator");
        // the same bounds apply: size, secret shapes, muting
        let big = Submission {
            body: &"x".repeat(5000),
            ..sub.clone()
        };
        match b.submit(&big, &cx) {
            Verdict::Refused(r) => assert_eq!(r.reason, BlockReason::TooLarge),
            other => panic!("{other:?}"),
        }
        let key = format!("{}{}", "sk_live_", "k".repeat(24));
        let leaky = Submission {
            body: &key,
            ..sub.clone()
        };
        match b.submit(&leaky, &cx) {
            Verdict::Refused(r) => assert_eq!(r.reason, BlockReason::SecretShape),
            other => panic!("{other:?}"),
        }
        // an operator is not rate limited as a task is, and needs no approval
        let approval_channel =
            board_with(channel_def(vec![("requires_approval", Value::Bool(true))]));
        match approval_channel.submit(&sub, &cx) {
            Verdict::Accepted(a) => assert!(a.action.is_none()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_channel_that_requires_approval_asks_for_one_bound_to_the_message() {
        let mut b = board_with(channel_def(vec![("requires_approval", Value::Bool(true))]));
        let env = Env::new();
        let v = send(&b, &env, "a", Some("b"), "hi", "needs a person");
        let Verdict::Accepted(a) = v else { panic!() };
        let action = a.action.clone().expect("an approval is needed");
        assert_eq!(action.tool, "message");
        let a = a.with_approval(ApprovalId::from_seq(1));
        b.apply(&a.event);
        let m = b.message(&a.id).unwrap();
        assert_eq!(m.approval, Some(ApprovalId::from_seq(1)));
        assert_eq!(m.approval_action(), action);
        assert_eq!(
            m.status(true),
            crate::status::MessageStatus::AwaitingApproval
        );
        assert!(!action.display().contains("needs a person"));
    }

    fn due_with(
        b: &MessageBoard,
        now: u64,
        roster: Vec<RosterEntry>,
        approval: impl Fn(&ApprovalId) -> Option<ApprovalStatus>,
    ) -> Vec<Due> {
        b.due(now, &|_| roster.clone(), &approval)
    }

    #[test]
    fn delivery_waits_for_a_running_recipient_and_expires_by_ttl() {
        let mut b = board_with(channel_def(vec![("ttl_secs", Value::int(60))]));
        let env = Env::new();
        let id = ok(&mut b, &env, "a", Some("b"), "hi", "waiting");
        // b is running: due now
        let due = due_with(&b, env.now, roster(), |_| None);
        assert_eq!(
            due,
            vec![Due::Deliver {
                id: id.clone(),
                to: t("b")
            }]
        );
        // b is ready but not running: nothing happens, the message waits
        let mut waiting = roster();
        waiting[1] = entry("b", TaskStatus::Ready);
        assert!(due_with(&b, env.now + 1000, waiting.clone(), |_| None).is_empty());
        // ... until the time to live is over
        let due = due_with(&b, env.now + 60_000, waiting, |_| None);
        assert_eq!(
            due,
            vec![Due::Expire {
                id: id.clone(),
                to: t("b"),
                reason: ExpireReason::Ttl
            }]
        );
    }

    #[test]
    fn a_recipient_that_ended_makes_its_messages_expire_at_once() {
        let mut b = board_with(channel_def(vec![]));
        let env = Env::new();
        let id = ok(&mut b, &env, "a", Some("b"), "hi", "late");
        let mut ended = roster();
        ended[1] = entry("b", TaskStatus::Failed);
        assert_eq!(
            due_with(&b, env.now, ended, |_| None),
            vec![Due::Expire {
                id,
                to: t("b"),
                reason: ExpireReason::RecipientEnded
            }]
        );
    }

    #[test]
    fn nothing_is_delivered_through_a_mute_and_it_goes_on_after_unmuting() {
        let mut b = board_with(channel_def(vec![]));
        let env = Env::new();
        let id = ok(&mut b, &env, "a", Some("b"), "hi", "held");
        let ev = b
            .mute_channel(
                &run(),
                &name("ch"),
                MuteReason::Operator,
                "operator",
                None,
                env.now,
            )
            .unwrap();
        b.apply(&ev);
        assert!(due_with(&b, env.now, roster(), |_| None).is_empty());
        let ev = b
            .unmute_channel(&run(), &name("ch"), "operator", env.now)
            .unwrap();
        b.apply(&ev);
        assert_eq!(due_with(&b, env.now, roster(), |_| None).len(), 1);
        // a muted recipient or a muted sender holds it too
        let ev = b
            .mute_task(
                &run(),
                &t("b"),
                MuteReason::Operator,
                "operator",
                None,
                env.now,
            )
            .unwrap();
        b.apply(&ev);
        assert!(due_with(&b, env.now, roster(), |_| None).is_empty());
        let ev = b.unmute_task(&run(), &t("b"), "operator", env.now).unwrap();
        b.apply(&ev);
        let ev = b
            .mute_task(
                &run(),
                &t("a"),
                MuteReason::Operator,
                "operator",
                None,
                env.now,
            )
            .unwrap();
        b.apply(&ev);
        assert!(due_with(&b, env.now, roster(), |_| None).is_empty());
        let _ = id;
    }

    #[test]
    fn delivery_follows_the_approval() {
        let mut b = board_with(channel_def(vec![("requires_approval", Value::Bool(true))]));
        let env = Env::new();
        let Verdict::Accepted(a) = send(&b, &env, "a", Some("b"), "hi", "approve me") else {
            panic!()
        };
        let a = a.with_approval(ApprovalId::from_seq(4));
        b.apply(&a.event);
        let id = a.id.clone();
        let with =
            |status: Option<ApprovalStatus>| due_with(&b, env.now, roster(), move |_| status);
        assert!(with(Some(ApprovalStatus::Pending)).is_empty());
        assert_eq!(with(Some(ApprovalStatus::Approved)).len(), 1);
        assert_eq!(with(Some(ApprovalStatus::Consumed)).len(), 1);
        assert_eq!(
            with(Some(ApprovalStatus::Denied)),
            vec![Due::Deny { id: id.clone() }]
        );
        assert_eq!(
            with(Some(ApprovalStatus::Expired)),
            vec![Due::Expire {
                id: id.clone(),
                to: t("b"),
                reason: ExpireReason::ApprovalExpired
            }]
        );
        assert_eq!(with(None).len(), 1);
        // a denial refuses the message and drops its text
        b.apply(&DomainEvent::MessageBlocked {
            id: id.clone(),
            run: run(),
            channel: "ch".into(),
            from: "task:a".into(),
            reason: BlockReason::Denied,
            detail: String::new(),
            digest: String::new(),
            bytes: 0,
            at_ms: env.now,
            source: None,
        });
        let m = b.message(&id).unwrap();
        assert!(m.is_refused() && m.body.is_empty());
        assert!(due_with(&b, env.now, roster(), |_| Some(ApprovalStatus::Approved)).is_empty());
    }

    #[test]
    fn delivered_and_expired_messages_are_never_due_again_and_inboxes_keep_their_order() {
        let mut b = board_with(channel_def(vec![]));
        let mut env = Env::new();
        let m1 = ok(&mut b, &env, "a", Some("b"), "one", "first");
        env.now += 5;
        let m2 = ok(&mut b, &env, "c", Some("b"), "two", "second");
        for (id, at) in [(&m2, 20u64), (&m1, 30u64)] {
            b.apply(&DomainEvent::MessageDelivered {
                id: id.clone(),
                to: t("b"),
                at_ms: at,
            });
        }
        assert!(due_with(&b, env.now, roster(), |_| None).is_empty());
        let items = b.inbox_items(&run(), &t("b"), &Toy);
        let ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            [m2.as_str(), m1.as_str()],
            "in the order they were delivered"
        );
        assert_eq!(items[0].seq, 1);
        assert_eq!(items[1].seq, 2);
        assert_eq!(b.next_inbox_seq(&run(), &t("b")), 3);
        // delivering the same message twice changes nothing
        b.apply(&DomainEvent::MessageDelivered {
            id: m1.clone(),
            to: t("b"),
            at_ms: 99,
        });
        assert_eq!(b.next_inbox_seq(&run(), &t("b")), 3);
        assert_eq!(
            b.message(&m1).unwrap().status(false),
            crate::status::MessageStatus::Delivered
        );
        assert!(b.pending_for(&run(), &t("b")).is_empty());
    }

    #[test]
    fn a_full_inbox_expires_the_message() {
        let mut b = board_with(channel_def(vec![]));
        let env = Env::new();
        let id = ok(&mut b, &env, "a", Some("b"), "hi", "one more");
        // fill b's inbox with delivered messages of other ids
        for i in 0..MAX_INBOX_ENTRIES {
            let mid = MessageId::from_seq(1000 + i as u64);
            b.apply(&DomainEvent::MessageSubmitted {
                id: mid.clone(),
                run: run(),
                channel: name("ch"),
                from: "operator".into(),
                to: vec![t("b")],
                kind: MessageKind::Info,
                subject: "s".into(),
                body: "b".into(),
                in_reply_to: None,
                depth: 0,
                digest: format!("d{i}"),
                at_ms: 1,
                expires_ms: u64::MAX,
                approval: None,
                source: None,
            });
            b.apply(&DomainEvent::MessageDelivered {
                id: mid,
                to: t("b"),
                at_ms: 2,
            });
        }
        assert_eq!(
            due_with(&b, env.now, roster(), |_| None),
            vec![Due::Expire {
                id,
                to: t("b"),
                reason: ExpireReason::InboxFull
            }]
        );
    }

    #[test]
    fn channels_with_nobody_left_are_found_once() {
        let mut b = board_with(channel_def(vec![]));
        let alive = roster();
        assert!(b.dead_channels(&|_| alive.clone()).is_empty());
        let mut gone = roster();
        gone[0] = entry("a", TaskStatus::Completed);
        gone[1] = entry("b", TaskStatus::Failed);
        assert_eq!(
            b.dead_channels(&|_| gone.clone()),
            vec![(run(), name("ch"))]
        );
        let ev = b
            .mute_channel(
                &run(),
                &name("ch"),
                MuteReason::EndpointEnded,
                "auto",
                None,
                5,
            )
            .unwrap();
        b.apply(&ev);
        assert!(
            b.dead_channels(&|_| gone.clone()).is_empty(),
            "already muted"
        );
    }

    #[test]
    fn a_mute_gate_answer_unmutes_or_keeps() {
        let mut b = board_with(channel_def(vec![]));
        let gate = DecisionId::from_seq(3);
        let ev = b
            .mute_channel(
                &run(),
                &name("ch"),
                MuteReason::ReplyDepth,
                "auto",
                Some(gate.clone()),
                5,
            )
            .unwrap();
        b.apply(&ev);
        assert_eq!(
            b.gate_target(&gate),
            Some(&MuteTarget::Channel(run(), name("ch")))
        );
        assert!(b.answer_gate(&gate, GATE_KEEP, 6).is_empty());
        let events = b.answer_gate(&gate, GATE_UNMUTE, 6);
        assert_eq!(events.len(), 1);
        for e in &events {
            b.apply(e);
        }
        assert!(b.channel(&run(), &name("ch")).unwrap().muted.is_none());
        assert!(
            b.answer_gate(&gate, GATE_UNMUTE, 7).is_empty(),
            "already unmuted"
        );
        assert!(b
            .answer_gate(&DecisionId::from_seq(9), GATE_UNMUTE, 7)
            .is_empty());
        // a task mute gate
        let gate = DecisionId::from_seq(4);
        let ev = b
            .mute_task(
                &run(),
                &t("a"),
                MuteReason::Flood,
                "auto",
                Some(gate.clone()),
                8,
            )
            .unwrap();
        b.apply(&ev);
        for e in b.answer_gate(&gate, GATE_UNMUTE, 9) {
            b.apply(&e);
        }
        assert!(b.task_muted(&run(), &t("a")).is_none());
    }

    #[test]
    fn channels_are_declared_once_and_bounded() {
        let mut b = board_with(channel_def(vec![]));
        let dup = channel_def(vec![]);
        assert!(
            b.open_channels(&run(), std::slice::from_ref(&dup)).is_err(),
            "same name twice"
        );
        assert!(b.open_channels(&run(), &[dup.clone(), dup]).is_err());
        let other_run = RunId::parse("r2").unwrap();
        for ev in b.open_channels(&other_run, &[channel_def(vec![])]).unwrap() {
            b.apply(&ev);
        }
        let mk = |n: usize| {
            ChannelDef::from_value(&Value::object([
                ("name", Value::str(format!("c{n}"))),
                (
                    "members",
                    Value::array(["task:a", "task:b"].map(Value::str)),
                ),
            ]))
            .unwrap()
        };
        let many: Vec<ChannelDef> = (0..MAX_CHANNELS_PER_RUN).map(mk).collect();
        assert!(b.open_channels(&other_run, &many).is_err());
        let ok: Vec<ChannelDef> = (0..MAX_CHANNELS_PER_RUN - 1).map(mk).collect();
        for ev in b.open_channels(&RunId::parse("r3").unwrap(), &ok).unwrap() {
            b.apply(&ev);
        }
        assert_eq!(b.channels().count(), MAX_CHANNELS_PER_RUN + 1);
    }

    #[test]
    fn channels_are_found_by_name_or_by_run_and_name() {
        let mut b = board_with(channel_def(vec![]));
        assert_eq!(b.find_channel("ch").unwrap(), (run(), name("ch")));
        assert_eq!(b.find_channel("r1/ch").unwrap(), (run(), name("ch")));
        assert!(b.find_channel("nope").is_err());
        assert!(b.find_channel("r2/ch").is_err());
        assert!(b.find_channel("bad name").is_err());
        let r2 = RunId::parse("r2").unwrap();
        for ev in b.open_channels(&r2, &[channel_def(vec![])]).unwrap() {
            b.apply(&ev);
        }
        assert!(b.find_channel("ch").is_err(), "ambiguous");
        assert_eq!(b.find_channel("r2/ch").unwrap().0, r2);
    }

    #[test]
    fn replaying_the_events_rebuilds_the_same_board() {
        let mut original = board_with(channel_def(vec![]));
        let mut log: Vec<DomainEvent> = original
            .open_channels(&RunId::parse("r9").unwrap(), &[channel_def(vec![])])
            .unwrap();
        let env = Env::new();
        for ev in &log {
            original.apply(ev);
        }
        let mut sent = Vec::new();
        for (i, (from, to)) in [("a", "b"), ("b", "c"), ("c", "a")].iter().enumerate() {
            let v = send(&original, &env, from, Some(to), "s", &format!("body {i}"));
            let Verdict::Accepted(a) = v else { panic!() };
            original.apply(&a.event);
            sent.push(a.event);
        }
        let bad = send(&original, &env, "outsider", Some("a"), "s", "x");
        let r = refuse(&mut original, bad);
        sent.extend(r.events);
        sent.push(DomainEvent::MessageDelivered {
            id: MessageId::from_seq(1),
            to: t("b"),
            at_ms: 7,
        });
        original.apply(sent.last().unwrap());
        log.extend(sent);
        let mut rebuilt = MessageBoard::new();
        let first = ChannelDef::from_value(&Value::object([
            ("name", Value::str("ch")),
            (
                "members",
                Value::array(["task:a", "task:b", "task:c"].map(Value::str)),
            ),
        ]))
        .unwrap();
        rebuilt.apply(&first.opened_event(&run()));
        for ev in &log {
            rebuilt.apply(ev);
        }
        assert_eq!(rebuilt.accepted_count(), original.accepted_count());
        assert_eq!(
            rebuilt
                .messages()
                .map(|m| (m.id.clone(), m.status(false)))
                .collect::<Vec<_>>(),
            original
                .messages()
                .map(|m| (m.id.clone(), m.status(false)))
                .collect::<Vec<_>>()
        );
        assert_eq!(rebuilt.next_inbox_seq(&run(), &t("b")), 2);
        assert_eq!(
            rebuilt.sent_total(&run(), &name("ch"), &Sender::Task(t("a"))),
            original.sent_total(&run(), &name("ch"), &Sender::Task(t("a")))
        );
        // the next id continues after the highest used one, refused messages included
        let v = send(&rebuilt, &env, "a", Some("c"), "s", "next");
        let Verdict::Accepted(a) = v else { panic!() };
        assert_eq!(a.id.seq(), 5);
    }

    #[test]
    fn refused_messages_are_the_first_to_be_forgotten() {
        let mut b = board_with(channel_def(vec![]));
        let env = Env::new();
        let keep = ok(&mut b, &env, "a", Some("b"), "hi", "keep me");
        for i in 0..(MESSAGES_KEPT + 10) {
            b.apply(&DomainEvent::MessageBlocked {
                id: MessageId::from_seq(10 + i as u64),
                run: run(),
                channel: "ch".into(),
                from: "operator".into(),
                reason: BlockReason::Invalid,
                detail: String::new(),
                digest: String::new(),
                bytes: 0,
                at_ms: 1,
                source: None,
            });
        }
        assert!(b.messages().count() <= MESSAGES_KEPT);
        assert!(b.message(&keep).is_some(), "an accepted message stays");
        assert!(
            b.message(&MessageId::from_seq(10)).is_none(),
            "the oldest refusal went"
        );
    }

    #[test]
    fn the_brief_notes_list_channels_inbox_and_waiting_messages() {
        let mut b = board_with(channel_def(vec![]));
        let env = Env::new();
        ok(&mut b, &env, "a", Some("b"), "question", "hello");
        let worker = crate::ids::WorkerName::parse("agent").unwrap();
        let notes = b.notes_for(&run(), &t("b"), Some(&worker));
        assert_eq!(notes.channels.len(), 1);
        assert_eq!(notes.waiting.len(), 1);
        assert_eq!(notes.waiting[0].subject, "question");
        assert_eq!(notes.in_inbox, 0);
        let none = b.notes_for(&run(), &t("outsider-not-listed"), None);
        assert!(none.channels.is_empty());
    }
}
