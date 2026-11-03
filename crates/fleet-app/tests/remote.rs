//! Tasks that run on a remote host, driven through the command interface with in-memory fakes for
//! every port, the remote host included.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_app::hosts::HostDef;
use fleet_app::ports::Signal;
use fleet_app::remote::{Collected, EXIT_COLLECT_FAILED};
use fleet_app::testing::remote::{remote_harness, FakeRemote};
use fleet_app::testing::{test_config, Harness};
use fleet_domain::contract::{ContractRead, ResultStatus, WorkerResult};
use fleet_domain::env::EnvPolicy;
use fleet_domain::ids::{HostName, Sha};
use fleet_domain::value::Value;

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    Value::object(pairs)
}

fn s(x: &str) -> Value {
    Value::str(x)
}

fn host_def(name: &str) -> HostDef {
    let mut h = HostDef::new(
        HostName::parse(name).unwrap(),
        "build.example.org",
        "fleet",
        "/keys/id_ed25519",
        "/srv/fleet",
    );
    h.allowed_workers = vec!["run-agent".to_string()];
    h.env = vec![("API_BASE".to_string(), "https://api.invalid".to_string())];
    h
}

fn config() -> fleet_app::config::AppConfig {
    let mut cfg = test_config();
    // The daemon's secret is even allowlisted locally: it still must not reach a remote worker.
    cfg.env = EnvPolicy::new(vec!["PATH".into(), "SENTINEL_TOKEN".into()]);
    let h = host_def("build");
    cfg.hosts.insert(h.name.clone(), h);
    cfg
}

fn setup(cfg: fleet_app::config::AppConfig) -> (Harness, FakeRemote) {
    let remote = FakeRemote::new();
    let mut h = remote_harness(cfg, &remote);
    h.call(
        "run.create",
        obj(vec![("objective", s("go remote")), ("repo", s("/repo"))]),
    )
    .unwrap();
    (h, remote)
}

fn add_remote(h: &mut Harness, id: &str, host: Option<&str>) {
    let mut args = vec![
        ("run", s("r1")),
        ("id", s(id)),
        ("title", s(&format!("task {id}"))),
    ];
    if let Some(host) = host {
        args.push(("host", s(host)));
    }
    h.call("task.add", obj(args)).unwrap();
}

fn start(h: &mut Harness) {
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
}

fn task_status(h: &mut Harness, id: &str) -> String {
    let st = h.call("status", Value::Null).unwrap().data().clone();
    for run in st.get("runs").and_then(Value::as_array).unwrap() {
        for t in run.get("tasks").and_then(Value::as_array).unwrap() {
            if t.get("id").and_then(Value::as_str) == Some(id) {
                return t.get("status").and_then(Value::as_str).unwrap().to_string();
            }
        }
    }
    panic!("no task {id}");
}

fn sha(c: char) -> Sha {
    Sha::parse(&c.to_string().repeat(40)).unwrap()
}

fn done_result(h: &Harness, worktree: &str, summary: &str) {
    h.world.write_result(
        worktree,
        ContractRead::Valid(WorkerResult {
            status: ResultStatus::Done,
            summary: summary.to_string(),
        }),
    );
}

#[test]
fn a_remote_task_is_prepared_launched_and_sees_remote_paths_and_only_explicit_variables() {
    let (mut h, remote) = setup(config());
    add_remote(&mut h, "a", Some("build"));
    start(&mut h);

    let prepared = remote.prepared();
    assert_eq!(prepared.len(), 1);
    assert_eq!(prepared[0].task.host.as_str(), "build");
    assert!(
        prepared[0].worktree.contains("a"),
        "the local worktree is what is sent"
    );
    let launched = remote.launched();
    assert_eq!(launched.len(), 1);
    let l = &launched[0];
    assert_eq!(l.program, "run-agent");
    assert_eq!(
        l.args[3], "/remote/build/r1/a",
        "{{worktree}} is the remote directory"
    );
    let env: std::collections::BTreeMap<_, _> = l.env.iter().cloned().collect();
    assert_eq!(
        env.get("API_BASE").map(String::as_str),
        Some("https://api.invalid")
    );
    assert_eq!(
        env.get("FLEET_WORKTREE").map(String::as_str),
        Some("/remote/build/r1/a")
    );
    assert_eq!(env.get("FLEET_TASK").map(String::as_str), Some("a"));
    assert!(
        !env.contains_key("SENTINEL_TOKEN")
            && !env.contains_key("HOME")
            && !env.contains_key("AGENT_MODE"),
        "nothing of the local environment or of the local worker settings is sent: {env:?}"
    );
    // The local session runs the ssh command the adapter built, in the local worktree.
    let id = h.world.session_for("a").unwrap();
    let spec = h.world.session_spec(id).unwrap();
    assert_eq!(spec.program, "/usr/bin/ssh");
    assert_eq!(spec.host, None);
    assert_eq!(spec.cwd, prepared[0].worktree);
    assert_eq!(task_status(&mut h, "a"), "running");
}

