//! An in-memory [`RemoteHost`]: records what the application layer asks of a host and answers as
//! scripted. No process, no network.

use crate::config::AppConfig;
use crate::ports::Signal;
use crate::remote::{
    CollectId, CollectRequest, Collected, LaunchRequest, LocalCommand, PrepareRequest, Prepared,
    RemoteHost, RemoteSessions, RemoteTask,
};
use crate::testing::{Harness, World};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::Sha;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Default)]
struct State {
    prepared: Vec<PrepareRequest>,
    launched: Vec<LaunchRequest>,
    collects: Vec<CollectRequest>,
    signals: Vec<(RemoteTask, Signal)>,
    abandoned: Vec<RemoteTask>,
    fail_prepare: Option<String>,
    fail_launch: Option<String>,
    fail_begin_collect: Option<String>,
    /// Polls that answer "still running" before the collect result is given.
    polls_before_done: u32,
    collect_result: Option<std::result::Result<Collected, String>>,
    reconcile_lines: Vec<String>,
    next: u64,
    pending: BTreeMap<u64, (u32, std::result::Result<Collected, String>)>,
    sha_counter: u64,
}

/// Shared handle: clone it, give one clone to [`RemoteSessions`] and keep one to script and inspect.
#[derive(Clone, Default)]
pub struct FakeRemote(Arc<Mutex<State>>);

impl FakeRemote {
    pub fn new() -> FakeRemote {
        FakeRemote::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        match self.0.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    pub fn fail_prepare(&self, message: Option<&str>) {
        self.lock().fail_prepare = message.map(str::to_string);
    }
    pub fn fail_launch(&self, message: Option<&str>) {
        self.lock().fail_launch = message.map(str::to_string);
    }
    pub fn fail_begin_collect(&self, message: Option<&str>) {
        self.lock().fail_begin_collect = message.map(str::to_string);
    }
    pub fn polls_before_done(&self, n: u32) {
        self.lock().polls_before_done = n;
    }
    /// What the next collects answer; by default no change and no notes.
    pub fn collect_answer(&self, answer: std::result::Result<Collected, String>) {
        self.lock().collect_result = Some(answer);
    }
    pub fn set_reconcile(&self, lines: &[&str]) {
        self.lock().reconcile_lines = lines.iter().map(|l| l.to_string()).collect();
    }

    pub fn prepared(&self) -> Vec<PrepareRequest> {
        self.lock().prepared.clone()
    }
    pub fn launched(&self) -> Vec<LaunchRequest> {
        self.lock().launched.clone()
    }
    pub fn collects(&self) -> Vec<CollectRequest> {
        self.lock().collects.clone()
    }
    pub fn signals(&self) -> Vec<(RemoteTask, Signal)> {
        self.lock().signals.clone()
    }
    pub fn abandoned(&self) -> Vec<RemoteTask> {
        self.lock().abandoned.clone()
    }
}

fn dir_of(task: &RemoteTask) -> String {
    format!("/remote/{}/{}/{}", task.host, task.run, task.task)
}

impl RemoteHost for FakeRemote {
    fn remote_dir(&self, task: &RemoteTask) -> Result<String> {
        Ok(dir_of(task))
    }

    fn prepare(&self, req: &PrepareRequest) -> Result<Prepared> {
        let mut g = self.lock();
        if let Some(m) = g.fail_prepare.clone() {
            return Err(FleetError::internal(m));
        }
        g.prepared.push(req.clone());
        g.sha_counter += 1;
        let sent = Sha::parse(&format!("{:040x}", g.sha_counter)).unwrap_or_else(|_| unreachable_sha());
        Ok(Prepared {
            dir: dir_of(&req.task),
            sent,
        })
    }

    fn launch_command(&self, req: &LaunchRequest) -> Result<LocalCommand> {
        let mut g = self.lock();
        if let Some(m) = g.fail_launch.clone() {
            return Err(FleetError::internal(m));
        }
        g.launched.push(req.clone());
        let mut args = vec!["-tt".to_string(), req.task.host.to_string(), req.program.clone()];
        args.extend(req.args.iter().cloned());
        Ok(LocalCommand {
            program: "/usr/bin/ssh".to_string(),
            args,
            env: vec![("TERM".to_string(), "xterm".to_string())],
        })
    }

