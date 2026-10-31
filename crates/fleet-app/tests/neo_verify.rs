//! Application tests of verification contracts: the review flow (a task with a contract
//! completes only after fleet's own run of the contract passed), evidence, retries with the
//! failure in the brief, `verify` on demand, restarts, and every way the rule could be
//! sidestepped. Driven through the command interface with in-memory fakes for every port.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_app::config::{AppConfig, GateDef};
use fleet_app::ports::{Job, VerifyOrigin};
use fleet_app::testing::{test_config, GateMode, Harness, VerifyMode};
use fleet_domain::contract::{ContractRead, ResultStatus, WorkerResult};
use fleet_domain::error::ErrorCode;
use fleet_domain::events::DomainEvent;
use fleet_domain::limits::MAX_CONCURRENT_JOBS;
use fleet_domain::quality::GateFormat;
use fleet_domain::status::Verdict;
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

/// A contract of command checks: `(id, mandatory)`.
fn contract(checks: &[(&str, bool)]) -> Value {
    obj(vec![
        ("version", Value::int(1)),
        (
            "checks",
            Value::array(checks.iter().map(|(id, mandatory)| {
                obj(vec![
                    ("id", s(id)),
                    ("kind", s("command")),
                    ("mandatory", Value::Bool(*mandatory)),
                    ("program", s("/usr/bin/make")),
                    ("args", list(&["test"])),
                ])
            })),
        ),
    ])
}

fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn gate(name: &str) -> GateDef {
    GateDef {
        name: name.to_string(),
        command: "run-tests".to_string(),
        args: vec![],
        env_pass: vec![],
        env_set: vec![],
        format: GateFormat::Exit,
        timeout_ms: 5000,
    }
}

/// Run r1 with task `a` (and `b` after it) and goal `g` with the given contract on `a`; started.
fn governed(checks: &[(&str, bool)], tweak: impl FnOnce(&mut AppConfig)) -> Harness {
    let mut cfg = test_config();
    tweak(&mut cfg);
    let mut h = Harness::with_config(cfg);
    h.call(
        "run.create",
        obj(vec![("objective", s("ship")), ("repo", s("/repo"))]),
    )
    .unwrap();
    for (id, after) in [("a", vec![]), ("b", vec!["a"])] {
        h.call(
            "task.add",
            obj(vec![
                ("run", s("r1")),
                ("id", s(id)),
                ("title", s(&format!("task {id}"))),
                ("after", list(&after)),
            ]),
        )
        .unwrap();
    }
    h.call(
        "goal.add",
        obj(vec![
            ("id", s("g")),
            ("title", s("the goal")),
            ("tasks", list(&["r1/a"])),
            ("contract", contract(checks)),
        ]),
    )
    .unwrap();
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
    h
}

fn finish_ok(h: &mut Harness, task: &str) {
    let session = h.world.session_for(task).expect("task has a live session");
    let spec = h.world.session_spec(session).unwrap();
    h.world.write_result(
        &spec.cwd,
        ContractRead::Valid(WorkerResult {
            status: ResultStatus::Done,
            summary: format!("{task} done, all tests pass"),
        }),
    );
    h.world.mark_dirty(&spec.cwd);
    h.world.exit(session, 0);
    h.orch.tick();
}

fn ticks(h: &mut Harness) {
    for _ in 0..4 {
        h.orch.tick();
    }
}

fn task_doc(h: &mut Harness, id: &str) -> Value {
    let st = h.call("status", Value::Null).unwrap().data().clone();
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
    text(&task_doc(h, id), "status")
}

fn goal(h: &mut Harness, id: &str) -> Value {
    h.call("goal.show", obj(vec![("goal", s(id))]))
        .unwrap()
        .data()
        .clone()
}

fn goal_status(h: &mut Harness, id: &str) -> String {
    text(&goal(h, id), "status")
}

fn verification(h: &mut Harness, id: &str) -> String {
    let g = goal(h, id);
    text(
        &g.get("tasks").and_then(Value::as_array).unwrap()[0],
        "verification",
    )
}

