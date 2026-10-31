//! Application tests: restart replay, orphan reconciliation, graceful shutdown, store failures,
//! event publication, the `info` and `events` commands, task references and the optional undo
//! tool, driven through the command interface with in-memory fakes for every port.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_app::config::AppConfig;
use fleet_app::testing::{test_config, Harness};
use fleet_domain::contract::{
    ContractRead, QuestionKind, ResultStatus, WorkerQuestion, WorkerResult,
};
use fleet_domain::error::ErrorCode;
use fleet_domain::events::DomainEvent;
use fleet_domain::supervision::ProcessIdentity;
use fleet_domain::value::Value;

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    Value::object(pairs)
}

fn s(x: &str) -> Value {
    Value::str(x)
}

fn list(items: &[&str]) -> Value {
    Value::array(items.iter().map(|i| Value::str(*i)))
}

fn new_run(h: &mut Harness) {
    let r = h
        .call(
            "run.create",
            obj(vec![
                ("objective", s("ship the feature")),
                ("repo", s("/repo")),
            ]),
        )
        .unwrap();
    assert_eq!(r.data().get("run").and_then(Value::as_str), Some("r1"));
}

fn add(h: &mut Harness, id: &str, after: &[&str]) {
    h.call(
        "task.add",
        obj(vec![
            ("run", s("r1")),
            ("id", s(id)),
            ("title", s(&format!("task {id}"))),
            ("spec", s("do the work")),
            ("after", list(after)),
        ]),
    )
    .unwrap();
}

fn start(h: &mut Harness) {
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
}

fn status(h: &mut Harness) -> Value {
    h.call("status", Value::Null).unwrap().data().clone()
}

fn task_doc(h: &mut Harness, id: &str) -> Value {
    let st = status(h);
    for run in st.get("runs").and_then(Value::as_array).unwrap() {
        for t in run.get("tasks").and_then(Value::as_array).unwrap() {
            if t.get("id").and_then(Value::as_str) == Some(id) {
                return t.clone();
            }
        }
    }
    panic!("task {id} not found");
}

fn task_status(h: &mut Harness, id: &str) -> String {
    task_doc(h, id)
        .get("status")
        .and_then(Value::as_str)
        .unwrap()
        .to_string()
}

fn retries_used(h: &mut Harness, id: &str) -> u64 {
    task_doc(h, id)
        .get("retries_used")
        .and_then(Value::as_u64)
        .unwrap()
}

fn attempt(h: &mut Harness, id: &str) -> u64 {
    task_doc(h, id)
        .get("attempt")
        .and_then(Value::as_u64)
        .unwrap()
}

fn finish_ok(h: &mut Harness, task: &str) {
    let session = h.world.session_for(task).expect("task has a live session");
    let spec = h.world.session_spec(session).unwrap();
    h.world.write_result(
        &spec.cwd,
        ContractRead::Valid(WorkerResult {
            status: ResultStatus::Done,
            summary: format!("{task} done"),
        }),
    );
    h.world.mark_dirty(&spec.cwd);
    h.world.exit(session, 0);
    h.orch.tick();
}

fn count(h: &Harness, kind: &str) -> usize {
    h.world.event_kinds().iter().filter(|k| **k == kind).count()
}

fn warnings(h: &Harness) -> Vec<String> {
    h.world
        .stored_events()
        .into_iter()
        .filter_map(|e| match e {
            DomainEvent::Warning { message } => Some(message),
            _ => None,
        })
        .collect()
}

fn last_brief(h: &Harness) -> String {
    h.world
        .briefs()
        .last()
        .expect("a brief was written")
        .1
        .clone()
}

fn reconciled(h: &Harness) -> Vec<(String, String)> {
    h.world
        .stored_events()
        .into_iter()
        .filter_map(|e| match e {
            DomainEvent::OrphanReconciled { task, action, .. } => Some((task.to_string(), action)),
            _ => None,
        })
        .collect()
}

fn question(text: &str) -> WorkerQuestion {
    WorkerQuestion {
        kind: QuestionKind::Question,
        text: text.to_string(),
        choices: vec![],
        action: None,
    }
}

fn cwd_of(h: &Harness, task: &str) -> String {
    h.world
        .session_spec(h.world.session_for(task).expect("live task"))
        .unwrap()
        .cwd
}

/// The pid and start token recorded when `task` was last dispatched.
fn identity_of(h: &Harness, task: &str) -> ProcessIdentity {
    h.world
        .stored_events()
        .into_iter()
        .rev()
        .find_map(|e| match e {
            DomainEvent::TaskDispatched {
                task: t,
                pid,
                start_token,
                ..
            } if t.as_str() == task => Some(ProcessIdentity::new(
                u32::try_from(pid).unwrap(),
                start_token,
            )),
            _ => None,
        })
        .expect("the task was dispatched")
}

fn gates(h: &mut Harness) -> Vec<Value> {
    h.call("gate.list", Value::Null)
        .unwrap()
        .data()
        .get("gates")
        .and_then(Value::as_array)
        .unwrap()
        .to_vec()
}

fn running_task() -> Harness {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    start(&mut h);
    assert_eq!(task_status(&mut h, "a"), "running");
    h
}

// ---- restart replay -----------------------------------------------------------------------------

