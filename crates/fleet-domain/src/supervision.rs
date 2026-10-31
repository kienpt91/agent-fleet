//! Rules for supervising worker processes: identity, timeouts, kill escalation, orphans and the
//! check that a control request does not come from a worker.
//!
//! Everything is a pure function of readings the caller passes in (elapsed time, a process start
//! token, an ancestry chain). Reading the OS and sending signals belong to the process adapter.

use crate::status::EndKind;

/// A process that is known by pid and by when it started. A pid alone is not an identity: the
/// number can be reused by an unrelated process after the original exits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    /// Opaque, stable for the life of one process (start time ticks or a start timestamp).
    pub start_token: String,
}

impl ProcessIdentity {
    pub fn new(pid: u32, start_token: impl Into<String>) -> Self {
        ProcessIdentity {
            pid,
            start_token: start_token.into(),
        }
    }

    /// True only when the process currently running under this pid started at the same time.
    /// An unreadable start token (`None`) is never a match.
    pub fn is_same_process(&self, current_start_token: Option<&str>) -> bool {
        self.pid > 1 && current_start_token.is_some_and(|t| !t.is_empty() && t == self.start_token)
    }
}

/// What to do with a process recorded by an earlier daemon run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrphanAction {
    /// Nothing is running under that pid.
    Gone,
    /// Same process: terminate its group.
    Terminate,
    /// The pid now belongs to a different process; do not signal it.
    PidReused,
}

pub fn orphan_action(recorded: &ProcessIdentity, running_token: Option<&str>) -> OrphanAction {
    match running_token {
        None => OrphanAction::Gone,
        Some(_) if recorded.is_same_process(running_token) => OrphanAction::Terminate,
        Some(_) => OrphanAction::PidReused,
    }
}

/// Wall-clock and silence limits of a worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct SupervisionPolicy {
    pub timeout_ms: Option<u64>,
    pub idle_timeout_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Continue,
    Timeout,
    Idle,
}

impl SupervisionPolicy {
    /// The total timeout wins when both limits are exceeded.
    pub fn check(&self, elapsed_ms: u64, idle_ms: u64) -> Verdict {
        if self.timeout_ms.is_some_and(|t| elapsed_ms >= t) {
            Verdict::Timeout
        } else if self.idle_timeout_ms.is_some_and(|t| idle_ms >= t) {
            Verdict::Idle
        } else {
            Verdict::Continue
        }
    }
}

impl Verdict {
    pub fn end_kind(self) -> Option<EndKind> {
        match self {
            Verdict::Continue => None,
            Verdict::Timeout => Some(EndKind::Timeout),
            Verdict::Idle => Some(EndKind::IdleTimeout),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KillStep {
    /// Send SIGTERM to the process group now.
    SendTerm,
    /// The group ignored SIGTERM for the grace period: send SIGKILL.
    SendKill,
    /// Signals were sent; keep waiting.
    Wait,
    /// The process is gone.
    Exited,
    /// SIGKILL was sent and the process is still there after the grace period.
    GiveUp,
}

/// TERM, then KILL after a grace period, then give up. Driven by repeated [`KillEscalation::step`]
/// calls with the current time and whether the process has exited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KillEscalation {
    pub reason: EndKind,
    grace_ms: u64,
    term_at: Option<u64>,
    kill_at: Option<u64>,
}

impl KillEscalation {
    pub fn new(reason: EndKind, grace_ms: u64) -> Self {
        KillEscalation {
            reason,
            grace_ms,
            term_at: None,
            kill_at: None,
        }
    }

    pub fn step(&mut self, now_ms: u64, exited: bool) -> KillStep {
        if exited {
            return KillStep::Exited;
        }
        match (self.term_at, self.kill_at) {
            (None, _) => {
                self.term_at = Some(now_ms);
                KillStep::SendTerm
            }
            (Some(t), None) => {
                if now_ms.saturating_sub(t) >= self.grace_ms {
                    self.kill_at = Some(now_ms);
                    KillStep::SendKill
                } else {
                    KillStep::Wait
                }
            }
            (Some(_), Some(k)) => {
                if now_ms.saturating_sub(k) >= self.grace_ms.max(1000) {
                    KillStep::GiveUp
                } else {
                    KillStep::Wait
                }
            }
        }
    }

    pub fn term_sent(&self) -> bool {
        self.term_at.is_some()
    }

    pub fn kill_sent(&self) -> bool {
        self.kill_at.is_some()
    }
}

/// One process in an ancestry chain read from the OS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
}

/// A process and its ancestors as read from the OS, nearest first, and whether the walk is
/// *complete*: it ended at the root of the process tree (init, or a parent outside this pid
/// namespace), not because of a cap on its length or because an ancestor could not be read (it
/// exited while being read, or is hidden from this user). Only a complete lineage can say that
/// the daemon is *not* among a process's ancestors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lineage {
    pub chain: Vec<ProcInfo>,
    pub complete: bool,
}

