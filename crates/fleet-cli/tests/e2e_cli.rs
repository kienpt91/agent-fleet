//! The command line as a user meets it: help, usage errors, `--json` documents and their schemas,
//! exit codes, doctor, attach, the dashboard without a terminal, and the optional undo tool.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod common;

use common::*;
use fleet_domain::error::ErrorCode;
use fleet_domain::value::Value;
use fleet_infra::testutil::{wait_until, TempDir};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::{Duration, Instant};

const ALL_SUBCOMMANDS: [&str; 37] = [
    "daemon",
    "run create",
    "task add",
    "plan",
    "start",
    "status",
    "attach",
    "send",
    "tail",
    "stop",
    "retry",
    "cancel",
    "gate list",
    "gate resolve",
    "approval list",
    "approve",
    "deny",
    "merge",
    "cleanup",
    "undo",
    "events",
    "shutdown",
    "tui",
    "doctor",
    "verify-log",
    "goals add",
    "goals attach",
    "goals status",
    "goals list",
    "goals show",
    "goals remaining",
    "verify",
    "msg list",
    "msg show",
    "msg mute",
    "msg unmute",
    "msg send",
];

/// `PATH` without any directory that holds `tool`.
fn path_without(tool: &str) -> String {
    system_path()
        .split(':')
        .filter(|d| !d.is_empty() && !std::path::Path::new(d).join(tool).exists())
        .collect::<Vec<_>>()
        .join(":")
}

fn quick_fleet() -> Fleet {
    let mut f = Fleet::new();
    let s = f.script("w.sh", "echo done-by-worker\n");
    f.write_cfg(&simple_cfg(&s, ""));
    f.start();
    f
}

fn assert_schema(v: &Value, schema: &str) {
    assert_eq!(jstr(v, "schema"), schema, "{}", v.canonical());
}

// ---- help, version, usage -------------------------------------------------------------------------

#[test]
fn help_exits_zero_and_mentions_every_subcommand() {
    let f = Fleet::new();
    let long = f.ok(&["--help"]);
    let short = f.ok(&["-h"]);
    assert_eq!(long.stdout, short.stdout);
    for sub in ALL_SUBCOMMANDS {
        assert!(long.stdout.contains(sub), "`{sub}` is not in the help text");
    }
    assert_eq!(fleet_cli::commands::SUBCOMMANDS, ALL_SUBCOMMANDS);
    assert!(long.stdout.contains("exit codes:"), "{}", long.stdout);
    assert_eq!(long.stderr, "");
    // help needs no state directory and no daemon
    let mut c = f.command();
    c.arg("--help");
    assert_eq!(run_cmd(c, Duration::from_secs(30)).code, 0);
}

