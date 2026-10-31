//! Security properties, end to end: private state, a control plane workers cannot reach, hostile
//! files and repositories, and approvals that cannot be reused for something else.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod common;

use common::*;
use fleet_infra::testutil::{git, wait_until, TempDir};
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;
use std::time::Duration;

use fleet_domain::error::{ErrorCode, FleetError};
use fleet_domain::value::Value;
use fleet_infra::protocol::{
    encode_request, parse_response, read_line_bounded, MAX_RESPONSE_BYTES,
};
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Instant;

use fleet_infra::client::{AttachEvent, Client};

/// Variables a worker or external command may see: the allowlist, what fleet adds, and what a
/// shell or the pty layer adds by itself.
const TOLERATED: [&str; 16] = [
    "PATH",
    "HOME",
    "LANG",
    "TERM",
    "FLEET_RUN",
    "FLEET_TASK",
    "FLEET_WORKTREE",
    "FLEET_BRIEF_FILE",
    "XDG_STATE_HOME",
    "SHELL",
    "PWD",
    "OLDPWD",
    "SHLVL",
    "_",
    "LINES",
    "COLUMNS",
];

fn env_names(dump: &str) -> BTreeSet<String> {
    dump.lines()
        .filter_map(|l| l.split_once('=').map(|(k, _)| k.to_string()))
        .filter(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .collect()
}

fn assert_only_tolerated(dump: &str) {
    for name in env_names(dump) {
        assert!(
            TOLERATED.contains(&name.as_str()),
            "unexpected variable {name} in the child environment:\n{dump}"
        );
    }
}

fn one_task_fleet(body: &str, cfg: Cfg) -> Fleet {
    let mut f = Fleet::new();
    let s = f.script("w.sh", body);
    f.write_cfg(&cfg.worker("w", &s, &["{task}"], ""));
    f.start();
    f
}

fn plain_cfg() -> Cfg {
    Cfg::new()
}

// ---- state directory and socket ---------------------------------------------------------------------

#[test]
fn state_directory_control_directory_and_socket_are_private() {
    let f = one_task_fleet("exit 0", plain_cfg());
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_task("r1", "t", &["completed"]);
    assert_eq!(mode_of(&f.state), 0o700);
    assert_eq!(mode_of(&f.state.join("control")), 0o700);
    assert_eq!(mode_of(&f.state.join("control/fleet.sock")), 0o600);
    assert_eq!(mode_of(&f.state.join("logs")), 0o700);
    assert_eq!(mode_of(&f.state.join("events.log")), 0o600);
    assert_eq!(mode_of(&f.state.join("events.head")), 0o600);
    assert_eq!(mode_of(&f.state.join("daemon.lock")), 0o600);
    assert_eq!(mode_of(&f.state.join("control/token")), 0o600);
    for log in walk_files(&f.state.join("logs")) {
        assert_eq!(mode_of(&log), 0o600, "{}", log.display());
    }
    // doctor agrees
    assert_eq!(f.cli(&["doctor"]).code, 0);
}

#[test]
fn a_loose_existing_state_directory_is_tightened_not_trusted() {
    let mut f = Fleet::new();
    fs::create_dir_all(f.state.join("control")).unwrap();
    for dir in [f.state.clone(), f.state.join("control")] {
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    }
    f.start();
    // ensure_private_dir removes group and other access from directories it finds
    assert_eq!(mode_of(&f.state), 0o700);
    assert_eq!(mode_of(&f.state.join("control")), 0o700);
    assert_eq!(mode_of(&f.state.join("control/fleet.sock")), 0o600);
}

#[test]
fn a_state_directory_that_is_a_symlink_is_refused() {
    let f = Fleet::new();
    let real = f.tmp.join("real");
    fs::create_dir(&real).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&real, &f.state).unwrap();
    let out = f.daemon_refused();
    assert_eq!(out.code, 7, "{out:?}");
    assert!(out.stderr.contains("symlink"), "{out:?}");
    assert!(
        !real.join("control").exists(),
        "the daemon used the symlinked directory"
    );
}

#[test]
fn an_event_log_that_is_a_symlink_is_refused_and_its_target_untouched() {
    let f = Fleet::new();
    fs::create_dir(&f.state).unwrap();
    fs::set_permissions(&f.state, fs::Permissions::from_mode(0o700)).unwrap();
    let target = f.tmp.write("elsewhere.log", "");
    symlink(&target, f.state.join("events.log")).unwrap();
    let out = f.daemon_refused();
    assert_eq!(out.code, 11, "{out:?}");
    assert!(out.stderr.contains("symlink"), "{out:?}");
    assert_eq!(
        fs::read(&target).unwrap().len(),
        0,
        "the daemon wrote through the symlink"
    );
}

