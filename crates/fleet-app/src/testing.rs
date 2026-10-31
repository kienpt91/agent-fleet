//! In-memory fakes of every port, for tests of the application layer and (through the contract
//! suites) of the real adapters.
//!
//! All fakes share one [`World`], a handle tests keep to script behaviour (make a session exit,
//! plant a worker's question file, make a merge conflict) and to inspect what happened.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

pub mod contracts;

use crate::config::{AppConfig, WorkerDef};
use crate::orchestrator::Orchestrator;
use crate::ports::*;
use fleet_domain::approval::Digest;
use fleet_domain::contract::{ContractRead, WorkerQuestion, WorkerResult};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::events::DomainEvent;
use fleet_domain::ids::{ApprovalId, RunId, Sha, TaskId, WorkerName};
use fleet_domain::output::Chunk;
use fleet_domain::quality::GateOutcome;
use fleet_domain::status::Verdict;
use fleet_domain::supervision::{LiveSession, ProcessIdentity};
use neo_verify::judge::CheckReport;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

/// How the fake job runner answers gate jobs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateMode {
    /// Every gate passes, immediately.
    Pass,
    /// The first gate fails with this output (the rest pass), immediately.
    Fail(String),
    /// The test completes jobs itself with [`World::complete_gates`].
    Manual,
}

/// How the fake job runner answers verification jobs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifyMode {
    /// Every check passes, immediately.
    Pass,
    /// The first mandatory check of every contract fails with this output (the rest pass),
    /// immediately.
    Fail(String),
    /// The test completes jobs itself with [`World::complete_verify`].
    Manual,
}

#[derive(Debug)]
pub struct FakeSession {
    pub spec: SessionSpec,
    pub identity: ProcessIdentity,
    pub state: SessionState,
    pub elapsed_ms: u64,
    pub idle_ms: u64,
    pub output: String,
    pub signals: Vec<Signal>,
    pub released: bool,
    /// A SIGTERM makes the session exit (as most well-behaved workers do).
    pub exit_on_term: bool,
    /// A SIGKILL makes the session exit.
    pub exit_on_kill: bool,
    pub refuse_signals: bool,
}

#[derive(Default)]
struct Inner {
    now_ms: u64,
    events: Vec<StoredEvent>,
    fail_append: bool,
    published: Vec<StoredEvent>,

    base_sha: Option<Sha>,
    missing_repos: BTreeSet<String>,
    conflict_branches: BTreeSet<String>,
    dirty_worktrees: BTreeSet<String>,
    fail_prepare: bool,
    prepared: Vec<(String, String)>,
    integration_created: BTreeSet<String>,
    merges: Vec<(String, String)>,
    worktree_merges: Vec<(String, Vec<String>)>,
    branch_heads: BTreeMap<String, Sha>,
    merge_requests: Vec<IntegrationMerge>,
    removed: Vec<(String, bool)>,
    merged_branches: BTreeSet<String>,
    sha_counter: u64,

    sessions: BTreeMap<u64, FakeSession>,
    next_session: u64,
    spawn_error: Option<String>,
    start_tokens: BTreeMap<u32, String>,
    terminated: Vec<ProcessIdentity>,

    questions: BTreeMap<String, ContractRead<WorkerQuestion>>,
    results: BTreeMap<String, ContractRead<WorkerResult>>,
    briefs: Vec<(String, String)>,
    fail_prepare_contract: bool,

    gate_mode: Option<GateMode>,
    verify_mode: Option<VerifyMode>,
    worktree_changed: Vec<String>,
    worktree_truncated: bool,
    fail_worktree_state: bool,
    jobs_started: Vec<Job>,
    jobs_done: VecDeque<JobDone>,
    jobs_running: usize,
    job_groups: Vec<LiveSession>,

    logs: BTreeMap<(String, String), (u32, String)>,

    undo_available: bool,
    undo_calls: Vec<String>,
    undo_fails: bool,
}

/// Shared handle to the state behind all fakes.
#[derive(Clone, Default)]
pub struct World(Arc<Mutex<Inner>>);

