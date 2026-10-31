//! Ports: everything the application layer needs from the outside world.
//!
//! The application layer calls these traits and never touches a file, a process, git or a clock
//! directly. Adapters (fleet-infra) implement them for real; `fleet_app::testing` has in-memory
//! fakes for tests. Every port is `Send` so the orchestrator can live on its own thread.

use fleet_domain::approval::Digest;
use fleet_domain::contract::{ContractRead, WorkerQuestion, WorkerResult};
use fleet_domain::error::Result;
use fleet_domain::events::DomainEvent;
use fleet_domain::ids::MessageId;
use fleet_domain::ids::{ApprovalId, GoalId, RunId, Sha, TaskId};
use fleet_domain::msg::inbox::InboxItem;
use fleet_domain::msg::message::OutboxMessage;
use fleet_domain::output::Chunk;
use fleet_domain::quality::{GateFormat, GateOutcome};
use fleet_domain::status::BlockReason;
use fleet_domain::supervision::{LiveSession, ProcessIdentity};
use neo_verify::contract::{ContractId, VerificationContract};
use neo_verify::judge::CheckReport;

// ---- time -------------------------------------------------------------------------------

pub trait Clock: Send {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;
}

// ---- event store ------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredEvent {
    pub seq: u64,
    pub ts_ms: u64,
    pub event: DomainEvent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ReplayInfo {
    pub records: u64,
}

/// Append-only, ordered, durable record of domain events.
pub trait EventStore: Send {
    /// Appends `events` as one atomic batch: either all are recorded or none. Returns the
    /// sequence number of the last event of the batch (the current head when the batch is empty).
    fn append(&mut self, ts_ms: u64, events: &[DomainEvent]) -> Result<u64>;
    /// Visits every stored event in order. A damaged store is an error (`Tampered`).
    fn replay(&self, visit: &mut dyn FnMut(StoredEvent) -> Result<()>) -> Result<ReplayInfo>;
}