#[test]
fn a_state_directory_inside_a_repository_is_rejected_when_a_run_is_created() {
    let mut f = Fleet::new();
    f.state = f.repo.join("st");
    f.start();
    let repo = f.repo.to_str().unwrap().to_string();
    let out = f.cli(&["run", "create", "--objective", "x", "--repo", &repo]);
    assert_eq!(out.code, 7, "{out:?}");
    assert!(out.stderr.contains("state directory"), "{out:?}");
    // a plan with a [run] table takes the same road
    let plan = f
        .tmp
        .write("plan.toml", &plan_toml(&f.repo, "r1", 1, &[t("a")]));
    let out = f.cli(&["plan", plan.to_str().unwrap()]);
    assert_eq!(out.code, 7, "{out:?}");
    assert!(jarr(&f.status(), "runs").is_empty());
}

// ---- the control plane is out of reach for workers --------------------------------------------------

#[test]
fn workers_cannot_learn_the_control_plane_or_the_daemons_secrets() {
    let out = TempDir::new("o");
    let mut f = Fleet::new();
    let secret = unique_token("sentinel-secret");
    let token = unique_token("sentinel-token");
    f.daemon_env
        .push(("FLEET_TEST_SENTINEL_SECRET".into(), secret.clone()));
    f.daemon_env.push(("SENTINEL_TOKEN".into(), token.clone()));
    let body = fill(
        "env | sort > @OUT@/env.txt\nprintf '%s\\n' \"$0\" \"$@\" > @OUT@/argv.txt\necho worker-output-marker\n",
        &[("OUT", out.path())],
    );
    let s = f.script("w.sh", &body);
    f.write_cfg(&plain_cfg().worker("w", &s, &["{task}"], ""));
    f.start();
    let daemon_token = fs::read_to_string(f.state.join("control/token")).unwrap();
    assert_eq!(
        daemon_token.len(),
        64,
        "the control token is 32 bytes of hex"
    );
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_task("r1", "t", &["completed"]);

    let env = out.read("env.txt");
    let argv = out.read("argv.txt");
    let state = f.state.to_str().unwrap();
    let state_slash = format!("{state}/");
    assert!(!env.is_empty(), "the worker did not dump its environment");
    for dump in [&env, &argv] {
        for forbidden in [
            "fleet.sock",
            "FLEET_SOCKET",
            "FLEET_TEST_SENTINEL_SECRET",
            "SENTINEL_TOKEN",
            "GIT_",
            secret.as_str(),
            token.as_str(),
            daemon_token.as_str(),
            state_slash.as_str(),
        ] {
            assert!(
                !dump.contains(forbidden),
                "{forbidden:?} leaked to the worker:\n{dump}"
            );
        }
    }
    assert_only_tolerated(&env);
    let names = env_names(&env);
    for must in [
        "FLEET_RUN",
        "FLEET_TASK",
        "FLEET_WORKTREE",
        "FLEET_BRIEF_FILE",
        "PATH",
        "HOME",
    ] {
        assert!(names.contains(must), "{must} missing:\n{env}");
    }
    let wt = f.worktree("r1", "t");
    assert!(env.lines().any(|l| l == "FLEET_RUN=r1"), "{env}");
    assert!(env.lines().any(|l| l == "FLEET_TASK=t"), "{env}");
    assert!(
        env.lines()
            .any(|l| l == format!("FLEET_WORKTREE={}", wt.display())),
        "{env}"
    );
    assert!(
        env.lines()
            .any(|l| l == format!("FLEET_BRIEF_FILE={}/.fleet/brief.md", wt.display())),
        "{env}"
    );
    assert!(
        env.lines()
            .any(|l| l == format!("HOME={}", f.home.display())),
        "{env}"
    );
    assert_eq!(argv.lines().last(), Some("t"), "{argv}");
    // `fleet` run by the worker without --state-dir must not find the operator's daemon
    assert!(
        env.lines()
            .any(|l| l == format!("XDG_STATE_HOME={}/.fleet/xdg-state", wt.display())),
        "{env}"
    );

    // and the secrets are nowhere in what fleet stores or shows either
    let tail = f.ok(&["tail", "t"]).stdout;
    assert!(tail.contains("worker-output-marker"), "{tail:?}");
    let st = f.cli(&["status", "--json"]).stdout;
    let brief = fs::read_to_string(wt.join(".fleet/brief.md")).unwrap();
    for shown in [&tail, &st, &brief, &f.daemon_log()] {
        assert!(
            !shown.contains(&daemon_token),
            "the token was shown: {shown}"
        );
        assert!(
            !shown.contains(&secret) && !shown.contains(&token),
            "{shown}"
        );
    }
    assert!(tree_contains(&f.state, &secret).is_none());
    assert!(tree_contains(&f.state, &token).is_none());

    // the control token and the socket path are in nothing the worker can read or has written
    let doctor = f.cli(&["doctor"]);
    assert_eq!(doctor.code, 0, "{doctor:?}");
    for shown in [&doctor.stdout, &doctor.stderr, &f.events_raw()] {
        assert!(
            !shown.contains(&daemon_token),
            "the token was shown: {shown}"
        );
    }
    assert!(
        tree_contains(&wt, &daemon_token).is_none(),
        "the control token is in the worker's worktree"
    );
    for needle in ["fleet.sock", state_slash.as_str()] {
        assert!(
            tree_contains(&wt, needle).is_none(),
            "{needle:?} is in the worker's worktree"
        );
    }
}