impl World {
    pub fn new() -> World {
        let w = World::default();
        {
            let mut g = w.lock();
            g.now_ms = 1_000_000;
            g.gate_mode = Some(GateMode::Pass);
            g.verify_mode = Some(VerifyMode::Pass);
        }
        w
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        match self.0.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// All ports backed by this world.
    pub fn ports(&self) -> Ports {
        Ports {
            clock: Box::new(FakeClock(self.clone())),
            store: Box::new(MemoryEventStore(self.clone())),
            vcs: Box::new(FakeVcs(self.clone())),
            sessions: Box::new(FakeSessions(self.clone())),
            contract: Box::new(FakeContract(self.clone())),
            jobs: Box::new(FakeJobs(self.clone())),
            logs: Box::new(MemoryLogs(self.clone())),
            undo: Box::new(FakeUndo(self.clone())),
            digest: Box::new(ToyDigest),
            publisher: Box::new(RecordingPublisher(self.clone())),
        }
    }

    // ---- time ----
    pub fn now_ms(&self) -> u64 {
        self.lock().now_ms
    }
    pub fn advance(&self, ms: u64) {
        self.lock().now_ms += ms;
    }

    // ---- events ----
    pub fn stored_events(&self) -> Vec<DomainEvent> {
        self.lock().events.iter().map(|e| e.event.clone()).collect()
    }
    pub fn event_kinds(&self) -> Vec<&'static str> {
        self.lock().events.iter().map(|e| e.event.kind()).collect()
    }
    pub fn published(&self) -> Vec<StoredEvent> {
        self.lock().published.clone()
    }
    pub fn fail_appends(&self, fail: bool) {
        self.lock().fail_append = fail;
    }

    // ---- vcs ----
    pub fn conflict_on(&self, branch: &str) {
        self.lock().conflict_branches.insert(branch.to_string());
    }
    pub fn clear_conflicts(&self) {
        self.lock().conflict_branches.clear();
    }
    pub fn mark_dirty(&self, worktree: &str) {
        self.lock().dirty_worktrees.insert(worktree.to_string());
    }
    pub fn fail_prepare_worktree(&self, fail: bool) {
        self.lock().fail_prepare = fail;
    }
    pub fn missing_repo(&self, repo: &str) {
        self.lock().missing_repos.insert(repo.to_string());
    }
    pub fn merges(&self) -> Vec<(String, String)> {
        self.lock().merges.clone()
    }
    pub fn worktree_merges(&self) -> Vec<(String, Vec<String>)> {
        self.lock().worktree_merges.clone()
    }
    /// Makes `branch` point at `head` (every other branch points at `ccc...c`).
    pub fn set_branch_head(&self, branch: &str, head: Sha) {
        self.lock().branch_heads.insert(branch.to_string(), head);
    }
    /// Every request that reached `merge_into_integration` (conflicting ones included).
    pub fn merge_requests(&self) -> Vec<IntegrationMerge> {
        self.lock().merge_requests.clone()
    }
    pub fn removed_worktrees(&self) -> Vec<(String, bool)> {
        self.lock().removed.clone()
    }
    pub fn mark_merged(&self, branch: &str) {
        self.lock().merged_branches.insert(branch.to_string());
    }
    /// Makes `is_merged` answer false again for a branch (for example after a force-push).
    pub fn unmark_merged(&self, branch: &str) {
        self.lock().merged_branches.remove(branch);
    }

    // ---- sessions ----
    pub fn session_ids(&self) -> Vec<SessionId> {
        self.lock().sessions.keys().map(|k| SessionId(*k)).collect()
    }
    pub fn live_session_ids(&self) -> Vec<SessionId> {
        self.lock()
            .sessions
            .iter()
            .filter(|(_, s)| !s.released)
            .map(|(k, _)| SessionId(*k))
            .collect()
    }
    pub fn session_spec(&self, id: SessionId) -> Option<SessionSpec> {
        self.lock().sessions.get(&id.0).map(|s| s.spec.clone())
    }
    pub fn session_for(&self, task: &str) -> Option<SessionId> {
        self.lock()
            .sessions
            .iter()
            .rev()
            .find(|(_, s)| s.spec.task.as_str() == task && !s.released)
            .map(|(k, _)| SessionId(*k))
    }
    pub fn exit(&self, id: SessionId, code: i64) {
        if let Some(s) = self.lock().sessions.get_mut(&id.0) {
            s.state = SessionState::Exited {
                code: Some(code),
                signal: None,
            };
        }
    }
    pub fn exit_by_signal(&self, id: SessionId, name: &str) {
        if let Some(s) = self.lock().sessions.get_mut(&id.0) {
            s.state = SessionState::Exited {
                code: None,
                signal: Some(name.to_string()),
            };
        }
    }
    pub fn set_timing(&self, id: SessionId, elapsed_ms: u64, idle_ms: u64) {
        if let Some(s) = self.lock().sessions.get_mut(&id.0) {
            s.elapsed_ms = elapsed_ms;
            s.idle_ms = idle_ms;
        }
    }
    pub fn print(&self, id: SessionId, text: &str) {
        if let Some(s) = self.lock().sessions.get_mut(&id.0) {
            s.output.push_str(text);
        }
    }
    pub fn signals(&self, id: SessionId) -> Vec<Signal> {
        self.lock()
            .sessions
            .get(&id.0)
            .map(|s| s.signals.clone())
            .unwrap_or_default()
    }
    pub fn configure_session(
        &self,
        id: SessionId,
        exit_on_term: bool,
        exit_on_kill: bool,
        refuse: bool,
    ) {
        if let Some(s) = self.lock().sessions.get_mut(&id.0) {
            s.exit_on_term = exit_on_term;
            s.exit_on_kill = exit_on_kill;
            s.refuse_signals = refuse;
        }
    }
    pub fn fail_spawn(&self, message: Option<&str>) {
        self.lock().spawn_error = message.map(str::to_string);
    }
    /// Pretends a process with this pid and start token is running (for orphan reconciliation).
    pub fn set_running_process(&self, pid: u32, token: &str) {
        self.lock().start_tokens.insert(pid, token.to_string());
    }
    pub fn terminated_orphans(&self) -> Vec<ProcessIdentity> {
        self.lock().terminated.clone()
    }