fn busy_config() -> AppConfig {
    let mut c = test_config();
    c.max_retries = 0;
    c.merge_needs_approval = false;
    c
}

/// a completed and merged, b failed, c and e blocked on a question and an approval, d pending.
fn busy_run() -> Harness {
    let mut h = Harness::with_config(busy_config());
    new_run(&mut h);
    for t in ["a", "b", "c", "e"] {
        add(&mut h, t, &[]);
    }
    add(&mut h, "d", &["c"]);
    start(&mut h);
    finish_ok(&mut h, "a");
    let b = h.world.session_for("b").unwrap();
    h.world.exit(b, 1);
    h.orch.tick();
    let c = h.world.session_for("c").unwrap();
    let dir = cwd_of(&h, "c");
    h.world
        .write_question(&dir, ContractRead::Valid(question("Which database?")));
    h.world.exit(c, 0);
    h.orch.tick();
    let e = h.world.session_for("e").unwrap();
    let dir = cwd_of(&h, "e");
    h.world.write_question(
        &dir,
        ContractRead::Valid(WorkerQuestion {
            kind: QuestionKind::Approval,
            text: "may I clean the cache?".into(),
            choices: vec![],
            action: None,
        }),
    );
    h.world.exit(e, 0);
    h.orch.tick();
    h.call("merge", obj(vec![("task", s("a"))])).unwrap();
    h
}

#[test]
fn after_a_restart_the_status_document_matches_the_one_before() {
    let mut h = busy_run();
    assert_eq!(task_status(&mut h, "a"), "completed");
    assert_eq!(task_status(&mut h, "b"), "failed");
    assert_eq!(task_status(&mut h, "c"), "blocked");
    assert_eq!(task_status(&mut h, "d"), "pending");
    assert_eq!(task_status(&mut h, "e"), "blocked");
    let before = status(&mut h);
    let gates_before = h.call("gate.list", Value::Null).unwrap().data().clone();
    let approvals_before = h.call("approval.list", Value::Null).unwrap().data().clone();

    h.restart(busy_config()).unwrap();

    let after = status(&mut h);
    for part in ["runs", "gates", "approvals"] {
        assert_eq!(
            before.get(part),
            after.get(part),
            "{part} differ after the restart"
        );
    }
    assert!(!before
        .get("gates")
        .and_then(Value::as_array)
        .unwrap()
        .is_empty());
    assert!(!before
        .get("approvals")
        .and_then(Value::as_array)
        .unwrap()
        .is_empty());
    assert_eq!(
        &gates_before,
        h.call("gate.list", Value::Null).unwrap().data()
    );
    assert_eq!(
        &approvals_before,
        h.call("approval.list", Value::Null).unwrap().data()
    );
}

#[test]
fn a_restart_does_not_start_or_change_anything_by_itself() {
    let mut h = busy_run();
    let sessions = h.world.session_ids().len();
    let before = status(&mut h);
    h.restart(busy_config()).unwrap();
    for _ in 0..3 {
        h.orch.tick();
    }
    assert_eq!(h.world.session_ids().len(), sessions);
    assert_eq!(h.orch.live_count(), 0);
    assert_eq!(before.get("runs"), status(&mut h).get("runs"));
}

#[test]
fn ids_keep_counting_after_a_restart() {
    let mut h = busy_run();
    h.restart(busy_config()).unwrap();
    let r = h
        .call(
            "run.create",
            obj(vec![("objective", s("second")), ("repo", s("/repo"))]),
        )
        .unwrap();
    assert_eq!(r.data().get("run").and_then(Value::as_str), Some("r2"));
    h.call(
        "task.add",
        obj(vec![("run", s("r2")), ("id", s("x")), ("title", s("x"))]),
    )
    .unwrap();
    h.call("run.start", obj(vec![("run", s("r2"))])).unwrap();
    let id = h.world.session_for("x").unwrap();
    let dir = h.world.session_spec(id).unwrap().cwd;
    h.world
        .write_question(&dir, ContractRead::Valid(question("Another question?")));
    h.world.exit(id, 0);
    h.orch.tick();
    let ids: Vec<String> = gates(&mut h)
        .iter()
        .map(|g| g.get("id").and_then(Value::as_str).unwrap().to_string())
        .collect();
    assert_eq!(
        ids,
        vec!["g1", "g2"],
        "the new gate does not reuse an old id"
    );
}

#[test]
fn a_decision_made_before_a_restart_is_still_valid_after_it() {
    let mut h = busy_run();
    h.restart(busy_config()).unwrap();
    h.call(
        "gate.resolve",
        obj(vec![("id", s("g1")), ("answer", s("sqlite"))]),
    )
    .unwrap();
    assert_eq!(task_status(&mut h, "c"), "running");
    assert!(last_brief(&h).contains("Operator says: sqlite"));
    let list = h.call("approval.list", Value::Null).unwrap().data().clone();
    let digest = list.get("approvals").and_then(Value::as_array).unwrap()[0]
        .get("digest")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();
    h.call(
        "approval.decide",
        obj(vec![
            ("id", s("a1")),
            ("approve", Value::Bool(true)),
            ("digest", s(&digest)),
        ]),
    )
    .unwrap();
    assert_eq!(task_status(&mut h, "e"), "running");
}

