//! Contract suites: the behaviour every implementation of a port must have.
//!
//! A suite is a plain function that takes a factory for the adapter under test and panics with a
//! message naming the violated rule. The in-memory fakes of [`World`](super::World) pass every
//! suite (`crates/fleet-app/tests/contracts_memory.rs`), and so do the real adapters
//! (`crates/fleet-infra/tests/contracts.rs`); a fake that drifts from the real thing, or the
//! other way around, fails here before it misleads an application test.
//!
//! | suite                    | port            | factory                                         |
//! |--------------------------|-----------------|-------------------------------------------------|
//! | [`event_store_contract`] | `EventStore`    | `&mut dyn FnMut() -> Box<dyn EventStore>`       |
//! | [`output_logs_contract`] | `OutputLogs`    | `&mut dyn FnMut() -> Box<dyn OutputLogs>`       |
//! | [`contract_files_contract`] | `ContractFiles` | `&mut dyn ContractHarness`                   |
//! | [`verify_jobs_contract`] | `JobRunner` (verification jobs) | `&mut dyn VerifyHarness`          |
//!
//! # Running a suite against an in-memory fake
//!
//! ```
//! use fleet_app::testing::{contracts, World};
//!
//! let world = World::new();
//! contracts::event_store_contract(&mut || world.ports().store);
//! contracts::output_logs_contract(&mut || world.ports().logs);
//! contracts::contract_files_contract(&mut contracts::MemoryContractHarness::new(world.clone()));
//! ```
//!
//! # Running a suite against a new adapter
//!
//! * **Event store and logs.** Write a factory closure. Every call must return a *new handle onto
//!   the same underlying storage*: the suites drop a handle and call the factory again to check
//!   that data survives reopening, so a factory that builds an empty store each time fails. Use
//!   a fresh temporary directory per suite run and capture its path in the closure.
//! * **Contract files.** Implement [`ContractHarness`]: `files` returns the adapter, `worktree`
//!   the worktree path the adapter is given, and `plant` makes a file of the given
//!   [`ContractFile`] exist in the shape described by [`Plant`], *replacing* whatever was there
//!   before (including a directory or a link). The suite builds worktree content only through
//!   the harness, so the harness decides how a symlink or a FIFO is made on its medium. Embed
//!   [`PLANT_MARKER`] in planted content that is not valid and use [`VALID_RESULT_JSON`] /
//!   [`VALID_QUESTION_JSON`] for `Plant::Valid`, so the suite can check that error texts do not
//!   echo worker-controlled content.
//!
//! Only behaviour shared by *all* implementations belongs here. Things that exist for one medium
//! only (the hash chain of the file log, symlink races, lock files, the 1 MiB retention of the
//! file logs) are tested in the adapter's own unit tests.

use super::World;
use crate::ports::{
    ContractFiles, EventStore, Job, JobDone, JobRunner, OutputLogs, ReplayInfo, StoredEvent,
    VerifyDone, VerifyJob, VerifyOrigin, VerifyTarget, WorktreeState,
};
use fleet_domain::contract::{
    ContractRead, QuestionKind, ResultStatus, WorkerQuestion, WorkerResult,
};
use fleet_domain::error::{ErrorCode, FleetError};
use fleet_domain::events::DomainEvent;
use fleet_domain::ids::{
    ApprovalId, DecisionId, EvidenceId, GoalId, RunId, Sha, TaskId, WorkerName,
};
use fleet_domain::status::{BlockKind, DecisionKind, EndKind, GoalStatus, Purpose, Verdict};
use fleet_domain::value::{Document, Value};
use std::fmt::Debug;

// ---- event store ----------------------------------------------------------------------------

fn run_id() -> RunId {
    RunId::parse("r1").unwrap()
}

fn task_id(name: &str) -> TaskId {
    TaskId::parse(name).unwrap()
}

fn sha(text: &str) -> Sha {
    Sha::parse(text).unwrap()
}