#[test]
fn a_worker_cannot_approve_resolve_or_read_status_but_the_operator_can() {
    let out = TempDir::new("o");
    let mut f = Fleet::new();
    let bin = Path::new(FLEET_BIN);
    let body = fill(
        "case \"$FLEET_TASK\" in\n  spy)\n    while [ ! -f @OUT@/go ]; do sleep 0.1; done\n    @FLEET@ --state-dir @STATE@ approve a1 > @OUT@/approve.out 2>&1; echo $? > @OUT@/approve.code\n    @FLEET@ --state-dir @STATE@ gate resolve g1 --answer x > @OUT@/gate.out 2>&1; echo $? > @OUT@/gate.code\n    @FLEET@ --state-dir @STATE@ status > @OUT@/status.out 2>&1; echo $? > @OUT@/status.code\n    @FLEET@ --state-dir @STATE@ status --json > @OUT@/json.out 2>&1; echo $? > @OUT@/json.code\n    @FLEET@ --state-dir @STATE@ merge done > @OUT@/merge.out 2>&1; echo $? > @OUT@/merge.code\n    @FLEET@ --state-dir @STATE@ stop done > @OUT@/stop.out 2>&1; echo $? > @OUT@/stop.code\n    @FLEET@ --state-dir @STATE@ send spy hello > @OUT@/send.out 2>&1; echo $? > @OUT@/send.code\n    @FLEET@ --state-dir @STATE@ shutdown > @OUT@/shutdown.out 2>&1; echo $? > @OUT@/shutdown.code\n    echo finished > @OUT@/all.done\n    ;;\n  *) echo quiet > f.txt ;;\nesac\n",
        &[("OUT", out.path()), ("FLEET", bin), ("STATE", &f.state)],
    );
    let s = f.script("w.sh", &body);
    f.write_cfg(&plain_cfg().worker("w", &s, &["{task}"], ""));
    f.start();
    f.plan_and_start("r1", 2, &[t("done"), t("spy")]);
    f.wait_task("r1", "done", &["completed"]);
    f.wait_live("r1", "spy");
    // a pending approval for the spy to attack
    assert_eq!(f.cli(&["merge", "done"]).code, 5);
    assert_eq!(jstr(&f.approvals()[0], "status"), "pending");
    fs::write(out.join("go"), "").unwrap();
    wait_file(&out.join("all.done"));

    for name in [
        "approve", "gate", "status", "json", "merge", "stop", "send", "shutdown",
    ] {
        let code = out.read(&format!("{name}.code"));
        let text = out.read(&format!("{name}.out"));
        assert_eq!(code.trim(), "10", "{name}: {text}");
        assert!(text.contains("worker processes"), "{name}: {text}");
    }
    assert!(
        !out.read("status.out").contains("completed"),
        "a worker read the status"
    );
    assert!(
        !out.read("json.out").contains("fleet.status"),
        "a worker read the status"
    );
    // nothing changed
    let approvals = f.approvals();
    assert_eq!(approvals.len(), 1);
    assert_eq!(jstr(&approvals[0], "status"), "pending");
    assert!(jstr(&f.task("r1", "done"), "merged").is_empty());

    // the same commands from the operator work
    f.wait_task("r1", "spy", &["completed"]);
    f.ok(&["approve", "a1"]);
    f.ok(&["merge", "done"]);
    assert!(!jstr(&f.task("r1", "done"), "merged").is_empty());
}

/// One request over a fresh connection, the way any process of the same user could send it.
fn raw_call(socket: &Path, cmd: &str, token: Option<&str>) -> Result<Value, FleetError> {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut line = encode_request(cmd, &Value::Null, token);
    line.push('\n');
    stream.write_all(line.as_bytes()).unwrap();
    let mut reader = BufReader::new(stream);
    let reply = read_line_bounded(&mut reader, MAX_RESPONSE_BYTES)
        .unwrap()
        .expect("the daemon answers");
    parse_response(&reply)
}

