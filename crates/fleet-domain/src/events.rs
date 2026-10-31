//! Domain events: the facts the system records. State is rebuilt by applying them in order, and
//! the audit log is their serialized form, so the field names here are a stable format.

use crate::ids::{ApprovalId, DecisionId, RunId, Sha, TaskId, WorkerName};
use crate::status::{BlockKind, DecisionKind, EndKind, Purpose};
use crate::value::{Field, Value};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    MissingType,
    UnknownType(String),
    BadField(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::MissingType => write!(f, "event has no type"),
            DecodeError::UnknownType(t) => write!(f, "unknown event type {:?}", clip(t)),
            DecodeError::BadField(n) => {
                write!(f, "event field {:?} is missing or invalid", clip(n))
            }
        }
    }
}

impl std::error::Error for DecodeError {}

fn clip(s: &str) -> String {
    s.chars().take(40).collect()
}

static NULL: Value = Value::Null;

fn field<T: Field>(v: &Value, name: &str) -> Result<T, DecodeError> {
    T::from_value(v.get(name).unwrap_or(&NULL))
        .ok_or_else(|| DecodeError::BadField(name.to_string()))
}

macro_rules! define_events {
    ($($(#[$meta:meta])* $name:ident = $tag:literal { $($field:ident : $ty:ty),* $(,)? }),+ $(,)?) => {
        #[derive(Clone, Debug, PartialEq)]
        pub enum DomainEvent {
            $($(#[$meta])* $name { $($field: $ty),* }),+
        }

        impl DomainEvent {
            /// Every event type name, in declaration order.
            pub const KINDS: &'static [&'static str] = &[$($tag),+];

            /// The stable type name written to the log.
            pub fn kind(&self) -> &'static str {
                match self {
                    $(DomainEvent::$name { .. } => $tag),+
                }
            }

            pub fn to_value(&self) -> Value {
                match self {
                    $(DomainEvent::$name { $($field),* } => {
                        #[allow(unused_mut)]
                        let mut map = BTreeMap::new();
                        map.insert("type".to_string(), Value::str($tag));
                        $(map.insert(stringify!($field).to_string(), Field::to_value($field));)*
                        Value::Object(map)
                    }),+
                }
            }

            pub fn from_value(v: &Value) -> Result<DomainEvent, DecodeError> {
                let tag = v.get("type").and_then(Value::as_str).ok_or(DecodeError::MissingType)?;
                match tag {
                    $($tag => Ok(DomainEvent::$name { $($field: field(v, stringify!($field))?),* }),)+
                    other => Err(DecodeError::UnknownType(other.to_string())),
                }
            }
        }
    };
}

define_events! {
    /// The daemon began a session of the log.
    DaemonStarted = "daemon_started" { pid: u64, version: String },
    RunCreated = "run_created" {
        run: RunId, objective: String, repo: String, base_ref: String, base_sha: Sha, max_parallel: u32,
    },
    RunStarted = "run_started" { run: RunId, max_parallel: u32 },
    TaskAdded = "task_added" {
        run: RunId, task: TaskId, title: String, spec: String, deps: Vec<TaskId>, worker: Option<WorkerName>,
    },
    /// Pending to ready: every dependency is completed.
    TaskReady = "task_ready" { run: RunId, task: TaskId },
    TaskDispatched = "task_dispatched" {
        run: RunId, task: TaskId, attempt: u32, branch: String, worktree: String, pid: u64, start_token: String,
    },
    SessionEnded = "session_ended" {
        run: RunId, task: TaskId, attempt: u32, outcome: EndKind, exit_code: Option<i64>,
        output_tail: String, stripped: u64,
    },
    TaskCommitted = "task_committed" { run: RunId, task: TaskId, head: Sha },
    /// The worker finished; the task waits for its quality gates.
    TaskReviewing = "task_reviewing" { run: RunId, task: TaskId, summary: String },
    GateChecked = "gate_checked" {
        run: RunId, task: TaskId, attempt: u32, gate: String, passed: bool, summary: String, output_tail: String,
    },
    TaskCompleted = "task_completed" { run: RunId, task: TaskId },
    TaskFailed = "task_failed" { run: RunId, task: TaskId, reason: String },
    /// The task goes back to ready for another attempt.
    RetryQueued = "retry_queued" { run: RunId, task: TaskId, retries_used: u32, reason: String },
    /// An operator reset a failed task.
    RetryReset = "retry_reset" { run: RunId, task: TaskId },
    NoteAdded = "note_added" { run: RunId, task: TaskId, note: String },
    TaskBlocked = "task_blocked" {
        run: RunId, task: TaskId, kind: BlockKind, subject: String, detail: String,
    },
    TaskUnblocked = "task_unblocked" { run: RunId, task: TaskId },
    DecisionOpened = "decision_opened" {
        id: DecisionId, run: RunId, task: TaskId, kind: DecisionKind, text: String, choices: Vec<String>,
    },
    DecisionResolved = "decision_resolved" { id: DecisionId, answer: String },
    ApprovalRequested = "approval_requested" {
        id: ApprovalId, run: RunId, task: TaskId, purpose: Purpose, tool: String, command: String,
        args: Vec<String>, digest: String, requested_ms: u64, expires_ms: u64,
    },
    ApprovalDecided = "approval_decided" { id: ApprovalId, approved: bool },
    ApprovalExpired = "approval_expired" { id: ApprovalId },
    ApprovalConsumed = "approval_consumed" { id: ApprovalId },
    ExternalCommandRan = "external_command_ran" {
        id: ApprovalId, exit_code: Option<i64>, summary: String, output_tail: String,
    },
    TaskMerged = "task_merged" { run: RunId, task: TaskId, merge_sha: Sha },
    MergeConflicted = "merge_conflicted" { run: RunId, task: TaskId, files: Vec<String> },
    IntegrationCreated = "integration_created" { run: RunId },
    CheckpointRecorded = "checkpoint_recorded" { run: RunId, task: TaskId, label: String, info: String },
    OrphanReconciled = "orphan_reconciled" { run: RunId, task: TaskId, action: String },
    WorktreeRemoved = "worktree_removed" { run: RunId, task: TaskId },
    Warning = "warning" { message: String },
}

impl DomainEvent {
    /// The run an event belongs to, when it names one.
    pub fn run(&self) -> Option<&RunId> {
        use DomainEvent::*;
        match self {
            RunCreated { run, .. }
            | RunStarted { run, .. }
            | TaskAdded { run, .. }
            | TaskReady { run, .. }
            | TaskDispatched { run, .. }
            | SessionEnded { run, .. }
            | TaskCommitted { run, .. }
            | TaskReviewing { run, .. }
            | GateChecked { run, .. }
            | TaskCompleted { run, .. }
            | TaskFailed { run, .. }
            | RetryQueued { run, .. }
            | RetryReset { run, .. }
            | NoteAdded { run, .. }
            | TaskBlocked { run, .. }
            | TaskUnblocked { run, .. }
            | DecisionOpened { run, .. }
            | ApprovalRequested { run, .. }
            | TaskMerged { run, .. }
            | MergeConflicted { run, .. }
            | IntegrationCreated { run }
            | CheckpointRecorded { run, .. }
            | OrphanReconciled { run, .. }
            | WorktreeRemoved { run, .. } => Some(run),
            DaemonStarted { .. }
            | DecisionResolved { .. }
            | ApprovalDecided { .. }
            | ApprovalExpired { .. }
            | ApprovalConsumed { .. }
            | ExternalCommandRan { .. }
            | Warning { .. } => None,
        }
    }

    /// One line for activity feeds. Contains no worker-controlled text beyond ids.
    pub fn headline(&self) -> String {
        use DomainEvent::*;
        match self {
            DaemonStarted { pid, .. } => format!("daemon started (pid {pid})"),
            RunCreated { run, .. } => format!("run {run} created"),
            RunStarted { run, max_parallel } => {
                format!("run {run} started (max parallel {max_parallel})")
            }
            TaskAdded { run, task, .. } => format!("{run}/{task} added"),
            TaskReady { run, task } => format!("{run}/{task} ready"),
            TaskDispatched {
                run, task, attempt, ..
            } => format!("{run}/{task} dispatched (attempt {attempt})"),
            SessionEnded {
                run, task, outcome, ..
            } => format!("{run}/{task} session {outcome}"),
            TaskCommitted { run, task, .. } => format!("{run}/{task} committed"),
            TaskReviewing { run, task, .. } => format!("{run}/{task} in review"),
            GateChecked {
                run,
                task,
                gate,
                passed,
                ..
            } => {
                format!(
                    "{run}/{task} gate {gate} {}",
                    if *passed { "passed" } else { "failed" }
                )
            }
            TaskCompleted { run, task } => format!("{run}/{task} completed"),
            TaskFailed { run, task, .. } => format!("{run}/{task} failed"),
            RetryQueued {
                run,
                task,
                retries_used,
                ..
            } => format!("{run}/{task} retry {retries_used} queued"),
            RetryReset { run, task } => format!("{run}/{task} retry reset"),
            NoteAdded { run, task, .. } => format!("{run}/{task} note added"),
            TaskBlocked {
                run, task, kind, ..
            } => format!("{run}/{task} blocked ({kind})"),
            TaskUnblocked { run, task } => format!("{run}/{task} unblocked"),
            DecisionOpened { id, run, task, .. } => format!("gate {id} opened for {run}/{task}"),
            DecisionResolved { id, .. } => format!("gate {id} resolved"),
            ApprovalRequested {
                id,
                run,
                task,
                purpose,
                ..
            } => {
                format!("approval {id} requested for {run}/{task} ({purpose})")
            }
            ApprovalDecided { id, approved } => {
                format!(
                    "approval {id} {}",
                    if *approved { "approved" } else { "denied" }
                )
            }
            ApprovalExpired { id } => format!("approval {id} expired"),
            ApprovalConsumed { id } => format!("approval {id} consumed"),
            ExternalCommandRan { id, .. } => format!("approval {id} command ran"),
            TaskMerged { run, task, .. } => format!("{run}/{task} merged"),
            MergeConflicted { run, task, .. } => format!("{run}/{task} merge conflict"),
            IntegrationCreated { run } => format!("run {run} integration branch created"),
            CheckpointRecorded {
                run, task, label, ..
            } => format!("{run}/{task} checkpoint {label}"),
            OrphanReconciled { run, task, action } => format!("{run}/{task} orphan: {action}"),
            WorktreeRemoved { run, task } => format!("{run}/{task} worktree removed"),
            Warning { .. } => "warning".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> RunId {
        RunId::parse("r1").unwrap()
    }
    fn task() -> TaskId {
        TaskId::parse("t1").unwrap()
    }

    #[test]
    fn kinds_are_unique_and_snake_case() {
        let mut seen = std::collections::BTreeSet::new();
        for k in DomainEvent::KINDS {
            assert!(seen.insert(*k), "duplicate {k}");
            assert!(k.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
        }
    }

    #[test]
    fn round_trip_through_value() {
        let events = vec![
            DomainEvent::DaemonStarted {
                pid: 42,
                version: "0.1.0".into(),
            },
            DomainEvent::TaskAdded {
                run: run(),
                task: task(),
                title: "a".into(),
                spec: "line1\nline2".into(),
                deps: vec![TaskId::parse("t0").unwrap()],
                worker: Some(WorkerName::parse("agent").unwrap()),
            },
            DomainEvent::SessionEnded {
                run: run(),
                task: task(),
                attempt: 2,
                outcome: EndKind::Signaled,
                exit_code: Some(-9),
                output_tail: "x".into(),
                stripped: 3,
            },
            DomainEvent::ApprovalRequested {
                id: ApprovalId::from_seq(1),
                run: run(),
                task: task(),
                purpose: Purpose::Merge,
                tool: "git".into(),
                command: "merge".into(),
                args: vec!["a".into(), "b".into()],
                digest: "ab".repeat(32),
                requested_ms: 10,
                expires_ms: 20,
            },
            DomainEvent::TaskMerged {
                run: run(),
                task: task(),
                merge_sha: Sha::parse(&"a".repeat(40)).unwrap(),
            },
            DomainEvent::Warning {
                message: "w".into(),
            },
        ];
        for e in events {
            assert_eq!(
                DomainEvent::from_value(&e.to_value()),
                Ok(e.clone()),
                "{}",
                e.kind()
            );
        }
    }

    #[test]
    fn decoding_rejects_damage() {
        let good = DomainEvent::TaskReady {
            run: run(),
            task: task(),
        }
        .to_value();
        assert!(DomainEvent::from_value(&good).is_ok());

        assert_eq!(
            DomainEvent::from_value(&Value::Null),
            Err(DecodeError::MissingType)
        );
        assert_eq!(
            DomainEvent::from_value(&Value::object([("type", Value::str("nope"))])),
            Err(DecodeError::UnknownType("nope".into()))
        );
        let missing = Value::object([
            ("type", Value::str("task_ready")),
            ("run", Value::str("r1")),
        ]);
        assert_eq!(
            DomainEvent::from_value(&missing),
            Err(DecodeError::BadField("task".into()))
        );
        let bad_id = Value::object([
            ("type", Value::str("task_ready")),
            ("run", Value::str("../../etc")),
            ("task", Value::str("t1")),
        ]);
        assert_eq!(
            DomainEvent::from_value(&bad_id),
            Err(DecodeError::BadField("run".into()))
        );
    }

    #[test]
    fn run_accessor_names_the_run() {
        assert_eq!(
            DomainEvent::TaskReady {
                run: run(),
                task: task()
            }
            .run(),
            Some(&run())
        );
        assert_eq!(
            DomainEvent::Warning {
                message: String::new()
            }
            .run(),
            None
        );
        assert_eq!(
            DomainEvent::DecisionResolved {
                id: DecisionId::from_seq(1),
                answer: String::new()
            }
            .run(),
            None
        );
    }

    #[test]
    fn headlines_do_not_echo_free_text() {
        let e = DomainEvent::TaskFailed {
            run: run(),
            task: task(),
            reason: "\u{1b}[31mevil".into(),
        };
        assert_eq!(e.headline(), "r1/t1 failed");
        let w = DomainEvent::Warning {
            message: "\u{1b}]0;x".into(),
        };
        assert_eq!(w.headline(), "warning");
    }
}
