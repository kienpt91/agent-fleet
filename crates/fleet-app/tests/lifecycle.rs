//! Application tests: a run from creation to completion, driven through the command interface
//! with in-memory fakes for every port.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_app::api::Reply;
use fleet_app::ports::Job;
use fleet_app::testing::{test_config, GateMode, Harness};
use fleet_domain::contract::{ContractRead, ResultStatus, WorkerResult};
use fleet_domain::error::ErrorCode;
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

fn status(h: &mut Harness) -> Value {
    h.call("status", Value::Null).unwrap().data().clone()
}

fn task_status(h: &mut Harness, id: &str) -> String {
    let st = status(h);
    let runs = st.get("runs").and_then(Value::as_array).unwrap().to_vec();
    for run in runs {
        for t in run.get("tasks").and_then(Value::as_array).unwrap() {
            if t.get("id").and_then(Value::as_str) == Some(id) {
                return t.get("status").and_then(Value::as_str).unwrap().to_string();
            }
        }
    }
    panic!("task {id} not found");
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

#[test]
fn a_task_naming_an_undefined_host_is_rejected_and_nothing_is_recorded() {
    let mut h = Harness::new();
    new_run(&mut h);
    let err = h
        .call(
            "task.add",
            obj(vec![
                ("run", s("r1")),
                ("id", s("a")),
                ("title", s("task a")),
                ("host", s("build1")),
            ]),
        )
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid, "{err:?}");
    assert!(
        err.message.contains("is not defined in the configuration"),
        "{err:?}"
    );
    assert!(!h.world.event_kinds().contains(&"task_added"));
}

#[test]
fn a_single_task_runs_to_completion() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    assert_eq!(task_status(&mut h, "a"), "ready");

    // nothing runs before the run is started
    h.orch.tick();
    assert!(h.world.session_ids().is_empty());

    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
    assert_eq!(task_status(&mut h, "a"), "running");
    finish_ok(&mut h, "a");
    assert_eq!(task_status(&mut h, "a"), "completed");

    let kinds = h.world.event_kinds();
    for expected in [
        "run_created",
        "task_added",
        "task_ready",
        "run_started",
        "task_dispatched",
        "session_ended",
        "task_reviewing",
        "task_committed",
        "task_completed",
    ] {
        assert!(kinds.contains(&expected), "missing {expected} in {kinds:?}");
    }
}

#[test]
fn dispatch_builds_the_command_from_the_worker_definition_with_an_allowlisted_environment() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
    let id = h.world.session_for("a").unwrap();
    let spec = h.world.session_spec(id).unwrap();
    assert_eq!(spec.program, "run-agent");
    assert_eq!(spec.args[0], "--task");
    assert_eq!(spec.args[1], "a");
    assert_eq!(spec.args[3], spec.cwd);
    assert!(
        spec.args[4].contains("# Task a: task a"),
        "brief inlined as the last argument"
    );
    let env: std::collections::BTreeMap<_, _> = spec.env.iter().cloned().collect();
    assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin:/bin"));
    assert_eq!(env.get("AGENT_MODE").map(String::as_str), Some("test"));
    assert_eq!(env.get("FLEET_TASK").map(String::as_str), Some("a"));
    assert_eq!(env.get("FLEET_RUN").map(String::as_str), Some("r1"));
    assert!(
        !env.contains_key("SENTINEL_TOKEN"),
        "the daemon's secrets are not inherited"
    );
    for (k, v) in &spec.env {
        if k != "XDG_STATE_HOME" {
            assert!(!k.contains("SOCKET") && !k.contains("STATE"), "{k}");
        }
        assert!(!v.contains("fleet.sock"), "{v}");
    }
}

#[test]
fn workers_get_a_private_state_directory_so_a_fleet_run_by_an_agent_cannot_find_the_daemon() {
    // The operator's own state location is allowlisted for the worker on purpose: fleet still
    // overrides it.
    let mut cfg = test_config();
    cfg.parent_env.push((
        "XDG_STATE_HOME".to_string(),
        "/home/test/.local/state".to_string(),
    ));
    cfg.parent_env.push((
        "FLEET_STATE_DIR".to_string(),
        "/home/test/.local/state/fleet".to_string(),
    ));
    for def in cfg.workers.values_mut() {
        def.env_pass = vec!["XDG_STATE_HOME".to_string(), "FLEET_STATE_DIR".to_string()];
        def.env_set = vec![("XDG_STATE_HOME".to_string(), "/elsewhere".to_string())];
    }
    let mut h = Harness::with_config(cfg);
    new_run(&mut h);
    add(&mut h, "a", &[]);
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
    let spec = h
        .world
        .session_spec(h.world.session_for("a").unwrap())
        .unwrap();
    let env: std::collections::BTreeMap<_, _> = spec.env.iter().cloned().collect();
    assert_eq!(
        env.get("XDG_STATE_HOME"),
        Some(&format!("{}/.fleet/xdg-state", spec.cwd))
    );
    assert!(!env.contains_key("FLEET_STATE_DIR"));
    for (k, v) in &spec.env {
        assert!(!v.contains("/home/test/.local/state"), "{k}={v}");
        assert!(!v.contains("fleet.sock"), "{k}={v}");
        // No capability token (64 hex characters) is ever handed to a worker.
        let looks_like_token = v.len() == 64 && v.chars().all(|c| c.is_ascii_hexdigit());
        assert!(!looks_like_token, "{k}");
    }
    for arg in &spec.args {
        assert!(!arg.contains("fleet.sock"));
    }
}