#[test]
fn the_control_socket_needs_the_per_run_token_for_everything_but_ping() {
    let mut f = one_task_fleet("exit 0", plain_cfg());
    let socket = f.state.join("control/fleet.sock");
    let token_file = f.state.join("control/token");
    let token = fs::read_to_string(&token_file).unwrap();
    // the socket's file mode is not the only barrier any more
    assert!(raw_call(&socket, "ping", None).is_ok());
    for cmd in ["status", "shutdown", "approval.list"] {
        let err = raw_call(&socket, cmd, None).unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden, "{cmd}: {err:?}");
        let err = raw_call(&socket, cmd, Some(&"0".repeat(64))).unwrap_err();
        assert_eq!(
            err.code,
            ErrorCode::Forbidden,
            "{cmd} with a wrong token: {err:?}"
        );
        assert!(
            !err.message.contains(&token),
            "an error must not repeat the token"
        );
    }
    // the refused shutdown did not stop anything; the real token works for the same commands
    assert!(raw_call(&socket, "status", Some(&token)).is_ok());
    assert_eq!(f.cli(&["status"]).code, 0);

    // a new daemon run has a new token, and the old one is worth nothing
    f.kill_daemon();
    f.start();
    let fresh = fs::read_to_string(&token_file).unwrap();
    assert_ne!(fresh, token);
    assert_eq!(mode_of(&token_file), 0o600);
    let err = raw_call(&socket, "status", Some(&token)).unwrap_err();
    assert_eq!(err.code, ErrorCode::Forbidden);
    assert!(raw_call(&socket, "status", Some(&fresh)).is_ok());
}

#[test]
fn a_worker_running_fleet_without_a_state_dir_finds_no_daemon_but_the_operator_does() {
    let out = TempDir::new("o");
    let mut f = Fleet::new();
    // The operator's daemon is where `fleet` looks by default: below HOME, which workers keep.
    f.state = f.home.join(".local/state/agent-fleet");
    let body = fill(
        "@FLEET@ status > @OUT@/status.out 2>&1; echo $? > @OUT@/status.code\n@FLEET@ approve a1 > @OUT@/approve.out 2>&1; echo $? > @OUT@/approve.code\necho \"$XDG_STATE_HOME\" > @OUT@/xdg.txt\n",
        &[("OUT", out.path()), ("FLEET", Path::new(FLEET_BIN))],
    );
    let s = f.script("w.sh", &body);
    f.write_cfg(&plain_cfg().worker("w", &s, &["{task}"], ""));
    f.start();
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_task("r1", "t", &["completed"]);

    for name in ["status", "approve"] {
        let code = out.read(&format!("{name}.code"));
        let text = out.read(&format!("{name}.out"));
        assert_eq!(code.trim(), "3", "{name} must find no daemon: {text}");
        assert!(!text.contains("fleet.status"), "{name}: {text}");
    }
    let wt = f.worktree("r1", "t");
    assert_eq!(
        out.read("xdg.txt").trim(),
        format!("{}/.fleet/xdg-state", wt.display())
    );
    assert!(!out.read("xdg.txt").contains(f.state.to_str().unwrap()));

    // the same lookup with the operator's own environment does reach the daemon
    let mut c = f.command();
    c.arg("status");
    let operator = run_cmd(c, Duration::from_secs(60));
    assert_eq!(operator.code, 0, "{operator:?}");
}

// ---- a worker that never reads its input cannot hold the daemon ---------------------------------------

#[test]
fn input_to_a_worker_that_never_reads_fails_with_an_error_and_the_daemon_stays_responsive() {
    let f = one_task_fleet("exec sleep 120", plain_cfg());
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_live("r1", "t");
    let mut client = Client::connect(&f.state.join("control/fleet.sock")).unwrap();
    let text = "x".repeat(4000);
    let started = Instant::now();
    let mut refused = 0;
    for i in 0..600 {
        let args = Value::object([
            ("task", Value::str("t")),
            ("text", Value::str(text.as_str())),
            ("newline", Value::Bool(false)),
        ]);
        let call = Instant::now();
        match client.call("task.send", args) {
            Ok(_) => {}
            Err(e) => {
                assert_eq!(e.code, ErrorCode::Unavailable, "send {i}: {e:?}");
                assert!(e.message.contains("not reading its input"), "{e:?}");
                refused += 1;
            }
        }
        assert!(
            call.elapsed() < Duration::from_secs(10),
            "send {i} took {:?}",
            call.elapsed()
        );
    }
    assert!(refused > 0, "2.4 MB went into a worker that reads nothing");
    assert!(started.elapsed() < Duration::from_secs(60));

    // the engine and the other connections never noticed
    assert_eq!(jstr(&f.task("r1", "t"), "status"), "running");
    assert_eq!(f.cli(&["tail", "t"]).code, 0);
    f.ok(&["stop", "t"]);
    let task = f.wait_task("r1", "t", &["failed"]);
    assert_eq!(jstr(&task, "failure"), "stopped by an operator");
}