/// One event of every kind the domain defines, with awkward text where a field takes text.
fn sample_events() -> Vec<DomainEvent> {
    let run = run_id();
    let t1 = task_id("t1");
    let t2 = task_id("t2");
    let approval = ApprovalId::from_seq(7);
    let decision = DecisionId::from_seq(3);
    let tricky = "line one\nline \"two\" with \\ backslash\tand tab\n\
                  日本語 — ünïcödé 🚀 {\"type\":\"warning\"} </script>";
    vec![
        DomainEvent::DaemonStarted {
            pid: 4_294_967_296,
            version: "0.1.0".into(),
        },
        DomainEvent::RunCreated {
            run: run.clone(),
            objective: tricky.into(),
            repo: "/tmp/a repo/with spaces".into(),
            base_ref: "main".into(),
            base_sha: sha(&"a".repeat(40)),
            max_parallel: 3,
        },
        DomainEvent::RunStarted {
            run: run.clone(),
            max_parallel: 2,
        },
        DomainEvent::TaskAdded {
            run: run.clone(),
            task: t2.clone(),
            title: "Ünï title".into(),
            spec: tricky.into(),
            deps: vec![t1.clone()],
            worker: Some(WorkerName::parse("agent").unwrap()),
        },
        DomainEvent::TaskAdded {
            run: run.clone(),
            task: t1.clone(),
            title: "plain".into(),
            spec: String::new(),
            deps: vec![],
            worker: None,
        },
        DomainEvent::TaskReady {
            run: run.clone(),
            task: t1.clone(),
        },
        DomainEvent::TaskDispatched {
            run: run.clone(),
            task: t1.clone(),
            attempt: 1,
            branch: "fleet/r1/t1".into(),
            worktree: "/tmp/wt/r1/t1".into(),
            pid: 4242,
            start_token: "Mon Jan  1 00:00:00 2024".into(),
        },
        DomainEvent::SessionEnded {
            run: run.clone(),
            task: t1.clone(),
            attempt: 1,
            outcome: EndKind::Exited,
            exit_code: Some(-9),
            output_tail:
                "compiling\n\\x1b[31merror\\x1b[0m \\u{202e}hidden\\u{202c} \\x07 bell\nok 日本"
                    .into(),
            stripped: 3,
        },
        DomainEvent::SessionEnded {
            run: run.clone(),
            task: t2.clone(),
            attempt: 2,
            outcome: EndKind::Timeout,
            exit_code: None,
            output_tail: String::new(),
            stripped: 0,
        },
        DomainEvent::TaskCommitted {
            run: run.clone(),
            task: t1.clone(),
            head: sha(&"b".repeat(40)),
        },
        DomainEvent::TaskReviewing {
            run: run.clone(),
            task: t1.clone(),
            summary: "done it".into(),
        },
        DomainEvent::GateChecked {
            run: run.clone(),
            task: t1.clone(),
            attempt: 1,
            gate: "tests".into(),
            passed: false,
            summary: "exit code 1".into(),
            output_tail: tricky.into(),
        },
        DomainEvent::TaskCompleted {
            run: run.clone(),
            task: t1.clone(),
        },
        DomainEvent::TaskFailed {
            run: run.clone(),
            task: t2.clone(),
            reason: "gate \"tests\" failed".into(),
        },
        DomainEvent::RetryQueued {
            run: run.clone(),
            task: t2.clone(),
            retries_used: 1,
            reason: "again".into(),
        },
        DomainEvent::RetryReset {
            run: run.clone(),
            task: t2.clone(),
        },
        DomainEvent::NoteAdded {
            run: run.clone(),
            task: t1.clone(),
            note: tricky.into(),
        },
        DomainEvent::TaskBlocked {
            run: run.clone(),
            task: t1.clone(),
            kind: BlockKind::Question,
            subject: "g3".into(),
            detail: "Which database?".into(),
        },
        DomainEvent::TaskUnblocked {
            run: run.clone(),
            task: t1.clone(),
        },
        DomainEvent::DecisionOpened {
            id: decision.clone(),
            run: run.clone(),
            task: t1.clone(),
            kind: DecisionKind::Question,
            text: "Which database?".into(),
            choices: vec!["postgres".into(), "sqlite \"embedded\"".into()],
        },
        DomainEvent::DecisionResolved {
            id: decision,
            answer: tricky.into(),
        },
        DomainEvent::ApprovalRequested {
            id: approval.clone(),
            run: run.clone(),
            task: t1.clone(),
            purpose: Purpose::ExternalCommand,
            tool: "command".into(),
            command: "/usr/bin/git push \"origin\" main".into(),
            args: vec![
                "--flag".into(),
                "has space".into(),
                "quote\"d".into(),
                "ünï".into(),
                String::new(),
            ],
            digest: "ab".repeat(32),
            requested_ms: 1_700_000_000_000,
            expires_ms: 1_700_000_300_000,
        },
        DomainEvent::ApprovalDecided {
            id: approval.clone(),
            approved: true,
        },
        DomainEvent::ApprovalExpired {
            id: approval.clone(),
        },
        DomainEvent::ApprovalConsumed {
            id: approval.clone(),
        },
        DomainEvent::ExternalCommandStarted {
            id: approval.clone(),
        },
        DomainEvent::ExternalCommandRan {
            id: approval,
            exit_code: None,
            summary: "killed".into(),
            output_tail: tricky.into(),
        },
        DomainEvent::TaskMerged {
            run: run.clone(),
            task: t1.clone(),
            merge_sha: sha("0123456789abcdef0123456789abcdef01234567"),
        },
        DomainEvent::MergeConflicted {
            run: run.clone(),
            task: t2.clone(),
            files: vec!["src/a b.rs".into(), "日本.txt".into()],
        },
        DomainEvent::IntegrationCreated { run: run.clone() },
        DomainEvent::CheckpointRecorded {
            run: run.clone(),
            task: t1.clone(),
            label: "before".into(),
            info: tricky.into(),
        },
        DomainEvent::OrphanReconciled {
            run: run.clone(),
            task: t1.clone(),
            action: "terminated".into(),
        },
        DomainEvent::WorktreeRemoved {
            run: run.clone(),
            task: t1.clone(),
        },
        DomainEvent::Warning {
            message: tricky.into(),
        },
        DomainEvent::GoalAdded {
            goal: GoalId::parse("ship-it").unwrap(),
            parent: Some(GoalId::parse("root").unwrap()),
            title: "Ünï goal".into(),
            intent: tricky.into(),
            tasks: vec!["r1/t1".into(), "r1/t2".into()],
        },
        DomainEvent::GoalStatusChanged {
            goal: GoalId::parse("ship-it").unwrap(),
            from: GoalStatus::Open,
            to: GoalStatus::Verified,
            reason: tricky.into(),
        },
        DomainEvent::CriteriaAttached {
            goal: GoalId::parse("ship-it").unwrap(),
            contract_id: "ab".repeat(32),
            contract: Document(Value::object([
                ("version", Value::int(1)),
                (
                    "checks",
                    Value::array([Value::object([
                        ("id", Value::str("tests")),
                        ("note", Value::str(tricky)),
                    ])]),
                ),
            ])),
        },
        DomainEvent::EvidenceRecorded {
            id: EvidenceId::from_seq(1),
            goal: GoalId::parse("ship-it").unwrap(),
            run,
            task: t1,
            contract_id: "ab".repeat(32),
            check: "tests".into(),
            verdict: Verdict::Failed,
            exit_code: Some(-1),
            summary: tricky.into(),
            output: tricky.into(),
            commit: sha(&"c".repeat(40)),
            tree: sha(&"d".repeat(40)),
            dirty: true,
            at_ms: 4_294_967_296,
        },
        DomainEvent::IssueLinked {
            run: RunId::parse("r1").unwrap(),
            task: TaskId::parse("t1").unwrap(),
            repo: "github.com/o/r".into(),
            number: 7,
        },
        DomainEvent::PullRequestStarted {
            id: ApprovalId::from_seq(2),
            run: RunId::parse("r1").unwrap(),
            task: TaskId::parse("t1").unwrap(),
            repo: "github.com/o/r".into(),
            base: "main".into(),
            branch: "fleet/r1/t1".into(),
            head: sha(&"c".repeat(40)),
        },
        DomainEvent::PullRequestOpened {
            id: ApprovalId::from_seq(2),
            run: RunId::parse("r1").unwrap(),
            task: TaskId::parse("t1").unwrap(),
            repo: "github.com/o/r".into(),
            number: 12,
            url: tricky.into(),
            head: sha(&"c".repeat(40)),
        },
        DomainEvent::PullRequestFailed {
            id: ApprovalId::from_seq(2),
            run: RunId::parse("r1").unwrap(),
            task: TaskId::parse("t1").unwrap(),
            reason: tricky.into(),
        },
    ]
}

/// What the store must hold, kept in step with every append the suite makes.
#[derive(Default)]
struct Expected {
    events: Vec<StoredEvent>,
}

impl Expected {
    fn head(&self) -> u64 {
        u64::try_from(self.events.len()).unwrap()
    }