#[test]
fn dependencies_gate_scheduling_and_their_branches_are_merged_into_dependents() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    add(&mut h, "b", &["a"]);
    assert_eq!(task_status(&mut h, "b"), "pending");
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
    assert_eq!(h.world.live_session_ids().len(), 1, "only a can run");

    finish_ok(&mut h, "a");
    assert_eq!(task_status(&mut h, "a"), "completed");
    assert_eq!(task_status(&mut h, "b"), "running");
    let merges = h.world.worktree_merges();
    assert_eq!(merges.len(), 1);
    assert_eq!(merges[0].1, vec!["fleet/r1/a".to_string()]);
    let briefs = h.world.briefs();
    let brief = &briefs.last().unwrap().1;
    assert!(
        brief.contains("a done"),
        "the dependency's summary is in the brief: {brief}"
    );
}

#[test]
fn parallelism_is_limited_per_run_and_globally() {
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
    for id in ["a", "b", "c"] {
        add(&mut h, id, &[]);
    }
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
    assert_eq!(h.world.live_session_ids().len(), 2);
    assert_eq!(task_status(&mut h, "c"), "ready");
    finish_ok(&mut h, "a");
    assert_eq!(task_status(&mut h, "c"), "running");
}

#[test]
fn the_global_session_cap_is_shared_between_runs() {
    let mut cfg = test_config();
    cfg.max_sessions = 2;
    let mut h = Harness::with_config(cfg);
    for _ in 0..2 {
        h.call(
            "run.create",
            obj(vec![("objective", s("o")), ("repo", s("/repo"))]),
        )
        .unwrap();
    }
    for (run, task) in [("r1", "a"), ("r1", "b"), ("r2", "c"), ("r2", "d")] {
        h.call(
            "task.add",
            obj(vec![("run", s(run)), ("id", s(task)), ("title", s(task))]),
        )
        .unwrap();
    }
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
    h.call("run.start", obj(vec![("run", s("r2"))])).unwrap();
    assert_eq!(h.world.live_session_ids().len(), 2);
}

#[test]
fn a_plan_is_loaded_atomically_and_cycles_are_rejected_without_events() {
    let mut h = Harness::new();
    let before = h.world.stored_events().len();
    let tasks = Value::array([
        obj(vec![
            ("id", s("a")),
            ("title", s("A")),
            ("after", list(&["b"])),
        ]),
        obj(vec![
            ("id", s("b")),
            ("title", s("B")),
            ("after", list(&["a"])),
        ]),
    ]);
    let err = h
        .call(
            "plan.load",
            obj(vec![
                (
                    "new_run",
                    obj(vec![("objective", s("o")), ("repo", s("/repo"))]),
                ),
                ("tasks", tasks),
            ]),
        )
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid);
    assert!(err.message.contains("cycle"));
    assert_eq!(
        h.world.stored_events().len(),
        before,
        "an invalid plan records nothing, not even the run"
    );
}

#[test]
fn a_plan_creates_the_run_and_its_tasks() {
    let mut h = Harness::new();
    let tasks = Value::array([
        obj(vec![("id", s("models")), ("title", s("Models"))]),
        obj(vec![
            ("id", s("api")),
            ("title", s("API")),
            ("after", list(&["models"])),
        ]),
        obj(vec![
            ("id", s("docs")),
            ("title", s("Docs")),
            ("after", list(&["api"])),
        ]),
    ]);
    let r = h
        .call(
            "plan.load",
            obj(vec![
                (
                    "new_run",
                    obj(vec![("objective", s("o")), ("repo", s("/repo"))]),
                ),
                ("tasks", tasks),
            ]),
        )
        .unwrap();
    assert_eq!(
        r.data().get("schema").and_then(Value::as_str),
        Some("fleet.plan/1")
    );
    assert_eq!(task_status(&mut h, "models"), "ready");
    assert_eq!(task_status(&mut h, "api"), "pending");
}