/// A worker session that is currently alive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveSession {
    pub pid: u32,
    pub pgid: u32,
}

/// True when the connecting process is a worker or a descendant of one: it, or any of its
/// ancestors, is a live worker's session leader or sits in a live worker's process group. Workers
/// run under the same user as the operator, so the socket's file mode cannot keep them out; this
/// check refuses the obvious path (a worker process or its children connecting to the socket).
/// A process that detaches completely from the worker's tree before connecting is not caught.
pub fn is_worker_origin(chain: &[ProcInfo], live: &[LiveSession]) -> bool {
    chain.iter().any(|p| {
        live.iter()
            .any(|s| s.pid == p.pid || (s.pgid != 0 && s.pgid == p.pgid))
    })
}

/// True when the connecting process was started, directly or through other processes, by the
/// daemon itself: one of its ancestors (not the process itself) is the daemon. The daemon starts
/// workers, quality gates, approved commands, git and the undo tool, and all of them run code or
/// read files an agent wrote, so none of them may talk to the daemon's interfaces. The operator's
/// own commands are never started by the daemon. A process that leaves the tree (a double fork
/// that is adopted by init, `setsid`) is not caught; callers combine this with
/// [`is_worker_origin`] over the process groups of everything the daemon has running.
///
/// A daemon that is pid 1 of its pid namespace (a container entrypoint) works the same way: init
/// is the last element of every lineage, so everything started inside the container, and every
/// process orphaned inside it, counts as started by the daemon. A process that entered the
/// namespace from outside (`docker exec`) has a parent outside it and does not.
pub fn is_started_by_daemon(chain: &[ProcInfo], daemon_pid: u32) -> bool {
    daemon_pid > 0 && chain.iter().skip(1).any(|p| p.pid == daemon_pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(pid: u32, ppid: u32, pgid: u32) -> ProcInfo {
        ProcInfo { pid, ppid, pgid }
    }

    #[test]
    fn a_process_started_by_the_daemon_is_recognized_by_its_ancestry_only() {
        // daemon 100, gate script 200, the gate's child 300; the operator's shell 50 and fleet 60
        let gate_child = [info(300, 200, 200), info(200, 100, 200), info(100, 50, 50)];
        assert!(is_started_by_daemon(&gate_child, 100));
        assert!(
            is_started_by_daemon(&gate_child[1..], 100),
            "the job leader itself"
        );
        let operator = [info(60, 50, 60), info(50, 1, 50)];
        assert!(!is_started_by_daemon(&operator, 100));
        // the daemon process itself (an in-process client, a test) is not "started by" itself
        assert!(!is_started_by_daemon(&[info(100, 50, 50)], 100));
        assert!(!is_started_by_daemon(&[], 100));
        // pid 0 never stands for the daemon
        assert!(!is_started_by_daemon(&gate_child, 0));
        // a daemon that is init of its namespace: everything below it is its own, a process that
        // came in from outside (its parent is not in the namespace) is not
        let inside = [info(5, 1, 5), info(1, 0, 1)];
        assert!(is_started_by_daemon(&inside, 1));
        assert!(!is_started_by_daemon(&[info(9, 0, 9)], 1));
        assert!(!is_started_by_daemon(&[info(1, 0, 1)], 1), "itself");
    }

    #[test]
    fn identity_requires_a_known_matching_start_token() {
        let id = ProcessIdentity::new(100, "5555");
        assert!(id.is_same_process(Some("5555")));
        assert!(!id.is_same_process(Some("5556")));
        assert!(!id.is_same_process(None));
        assert!(!id.is_same_process(Some("")));
        assert!(!ProcessIdentity::new(100, "").is_same_process(Some("")));
    }

    #[test]
    fn pid_zero_and_one_are_never_signalled() {
        assert!(!ProcessIdentity::new(0, "1").is_same_process(Some("1")));
        assert!(!ProcessIdentity::new(1, "1").is_same_process(Some("1")));
    }

    #[test]
    fn orphan_action_by_start_token() {
        let id = ProcessIdentity::new(200, "10");
        assert_eq!(orphan_action(&id, None), OrphanAction::Gone);
        assert_eq!(orphan_action(&id, Some("10")), OrphanAction::Terminate);
        assert_eq!(orphan_action(&id, Some("11")), OrphanAction::PidReused);
    }

    #[test]
    fn timeouts() {
        let p = SupervisionPolicy {
            timeout_ms: Some(1000),
            idle_timeout_ms: Some(300),
        };
        assert_eq!(p.check(10, 10), Verdict::Continue);
        assert_eq!(p.check(500, 300), Verdict::Idle);
        assert_eq!(p.check(1000, 0), Verdict::Timeout);
        assert_eq!(p.check(1000, 900), Verdict::Timeout);
        assert_eq!(
            SupervisionPolicy::default().check(u64::MAX, u64::MAX),
            Verdict::Continue
        );
        assert_eq!(Verdict::Idle.end_kind(), Some(EndKind::IdleTimeout));
        assert_eq!(Verdict::Timeout.end_kind(), Some(EndKind::Timeout));
        assert_eq!(Verdict::Continue.end_kind(), None);
    }

    #[test]
    fn escalation_goes_term_then_kill_then_gives_up() {
        let mut k = KillEscalation::new(EndKind::Timeout, 5000);
        assert_eq!(k.step(0, false), KillStep::SendTerm);
        assert!(k.term_sent() && !k.kill_sent());
        assert_eq!(k.step(100, false), KillStep::Wait);
        assert_eq!(k.step(4999, false), KillStep::Wait);
        assert_eq!(k.step(5000, false), KillStep::SendKill);
        assert!(k.kill_sent());
        assert_eq!(k.step(5500, false), KillStep::Wait);
        assert_eq!(k.step(10_000, false), KillStep::GiveUp);
    }

    #[test]
    fn escalation_stops_as_soon_as_the_process_is_gone() {
        let mut k = KillEscalation::new(EndKind::Stopped, 5000);
        assert_eq!(k.step(0, false), KillStep::SendTerm);
        assert_eq!(k.step(10, true), KillStep::Exited);
        let mut fresh = KillEscalation::new(EndKind::Stopped, 5000);
        assert_eq!(fresh.step(0, true), KillStep::Exited);
        assert!(!fresh.term_sent());
    }

    #[test]
    fn zero_grace_still_sends_term_before_kill() {
        let mut k = KillEscalation::new(EndKind::Stopped, 0);
        assert_eq!(k.step(0, false), KillStep::SendTerm);
        assert_eq!(k.step(0, false), KillStep::SendKill);
        assert_eq!(k.step(0, false), KillStep::Wait);
        assert_eq!(k.step(1000, false), KillStep::GiveUp);
    }

    #[test]
    fn worker_origin_by_ancestry_or_process_group() {
        let live = [LiveSession {
            pid: 500,
            pgid: 500,
        }];
        let outside = [
            ProcInfo {
                pid: 900,
                ppid: 800,
                pgid: 900,
            },
            ProcInfo {
                pid: 800,
                ppid: 1,
                pgid: 800,
            },
        ];
        assert!(!is_worker_origin(&outside, &live));
        let child = [ProcInfo {
            pid: 901,
            ppid: 500,
            pgid: 500,
        }];
        assert!(is_worker_origin(&child, &live));
        let grandchild = [
            ProcInfo {
                pid: 950,
                ppid: 901,
                pgid: 950,
            },
            ProcInfo {
                pid: 901,
                ppid: 500,
                pgid: 901,
            },
            ProcInfo {
                pid: 500,
                ppid: 1,
                pgid: 500,
            },
        ];
        assert!(is_worker_origin(&grandchild, &live));
        assert!(!is_worker_origin(&[], &live));
        assert!(!is_worker_origin(&child, &[]));
    }
}
