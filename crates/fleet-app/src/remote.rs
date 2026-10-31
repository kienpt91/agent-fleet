//! Remote worktrees: the `RemoteHost` port and the session host that uses it.
//!
//! A task that names a host runs its worker on that host. Only the *process* is remote: the task's
//! worktree, its branch, the quality gates, verification and merging all stay on this machine.
//! The protocol, in the order the orchestrator drives it:
//!
//! 1. **Prepare.** The commit checked out in the local task worktree (plus the brief) is shipped to
//!    the host, which gets a worktree below its configured base directory.
//! 2. **Launch.** The worker runs on the host; the adapter hands back the *local* command
//!    (`ssh -tt ...`) which the ordinary pty session host runs, so the sanitizer, the output ring
//!    buffer, redaction, the output bounds and the process-group registration apply unchanged.
//! 3. **Collect.** When the local command ended, whatever the worker left is committed on the host
//!    and brought back as a git bundle into a quarantine ref of the local repository, verified there
//!    (ancestry, paths, sizes) and only then turned into the task branch. The worker's contract
//!    files come back through bounded reads. Nothing the host reports is believed: the commit and
//!    tree that evidence and merges use are the ones read from the local repository afterwards.
//! 4. **Clean up.** The remote worktree is removed (best effort, reported).
//!
//! [`RemoteSessions`] is the [`SessionHost`] that runs this protocol around an inner session host;
//! the adapter that talks to a real machine (`fleet-infra`, OpenSSH) implements [`RemoteHost`].

use crate::hosts::HostDef;
use crate::ports::{
    SessionHost, SessionId, SessionOutput, SessionReading, SessionSpec, SessionState, Signal,
    SignalOutcome, Spawned,
};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::{show, HostName, RunId, Sha, TaskId};
use fleet_domain::output::Chunk;
use fleet_domain::supervision::{LiveSession, ProcessIdentity};
use std::collections::BTreeMap;
use std::sync::Arc;

/// The exit code a session reports when the worker ended well but its work could not be brought
/// back: the attempt must not count as a success.
pub const EXIT_COLLECT_FAILED: i64 = 125;