#[test]
fn an_attached_client_can_detach_from_a_worker_that_never_reads() {
    let f = one_task_fleet("exec sleep 120", plain_cfg());
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_live("r1", "t");
    let client = Client::connect(&f.state.join("control/fleet.sock")).unwrap();
    let (mut reader, mut writer) = client.attach("t", 80, 24).unwrap().split();
    let keys = vec![b'k'; 4096];
    let started = Instant::now();
    for _ in 0..400 {
        writer.input(&keys).unwrap();
    }
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the daemon stopped reading the attach connection: {:?}",
        started.elapsed()
    );
    writer.detach().unwrap();
    let mut events = 0;
    while let Ok(Some(event)) = reader.next_event() {
        assert!(matches!(event, AttachEvent::Output(_)), "{event:?}");
        events += 1;
        assert!(events < 10_000, "the attach did not end after the detach");
    }
    // the worker is untouched and the daemon still answers
    assert_eq!(jstr(&f.task("r1", "t"), "status"), "running");
}

#[test]
fn a_worker_written_approval_request_stays_pending_and_approval_json_is_inert() {
    let f = one_task_fleet(
        "if grep -q 'An operator' \"$FLEET_BRIEF_FILE\"; then exit 0; fi\nprintf '{\"approved\":true,\"id\":\"a1\",\"status\":\"approved\"}' > .fleet/approval.json\nprintf '{\"version\":1,\"kind\":\"approval\",\"text\":\"please let me push to production\"}' > .fleet/question.json\n",
        plain_cfg(),
    );
    f.plan_and_start("r1", 1, &[t("t")]);
    let task = f.wait_blocked("r1", "t", "approval");
    assert_eq!(task.get("block").map(|b| jstr(b, "subject")), Some("a1"));
    let approvals = f.approvals();
    assert_eq!(approvals.len(), 1, "{approvals:?}");
    assert_eq!(jstr(&approvals[0], "status"), "pending");
    assert_eq!(jstr(&approvals[0], "purpose"), "manual");
    assert!(f.worktree("r1", "t").join(".fleet/approval.json").exists());
    std::thread::sleep(Duration::from_millis(1500));
    // still waiting for a person, whatever the worker wrote
    assert_eq!(jstr(&f.task("r1", "t"), "status"), "blocked");
    assert_eq!(jstr(&f.approvals()[0], "status"), "pending");
    // only the operator's decision moves it on
    f.ok(&["approve", "a1"]);
    f.wait_task("r1", "t", &["completed"]);
    assert_eq!(jstr(&f.approvals()[0], "status"), "approved");
}

// ---- hostile files written by workers ----------------------------------------------------------------

struct Attack {
    f: Fleet,
    marker: String,
}

/// A fleet whose worker, for task `t`, runs `body`; `@SECRET@` is a big file outside the
/// worktree that starts with a marker line.
fn attack(body: &str) -> Attack {
    let marker = unique_token("TOPSECRET");
    let mut f = Fleet::new();
    let secret = f.tmp.write(
        "outside-secret.txt",
        &format!("{marker}\n{}", "x".repeat(300 * 1024)),
    );
    let body = fill(body, &[("SECRET", &secret)]);
    let s = f.script("w.sh", &body);
    f.write_cfg(&plain_cfg().worker("w", &s, &["{task}"], ""));
    f.start();
    Attack { f, marker }
}

impl Attack {
    fn assert_survived(&self, what: &str) {
        let f = &self.f;
        let task = f.wait_task("r1", "t", &["completed", "failed"]);
        assert_eq!(
            jstr(&task, "status"),
            "completed",
            "{what}: {}",
            task.canonical()
        );
        let raw = f.events_raw();
        assert!(
            raw.contains("\"type\":\"warning\""),
            "{what}: no warning recorded"
        );
        assert!(raw.contains("result.json ignored"), "{what}: {raw}");
        assert!(
            jstr(&task, "summary").is_empty(),
            "{what}: {}",
            task.canonical()
        );
        // nothing from outside the worktree was read into anything fleet shows or stores
        let shown = [
            f.cli(&["status", "--json"]).stdout,
            f.cli(&["events", "--json"]).stdout,
            f.cli(&["tail", "t"]).stdout,
            raw,
        ];
        for text in &shown {
            assert!(
                !text.contains(&self.marker),
                "{what}: the outside file was read"
            );
        }
        assert!(tree_contains(&f.state, &self.marker).is_none(), "{what}");
        assert!(tree_contains(&f.repo, &self.marker).is_none(), "{what}");
    }
}

