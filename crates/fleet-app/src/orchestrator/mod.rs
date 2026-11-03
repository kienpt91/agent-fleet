//! The orchestrator: the application service that turns commands and the passage of time into
//! domain events, using ports for everything outside the domain.
//!
//! It is single-threaded by design (the daemon runs it on one thread and serializes requests to
//! it); long work happens behind the `JobRunner` and `SessionHost` ports. Every state change goes
//! through [`Orchestrator::commit`]: events are appended to the store first and applied to the
//! in-memory state only afterwards, so a store failure leaves state and log consistent, and
//! subscribers only ever see events that are durable.

mod commands;
mod dispatch;
mod messaging;
mod reconcile;
mod supervise;

use crate::config::AppConfig;
use crate::neo::{VerifyOutcome, Verifying};
use crate::ports::{Ports, SessionId, StoredEvent};
use crate::views::Recent;
use fleet_domain::error::Result;
use fleet_domain::events::DomainEvent;
use fleet_domain::fleet::{FleetState, Policy};
use fleet_domain::ids::{ApprovalId, RunId, TaskId};
use fleet_domain::msg::board::MessageBoard;
use fleet_domain::run::{RetryPolicy, WorkerEnd};
use fleet_domain::sanitize::{head_str, sanitize_text};
use fleet_domain::supervision::{KillEscalation, SupervisionPolicy};
use neo_goals::ledger::Ledger;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Entries kept for the activity feed.
const RECENT_KEPT: usize = 200;
/// The same warning is not recorded again within this time.
const WARNING_REPEAT_MS: u64 = 60_000;
/// Maximum characters of a warning message.
const WARNING_MAX: usize = 400;

pub(crate) type Key = (RunId, TaskId);

/// A running worker session.
pub(crate) struct Live {
    pub session: SessionId,
    pub attempt: u32,
    pub worktree: String,
    pub supervision: SupervisionPolicy,
    pub kill: Option<KillEscalation>,
    /// When a question file was first seen while the worker kept running.
    pub question_seen_ms: Option<u64>,
}

pub struct Orchestrator {
    pub(crate) cfg: AppConfig,
    pub(crate) ports: Ports,
    pub(crate) state: FleetState,
    /// The goal ledger, a projection of the same event log as `state`.
    pub(crate) ledger: Ledger,
    /// The message channels and messages, a projection of the same event log.
    pub(crate) board: MessageBoard,
    pub(crate) policy: Policy,
    pub(crate) live: BTreeMap<Key, Live>,
    /// Quality-gate jobs in flight: (run, task, attempt).
    pub(crate) gate_jobs: BTreeSet<(RunId, TaskId, u32)>,
    /// Reviews whose worker output was committed already.
    pub(crate) committed: BTreeSet<(RunId, TaskId, u32)>,
    pub(crate) command_jobs: BTreeSet<ApprovalId>,
    /// Verification jobs in flight: the task's attempt and why they run.
    pub(crate) verifying: BTreeMap<Key, Verifying>,
    /// Tickets handed out by `verify`, and how the last runs ended.
    pub(crate) verify_seq: u64,
    pub(crate) verify_outcomes: VecDeque<VerifyOutcome>,
    /// Reviews whose contracts passed in fleet's own run: (run, task, attempt). Only these may
    /// go on to the quality gates and complete.
    pub(crate) verified: BTreeSet<(RunId, TaskId, u32)>,
    /// Session ends that could not be recorded yet (the store failed); retried every tick so a
    /// task never stays `running` without a session.
    pub(crate) pending_ends: BTreeMap<Key, WorkerEnd>,
    /// Invalid outbox files rejected without an event since the last one that was recorded, per
    /// task (not persisted: it only enriches the next record).
    pub(crate) skipped_files: BTreeMap<Key, u32>,
    pub(crate) recent: VecDeque<Recent>,
    pub(crate) started_ms: u64,
    pub(crate) pid: u64,
    pub(crate) version: String,
    pub(crate) shutting_down: bool,
    pub(crate) shutdown_requested: bool,
    last_warning: Option<(String, u64)>,
}

impl Orchestrator {
    /// Builds the orchestrator and rebuilds state by replaying the event store.
    pub fn new(cfg: AppConfig, ports: Ports) -> Result<Orchestrator> {
        cfg.validate()?;
        let mut state = FleetState::new();
        let mut ledger = Ledger::new();
        let mut board = MessageBoard::new();
        let mut recent: VecDeque<Recent> = VecDeque::new();
        ports.store.replay(&mut |s: StoredEvent| {
            push_recent(&mut recent, s.seq, s.ts_ms, &s.event);
            state.apply(&s.event);
            ledger.apply_checked(&s.event, ports.digest.as_ref());
            board.apply(&s.event);
            Ok(())
        })?;
        let policy = Policy {
            retry: RetryPolicy {
                max_retries: cfg.max_retries,
            },
            approval_ttl_ms: cfg.approval_ttl_ms,
            external_commands: cfg.external_commands,
            merge_needs_approval: cfg.merge_needs_approval,
        };
        let started_ms = ports.clock.now_ms();
        Ok(Orchestrator {
            cfg,
            ports,
            state,
            ledger,
            board,
            policy,
            live: BTreeMap::new(),
            gate_jobs: BTreeSet::new(),
            committed: BTreeSet::new(),
            command_jobs: BTreeSet::new(),
            verifying: BTreeMap::new(),
            verify_seq: 0,
            verify_outcomes: VecDeque::new(),
            verified: BTreeSet::new(),
            pending_ends: BTreeMap::new(),
            skipped_files: BTreeMap::new(),
            recent,
            started_ms,
            pid: 0,
            version: String::new(),
            shutting_down: false,
            shutdown_requested: false,
            last_warning: None,
        })
    }