    fn append(&mut self, store: &mut dyn EventStore, ts_ms: u64, batch: &[DomainEvent]) {
        let returned = store.append(ts_ms, batch).unwrap_or_else(|e| {
            panic!(
                "event store contract: append of {} events failed: {e}",
                batch.len()
            )
        });
        for event in batch {
            let seq = self.head() + 1;
            self.events.push(StoredEvent {
                seq,
                ts_ms,
                event: event.clone(),
            });
        }
        assert_eq!(
            returned,
            self.head(),
            "event store contract: append must return the sequence number of the last event of the batch"
        );
    }

    fn check(&self, store: &dyn EventStore, when: &str) {
        let (got, info) = replay_all(store);
        assert_eq!(
            info.records,
            self.head(),
            "event store contract ({when}): ReplayInfo.records must be the number of events"
        );
        assert_eq!(
            got.len(),
            self.events.len(),
            "event store contract ({when}): wrong number of events replayed"
        );
        for (g, e) in got.iter().zip(&self.events) {
            assert_eq!(
                g, e,
                "event store contract ({when}): event {} differs after replay",
                e.seq
            );
        }
    }
}

fn replay_all(store: &dyn EventStore) -> (Vec<StoredEvent>, ReplayInfo) {
    let mut out = Vec::new();
    let info = store
        .replay(&mut |e| {
            out.push(e);
            Ok(())
        })
        .unwrap_or_else(|e| panic!("event store contract: replay failed: {e}"));
    (out, info)
}

/// Drops `old` before calling the factory, as a process restart would.
fn reopen<T: ?Sized>(open: &mut dyn FnMut() -> Box<T>, old: Box<T>) -> Box<T> {
    drop(old);
    open()
}

fn notes(count: usize) -> Vec<DomainEvent> {
    (0..count)
        .map(|i| DomainEvent::NoteAdded {
            run: run_id(),
            task: task_id("t1"),
            note: format!("note {i}"),
        })
        .collect()
}

/// Checks an [`EventStore`]. Every call of `open` must return a handle onto the same storage.
///
/// Rules: a new store is empty; `append` returns the sequence number of the last event of the
/// batch and numbers continue across batches and reopening; `replay` yields events in order with
/// consecutive sequence numbers starting at 1 and the timestamp given at append; an empty batch
/// is a no-op that returns the current head; every event kind of the domain survives storage;
/// an error from the visitor aborts the replay and is returned unchanged.
pub fn event_store_contract(open: &mut dyn FnMut() -> Box<dyn EventStore>) {
    let mut expected = Expected::default();
    let mut store = open();

    // 1. A new store is empty, and an empty batch on it is a no-op returning head 0.
    let (events, info) = replay_all(store.as_ref());
    assert!(
        events.is_empty(),
        "event store contract: a new store must replay no events"
    );
    assert_eq!(
        info.records, 0,
        "event store contract: a new store must report 0 records"
    );
    assert_eq!(
        store
            .append(10, &[])
            .unwrap_or_else(|e| panic!("event store contract: empty append failed: {e}")),
        0,
        "event store contract: an empty batch on an empty store returns head 0"
    );
    expected.check(store.as_ref(), "after an empty append on a new store");

    // 2. The first batch is numbered from 1 and returns its last sequence number.
    let first = [
        DomainEvent::DaemonStarted {
            pid: 1,
            version: "0.1.0".into(),
        },
        DomainEvent::RunStarted {
            run: run_id(),
            max_parallel: 2,
        },
        DomainEvent::Warning {
            message: "third".into(),
        },
    ];
    expected.append(store.as_mut(), 1_000, &first);
    expected.check(store.as_ref(), "after the first batch");
    assert_eq!(
        expected.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "event store contract: sequence numbers start at 1"
    );

    // 3. Sequence numbers continue across batches; each batch keeps its own timestamp.
    expected.append(
        store.as_mut(),
        2_000,
        &[DomainEvent::Warning {
            message: "fourth".into(),
        }],
    );
    expected.check(store.as_ref(), "after the second batch");
    let stamps: Vec<u64> = expected.events.iter().map(|e| e.ts_ms).collect();
    assert_eq!(
        stamps,
        vec![1_000, 1_000, 1_000, 2_000],
        "event store contract: timestamps are kept per batch"
    );

    // 4. An empty batch changes nothing and returns the current head.
    let head = store
        .append(9_999, &[])
        .unwrap_or_else(|e| panic!("event store contract: empty append failed: {e}"));
    assert_eq!(
        head,
        expected.head(),
        "event store contract: an empty batch returns the current head"
    );
    expected.check(store.as_ref(), "after an empty append");

    // 5. A larger batch is one unit: every event is there, in order, with one timestamp.
    expected.append(store.as_mut(), 3_000, &notes(40));
    expected.check(store.as_ref(), "after a batch of 40");
    assert_eq!(
        expected.head(),
        44,
        "event store contract: test bookkeeping"
    );

    // 6. Replay does not consume anything: a second replay gives the same events.
    expected.check(store.as_ref(), "on a second replay");

    // 7. Events survive reopening, and appending continues with the next sequence number.
    store = reopen(open, store);
    expected.check(store.as_ref(), "after reopening");
    expected.append(
        store.as_mut(),
        4_000,
        &[DomainEvent::Warning {
            message: "after reopen".into(),
        }],
    );
    expected.check(store.as_ref(), "after appending to a reopened store");
    store = reopen(open, store);
    expected.check(store.as_ref(), "after reopening a second time");

    // 8. Every kind of event the domain defines round-trips faithfully.
    let sample = sample_events();
    let mut kinds: Vec<&str> = sample.iter().map(DomainEvent::kind).collect();
    kinds.sort_unstable();
    kinds.dedup();
    let mut all: Vec<&str> = DomainEvent::KINDS.to_vec();
    all.sort_unstable();
    assert!(
        kinds.len() >= 12,
        "event store contract: the sample must cover at least 12 kinds"
    );
    assert_eq!(
        kinds, all,
        "event store contract: the sample must cover every event kind of the domain"
    );
    expected.append(store.as_mut(), 5_000, &sample);
    expected.check(store.as_ref(), "after storing every event kind");
    store = reopen(open, store);
    expected.check(
        store.as_ref(),
        "after reopening with every event kind stored",
    );

    // 9. A visitor error aborts the replay and comes back unchanged; the store stays usable.
    let mut visited = 0;
    let err = store
        .replay(&mut |_| {
            visited += 1;
            if visited == 2 {
                Err(FleetError::conflict("stop here"))
            } else {
                Ok(())
            }
        })
        .expect_err("event store contract: an error from the visitor must be returned by replay");
    assert_eq!(
        visited, 2,
        "event store contract: replay must stop at the first visitor error"
    );
    assert_eq!(
        err.code,
        ErrorCode::Conflict,
        "event store contract: the visitor's error code must be kept"
    );
    assert!(
        err.message.contains("stop here"),
        "event store contract: the visitor's error message must be kept, got {:?}",
        err.message
    );
    expected.check(store.as_ref(), "after an aborted replay");
    expected.append(
        store.as_mut(),
        6_000,
        &[DomainEvent::Warning {
            message: "still works".into(),
        }],
    );
    expected.check(
        store.as_ref(),
        "after appending following an aborted replay",
    );
}