#[test]
fn every_restart_is_recorded_in_the_log() {
    let mut h = busy_run();
    assert_eq!(count(&h, "daemon_started"), 1);
    h.restart(busy_config()).unwrap();
    h.restart(busy_config()).unwrap();
    assert_eq!(count(&h, "daemon_started"), 3);
}

// ---- orphan reconciliation ----------------------------------------------------------------------

#[test]
fn a_task_running_when_the_daemon_died_is_requeued_without_using_a_retry() {
    let mut h = running_task();
    h.restart(test_config()).unwrap();
    assert_eq!(task_status(&mut h, "a"), "ready");
    assert_eq!(retries_used(&mut h, "a"), 0);
    assert_eq!(count(&h, "retry_queued"), 1);
    h.orch.tick();
    assert_eq!(task_status(&mut h, "a"), "running");
    assert_eq!(attempt(&mut h, "a"), 2);
    assert_eq!(retries_used(&mut h, "a"), 0);
    assert_eq!(h.orch.live_count(), 1);
    assert!(
        last_brief(&h).contains("interrupted by a daemon restart"),
        "{}",
        last_brief(&h)
    );
}

#[test]
fn interrupted_tasks_are_requeued_even_when_no_retries_are_configured() {
    let mut h = running_task();
    let mut cfg = test_config();
    cfg.max_retries = 0;
    h.restart(cfg).unwrap();
    assert_eq!(
        task_status(&mut h, "a"),
        "ready",
        "a crash is not the task's fault"
    );
}

#[test]
fn repeated_crashes_never_exhaust_the_retry_budget() {
    let mut h = running_task();
    for _ in 0..5 {
        h.restart(test_config()).unwrap();
        h.orch.tick();
        assert_eq!(task_status(&mut h, "a"), "running");
    }
    assert_eq!(retries_used(&mut h, "a"), 0);
    assert_eq!(attempt(&mut h, "a"), 6);
}

#[test]
fn an_orphan_whose_process_has_the_same_start_token_is_terminated() {
    let mut h = running_task();
    let identity = identity_of(&h, "a");
    h.world
        .set_running_process(identity.pid, &identity.start_token);
    h.restart(test_config()).unwrap();

    assert_eq!(h.world.terminated_orphans(), vec![identity]);
    let r = reconciled(&h);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, "a");
    assert!(r[0].1.contains("terminated"), "{}", r[0].1);
    assert_eq!(task_status(&mut h, "a"), "ready");
    assert_eq!(retries_used(&mut h, "a"), 0);
}

#[test]
fn an_orphan_whose_pid_now_belongs_to_another_process_is_never_signalled() {
    let mut h = running_task();
    let identity = identity_of(&h, "a");
    h.world
        .set_running_process(identity.pid, "a-completely-different-start-token");
    h.restart(test_config()).unwrap();

    assert!(
        h.world.terminated_orphans().is_empty(),
        "pid reuse protection: the stranger is left alone"
    );
    let w = warnings(&h);
    assert!(
        w.iter()
            .any(|m| m.contains("r1/a") && m.contains("now belongs to another process")),
        "{w:?}"
    );
    let r = reconciled(&h);
    assert_eq!(r.len(), 1);
    assert!(
        r[0].1.contains("reused") && r[0].1.contains("nothing was signalled"),
        "{}",
        r[0].1
    );
    assert_eq!(
        task_status(&mut h, "a"),
        "ready",
        "the task is still queued again"
    );
}

#[test]
fn an_orphan_whose_process_is_gone_is_simply_requeued() {
    let mut h = running_task();
    h.restart(test_config()).unwrap();
    assert!(h.world.terminated_orphans().is_empty());
    let r = reconciled(&h);
    assert_eq!(r.len(), 1);
    assert!(r[0].1.contains("already gone"), "{}", r[0].1);
    assert!(warnings(&h).is_empty());
    assert_eq!(task_status(&mut h, "a"), "ready");
}

#[test]
fn every_orphan_is_judged_on_its_own() {
    let mut h = Harness::new();
    new_run(&mut h);
    for t in ["a", "b", "c"] {
        add(&mut h, t, &[]);
    }
    start(&mut h);
    let (a, b) = (identity_of(&h, "a"), identity_of(&h, "b"));
    h.world.set_running_process(a.pid, &a.start_token); // same process
    h.world.set_running_process(b.pid, "someone-else"); // pid reused
                                                        // c: gone
    h.restart(test_config()).unwrap();

    assert_eq!(h.world.terminated_orphans(), vec![a]);
    let r = reconciled(&h);
    assert_eq!(r.len(), 3);
    for (task, action) in r {
        match task.as_str() {
            "a" => assert!(action.contains("terminated"), "{action}"),
            "b" => assert!(action.contains("reused"), "{action}"),
            "c" => assert!(action.contains("already gone"), "{action}"),
            other => panic!("unexpected task {other}"),
        }
    }
    for t in ["a", "b", "c"] {
        assert_eq!(task_status(&mut h, t), "ready");
    }
}