#[test]
fn the_session_ends_only_after_the_work_was_collected_and_then_the_task_completes() {
    let (mut h, remote) = setup(config());
    remote.polls_before_done(2);
    remote.collect_answer(Ok(Collected {
        head: Some(sha('b')),
        notes: vec!["fleet: remote: 1 commit brought back".to_string()],
    }));
    add_remote(&mut h, "a", Some("build"));
    start(&mut h);
    let id = h.world.session_for("a").unwrap();
    let wt = h.world.session_spec(id).unwrap().cwd;
    done_result(&h, &wt, "a done");
    h.world.mark_dirty(&wt);
    h.world.exit(id, 0);

    h.orch.tick();
    assert_eq!(
        remote.collects().len(),
        1,
        "collecting starts when the local command ended"
    );
    assert_eq!(task_status(&mut h, "a"), "running", "still collecting");
    h.orch.tick();
    assert_eq!(task_status(&mut h, "a"), "running");
    h.orch.tick();
    assert_eq!(task_status(&mut h, "a"), "running");
    h.orch.tick();
    assert_eq!(task_status(&mut h, "a"), "completed");
    assert_eq!(remote.collects()[0].sent, sha_of_first_prepare(&remote));
    let log = h.world.saved_log("r1", "a").unwrap();
    assert!(
        log.contains("fleet: remote: 1 commit brought back"),
        "{log}"
    );
}

fn sha_of_first_prepare(_remote: &FakeRemote) -> Sha {
    // The fake numbers the commits it was sent: the first is 0...01.
    Sha::parse(&format!("{:040x}", 1)).unwrap()
}