// ---- output logs ----------------------------------------------------------------------------

fn ids(run: &str, task: &str) -> (RunId, TaskId) {
    (RunId::parse(run).unwrap(), TaskId::parse(task).unwrap())
}

fn tail_of(logs: &dyn OutputLogs, run: &str, task: &str, max: usize) -> Option<String> {
    let (r, t) = ids(run, task);
    logs.tail(&r, &t, max)
}

fn save(logs: &mut dyn OutputLogs, run: &str, task: &str, attempt: u32, text: &str) {
    let (r, t) = ids(run, task);
    logs.save(&r, &t, attempt, text).unwrap_or_else(|e| {
        panic!("output logs contract: save of {run}/{task} attempt {attempt} failed: {e}")
    });
}

fn remove(logs: &mut dyn OutputLogs, run: &str, task: &str) {
    let (r, t) = ids(run, task);
    logs.remove(&r, &t);
}

/// Checks an [`OutputLogs`]. Every call of `open` must return a handle onto the same storage.
///
/// Rules: the tail of a task without output is `None`; saved text comes back; the highest attempt
/// wins; `tail` returns at most `max` bytes taken from the end on a character boundary; the end
/// of very large output is kept; `remove` forgets every attempt of one task and is idempotent;
/// runs and tasks (including ids that are prefixes of each other) do not see each other's output.
pub fn output_logs_contract(open: &mut dyn FnMut() -> Box<dyn OutputLogs>) {
    let mut logs = open();
    let all = usize::MAX;

    // 1. Nothing saved: None, for unknown runs and for unknown tasks of known runs.
    assert_eq!(
        tail_of(logs.as_ref(), "r1", "t1", all),
        None,
        "output logs contract: no output saved yet"
    );
    save(logs.as_mut(), "r1", "t1", 1, "x");
    assert_eq!(
        tail_of(logs.as_ref(), "r1", "t2", all),
        None,
        "output logs contract: other task of a known run"
    );
    assert_eq!(
        tail_of(logs.as_ref(), "r2", "t1", all),
        None,
        "output logs contract: same task id in another run"
    );
    remove(logs.as_mut(), "r1", "t1");

    // 2. Save then tail round-trips exactly, including newlines, unicode and empty text.
    let text = "first line\r\nsecond \"line\"\t日本語 🚀\n\nlast line without newline";
    save(logs.as_mut(), "r1", "t1", 1, text);
    assert_eq!(
        tail_of(logs.as_ref(), "r1", "t1", all).as_deref(),
        Some(text),
        "output logs contract: round trip"
    );
    save(logs.as_mut(), "r1", "t1", 1, "");
    assert_eq!(
        tail_of(logs.as_ref(), "r1", "t1", all).as_deref(),
        Some(""),
        "output logs contract: empty output is stored as empty, not as missing"
    );
    save(logs.as_mut(), "r1", "t1", 1, "rewritten");
    assert_eq!(
        tail_of(logs.as_ref(), "r1", "t1", all).as_deref(),
        Some("rewritten"),
        "output logs contract: saving an attempt again replaces its text"
    );

    // 3. The newest attempt wins, also when the numbers gain a digit.
    save(logs.as_mut(), "r1", "t1", 2, "second attempt");
    assert_eq!(
        tail_of(logs.as_ref(), "r1", "t1", all).as_deref(),
        Some("second attempt")
    );
    save(logs.as_mut(), "r1", "t1", 9, "ninth attempt");
    save(logs.as_mut(), "r1", "t1", 10, "tenth attempt");
    assert_eq!(
        tail_of(logs.as_ref(), "r1", "t1", all).as_deref(),
        Some("tenth attempt"),
        "output logs contract: attempt 10 is newer than attempt 9"
    );

    // 4. tail never returns more than `max` bytes, never splits a character, and returns the end.
    let mixed = format!("ab{}", "日本語".repeat(5));
    save(logs.as_mut(), "r1", "t2", 1, &mixed);
    for max in 0..=mixed.len() + 2 {
        let got = tail_of(logs.as_ref(), "r1", "t2", max).unwrap_or_default();
        assert!(
            got.len() <= max,
            "output logs contract: tail({max}) returned {} bytes",
            got.len()
        );
        assert!(
            mixed.ends_with(&got),
            "output logs contract: tail({max}) must be a suffix of the output"
        );
        assert!(
            got.len() + 3 >= max.min(mixed.len()),
            "output logs contract: tail({max}) dropped more than a partial character ({} bytes returned)",
            got.len()
        );
    }
    assert_eq!(
        tail_of(logs.as_ref(), "r1", "t2", mixed.len()).as_deref(),
        Some(mixed.as_str())
    );
    save(logs.as_mut(), "r1", "t3", 1, &"🚀".repeat(10));
    assert_eq!(
        tail_of(logs.as_ref(), "r1", "t3", 5).as_deref(),
        Some("🚀"),
        "output logs contract: 4-byte character"
    );
    assert_eq!(
        tail_of(logs.as_ref(), "r1", "t3", 8).as_deref(),
        Some("🚀🚀")
    );
    save(logs.as_mut(), "r1", "t4", 1, &"é".repeat(100));
    assert_eq!(
        tail_of(logs.as_ref(), "r1", "t4", 50).as_deref(),
        Some("é".repeat(25).as_str())
    );
    let odd = tail_of(logs.as_ref(), "r1", "t4", 51).unwrap_or_default();
    assert!(
        (50..=51).contains(&odd.len()) && odd.chars().all(|c| c == 'é'),
        "output logs contract: odd max"
    );

    // 5. Huge output keeps its end (adapters may drop anything older than the last MiB).
    let mib = 1024 * 1024;
    let mut huge = String::from("HEAD-MARKER\n");
    while huge.len() < 3 * mib {
        huge.push_str("0123456789 ünï 日本 filler line\n");
    }
    huge.push_str("TAIL-MARKER\n");
    save(logs.as_mut(), "r2", "big", 1, &huge);
    let small = tail_of(logs.as_ref(), "r2", "big", 4096).unwrap_or_default();
    assert!(
        huge.ends_with(&small) && small.len() <= 4096 && small.len() >= 4093,
        "output logs contract: tail of huge output"
    );
    assert!(small.ends_with("TAIL-MARKER\n"));
    let large = tail_of(logs.as_ref(), "r2", "big", all).unwrap_or_default();
    assert!(
        large.ends_with("TAIL-MARKER\n") && huge.ends_with(&large),
        "output logs contract: the end of huge output must be kept"
    );
    assert!(
        large.len() + 4 >= mib,
        "output logs contract: at least the last MiB must be kept, got {}",
        large.len()
    );
    remove(logs.as_mut(), "r2", "big");

    // 6. remove forgets every attempt of that task only, and is idempotent.
    save(logs.as_mut(), "r3", "t1", 1, "one");
    save(logs.as_mut(), "r3", "t1", 2, "two");
    save(logs.as_mut(), "r3", "t2", 1, "other task");
    remove(logs.as_mut(), "r3", "t1");
    assert_eq!(
        tail_of(logs.as_ref(), "r3", "t1", all),
        None,
        "output logs contract: remove drops all attempts"
    );
    assert_eq!(
        tail_of(logs.as_ref(), "r3", "t2", all).as_deref(),
        Some("other task"),
        "remove must keep other tasks"
    );
    remove(logs.as_mut(), "r3", "t1");
    remove(logs.as_mut(), "r3", "never-saved");
    remove(logs.as_mut(), "never-run", "t1");
    assert_eq!(
        tail_of(logs.as_ref(), "r3", "t2", all).as_deref(),
        Some("other task")
    );
    save(logs.as_mut(), "r3", "t1", 1, "after removal");
    assert_eq!(
        tail_of(logs.as_ref(), "r3", "t1", all).as_deref(),
        Some("after removal"),
        "save after remove"
    );
    remove(logs.as_mut(), "r3", "t2");
    assert_eq!(tail_of(logs.as_ref(), "r3", "t2", all), None);
    assert_eq!(
        tail_of(logs.as_ref(), "r3", "t1", all).as_deref(),
        Some("after removal")
    );
    remove(logs.as_mut(), "r3", "t1");

    // 7. Runs and tasks are isolated, including task ids that are prefixes of each other.
    save(logs.as_mut(), "r4", "a", 1, "task a");
    save(logs.as_mut(), "r4", "a-1", 1, "task a-1");
    save(logs.as_mut(), "r4", "a-1-1", 1, "task a-1-1");
    save(logs.as_mut(), "r5", "a", 1, "run r5 task a");
    assert_eq!(
        tail_of(logs.as_ref(), "r4", "a", all).as_deref(),
        Some("task a")
    );
    assert_eq!(
        tail_of(logs.as_ref(), "r4", "a-1", all).as_deref(),
        Some("task a-1")
    );
    assert_eq!(
        tail_of(logs.as_ref(), "r4", "a-1-1", all).as_deref(),
        Some("task a-1-1")
    );
    assert_eq!(
        tail_of(logs.as_ref(), "r5", "a", all).as_deref(),
        Some("run r5 task a")
    );
    remove(logs.as_mut(), "r4", "a");
    assert_eq!(tail_of(logs.as_ref(), "r4", "a", all), None);
    assert_eq!(
        tail_of(logs.as_ref(), "r4", "a-1", all).as_deref(),
        Some("task a-1"),
        "output logs contract: removing task `a` must not touch task `a-1`"
    );
    assert_eq!(
        tail_of(logs.as_ref(), "r5", "a", all).as_deref(),
        Some("run r5 task a")
    );
    remove(logs.as_mut(), "r4", "a-1");
    assert_eq!(
        tail_of(logs.as_ref(), "r4", "a-1-1", all).as_deref(),
        Some("task a-1-1")
    );
    remove(logs.as_mut(), "r4", "a-1-1");
    remove(logs.as_mut(), "r5", "a");

    // 8. Unusual but valid ids work.
    let long = "z".repeat(40);
    let pairs = [
        ("0", "0"),
        ("r-9_x", "t_1-2"),
        (long.as_str(), long.as_str()),
        ("9", "a"),
        ("a", "9"),
    ];
    for (i, (run, task)) in pairs.iter().enumerate() {
        save(logs.as_mut(), run, task, 1, &format!("output {i}"));
    }
    for (i, (run, task)) in pairs.iter().enumerate() {
        assert_eq!(
            tail_of(logs.as_ref(), run, task, all),
            Some(format!("output {i}")),
            "output logs contract: ids {run}/{task}"
        );
    }
    for (run, task) in pairs {
        remove(logs.as_mut(), run, task);
        assert_eq!(tail_of(logs.as_ref(), run, task, all), None);
    }

    // 9. Handles share storage: output is visible through another handle, survives reopening,
    //    and a removal through one handle is seen through the other.
    save(logs.as_mut(), "r6", "t1", 1, "shared");
    let mut other = open();
    assert_eq!(
        tail_of(other.as_ref(), "r6", "t1", all).as_deref(),
        Some("shared"),
        "visible through a second handle"
    );
    logs = reopen(open, logs);
    assert_eq!(
        tail_of(logs.as_ref(), "r6", "t1", all).as_deref(),
        Some("shared"),
        "output survives reopening"
    );
    remove(other.as_mut(), "r6", "t1");
    assert_eq!(
        tail_of(logs.as_ref(), "r6", "t1", all),
        None,
        "a removal is seen through the other handle"
    );
    drop(other);
    assert_eq!(
        tail_of(open().as_ref(), "r6", "t1", all),
        None,
        "output stays removed after reopening"
    );
}