#[test]
fn an_orphan_that_had_written_a_done_result_goes_to_review_and_completes() {
    let mut h = running_task();
    let dir = cwd_of(&h, "a");
    h.world.write_result(
        &dir,
        ContractRead::Valid(WorkerResult {
            status: ResultStatus::Done,
            summary: "finished before the crash".into(),
        }),
    );
    h.world.mark_dirty(&dir);
    h.restart(test_config()).unwrap();

    assert_eq!(task_status(&mut h, "a"), "completed");
    assert_eq!(
        task_doc(&mut h, "a").get("summary").and_then(Value::as_str),
        Some("finished before the crash")
    );
    assert_eq!(count(&h, "retry_queued"), 0);
    for k in [
        "orphan_reconciled",
        "session_ended",
        "task_reviewing",
        "task_committed",
        "task_completed",
    ] {
        assert!(h.world.event_kinds().contains(&k), "missing {k}");
    }
}

#[test]
fn an_orphan_whose_result_says_failed_is_not_completed() {
    let mut h = running_task();
    let dir = cwd_of(&h, "a");
    h.world.write_result(
        &dir,
        ContractRead::Valid(WorkerResult {
            status: ResultStatus::Failed,
            summary: "could not do it".into(),
        }),
    );
    h.restart(test_config()).unwrap();
    assert_eq!(count(&h, "task_completed"), 0);
    assert_ne!(task_status(&mut h, "a"), "completed");
}

#[test]
fn an_orphan_with_a_question_file_opens_the_gate() {
    let mut h = running_task();
    let dir = cwd_of(&h, "a");
    h.world
        .write_question(&dir, ContractRead::Valid(question("Which database?")));
    h.restart(test_config()).unwrap();

    assert_eq!(task_status(&mut h, "a"), "blocked");
    let g = gates(&mut h);
    assert_eq!(g.len(), 1);
    assert_eq!(
        g[0].get("text").and_then(Value::as_str),
        Some("Which database?")
    );
    assert_eq!(retries_used(&mut h, "a"), 0);
    h.call(
        "gate.resolve",
        obj(vec![("id", s("g1")), ("answer", s("sqlite"))]),
    )
    .unwrap();
    assert_eq!(task_status(&mut h, "a"), "running");
}

#[test]
fn only_running_tasks_are_orphans() {
    let mut h = busy_run();
    h.restart(busy_config()).unwrap();
    assert!(reconciled(&h).is_empty());
    assert_eq!(count(&h, "retry_queued"), 0);
}

// ---- graceful shutdown --------------------------------------------------------------------------

fn two_running_one_waiting() -> Harness {
    let mut h = Harness::new();
    h.call(
        "run.create",
        obj(vec![
            ("objective", s("o")),
            ("repo", s("/repo")),
            ("max_parallel", Value::int(2u64)),
        ]),
    )
    .unwrap();
    for t in ["a", "b", "c"] {
        add(&mut h, t, &[]);
    }
    start(&mut h);
    assert_eq!(h.world.live_session_ids().len(), 2);
    assert_eq!(task_status(&mut h, "c"), "ready");
    h
}

#[test]
fn shutdown_signals_the_workers_and_requeues_their_tasks_without_using_retries() {
    let mut h = two_running_one_waiting();
    let (a, b) = (
        h.world.session_for("a").unwrap(),
        h.world.session_for("b").unwrap(),
    );
    let reply = h.call("shutdown", Value::Null).unwrap();
    assert_eq!(
        reply.data().get("stopping").and_then(Value::as_bool),
        Some(true)
    );
    assert!(h.orch.shutdown_requested());

    h.orch.tick();
    assert_eq!(h.world.signals(a), vec![fleet_app::ports::Signal::Term]);
    assert_eq!(h.world.signals(b), vec![fleet_app::ports::Signal::Term]);
    h.orch.tick();
    assert_eq!(
        h.orch.live_count(),
        0,
        "the caller can stop once no worker is left"
    );
    for t in ["a", "b"] {
        assert_eq!(task_status(&mut h, t), "ready", "{t}");
        assert_eq!(retries_used(&mut h, t), 0, "{t}");
    }
    assert_eq!(count(&h, "retry_queued"), 2);
}

#[test]
fn shutdown_stops_dispatching_even_when_slots_free_up() {
    let mut h = two_running_one_waiting();
    h.call("shutdown", Value::Null).unwrap();
    for _ in 0..4 {
        h.orch.tick();
    }
    assert_eq!(
        h.world.session_ids().len(),
        2,
        "c never started and nothing restarted"
    );
    assert_eq!(task_status(&mut h, "c"), "ready");
    // a run started during the shutdown does not dispatch either
    h.call(
        "run.create",
        obj(vec![("objective", s("late")), ("repo", s("/repo"))]),
    )
    .unwrap();
    h.call(
        "task.add",
        obj(vec![
            ("run", s("r2")),
            ("id", s("late")),
            ("title", s("late")),
        ]),
    )
    .unwrap();
    h.call("run.start", obj(vec![("run", s("r2"))])).unwrap();
    h.orch.tick();
    assert_eq!(h.world.session_ids().len(), 2);
}

#[test]
fn shutdown_kills_a_worker_that_ignores_term_after_the_grace_period() {
    let mut h = running_task();
    let id = h.world.session_for("a").unwrap();
    h.world.configure_session(id, false, true, false);
    h.call("shutdown", Value::Null).unwrap();
    h.orch.tick();
    assert_eq!(h.world.signals(id), vec![fleet_app::ports::Signal::Term]);
    h.world.advance(1000);
    h.orch.tick();
    assert_eq!(
        h.world.signals(id),
        vec![
            fleet_app::ports::Signal::Term,
            fleet_app::ports::Signal::Kill
        ]
    );
    h.orch.tick();
    assert_eq!(h.orch.live_count(), 0);
    assert_eq!(task_status(&mut h, "a"), "ready");
    assert_eq!(retries_used(&mut h, "a"), 0);
}