    // ---- worker files ----
    pub fn write_question(&self, worktree: &str, q: ContractRead<WorkerQuestion>) {
        self.lock().questions.insert(worktree.to_string(), q);
    }
    pub fn write_result(&self, worktree: &str, r: ContractRead<WorkerResult>) {
        self.lock().results.insert(worktree.to_string(), r);
    }
    pub fn briefs(&self) -> Vec<(String, String)> {
        self.lock().briefs.clone()
    }
    pub fn fail_contract_prepare(&self, fail: bool) {
        self.lock().fail_prepare_contract = fail;
    }

    // ---- jobs ----
    pub fn set_gate_mode(&self, mode: GateMode) {
        self.lock().gate_mode = Some(mode);
    }
    pub fn set_verify_mode(&self, mode: VerifyMode) {
        self.lock().verify_mode = Some(mode);
    }
    /// The paths the fake worktrees report as changed (for diff constraints).
    pub fn set_worktree_changes(&self, paths: &[&str], truncated: bool) {
        let mut g = self.lock();
        g.worktree_changed = paths.iter().map(|p| p.to_string()).collect();
        g.worktree_truncated = truncated;
    }
    /// The process groups the fake job runner reports as running (what a gate or a verification
    /// check started).
    pub fn set_job_groups(&self, groups: &[(u32, u32)]) {
        self.lock().job_groups = groups
            .iter()
            .map(|(pid, pgid)| LiveSession {
                pid: *pid,
                pgid: *pgid,
            })
            .collect();
    }
    /// Makes one job slot look taken (a job that never finishes).
    pub fn occupy_job_slot(&self) {
        self.lock().jobs_running += 1;
    }
    pub fn free_job_slots(&self) {
        self.lock().jobs_running = 0;
    }
    pub fn fail_worktree_state(&self, fail: bool) {
        self.lock().fail_worktree_state = fail;
    }
    /// Verification jobs started so far, oldest first.
    pub fn verify_jobs(&self) -> Vec<crate::ports::VerifyJob> {
        self.lock()
            .jobs_started
            .iter()
            .filter_map(|j| match j {
                Job::Verify(v) => Some((**v).clone()),
                _ => None,
            })
            .collect()
    }
    /// Completes the newest verification job of `run/task` with every check passing except the
    /// ones named in `verdicts`.
    pub fn complete_verify(&self, run: &str, task: &str, verdicts: &[(&str, Verdict)]) {
        let mut g = self.lock();
        let job = g
            .jobs_started
            .iter()
            .rev()
            .find_map(|j| match j {
                Job::Verify(v) if v.run.as_str() == run && v.task.as_str() == task => {
                    Some(v.clone())
                }
                _ => None,
            })
            .expect("no verification job was started for the task");
        g.jobs_running = g.jobs_running.saturating_sub(1);
        let done = verify_outcome(&job, |check, _| {
            verdicts
                .iter()
                .find(|(c, _)| *c == check)
                .map_or((Verdict::Passed, String::new()), |(_, v)| {
                    (*v, String::new())
                })
        });
        g.jobs_done.push_back(JobDone::Verify(Box::new(done)));
    }
    pub fn jobs_started(&self) -> Vec<Job> {
        self.lock().jobs_started.clone()
    }
    pub fn complete_gates(&self, run: &str, task: &str, attempt: u32, outcomes: Vec<GateOutcome>) {
        let mut g = self.lock();
        g.jobs_running = g.jobs_running.saturating_sub(1);
        g.jobs_done.push_back(JobDone::Gates {
            run: RunId::parse(run).unwrap(),
            task: TaskId::parse(task).unwrap(),
            attempt,
            outcomes,
        });
    }
    pub fn complete_command(
        &self,
        approval: &ApprovalId,
        exit_code: Option<i64>,
        summary: &str,
        output: &str,
    ) {
        let mut g = self.lock();
        g.jobs_running = g.jobs_running.saturating_sub(1);
        g.jobs_done.push_back(JobDone::Command {
            approval: approval.clone(),
            exit_code,
            summary: summary.to_string(),
            output: output.to_string(),
        });
    }

