//! Crash recovery: what to do with sessions an earlier daemon run left behind.

use super::{Key, Orchestrator};
use fleet_domain::error::Result;
use fleet_domain::events::DomainEvent;
use fleet_domain::fleet::INTERRUPTED_COMMAND;
use fleet_domain::status::{EndKind, TaskStatus};
use fleet_domain::supervision::{orphan_action, OrphanAction, ProcessIdentity};

use crate::ports::SignalOutcome;

impl Orchestrator {
    /// Records that a daemon session began and reconciles tasks that were running when the
    /// previous one ended. A worker cannot be re-attached after a restart (its terminal is
    /// gone), so a surviving process is stopped, after checking it is still the process that was
    /// started, and the task is queued for another attempt (or reviewed when it had finished).
    pub fn startup(&mut self, pid: u64, version: &str) -> Result<()> {
        self.pid = pid;
        self.version = version.to_string();
        self.commit(vec![DomainEvent::DaemonStarted {
            pid,
            version: version.to_string(),
        }])?;
        self.reconcile_orphans();
        self.reconcile_commands();
        self.reconcile_handoffs();
        // Whatever the replayed history leaves unsettled in the goal ledger (a verification that
        // no longer matches the commit of its task) is settled before anything runs.
        let settle = self.ledger.resettle();
        if let Err(e) = self.commit(settle) {
            self.warn(e.message);
        }
        Ok(())
    }

    /// An approved command that was started but never reported back was cut off by the previous
    /// daemon run. It is recorded as unfinished with an unknown outcome and is never started
    /// again (its approval was consumed when it started); the task that waited for it goes back
    /// in line with a note saying so.
    fn reconcile_commands(&mut self) {
        let ids: Vec<String> = self
            .state
            .interrupted_commands()
            .iter()
            .map(|a| a.id.to_string())
            .collect();
        let events = self.state.abandon_interrupted();
        if let Err(e) = self.commit(events) {
            self.warn(e.message);
            return;
        }
        for id in ids {
            self.warn(format!("approval {id}: {INTERRUPTED_COMMAND}"));
        }
    }

    fn reconcile_orphans(&mut self) {
        struct Orphan {
            key: Key,
            attempt: u32,
            pid: u64,
            token: String,
            worktree: String,
        }
        let orphans: Vec<Orphan> = self
            .state
            .runs()
            .flat_map(|r| {
                r.tasks()
                    .iter()
                    .filter(|t| t.status == TaskStatus::Running)
                    .map(|t| Orphan {
                        key: (r.id.clone(), t.id.clone()),
                        attempt: t.attempt,
                        pid: t.session.as_ref().map_or(0, |s| s.pid),
                        token: t
                            .session
                            .as_ref()
                            .map(|s| s.start_token.clone())
                            .unwrap_or_default(),
                        worktree: t.worktree.clone().unwrap_or_default(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        for o in orphans {
            let pid = u32::try_from(o.pid).unwrap_or(0);
            let identity = ProcessIdentity::new(pid, o.token.clone());
            let running = if pid > 1 {
                self.ports.sessions.start_token(pid)
            } else {
                None
            };
            let action = match orphan_action(&identity, running.as_deref()) {
                OrphanAction::Gone => "its process was already gone".to_string(),
                OrphanAction::PidReused => {
                    self.warn(format!(
                        "{}/{}: pid {pid} now belongs to another process; it was not signalled",
                        o.key.0, o.key.1
                    ));
                    "its pid was reused by another process; nothing was signalled".to_string()
                }
                OrphanAction::Terminate => match self
                    .ports
                    .sessions
                    .terminate_orphan(&identity, self.cfg.kill_grace_ms)
                {
                    SignalOutcome::Sent => "its process group was terminated".to_string(),
                    SignalOutcome::Gone => "its process was already gone".to_string(),
                    SignalOutcome::Refused(why) => format!("not signalled: {why}"),
                },
            };
            let event = DomainEvent::OrphanReconciled {
                run: o.key.0.clone(),
                task: o.key.1.clone(),
                action,
            };
            if let Err(e) = self.commit(vec![event]) {
                self.warn(e.message);
                continue;
            }
            // The daemon found this task running after a crash or restart: record a handoff from
            // fleet's own state (plus whatever summary the worker had written) before the task is
            // queued again, so the next attempt is briefed from it.
            self.stash_worker_summary(&o.key, &o.worktree);
            self.auto_handoff(&o.key, neo_context::handoff::HandoffTrigger::Recovery);
            let end = self.worker_end(&o.worktree, o.attempt, EndKind::Orphaned, None, "", 0);
            self.settle(&o.key, end);
        }
    }

    /// Graceful shutdown: stop dispatching and ask every worker to stop. The tasks go back in line
    /// without using a retry, exactly like after a crash. The caller keeps ticking until
    /// [`Orchestrator::live_count`] reaches zero.
    pub fn begin_shutdown(&mut self) {
        self.shutting_down = true;
        let keys: Vec<Key> = self.live.keys().cloned().collect();
        for key in keys {
            self.begin_stop(&key, EndKind::Orphaned);
        }
    }
}