#[test]
fn work_that_was_already_reported_done_is_kept_by_a_shutdown() {
    let mut h = running_task();
    let id = h.world.session_for("a").unwrap();
    let dir = cwd_of(&h, "a");
    h.world.write_result(
        &dir,
        ContractRead::Valid(WorkerResult {
            status: ResultStatus::Done,
            summary: "done just in time".into(),
        }),
    );
    h.call("shutdown", Value::Null).unwrap();
    h.orch.tick();
    h.orch.tick();
    assert!(!h.world.signals(id).is_empty());
    assert_eq!(task_status(&mut h, "a"), "completed");
}

#[test]
fn after_a_graceful_shutdown_a_new_daemon_resumes_the_work() {
    let mut h = two_running_one_waiting();
    h.call("shutdown", Value::Null).unwrap();
    h.orch.tick();
    h.orch.tick();
    h.restart(test_config()).unwrap();
    for _ in 0..2 {
        h.orch.tick();
    }
    assert_eq!(h.orch.live_count(), 2, "the new daemon dispatches again");
    for t in ["a", "b"] {
        assert_eq!(retries_used(&mut h, t), 0);
    }
    assert!(
        reconciled(&h).is_empty(),
        "nothing was left running to reconcile"
    );
}

// ---- store failures and publication ---------------------------------------------------------------

#[test]
fn a_store_failure_fails_the_command_changes_nothing_and_publishes_nothing() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    let stored = h.world.stored_events().len();
    let published = h.world.published().len();
    let before = status(&mut h);

    h.world.fail_appends(true);
    let err = h
        .call(
            "task.add",
            obj(vec![("run", s("r1")), ("id", s("b")), ("title", s("b"))]),
        )
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Internal);
    assert!(h
        .call(
            "run.create",
            obj(vec![("objective", s("o")), ("repo", s("/repo"))])
        )
        .is_err());
    assert!(h.call("run.start", obj(vec![("run", s("r1"))])).is_err());
    assert!(h.call("task.cancel", obj(vec![("task", s("a"))])).is_err());
    assert_eq!(h.world.stored_events().len(), stored);
    assert_eq!(
        h.world.published().len(),
        published,
        "subscribers never see events that are not durable"
    );
    h.world.fail_appends(false);

    let after = status(&mut h);
    assert_eq!(
        before.get("runs"),
        after.get("runs"),
        "the in-memory state did not change either"
    );
    assert_eq!(task_status(&mut h, "a"), "ready");
    assert!(
        h.world.session_ids().is_empty(),
        "the failed start dispatched nothing"
    );

    // the same commands work once the store is back
    h.call(
        "task.add",
        obj(vec![("run", s("r1")), ("id", s("b")), ("title", s("b"))]),
    )
    .unwrap();
    assert_eq!(task_status(&mut h, "b"), "ready");
}

#[test]
fn a_store_failure_while_answering_a_gate_leaves_it_open() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    start(&mut h);
    let id = h.world.session_for("a").unwrap();
    let dir = h.world.session_spec(id).unwrap().cwd;
    h.world
        .write_question(&dir, ContractRead::Valid(question("Which database?")));
    h.world.exit(id, 0);
    h.orch.tick();
    assert_eq!(task_status(&mut h, "a"), "blocked");

    h.world.fail_appends(true);
    assert!(h
        .call(
            "gate.resolve",
            obj(vec![("id", s("g1")), ("answer", s("sqlite"))])
        )
        .is_err());
    h.world.fail_appends(false);
    assert_eq!(task_status(&mut h, "a"), "blocked");
    assert_eq!(
        gates(&mut h)[0].get("open").and_then(Value::as_bool),
        Some(true)
    );
    h.call(
        "gate.resolve",
        obj(vec![("id", s("g1")), ("answer", s("sqlite"))]),
    )
    .unwrap();
    assert_eq!(task_status(&mut h, "a"), "running");
}

#[test]
fn a_store_failure_while_a_session_ends_does_not_leave_a_running_task_without_a_session() {
    let mut h = running_task();
    let id = h.world.session_for("a").unwrap();
    h.world.exit(id, 0);
    h.world.fail_appends(true);
    h.orch.tick();
    h.world.fail_appends(false);
    for _ in 0..3 {
        h.orch.tick();
    }
    let doc = task_doc(&mut h, "a");
    let state = doc.get("status").and_then(Value::as_str).unwrap();
    let live = doc.get("live").and_then(Value::as_bool).unwrap();
    assert!(
        state != "running" || live,
        "the task is shown as running but nothing supervises it any more (status {state}, live {live})"
    );
}