    // ---- logs / undo ----
    pub fn saved_log(&self, run: &str, task: &str) -> Option<String> {
        self.lock()
            .logs
            .get(&(run.to_string(), task.to_string()))
            .map(|(_, t)| t.clone())
    }
    pub fn set_undo(&self, available: bool) {
        self.lock().undo_available = available;
    }
    pub fn undo_fails(&self, fails: bool) {
        self.lock().undo_fails = fails;
    }
    pub fn undo_calls(&self) -> Vec<String> {
        self.lock().undo_calls.clone()
    }
}

// ---- digest ---------------------------------------------------------------------------------

/// A deterministic stand-in for SHA-256 (not cryptographic) so tests can use approvals.
pub struct ToyDigest;

impl Digest for ToyDigest {
    fn sha256_hex(&self, data: &[u8]) -> String {
        let mut acc: [u64; 4] = [
            0xcbf2_9ce4_8422_2325,
            0x1000_0000_01b3,
            0x9e37_79b9_7f4a_7c15,
            0xff51_afd7_ed55_8ccd,
        ];
        for (i, b) in data.iter().enumerate() {
            let slot = i % 4;
            if let Some(a) = acc.get_mut(slot) {
                *a = (*a ^ u64::from(*b))
                    .wrapping_mul(0x1000_0000_01b3)
                    .rotate_left(5);
            }
            let next = (slot + 1) % 4;
            let prev = acc.get(slot).copied().unwrap_or(0);
            if let Some(n) = acc.get_mut(next) {
                *n ^= prev;
            }
        }
        acc.iter().map(|a| format!("{a:016x}")).collect()
    }
}

// ---- clock ----------------------------------------------------------------------------------

pub struct FakeClock(World);
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.now_ms()
    }
}

// ---- event store ---------------------------------------------------------------------------

pub struct MemoryEventStore(World);

impl EventStore for MemoryEventStore {
    fn append(&mut self, ts_ms: u64, events: &[DomainEvent]) -> Result<u64> {
        let mut g = self.0.lock();
        if g.fail_append {
            return Err(FleetError::internal("the store is unavailable"));
        }
        for e in events {
            let seq = g.events.len() as u64 + 1;
            g.events.push(StoredEvent {
                seq,
                ts_ms,
                event: e.clone(),
            });
        }
        Ok(g.events.len() as u64)
    }

    fn replay(&self, visit: &mut dyn FnMut(StoredEvent) -> Result<()>) -> Result<ReplayInfo> {
        let events = self.0.lock().events.clone();
        let records = events.len() as u64;
        for e in events {
            visit(e)?;
        }
        Ok(ReplayInfo { records })
    }
}

pub struct RecordingPublisher(World);
impl EventPublisher for RecordingPublisher {
    fn publish(&mut self, events: &[StoredEvent]) {
        self.0.lock().published.extend_from_slice(events);
    }
}

// ---- version control -----------------------------------------------------------------------

pub struct FakeVcs(World);

impl FakeVcs {
    fn next_sha(g: &mut Inner, tag: char) -> Sha {
        g.sha_counter += 1;
        let hex = format!("{:x}", g.sha_counter);
        let mut s = tag.to_string().repeat(40 - hex.len());
        s.push_str(&hex);
        Sha::parse(&s).unwrap()
    }
}

