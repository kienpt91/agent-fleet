//! An in-memory [`RemoteHost`]: records what the application layer asks of a host and answers as
//! scripted. No process, no network.

use crate::config::AppConfig;
use crate::ports::Signal;
use crate::remote::{
    CollectId, CollectRequest, Collected, LaunchRequest, LocalCommand, PrepareRequest, Prepared,
    RemoteHost, RemoteSessions, RemoteTask,
};
use crate::testing::forge::{FakeForgeJobs, ForgeWorld};
use crate::testing::{Harness, World};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::Sha;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Default)]
struct State {
    /// Worker commands that may be launched; empty means every command.
    allowed: Vec<String>,
    /// Worktrees that exist on the fake host now.
    existing: std::collections::BTreeSet<String>,
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

    /// Only these worker commands may be launched (like a host's `allowed_workers`).
    pub fn allow_workers(&self, commands: &[&str]) {
        self.lock().allowed = commands.iter().map(|c| c.to_string()).collect();
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

    /// True while the task's worktree exists on the fake host.
    pub fn exists(&self, task: &RemoteTask) -> bool {
        self.lock().existing.contains(&dir_of(task))
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
        g.existing.insert(dir_of(&req.task));
        g.sha_counter += 1;
        let sent =
            Sha::parse(&format!("{:040x}", g.sha_counter)).unwrap_or_else(|_| unreachable_sha());
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
        if !g.allowed.is_empty() && !g.allowed.contains(&req.program) {
            return Err(FleetError::forbidden("worker command is not allowed"));
        }
        g.launched.push(req.clone());
        let mut args = vec![
            "-tt".to_string(),
            req.task.host.to_string(),
            req.program.clone(),
        ];
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
        g.existing.remove(&dir_of(&req.task));
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
        let Some((left, _)) = g.pending.get_mut(&id.0) else {
            return Some(Err(FleetError::internal("unknown collect job")));
        };
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
        let mut g = self.lock();
        g.abandoned.push(task.clone());
        g.existing.remove(&dir_of(task));
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
    let forge = ForgeWorld::new();
    let enabled = cfg.forge.enabled;
    let mut orch = crate::orchestrator::Orchestrator::new(cfg, ports).unwrap();
    if enabled {
        orch.attach_forge(Box::new(FakeForgeJobs::new(forge.clone())));
    }
    orch.startup(4242, "test").unwrap();
    Harness { orch, world, forge }
}

/// Placeholder while the real one is moved into the wrapper.
struct NoSessions;

impl crate::ports::SessionHost for NoSessions {
    fn spawn(&mut self, _: &crate::ports::SessionSpec) -> Result<crate::ports::Spawned> {
        Err(FleetError::internal("no sessions"))
    }
    fn poll(&mut self, _: crate::ports::SessionId) -> crate::ports::SessionState {
        crate::ports::SessionState::Exited {
            code: None,
            signal: None,
        }
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
    fn read_since(
        &self,
        _: crate::ports::SessionId,
        _: u64,
        _: usize,
    ) -> Option<fleet_domain::output::Chunk> {
        None
    }
    fn release(&mut self, _: crate::ports::SessionId) -> crate::ports::SessionOutput {
        crate::ports::SessionOutput {
            text: String::new(),
            stripped: 0,
        }
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
    fn notify(&mut self, _: crate::ports::SessionId, _: &str) -> Result<()> {
        Err(FleetError::unavailable("no sessions"))
    }
}

// ---- the contract every RemoteHost must satisfy ----------------------------------------------------

/// What the worker of a contract run does to its worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerChange {
    Nothing,
    WriteFile { name: String, content: String },
}

/// Everything the contract suite needs from a medium: the adapter, a prepared local task worktree
/// and a way to run the command the adapter built (a real ssh client for the real adapter).
pub trait RemoteHarness {
    fn host(&self) -> &dyn RemoteHost;
    fn task(&self) -> RemoteTask;
    /// A local task worktree on its task branch with a commit and `.fleet/brief.md`.
    fn worktree(&self) -> String;
    /// The worker command the host allows and the arguments that make it do `change`.
    fn worker(&self, change: &WorkerChange) -> (String, Vec<String>);
    /// The environment given to the remote worker.
    fn worker_env(&self) -> Vec<(String, String)>;
    /// Runs the local command to the end and returns its exit code.
    fn run_local(&mut self, command: &LocalCommand, change: &WorkerChange) -> i32;
    /// Content of a file of the local worktree.
    fn local_file(&self, relative: &str) -> Option<String>;
    /// The commit the local task branch points at.
    fn local_head(&self) -> String;
    /// True while the worktree of the task exists on the host.
    fn remote_exists(&self) -> bool;
}

fn collect_to_end(h: &dyn RemoteHarness, req: &CollectRequest) -> Result<Collected> {
    let id = h.host().begin_collect(req).expect("begin_collect");
    let start = std::time::Instant::now();
    loop {
        if let Some(answer) = h.host().poll_collect(id) {
            return answer;
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(120),
            "collecting did not finish"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// The behaviour of a [`RemoteHost`], whatever it talks to.
pub fn remote_host_contract(h: &mut dyn RemoteHarness) {
    let task = h.task();
    let worktree = h.worktree();

    // The directory is stable and below the host's own naming (its last parts are run and task).
    let dir = h.host().remote_dir(&task).expect("remote_dir");
    assert_eq!(
        dir,
        h.host().remote_dir(&task).expect("remote_dir"),
        "remote_dir is stable"
    );
    assert!(
        dir.ends_with(&format!("/{}/{}", task.run, task.task)),
        "{dir} does not end in run/task"
    );
    assert!(dir.starts_with('/') && !dir.contains(".."), "{dir}");

    // Preparing makes the worktree; preparing again replaces what an earlier attempt left.
    let request = PrepareRequest {
        task: task.clone(),
        attempt: 1,
        worktree: worktree.clone(),
    };
    let first = h.host().prepare(&request).expect("prepare");
    assert_eq!(first.dir, dir);
    assert!(h.remote_exists(), "prepare made no remote worktree");
    let second = h.host().prepare(&request).expect("prepare again");
    assert_eq!(second.dir, dir);

    // A worker command the host does not allow cannot be launched.
    let refused = h.host().launch_command(&LaunchRequest {
        task: task.clone(),
        program: "definitely-not-allowed".to_string(),
        args: vec![],
        env: h.worker_env(),
    });
    assert!(
        refused.is_err(),
        "a command the host does not allow was launched"
    );

    // A worker that writes a file: the file and a new head come back, the remote is removed.
    let change = WorkerChange::WriteFile {
        name: "from-the-host.txt".to_string(),
        content: "made remotely\n".to_string(),
    };
    let (program, args) = h.worker(&change);
    let command = h
        .host()
        .launch_command(&LaunchRequest {
            task: task.clone(),
            program,
            args,
            env: h.worker_env(),
        })
        .expect("launch_command");
    assert!(
        command.program.starts_with('/'),
        "the local program is not an absolute path"
    );
    let before = h.local_head();
    assert_eq!(h.run_local(&command, &change), 0);
    let got = collect_to_end(
        h,
        &CollectRequest {
            task: task.clone(),
            worktree: worktree.clone(),
            sent: second.sent.clone(),
            attempt: 1,
        },
    )
    .expect("collect");
    let head = got.head.expect("a changed file comes back as a new head");
    assert_eq!(
        head.as_str(),
        h.local_head(),
        "the reported head is the local branch head"
    );
    assert_ne!(h.local_head(), before);
    assert_eq!(
        h.local_file("from-the-host.txt").as_deref(),
        Some("made remotely\n")
    );
    assert!(!h.remote_exists(), "the remote worktree was not removed");

    // A worker that changes nothing leaves the head alone.
    let again = h
        .host()
        .prepare(&request)
        .expect("prepare after collecting");
    let nothing = WorkerChange::Nothing;
    let (program, args) = h.worker(&nothing);
    let command = h
        .host()
        .launch_command(&LaunchRequest {
            task: task.clone(),
            program,
            args,
            env: h.worker_env(),
        })
        .expect("launch_command");
    let before = h.local_head();
    assert_eq!(h.run_local(&command, &nothing), 0);
    let got = collect_to_end(
        h,
        &CollectRequest {
            task: task.clone(),
            worktree,
            sent: again.sent,
            attempt: 2,
        },
    )
    .expect("collect");
    assert_eq!(got.head, None);
    assert_eq!(h.local_head(), before);

    // Giving up removes the worktree, and an answer for an unknown job is an error, not a hang.
    h.host().prepare(&request).expect("prepare for abandon");
    assert!(h.remote_exists());
    h.host().abandon(&task);
    let start = std::time::Instant::now();
    while h.remote_exists() {
        assert!(
            start.elapsed() < std::time::Duration::from_secs(60),
            "abandon did not remove the remote worktree"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(matches!(
        h.host().poll_collect(CollectId(u64::MAX)),
        Some(Err(_))
    ));
}

/// A harness for [`FakeRemote`]: the "local command" is a no-op and the worker's effect is
/// scripted into the fake.
pub struct MemoryRemoteHarness {
    remote: FakeRemote,
    files: BTreeMap<String, String>,
    head: u64,
}

impl MemoryRemoteHarness {
    pub fn new() -> MemoryRemoteHarness {
        let remote = FakeRemote::new();
        remote.allow_workers(&["agent"]);
        MemoryRemoteHarness {
            remote,
            files: BTreeMap::new(),
            head: 1,
        }
    }

    fn head_sha(&self) -> Sha {
        Sha::parse(&format!("{:040x}", self.head)).expect("a valid sha")
    }
}

impl Default for MemoryRemoteHarness {
    fn default() -> Self {
        MemoryRemoteHarness::new()
    }
}

impl RemoteHarness for MemoryRemoteHarness {
    fn host(&self) -> &dyn RemoteHost {
        &self.remote
    }
    fn task(&self) -> RemoteTask {
        RemoteTask {
            host: fleet_domain::ids::HostName::parse("build").expect("host"),
            run: fleet_domain::ids::RunId::parse("r1").expect("run"),
            task: fleet_domain::ids::TaskId::parse("t1").expect("task"),
        }
    }
    fn worktree(&self) -> String {
        "/local/wt".to_string()
    }
    fn worker(&self, _change: &WorkerChange) -> (String, Vec<String>) {
        ("agent".to_string(), vec![])
    }
    fn worker_env(&self) -> Vec<(String, String)> {
        vec![("A".to_string(), "b".to_string())]
    }
    fn run_local(&mut self, _command: &LocalCommand, change: &WorkerChange) -> i32 {
        match change {
            WorkerChange::Nothing => self.remote.collect_answer(Ok(Collected {
                head: None,
                notes: vec![],
            })),
            WorkerChange::WriteFile { name, content } => {
                self.files.insert(name.clone(), content.clone());
                self.head += 1;
                self.remote.collect_answer(Ok(Collected {
                    head: Some(self.head_sha()),
                    notes: vec![],
                }));
            }
        }
        0
    }
    fn local_file(&self, relative: &str) -> Option<String> {
        self.files.get(relative).cloned()
    }
    fn local_head(&self) -> String {
        self.head_sha().as_str().to_string()
    }
    fn remote_exists(&self) -> bool {
        self.remote.exists(&self.task())
    }
}