#[test]
fn published_events_are_exactly_the_stored_events_in_order() {
    let mut h = busy_run();
    h.restart(busy_config()).unwrap();
    h.call(
        "gate.resolve",
        obj(vec![("id", s("g1")), ("answer", s("sqlite"))]),
    )
    .unwrap();
    h.orch.tick();

    let stored = h.world.stored_events();
    let published = h.world.published();
    assert_eq!(
        published.len(),
        stored.len(),
        "every stored event was published exactly once"
    );
    let mut last_ts = 0;
    for (i, (p, e)) in published.iter().zip(&stored).enumerate() {
        assert_eq!(p.seq, i as u64 + 1, "sequence numbers are consecutive");
        assert_eq!(&p.event, e);
        assert!(p.ts_ms >= last_ts);
        last_ts = p.ts_ms;
    }
}

#[test]
fn events_of_one_command_are_published_as_they_were_stored_together() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    start(&mut h);
    let id = h.world.session_for("a").unwrap();
    let dir = h.world.session_spec(id).unwrap().cwd;
    h.world
        .write_question(&dir, ContractRead::Valid(question("Which database?")));
    h.world.exit(id, 0);
    h.orch.tick();
    let published: Vec<&'static str> = h.world.published().iter().map(|p| p.event.kind()).collect();
    let tail: Vec<&str> = published.iter().rev().take(3).rev().copied().collect();
    assert_eq!(
        tail,
        vec!["session_ended", "decision_opened", "task_blocked"]
    );
}

// ---- info and events ------------------------------------------------------------------------------

#[test]
fn info_describes_the_daemon_and_its_configuration() {
    let mut h = Harness::new();
    let info = h.call("info", Value::Null).unwrap();
    let d = info.data();
    assert_eq!(
        d.get("schema").and_then(Value::as_str),
        Some("fleet.info/1")
    );
    assert_eq!(d.get("version").and_then(Value::as_str), Some("test"));
    assert_eq!(d.get("max_sessions").and_then(Value::as_u64), Some(4));
    assert_eq!(d.get("sessions").and_then(Value::as_u64), Some(0));
    assert_eq!(d.get("runs").and_then(Value::as_u64), Some(0));
    assert_eq!(d.get("tasks").and_then(Value::as_u64), Some(0));
    assert_eq!(
        d.get("merge_needs_approval").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        d.get("external_commands").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(d.get("gates").and_then(Value::as_u64), Some(0));
    assert_eq!(
        d.get("undo_available").and_then(Value::as_bool),
        Some(false)
    );
    let workers: Vec<&str> = d
        .get("workers")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(workers, vec!["agent"]);

    new_run(&mut h);
    add(&mut h, "a", &[]);
    add(&mut h, "b", &[]);
    start(&mut h);
    h.world.set_undo(true);
    let info = h.call("info", Value::Null).unwrap();
    let d = info.data();
    assert_eq!(d.get("runs").and_then(Value::as_u64), Some(1));
    assert_eq!(d.get("tasks").and_then(Value::as_u64), Some(2));
    assert_eq!(d.get("sessions").and_then(Value::as_u64), Some(2));
    assert_eq!(d.get("undo_available").and_then(Value::as_bool), Some(true));
}

#[test]
fn info_reflects_a_configuration_without_approvals_or_commands() {
    let mut cfg = test_config();
    cfg.merge_needs_approval = false;
    cfg.external_commands = false;
    let mut h = Harness::with_config(cfg);
    let info = h.call("info", Value::Null).unwrap();
    assert_eq!(
        info.data()
            .get("merge_needs_approval")
            .and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        info.data()
            .get("external_commands")
            .and_then(Value::as_bool),
        Some(false)
    );
}

fn event_lines(h: &mut Harness, limit: Option<u64>) -> Vec<String> {
    let args = match limit {
        Some(n) => obj(vec![("limit", Value::int(n))]),
        None => Value::Null,
    };
    let reply = h.call("events", args).unwrap();
    assert_eq!(
        reply.data().get("schema").and_then(Value::as_str),
        Some("fleet.events/1")
    );
    reply
        .data()
        .get("events")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|e| e.get("line").and_then(Value::as_str).unwrap().to_string())
        .collect()
}

#[test]
fn events_lists_the_most_recent_activity_oldest_first_and_honours_the_limit() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    start(&mut h);
    finish_ok(&mut h, "a");
    let all = event_lines(&mut h, None);
    assert!(all.iter().any(|l| l == "run r1 created"), "{all:?}");
    assert!(
        all.iter().any(|l| l == "r1/a dispatched (attempt 1)"),
        "{all:?}"
    );
    assert_eq!(all.last().map(String::as_str), Some("r1/a completed"));

    let two = event_lines(&mut h, Some(2));
    assert_eq!(two.len(), 2);
    assert_eq!(two, all[all.len() - 2..].to_vec());
    assert_eq!(
        event_lines(&mut h, Some(0)).len(),
        1,
        "a limit of zero is raised to one"
    );
    let seqs: Vec<u64> = h
        .call("events", Value::Null)
        .unwrap()
        .data()
        .get("events")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|e| e.get("seq").and_then(Value::as_u64).unwrap())
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn events_never_contain_text_written_by_a_worker_or_warnings() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    start(&mut h);
    let id = h.world.session_for("a").unwrap();
    let dir = h.world.session_spec(id).unwrap().cwd;
    h.world.write_question(
        &dir,
        ContractRead::Valid(question("PLEASE-LEAK-THIS question")),
    );
    h.world.write_result(
        &dir,
        ContractRead::Invalid("PLEASE-LEAK-THIS result".into()),
    );
    h.world.exit(id, 0);
    h.orch.tick();
    assert!(
        !warnings(&h).is_empty(),
        "the invalid result produced a warning"
    );
    let lines = event_lines(&mut h, Some(500));
    assert!(
        lines.iter().all(|l| !l.contains("LEAK") && l != "warning"),
        "{lines:?}"
    );
}