    fn begin_collect(&self, req: &CollectRequest) -> Result<CollectId> {
        let mut g = self.lock();
        if let Some(m) = g.fail_begin_collect.clone() {
            return Err(FleetError::internal(m));
        }
        g.collects.push(req.clone());
        g.next += 1;
        let id = g.next;
        let answer = g.collect_result.clone().unwrap_or(Ok(Collected {
            head: None,
            notes: Vec::new(),
        }));
        let polls = g.polls_before_done;
        g.pending.insert(id, (polls, answer));
        Ok(CollectId(id))
    }

    fn poll_collect(&self, id: CollectId) -> Option<Result<Collected>> {
        let mut g = self.lock();
        let (left, _) = g.pending.get_mut(&id.0)?;
        if *left > 0 {
            *left -= 1;
            return None;
        }
        let (_, answer) = g.pending.remove(&id.0)?;
        Some(answer.map_err(FleetError::internal))
    }

    fn signal(&self, task: &RemoteTask, signal: Signal) {
        self.lock().signals.push((task.clone(), signal));
    }

    fn abandon(&self, task: &RemoteTask) {
        self.lock().abandoned.push(task.clone());
    }

    fn reconcile(&self) -> Vec<String> {
        self.lock().reconcile_lines.clone()
    }
}

fn unreachable_sha() -> Sha {
    Sha::parse(&"0".repeat(40)).expect("a 40-digit hex id is a valid sha")
}

/// An orchestrator whose session host is [`RemoteSessions`] around the fake session host of a
/// fresh world, with `remote` as the host.
pub fn remote_harness(cfg: AppConfig, remote: &FakeRemote) -> Harness {
    let world = World::new();
    let mut ports = world.ports();
    let hosts = cfg.hosts.clone();
    let inner = std::mem::replace(&mut ports.sessions, Box::new(NoSessions));
    ports.sessions = Box::new(RemoteSessions::new(inner, Arc::new(remote.clone()), hosts));
    let mut orch = crate::orchestrator::Orchestrator::new(cfg, ports).unwrap();
    orch.startup(4242, "test").unwrap();
    Harness { orch, world }
}

/// Placeholder while the real one is moved into the wrapper.
struct NoSessions;

impl crate::ports::SessionHost for NoSessions {
    fn spawn(&mut self, _: &crate::ports::SessionSpec) -> Result<crate::ports::Spawned> {
        Err(FleetError::internal("no sessions"))
    }
    fn poll(&mut self, _: crate::ports::SessionId) -> crate::ports::SessionState {
        crate::ports::SessionState::Exited { code: None, signal: None }
    }
    fn reading(&self, _: crate::ports::SessionId) -> Option<crate::ports::SessionReading> {
        None
    }
    fn signal(&mut self, _: crate::ports::SessionId, _: Signal) -> crate::ports::SignalOutcome {
        crate::ports::SignalOutcome::Gone
    }
    fn snapshot(&self, _: crate::ports::SessionId, _: usize) -> Option<String> {
        None
    }
    fn read_since(&self, _: crate::ports::SessionId, _: u64, _: usize) -> Option<fleet_domain::output::Chunk> {
        None
    }
    fn release(&mut self, _: crate::ports::SessionId) -> crate::ports::SessionOutput {
        crate::ports::SessionOutput { text: String::new(), stripped: 0 }
    }
    fn live(&self) -> Vec<fleet_domain::supervision::LiveSession> {
        Vec::new()
    }
    fn start_token(&self, _: u32) -> Option<String> {
        None
    }
    fn terminate_orphan(
        &mut self,
        _: &fleet_domain::supervision::ProcessIdentity,
        _: u64,
    ) -> crate::ports::SignalOutcome {
        crate::ports::SignalOutcome::Gone
    }
}