#[test]
fn a_result_json_symlink_to_an_outside_file_is_never_followed() {
    let a = attack("ln -s @SECRET@ .fleet/result.json\n");
    a.f.plan_and_start("r1", 1, &[t("t")]);
    a.assert_survived("symlink");
}

#[test]
fn a_result_json_fifo_does_not_hang_the_task() {
    let a = attack("mkfifo .fleet/result.json\n");
    a.f.plan_and_start("r1", 1, &[t("t")]);
    a.assert_survived("fifo");
}

#[test]
fn an_oversized_result_json_is_ignored() {
    let a = attack("head -c 307200 /dev/zero | tr '\\000' 'x' > .fleet/result.json\n");
    a.f.plan_and_start("r1", 1, &[t("t")]);
    a.assert_survived("300 KiB file");
}

#[test]
fn oversized_or_malformed_question_json_is_ignored() {
    let a = attack(
        "printf '{\"version\":1,\"status\":\"done\",\"summary\":\"fine\"}' > .fleet/result.json\ncase \"$FLEET_TASK\" in\n  big) head -c 307200 /dev/zero | tr '\\000' 'x' > .fleet/question.json ;;\n  bad) echo '{not json' > .fleet/question.json ;;\n  link) ln -s @SECRET@ .fleet/question.json ;;\nesac\n",
    );
    let f = &a.f;
    f.plan_and_start("r1", 3, &[t("big"), t("bad"), t("link")]);
    for task in ["big", "bad", "link"] {
        let t = f.wait_task("r1", task, &["completed", "blocked", "failed"]);
        assert_eq!(jstr(&t, "status"), "completed", "{task}: {}", t.canonical());
        assert_eq!(jstr(&t, "summary"), "fine");
    }
    assert!(f.gates().is_empty());
    assert!(f.approvals().is_empty());
    let raw = f.events_raw();
    assert_eq!(raw.matches("question.json ignored").count(), 3, "{raw}");
    assert!(!raw.contains(&a.marker));
    assert!(tree_contains(&f.state, &a.marker).is_none());
}

// ---- hostile repository ---------------------------------------------------------------------------------