// ---- contract files -------------------------------------------------------------------------

/// Text of a valid `result.json`: status `done`, summary `done it`.
pub const VALID_RESULT_JSON: &str = r#"{"version":1,"status":"done","summary":"done it"}"#;
/// Text of a valid `question.json`: kind `question`, text `Which database?`.
pub const VALID_QUESTION_JSON: &str = r#"{"version":1,"kind":"question","text":"Which database?"}"#;
/// A string a harness embeds in every planted file that is not valid. A reason shown for such a
/// file must never contain it, because the text is worker-controlled.
pub const PLANT_MARKER: &str = "PLANTED-CONTENT-MARKER-7731";

/// Which worker file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractFile {
    /// `.fleet/result.json`
    Result,
    /// `.fleet/question.json`
    Question,
}

/// What a harness puts in place of a worker file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plant {
    /// A valid file ([`VALID_RESULT_JSON`] / [`VALID_QUESTION_JSON`]).
    Valid,
    /// Text that is not JSON.
    Malformed,
    /// JSON of the wrong shape (a field has a value outside its vocabulary).
    WrongShape,
    /// Valid JSON far above the size limit of worker files (200 KiB).
    Oversize,
    /// A symbolic link to a valid file outside the worktree.
    Symlink,
    /// A named pipe nobody writes to.
    Fifo,
    /// A directory.
    Directory,
}

