//! Application tests: supervision of running workers (timeouts, kill escalation, exits, stops,
//! start failures and what a worker's files change), driven through the command interface with
//! in-memory fakes for every port.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_app::ports::{SessionId, Signal};
use fleet_app::testing::{test_config, Harness};
use fleet_domain::contract::{
    ContractRead, QuestionKind, ResultStatus, WorkerQuestion, WorkerResult,
};
use fleet_domain::error::ErrorCode;
use fleet_domain::events::DomainEvent;
use fleet_domain::status::EndKind;
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

fn failure(h: &mut Harness, id: &str) -> String {
    task_doc(h, id)
        .get("failure")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn cwd(h: &Harness, id: SessionId) -> String {
    h.world.session_spec(id).unwrap().cwd
}

fn finish_ok(h: &mut Harness, task: &str) {
    let session = h.world.session_for(task).expect("task has a live session");
    let dir = cwd(h, session);
    h.world.write_result(
        &dir,
        ContractRead::Valid(WorkerResult {
            status: ResultStatus::Done,
            summary: format!("{task} done"),
        }),
    );
    h.world.mark_dirty(&dir);
    h.world.exit(session, 0);
    h.orch.tick();
}

fn count(h: &Harness, kind: &str) -> usize {
    h.world.event_kinds().iter().filter(|k| **k == kind).count()
}

fn ended(h: &Harness) -> Vec<EndKind> {
    h.world
        .stored_events()
        .into_iter()
        .filter_map(|e| match e {
            DomainEvent::SessionEnded { outcome, .. } => Some(outcome),
            _ => None,
        })
        .collect()
}

fn ended_tails(h: &Harness) -> Vec<String> {
    h.world
        .stored_events()
        .into_iter()
        .filter_map(|e| match e {
            DomainEvent::SessionEnded { output_tail, .. } => Some(output_tail),
            _ => None,
        })
        .collect()
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

fn question(text: &str) -> WorkerQuestion {
    WorkerQuestion {
        kind: QuestionKind::Question,
        text: text.to_string(),
        choices: vec![],
        action: None,
    }
}

/// A started run with one task `a` whose worker is live.
fn boot() -> (Harness, SessionId) {
    boot_with(test_config())
}

fn boot_with(cfg: fleet_app::config::AppConfig) -> (Harness, SessionId) {
    let mut h = Harness::with_config(cfg);
    new_run(&mut h);
    add(&mut h, "a", &[]);
    start(&mut h);
    let id = h.world.session_for("a").expect("a is running");
    (h, id)
}

fn stop(
    h: &mut Harness,
    task: &str,
) -> Result<fleet_app::api::Reply, fleet_domain::error::FleetError> {
    h.call("task.stop", obj(vec![("task", s(task))]))
}

// ---- timeouts and kill escalation -------------------------------------------------------------

#[test]
fn the_total_timeout_sends_term_first_and_ends_the_session_as_a_timeout() {
    let (mut h, id) = boot();
    h.world.set_timing(id, 60_000, 0);
    h.orch.tick();
    assert_eq!(h.world.signals(id), vec![Signal::Term]);
    assert!(
        ended(&h).is_empty(),
        "the session is only over once the exit was observed"
    );
    h.orch.tick();
    assert_eq!(ended(&h), vec![EndKind::Timeout]);
    assert_eq!(
        h.world.signals(id),
        vec![Signal::Term],
        "a worker that stops on TERM is never killed"
    );
    // the timeout counts as a failed attempt and the task is queued again
    assert_eq!(count(&h, "retry_queued"), 1);
    assert_eq!(retries_used(&mut h, "a"), 1);
    assert_eq!(attempt(&mut h, "a"), 2);
    assert_eq!(task_status(&mut h, "a"), "running");
}

#[test]
fn the_idle_timeout_ends_the_session_as_idle_timeout() {
    let (mut h, id) = boot();
    h.world.set_timing(id, 1_000, 30_000);
    h.orch.tick();
    assert_eq!(h.world.signals(id), vec![Signal::Term]);
    h.orch.tick();
    assert_eq!(ended(&h), vec![EndKind::IdleTimeout]);
    assert_eq!(retries_used(&mut h, "a"), 1);
}

#[test]
fn the_total_timeout_wins_when_both_limits_are_exceeded() {
    let (mut h, id) = boot();
    h.world.set_timing(id, 60_000, 30_000);
    h.orch.tick();
    h.orch.tick();
    assert_eq!(ended(&h), vec![EndKind::Timeout]);
}

#[test]
fn a_worker_just_inside_its_limits_is_left_alone() {
    let (mut h, id) = boot();
    h.world.set_timing(id, 59_999, 29_999);
    for _ in 0..3 {
        h.orch.tick();
    }
    assert!(h.world.signals(id).is_empty());
    assert!(ended(&h).is_empty());
    assert_eq!(task_status(&mut h, "a"), "running");
    assert_eq!(attempt(&mut h, "a"), 1);
}

#[test]
fn a_worker_without_limits_is_never_timed_out() {
    let mut cfg = test_config();
    for w in cfg.workers.values_mut() {
        w.timeout_ms = None;
        w.idle_timeout_ms = None;
    }
    let (mut h, id) = boot_with(cfg);
    h.world.set_timing(id, u64::MAX / 2, u64::MAX / 2);
    h.orch.tick();
    h.orch.tick();
    assert!(h.world.signals(id).is_empty());
    assert_eq!(task_status(&mut h, "a"), "running");
}

#[test]
fn a_worker_that_ignores_term_is_killed_after_the_grace_period() {
    let (mut h, id) = boot();
    h.world.configure_session(id, false, true, false);
    h.world.set_timing(id, 60_000, 0);
    h.orch.tick();
    assert_eq!(h.world.signals(id), vec![Signal::Term]);
    h.world.advance(999);
    h.orch.tick();
    assert_eq!(
        h.world.signals(id),
        vec![Signal::Term],
        "KILL waits for the full grace period"
    );
    h.world.advance(1);
    h.orch.tick();
    assert_eq!(h.world.signals(id), vec![Signal::Term, Signal::Kill]);
    h.orch.tick();
    assert_eq!(
        ended(&h),
        vec![EndKind::Timeout],
        "the end is still attributed to the timeout"
    );
}

#[test]
fn a_worker_that_ignores_both_signals_ends_as_kill_failed_and_the_task_retries() {
    let (mut h, id) = boot();
    h.world.configure_session(id, false, false, false);
    h.world.set_timing(id, 60_000, 0);
    h.orch.tick();
    h.world.advance(1000);
    h.orch.tick();
    assert_eq!(h.world.signals(id), vec![Signal::Term, Signal::Kill]);
    h.world.advance(999);
    h.orch.tick();
    assert!(
        ended(&h).is_empty(),
        "the escalation does not give up early"
    );
    h.world.advance(1);
    h.orch.tick();
    assert_eq!(ended(&h), vec![EndKind::KillFailed]);
    assert_eq!(
        h.world.signals(id),
        vec![Signal::Term, Signal::Kill],
        "no further signals after giving up"
    );
    assert_eq!(retries_used(&mut h, "a"), 1);
    assert_eq!(attempt(&mut h, "a"), 2);
    assert_eq!(task_status(&mut h, "a"), "running");
    assert_ne!(
        h.world.session_for("a"),
        Some(id),
        "a fresh session was started"
    );
}

#[test]
fn unkillable_workers_use_up_the_retry_budget_and_then_the_task_fails() {
    let (mut h, _) = boot();
    for _ in 0..3 {
        let id = h
            .world
            .session_for("a")
            .expect("a live session for every attempt");
        h.world.configure_session(id, false, false, false);
        h.world.set_timing(id, 60_000, 0);
        h.orch.tick();
        h.world.advance(1000);
        h.orch.tick();
        h.world.advance(1000);
        h.orch.tick();
    }
    assert_eq!(ended(&h), vec![EndKind::KillFailed; 3]);
    assert_eq!(task_status(&mut h, "a"), "failed");
    assert_eq!(failure(&mut h, "a"), "worker did not stop when asked to");
    assert!(h.world.live_session_ids().is_empty());
    h.orch.tick();
    assert_eq!(count(&h, "task_dispatched"), 3, "no fourth attempt");
}

#[test]
fn refused_signals_produce_a_warning_and_are_never_recorded_as_sent() {
    let (mut h, id) = boot();
    h.world.configure_session(id, true, true, true);
    h.world.set_timing(id, 60_000, 0);
    h.orch.tick();
    assert!(
        h.world.signals(id).is_empty(),
        "the host refused: nothing was signalled"
    );
    let w = warnings(&h);
    assert_eq!(w.len(), 1, "{w:?}");
    assert!(
        w[0].contains("not signalled") && w[0].contains("pid reused"),
        "{}",
        w[0]
    );
    assert!(w[0].contains("r1/a"));
    h.world.advance(1000);
    h.orch.tick();
    assert!(h.world.signals(id).is_empty());
    assert!(ended(&h).is_empty());
    // nothing ever stops it, so the escalation gives up and reports it honestly
    h.world.advance(1000);
    h.orch.tick();
    assert_eq!(ended(&h), vec![EndKind::KillFailed]);
    assert!(h.world.signals(id).is_empty());
}

// ---- operator stop ---------------------------------------------------------------------------

#[test]
fn stopping_a_running_task_fails_it_without_a_retry() {
    let (mut h, id) = boot();
    let reply = stop(&mut h, "a").unwrap();
    assert_eq!(
        reply.data().get("status").and_then(Value::as_str),
        Some("stopping")
    );
    assert_eq!(
        task_status(&mut h, "a"),
        "running",
        "the task ends when the session does"
    );
    h.orch.tick();
    assert_eq!(h.world.signals(id), vec![Signal::Term]);
    h.orch.tick();
    assert_eq!(ended(&h), vec![EndKind::Stopped]);
    assert_eq!(task_status(&mut h, "a"), "failed");
    assert_eq!(failure(&mut h, "a"), "stopped by an operator");
    assert_eq!(retries_used(&mut h, "a"), 0);
    assert_eq!(count(&h, "retry_queued"), 0);
    assert_eq!(h.world.session_ids().len(), 1, "no new session is started");
    assert_eq!(h.orch.live_count(), 0);
    h.orch.tick();
    assert_eq!(h.world.session_ids().len(), 1);
}

#[test]
fn stop_escalates_to_kill_for_a_worker_that_ignores_term() {
    let (mut h, id) = boot();
    h.world.configure_session(id, false, true, false);
    stop(&mut h, "a").unwrap();
    h.orch.tick();
    assert_eq!(h.world.signals(id), vec![Signal::Term]);
    h.world.advance(1000);
    h.orch.tick();
    assert_eq!(h.world.signals(id), vec![Signal::Term, Signal::Kill]);
    h.orch.tick();
    assert_eq!(ended(&h), vec![EndKind::Stopped]);
    assert_eq!(task_status(&mut h, "a"), "failed");
}

#[test]
fn stopping_twice_signals_once() {
    let (mut h, id) = boot();
    stop(&mut h, "a").unwrap();
    stop(&mut h, "a").unwrap();
    h.orch.tick();
    h.orch.tick();
    assert_eq!(h.world.signals(id), vec![Signal::Term]);
    assert_eq!(ended(&h), vec![EndKind::Stopped]);
}

#[test]
fn a_stop_wins_over_a_question_the_worker_left_behind() {
    let (mut h, id) = boot();
    let dir = cwd(&h, id);
    h.world
        .write_question(&dir, ContractRead::Valid(question("Which database?")));
    stop(&mut h, "a").unwrap();
    h.orch.tick();
    h.orch.tick();
    assert_eq!(task_status(&mut h, "a"), "failed");
    assert_eq!(
        count(&h, "decision_opened"),
        0,
        "a stopped task does not ask anything"
    );
}

#[test]
fn stopping_a_ready_task_cancels_it() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    assert_eq!(task_status(&mut h, "a"), "ready");
    stop(&mut h, "a").unwrap();
    assert_eq!(task_status(&mut h, "a"), "failed");
    assert_eq!(failure(&mut h, "a"), "cancelled by an operator");
    assert_eq!(count(&h, "session_ended"), 0);
}

#[test]
fn stopping_a_pending_task_cancels_it() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    add(&mut h, "b", &["a"]);
    start(&mut h);
    assert_eq!(task_status(&mut h, "b"), "pending");
    stop(&mut h, "b").unwrap();
    assert_eq!(task_status(&mut h, "b"), "failed");
    assert_eq!(failure(&mut h, "b"), "cancelled by an operator");
    assert_eq!(
        task_status(&mut h, "a"),
        "running",
        "other tasks are not affected"
    );
}

