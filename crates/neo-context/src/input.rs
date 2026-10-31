//! What a pack is compiled from: plain data the application layer collects from the fleet state,
//! the goal ledger, the repository at a recorded commit, the event log and the configuration.
//!
//! Nothing here knows where a value came from. What matters is *who wrote it*: the fields of
//! [`PackInput`] are grouped so that text an agent or a tool wrote can only be placed in
//! [`UnverifiedInput`] (and `HandoffPacket::summary`), which the compiler renders in layer 7 only,
//! labelled and quoted. Everything else is operator or fleet authored, or read from the repository
//! at a commit fleet recorded.

use crate::handoff::HandoffPacket;

/// Which task a pack is for, and what it was compiled at. Shown in the manifest; no layer's
/// content depends on it, so layers 1 to 5 stay identical from one attempt to the next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subject {
    pub run: String,
    pub task: String,
    pub attempt: u32,
    /// The run's base commit.
    pub base: String,
    /// The commit of the task's last committed work, when it has one.
    pub head: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlossaryEntry {
    pub term: String,
    pub meaning: String,
}

/// Layer 2: the objective of the run, the operator's constraints and the glossary.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CharterInput {
    pub objective: String,
    pub constraints: Vec<String>,
    pub glossary: Vec<GlossaryEntry>,
}

/// One goal of the ledger that the task works on, with the goals above it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoalLine {
    pub id: String,
    pub title: String,
    pub status: String,
    pub intent: String,
    /// Ids and titles of the goals above it, the root first.
    pub ancestors: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskLine {
    pub id: String,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependencyLine {
    pub id: String,
    pub title: String,
    pub done: bool,
}

/// Layer 3: where the task sits.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PlanInput {
    pub goals: Vec<GoalLine>,
    /// Completed tasks of the run, in plan order. Their ids and titles only: what a worker said
    /// about them is not here.
    pub done: Vec<TaskLine>,
    /// Tasks of the run that are not completed (this one included), in plan order.
    pub remaining: Vec<TaskLine>,
    pub dependencies: Vec<DependencyLine>,
}

/// One check of a contract that governs the task, described for the agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckLine {
    pub id: String,
    pub mandatory: bool,
    /// `command`, `refcheck-json`, `file` or `diff`.
    pub kind: String,
    /// What the check does, in one line (program and arguments, the file assertion, the globs).
    pub what: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractLines {
    pub goal: String,
    /// The first 12 digits of the contract id.
    pub contract: String,
    pub checks: Vec<CheckLine>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckState {
    NotRun,
    Passed,
    Failed,
    Error,
}

impl CheckState {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckState::NotRun => "not run yet",
            CheckState::Passed => "passed",
            CheckState::Failed => "failed",
            CheckState::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Option<CheckState> {
        [
            CheckState::NotRun,
            CheckState::Passed,
            CheckState::Failed,
            CheckState::Error,
        ]
        .into_iter()
        .find(|c| c.as_str() == s)
    }

    pub fn is_passed(self) -> bool {
        self == CheckState::Passed
    }
}

/// What fleet's own runs of a check say right now (a fact fleet observed, not a statement).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationLine {
    pub goal: String,
    pub check: String,
    pub mandatory: bool,
    pub state: CheckState,
    /// The commit the newest counting evidence is about, shortened.
    pub commit: Option<String>,
    pub evidence: Option<String>,
}

/// Layer 4: the task and how it will be verified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskInput {
    pub id: String,
    pub title: String,
    pub spec: String,
    pub contracts: Vec<ContractLines>,
    pub verification: Vec<VerificationLine>,
}

/// Layer 5: one fact of the static knowledge file. A fact without a source is not a fact: the
/// compiler refuses it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fact {
    pub id: String,
    pub claim: String,
    /// Where it comes from: a file and commit, a command, a URL and date, or a person.
    pub source: String,
    /// Optional note about how recent or how well checked it is.
    pub freshness: String,
}

/// Where a file excerpt was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    /// The task's own committed diff changed it.
    Changed,
    /// A path the task's contract allows.
    Allowed,
}

/// A file as the repository has it at one commit, as far as the adapter read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Excerpt {
    pub path: String,
    /// The repository's own id of the content (the git blob id).
    pub blob: String,
    /// Size of the whole file.
    pub size: u64,
    /// The first bytes of the file, at most `limits::MAX_SCAN_BYTES`. Everything in them is scanned
    /// for secrets; only the first `MAX_EXCERPT_BYTES` are shown.
    pub bytes: Vec<u8>,
    pub selection: Selection,
}

/// Layer 6: files as they were at the run's base commit (before any agent touched them).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct WorkingSetInput {
    pub files: Vec<Excerpt>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventLine {
    pub seq: u64,
    /// Fleet's own one-line description of the event: ids and kinds, never worker text.
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteInput {
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependencySummary {
    pub task: String,
    pub title: String,
    pub summary: String,
}

/// A path the task's committed work changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedPath {
    pub path: String,
}

/// A piece of the output of one of fleet's own runs of a check. The verdict is fleet's; the
/// output is whatever the checked code printed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidenceExcerpt {
    pub id: String,
    pub goal: String,
    pub check: String,
    pub verdict: String,
    pub summary: String,
    pub output: String,
}

/// Everything an agent or a tool wrote, or that the code of the task printed. Layer 7 only.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct UnverifiedInput {
    /// Notes of earlier attempts: fleet's statements mixed with quoted worker output and operator
    /// answers.
    pub notes: Vec<NoteInput>,
    pub dependency_summaries: Vec<DependencySummary>,
    /// Paths the task's committed diff changed, and how many there were in all.
    pub changed: Vec<ChangedPath>,
    pub changed_total: u64,
    /// The committed versions of changed files (the worker's own work).
    pub work: Vec<Excerpt>,
    pub evidence: Vec<EvidenceExcerpt>,
}

/// Everything one compile needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackInput {
    pub subject: Subject,
    pub charter: CharterInput,
    pub plan: PlanInput,
    pub task: TaskInput,
    pub knowledge: Vec<Fact>,
    pub working_set: WorkingSetInput,
    /// Oldest first.
    pub events: Vec<EventLine>,
    pub unverified: UnverifiedInput,
    /// The handoff the pack continues from, if the task has one.
    pub handoff: Option<HandoffPacket>,
}