fn evidence_events(h: &Harness) -> Vec<DomainEvent> {
    h.world
        .stored_events()
        .into_iter()
        .filter(|e| matches!(e, DomainEvent::EvidenceRecorded { .. }))
        .collect()
}

fn count(h: &Harness, kind: &str) -> usize {
    h.world.event_kinds().iter().filter(|k| **k == kind).count()
}

fn last_brief(h: &Harness) -> String {
    h.world
        .briefs()
        .last()
        .expect("a brief was written")
        .1
        .clone()
}

// ---- the rule -------------------------------------------------------------------------------

#[test]
fn a_task_with_a_contract_completes_only_after_fleets_own_run_passed() {
    let mut h = governed(&[("tests", true)], |_| {});
    h.world.set_verify_mode(VerifyMode::Manual);
    finish_ok(&mut h, "a");
    ticks(&mut h);
    // the worker reported success, the work is committed, and the contract is running
    assert_eq!(task_status(&mut h, "a"), "review");
    assert_eq!(h.world.verify_jobs().len(), 1);
    assert_eq!(verification(&mut h, "g"), "unverified");
    assert_eq!(goal_status(&mut h, "g"), "open");
    // no amount of waiting completes it, and the dependent never starts
    for _ in 0..20 {
        h.orch.tick();
    }
    assert_eq!(task_status(&mut h, "a"), "review");
    assert_eq!(task_status(&mut h, "b"), "pending");
    assert_eq!(h.world.verify_jobs().len(), 1, "one run is enough");
    assert_eq!(count(&h, "task_completed"), 0);

    // fleet's own run passes: evidence, the goal is verified, the task completes
    h.world.complete_verify("r1", "a", &[]);
    ticks(&mut h);
    assert_eq!(task_status(&mut h, "a"), "completed");
    assert_eq!(goal_status(&mut h, "g"), "verified");
    assert_eq!(verification(&mut h, "g"), "verified");
    assert_ne!(
        task_status(&mut h, "b"),
        "pending",
        "the dependent is unblocked"
    );
    let kinds = h.world.event_kinds();
    let evidence_at = kinds
        .iter()
        .position(|k| *k == "evidence_recorded")
        .unwrap();
    let completed_at = kinds.iter().position(|k| *k == "task_completed").unwrap();
    assert!(evidence_at < completed_at, "{kinds:?}");
}

#[test]
fn the_default_fake_passes_so_the_whole_flow_runs_in_a_few_ticks() {
    let mut h = governed(&[("tests", true)], |_| {});
    finish_ok(&mut h, "a");
    ticks(&mut h);
    assert_eq!(task_status(&mut h, "a"), "completed");
    assert_eq!(goal_status(&mut h, "g"), "verified");
    assert_eq!(evidence_events(&h).len(), 1);
}

#[test]
fn a_task_without_a_contract_is_not_held_back() {
    let mut h = governed(&[("tests", true)], |_| {});
    h.world.set_verify_mode(VerifyMode::Manual);
    // b has no goal: after a is verified and completed, b runs and completes without a job
    finish_ok(&mut h, "a");
    ticks(&mut h);
    h.world.complete_verify("r1", "a", &[]);
    ticks(&mut h);
    finish_ok(&mut h, "b");
    ticks(&mut h);
    assert_eq!(task_status(&mut h, "b"), "completed");
    assert_eq!(h.world.verify_jobs().len(), 1);
}