#[test]
fn stopping_a_blocked_task_cancels_it() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    add(&mut h, "b", &["a"]);
    stop(&mut h, "a").unwrap();
    assert_eq!(
        task_status(&mut h, "b"),
        "blocked",
        "a failed dependency blocks its dependents"
    );
    stop(&mut h, "b").unwrap();
    assert_eq!(task_status(&mut h, "b"), "failed");
    assert_eq!(failure(&mut h, "b"), "cancelled by an operator");
}

#[test]
fn stopping_a_completed_or_failed_task_is_a_conflict() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    add(&mut h, "b", &[]);
    start(&mut h);
    finish_ok(&mut h, "a");
    assert_eq!(task_status(&mut h, "a"), "completed");
    let err = stop(&mut h, "a").unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert!(
        err.message.contains("only running tasks can be stopped"),
        "{}",
        err.message
    );
    assert_eq!(task_status(&mut h, "a"), "completed");

    stop(&mut h, "b").unwrap();
    h.orch.tick();
    h.orch.tick();
    assert_eq!(task_status(&mut h, "b"), "failed");
    assert_eq!(stop(&mut h, "b").unwrap_err().code, ErrorCode::Conflict);
}

#[test]
fn stopping_an_unknown_task_is_not_found() {
    let (mut h, _) = boot();
    assert_eq!(stop(&mut h, "zzz").unwrap_err().code, ErrorCode::NotFound);
    assert_eq!(
        stop(&mut h, "r1/zzz").unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(stop(&mut h, "r9/a").unwrap_err().code, ErrorCode::NotFound);
    assert_eq!(task_status(&mut h, "a"), "running");
}

// ---- exits -----------------------------------------------------------------------------------

#[test]
fn a_worker_killed_by_a_signal_is_a_failed_attempt() {
    let (mut h, id) = boot();
    h.world.exit_by_signal(id, "SIGSEGV");
    h.orch.tick();
    assert_eq!(ended(&h), vec![EndKind::Signaled]);
    assert_eq!(retries_used(&mut h, "a"), 1);
    assert!(
        last_brief(&h).contains("worker was killed by a signal"),
        "{}",
        last_brief(&h)
    );
    assert_eq!(task_status(&mut h, "a"), "running");
}

#[test]
fn a_nonzero_exit_is_retried_up_to_max_retries_and_then_the_task_fails() {
    let (mut h, _) = boot();
    for round in 1..=3u64 {
        let id = h
            .world
            .session_for("a")
            .expect("a live session for every attempt");
        h.world.exit(id, 3);
        h.orch.tick();
        if round < 3 {
            assert_eq!(task_status(&mut h, "a"), "running", "round {round}");
            assert_eq!(retries_used(&mut h, "a"), round);
            assert_eq!(attempt(&mut h, "a"), round + 1);
        }
    }
    assert_eq!(task_status(&mut h, "a"), "failed");
    assert_eq!(failure(&mut h, "a"), "worker exited with code 3");
    assert_eq!(count(&h, "task_dispatched"), 3);
    assert_eq!(count(&h, "retry_queued"), 2);
    assert!(h.world.live_session_ids().is_empty());
    h.orch.tick();
    assert_eq!(
        count(&h, "task_dispatched"),
        3,
        "a failed task is not started again"
    );
}

#[test]
fn with_no_retries_the_first_failure_is_final() {
    let mut cfg = test_config();
    cfg.max_retries = 0;
    let (mut h, id) = boot_with(cfg);
    h.world.exit(id, 1);
    h.orch.tick();
    assert_eq!(task_status(&mut h, "a"), "failed");
    assert_eq!(count(&h, "retry_queued"), 0);
}

#[test]
fn the_retry_note_carries_the_exit_code_and_the_last_output() {
    let (mut h, id) = boot();
    h.world
        .print(id, "compiling...\nerror: cannot find crate `serde`\n");
    h.world.exit(id, 2);
    h.orch.tick();
    let brief = last_brief(&h);
    assert!(
        brief.contains("Attempt 1 failed: worker exited with code 2."),
        "{brief}"
    );
    assert!(brief.contains("Last output:"), "{brief}");
    assert!(brief.contains("cannot find crate `serde`"), "{brief}");
    assert!(brief.contains("This is attempt 2."), "{brief}");
    let saved = h
        .world
        .saved_log("r1", "a")
        .expect("the output of the ended attempt is kept");
    assert!(saved.contains("cannot find crate"));
}

#[test]
fn a_clean_exit_without_a_result_file_completes_the_task() {
    let (mut h, id) = boot();
    h.world.exit(id, 0);
    h.orch.tick();
    assert_eq!(task_status(&mut h, "a"), "completed");
    assert!(task_doc(&mut h, "a").get("summary").unwrap().is_null());
    assert_eq!(ended(&h), vec![EndKind::Exited]);
}

#[test]
fn a_result_reporting_failure_with_exit_code_zero_is_a_failure() {
    let (mut h, id) = boot();
    let dir = cwd(&h, id);
    h.world.write_result(
        &dir,
        ContractRead::Valid(WorkerResult {
            status: ResultStatus::Failed,
            summary: "the tests cannot pass".into(),
        }),
    );
    h.world.exit(id, 0);
    h.orch.tick();
    assert_eq!(retries_used(&mut h, "a"), 1);
    assert_eq!(count(&h, "task_reviewing"), 0);
    let brief = last_brief(&h);
    assert!(
        brief.contains("worker reported failure: the tests cannot pass"),
        "{brief}"
    );
}

#[test]
fn a_reported_failure_exhausts_retries_like_any_other_failure() {
    let (mut h, _) = boot();
    for _ in 0..3 {
        let id = h
            .world
            .session_for("a")
            .expect("a live session for every attempt");
        let dir = cwd(&h, id);
        h.world.write_result(
            &dir,
            ContractRead::Valid(WorkerResult {
                status: ResultStatus::Failed,
                summary: "gave up".into(),
            }),
        );
        h.world.exit(id, 0);
        h.orch.tick();
    }
    assert_eq!(task_status(&mut h, "a"), "failed");
    assert_eq!(failure(&mut h, "a"), "worker reported failure: gave up");
}

#[test]
fn a_result_saying_done_never_hides_a_nonzero_exit() {
    let (mut h, id) = boot();
    let dir = cwd(&h, id);
    h.world.write_result(
        &dir,
        ContractRead::Valid(WorkerResult {
            status: ResultStatus::Done,
            summary: "all good".into(),
        }),
    );
    h.world.exit(id, 1);
    h.orch.tick();
    assert_eq!(retries_used(&mut h, "a"), 1);
    assert_eq!(count(&h, "task_reviewing"), 0);
    assert!(last_brief(&h).contains("worker exited with code 1"));
}

#[test]
fn an_invalid_result_file_warns_but_the_task_follows_the_exit_code() {
    let (mut h, id) = boot();
    let dir = cwd(&h, id);
    h.world
        .write_result(&dir, ContractRead::Invalid("not valid JSON".into()));
    h.world.exit(id, 0);
    h.orch.tick();
    let w = warnings(&h);
    assert!(
        w.iter().any(|m| m.contains("r1/a")
            && m.contains("result.json ignored")
            && m.contains("not valid JSON")),
        "{w:?}"
    );
    assert_eq!(task_status(&mut h, "a"), "completed");
}

#[test]
fn an_invalid_result_file_does_not_rescue_a_failing_exit() {
    let (mut h, id) = boot();
    let dir = cwd(&h, id);
    h.world.write_result(
        &dir,
        ContractRead::Invalid("status must be done or failed".into()),
    );
    h.world.exit(id, 4);
    h.orch.tick();
    assert_eq!(retries_used(&mut h, "a"), 1);
    assert!(warnings(&h)
        .iter()
        .any(|m| m.contains("result.json ignored")));
}

#[test]
fn an_unreadable_question_file_is_ignored_with_a_warning() {
    let (mut h, id) = boot();
    let dir = cwd(&h, id);
    h.world.write_question(
        &dir,
        ContractRead::Invalid("kind must be question or approval".into()),
    );
    h.world.exit(id, 0);
    h.orch.tick();
    let w = warnings(&h);
    assert!(
        w.iter().any(|m| m.contains("question.json ignored")),
        "{w:?}"
    );
    assert_eq!(count(&h, "decision_opened"), 0);
    assert_eq!(task_status(&mut h, "a"), "completed");
}

// ---- start failures ---------------------------------------------------------------------------

#[test]
fn spawn_failures_consume_retries_and_end_failed_with_the_error_in_the_note() {
    let mut h = Harness::new();
    h.world.fail_spawn(Some("no such program: run-agent"));
    new_run(&mut h);
    add(&mut h, "a", &[]);
    start(&mut h);
    for _ in 0..6 {
        h.orch.tick();
    }
    assert_eq!(task_status(&mut h, "a"), "failed");
    assert_eq!(failure(&mut h, "a"), "worker could not be started");
    assert_eq!(
        ended(&h),
        vec![EndKind::SpawnFailed; 3],
        "one recorded attempt per try, then it stops"
    );
    assert_eq!(count(&h, "task_dispatched"), 3);
    assert_eq!(count(&h, "retry_queued"), 2);
    assert!(h.world.session_ids().is_empty());
    for tail in ended_tails(&h) {
        assert!(
            tail.contains("could not start the worker: no such program: run-agent"),
            "{tail}"
        );
    }
    let briefs = h.world.briefs();
    assert_eq!(briefs.len(), 3);
    assert!(
        briefs[1].1.contains("no such program: run-agent"),
        "the note reaches the next brief"
    );
}

#[test]
fn a_task_that_failed_to_start_runs_after_an_operator_retry_once_the_cause_is_fixed() {
    let mut h = Harness::new();
    h.world.fail_spawn(Some("boom"));
    new_run(&mut h);
    add(&mut h, "a", &[]);
    start(&mut h);
    for _ in 0..4 {
        h.orch.tick();
    }
    assert_eq!(task_status(&mut h, "a"), "failed");
    h.world.fail_spawn(None);
    h.call("task.retry", obj(vec![("task", s("a"))])).unwrap();
    assert_eq!(task_status(&mut h, "a"), "running");
    assert_eq!(retries_used(&mut h, "a"), 0);
    finish_ok(&mut h, "a");
    assert_eq!(task_status(&mut h, "a"), "completed");
}

#[test]
fn a_worktree_failure_is_bounded_and_reported_like_a_start_failure() {
    let mut h = Harness::new();
    h.world.fail_prepare_worktree(true);
    new_run(&mut h);
    add(&mut h, "a", &[]);
    start(&mut h);
    for _ in 0..6 {
        h.orch.tick();
    }
    assert_eq!(task_status(&mut h, "a"), "failed");
    assert_eq!(ended(&h), vec![EndKind::SpawnFailed; 3]);
    assert_eq!(count(&h, "task_dispatched"), 3);
    assert!(h.world.session_ids().is_empty());
    assert!(
        h.world.briefs().is_empty(),
        "no brief is written without a worktree"
    );
    for tail in ended_tails(&h) {
        assert!(
            tail.contains("could not prepare the worktree: git worktree add failed"),
            "{tail}"
        );
    }
    h.world.fail_prepare_worktree(false);
    h.call("task.retry", obj(vec![("task", s("a"))])).unwrap();
    assert_eq!(task_status(&mut h, "a"), "running");
}

#[test]
fn a_failure_to_prepare_the_worker_files_is_bounded_and_reported() {
    let mut h = Harness::new();
    h.world.fail_contract_prepare(true);
    new_run(&mut h);
    add(&mut h, "a", &[]);
    start(&mut h);
    for _ in 0..6 {
        h.orch.tick();
    }
    assert_eq!(task_status(&mut h, "a"), "failed");
    assert_eq!(ended(&h), vec![EndKind::SpawnFailed; 3]);
    assert!(
        h.world.session_ids().is_empty(),
        "no process is started without its brief"
    );
    for tail in ended_tails(&h) {
        assert!(
            tail.contains("could not prepare .fleet: .fleet is not writable"),
            "{tail}"
        );
    }
}

#[test]
fn a_start_failure_of_one_task_does_not_stop_the_others() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    start(&mut h);
    assert_eq!(task_status(&mut h, "a"), "running");
    h.world.fail_spawn(Some("boom"));
    add(&mut h, "b", &[]);
    h.orch.tick();
    assert_eq!(task_status(&mut h, "a"), "running");
    assert_eq!(count(&h, "retry_queued"), 1, "only b failed to start");
}