#[test]
fn a_plan_into_an_existing_run_validates_against_it() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    let dup = Value::array([obj(vec![("id", s("a")), ("title", s("again"))])]);
    assert!(h
        .call("plan.load", obj(vec![("run", s("r1")), ("tasks", dup)]))
        .is_err());
    let both = h.call(
        "plan.load",
        obj(vec![
            ("run", s("r1")),
            (
                "new_run",
                obj(vec![("objective", s("o")), ("repo", s("/repo"))]),
            ),
            ("tasks", Value::array([obj(vec![("title", s("x"))])])),
        ]),
    );
    assert_eq!(both.unwrap_err().code, ErrorCode::Invalid);
    let neither = h.call(
        "plan.load",
        obj(vec![(
            "tasks",
            Value::array([obj(vec![("title", s("x"))])]),
        )]),
    );
    assert_eq!(neither.unwrap_err().code, ErrorCode::Invalid);
}

#[test]
fn creating_a_run_for_a_missing_repository_fails_cleanly() {
    let mut h = Harness::new();
    h.world.missing_repo("/nope");
    let err = h
        .call(
            "run.create",
            obj(vec![("objective", s("o")), ("repo", s("/nope"))]),
        )
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    assert!(!h.world.event_kinds().contains(&"run_created"));
    let err = h
        .call(
            "run.create",
            obj(vec![
                ("objective", s("o")),
                ("repo", s("/repo")),
                ("base", s("--evil")),
            ]),
        )
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid);
}

#[test]
fn a_task_without_a_configured_worker_fails_instead_of_looping() {
    let mut cfg = test_config();
    cfg.default_worker = None;
    let mut h = Harness::with_config(cfg);
    new_run(&mut h);
    add(&mut h, "a", &[]);
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
    assert_eq!(task_status(&mut h, "a"), "failed");
    h.orch.tick();
    assert!(h.world.session_ids().is_empty());
}

#[test]
fn gates_run_after_the_worker_and_must_pass_before_completion() {
    let mut cfg = test_config();
    cfg.gates.push(fleet_app::config::GateDef {
        name: "tests".into(),
        command: "run-tests".into(),
        args: vec!["--dir".into(), "{worktree}".into()],
        env_pass: vec![],
        env_set: vec![],
        format: fleet_domain::quality::GateFormat::Exit,
        timeout_ms: 5000,
    });
    let mut h = Harness::with_config(cfg);
    h.world.set_gate_mode(GateMode::Manual);
    new_run(&mut h);
    add(&mut h, "a", &[]);
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
    finish_ok(&mut h, "a");
    assert_eq!(
        task_status(&mut h, "a"),
        "review",
        "waiting for the gate job"
    );
    let jobs = h.world.jobs_started();
    let Job::Gates {
        gates,
        worktree,
        attempt,
        ..
    } = &jobs[0]
    else {
        panic!("expected a gate job")
    };
    assert_eq!(gates[0].args[1], *worktree);
    assert_eq!(*attempt, 1);
    // the gate job is not started twice
    h.orch.tick();
    h.orch.tick();
    assert_eq!(h.world.jobs_started().len(), 1);

    h.world.complete_gates(
        "r1",
        "a",
        1,
        vec![fleet_domain::quality::GateOutcome {
            name: "tests".into(),
            passed: true,
            summary: "exit code 0".into(),
            output: String::new(),
        }],
    );
    h.orch.tick();
    assert_eq!(task_status(&mut h, "a"), "completed");
}

#[test]
fn a_failing_gate_retries_the_worker_with_the_failure_in_its_brief() {
    let mut cfg = test_config();
    cfg.gates.push(fleet_app::config::GateDef {
        name: "tests".into(),
        command: "run-tests".into(),
        args: vec![],
        env_pass: vec![],
        env_set: vec![],
        format: fleet_domain::quality::GateFormat::Exit,
        timeout_ms: 5000,
    });
    let mut h = Harness::with_config(cfg);
    h.world
        .set_gate_mode(GateMode::Fail("FAILED test_parse: expected 3 got 4".into()));
    new_run(&mut h);
    add(&mut h, "a", &[]);
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
    finish_ok(&mut h, "a");
    // the gate failed immediately, the task is ready again and was re-dispatched in the same tick
    assert_eq!(task_status(&mut h, "a"), "running");
    let briefs = h.world.briefs();
    let brief = &briefs.last().unwrap().1;
    assert!(brief.contains("Quality gate 'tests' failed"), "{brief}");
    assert!(
        brief.contains("FAILED test_parse: expected 3 got 4"),
        "{brief}"
    );
    assert!(brief.contains("attempt 2"));
}