// ---- task references --------------------------------------------------------------------------------

fn two_runs_with_a() -> Harness {
    let mut h = Harness::new();
    for run in ["r1", "r2"] {
        h.call(
            "run.create",
            obj(vec![("objective", s("o")), ("repo", s("/repo"))]),
        )
        .unwrap();
        h.call(
            "task.add",
            obj(vec![("run", s(run)), ("id", s("a")), ("title", s("a"))]),
        )
        .unwrap();
    }
    h.call(
        "task.add",
        obj(vec![("run", s("r2")), ("id", s("b")), ("title", s("b"))]),
    )
    .unwrap();
    h
}

fn status_of(h: &mut Harness, run: &str, task: &str) -> String {
    let st = h
        .call("status", obj(vec![("run", s(run))]))
        .unwrap()
        .data()
        .clone();
    let runs = st.get("runs").and_then(Value::as_array).unwrap().to_vec();
    assert_eq!(runs.len(), 1);
    for t in runs[0].get("tasks").and_then(Value::as_array).unwrap() {
        if t.get("id").and_then(Value::as_str) == Some(task) {
            return t.get("status").and_then(Value::as_str).unwrap().to_string();
        }
    }
    panic!("no task {task} in {run}");
}

#[test]
fn unknown_task_references_are_not_found() {
    let mut h = two_runs_with_a();
    for reference in ["zzz", "r1/zzz", "r9/a"] {
        for cmd in [
            "task.cancel",
            "task.stop",
            "task.retry",
            "task.tail",
            "task.attach",
            "merge",
            "undo",
        ] {
            let err = h.call(cmd, obj(vec![("task", s(reference))])).unwrap_err();
            assert_eq!(err.code, ErrorCode::NotFound, "{cmd} {reference}");
        }
    }
}

#[test]
fn malformed_task_references_are_invalid() {
    let mut h = two_runs_with_a();
    for reference in ["", "r1/", "/a", "../x", "a b", "r1/a/b", "r1\n/a"] {
        let err = h
            .call("task.tail", obj(vec![("task", s(reference))]))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid, "{reference:?}");
    }
    assert_eq!(
        h.call("task.tail", Value::Null).unwrap_err().code,
        ErrorCode::Invalid
    );
}

#[test]
fn a_bare_task_id_that_exists_in_several_runs_is_ambiguous_and_must_be_qualified() {
    let mut h = two_runs_with_a();
    let err = h
        .call("task.cancel", obj(vec![("task", s("a"))]))
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid);
    assert!(
        err.message.contains("r1")
            && err.message.contains("r2")
            && err.message.contains("<run>/<task>"),
        "{}",
        err.message
    );
    assert_eq!(
        status_of(&mut h, "r1", "a"),
        "ready",
        "an ambiguous reference changes nothing"
    );
    assert_eq!(status_of(&mut h, "r2", "a"), "ready");

    h.call("task.cancel", obj(vec![("task", s("r2/a"))]))
        .unwrap();
    assert_eq!(status_of(&mut h, "r2", "a"), "failed");
    assert_eq!(
        status_of(&mut h, "r1", "a"),
        "ready",
        "only the qualified task was cancelled"
    );
}

#[test]
fn a_bare_task_id_that_is_unique_resolves_without_a_run() {
    let mut h = two_runs_with_a();
    let tail = h.call("task.tail", obj(vec![("task", s("b"))])).unwrap();
    assert_eq!(
        tail.data().get("task").and_then(Value::as_str),
        Some("r2/b")
    );
    h.call("task.cancel", obj(vec![("task", s("b"))])).unwrap();
    assert_eq!(status_of(&mut h, "r2", "b"), "failed");
}

// ---- undo ---------------------------------------------------------------------------------------------

#[test]
fn undo_is_unavailable_without_the_tool() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    start(&mut h);
    finish_ok(&mut h, "a");
    let events = h.world.stored_events().len();
    let err = h.call("undo", obj(vec![("task", s("a"))])).unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    assert!(err.message.contains("agent-undo"), "{}", err.message);
    assert!(h.world.undo_calls().is_empty());
    assert_eq!(h.world.stored_events().len(), events);
    assert_eq!(
        count(&h, "checkpoint_recorded"),
        0,
        "no checkpoints without the tool"
    );
    let st = status(&mut h);
    assert_eq!(
        st.get("daemon")
            .and_then(|d| d.get("undo"))
            .and_then(Value::as_str),
        Some("unavailable")
    );
}