impl Vcs for FakeVcs {
    fn resolve_base(&self, repo: &str, base_ref: &str) -> Result<BaseCommit> {
        let g = self.0.lock();
        if g.missing_repos.contains(repo) {
            return Err(FleetError::not_found(format!(
                "{repo} is not a git repository"
            )));
        }
        if base_ref != "HEAD" {
            fleet_domain::ids::validate_ref_name(base_ref)?;
        }
        let sha = match &g.base_sha {
            Some(s) => s.clone(),
            None => Sha::parse(&"a".repeat(40))?,
        };
        Ok(BaseCommit {
            ref_name: base_ref.to_string(),
            sha,
        })
    }

    fn prepare_worktree(
        &self,
        repo: &str,
        run: &RunId,
        task: &TaskId,
        branch: &str,
        _base: &Sha,
    ) -> Result<String> {
        let mut g = self.0.lock();
        if g.fail_prepare {
            return Err(FleetError::internal("git worktree add failed"));
        }
        let path = format!("{repo}/.fleet-worktrees/{run}/{task}");
        g.prepared.push((path.clone(), branch.to_string()));
        Ok(path)
    }

    fn merge_into_worktree(
        &self,
        worktree: &str,
        branches: &[String],
        _identity: &Identity,
    ) -> Result<MergeOutcome> {
        let mut g = self.0.lock();
        g.worktree_merges
            .push((worktree.to_string(), branches.to_vec()));
        if branches.iter().any(|b| g.conflict_branches.contains(b)) {
            return Ok(MergeOutcome::Conflict {
                files: vec!["src/shared.rs".to_string()],
            });
        }
        let sha = Self::next_sha(&mut g, 'b');
        Ok(MergeOutcome::Merged { sha })
    }

    fn commit_all(
        &self,
        worktree: &str,
        _message: &str,
        _identity: &Identity,
    ) -> Result<Option<Sha>> {
        let mut g = self.0.lock();
        if g.dirty_worktrees.remove(worktree) {
            Ok(Some(Self::next_sha(&mut g, 'c')))
        } else {
            Ok(None)
        }
    }

    fn branch_head(&self, _repo: &str, branch: &str) -> Result<Sha> {
        match self.0.lock().branch_heads.get(branch) {
            Some(head) => Ok(head.clone()),
            None => Sha::parse(&"c".repeat(40)),
        }
    }

    fn merge_into_integration(&self, req: &IntegrationMerge) -> Result<IntegrationResult> {
        let mut g = self.0.lock();
        g.merge_requests.push(req.clone());
        let created = g.integration_created.insert(req.run.to_string());
        if g.conflict_branches.contains(&req.task_branch) {
            return Ok(IntegrationResult {
                outcome: MergeOutcome::Conflict {
                    files: vec!["src/lib.rs".to_string()],
                },
                created,
            });
        }
        g.merges
            .push((req.integration_branch.clone(), req.task_branch.clone()));
        g.merged_branches.insert(req.task_branch.clone());
        let sha = Self::next_sha(&mut g, 'd');
        Ok(IntegrationResult {
            outcome: MergeOutcome::Merged { sha },
            created,
        })
    }

    fn remove_worktree(
        &self,
        _repo: &str,
        worktree: &str,
        branch: &str,
        delete_branch: bool,
    ) -> Result<()> {
        let mut g = self.0.lock();
        g.removed.push((
            if worktree.is_empty() {
                branch.to_string()
            } else {
                worktree.to_string()
            },
            delete_branch,
        ));
        Ok(())
    }

    fn is_merged(&self, _repo: &str, branch: &str, _into: &str) -> Result<bool> {
        Ok(self.0.lock().merged_branches.contains(branch))
    }

    fn worktree_state(&self, _worktree: &str, _base: &Sha) -> Result<WorktreeState> {
        let g = self.0.lock();
        if g.fail_worktree_state {
            return Err(FleetError::internal("git could not read the worktree"));
        }
        Ok(WorktreeState {
            head: Sha::parse(&"d".repeat(40))?,
            changed: g.worktree_changed.clone(),
            truncated: g.worktree_truncated,
        })
    }
}

// ---- sessions ------------------------------------------------------------------------------

pub struct FakeSessions(World);