// ---- version control ----------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseCommit {
    /// The name the user gave (`HEAD`, `main`, ...).
    pub ref_name: String,
    pub sha: Sha,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged {
        sha: Sha,
    },
    /// The merge was attempted, found conflicts and was aborted; the target is unchanged.
    Conflict {
        files: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrationMerge {
    pub repo: String,
    pub run: RunId,
    pub task: TaskId,
    pub base: Sha,
    pub integration_branch: String,
    pub task_branch: String,
    /// The commit of the task branch that was read, gated and (when required) approved. The
    /// adapter merges exactly this commit and fails with a conflict when the branch no longer
    /// points at it, so a branch that moved after the approval is never merged.
    pub head: Sha,
    pub message: String,
    pub identity: Identity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrationResult {
    pub outcome: MergeOutcome,
    /// The integration branch did not exist before this call.
    pub created: bool,
}

/// What a task's worktree looks like right now, for verification. Verification is about the
/// *commit*: what is merged is the branch, never the working state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeState {
    /// The commit checked out in the worktree.
    pub head: Sha,
    /// The tree object of `head`.
    pub tree: Sha,
    /// Paths the commits since the run's base commit changed (`base..head`), relative to the
    /// repository root, at most `neo_verify::limits::MAX_CHANGED_PATHS` of them. Paths under
    /// `.fleet/` are listed like any other: a branch that carries one is not within any scope that
    /// does not allow it.
    pub changed: Vec<String>,
    /// There were more changed paths than `changed` lists.
    pub truncated: bool,
    /// The worktree has changes that are not in `head` (staged, modified or untracked files that
    /// are not ignored, apart from fleet's own `.fleet` directory).
    pub dirty: bool,
}

/// Version control. Every operation happens on fleet's own branches and worktrees; the user's
/// checked-out branch and working tree are never touched.
pub trait Vcs: Send {
    /// Resolves a ref (`HEAD` or a branch name) of `repo` to a commit.
    fn resolve_base(&self, repo: &str, base_ref: &str) -> Result<BaseCommit>;
    /// Creates (or reuses) the worktree and branch of a task, starting at `base`. Returns the
    /// worktree path. Idempotent.
    fn prepare_worktree(
        &self,
        repo: &str,
        run: &RunId,
        task: &TaskId,
        branch: &str,
        base: &Sha,
    ) -> Result<String>;
    /// Merges `branches` into the branch checked out in `worktree` (dependency work).
    fn merge_into_worktree(
        &self,
        worktree: &str,
        branches: &[String],
        identity: &Identity,
    ) -> Result<MergeOutcome>;
    /// Commits everything in the worktree (except fleet's own `.fleet` directory). `None` when
    /// there was nothing to commit.
    fn commit_all(&self, worktree: &str, message: &str, identity: &Identity)
        -> Result<Option<Sha>>;
    /// The commit a branch points at.
    fn branch_head(&self, repo: &str, branch: &str) -> Result<Sha>;
    /// The tree object of `commit`, read from the repository itself (never through a worktree),
    /// ignoring replacement objects: what a merge of that commit brings in.
    fn commit_tree(&self, repo: &str, commit: &Sha) -> Result<Sha>;
    /// Merges a task branch into the run's integration branch, creating that branch first when
    /// needed. A conflict aborts the merge and leaves the integration branch as it was.
    fn merge_into_integration(&self, req: &IntegrationMerge) -> Result<IntegrationResult>;
    /// Removes a worktree (and its branch when `delete_branch`). A worktree path that is empty or
    /// already gone is not an error, so a branch can be deleted after its worktree was cleaned.
    fn remove_worktree(
        &self,
        repo: &str,
        worktree: &str,
        branch: &str,
        delete_branch: bool,
    ) -> Result<()>;
    /// True when `branch` is contained in `into`.
    fn is_merged(&self, repo: &str, branch: &str, into: &str) -> Result<bool>;
    /// The commit the task's branch points at, its tree, the paths it changed since `base` and
    /// whether the worktree has uncommitted changes. Commit and tree are read from the
    /// repository (the branch the worktree has checked out), not from whatever the worktree's own
    /// git directory says; a worktree that does not belong to the repository, or is not on its
    /// task's branch, is refused.
    fn worktree_state(&self, worktree: &str, base: &Sha) -> Result<WorktreeState>;
}

// ---- sessions -----------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSpec {
    pub run: RunId,
    pub task: TaskId,
    pub attempt: u32,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    /// The complete environment of the process; nothing else is inherited.
    pub env: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spawned {
    pub id: SessionId,
    pub identity: ProcessIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionState {
    Running,
    Exited {
        code: Option<i64>,
        signal: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionReading {
    pub elapsed_ms: u64,
    /// Time since the session last produced output.
    pub idle_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignalOutcome {
    Sent,
    /// Nothing is running there any more.
    Gone,
    /// The process behind the pid is not the one that was started; nothing was signalled.
    Refused(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionOutput {
    /// Everything retained in the output ring (sanitized, redacted).
    pub text: String,
    pub stripped: u64,
}

/// Pty sessions with their own process group and a bounded output buffer.
pub trait SessionHost: Send {
    fn spawn(&mut self, spec: &SessionSpec) -> Result<Spawned>;
    /// Non-blocking. Reaps the child once it ended; keeps answering `Exited` afterwards.
    fn poll(&mut self, id: SessionId) -> SessionState;
    fn reading(&self, id: SessionId) -> Option<SessionReading>;
    /// Signals the whole process group after verifying the process is still the one started.
    fn signal(&mut self, id: SessionId, signal: Signal) -> SignalOutcome;
    /// Last `max` bytes of output, including an unterminated line.
    fn snapshot(&self, id: SessionId, max: usize) -> Option<String>;
    /// Complete lines after `offset`.
    fn read_since(&self, id: SessionId, offset: u64, max: usize) -> Option<Chunk>;
    /// After the leader exited: stop stragglers in its group, finish the output, free resources.
    fn release(&mut self, id: SessionId) -> SessionOutput;
    /// Live sessions, for the worker-origin check of the control socket.
    fn live(&self) -> Vec<LiveSession>;
    /// Start token of whatever currently runs under `pid` (`None` when nothing does).
    fn start_token(&self, pid: u32) -> Option<String>;
    /// Terminates the process group of a process recorded by an earlier daemon run. Verifies
    /// `identity` first; waits up to `grace_ms` between TERM and KILL.
    fn terminate_orphan(&mut self, identity: &ProcessIdentity, grace_ms: u64) -> SignalOutcome;
    /// Types one line (and a newline) into the session's terminal, for the optional
    /// `notify_input` notice. Never blocks: when the worker does not read its input the line is
    /// dropped and the answer is an error. The caller passes a fixed sentence and an id, never
    /// text a peer wrote.
    fn notify(&mut self, id: SessionId, line: &str) -> Result<()>;
}

// ---- worker contract files ------------------------------------------------------------------

pub trait ContractFiles: Send {
    /// Creates `.fleet/` in the worktree, writes the brief and removes stale contract files.
    fn prepare(&self, worktree: &str, brief: &str) -> Result<()>;
    fn read_question(&self, worktree: &str) -> ContractRead<WorkerQuestion>;
    fn read_result(&self, worktree: &str) -> ContractRead<WorkerResult>;
    /// Cheap check used while the worker runs: does a question file exist?
    fn question_present(&self, worktree: &str) -> bool;
}

// ---- messages between agents ------------------------------------------------------------------

/// What one file of a worker's outbox turned out to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutboxRead {
    /// A well-formed message (shape checked only: the board decides everything else).
    Valid(OutboxMessage),
    /// Not a message; the reason and a text that is safe to record.
    Invalid(BlockReason, String),
    /// Not finished: it parses badly and was written moments ago. Looked at again later.
    NotReady,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxItem {
    /// The file's name, `<n>.json`.
    pub name: String,
    pub read: OutboxRead,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutboxScan {
    pub items: Vec<OutboxItem>,
    /// The outbox could not be opened safely (a link, a file where a directory belongs): nothing
    /// was read. The text is safe to show.
    pub refused: Option<String>,
}

/// Reads what workers write to `.fleet/outbox/`. Everything below the worktree is the worker's:
/// the reader never follows a link, never waits on a FIFO and reads at most a fixed number of
/// bytes of at most a fixed number of files.
pub trait OutboxReader: Send {
    /// At most `max` pending files of the worktree's outbox, in numeric order of their names.
    fn scan(&self, worktree: &str, max: usize) -> OutboxScan;
    /// Marks a file as dealt with so it is not read again: an accepted message's file is
    /// removed, a refused one is renamed `<n>.rejected`.
    fn settle(&self, worktree: &str, name: &str, accepted: bool) -> Result<()>;
}

/// What a write to an inbox did, for the audit and the operator.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InboxReport {
    /// Messages whose file was written or rewritten.
    pub written: Vec<MessageId>,
    /// Files in the inbox that the daemon did not write, removed. A worker made them (or a
    /// delivery was altered): they look like deliveries and are not.
    pub removed: Vec<String>,
}

/// Writes the inbox of a worker: `.fleet/inbox/<message id>.json` and the `index.json` that lists
/// the real ones with their digests. Only the daemon writes there.
pub trait InboxWriter: Send {
    /// Makes `.fleet/outbox/` and `.fleet/inbox/` exist (replacing whatever else is there: the
    /// worker is not running), empties the outbox of an earlier attempt and brings the inbox in
    /// line with `items`.
    fn prepare(&self, worktree: &str, items: &[InboxItem]) -> Result<InboxReport>;
    /// Brings the inbox of a *running* worker in line with `items`, the complete list of what
    /// was delivered: every file present with exactly its content, `index.json` rewritten, every
    /// file the daemon did not write removed. A `.fleet` or `inbox` that is a link, or anything
    /// that is not what it should be, is an error and nothing is written (fail closed).
    fn sync(&self, worktree: &str, items: &[InboxItem]) -> Result<InboxReport>;
}

/// Redaction of text that is stored or delivered: the configured rules, literals and values of
/// secret-looking variables of the daemon.
pub trait TextRedactor: Send {
    fn redact(&self, text: &str) -> String;
}

// ---- background jobs ------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateSpec {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub format: GateFormat,
    pub timeout_ms: u64,
}

/// One goal's contract to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyTarget {
    pub goal: GoalId,
    pub contract_id: ContractId,
    pub contract: VerificationContract,
}

/// Why a contract is being run: as part of reviewing a task (its result decides whether the task
/// may complete) or because an operator asked (`fleet verify`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyOrigin {
    Review,
    Manual,
}

/// Runs the contracts of goals, independently of the worker, in the task's worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyJob {
    pub run: RunId,
    pub task: TaskId,
    pub attempt: u32,
    pub origin: VerifyOrigin,
    pub worktree: String,
    /// What the worktree looked like when the job was started: the commit the checks run on (a
    /// clean export of it, not the worktree), what that commit changed (for diff constraints) and
    /// whether the worktree had uncommitted changes.
    pub state: WorktreeState,
    /// The environment of every command before the adapter isolates it: the operator's allowlist
    /// and nothing else. The adapter replaces the home and temporary directories, the working
    /// directory and everything that points into the worktree.
    pub env: Vec<(String, String)>,
    pub targets: Vec<VerifyTarget>,
}

/// What running one goal's contract produced: a report per check, in contract order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedContract {
    pub goal: GoalId,
    pub contract_id: ContractId,
    pub reports: Vec<CheckReport>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyDone {
    pub run: RunId,
    pub task: TaskId,
    pub attempt: u32,
    pub origin: VerifyOrigin,
    /// The commit that was exported and checked, its tree and whether the worktree had
    /// uncommitted changes when the run started (the job's [`WorktreeState`]).
    pub commit: Sha,
    pub tree: Sha,
    pub dirty: bool,
    pub results: Vec<VerifiedContract>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Job {
    Gates {
        run: RunId,
        task: TaskId,
        attempt: u32,
        worktree: String,
        gates: Vec<GateSpec>,
    },
    /// A command a human approved on behalf of a worker.
    Command {
        approval: ApprovalId,
        worktree: String,
        program: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        timeout_ms: u64,
    },
    /// The contracts of the goals a task works on.
    Verify(Box<VerifyJob>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobDone {
    Gates {
        run: RunId,
        task: TaskId,
        attempt: u32,
        outcomes: Vec<GateOutcome>,
    },
    Command {
        approval: ApprovalId,
        exit_code: Option<i64>,
        summary: String,
        output: String,
    },
    Verify(Box<VerifyDone>),
}

/// Runs long commands off the orchestrator's thread.
pub trait JobRunner: Send {
    fn start(&mut self, job: Job) -> Result<()>;
    /// Results that arrived since the last call.
    fn poll(&mut self) -> Vec<JobDone>;
    fn running(&self) -> usize;
    /// The process groups of the children running jobs have started. They run code a worker
    /// wrote (gates, approved commands, verification checks): the origin check of the control
    /// socket treats them, and what they start, like workers.
    fn live(&self) -> Vec<LiveSession>;
}

// ---- stored output ------------------------------------------------------------------------------

pub trait OutputLogs: Send {
    fn save(&mut self, run: &RunId, task: &TaskId, attempt: u32, text: &str) -> Result<()>;
    /// Last `max` bytes of the most recent attempt's output.
    fn tail(&self, run: &RunId, task: &TaskId, max: usize) -> Option<String>;
    fn remove(&mut self, run: &RunId, task: &TaskId);
}

// ---- optional undo tool --------------------------------------------------------------------------

pub trait UndoTool: Send {
    fn available(&self) -> bool;
    /// Records a checkpoint of the worktree; returns a short description.
    fn checkpoint(&self, worktree: &str, label: &str) -> Result<String>;
    /// Previews (`apply == false`) or applies reverting the worktree's last checkpoint; returns
    /// the tool's report.
    fn undo(&self, worktree: &str, apply: bool) -> Result<String>;
}

// ---- event publication --------------------------------------------------------------------------

/// Receives events after they were stored and applied.
pub trait EventPublisher: Send {
    fn publish(&mut self, events: &[StoredEvent]);
}

/// Hashing for approval digests and the audit chain; `Send` version of the domain port.
pub trait SendDigest: Digest + Send {}
impl<T: Digest + Send> SendDigest for T {}

/// All ports of the orchestrator.
pub struct Ports {
    pub clock: Box<dyn Clock>,
    pub store: Box<dyn EventStore>,
    pub vcs: Box<dyn Vcs>,
    pub sessions: Box<dyn SessionHost>,
    pub contract: Box<dyn ContractFiles>,
    pub outbox: Box<dyn OutboxReader>,
    pub inbox: Box<dyn InboxWriter>,
    pub redact: Box<dyn TextRedactor>,
    pub jobs: Box<dyn JobRunner>,
    pub logs: Box<dyn OutputLogs>,
    pub undo: Box<dyn UndoTool>,
    pub digest: Box<dyn SendDigest>,
    pub publisher: Box<dyn EventPublisher>,
}