// ---- a question file while the worker runs -----------------------------------------------------

#[test]
fn a_question_file_while_the_worker_runs_stops_it_after_the_grace_period_and_opens_the_gate() {
    let (mut h, id) = boot();
    let dir = cwd(&h, id);
    h.world
        .write_question(&dir, ContractRead::Valid(question("Which database?")));
    h.orch.tick();
    assert!(
        h.world.signals(id).is_empty(),
        "the worker gets a moment to exit by itself"
    );
    h.world.advance(499);
    h.orch.tick();
    assert!(h.world.signals(id).is_empty());
    h.world.advance(1);
    h.orch.tick();
    assert_eq!(h.world.signals(id), vec![Signal::Term]);
    h.orch.tick();

    assert_eq!(task_status(&mut h, "a"), "blocked");
    let block = task_doc(&mut h, "a").get("block").cloned().unwrap();
    assert_eq!(block.get("kind").and_then(Value::as_str), Some("question"));
    let gates = h
        .call("gate.list", Value::Null)
        .unwrap()
        .data()
        .get("gates")
        .and_then(Value::as_array)
        .unwrap()
        .to_vec();
    assert_eq!(gates.len(), 1);
    assert_eq!(
        gates[0].get("text").and_then(Value::as_str),
        Some("Which database?")
    );
    assert_eq!(retries_used(&mut h, "a"), 0, "a question is not a failure");
    assert_eq!(count(&h, "retry_queued"), 0);
}