/// [`VerifyHarness`] over the in-memory fake of a [`World`]: the fake runner answers from its
/// verification mode, which this harness derives from the files it was asked to place.
pub struct MemoryVerifyHarness {
    world: World,
}

impl MemoryVerifyHarness {
    pub fn new(world: World) -> MemoryVerifyHarness {
        MemoryVerifyHarness { world }
    }
}

impl VerifyHarness for MemoryVerifyHarness {
    fn runner(&mut self) -> Box<dyn JobRunner> {
        self.world.ports().jobs
    }

    fn worktree(&self) -> String {
        "/work/wt".to_string()
    }

    fn state(&self) -> WorktreeState {
        WorktreeState {
            head: sha(&"e".repeat(40)),
            tree: sha(&"f".repeat(40)),
            changed: Vec::new(),
            truncated: false,
            dirty: false,
        }
    }

    fn place(&mut self, present: &[&str]) {
        // The fake cannot look at files; it fails the first mandatory check when `first.txt` is
        // missing and passes otherwise, which is what the suite's scenarios call for.
        let mode = if present.contains(&"first.txt") {
            super::VerifyMode::Pass
        } else {
            super::VerifyMode::Fail("first.txt does not exist".to_string())
        };
        self.world.set_verify_mode(mode);
    }
}

/// The medium a [`ContractFiles`] adapter works on, from the suite's point of view.
pub trait ContractHarness {
    /// The adapter under test.
    fn files(&self) -> &dyn ContractFiles;
    /// The worktree path passed to the adapter. Nothing is planted initially.
    fn worktree(&self) -> String;
    /// Makes `file` exist as described by `kind`, replacing whatever was there before.
    fn plant(&mut self, file: ContractFile, kind: Plant);
    /// Plants a valid result and a valid question, as a previous attempt would have left them.
    fn plant_stale_files(&mut self);
}

/// [`ContractHarness`] over the in-memory fake of a [`World`].
pub struct MemoryContractHarness {
    world: World,
    files: Box<dyn ContractFiles>,
    worktree: String,
}

impl MemoryContractHarness {
    pub fn new(world: World) -> MemoryContractHarness {
        let files = world.ports().contract;
        MemoryContractHarness {
            world,
            files,
            worktree: "/work/wt".to_string(),
        }
    }
}

impl ContractHarness for MemoryContractHarness {
    fn files(&self) -> &dyn ContractFiles {
        self.files.as_ref()
    }

    fn worktree(&self) -> String {
        self.worktree.clone()
    }

    fn plant(&mut self, file: ContractFile, kind: Plant) {
        let name = match file {
            ContractFile::Result => "result.json",
            ContractFile::Question => "question.json",
        };
        let reason = format!("{name}: planted {kind:?} file");
        match (file, kind) {
            (ContractFile::Result, Plant::Valid) => {
                let valid = WorkerResult {
                    status: ResultStatus::Done,
                    summary: "done it".to_string(),
                };
                self.world
                    .write_result(&self.worktree, ContractRead::Valid(valid));
            }
            (ContractFile::Question, Plant::Valid) => {
                let valid = WorkerQuestion {
                    kind: QuestionKind::Question,
                    text: "Which database?".to_string(),
                    choices: vec![],
                    action: None,
                };
                self.world
                    .write_question(&self.worktree, ContractRead::Valid(valid));
            }
            (ContractFile::Result, _) => self
                .world
                .write_result(&self.worktree, ContractRead::Invalid(reason)),
            (ContractFile::Question, _) => self
                .world
                .write_question(&self.worktree, ContractRead::Invalid(reason)),
        }
    }

    fn plant_stale_files(&mut self) {
        self.plant(ContractFile::Result, Plant::Valid);
        self.plant(ContractFile::Question, Plant::Valid);
    }
}

const BOTH_FILES: [ContractFile; 2] = [ContractFile::Result, ContractFile::Question];
const BROKEN: [Plant; 6] = [
    Plant::Malformed,
    Plant::WrongShape,
    Plant::Oversize,
    Plant::Symlink,
    Plant::Fifo,
    Plant::Directory,
];
/// Kinds that `prepare` must be able to clear (a directory is a conflict the adapter may refuse).
const CLEARABLE: [Plant; 6] = [
    Plant::Valid,
    Plant::Malformed,
    Plant::WrongShape,
    Plant::Oversize,
    Plant::Symlink,
    Plant::Fifo,
];

#[derive(Debug)]
enum Seen {
    Absent,
    Valid,
    Invalid(String),
}

fn classify<T: PartialEq + Debug>(read: ContractRead<T>, expected: T, what: &str) -> Seen {
    match read {
        ContractRead::Absent => Seen::Absent,
        ContractRead::Valid(v) => {
            assert_eq!(
                v, expected,
                "contract files contract: the valid {what} was read back wrongly"
            );
            Seen::Valid
        }
        ContractRead::Invalid(reason) => Seen::Invalid(reason),
    }
}

fn read_file(h: &dyn ContractHarness, file: ContractFile) -> Seen {
    let wt = h.worktree();
    match file {
        ContractFile::Result => {
            let expected = WorkerResult {
                status: ResultStatus::Done,
                summary: "done it".to_string(),
            };
            classify(h.files().read_result(&wt), expected, "result")
        }
        ContractFile::Question => {
            let expected = WorkerQuestion {
                kind: QuestionKind::Question,
                text: "Which database?".to_string(),
                choices: vec![],
                action: None,
            };
            classify(h.files().read_question(&wt), expected, "question")
        }
    }
}

fn assert_absent(h: &dyn ContractHarness, file: ContractFile, when: &str) {
    let seen = read_file(h, file);
    assert!(
        matches!(seen, Seen::Absent),
        "contract files contract: {file:?} must be Absent {when}, got {seen:?}"
    );
}