#[test]
fn gate_retries_are_bounded() {
    let mut cfg = test_config();
    cfg.max_retries = 1;
    cfg.gates.push(fleet_app::config::GateDef {
        name: "tests".into(),
        command: "run-tests".into(),
        args: vec![],
        env_pass: vec![],
        env_set: vec![],
        format: fleet_domain::quality::GateFormat::Exit,
        timeout_ms: 5000,
    });
    let mut h = Harness::with_config(cfg);
    h.world.set_gate_mode(GateMode::Fail("boom".into()));
    new_run(&mut h);
    add(&mut h, "a", &[]);
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
    finish_ok(&mut h, "a");
    finish_ok(&mut h, "a");
    assert_eq!(task_status(&mut h, "a"), "failed");
    h.orch.tick();
    assert!(h.world.live_session_ids().is_empty());
    // an operator can reset it
    h.call("task.retry", obj(vec![("task", s("a"))])).unwrap();
    assert_eq!(task_status(&mut h, "a"), "running");
}

#[test]
fn status_reports_runs_counts_and_live_flags() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    add(&mut h, "b", &["a"]);
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
    let st = status(&mut h);
    assert_eq!(
        st.get("schema").and_then(Value::as_str),
        Some("fleet.status/1")
    );
    let run = &st.get("runs").and_then(Value::as_array).unwrap()[0];
    assert_eq!(run.get("status").and_then(Value::as_str), Some("active"));
    let counts = run.get("counts").unwrap();
    assert_eq!(counts.get("running").and_then(Value::as_u64), Some(1));
    assert_eq!(counts.get("pending").and_then(Value::as_u64), Some(1));
    let tasks = run.get("tasks").and_then(Value::as_array).unwrap();
    assert_eq!(tasks[0].get("live").and_then(Value::as_bool), Some(true));
    assert_eq!(tasks[1].get("live").and_then(Value::as_bool), Some(false));
    let daemon = st.get("daemon").unwrap();
    assert_eq!(daemon.get("sessions").and_then(Value::as_u64), Some(1));
    assert_eq!(daemon.get("pid").and_then(Value::as_u64), Some(4242));
    assert!(!st
        .get("recent")
        .and_then(Value::as_array)
        .unwrap()
        .is_empty());
}

#[test]
fn tail_send_and_attach_resolve_the_live_session() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
    let id = h.world.session_for("a").unwrap();
    h.world.print(id, "hello\nworld\n");

    let tail = h.call("task.tail", obj(vec![("task", s("a"))])).unwrap();
    assert_eq!(tail.data().get("live").and_then(Value::as_bool), Some(true));
    assert_eq!(
        tail.data().get("text").and_then(Value::as_str),
        Some("hello\nworld\n")
    );
    let next = tail.data().get("next").and_then(Value::as_u64).unwrap();
    h.world.print(id, "more\n");
    let again = h
        .call(
            "task.tail",
            obj(vec![("task", s("r1/a")), ("since", Value::int(next))]),
        )
        .unwrap();
    assert_eq!(
        again.data().get("text").and_then(Value::as_str),
        Some("more\n")
    );

    let Reply::Input { session, bytes, .. } = h
        .call("task.send", obj(vec![("task", s("a")), ("text", s("yes"))]))
        .unwrap()
    else {
        panic!("expected input")
    };
    assert_eq!(session, id);
    assert_eq!(bytes, b"yes\r");
    let Reply::Input { bytes, .. } = h
        .call(
            "task.send",
            obj(vec![
                ("task", s("a")),
                ("text", s("yes")),
                ("newline", Value::Bool(false)),
            ]),
        )
        .unwrap()
    else {
        panic!("expected input")
    };
    assert_eq!(bytes, b"yes");
    assert!(matches!(
        h.call("task.attach", obj(vec![("task", s("a"))])).unwrap(),
        Reply::Attach { .. }
    ));

    h.world.exit(id, 0);
    h.orch.tick();
    assert_eq!(
        h.call("task.send", obj(vec![("task", s("a")), ("text", s("x"))]))
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    assert_eq!(
        h.call("task.attach", obj(vec![("task", s("a"))]))
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    let after = h.call("task.tail", obj(vec![("task", s("a"))])).unwrap();
    assert_eq!(
        after.data().get("live").and_then(Value::as_bool),
        Some(false)
    );
    assert!(
        after
            .data()
            .get("text")
            .and_then(Value::as_str)
            .unwrap()
            .contains("hello"),
        "finished output comes from the stored log"
    );
}