    pub fn state(&self) -> &FleetState {
        &self.state
    }

    pub fn config(&self) -> &AppConfig {
        &self.cfg
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested
    }

    /// Number of worker sessions currently alive.
    pub fn live_count(&self) -> usize {
        self.live.len()
    }

    /// The process groups that count as worker origin for the control socket: live worker
    /// sessions and the children of running jobs (gates, approved commands, verification checks),
    /// which execute files a worker wrote.
    pub fn live_sessions(&self) -> Vec<fleet_domain::supervision::LiveSession> {
        let mut live = self.ports.sessions.live();
        live.extend(self.ports.jobs.live());
        live
    }

    /// The session of a live task, for the transport's attach and send paths.
    pub fn session_of(&self, run: &RunId, task: &TaskId) -> Option<SessionId> {
        self.live
            .get(&(run.clone(), task.clone()))
            .map(|l| l.session)
    }

    pub(crate) fn now(&self) -> u64 {
        self.ports.clock.now_ms()
    }

    /// Stores events, then applies and publishes them.
    pub(crate) fn commit(&mut self, events: Vec<DomainEvent>) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let ts = self.now();
        let last = self.ports.store.append(ts, &events)?;
        let count = events.len() as u64;
        let first = last.saturating_add(1).saturating_sub(count);
        let mut stored = Vec::with_capacity(events.len());
        let mut committed_task = false;
        for (i, event) in events.into_iter().enumerate() {
            let seq = first.saturating_add(i as u64);
            self.state.apply(&event);
            self.ledger
                .apply_checked(&event, self.ports.digest.as_ref());
            self.board.apply(&event);
            committed_task |= matches!(event, DomainEvent::TaskCommitted { .. });
            push_recent(&mut self.recent, seq, ts, &event);
            stored.push(StoredEvent {
                seq,
                ts_ms: ts,
                event,
            });
        }
        self.ports.publisher.publish(&stored);
        if committed_task {
            // The commit a task's evidence has to name changed: what was verified at an earlier
            // commit is not verified any more.
            let settle = self.ledger.resettle();
            self.commit(settle)?;
        }
        Ok(())
    }

    /// Records a warning (deduplicated); never fails the caller.
    pub(crate) fn warn(&mut self, message: impl AsRef<str>) {
        let text = head_str(
            sanitize_text(message.as_ref().as_bytes()).trim(),
            WARNING_MAX,
        )
        .to_string();
        let now = self.now();
        if let Some((last, at)) = &self.last_warning {
            if *last == text && now.saturating_sub(*at) < WARNING_REPEAT_MS {
                return;
            }
        }
        self.last_warning = Some((text.clone(), now));
        let _ = self.commit(vec![DomainEvent::Warning { message: text }]);
    }

    /// Advances time-driven work: approval expiry, supervision, jobs, readiness, scheduling.
    pub fn tick(&mut self) {
        type Step = fn(&mut Orchestrator) -> Result<()>;
        let steps: [Step; 8] = [
            Orchestrator::expire_approvals,
            Orchestrator::supervise,
            Orchestrator::collect_jobs,
            Orchestrator::start_commands,
            Orchestrator::review_scan,
            Orchestrator::promote,
            Orchestrator::schedule,
            Orchestrator::messages,
        ];
        for step in steps {
            if let Err(e) = step(self) {
                self.warn(e.message);
            }
        }
    }

    fn expire_approvals(&mut self) -> Result<()> {
        let events = self.state.expire_approvals(self.now());
        self.commit(events)
    }

    /// Settles readiness and dependency blocking until nothing changes.
    pub(crate) fn promote(&mut self) -> Result<()> {
        for _ in 0..8 {
            let events = self.state.promote_all();
            if events.is_empty() {
                break;
            }
            self.commit(events)?;
        }
        Ok(())
    }
}

fn push_recent(recent: &mut VecDeque<Recent>, seq: u64, ts_ms: u64, event: &DomainEvent) {
    if matches!(event, DomainEvent::Warning { .. }) {
        return;
    }
    recent.push_back(Recent {
        seq,
        ts_ms,
        line: event.headline(),
    });
    while recent.len() > RECENT_KEPT {
        recent.pop_front();
    }
}