impl SessionHost for FakeSessions {
    fn spawn(&mut self, spec: &SessionSpec) -> Result<Spawned> {
        let mut g = self.0.lock();
        if let Some(msg) = g.spawn_error.clone() {
            return Err(FleetError::internal(msg));
        }
        g.next_session += 1;
        let id = g.next_session;
        let identity = ProcessIdentity::new(10_000 + id as u32, format!("token-{id}"));
        g.sessions.insert(
            id,
            FakeSession {
                spec: spec.clone(),
                identity: identity.clone(),
                state: SessionState::Running,
                elapsed_ms: 0,
                idle_ms: 0,
                output: String::new(),
                signals: Vec::new(),
                released: false,
                exit_on_term: true,
                exit_on_kill: true,
                refuse_signals: false,
            },
        );
        Ok(Spawned {
            id: SessionId(id),
            identity,
        })
    }

    fn poll(&mut self, id: SessionId) -> SessionState {
        self.0.lock().sessions.get(&id.0).map_or(
            SessionState::Exited {
                code: None,
                signal: None,
            },
            |s| s.state.clone(),
        )
    }

    fn reading(&self, id: SessionId) -> Option<SessionReading> {
        self.0.lock().sessions.get(&id.0).map(|s| SessionReading {
            elapsed_ms: s.elapsed_ms,
            idle_ms: s.idle_ms,
        })
    }

    fn signal(&mut self, id: SessionId, signal: Signal) -> SignalOutcome {
        let mut g = self.0.lock();
        let Some(s) = g.sessions.get_mut(&id.0) else {
            return SignalOutcome::Gone;
        };
        if s.refuse_signals {
            return SignalOutcome::Refused("pid reused".to_string());
        }
        if s.state != SessionState::Running {
            return SignalOutcome::Gone;
        }
        s.signals.push(signal);
        let dies = match signal {
            Signal::Term => s.exit_on_term,
            Signal::Kill => s.exit_on_kill,
        };
        if dies {
            s.state = SessionState::Exited {
                code: None,
                signal: Some(
                    if signal == Signal::Term {
                        "SIGTERM"
                    } else {
                        "SIGKILL"
                    }
                    .to_string(),
                ),
            };
        }
        SignalOutcome::Sent
    }

    fn snapshot(&self, id: SessionId, max: usize) -> Option<String> {
        self.0
            .lock()
            .sessions
            .get(&id.0)
            .map(|s| fleet_domain::sanitize::tail_str(&s.output, max).to_string())
    }

    fn read_since(&self, id: SessionId, offset: u64, max: usize) -> Option<Chunk> {
        let g = self.0.lock();
        let s = g.sessions.get(&id.0)?;
        let total = s.output.len() as u64;
        let from = offset.min(total);
        let start = usize::try_from(from).unwrap_or(usize::MAX);
        let text: String = s
            .output
            .get(start..)
            .unwrap_or("")
            .chars()
            .take(max)
            .collect();
        let next = from + text.len() as u64;
        Some(Chunk {
            from,
            next,
            text,
            lost: false,
        })
    }

    fn release(&mut self, id: SessionId) -> SessionOutput {
        let mut g = self.0.lock();
        match g.sessions.get_mut(&id.0) {
            Some(s) => {
                s.released = true;
                SessionOutput {
                    text: s.output.clone(),
                    stripped: 0,
                }
            }
            None => SessionOutput {
                text: String::new(),
                stripped: 0,
            },
        }
    }

    fn live(&self) -> Vec<LiveSession> {
        self.0
            .lock()
            .sessions
            .values()
            .filter(|s| !s.released)
            .map(|s| LiveSession {
                pid: s.identity.pid,
                pgid: s.identity.pid,
            })
            .collect()
    }

    fn start_token(&self, pid: u32) -> Option<String> {
        self.0.lock().start_tokens.get(&pid).cloned()
    }

    fn terminate_orphan(&mut self, identity: &ProcessIdentity, _grace_ms: u64) -> SignalOutcome {
        let mut g = self.0.lock();
        if g.start_tokens.get(&identity.pid).map(String::as_str)
            != Some(identity.start_token.as_str())
        {
            return SignalOutcome::Refused("start time differs".to_string());
        }
        g.start_tokens.remove(&identity.pid);
        g.terminated.push(identity.clone());
        SignalOutcome::Sent
    }
}

// ---- worker files --------------------------------------------------------------------------

pub struct FakeContract(World);

impl ContractFiles for FakeContract {
    fn prepare(&self, worktree: &str, brief: &str) -> Result<()> {
        let mut g = self.0.lock();
        if g.fail_prepare_contract {
            return Err(FleetError::internal(".fleet is not writable"));
        }
        g.briefs.push((worktree.to_string(), brief.to_string()));
        g.questions.remove(worktree);
        g.results.remove(worktree);
        Ok(())
    }