#[test]
fn version_names_the_binary() {
    let f = Fleet::new();
    let out = f.ok(&["--version"]);
    assert_eq!(
        out.stdout.trim(),
        format!("fleet {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn unknown_commands_and_options_are_one_line_usage_errors() {
    let f = Fleet::new();
    for (args, needle) in [
        (vec!["frobnicate"], "unknown command"),
        (vec!["status", "--bogus"], "unknown option"),
        (vec!["run"], "needs a subcommand"),
        (vec!["run", "frobnicate"], "unknown run subcommand"),
        (vec!["gate"], "needs a subcommand"),
        (vec!["tail"], "missing"),
        (vec!["merge", "a", "b"], "unexpected argument"),
        (vec!["start"], "--run is required"),
        (vec![], "missing command"),
    ] {
        let out = f.cli(&args);
        assert_eq!(out.code, 2, "{args:?}: {out:?}");
        assert_eq!(out.stdout, "", "{args:?}");
        assert!(
            out.stderr_first_line().starts_with("fleet: "),
            "{args:?}: {out:?}"
        );
        assert!(
            out.stderr_first_line().contains(needle),
            "{args:?}: {out:?}"
        );
        assert!(!out.stderr.contains("panicked"), "{args:?}: {out:?}");
        assert!(out.stderr.lines().count() <= 2, "{args:?}: {out:?}");
    }
}

#[test]
fn plan_files_are_checked_before_the_daemon_is_asked() {
    let f = Fleet::new();
    let missing = f.cli(&["plan", "nope.toml"]);
    assert_eq!(missing.code, 4, "{missing:?}");
    let wrong_ext = f.tmp.write("plan.txt", "x");
    assert_eq!(f.cli(&["plan", wrong_ext.to_str().unwrap()]).code, 7);
    let typo = f
        .tmp
        .write("typo.toml", "[[tasks]]\nid = \"a\"\ntitel = \"x\"\n");
    let out = f.cli(&["plan", typo.to_str().unwrap()]);
    assert_eq!(out.code, 7, "{out:?}");
    assert!(out.stderr.contains("titel"), "{out:?}");
    // an acceptable plan still needs a daemon
    let ok = f
        .tmp
        .write("ok.toml", "[[tasks]]\nid = \"a\"\ntitle = \"x\"\n");
    assert_eq!(f.cli(&["plan", ok.to_str().unwrap()]).code, 3);
}

// ---- json documents and exit codes ----------------------------------------------------------------

#[test]
fn every_read_command_prints_a_json_document_with_its_schema() {
    let f = quick_fleet();
    let repo = f.repo.to_str().unwrap();
    // build a run through the individual commands
    let created = f.json(&[
        "run",
        "create",
        "--objective",
        "o",
        "--repo",
        repo,
        "--id",
        "r1",
    ]);
    assert_schema(&created, "fleet.run.create/1");
    assert_eq!(jstr(&created, "run"), "r1");
    let a = f.json(&["task", "add", "--run", "r1", "--title", "A", "--id", "a"]);
    assert_schema(&a, "fleet.task.add/1");
    let b = f.json(&[
        "task", "add", "--run", "r1", "--title", "B", "--id", "b", "--after", "a",
    ]);
    assert_schema(&b, "fleet.task.add/1");
    let started = f.json(&["start", "--run", "r1"]);
    assert_schema(&started, "fleet.run.start/1");
    f.wait_task("r1", "b", &["completed"]);

    assert_schema(&f.json(&["status"]), "fleet.status/1");
    assert_schema(&f.json(&["status", "--run", "r1"]), "fleet.status/1");
    assert_schema(&f.json(&["gate", "list"]), "fleet.gate.list/1");
    assert_schema(&f.json(&["approval", "list"]), "fleet.approval.list/1");
    let events = f.json(&["events", "--limit", "3"]);
    assert_schema(&events, "fleet.events/1");
    assert_eq!(jarr(&events, "events").len(), 3);
    for e in jarr(&events, "events") {
        assert_eq!(keys(e), ["line", "seq", "ts_ms"]);
    }
    let text_events = f.ok(&["events", "--limit", "3"]).stdout;
    assert_eq!(text_events.lines().count(), 3, "{text_events}");
    assert!(
        text_events.lines().all(|l| l.starts_with('#')),
        "{text_events}"
    );
    let tail = f.json(&["tail", "b"]);
    assert_schema(&tail, "fleet.tail/1");
    assert!(
        jstr(&tail, "text").contains("done-by-worker"),
        "{}",
        tail.canonical()
    );
    assert_schema(&f.json(&["verify-log"]), "fleet.verify-log/1");
    let doctor = f.json(&["doctor"]);
    assert_schema(&doctor, "fleet.doctor/1");
    assert!(jbool(&doctor, "ok"), "{}", doctor.canonical());
    assert_schema(&f.json(&["cleanup"]), "fleet.cleanup/1");
    assert_schema(&f.json(&["shutdown"]), "fleet.shutdown/1");
}

#[test]
fn error_json_has_a_stable_shape_and_the_exit_code_matches_the_error_code() {
    let f = quick_fleet();
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_task("r1", "t", &["completed"]);
    let repo = f.repo.to_str().unwrap();

    let cases: Vec<(Vec<&str>, &str, i32)> = vec![
        (vec!["stop", "nosuch"], "not_found", 4),
        (vec!["retry", "r1/t"], "conflict", 6),
        (vec!["merge", "t"], "approval_required", 5),
        (
            vec![
                "run",
                "create",
                "--objective",
                "x",
                "--repo",
                repo,
                "--base=-x",
            ],
            "invalid",
            7,
        ),
        (vec!["frobnicate"], "usage", 2),
    ];
    for (args, code_name, exit) in cases {
        let (code, doc) = f.json_err(&args);
        assert_eq!(code, exit, "{args:?}: {}", doc.canonical());
        assert_schema(&doc, "fleet.error/1");
        assert_eq!(keys(&doc), ["error", "schema"]);
        let e = doc.get("error").unwrap();
        assert_eq!(keys(e), ["code", "message", "subject"], "{args:?}");
        assert_eq!(jstr(e, "code"), code_name, "{args:?}");
        assert!(!jstr(e, "message").is_empty());
        // the exit code is the one the domain assigns to that error code
        let parsed = ErrorCode::parse(jstr(e, "code")).expect("a known error code");
        assert_eq!(parsed.exit_code(), exit, "{args:?}");
        if code_name == "approval_required" {
            assert_eq!(jstr(e, "subject"), "a1");
        }
    }
    // in text mode the same errors go to stderr, one `fleet:` line first
    let out = f.cli(&["stop", "nosuch"]);
    assert_eq!(out.code, 4);
    assert_eq!(out.stdout, "");
    assert!(out.stderr_first_line().starts_with("fleet: "), "{out:?}");
}

#[test]
fn without_a_daemon_commands_exit_with_daemon_down() {
    let f = Fleet::new();
    let out = f.cli(&["status"]);
    assert_eq!(out.code, 3, "{out:?}");
    assert!(out.stderr.contains("fleet daemon"), "{out:?}");
    let (code, doc) = f.json_err(&["status"]);
    assert_eq!(code, 3);
    assert_eq!(jstr(doc.get("error").unwrap(), "code"), "daemon_down");
    for args in [
        vec!["gate", "list"],
        vec!["approval", "list"],
        vec!["events"],
        vec!["shutdown"],
    ] {
        assert_eq!(f.cli(&args).code, 3, "{args:?}");
    }
}

#[test]
fn status_json_has_exactly_the_documented_keys() {
    let mut f = Fleet::new();
    let body = "case \"$FLEET_TASK\" in\n  q) printf '{\"version\":1,\"kind\":\"question\",\"text\":\"which?\",\"choices\":[\"a\",\"b\"]}' > .fleet/question.json ;;\n  *) echo x > x.txt ;;\nesac\n";
    let s = f.script("w.sh", body);
    f.write_cfg(&simple_cfg(&s, "").set("approval_ttl_secs", "900"));
    f.path_env = path_without("agent-undo");
    f.start();
    f.plan_and_start("r1", 2, &[t("q"), t("d")]);
    f.wait_blocked("r1", "q", "question");
    f.wait_task("r1", "d", &["completed"]);
    assert_eq!(f.cli(&["merge", "d"]).code, 5);

    let st = f.status();
    assert_eq!(
        keys(&st),
        [
            "approvals",
            "daemon",
            "gates",
            "now_ms",
            "recent",
            "runs",
            "schema"
        ]
    );
    assert_eq!(
        keys(st.get("daemon").unwrap()),
        [
            "max_sessions",
            "output_bytes",
            "pid",
            "rss_kb",
            "sessions",
            "undo",
            "uptime_ms",
            "version"
        ]
    );
    let daemon = st.get("daemon").unwrap();
    assert_eq!(jstr(daemon, "undo"), "unavailable");
    assert_eq!(jstr(daemon, "version"), env!("CARGO_PKG_VERSION"));
    assert_eq!(jnum(daemon, "pid"), u64::from(f.daemon_pid()));
    let run = &jarr(&st, "runs")[0];
    assert_eq!(
        keys(run),
        [
            "base_ref",
            "base_sha",
            "counts",
            "id",
            "integration_branch",
            "max_parallel",
            "objective",
            "repo",
            "status",
            "tasks"
        ]
    );
    assert_eq!(
        keys(run.get("counts").unwrap()),
        [
            "blocked",
            "completed",
            "failed",
            "pending",
            "ready",
            "review",
            "running"
        ]
    );
    assert!(!jstr(run, "base_ref").is_empty());
    assert_eq!(jstr(run, "base_sha").len(), 40);
    assert_eq!(jstr(run, "integration_branch"), "fleet/r1/integration");
    for task in jarr(run, "tasks") {
        assert_eq!(
            keys(task),
            [
                "attempt",
                "block",
                "branch",
                "deps",
                "failure",
                "gates",
                "head",
                "id",
                "live",
                "merged",
                "retries_used",
                "status",
                "summary",
                "title",
                "worker",
                "worktree"
            ]
        );
    }
    let gate = &jarr(&st, "gates")[0];
    assert_eq!(
        keys(gate),
        ["answer", "choices", "id", "kind", "open", "run", "task", "text"]
    );
    let approval = &jarr(&st, "approvals")[0];
    assert_eq!(
        keys(approval),
        [
            "args",
            "command",
            "digest",
            "display",
            "expires_ms",
            "id",
            "purpose",
            "requested_ms",
            "run",
            "status",
            "task",
            "tool"
        ]
    );
    // a status filtered to an unknown run is empty, not an error
    let none = f.json(&["status", "--run", "nosuch"]);
    assert!(jarr(&none, "runs").is_empty());
}

#[test]
fn a_json_plan_file_loads_like_a_toml_one() {
    let f = quick_fleet();
    let doc = format!(
        "{{\"run\":{{\"objective\":\"json plan\",\"repo\":\"{}\",\"id\":\"rj\"}},\"tasks\":[{{\"id\":\"a\",\"title\":\"A\"}},{{\"id\":\"b\",\"title\":\"B\",\"after\":[\"a\"]}}]}}",
        f.repo.display()
    );
    let file = f.tmp.write("plan.json", &doc);
    let out = f.json(&["plan", file.to_str().unwrap()]);
    assert_schema(&out, "fleet.plan/1");
    let st = f.status();
    let run = &jarr(&st, "runs")[0];
    assert_eq!(jstr(run, "id"), "rj");
    assert_eq!(jarr(run, "tasks").len(), 2);
}

// ---- doctor ------------------------------------------------------------------------------------------------

#[test]
fn doctor_is_healthy_for_a_good_setup_with_and_without_a_daemon() {
    let mut f = quick_fleet();
    let out = f.ok(&["doctor"]);
    assert!(out.stdout.contains("[ok  ] git"), "{out:?}");
    assert!(out.stdout.contains("[ok  ] state dir"), "{out:?}");
    assert!(out.stdout.contains("[ok  ] socket permissions"), "{out:?}");
    assert!(
        out.stdout.contains("[ok  ] daemon: running and answering"),
        "{out:?}"
    );
    assert!(out.stdout.trim_end().ends_with("healthy"), "{out:?}");
    // with the configuration named explicitly its workers are listed
    let cfg = f.config.to_str().unwrap().to_string();
    let with_cfg = f.ok(&["doctor", "--config", &cfg]);
    assert!(with_cfg.stdout.contains("worker w"), "{with_cfg:?}");
    assert_eq!(f.shutdown(), 0);
    // after the daemon is gone the state directory is still fine
    let out = f.ok(&["doctor"]);
    assert!(out.stdout.contains("[info] daemon: not running"), "{out:?}");
    assert!(out.stdout.trim_end().ends_with("healthy"), "{out:?}");
}

#[test]
fn doctor_fails_with_exit_13_for_a_loose_state_dir_or_a_missing_git() {
    let mut f = Fleet::new();
    fs::create_dir(&f.state).unwrap();
    fs::set_permissions(&f.state, fs::Permissions::from_mode(0o755)).unwrap();
    let out = f.cli(&["doctor"]);
    assert_eq!(out.code, 13, "{out:?}");
    assert!(out.stdout.contains("[FAIL] state dir"), "{out:?}");
    assert!(out.stdout.contains("755"), "{out:?}");
    assert!(
        out.stdout.contains("unhealthy: 1 check(s) failed"),
        "{out:?}"
    );
    // doctor only looks: the directory was not changed
    assert_eq!(mode_of(&f.state), 0o755);
    let json = f.cli(&["doctor", "--json"]);
    assert_eq!(json.code, 13);
    let doc = json.json();
    assert_schema(&doc, "fleet.doctor/1");
    assert!(!jbool(&doc, "ok"));
    assert!(jarr(&doc, "checks")
        .iter()
        .any(|c| jstr(c, "name") == "state dir" && jstr(c, "status") == "fail"));

    // fix the mode, then take git away
    fs::set_permissions(&f.state, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(f.cli(&["doctor"]).code, 0);
    let empty = f.tmp.join("empty-bin");
    fs::create_dir(&empty).unwrap();
    f.path_env = empty.to_str().unwrap().to_string();
    let out = f.cli(&["doctor"]);
    assert_eq!(out.code, 13, "{out:?}");
    assert!(out.stdout.contains("[FAIL] git"), "{out:?}");
}

#[test]
fn doctor_json_lists_checks_with_names_statuses_and_details() {
    let f = Fleet::new();
    let doc = f.json(&["doctor"]);
    assert_schema(&doc, "fleet.doctor/1");
    assert_eq!(keys(&doc), ["checks", "ok", "schema"]);
    let checks = jarr(&doc, "checks");
    let names: Vec<&str> = checks.iter().map(|c| jstr(c, "name")).collect();
    for want in [
        "platform",
        "git",
        "state dir",
        "socket permissions",
        "daemon",
        "event log",
        "agent-undo",
    ] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }
    for c in checks {
        assert_eq!(keys(c), ["detail", "name", "status"]);
        assert!(["ok", "info", "fail"].contains(&jstr(c, "status")));
    }
}

// ---- attach and the dashboard -------------------------------------------------------------------------------

#[test]
fn attach_with_piped_stdin_echoes_input_and_detaches_leaving_the_worker_running() {
    let mut f = Fleet::new();
    let s = f.script(
        "w.sh",
        "echo attach-ready\nwhile read line; do echo \"echoed: $line\"; done\n",
    );
    f.write_cfg(&simple_cfg(&s, ""));
    f.start();
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_live("r1", "t");

    let mut c = f.command();
    c.arg("--state-dir").arg(&f.state).args(["attach", "t"]);
    c.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = c.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdout.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut accumulated = Vec::<u8>::new();
    let mut seen = move || {
        while let Ok(chunk) = rx.try_recv() {
            accumulated.extend_from_slice(&chunk);
        }
        String::from_utf8_lossy(&accumulated).into_owned()
    };

    stdin.write_all(b"hello\r").unwrap();
    stdin.flush().unwrap();
    assert!(
        wait_until(WAIT_MS, || seen().contains("echoed: hello")),
        "the worker's echo never reached the attached terminal: {:?}",
        seen()
    );
    // Ctrl-] detaches; the attach command exits 0
    stdin.write_all(&[0x1d]).unwrap();
    stdin.flush().unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            panic!(
                "attach did not exit after the detach key; output {:?}",
                seen()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(0));
    let _ = reader.join();
    drop(stdin);

    // the worker lives on, still reading
    let task = f.task("r1", "t");
    assert_eq!(jstr(&task, "status"), "running", "{}", task.canonical());
    assert!(jbool(&task, "live"));
    f.ok(&["send", "t", "after-detach"]);
    assert!(
        wait_until(WAIT_MS, || f
            .ok(&["tail", "t"])
            .stdout
            .contains("echoed: after-detach")),
        "the worker no longer reads its terminal"
    );
}

#[test]
fn attach_without_a_session_is_a_conflict_and_an_unknown_task_is_not_found() {
    let f = quick_fleet();
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_task("r1", "t", &["completed"]);
    let out = f.cli(&["attach", "t"]);
    assert_eq!(out.code, 6, "{out:?}");
    assert!(out.stderr.contains("no running session"), "{out:?}");
    assert_eq!(f.cli(&["attach", "nosuch"]).code, 4);
    // --json makes no sense for an interactive command
    assert_eq!(f.cli(&["attach", "t", "--json"]).code, 2);
}

#[test]
fn the_dashboard_without_a_terminal_fails_with_a_usage_message_not_a_panic() {
    let f = quick_fleet();
    let out = f.cli(&["tui"]);
    assert_eq!(out.code, 2, "{out:?}");
    assert!(out.stderr_first_line().starts_with("fleet: "), "{out:?}");
    assert!(out.stderr.contains("interactive terminal"), "{out:?}");
    assert!(!out.stderr.contains("panicked"), "{out:?}");
    assert_eq!(out.stdout, "");
    assert_eq!(f.cli(&["tui", "--json"]).code, 2);
}

// ---- the optional undo tool -----------------------------------------------------------------------------------------

fn undo_calls(log: &str) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    for line in log.lines() {
        if line == "---" {
            out.push(Vec::new());
        } else if let Some(last) = out.last_mut() {
            last.push(line.to_string());
        }
    }
    out
}

#[test]
fn a_fake_agent_undo_gets_checkpoints_and_undo_requests() {
    let bin = TempDir::new("b");
    let log = bin.join("undo.log");
    let tool = bin.join("agent-undo");
    fs::write(
        &tool,
        fill(
            "#!/bin/sh\n{ echo ---; for a in \"$@\"; do echo \"$a\"; done; } >> @LOG@\necho '{\"ok\":true}'\n",
            &[("LOG", &log)],
        ),
    )
    .unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();

    let mut f = Fleet::new();
    let s = f.script("w.sh", "echo change > changed.txt\n");
    f.write_cfg(&simple_cfg(&s, ""));
    f.path_env = format!("{}:{}", bin.path().display(), path_without("agent-undo"));
    f.start();
    assert_eq!(jstr(f.status().get("daemon").unwrap(), "undo"), "available");
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_task("r1", "t", &["completed"]);

    let wt = f.worktree("r1", "t");
    let wt_s = wt.to_str().unwrap().to_string();
    let call = |tail: &[&str]| -> Vec<String> {
        let mut v = vec!["-C".to_string(), wt_s.clone(), "--json".to_string()];
        v.extend(tail.iter().map(|s| s.to_string()));
        v
    };
    let calls = undo_calls(&fs::read_to_string(&log).unwrap());
    assert!(
        calls.contains(&call(&["checkpoint", "--label", "fleet before r1/t"])),
        "no checkpoint before dispatch in {calls:?}"
    );
    assert!(
        calls.contains(&call(&["checkpoint", "--label", "fleet after r1/t"])),
        "no checkpoint after review in {calls:?}"
    );
    let recent = f.recent_lines();
    assert!(
        recent.iter().any(|l| l.contains("r1/t checkpoint before")),
        "{recent:?}"
    );
    assert!(
        recent.iter().any(|l| l.contains("r1/t checkpoint after")),
        "{recent:?}"
    );

    // a preview, without --apply
    let before = calls.len();
    let preview = f.json(&["undo", "r1/t"]);
    assert_schema(&preview, "fleet.undo/1");
    assert!(!jbool(&preview, "applied"));
    let calls = undo_calls(&fs::read_to_string(&log).unwrap());
    assert_eq!(calls.len(), before + 1);
    assert_eq!(calls.last().unwrap(), &call(&["undo", "--last"]));
    let text = f.ok(&["undo", "r1/t"]);
    assert!(!text.stdout.is_empty());
    // and the real thing
    let applied = f.json(&["undo", "r1/t", "--apply"]);
    assert!(jbool(&applied, "applied"));
    let calls = undo_calls(&fs::read_to_string(&log).unwrap());
    assert_eq!(calls.last().unwrap(), &call(&["undo", "--last", "--apply"]));
    // nothing but the preview ran `--apply`
    let applies = calls
        .iter()
        .filter(|c| c.iter().any(|a| a == "--apply"))
        .count();
    assert_eq!(applies, 1, "{calls:?}");
}

#[test]
fn without_agent_undo_fleet_works_and_undo_is_unavailable() {
    let mut f = Fleet::new();
    let s = f.script("w.sh", "echo change > changed.txt\n");
    f.write_cfg(&simple_cfg(&s, ""));
    f.path_env = path_without("agent-undo");
    f.start();
    assert_eq!(
        jstr(f.status().get("daemon").unwrap(), "undo"),
        "unavailable"
    );
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_task("r1", "t", &["completed"]);
    assert!(!f.recent_lines().iter().any(|l| l.contains("checkpoint")));
    let out = f.cli(&["undo", "r1/t"]);
    assert_eq!(out.code, 9, "{out:?}");
    assert!(out.stderr.contains("agent-undo"), "{out:?}");
    let (code, doc) = f.json_err(&["undo", "r1/t", "--apply"]);
    assert_eq!(code, 9);
    assert_eq!(jstr(doc.get("error").unwrap(), "code"), "unavailable");
    // an unknown task is still reported as such
    assert_eq!(f.cli(&["undo", "nosuch"]).code, 4);
    assert!(!f.recent_lines().is_empty());
}