#[test]
fn a_clean_exit_whose_work_could_not_be_brought_back_is_a_failed_attempt() {
    let (mut h, remote) = setup(config());
    remote.collect_answer(Err("the host refused the bundle".to_string()));
    add_remote(&mut h, "a", Some("build"));
    start(&mut h);
    let id = h.world.session_for("a").unwrap();
    let wt = h.world.session_spec(id).unwrap().cwd;
    done_result(&h, &wt, "claims to be done");
    h.world.exit(id, 0);
    h.orch.tick();
    h.orch.tick();
    // The attempt did not complete: it is queued for another one (retry budget) or failed.
    let status = task_status(&mut h, "a");
    assert_ne!(status, "completed");
    let log = h.world.saved_log("r1", "a").unwrap();
    assert!(log.contains("could not be brought back"), "{log}");
    let ended = h
        .world
        .stored_events()
        .into_iter()
        .filter_map(|e| match e {
            fleet_domain::events::DomainEvent::SessionEnded { exit_code, .. } => Some(exit_code),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ended, vec![Some(EXIT_COLLECT_FAILED)]);
}

#[test]
fn a_stop_signals_the_remote_worker_and_the_local_client() {
    let (mut h, remote) = setup(config());
    add_remote(&mut h, "a", Some("build"));
    start(&mut h);
    let id = h.world.session_for("a").unwrap();
    h.call("task.stop", obj(vec![("task", s("r1/a"))])).unwrap();
    h.orch.tick();
    let sent = remote.signals();
    assert!(
        sent.iter()
            .any(|(t, sig)| t.task.as_str() == "a" && *sig == Signal::Term),
        "{sent:?}"
    );
    assert!(h.world.signals(id).contains(&Signal::Term));
}

#[test]
fn preparing_or_launching_that_fails_is_a_failed_attempt_and_no_process_is_left() {
    for (label, fail_prepare) in [("prepare", true), ("launch", false)] {
        let (mut h, remote) = setup(config());
        if fail_prepare {
            remote.fail_prepare(Some("host unreachable"));
        } else {
            remote.fail_launch(Some("cannot build the ssh command"));
        }
        add_remote(&mut h, "a", Some("build"));
        start(&mut h);
        assert!(h.world.session_for("a").is_none(), "{label}: no session");
        assert_ne!(task_status(&mut h, "a"), "running", "{label}");
        let events = h.world.event_kinds();
        assert!(events.contains(&"session_ended"), "{label}: {events:?}");
        if !fail_prepare {
            assert_eq!(
                remote.abandoned().len(),
                1,
                "the half-made remote worktree is removed"
            );
        }
    }
}

#[test]
fn a_worker_command_the_host_does_not_allow_fails_the_task_without_touching_the_host() {
    let mut cfg = config();
    let name = HostName::parse("build").unwrap();
    cfg.hosts.get_mut(&name).unwrap().allowed_workers = vec!["something-else".to_string()];
    let (mut h, remote) = setup(cfg);
    add_remote(&mut h, "a", Some("build"));
    start(&mut h);
    assert_eq!(task_status(&mut h, "a"), "failed");
    assert!(remote.prepared().is_empty());
}

#[test]
fn a_host_with_no_free_slot_leaves_its_tasks_ready_while_local_tasks_start() {
    let mut cfg = config();
    let name = HostName::parse("build").unwrap();
    cfg.hosts.get_mut(&name).unwrap().max_sessions = 1;
    let (mut h, remote) = setup(cfg);
    add_remote(&mut h, "a", Some("build"));
    add_remote(&mut h, "b", Some("build"));
    add_remote(&mut h, "c", None);
    start(&mut h);
    assert_eq!(task_status(&mut h, "a"), "running");
    assert_eq!(
        task_status(&mut h, "b"),
        "ready",
        "waits for the host's slot"
    );
    assert_eq!(
        task_status(&mut h, "c"),
        "running",
        "a local task is not held up"
    );
    assert_eq!(remote.prepared().len(), 1);
    // The first one ends; the waiting one starts.
    let id = h.world.session_for("a").unwrap();
    let wt = h.world.session_spec(id).unwrap().cwd;
    done_result(&h, &wt, "a done");
    h.world.mark_dirty(&wt);
    h.world.exit(id, 0);
    h.orch.tick();
    h.orch.tick();
    assert_eq!(task_status(&mut h, "b"), "running");
}

#[test]
fn a_local_task_never_touches_the_remote_host() {
    let (mut h, remote) = setup(config());
    add_remote(&mut h, "a", None);
    start(&mut h);
    assert!(remote.prepared().is_empty() && remote.launched().is_empty());
    let id = h.world.session_for("a").unwrap();
    let spec = h.world.session_spec(id).unwrap();
    assert_eq!(spec.program, "run-agent");
    assert!(
        spec.env.iter().any(|(k, _)| k == "SENTINEL_TOKEN"),
        "locally the allowlist applies as before"
    );
}

#[test]
fn the_startup_reports_what_the_remote_reconciliation_found() {
    let remote = FakeRemote::new();
    remote.set_reconcile(&["host build: removed 2 leftover worktrees"]);
    let h = remote_harness(config_with_remote(&remote), &remote);
    let kinds = h.world.event_kinds();
    assert!(kinds.contains(&"warning"), "{kinds:?}");
}

fn config_with_remote(_remote: &FakeRemote) -> fleet_app::config::AppConfig {
    config()
}

// ---- compiled context and messages ----------------------------------------------------------

#[test]
fn a_remote_task_of_a_compiled_run_gets_the_pack_and_the_pointer_brief_before_the_host_is_prepared()
{
    let remote = FakeRemote::new();
    let mut h = remote_harness(config(), &remote);
    h.call(
        "run.create",
        obj(vec![
            ("objective", s("go remote")),
            ("repo", s("/repo")),
            ("context", s("compiled")),
        ]),
    )
    .unwrap();
    add_remote(&mut h, "a", Some("build"));
    start(&mut h);

    // What the adapter sends to the host is read from the local worktree, so the pack and the
    // brief must be in it when the host is prepared (and they are written by the same code as for
    // a local task).
    let prepared = remote.prepared();
    assert_eq!(prepared.len(), 1);
    let wt = prepared[0].worktree.clone();
    let (_, brief) = h
        .world
        .briefs()
        .into_iter()
        .find(|(w, _)| *w == wt)
        .expect("the brief was written to the worktree that is sent");
    assert!(brief.contains(".fleet/context.md"), "{brief}");
    let (text, manifest) = h
        .world
        .context_files(&wt)
        .expect("the pack was written to the worktree that is sent");
    assert!(!text.is_empty() && manifest.contains("layers"));
    assert_eq!(h.world.saved_packs("r1", "a").len(), 1);
    // and the worker's own paths are the host's
    assert_eq!(remote.launched()[0].args[3], "/remote/build/r1/a");
}