#[test]
fn planted_hooks_fsmonitor_and_filters_never_run_and_the_daemons_secret_stays_out_of_git() {
    let markers = TempDir::new("m");
    let mut f = Fleet::new();
    let secret = unique_token("sentinel-secret");
    f.daemon_env
        .push(("FLEET_TEST_SENTINEL_SECRET".into(), secret.clone()));
    f.daemon_env
        .push(("SENTINEL_TOKEN".into(), unique_token("sentinel-token")));

    // attributes are part of the repository's history, like a real hostile project
    fs::write(f.repo.join(".gitattributes"), "* filter=leak\n").unwrap();
    git(&f.repo, &["add", "-A"]);
    git(&f.repo, &["commit", "-q", "-m", "attributes"]);
    let head_before = git(&f.repo, &["rev-parse", "HEAD"]);

    // hooks in the repository, a hooksPath in its config, an fsmonitor and a clean/smudge filter
    let hook_dir = f.tmp.join("evil-hooks");
    fs::create_dir_all(&hook_dir).unwrap();
    let names = [
        "pre-commit",
        "prepare-commit-msg",
        "commit-msg",
        "post-commit",
        "post-checkout",
        "post-merge",
        "pre-merge-commit",
        "post-rewrite",
        "reference-transaction",
        "post-index-change",
    ];
    fs::create_dir_all(f.repo.join(".git/hooks")).unwrap();
    for name in names {
        let body = format!(
            "#!/bin/sh\ntouch {}/hook-{name}\nexit 0\n",
            markers.path().display()
        );
        for dir in [f.repo.join(".git/hooks"), hook_dir.clone()] {
            let p = dir.join(name);
            fs::write(&p, &body).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let fsmonitor = f.tmp.script(
        "fsmonitor.sh",
        &format!(
            "#!/bin/sh\ntouch {}/fsmonitor\nexit 1\n",
            markers.path().display()
        ),
    );
    let filter = f.tmp.script(
        "filter.sh",
        &format!(
            "#!/bin/sh\ntouch {}/filter-$1\ncat\n",
            markers.path().display()
        ),
    );
    let config_path = f.repo.join(".git/config");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str(&format!(
        "[core]\n\tfsmonitor = {}\n\thooksPath = {}\n[filter \"leak\"]\n\tclean = {} clean\n\tsmudge = {} smudge\n\trequired = true\n",
        fsmonitor.display(),
        hook_dir.display(),
        filter.display(),
        filter.display()
    ));
    fs::write(&config_path, config).unwrap();

    let s = f.script("w.sh", "echo \"$FLEET_TASK\" > \"$FLEET_TASK.txt\"\n");
    f.write_cfg(&plain_cfg().worker("w", &s, &["{task}"], ""));
    f.start();
    f.plan_and_start("r1", 2, &[t("a"), t("b").after(&["a"])]);
    f.wait_task("r1", "a", &["completed"]);
    f.wait_task("r1", "b", &["completed"]);
    merge_with_approval(&f, "a");
    merge_with_approval(&f, "b");

    // the work is there ...
    let tree = safe_git(
        &f.repo,
        &["ls-tree", "-r", "--name-only", "fleet/r1/integration"],
    );
    assert!(
        tree.lines().any(|l| l == "a.txt") && tree.lines().any(|l| l == "b.txt"),
        "{tree}"
    );
    assert_eq!(safe_git(&f.repo, &["rev-parse", "HEAD"]), head_before);
    // ... and none of the planted programs ran
    let ran: Vec<String> = fs::read_dir(markers.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(ran.is_empty(), "planted programs ran: {ran:?}");
    // the daemon's secret never reached git's files or a worktree
    assert!(
        tree_contains(&f.repo, &secret).is_none(),
        "the secret leaked into {:?}",
        tree_contains(&f.repo, &secret)
    );
}

// ---- approvals ---------------------------------------------------------------------------------------------

fn completed_task_fleet(cfg: Cfg) -> Fleet {
    let f = one_task_fleet("echo work > work.txt\n", cfg);
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_task("r1", "t", &["completed"]);
    f
}

fn integration_exists(f: &Fleet) -> bool {
    !git(&f.repo, &["branch", "--list", "fleet/r1/integration"]).is_empty()
}

#[test]
fn an_approval_cannot_be_used_after_the_branch_head_changed() {
    let f = completed_task_fleet(plain_cfg());
    let first = f.cli(&["merge", "t"]);
    assert_eq!(first.code, 5, "{first:?}");
    let d1 = jstr(&f.approvals()[0], "digest").to_string();
    f.ok(&["approve", "a1"]);

    // a new commit on the task branch changes what the approval was bound to
    let wt = f.worktree("r1", "t");
    git(
        &wt,
        &["commit", "--allow-empty", "-q", "-m", "sneaky extra commit"],
    );
    let second = f.cli(&["merge", "t"]);
    assert_eq!(
        second.code, 5,
        "the stale approval was accepted: {second:?}"
    );
    assert!(second.stderr.contains("fleet approve a2"), "{second:?}");
    assert!(!integration_exists(&f), "the merge went ahead");
    let approvals = f.approvals();
    assert_eq!(approvals.len(), 2);
    assert_eq!(jstr(&approvals[0], "status"), "approved");
    assert_eq!(jstr(&approvals[1], "status"), "pending");
    assert_ne!(jstr(&approvals[1], "digest"), d1);

    // the new approval covers the new head
    f.ok(&["approve", "a2"]);
    f.ok(&["merge", "t"]);
    let log = git(&f.repo, &["log", "--format=%s", "fleet/r1/integration"]);
    assert!(log.contains("sneaky extra commit"), "{log}");
}

#[test]
fn an_approval_is_used_up_by_the_merge_it_allowed() {
    let f = completed_task_fleet(plain_cfg());
    assert_eq!(f.cli(&["merge", "t"]).code, 5);
    f.ok(&["approve", "a1"]);
    f.ok(&["merge", "t"]);
    assert_eq!(jstr(&f.approvals()[0], "status"), "consumed");
    // approving it again is a conflict, not a second merge
    assert_eq!(f.cli(&["approve", "a1"]).code, 6);
}

#[test]
fn approvals_expire() {
    let f = completed_task_fleet(plain_cfg().set("approval_ttl_secs", "2"));
    assert_eq!(f.cli(&["merge", "t"]).code, 5);
    assert!(
        wait_until(WAIT_MS, || jstr(&f.approvals()[0], "status") == "expired"),
        "the approval never expired"
    );
    let late = f.cli(&["approve", "a1"]);
    assert_eq!(late.code, 6, "{late:?}");
    // the merge asks again instead of using the lapsed approval
    let again = f.cli(&["merge", "t"]);
    assert_eq!(again.code, 5, "{again:?}");
    assert!(again.stderr.contains("fleet approve a2"), "{again:?}");
    assert!(!integration_exists(&f));
}

#[test]
fn a_denied_approval_blocks_the_merge() {
    let f = completed_task_fleet(plain_cfg());
    assert_eq!(f.cli(&["merge", "t"]).code, 5);
    f.ok(&["deny", "a1"]);
    assert_eq!(jstr(&f.approvals()[0], "status"), "denied");
    // a denial cannot be turned into an approval afterwards
    assert_eq!(f.cli(&["approve", "a1"]).code, 6);
    // and the merge does not happen: it asks for a new approval
    let again = f.cli(&["merge", "t"]);
    assert_eq!(again.code, 5, "{again:?}");
    assert!(!integration_exists(&f));
    assert!(jstr(&f.task("r1", "t"), "merged").is_empty());
}

#[test]
fn deciding_an_unknown_approval_or_gate_is_not_found() {
    let f = one_task_fleet("exit 0", plain_cfg());
    assert_eq!(f.cli(&["approve", "a99"]).code, 4);
    assert_eq!(f.cli(&["deny", "a99"]).code, 4);
    assert_eq!(f.cli(&["gate", "resolve", "g99", "--answer", "x"]).code, 4);
    // a malformed id is invalid input, not a crash
    let (code, err) = f.json_err(&["approve", "not an id"]);
    assert!(code == 4 || code == 7, "{err:?}");
}

// ---- approved external commands ----------------------------------------------------------------------------

struct External {
    f: Fleet,
    out: TempDir,
}

fn external_fleet() -> External {
    let out = TempDir::new("o");
    let mut f = Fleet::new();
    let secret = unique_token("sentinel-secret");
    f.daemon_env
        .push(("FLEET_TEST_SENTINEL_SECRET".into(), secret));
    let ext = f.script(
        "ext.sh",
        &fill(
            "echo \"$@\" >> @OUT@/ext-args.txt\nenv > @OUT@/ext-env.txt\necho ran >> @OUT@/ext-runs.txt\necho helper-output-line\n",
            &[("OUT", out.path())],
        ),
    );
    let body = fill(
        "if grep -q -e 'The approved command was started' -e 'An operator denied' \"$FLEET_BRIEF_FILE\"; then exit 0; fi\nprintf '{\"version\":1,\"kind\":\"approval\",\"text\":\"need the helper\",\"action\":{\"command\":\"@EXT@\",\"args\":[\"one\",\"two\"]}}' > .fleet/question.json\n",
        &[("EXT", &ext)],
    );
    let s = f.script("w.sh", &body);
    f.write_cfg(&plain_cfg().worker("w", &s, &["{task}"], ""));
    f.start();
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_blocked("r1", "t", "approval");
    External { f, out }
}

#[test]
fn an_approved_external_command_runs_exactly_once_with_the_allowlisted_environment() {
    let x = external_fleet();
    let f = &x.f;
    let approvals = f.approvals();
    assert_eq!(approvals.len(), 1);
    assert_eq!(jstr(&approvals[0], "purpose"), "external-command");
    assert_eq!(jstr(&approvals[0], "status"), "pending");
    assert_eq!(
        jstr(&approvals[0], "command"),
        f.tmp.join("ext.sh").to_str().unwrap()
    );
    let args: Vec<&str> = jarr(&approvals[0], "args")
        .iter()
        .filter_map(|a| a.as_str())
        .collect();
    assert_eq!(args, ["one", "two"]);
    // nothing runs before a person approves
    std::thread::sleep(Duration::from_millis(1000));
    assert!(
        !x.out.exists("ext-runs.txt"),
        "the command ran without approval"
    );

    f.ok(&["approve", "a1"]);
    wait_file(&x.out.join("ext-runs.txt"));
    f.wait_task("r1", "t", &["completed"]);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        x.out.read("ext-runs.txt").lines().count(),
        1,
        "the command ran more than once"
    );
    assert_eq!(x.out.read("ext-args.txt").trim(), "one two");
    let env = x.out.read("ext-env.txt");
    assert_only_tolerated(&env);
    assert!(
        !env.contains("FLEET_TEST_SENTINEL_SECRET") && !env.contains("fleet.sock"),
        "{env}"
    );
    assert!(env.lines().any(|l| l == "FLEET_TASK=t"), "{env}");
    assert_eq!(jstr(&f.approvals()[0], "status"), "consumed");
    // the worker was told what happened
    let brief = fs::read_to_string(f.worktree("r1", "t").join(".fleet/brief.md")).unwrap();
    assert!(
        brief.contains("The approved command was started"),
        "{brief}"
    );
    assert!(brief.contains("Result: exit code 0"), "{brief}");
    assert!(brief.contains("helper-output-line"), "{brief}");
}

#[test]
fn a_denied_external_command_never_runs() {
    let x = external_fleet();
    let f = &x.f;
    f.ok(&["deny", "a1"]);
    f.wait_task("r1", "t", &["completed"]);
    std::thread::sleep(Duration::from_millis(1000));
    assert!(!x.out.exists("ext-runs.txt"), "a denied command ran");
    assert_eq!(jstr(&f.approvals()[0], "status"), "denied");
    let brief = fs::read_to_string(f.worktree("r1", "t").join(".fleet/brief.md")).unwrap();
    assert!(brief.contains("An operator denied"), "{brief}");
}