fn assert_valid(h: &dyn ContractHarness, file: ContractFile, when: &str) {
    let seen = read_file(h, file);
    assert!(
        matches!(seen, Seen::Valid),
        "contract files contract: {file:?} must be Valid {when}, got {seen:?}"
    );
}

fn assert_invalid(h: &dyn ContractHarness, file: ContractFile, kind: Plant) {
    let reason = match read_file(h, file) {
        Seen::Invalid(reason) => reason,
        other => panic!("contract files contract: a planted {kind:?} {file:?} must read as Invalid, got {other:?}"),
    };
    assert!(
        !reason.trim().is_empty(),
        "contract files contract: the reason for {kind:?} {file:?} is empty"
    );
    assert!(
        !reason.chars().any(char::is_control),
        "contract files contract: the reason for {kind:?} {file:?} has control characters: {reason:?}"
    );
    for secret in [PLANT_MARKER, "done it", "Which database?"] {
        assert!(
            !reason.contains(secret),
            "contract files contract: the reason for {kind:?} {file:?} echoes file content: {reason:?}"
        );
    }
}

fn question_present(h: &dyn ContractHarness) -> bool {
    h.files().question_present(&h.worktree())
}

fn prepare(h: &dyn ContractHarness, brief: &str) {
    h.files()
        .prepare(&h.worktree(), brief)
        .unwrap_or_else(|e| panic!("contract files contract: prepare failed: {e}"));
}

/// Checks a [`ContractFiles`] adapter through a [`ContractHarness`] that starts with nothing
/// planted.
///
/// Rules: missing files read as `Absent`; a valid file reads back as its value; a file that is
/// malformed, of the wrong shape, oversize, a link, a pipe or a directory reads as `Invalid`
/// with a printable reason that does not echo its content, without blocking or leaving the
/// worktree; `question_present` reports a question file of any kind; `prepare` removes stale
/// result and question files, can be repeated, and leaves the worktree ready for new files.
pub fn contract_files_contract(h: &mut dyn ContractHarness) {
    // 1. Nothing planted: both files are Absent and no question is present.
    for file in BOTH_FILES {
        assert_absent(h, file, "when nothing was planted");
    }
    assert!(
        !question_present(h),
        "contract files contract: question_present must be false without a file"
    );
    let elsewhere = format!("{}/no-such-worktree", h.worktree());
    assert!(
        matches!(h.files().read_result(&elsewhere), ContractRead::Absent),
        "unknown worktree: result"
    );
    assert!(
        matches!(h.files().read_question(&elsewhere), ContractRead::Absent),
        "unknown worktree: question"
    );
    assert!(
        !h.files().question_present(&elsewhere),
        "contract files contract: unknown worktree: question_present"
    );

    // 2. A valid result reads back; the question file is not affected by it.
    h.plant(ContractFile::Result, Plant::Valid);
    assert_valid(h, ContractFile::Result, "after planting a valid result");
    assert_absent(h, ContractFile::Question, "when only a result was planted");
    assert!(
        !question_present(h),
        "contract files contract: a result is not a question"
    );

    // 3. A valid question reads back and is present; the result is still valid.
    h.plant(ContractFile::Question, Plant::Valid);
    assert_valid(h, ContractFile::Question, "after planting a valid question");
    assert!(
        question_present(h),
        "contract files contract: question_present must be true for a valid question"
    );
    assert_valid(
        h,
        ContractFile::Result,
        "after planting a question next to it",
    );

    // 4. Every kind of broken file reads as Invalid with a safe reason, for both files, and a
    //    file of any kind counts as a present question.
    for kind in BROKEN {
        for file in BOTH_FILES {
            h.plant(file, kind);
            assert_invalid(h, file, kind);
        }
        assert!(
            question_present(h),
            "contract files contract: question_present must be true for a {kind:?} question file"
        );
    }

    // 5. A broken file can be replaced by a valid one.
    for file in BOTH_FILES {
        h.plant(file, Plant::Valid);
        assert_valid(h, file, "after replacing a broken file with a valid one");
    }

    // 6. prepare clears stale files of every kind, for both files.
    for kind in CLEARABLE {
        for file in BOTH_FILES {
            h.plant(file, kind);
        }
        prepare(h, "the brief of a new attempt");
        for file in BOTH_FILES {
            assert_absent(
                h,
                file,
                &format!("after prepare cleared a stale {kind:?} file"),
            );
        }
        assert!(
            !question_present(h),
            "contract files contract: prepare must clear a stale {kind:?} question"
        );
    }

    // 7. prepare is idempotent, also when there is nothing to clear.
    prepare(h, "brief");
    prepare(h, "a longer brief with unicode: ünï 日本語 🚀\nand lines");
    for file in BOTH_FILES {
        assert_absent(h, file, "after prepare was repeated");
    }
    h.plant_stale_files();
    assert_valid(h, ContractFile::Result, "when stale files are planted");
    assert_valid(h, ContractFile::Question, "when stale files are planted");
    prepare(h, "brief");
    prepare(h, "brief");
    for file in BOTH_FILES {
        assert_absent(
            h,
            file,
            "after stale files were cleared and prepare was repeated",
        );
    }

    // 8. Files planted after prepare are read normally.
    h.plant(ContractFile::Result, Plant::Valid);
    h.plant(ContractFile::Question, Plant::Valid);
    assert_valid(h, ContractFile::Result, "when planted after prepare");
    assert_valid(h, ContractFile::Question, "when planted after prepare");
    assert!(
        question_present(h),
        "contract files contract: question_present after prepare and a new question"
    );
    h.plant(ContractFile::Result, Plant::Malformed);
    assert_invalid(h, ContractFile::Result, Plant::Malformed);
}

// ---- verification jobs --------------------------------------------------------------------------

/// The medium a runner of verification jobs works on, from the suite's point of view.
///
/// The suite's contract has three file checks: `first` (mandatory, `first.txt` exists), `second`
/// (mandatory, `second.txt` exists) and `optional` (not mandatory, `optional.txt` exists).
pub trait VerifyHarness {
    /// A fresh runner of the adapter under test.
    fn runner(&mut self) -> Box<dyn JobRunner>;
    /// The worktree path given to the jobs.
    fn worktree(&self) -> String;
    /// The state of that worktree the jobs are started with: the commit the files placed by
    /// [`VerifyHarness::place`] are committed as, and its tree.
    fn state(&self) -> WorktreeState;
    /// Makes exactly the files named in `present` (relative paths) exist in the worktree, so that
    /// the checks of the suite's contract pass or fail accordingly.
    fn place(&mut self, present: &[&str]);
}