#[test]
fn the_job_carries_what_it_needs_and_nothing_secret() {
    let mut h = governed(&[("tests", true), ("lint", false)], |_| {});
    h.world.set_verify_mode(VerifyMode::Manual);
    h.world
        .set_worktree_changes(&["src/a.rs", "docs/b.md"], false);
    finish_ok(&mut h, "a");
    ticks(&mut h);
    let jobs = h.world.verify_jobs();
    let job = &jobs[0];
    assert_eq!(
        (job.run.as_str(), job.task.as_str(), job.attempt),
        ("r1", "a", 1)
    );
    assert_eq!(job.origin, VerifyOrigin::Review);
    assert!(
        job.worktree.ends_with(".fleet-worktrees/r1/a"),
        "{}",
        job.worktree
    );
    assert_eq!(job.state.changed, ["src/a.rs", "docs/b.md"]);
    assert_eq!(job.targets.len(), 1);
    assert_eq!(job.targets[0].goal.as_str(), "g");
    assert_eq!(job.targets[0].contract.checks().len(), 2);
    let env: std::collections::BTreeMap<_, _> = job.env.iter().cloned().collect();
    assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin:/bin"));
    assert_eq!(env.get("FLEET_TASK").map(String::as_str), Some("a"));
    assert!(
        !env.contains_key("SENTINEL_TOKEN"),
        "no secret of the daemon reaches a check"
    );
    assert!(!env
        .keys()
        .any(|k| k.contains("TOKEN") || k.contains("SECRET")));
    assert!(
        !env.contains_key("AGENT_MODE"),
        "worker-only settings stay with the worker"
    );
}

// ---- failure ----------------------------------------------------------------------------------

#[test]
fn a_failed_check_is_evidence_a_retry_and_a_quote_in_the_next_brief() {
    let mut h = governed(&[("tests", true), ("lint", false)], |_| {});
    h.world
        .set_verify_mode(VerifyMode::Fail("assertion failed: 2 + 2 == 5".into()));
    finish_ok(&mut h, "a");
    ticks(&mut h);
    assert_eq!(
        task_status(&mut h, "a"),
        "running",
        "retried after the failure"
    );
    assert_eq!(count(&h, "task_completed"), 0);
    assert_eq!(goal_status(&mut h, "g"), "open");
    assert_eq!(verification(&mut h, "g"), "failed");
    let evidence = evidence_events(&h);
    assert_eq!(
        evidence.len(),
        2,
        "every check is recorded, mandatory or not"
    );
    let DomainEvent::EvidenceRecorded {
        check,
        verdict,
        output,
        run,
        task,
        commit,
        ..
    } = &evidence[0]
    else {
        panic!("not evidence")
    };
    assert_eq!((check.as_str(), *verdict), ("tests", Verdict::Failed));
    assert_eq!(output, "assertion failed: 2 + 2 == 5");
    assert_eq!(run.as_ref().unwrap().as_str(), "r1");
    assert_eq!(task.as_ref().unwrap().as_str(), "a");
    assert_eq!(commit.as_ref().unwrap().as_str(), "d".repeat(40));
    let brief = last_brief(&h);
    assert!(brief.contains("verify g/tests"), "{brief}");
    assert!(
        brief.contains("> assertion failed: 2 + 2 == 5"),
        "the failure is quoted: {brief}"
    );
    assert!(
        !brief.contains("verify g/lint"),
        "an optional check is not a failure of the task"
    );
    // the failure was also recorded as a failed gate
    assert!(h.world.stored_events().iter().any(|e| matches!(
        e,
        DomainEvent::GateChecked { gate, passed: false, .. } if gate == "verify g/tests"
    )));
}