    fn read_question(&self, worktree: &str) -> ContractRead<WorkerQuestion> {
        self.0
            .lock()
            .questions
            .get(worktree)
            .cloned()
            .unwrap_or(ContractRead::Absent)
    }

    fn read_result(&self, worktree: &str) -> ContractRead<WorkerResult> {
        self.0
            .lock()
            .results
            .get(worktree)
            .cloned()
            .unwrap_or(ContractRead::Absent)
    }

    fn question_present(&self, worktree: &str) -> bool {
        self.0.lock().questions.contains_key(worktree)
    }
}

// ---- jobs -------------------------------------------------------------------------------------

pub struct FakeJobs(World);

/// The completed form of a verification job. `verdict` decides each check from its id and from
/// whether it is the first mandatory check of its contract.
fn verify_outcome(
    job: &crate::ports::VerifyJob,
    verdict: impl Fn(&str, bool) -> (Verdict, String),
) -> crate::ports::VerifyDone {
    let results = job
        .targets
        .iter()
        .map(|t| {
            let first = t.contract.mandatory_checks().next().map(|c| c.id.clone());
            crate::ports::VerifiedContract {
                goal: t.goal.clone(),
                contract_id: t.contract_id.clone(),
                reports: t
                    .contract
                    .checks()
                    .iter()
                    .map(|c| {
                        let (v, output) = verdict(c.id.as_str(), first.as_ref() == Some(&c.id));
                        CheckReport {
                            check: c.id.clone(),
                            verdict: v,
                            exit_code: Some(if v == Verdict::Passed { 0 } else { 1 }),
                            summary: if v == Verdict::Passed {
                                "ok".to_string()
                            } else {
                                format!("{v}")
                            },
                            output,
                        }
                    })
                    .collect(),
            }
        })
        .collect();
    crate::ports::VerifyDone {
        run: job.run.clone(),
        task: job.task.clone(),
        attempt: job.attempt,
        origin: job.origin,
        commit: job.state.head.clone(),
        results,
    }
}

impl JobRunner for FakeJobs {
    fn start(&mut self, job: Job) -> Result<()> {
        let mut g = self.0.lock();
        if let Job::Command { approval, .. } = &job {
            let recorded = g.events.iter().any(|e| {
                matches!(&e.event, DomainEvent::ExternalCommandStarted { id } if id == approval)
            });
            if !recorded {
                return Err(FleetError::internal(
                    "a command was launched before its start was recorded",
                ));
            }
        }
        if let Job::Verify(v) = &job {
            let done = match g.verify_mode.clone() {
                Some(VerifyMode::Pass) => {
                    Some(verify_outcome(v, |_, _| (Verdict::Passed, String::new())))
                }
                Some(VerifyMode::Fail(text)) => Some(verify_outcome(v, |_, first| {
                    if first {
                        (Verdict::Failed, text.clone())
                    } else {
                        (Verdict::Passed, String::new())
                    }
                })),
                Some(VerifyMode::Manual) | None => None,
            };
            match done {
                Some(done) => g.jobs_done.push_back(JobDone::Verify(Box::new(done))),
                None => g.jobs_running += 1,
            }
            g.jobs_started.push(job);
            return Ok(());
        }
        if let Job::Gates {
            run,
            task,
            attempt,
            gates,
            ..
        } = &job
        {
            let outcomes: Option<Vec<GateOutcome>> = match g.gate_mode.clone() {
                Some(GateMode::Pass) => Some(
                    gates
                        .iter()
                        .map(|s| GateOutcome {
                            name: s.name.clone(),
                            passed: true,
                            summary: "exit code 0".into(),
                            output: String::new(),
                        })
                        .collect(),
                ),
                Some(GateMode::Fail(text)) => Some(
                    gates
                        .iter()
                        .enumerate()
                        .map(|(i, s)| GateOutcome {
                            name: s.name.clone(),
                            passed: i != 0,
                            summary: if i == 0 {
                                "exit code 1".into()
                            } else {
                                "exit code 0".into()
                            },
                            output: if i == 0 { text.clone() } else { String::new() },
                        })
                        .collect(),
                ),
                Some(GateMode::Manual) | None => None,
            };
            if let Some(outcomes) = outcomes {
                g.jobs_done.push_back(JobDone::Gates {
                    run: run.clone(),
                    task: task.clone(),
                    attempt: *attempt,
                    outcomes,
                });
            } else {
                g.jobs_running += 1;
            }
        } else {
            g.jobs_running += 1;
        }
        g.jobs_started.push(job);
        Ok(())
    }