#[test]
fn undo_previews_by_default_and_applies_only_when_asked() {
    let mut h = Harness::new();
    h.world.set_undo(true);
    new_run(&mut h);
    add(&mut h, "a", &[]);
    start(&mut h);
    finish_ok(&mut h, "a");
    let wt = "/repo/.fleet-worktrees/r1/a";

    let preview = h.call("undo", obj(vec![("task", s("a"))])).unwrap();
    assert_eq!(
        preview.data().get("applied").and_then(Value::as_bool),
        Some(false)
    );
    assert!(preview
        .data()
        .get("report")
        .and_then(Value::as_str)
        .unwrap()
        .contains("apply=false"));
    assert_eq!(
        preview.data().get("task").and_then(Value::as_str),
        Some("r1/a")
    );
    let explicit_preview = h
        .call(
            "undo",
            obj(vec![("task", s("a")), ("apply", Value::Bool(false))]),
        )
        .unwrap();
    assert_eq!(
        explicit_preview
            .data()
            .get("applied")
            .and_then(Value::as_bool),
        Some(false)
    );
    let applied = h
        .call(
            "undo",
            obj(vec![("task", s("a")), ("apply", Value::Bool(true))]),
        )
        .unwrap();
    assert_eq!(
        applied.data().get("applied").and_then(Value::as_bool),
        Some(true)
    );

    let calls: Vec<String> = h
        .world
        .undo_calls()
        .into_iter()
        .filter(|c| c.starts_with("undo "))
        .collect();
    assert_eq!(
        calls,
        vec![
            format!("undo {wt} apply=false"),
            format!("undo {wt} apply=false"),
            format!("undo {wt} apply=true")
        ]
    );
    assert_eq!(
        h.call("undo", obj(vec![("task", s("a")), ("apply", s("yes"))]))
            .unwrap_err()
            .code,
        ErrorCode::Invalid
    );
}

#[test]
fn undo_is_refused_while_the_task_runs() {
    let mut h = Harness::new();
    h.world.set_undo(true);
    new_run(&mut h);
    add(&mut h, "a", &[]);
    start(&mut h);
    let err = h
        .call(
            "undo",
            obj(vec![("task", s("a")), ("apply", Value::Bool(true))]),
        )
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert!(err.message.contains("running"), "{}", err.message);
    assert!(h.world.undo_calls().iter().all(|c| !c.starts_with("undo ")));
}

#[test]
fn undo_needs_a_worktree() {
    let mut h = Harness::new();
    h.world.set_undo(true);
    new_run(&mut h);
    add(&mut h, "a", &[]);
    let err = h.call("undo", obj(vec![("task", s("a"))])).unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert!(err.message.contains("no worktree"), "{}", err.message);
}

#[test]
fn checkpoints_are_recorded_before_dispatch_and_after_review_when_the_tool_is_there() {
    let mut h = Harness::new();
    h.world.set_undo(true);
    new_run(&mut h);
    add(&mut h, "a", &[]);
    start(&mut h);
    finish_ok(&mut h, "a");

    let labels: Vec<String> = h
        .world
        .stored_events()
        .into_iter()
        .filter_map(|e| match e {
            DomainEvent::CheckpointRecorded { label, .. } => Some(label),
            _ => None,
        })
        .collect();
    assert_eq!(labels, vec!["before".to_string(), "after".to_string()]);
    let kinds = h.world.event_kinds();
    let first = kinds
        .iter()
        .position(|k| *k == "checkpoint_recorded")
        .unwrap();
    let last = kinds
        .iter()
        .rposition(|k| *k == "checkpoint_recorded")
        .unwrap();
    let dispatched = kinds.iter().position(|k| *k == "task_dispatched").unwrap();
    let committed = kinds.iter().position(|k| *k == "task_committed").unwrap();
    assert!(
        first < dispatched,
        "the 'before' checkpoint precedes the worker"
    );
    assert!(
        last > committed,
        "the 'after' checkpoint follows the commit of the worker's output"
    );

    let wt = "/repo/.fleet-worktrees/r1/a";
    assert_eq!(
        h.world.undo_calls(),
        vec![
            format!("checkpoint {wt} fleet before r1/a"),
            format!("checkpoint {wt} fleet after r1/a")
        ]
    );
}

#[test]
fn every_attempt_gets_its_own_before_checkpoint() {
    let mut h = Harness::new();
    h.world.set_undo(true);
    new_run(&mut h);
    add(&mut h, "a", &[]);
    start(&mut h);
    let id = h.world.session_for("a").unwrap();
    h.world.exit(id, 1);
    h.orch.tick();
    let before = h
        .world
        .undo_calls()
        .iter()
        .filter(|c| c.contains("fleet before"))
        .count();
    assert_eq!(before, 2);
}

#[test]
fn a_failing_undo_tool_never_blocks_dispatch() {
    let mut h = Harness::new();
    h.world.set_undo(true);
    h.world.undo_fails(true);
    new_run(&mut h);
    add(&mut h, "a", &[]);
    start(&mut h);
    assert_eq!(
        task_status(&mut h, "a"),
        "running",
        "the worker started although the checkpoint failed"
    );
    assert_eq!(count(&h, "checkpoint_recorded"), 0);
    let w = warnings(&h);
    assert!(
        w.iter()
            .any(|m| m.contains("r1/a") && m.contains("undo checkpoint failed")),
        "{w:?}"
    );

    finish_ok(&mut h, "a");
    assert_eq!(
        task_status(&mut h, "a"),
        "completed",
        "and the review did not depend on it either"
    );
    let err = h.call("undo", obj(vec![("task", s("a"))])).unwrap_err();
    assert_eq!(
        err.code,
        ErrorCode::Internal,
        "an explicit undo reports the tool's failure"
    );
}