/// A contract of three file-exists checks, as described on [`VerifyHarness`].
fn verify_target(goal: &str) -> VerifyTarget {
    use fleet_domain::value::Value;
    use neo_verify::contract::VerificationContract;
    let check = |id: &str, path: &str, mandatory: bool| {
        Value::object([
            ("id", Value::str(id)),
            ("kind", Value::str("file")),
            ("mandatory", Value::Bool(mandatory)),
            ("path", Value::str(path)),
            ("assert", Value::str("exists")),
        ])
    };
    let contract = VerificationContract::from_value(&Value::object([
        ("version", Value::int(1)),
        (
            "checks",
            Value::array([
                check("first", "first.txt", true),
                check("second", "second.txt", true),
                check("optional", "optional.txt", false),
            ]),
        ),
    ]))
    .unwrap();
    VerifyTarget {
        goal: fleet_domain::ids::GoalId::parse(goal).unwrap(),
        contract_id: contract.id(&super::ToyDigest),
        contract,
    }
}

fn verify_job(h: &dyn VerifyHarness, task: &str, goals: &[&str]) -> VerifyJob {
    VerifyJob {
        run: run_id(),
        task: task_id(task),
        attempt: 3,
        origin: VerifyOrigin::Manual,
        worktree: h.worktree(),
        state: h.state(),
        env: vec![("PATH".to_string(), "/usr/bin:/bin".to_string())],
        targets: goals.iter().map(|g| verify_target(g)).collect(),
    }
}

/// Polls until `want` verification jobs finished (at most 30 seconds).
fn collect_verified(runner: &mut dyn JobRunner, want: usize) -> Vec<VerifyDone> {
    let started = std::time::Instant::now();
    let mut done = Vec::new();
    while done.len() < want {
        for d in runner.poll() {
            match d {
                JobDone::Verify(v) => done.push(*v),
                other => panic!("verify jobs contract: unexpected result {other:?}"),
            }
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "verify jobs contract: {} of {want} results arrived within 30 seconds",
            done.len()
        );
        if done.len() < want {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    done
}

/// What every runner of verification jobs must do: report on every check of every contract, in
/// order, with the verdict its files and processes call for; echo the identity of the job; and
/// report each job exactly once.
pub fn verify_jobs_contract(h: &mut dyn VerifyHarness) {
    use fleet_domain::status::Verdict;
    let mut runner = h.runner();

    // 1. Everything in place: every check passes. The result names the job it belongs to.
    h.place(&["first.txt", "second.txt", "optional.txt"]);
    let job = verify_job(h, "t1", &["g1"]);
    runner.start(Job::Verify(Box::new(job.clone()))).unwrap();
    let done = collect_verified(runner.as_mut(), 1).remove(0);
    assert_eq!(
        (
            done.run.clone(),
            done.task.clone(),
            done.attempt,
            done.origin
        ),
        (job.run.clone(), job.task.clone(), 3, VerifyOrigin::Manual),
        "verify jobs contract: the result must echo run, task, attempt and origin"
    );
    assert_eq!(
        (done.commit.clone(), done.tree.clone(), done.dirty),
        (job.state.head.clone(), job.state.tree.clone(), job.state.dirty),
        "verify jobs contract: the result must carry the commit, tree and dirtiness the job was given"
    );
    assert_eq!(
        done.results.len(),
        1,
        "verify jobs contract: one result per target"
    );
    let result = &done.results[0];
    assert_eq!(result.goal, job.targets[0].goal);
    assert_eq!(result.contract_id, job.targets[0].contract_id);
    let ids: Vec<&str> = result.reports.iter().map(|r| r.check.as_str()).collect();
    assert_eq!(
        ids,
        ["first", "second", "optional"],
        "verify jobs contract: one report per check, in contract order"
    );
    assert!(
        result.reports.iter().all(|r| r.verdict == Verdict::Passed),
        "verify jobs contract: with every file present every check passes: {:?}",
        result.reports
    );

    // 2. A missing file fails its check only; the other checks are still run and reported.
    h.place(&["second.txt", "optional.txt"]);
    runner
        .start(Job::Verify(Box::new(verify_job(h, "t1", &["g1"]))))
        .unwrap();
    let done = collect_verified(runner.as_mut(), 1).remove(0);
    let verdicts: Vec<Verdict> = done.results[0].reports.iter().map(|r| r.verdict).collect();
    assert_eq!(
        verdicts,
        [Verdict::Failed, Verdict::Passed, Verdict::Passed],
        "verify jobs contract: a failed check must not stop the others from running"
    );
    for r in &done.results[0].reports {
        assert!(
            !r.summary.contains('\n') && !r.summary.chars().any(char::is_control),
            "verify jobs contract: a summary is one clean line: {:?}",
            r.summary
        );
        assert!(
            r.output.len() <= 70_000,
            "verify jobs contract: output must be bounded"
        );
    }

    // 3. Several targets: one result each, in the order given.
    h.place(&["first.txt", "second.txt", "optional.txt"]);
    runner
        .start(Job::Verify(Box::new(verify_job(h, "t2", &["g1", "g2"]))))
        .unwrap();
    let done = collect_verified(runner.as_mut(), 1).remove(0);
    let goals: Vec<&str> = done.results.iter().map(|r| r.goal.as_str()).collect();
    assert_eq!(
        goals,
        ["g1", "g2"],
        "verify jobs contract: targets keep their order"
    );
    assert!(done
        .results
        .iter()
        .all(|r| r.reports.iter().all(|x| x.verdict == Verdict::Passed)));

    // 4. Jobs run concurrently without mixing up their results, and each is reported once.
    runner
        .start(Job::Verify(Box::new(verify_job(h, "ta", &["g1"]))))
        .unwrap();
    runner
        .start(Job::Verify(Box::new(verify_job(h, "tb", &["g1"]))))
        .unwrap();
    let mut done = collect_verified(runner.as_mut(), 2);
    done.sort_by(|a, b| a.task.cmp(&b.task));
    assert_eq!(
        (done[0].task.as_str(), done[1].task.as_str()),
        ("ta", "tb"),
        "verify jobs contract: each result belongs to its own job"
    );
    assert!(
        runner.poll().is_empty(),
        "verify jobs contract: a result is delivered once"
    );
    let waited = std::time::Instant::now();
    while runner.running() > 0 {
        assert!(
            waited.elapsed() < std::time::Duration::from_secs(10),
            "verify jobs contract: running() must return to 0 after the results were collected"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