#[test]
fn a_failing_contract_then_a_passing_one() {
    let mut h = governed(&[("tests", true)], |_| {});
    h.world
        .set_verify_mode(VerifyMode::Fail("1 test failed".into()));
    finish_ok(&mut h, "a");
    ticks(&mut h);
    assert_eq!(task_status(&mut h, "a"), "running");
    h.world.set_verify_mode(VerifyMode::Pass);
    finish_ok(&mut h, "a");
    ticks(&mut h);
    assert_eq!(task_status(&mut h, "a"), "completed");
    assert_eq!(goal_status(&mut h, "g"), "verified");
    let evidence = evidence_events(&h);
    let verdicts: Vec<Verdict> = evidence
        .iter()
        .map(|e| match e {
            DomainEvent::EvidenceRecorded { verdict, .. } => *verdict,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(verdicts, [Verdict::Failed, Verdict::Passed]);
}

#[test]
fn a_contract_that_keeps_failing_fails_the_task_when_retries_run_out() {
    let mut h = governed(&[("tests", true)], |c| c.max_retries = 1);
    h.world.set_verify_mode(VerifyMode::Fail("nope".into()));
    finish_ok(&mut h, "a");
    ticks(&mut h);
    finish_ok(&mut h, "a");
    ticks(&mut h);
    assert_eq!(task_status(&mut h, "a"), "failed");
    assert_eq!(goal_status(&mut h, "g"), "open");
    assert_eq!(
        task_status(&mut h, "b"),
        "blocked",
        "the dependent never ran"
    );
}

#[test]
fn an_errored_check_is_not_a_pass() {
    let mut h = governed(&[("tests", true)], |_| {});
    h.world.set_verify_mode(VerifyMode::Manual);
    finish_ok(&mut h, "a");
    ticks(&mut h);
    h.world
        .complete_verify("r1", "a", &[("tests", Verdict::Error)]);
    ticks(&mut h);
    assert_ne!(task_status(&mut h, "a"), "completed");
    assert_eq!(count(&h, "task_completed"), 0);
    assert_eq!(goal_status(&mut h, "g"), "open");
}

#[test]
fn optional_checks_never_decide() {
    let mut h = governed(&[("tests", true), ("lint", false)], |_| {});
    h.world.set_verify_mode(VerifyMode::Manual);
    finish_ok(&mut h, "a");
    ticks(&mut h);
    h.world
        .complete_verify("r1", "a", &[("lint", Verdict::Failed)]);
    ticks(&mut h);
    assert_eq!(task_status(&mut h, "a"), "completed");
    assert_eq!(goal_status(&mut h, "g"), "verified");
}

// ---- gates and verification together ----------------------------------------------------------

#[test]
fn quality_gates_run_after_the_contract_and_both_must_pass() {
    let mut h = governed(&[("tests", true)], |c| c.gates = vec![gate("fmt")]);
    h.world.set_gate_mode(GateMode::Manual);
    h.world.set_verify_mode(VerifyMode::Manual);
    finish_ok(&mut h, "a");
    ticks(&mut h);
    assert!(
        h.world
            .jobs_started()
            .iter()
            .all(|j| !matches!(j, Job::Gates { .. })),
        "gates wait for the contract"
    );
    h.world.complete_verify("r1", "a", &[]);
    ticks(&mut h);
    assert!(h
        .world
        .jobs_started()
        .iter()
        .any(|j| matches!(j, Job::Gates { .. })));
    assert_eq!(task_status(&mut h, "a"), "review");
    h.world.complete_gates(
        "r1",
        "a",
        1,
        vec![fleet_domain::quality::GateOutcome {
            name: "fmt".into(),
            passed: true,
            summary: "exit code 0".into(),
            output: String::new(),
        }],
    );
    ticks(&mut h);
    assert_eq!(task_status(&mut h, "a"), "completed");
}

#[test]
fn a_goal_added_while_the_gates_run_still_has_to_be_verified() {
    // the task has no contract when it enters review; a goal with one is added before the gates
    // answer. The passing gates must not complete it.
    let mut cfg = test_config();
    cfg.gates = vec![gate("fmt")];
    let mut h = Harness::with_config(cfg);
    h.world.set_gate_mode(GateMode::Manual);
    h.world.set_verify_mode(VerifyMode::Manual);
    h.call(
        "run.create",
        obj(vec![("objective", s("o")), ("repo", s("/repo"))]),
    )
    .unwrap();
    h.call(
        "task.add",
        obj(vec![("run", s("r1")), ("id", s("a")), ("title", s("a"))]),
    )
    .unwrap();
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
    finish_ok(&mut h, "a");
    ticks(&mut h);
    h.call(
        "goal.add",
        obj(vec![
            ("id", s("late")),
            ("title", s("late")),
            ("tasks", list(&["r1/a"])),
            ("contract", contract(&[("tests", true)])),
        ]),
    )
    .unwrap();
    h.world.complete_gates(
        "r1",
        "a",
        1,
        vec![fleet_domain::quality::GateOutcome {
            name: "fmt".into(),
            passed: true,
            summary: "exit code 0".into(),
            output: String::new(),
        }],
    );
    ticks(&mut h);
    assert_eq!(
        task_status(&mut h, "a"),
        "review",
        "gates alone no longer complete the task"
    );
    assert_eq!(count(&h, "task_completed"), 0);
    assert_eq!(h.world.verify_jobs().len(), 1, "the contract runs now");
    h.world.complete_verify("r1", "a", &[]);
    ticks(&mut h);
    h.world.complete_gates(
        "r1",
        "a",
        1,
        vec![fleet_domain::quality::GateOutcome {
            name: "fmt".into(),
            passed: true,
            summary: "exit code 0".into(),
            output: String::new(),
        }],
    );
    ticks(&mut h);
    assert_eq!(task_status(&mut h, "a"), "completed");
}

// ---- things that change under a running verification --------------------------------------------

#[test]
fn a_contract_replaced_during_the_run_drops_the_result_and_runs_again() {
    let mut h = governed(&[("tests", true)], |_| {});
    h.world.set_verify_mode(VerifyMode::Manual);
    finish_ok(&mut h, "a");
    ticks(&mut h);
    h.call(
        "goal.attach",
        obj(vec![
            ("goal", s("g")),
            ("contract", contract(&[("tests", true), ("docs", true)])),
        ]),
    )
    .unwrap();
    h.world.complete_verify("r1", "a", &[]);
    ticks(&mut h);
    assert_eq!(
        count(&h, "evidence_recorded"),
        0,
        "evidence of the old contract is refused"
    );
    assert_eq!(task_status(&mut h, "a"), "review");
    assert_eq!(h.world.verify_jobs().len(), 2, "the new contract is run");
    assert_eq!(
        h.world.verify_jobs()[1].targets[0].contract.checks().len(),
        2
    );
    h.world.complete_verify("r1", "a", &[]);
    ticks(&mut h);
    assert_eq!(task_status(&mut h, "a"), "completed");
}

#[test]
fn a_restart_during_verification_runs_it_again_and_keeps_the_ledger() {
    let mut h = governed(&[("tests", true)], |_| {});
    h.world.set_verify_mode(VerifyMode::Manual);
    finish_ok(&mut h, "a");
    ticks(&mut h);
    assert_eq!(h.world.verify_jobs().len(), 1);
    h.restart(test_config()).unwrap();
    assert_eq!(task_status(&mut h, "a"), "review");
    assert_eq!(goal_status(&mut h, "g"), "open");
    ticks(&mut h);
    assert_eq!(h.world.verify_jobs().len(), 2, "the lost run is repeated");
    h.world.complete_verify("r1", "a", &[]);
    ticks(&mut h);
    assert_eq!(task_status(&mut h, "a"), "completed");
    // and the evidence survives another restart
    h.restart(test_config()).unwrap();
    assert_eq!(goal_status(&mut h, "g"), "verified");
    assert_eq!(
        goal(&mut h, "g")
            .get("history")
            .and_then(Value::as_array)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn an_abandoned_goal_stops_governing_its_task() {
    let mut h = governed(&[("tests", true)], |_| {});
    h.world.set_verify_mode(VerifyMode::Manual);
    h.call(
        "goal.status",
        obj(vec![("goal", s("g")), ("status", s("abandoned"))]),
    )
    .unwrap();
    finish_ok(&mut h, "a");
    ticks(&mut h);
    assert_eq!(task_status(&mut h, "a"), "completed");
    assert!(h.world.verify_jobs().is_empty());
}

#[test]
fn the_job_cap_makes_the_review_wait_instead_of_failing() {
    let mut h = governed(&[("tests", true)], |_| {});
    h.world.set_verify_mode(VerifyMode::Manual);
    for _ in 0..MAX_CONCURRENT_JOBS {
        h.world.occupy_job_slot();
    }
    finish_ok(&mut h, "a");
    ticks(&mut h);
    assert!(h.world.verify_jobs().is_empty(), "no slot, no job");
    assert_eq!(task_status(&mut h, "a"), "review");
    h.world.free_job_slots();
    ticks(&mut h);
    assert_eq!(h.world.verify_jobs().len(), 1);
}

#[test]
fn a_worktree_that_cannot_be_read_fails_the_task_instead_of_completing_it() {
    let mut h = governed(&[("tests", true)], |_| {});
    h.world.fail_worktree_state(true);
    finish_ok(&mut h, "a");
    ticks(&mut h);
    assert_eq!(task_status(&mut h, "a"), "failed");
    assert!(text(&task_doc(&mut h, "a"), "failure").contains("could not start the verification"));
    assert_eq!(count(&h, "task_completed"), 0);
}

#[test]
fn evidence_that_cannot_be_recorded_does_not_complete_the_task() {
    let mut h = governed(&[("tests", true)], |_| {});
    h.world.set_verify_mode(VerifyMode::Manual);
    finish_ok(&mut h, "a");
    ticks(&mut h);
    h.world.fail_appends(true);
    h.world.complete_verify("r1", "a", &[]);
    h.orch.tick();
    h.world.fail_appends(false);
    assert_eq!(count(&h, "task_completed"), 0);
    assert_eq!(goal_status(&mut h, "g"), "open");
    // the review runs the contract again and completes on a recorded pass
    ticks(&mut h);
    h.world.complete_verify("r1", "a", &[]);
    ticks(&mut h);
    assert_eq!(task_status(&mut h, "a"), "completed");
}

// ---- fleet verify ---------------------------------------------------------------------------------

fn verify_call(
    h: &mut Harness,
    target: &str,
) -> fleet_domain::error::Result<fleet_app::api::Reply> {
    h.call("verify", obj(vec![("target", s(target))]))
}

fn status_doc(h: &mut Harness, target: &str) -> Value {
    h.call("verify.status", obj(vec![("target", s(target))]))
        .unwrap()
        .data()
        .clone()
}

/// A governed task whose worker finished and whose review verification already ran (and failed),
/// so that `fleet verify` has a worktree to work on.
fn reviewed_failing() -> Harness {
    let mut h = governed(&[("tests", true)], |c| c.max_retries = 0);
    h.world.set_verify_mode(VerifyMode::Fail("broken".into()));
    finish_ok(&mut h, "a");
    ticks(&mut h);
    assert_eq!(task_status(&mut h, "a"), "failed");
    h
}

#[test]
fn verify_runs_the_contract_on_demand_and_records_evidence_only() {
    let mut h = reviewed_failing();
    assert_eq!(count(&h, "evidence_recorded"), 1);
    h.world.set_verify_mode(VerifyMode::Manual);
    let started = verify_call(&mut h, "r1/a").unwrap();
    assert_eq!(text(started.data(), "schema"), "fleet.verify.started/1");
    assert_eq!(text(started.data(), "task"), "r1/a");
    assert_eq!(
        h.world.verify_jobs().last().unwrap().origin,
        VerifyOrigin::Manual
    );
    let running = status_doc(&mut h, "r1/a");
    assert_eq!(running.get("running"), Some(&Value::Bool(true)));
    assert_eq!(
        verify_call(&mut h, "r1/a").unwrap_err().code,
        ErrorCode::Conflict,
        "one run per task at a time"
    );
    h.world.complete_verify("r1", "a", &[]);
    ticks(&mut h);
    let done = status_doc(&mut h, "r1/a");
    assert_eq!(done.get("running"), Some(&Value::Bool(false)));
    assert_eq!(done.get("passed"), Some(&Value::Bool(true)));
    assert_eq!(text(&done, "verification"), "verified");
    let goals = done.get("goals").and_then(Value::as_array).unwrap();
    assert_eq!(text(&goals[0], "goal"), "g");
    assert_eq!(text(&goals[0], "contract"), "passed");
    assert_eq!(text(&goals[0], "status"), "verified");
    let checks = goals[0].get("checks").and_then(Value::as_array).unwrap();
    assert_eq!(text(checks[0].get("latest").unwrap(), "verdict"), "passed");
    // the task itself was not touched: it stays failed (the operator retries it)
    assert_eq!(task_status(&mut h, "a"), "failed");
    assert_eq!(count(&h, "task_completed"), 0);
}

#[test]
fn verify_by_goal_uses_its_single_task_or_asks_for_one() {
    let mut h = reviewed_failing();
    h.world.set_verify_mode(VerifyMode::Manual);
    let started = verify_call(&mut h, "g").unwrap();
    assert_eq!(text(started.data(), "task"), "r1/a");
    h.world
        .complete_verify("r1", "a", &[("tests", Verdict::Failed)]);
    ticks(&mut h);
    let done = status_doc(&mut h, "g");
    assert_eq!(done.get("passed"), Some(&Value::Bool(false)));
    // a goal with no task needs --task
    h.call(
        "goal.add",
        obj(vec![
            ("id", s("loose")),
            ("title", s("loose")),
            ("contract", contract(&[("c", true)])),
        ]),
    )
    .unwrap();
    assert_eq!(
        verify_call(&mut h, "loose").unwrap_err().code,
        ErrorCode::Invalid
    );
    let with_task = h
        .call(
            "verify",
            obj(vec![("target", s("loose")), ("task", s("r1/a"))]),
        )
        .unwrap();
    assert_eq!(text(with_task.data(), "task"), "r1/a");
    // a goal without a contract cannot be verified
    h.call(
        "goal.add",
        obj(vec![("id", s("bare")), ("title", s("bare"))]),
    )
    .unwrap();
    assert_eq!(
        verify_call(&mut h, "bare").unwrap_err().code,
        ErrorCode::Conflict
    );
}

#[test]
fn verify_refuses_what_cannot_be_verified() {
    let mut h = governed(&[("tests", true)], |_| {});
    h.world.set_verify_mode(VerifyMode::Manual);
    // a is running
    assert_eq!(
        verify_call(&mut h, "r1/a").unwrap_err().code,
        ErrorCode::Conflict
    );
    // b has no goal
    assert_eq!(
        verify_call(&mut h, "r1/b").unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(
        verify_call(&mut h, "r1/zzz").unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(
        verify_call(&mut h, "nothing").unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(
        h.call("verify.status", obj(vec![("target", s("nothing"))]))
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert!(h.world.verify_jobs().is_empty());
}

#[test]
fn verify_needs_a_worktree_and_a_readable_one() {
    let mut cfg = test_config();
    cfg.max_sessions = 1;
    let mut h = Harness::with_config(cfg);
    h.call(
        "run.create",
        obj(vec![("objective", s("o")), ("repo", s("/repo"))]),
    )
    .unwrap();
    for t in ["a", "b"] {
        h.call(
            "task.add",
            obj(vec![("run", s("r1")), ("id", s(t)), ("title", s(t))]),
        )
        .unwrap();
    }
    h.call(
        "goal.add",
        obj(vec![
            ("id", s("gb")),
            ("title", s("b")),
            ("tasks", list(&["r1/b"])),
            ("contract", contract(&[("t", true)])),
        ]),
    )
    .unwrap();
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
    // b waits for the only slot: no worktree yet
    assert_eq!(
        verify_call(&mut h, "r1/b").unwrap_err().code,
        ErrorCode::Conflict
    );
    // a finishes (not governed), b gets its worktree and is running: still refused
    finish_ok(&mut h, "a");
    ticks(&mut h);
    assert_eq!(task_status(&mut h, "b"), "running");
    assert_eq!(
        verify_call(&mut h, "r1/b").unwrap_err().code,
        ErrorCode::Conflict
    );
    // b finishes; its review fails because git cannot read the worktree
    h.world.fail_worktree_state(true);
    finish_ok(&mut h, "b");
    ticks(&mut h);
    assert_eq!(
        verify_call(&mut h, "r1/b").unwrap_err().code,
        ErrorCode::Internal
    );
}

#[test]
fn verify_status_names_the_running_state_of_a_task_nobody_verified() {
    let mut h = governed(&[("tests", true)], |_| {});
    let st = status_doc(&mut h, "r1/a");
    assert_eq!(st.get("running"), Some(&Value::Bool(false)));
    assert_eq!(st.get("passed"), Some(&Value::Bool(false)));
    assert_eq!(text(&st, "verification"), "pending");
    let goals = st.get("goals").and_then(Value::as_array).unwrap();
    assert_eq!(text(&goals[0], "contract"), "pending");
}