    fn poll(&mut self) -> Vec<JobDone> {
        self.0.lock().jobs_done.drain(..).collect()
    }

    fn running(&self) -> usize {
        self.0.lock().jobs_running
    }

    fn live(&self) -> Vec<LiveSession> {
        self.0.lock().job_groups.clone()
    }
}

// ---- logs and undo ----------------------------------------------------------------------------

pub struct MemoryLogs(World);

impl OutputLogs for MemoryLogs {
    fn save(&mut self, run: &RunId, task: &TaskId, attempt: u32, text: &str) -> Result<()> {
        self.0.lock().logs.insert(
            (run.to_string(), task.to_string()),
            (attempt, text.to_string()),
        );
        Ok(())
    }

    fn tail(&self, run: &RunId, task: &TaskId, max: usize) -> Option<String> {
        self.0
            .lock()
            .logs
            .get(&(run.to_string(), task.to_string()))
            .map(|(_, t)| fleet_domain::sanitize::tail_str(t, max).to_string())
    }

    fn remove(&mut self, run: &RunId, task: &TaskId) {
        self.0
            .lock()
            .logs
            .remove(&(run.to_string(), task.to_string()));
    }
}

pub struct FakeUndo(World);

impl UndoTool for FakeUndo {
    fn available(&self) -> bool {
        self.0.lock().undo_available
    }

    fn checkpoint(&self, worktree: &str, label: &str) -> Result<String> {
        let mut g = self.0.lock();
        if g.undo_fails {
            return Err(FleetError::internal("agent-undo failed"));
        }
        g.undo_calls.push(format!("checkpoint {worktree} {label}"));
        Ok(format!("checkpoint {}", g.undo_calls.len()))
    }

    fn undo(&self, worktree: &str, apply: bool) -> Result<String> {
        let mut g = self.0.lock();
        if g.undo_fails {
            return Err(FleetError::internal("agent-undo failed"));
        }
        g.undo_calls.push(format!("undo {worktree} apply={apply}"));
        Ok(format!("undo of {worktree} (apply={apply})"))
    }
}

// ---- a ready-made orchestrator --------------------------------------------------------------

/// Configuration with one worker named `agent`, two retries and no gates.
pub fn test_config() -> AppConfig {
    let mut cfg = AppConfig::new(vec![
        ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ("HOME".to_string(), "/home/test".to_string()),
        ("SENTINEL_TOKEN".to_string(), "daemon-secret".to_string()),
    ]);
    let name = WorkerName::parse("agent").unwrap();
    cfg.workers.insert(
        name.clone(),
        WorkerDef {
            name: name.clone(),
            command: "run-agent".to_string(),
            args: vec![
                "--task".into(),
                "{task}".into(),
                "--dir".into(),
                "{worktree}".into(),
                "{brief}".into(),
            ],
            env_pass: vec![],
            env_set: vec![("AGENT_MODE".to_string(), "test".to_string())],
            timeout_ms: Some(60_000),
            idle_timeout_ms: Some(30_000),
        },
    );
    cfg.default_worker = Some(name);
    cfg.max_sessions = 4;
    cfg.max_retries = 2;
    cfg.kill_grace_ms = 1000;
    cfg.question_grace_ms = 500;
    cfg
}

/// An orchestrator wired to a fresh [`World`].
pub struct Harness {
    pub orch: Orchestrator,
    pub world: World,
}

impl Harness {
    pub fn new() -> Harness {
        Harness::with_config(test_config())
    }

    pub fn with_config(cfg: AppConfig) -> Harness {
        let world = World::new();
        let mut orch = Orchestrator::new(cfg, world.ports()).unwrap();
        orch.startup(4242, "test").unwrap();
        Harness { orch, world }
    }

    /// Restarts the orchestrator on the same world (a daemon restart): state is rebuilt from the
    /// stored events and live sessions of the old instance are forgotten.
    pub fn restart(&mut self, cfg: AppConfig) -> Result<()> {
        let mut orch = Orchestrator::new(cfg, self.world.ports())?;
        orch.startup(4343, "test")?;
        self.orch = orch;
        Ok(())
    }

    pub fn call(
        &mut self,
        cmd: &str,
        args: fleet_domain::value::Value,
    ) -> Result<crate::api::Reply> {
        let command = crate::api::parse_command(cmd, &args)?;
        self.orch.handle(command)
    }
}

impl Default for Harness {
    fn default() -> Self {
        Harness::new()
    }
}