/// One task on one host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteTask {
    pub host: HostName,
    pub run: RunId,
    pub task: TaskId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrepareRequest {
    pub task: RemoteTask,
    pub attempt: u32,
    /// The local task worktree whose checked-out commit and brief are sent.
    pub worktree: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prepared {
    /// The worktree on the host.
    pub dir: String,
    /// The commit that was sent; collecting verifies the returned work descends from it.
    pub sent: Sha,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchRequest {
    pub task: RemoteTask,
    /// The worker command and its arguments, as they run on the host.
    pub program: String,
    pub args: Vec<String>,
    /// The complete environment of the remote worker.
    pub env: Vec<(String, String)>,
}

/// A command for this machine's own session host: the ssh client with its fixed arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalCommand {
    pub program: String,
    pub args: Vec<String>,
    /// The complete environment of the local process.
    pub env: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CollectRequest {
    pub task: RemoteTask,
    pub worktree: String,
    pub sent: Sha,
    pub attempt: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CollectId(pub u64);

/// What collecting brought back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collected {
    /// The commit the local task branch points at now when the worker changed anything.
    pub head: Option<Sha>,
    /// What happened, for the attempt's output: fleet's own sentences.
    pub notes: Vec<String>,
}

/// What the application layer needs from a remote host. Every method may be called from the
/// orchestrator's thread and must return quickly or be bounded; collecting and signalling are
/// asynchronous by design.
pub trait RemoteHost: Send + Sync {
    /// The worktree directory of a task on its host.
    fn remote_dir(&self, task: &RemoteTask) -> Result<String>;
    /// Ships the commit and the brief and creates the remote worktree. Replaces a stale remote
    /// worktree of the same task. Bounded by timeouts.
    fn prepare(&self, req: &PrepareRequest) -> Result<Prepared>;
    /// The local command that runs the worker on the host (in a terminal).
    fn launch_command(&self, req: &LaunchRequest) -> Result<LocalCommand>;
    /// Starts bringing the work back; the answer comes from [`RemoteHost::poll_collect`]. The remote
    /// worktree is removed at the end whatever the outcome.
    fn begin_collect(&self, req: &CollectRequest) -> Result<CollectId>;
    /// `None` while collecting runs.
    fn poll_collect(&self, id: CollectId) -> Option<Result<Collected>>;
    /// Asks the host to signal the worker's process group. Never blocks; best effort.
    fn signal(&self, task: &RemoteTask, signal: Signal);
    /// Gives up on a task: stops the worker and removes the remote worktree, in the background.
    fn abandon(&self, task: &RemoteTask);
    /// Cleans up what an earlier daemon run left on hosts. One line per finding.
    fn reconcile(&self) -> Vec<String>;
}

enum Phase {
    Running,
    Collecting {
        id: CollectId,
        exit: SessionState,
    },
    Done {
        exit: SessionState,
        notes: Vec<String>,
    },
}

struct Remote {
    task: RemoteTask,
    worktree: String,
    attempt: u32,
    sent: Sha,
    phase: Phase,
}

/// The session host for tasks that name a host: local sessions run through the inner host as
/// usual; a remote one is prepared, launched as an `ssh` command in a local pty, and collected
/// when that command ends. The session is reported as exited only after collecting finished, so
/// the orchestrator reads the worker's files and the branch after they were brought back.
pub struct RemoteSessions {
    inner: Box<dyn SessionHost>,
    remote: Arc<dyn RemoteHost>,
    hosts: BTreeMap<HostName, HostDef>,
    active: BTreeMap<SessionId, Remote>,
}

impl RemoteSessions {
    pub fn new(
        inner: Box<dyn SessionHost>,
        remote: Arc<dyn RemoteHost>,
        hosts: BTreeMap<HostName, HostDef>,
    ) -> RemoteSessions {
        RemoteSessions {
            inner,
            remote,
            hosts,
            active: BTreeMap::new(),
        }
    }

    fn sessions_on(&self, host: &HostName) -> usize {
        self.active
            .values()
            .filter(|r| r.task.host == *host)
            .count()
    }
}

fn collect_failed(exit: &SessionState) -> SessionState {
    match exit {
        SessionState::Exited { code: Some(0), .. } => SessionState::Exited {
            code: Some(EXIT_COLLECT_FAILED),
            signal: None,
        },
        other => other.clone(),
    }
}

impl SessionHost for RemoteSessions {
    fn spawn(&mut self, spec: &SessionSpec) -> Result<Spawned> {
        let Some(host) = &spec.host else {
            return self.inner.spawn(spec);
        };
        let def = self.hosts.get(host).ok_or_else(|| {
            FleetError::not_found(format!("host {} is not configured", show(host.as_str())))
        })?;
        if !def.allows_worker(&spec.program) {
            return Err(FleetError::forbidden(format!(
                "worker command {} is not allowed on host {host}",
                show(&spec.program)
            )));
        }
        if self.sessions_on(host) >= def.max_sessions {
            return Err(FleetError::limit(format!(
                "host {host} already runs {} sessions",
                def.max_sessions
            )));
        }
        let task = RemoteTask {
            host: host.clone(),
            run: spec.run.clone(),
            task: spec.task.clone(),
        };
        let prepared = self.remote.prepare(&PrepareRequest {
            task: task.clone(),
            attempt: spec.attempt,
            worktree: spec.cwd.clone(),
        })?;
        let command = match self.remote.launch_command(&LaunchRequest {
            task: task.clone(),
            program: spec.program.clone(),
            args: spec.args.clone(),
            env: spec.env.clone(),
        }) {
            Ok(c) => c,
            Err(e) => {
                self.remote.abandon(&task);
                return Err(e);
            }
        };
        let local = SessionSpec {
            run: spec.run.clone(),
            task: spec.task.clone(),
            attempt: spec.attempt,
            program: command.program,
            args: command.args,
            cwd: spec.cwd.clone(),
            env: command.env,
            host: None,
        };
        match self.inner.spawn(&local) {
            Ok(spawned) => {
                self.active.insert(
                    spawned.id,
                    Remote {
                        task,
                        worktree: spec.cwd.clone(),
                        attempt: spec.attempt,
                        sent: prepared.sent,
                        phase: Phase::Running,
                    },
                );
                Ok(spawned)
            }
            Err(e) => {
                self.remote.abandon(&task);
                Err(e)
            }
        }
    }

    fn poll(&mut self, id: SessionId) -> SessionState {
        enum Step {
            Running(CollectRequest),
            Collecting(CollectId, SessionState),
        }
        let Some(remote) = self.active.get(&id) else {
            return self.inner.poll(id);
        };
        let step = match &remote.phase {
            Phase::Running => Step::Running(CollectRequest {
                task: remote.task.clone(),
                worktree: remote.worktree.clone(),
                sent: remote.sent.clone(),
                attempt: remote.attempt,
            }),
            Phase::Collecting { id: cid, exit } => Step::Collecting(*cid, exit.clone()),
            Phase::Done { exit, .. } => return exit.clone(),
        };
        let next = match step {
            Step::Running(request) => {
                let state = self.inner.poll(id);
                if state == SessionState::Running {
                    return state;
                }
                match self.remote.begin_collect(&request) {
                    Ok(cid) => Phase::Collecting {
                        id: cid,
                        exit: state,
                    },
                    Err(e) => Phase::Done {
                        exit: collect_failed(&state),
                        notes: vec![format!(
                            "fleet: the remote work could not be collected: {}",
                            e.message
                        )],
                    },
                }
            }
            Step::Collecting(cid, exit) => match self.remote.poll_collect(cid) {
                None => return SessionState::Running,
                Some(Ok(c)) => Phase::Done {
                    exit,
                    notes: c.notes,
                },
                Some(Err(e)) => Phase::Done {
                    exit: collect_failed(&exit),
                    notes: vec![format!(
                        "fleet: the remote work could not be brought back: {}",
                        e.message
                    )],
                },
            },
        };
        if let Some(r) = self.active.get_mut(&id) {
            r.phase = next;
        }
        // One more look: collecting may have been over at once (and a session still running
        // answers Running here without touching the host again).
        match self.active.get(&id).map(|r| &r.phase) {
            Some(Phase::Done { exit, .. }) => exit.clone(),
            _ => SessionState::Running,
        }
    }

    fn reading(&self, id: SessionId) -> Option<SessionReading> {
        let reading = self.inner.reading(id)?;
        match self.active.get(&id).map(|r| &r.phase) {
            // Collecting is fleet's own work: the worker is not silent while it runs.
            Some(Phase::Collecting { .. }) => Some(SessionReading {
                elapsed_ms: reading.elapsed_ms,
                idle_ms: 0,
            }),
            _ => Some(reading),
        }
    }

    fn winding_down(&self, id: SessionId) -> bool {
        matches!(
            self.active.get(&id).map(|r| &r.phase),
            Some(Phase::Collecting { .. })
        )
    }

    fn signal(&mut self, id: SessionId, signal: Signal) -> SignalOutcome {
        match self.active.get(&id) {
            None => self.inner.signal(id, signal),
            Some(remote) => match remote.phase {
                Phase::Running => {
                    // The worker on the host first (it may ignore the hang-up that ending the
                    // ssh client causes), then the local client.
                    self.remote.signal(&remote.task, signal);
                    self.inner.signal(id, signal)
                }
                Phase::Collecting { .. } | Phase::Done { .. } => SignalOutcome::Gone,
            },
        }
    }

    fn snapshot(&self, id: SessionId, max: usize) -> Option<String> {
        self.inner.snapshot(id, max)
    }

    fn read_since(&self, id: SessionId, offset: u64, max: usize) -> Option<Chunk> {
        self.inner.read_since(id, offset, max)
    }

    fn release(&mut self, id: SessionId) -> SessionOutput {
        let mut out = self.inner.release(id);
        if let Some(remote) = self.active.remove(&id) {
            let notes = match remote.phase {
                Phase::Done { notes, .. } => notes,
                Phase::Running | Phase::Collecting { .. } => {
                    self.remote.abandon(&remote.task);
                    vec![
                        "fleet: the remote worker was abandoned before its work was collected; the remote worktree is removed in the background"
                            .to_string(),
                    ]
                }
            };
            for note in notes {
                if !out.text.is_empty() && !out.text.ends_with('\n') {
                    out.text.push('\n');
                }
                out.text.push_str(&note);
                out.text.push('\n');
            }
        }
        out
    }

    fn live(&self) -> Vec<LiveSession> {
        self.inner.live()
    }

    fn start_token(&self, pid: u32) -> Option<String> {
        self.inner.start_token(pid)
    }

    fn terminate_orphan(&mut self, identity: &ProcessIdentity, grace_ms: u64) -> SignalOutcome {
        self.inner.terminate_orphan(identity, grace_ms)
    }

    fn remote_dir(&self, host: &HostName, run: &RunId, task: &TaskId) -> Result<String> {
        self.remote.remote_dir(&RemoteTask {
            host: host.clone(),
            run: run.clone(),
            task: task.clone(),
        })
    }

    fn reconcile_remote(&mut self) -> Vec<String> {
        self.remote.reconcile()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::remote::FakeRemote;
    use crate::testing::World;
    use fleet_domain::error::ErrorCode;

    fn host(name: &str, sessions: usize) -> HostDef {
        let mut h = HostDef::new(
            HostName::parse(name).unwrap(),
            "h.example",
            "u",
            "/k",
            "/srv/f",
        );
        h.allowed_workers = vec!["agent".to_string()];
        h.max_sessions = sessions;
        h
    }

    struct Rig {
        sessions: RemoteSessions,
        world: World,
        remote: FakeRemote,
    }

    fn rig(max: usize) -> Rig {
        let world = World::new();
        let remote = FakeRemote::new();
        let hosts = std::iter::once(("build", host("build", max)))
            .map(|(n, h)| (HostName::parse(n).unwrap(), h))
            .collect();
        let sessions = RemoteSessions::new(world.ports().sessions, Arc::new(remote.clone()), hosts);
        Rig {
            sessions,
            world,
            remote,
        }
    }

    fn spec(task: &str, program: &str, host: Option<&str>) -> SessionSpec {
        SessionSpec {
            run: RunId::parse("r1").unwrap(),
            task: TaskId::parse(task).unwrap(),
            attempt: 1,
            program: program.to_string(),
            args: vec!["--go".to_string()],
            cwd: "/wt".to_string(),
            env: vec![("A".to_string(), "b".to_string())],
            host: host.map(|h| HostName::parse(h).unwrap()),
        }
    }

    #[test]
    fn a_session_without_a_host_is_the_inner_session_untouched() {
        let mut r = rig(1);
        let s = r.sessions.spawn(&spec("a", "run-agent", None)).unwrap();
        assert_eq!(r.sessions.poll(s.id), SessionState::Running);
        r.world.exit(s.id, 0);
        assert!(matches!(
            r.sessions.poll(s.id),
            SessionState::Exited { code: Some(0), .. }
        ));
        assert!(r.remote.prepared().is_empty() && r.remote.collects().is_empty());
        assert_eq!(r.sessions.signal(s.id, Signal::Term), SignalOutcome::Gone);
        let out = r.sessions.release(s.id);
        assert_eq!(out.text, "");
        assert!(r.remote.abandoned().is_empty());
    }

    #[test]
    fn an_unknown_host_a_worker_that_is_not_allowed_and_a_full_host_are_refused_before_the_host_is_touched(
    ) {
        let mut r = rig(1);
        let err = r
            .sessions
            .spawn(&spec("a", "agent", Some("nowhere")))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        let err = r
            .sessions
            .spawn(&spec("a", "rm", Some("build")))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        assert!(r.remote.prepared().is_empty());
        let first = r
            .sessions
            .spawn(&spec("a", "agent", Some("build")))
            .unwrap();
        let err = r
            .sessions
            .spawn(&spec("b", "agent", Some("build")))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Limit);
        assert_eq!(r.remote.prepared().len(), 1);
        // The slot is free again once the first session was released.
        r.world.exit(first.id, 0);
        while r.sessions.poll(first.id) == SessionState::Running {}
        r.sessions.release(first.id);
        assert!(r.sessions.spawn(&spec("b", "agent", Some("build"))).is_ok());
    }

    #[test]
    fn a_launch_or_spawn_that_fails_removes_the_remote_worktree_it_made() {
        let mut r = rig(2);
        r.remote.fail_launch(Some("no command"));
        assert!(r
            .sessions
            .spawn(&spec("a", "agent", Some("build")))
            .is_err());
        assert_eq!(r.remote.abandoned().len(), 1);
        r.remote.fail_launch(None);
        r.world.fail_spawn(Some("no pty"));
        assert!(r
            .sessions
            .spawn(&spec("a", "agent", Some("build")))
            .is_err());
        assert_eq!(r.remote.abandoned().len(), 2);
        r.world.fail_spawn(None);
        r.remote.fail_prepare(Some("down"));
        assert!(r
            .sessions
            .spawn(&spec("a", "agent", Some("build")))
            .is_err());
        assert_eq!(
            r.remote.abandoned().len(),
            2,
            "nothing was made, nothing to remove"
        );
    }

    #[test]
    fn the_remote_launch_gets_the_program_arguments_and_environment_of_the_spec() {
        let mut r = rig(1);
        r.sessions
            .spawn(&spec("a", "agent", Some("build")))
            .unwrap();
        let l = &r.remote.launched()[0];
        assert_eq!(
            (l.program.as_str(), l.args.clone()),
            ("agent", vec!["--go".to_string()])
        );
        assert_eq!(l.env, vec![("A".to_string(), "b".to_string())]);
        assert_eq!(
            r.sessions
                .remote_dir(
                    &HostName::parse("build").unwrap(),
                    &RunId::parse("r1").unwrap(),
                    &TaskId::parse("a").unwrap()
                )
                .unwrap(),
            "/remote/build/r1/a"
        );
        let s = r.world.session_for("a").unwrap();
        assert_eq!(r.world.session_spec(s).unwrap().program, "/usr/bin/ssh");
    }

    #[test]
    fn the_end_waits_for_collecting_and_a_failure_to_collect_turns_a_clean_exit_into_a_failure() {
        let mut r = rig(1);
        let s = r
            .sessions
            .spawn(&spec("a", "agent", Some("build")))
            .unwrap();
        r.remote.polls_before_done(1);
        r.remote.collect_answer(Err("refused".to_string()));
        r.world.exit(s.id, 0);
        assert_eq!(
            r.sessions.poll(s.id),
            SessionState::Running,
            "collecting started"
        );
        assert_eq!(r.sessions.reading(s.id).unwrap().idle_ms, 0);
        assert_eq!(
            r.sessions.signal(s.id, Signal::Kill),
            SignalOutcome::Gone,
            "nothing left to signal"
        );
        assert_eq!(
            r.sessions.poll(s.id),
            SessionState::Running,
            "still collecting"
        );
        assert_eq!(
            r.sessions.poll(s.id),
            SessionState::Exited {
                code: Some(EXIT_COLLECT_FAILED),
                signal: None
            }
        );
        assert_eq!(
            r.sessions.poll(s.id),
            SessionState::Exited {
                code: Some(EXIT_COLLECT_FAILED),
                signal: None
            }
        );
        let out = r.sessions.release(s.id);
        assert!(
            out.text.contains("could not be brought back: refused"),
            "{}",
            out.text
        );
        assert!(
            r.remote.abandoned().is_empty(),
            "collecting cleans up by itself"
        );
    }

    #[test]
    fn exits_that_were_already_failures_keep_their_code_and_signal() {
        for (code, signal) in [(3, None), (0, Some("SIGKILL"))] {
            let mut r = rig(1);
            r.remote.collect_answer(Err("x".to_string()));
            let s = r
                .sessions
                .spawn(&spec("a", "agent", Some("build")))
                .unwrap();
            match signal {
                Some(name) => r.world.exit_by_signal(s.id, name),
                None => r.world.exit(s.id, code),
            }
            let state = loop {
                let st = r.sessions.poll(s.id);
                if st != SessionState::Running {
                    break st;
                }
            };
            match signal {
                Some(name) => assert_eq!(
                    state,
                    SessionState::Exited {
                        code: None,
                        signal: Some(name.to_string())
                    }
                ),
                None => assert_eq!(
                    state,
                    SessionState::Exited {
                        code: Some(3),
                        signal: None
                    }
                ),
            }
        }
    }

    #[test]
    fn a_collect_that_cannot_even_start_is_reported_and_the_exit_is_a_failure() {
        let mut r = rig(1);
        let s = r
            .sessions
            .spawn(&spec("a", "agent", Some("build")))
            .unwrap();
        r.remote.fail_begin_collect(Some("too many operations"));
        r.world.exit(s.id, 0);
        assert!(matches!(
            r.sessions.poll(s.id),
            SessionState::Exited {
                code: Some(EXIT_COLLECT_FAILED),
                ..
            }
        ));
        assert!(r
            .sessions
            .release(s.id)
            .text
            .contains("could not be collected: too many operations"));
    }

    #[test]
    fn a_stop_reaches_the_host_first_and_then_the_local_client() {
        let mut r = rig(1);
        let s = r
            .sessions
            .spawn(&spec("a", "agent", Some("build")))
            .unwrap();
        assert_eq!(r.sessions.signal(s.id, Signal::Term), SignalOutcome::Sent);
        assert_eq!(r.remote.signals().len(), 1);
        assert_eq!(r.remote.signals()[0].1, Signal::Term);
        assert_eq!(r.world.signals(s.id), vec![Signal::Term]);
    }

    #[test]
    fn releasing_a_session_whose_work_was_never_collected_abandons_the_remote_worktree() {
        let mut r = rig(1);
        let s = r
            .sessions
            .spawn(&spec("a", "agent", Some("build")))
            .unwrap();
        let out = r.sessions.release(s.id);
        assert!(out.text.contains("abandoned"), "{}", out.text);
        assert_eq!(r.remote.abandoned().len(), 1);
        // Mid-collect is the same.
        let mut r = rig(1);
        r.remote.polls_before_done(5);
        let s = r
            .sessions
            .spawn(&spec("a", "agent", Some("build")))
            .unwrap();
        r.world.exit(s.id, 0);
        assert_eq!(r.sessions.poll(s.id), SessionState::Running);
        r.sessions.release(s.id);
        assert_eq!(r.remote.abandoned().len(), 1);
    }

    #[test]
    fn reconciling_and_the_local_views_pass_through() {
        let mut r = rig(1);
        r.remote.set_reconcile(&["one", "two"]);
        assert_eq!(
            r.sessions.reconcile_remote(),
            vec!["one".to_string(), "two".to_string()]
        );
        let s = r
            .sessions
            .spawn(&spec("a", "agent", Some("build")))
            .unwrap();
        r.world.print(s.id, "hello\n");
        assert!(r.sessions.snapshot(s.id, 100).unwrap().contains("hello"));
        assert_eq!(r.sessions.read_since(s.id, 0, 100).unwrap().text, "hello\n");
        assert_eq!(r.sessions.live().len(), 1);
        assert!(r.sessions.start_token(s.identity.pid).is_none());
        r.world
            .set_running_process(s.identity.pid, &s.identity.start_token);
        assert_eq!(
            r.sessions.terminate_orphan(&s.identity, 10),
            SignalOutcome::Sent
        );
    }
}
