//! Supervision of running workers: exits, timeouts, kill escalation, and what a finished session
//! means for its task.

use super::{Key, Orchestrator};
use fleet_domain::contract::ContractRead;
use fleet_domain::error::Result;
use fleet_domain::events::DomainEvent;
use fleet_domain::limits::EVENT_OUTPUT_TAIL;
use fleet_domain::run::WorkerEnd;
use fleet_domain::sanitize::tail_str;
use fleet_domain::status::{EndKind, TaskStatus};
use fleet_domain::supervision::{KillEscalation, KillStep};

use crate::ports::{SessionState, Signal, SignalOutcome};

impl Orchestrator {
    pub(crate) fn supervise(&mut self) -> Result<()> {
        let retry: Vec<Key> = self.pending_ends.keys().cloned().collect();
        for key in retry {
            if let Some(end) = self.pending_ends.remove(&key) {
                self.settle(&key, end);
            }
        }
        let keys: Vec<Key> = self.live.keys().cloned().collect();
        for key in keys {
            let Some(live) = self.live.get(&key) else {
                continue;
            };
            match self.ports.sessions.poll(live.session) {
                SessionState::Exited { code, signal } => {
                    let killed_for = live.kill.as_ref().map(|k| k.reason);
                    let (outcome, exit_code) = match (killed_for, signal) {
                        (Some(reason), _) => (reason, None),
                        (None, Some(_)) => (EndKind::Signaled, None),
                        (None, None) => (EndKind::Exited, code),
                    };
                    self.finish(&key, outcome, exit_code);
                }
                SessionState::Running => self.supervise_running(&key),
            }
        }
        Ok(())
    }

    fn supervise_running(&mut self, key: &Key) {
        let now = self.now();
        let grace = self.cfg.kill_grace_ms;
        let question_grace = self.cfg.question_grace_ms;
        let Some(live) = self.live.get(key) else {
            return;
        };
        let reading = self.ports.sessions.reading(live.session);
        let question_present =
            live.kill.is_none() && self.ports.contract.question_present(&live.worktree);
        let Some(live) = self.live.get_mut(key) else {
            return;
        };
        if live.kill.is_none() {
            if let Some(kind) =
                reading.and_then(|r| live.supervision.check(r.elapsed_ms, r.idle_ms).end_kind())
            {
                live.kill = Some(KillEscalation::new(kind, grace));
            } else if question_present {
                // The worker asked and is still running: give it a moment to exit by itself, then
                // stop it so the question can be answered.
                match live.question_seen_ms {
                    None => live.question_seen_ms = Some(now),
                    Some(t) if now.saturating_sub(t) >= question_grace => {
                        live.kill = Some(KillEscalation::new(EndKind::Exited, grace));
                    }
                    Some(_) => {}
                }
            }
        }
        let session = live.session;
        let step = live.kill.as_mut().map(|k| k.step(now, false));
        match step {
            Some(KillStep::SendTerm) => self.send_signal(key, session, Signal::Term),
            Some(KillStep::SendKill) => self.send_signal(key, session, Signal::Kill),
            Some(KillStep::GiveUp) => self.finish(key, EndKind::KillFailed, None),
            Some(KillStep::Wait) | Some(KillStep::Exited) | None => {}
        }
    }

    fn send_signal(&mut self, key: &Key, session: crate::ports::SessionId, signal: Signal) {
        if let SignalOutcome::Refused(why) = self.ports.sessions.signal(session, signal) {
            self.warn(format!("{}/{}: not signalled: {why}", key.0, key.1));
        }
    }

    /// Requests that a running task's worker be stopped. Returns false when it is not live.
    pub(crate) fn begin_stop(&mut self, key: &Key, reason: EndKind) -> bool {
        let grace = self.cfg.kill_grace_ms;
        match self.live.get_mut(key) {
            Some(live) => {
                if live.kill.is_none() {
                    live.kill = Some(KillEscalation::new(reason, grace));
                }
                true
            }
            None => false,
        }
    }

    /// Ends a session: releases it, saves its output, reads the worker's files and lets the
    /// domain decide what the end means.
    pub(crate) fn finish(&mut self, key: &Key, outcome: EndKind, exit_code: Option<i64>) {
        let Some(live) = self.live.remove(key) else {
            return;
        };
        let out = self.ports.sessions.release(live.session);
        if let Err(e) = self
            .ports
            .logs
            .save(&key.0, &key.1, live.attempt, &out.text)
        {
            self.warn(format!(
                "{}/{}: could not save output: {}",
                key.0, key.1, e.message
            ));
        }
        self.stash_worker_summary(key, &live.worktree);
        // a message written just before the worker exited is still a message
        self.read_outbox_of(key, &live.worktree, live.attempt);
        let end = self.worker_end(
            &live.worktree,
            live.attempt,
            outcome,
            exit_code,
            &out.text,
            out.stripped,
        );
        self.settle(key, end);
    }

    /// Reads the worker contract files and builds the end report.
    pub(crate) fn worker_end(
        &mut self,
        worktree: &str,
        attempt: u32,
        outcome: EndKind,
        exit_code: Option<i64>,
        output: &str,
        stripped: u64,
    ) -> WorkerEnd {
        let mut warning: Option<String> = None;
        let question = match self.ports.contract.read_question(worktree) {
            ContractRead::Valid(q) => Some(q),
            ContractRead::Absent => None,
            ContractRead::Invalid(why) => {
                warning = Some(format!("question.json ignored: {why}"));
                None
            }
        };
        let result = match self.ports.contract.read_result(worktree) {
            ContractRead::Valid(r) => Some(r),
            ContractRead::Absent => None,
            ContractRead::Invalid(why) => {
                let text = format!("result.json ignored: {why}");
                warning = Some(match warning {
                    Some(w) => format!("{w}; {text}"),
                    None => text,
                });
                None
            }
        };
        WorkerEnd {
            attempt,
            outcome,
            exit_code,
            output_tail: tail_str(output, EVENT_OUTPUT_TAIL).to_string(),
            stripped,
            result,
            question,
            contract_warning: warning,
        }
    }

    /// Applies a worker end to the task and continues with review when it succeeded.
    pub(crate) fn settle(&mut self, key: &Key, end: WorkerEnd) {
        let now = self.now();
        let events = match self.state.worker_ended(
            &key.0,
            &key.1,
            &end,
            &self.policy,
            now,
            self.ports.digest.as_ref(),
        ) {
            Ok(events) => events,
            Err(e) => {
                self.warn(format!(
                    "{}/{}: could not settle the session: {}",
                    key.0, key.1, e.message
                ));
                return;
            }
        };
        if let Err(e) = self.commit(events) {
            self.warn(format!(
                "{}/{}: could not record the session end: {}",
                key.0, key.1, e.message
            ));
            self.pending_ends.insert(key.clone(), end);
            return;
        }
        if self.task_status(key) == Some(TaskStatus::Review) {
            self.begin_review(key);
        }
    }

    pub(crate) fn task_status(&self, key: &Key) -> Option<TaskStatus> {
        self.state
            .run(&key.0)
            .ok()
            .and_then(|r| r.task(&key.1))
            .map(|t| t.status)
    }

    pub(crate) fn fail_task(&mut self, key: &Key, reason: String) {
        let ev = DomainEvent::TaskFailed {
            run: key.0.clone(),
            task: key.1.clone(),
            reason,
        };
        if let Err(e) = self.commit(vec![ev]) {
            self.warn(e.message);
        }
    }
}