#[test]
fn a_worker_that_exits_on_its_own_within_the_grace_period_is_not_signalled() {
    let (mut h, id) = boot();
    let dir = cwd(&h, id);
    h.world
        .write_question(&dir, ContractRead::Valid(question("Which database?")));
    h.orch.tick();
    h.world.exit(id, 0);
    h.orch.tick();
    assert!(h.world.signals(id).is_empty());
    assert_eq!(task_status(&mut h, "a"), "blocked");
    assert_eq!(ended(&h), vec![EndKind::Exited]);
}

// ---- capacity ---------------------------------------------------------------------------------

#[test]
fn live_sessions_never_exceed_max_sessions() {
    let mut cfg = test_config();
    cfg.max_sessions = 2;
    let mut h = Harness::with_config(cfg);
    h.call(
        "run.create",
        obj(vec![
            ("objective", s("o")),
            ("repo", s("/repo")),
            ("max_parallel", Value::int(6u64)),
        ]),
    )
    .unwrap();
    let names = ["a", "b", "c", "d", "e", "f"];
    for n in names {
        add(&mut h, n, &[]);
    }
    start(&mut h);
    let mut finished = 0;
    loop {
        let live = h.world.live_session_ids();
        assert!(live.len() <= 2, "{} sessions are live", live.len());
        assert!(h.orch.live_count() <= 2);
        if live.is_empty() {
            break;
        }
        let name = names
            .iter()
            .find(|t| h.world.session_for(t).is_some())
            .expect("a live task");
        finish_ok(&mut h, name);
        finished += 1;
        assert!(finished <= names.len(), "more completions than tasks");
    }
    assert_eq!(finished, names.len());
    for n in names {
        assert_eq!(task_status(&mut h, n), "completed");
    }
}

#[test]
fn retries_do_not_exceed_the_session_cap_either() {
    let mut cfg = test_config();
    cfg.max_sessions = 2;
    let mut h = Harness::with_config(cfg);
    new_run(&mut h);
    for n in ["a", "b", "c"] {
        add(&mut h, n, &[]);
    }
    start(&mut h);
    assert_eq!(h.world.live_session_ids().len(), 2);
    for _ in 0..3 {
        let id = h.world.session_for("a").expect("a is live");
        h.world.exit(id, 1);
        h.orch.tick();
        assert!(h.world.live_session_ids().len() <= 2);
        assert!(h.orch.live_count() <= 2);
    }
    assert_eq!(task_status(&mut h, "a"), "failed");
    assert!(
        h.world.session_for("c").is_some(),
        "the freed slot goes to the waiting task"
    );
}
