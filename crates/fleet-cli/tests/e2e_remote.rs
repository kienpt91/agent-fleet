//! Remote worktrees end to end: the real `fleet` binary and daemon, a real OpenSSH server started
//! for the test, workers that run on that "remote" host. Needs `ssh`, `ssh-keygen` and sshd; without
//! them the tests are skipped (and fail when `FLEET_SSH_TESTS=1`, see `scripts/ssh-env.sh`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod common;

use common::*;
use fleet_infra::testutil::sshd::TestSshd;
use fleet_infra::testutil::{run_in_pty, wait_until};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const KEY_LINE: &str = "ssh-ed25519";

/// A worker argument as fleet.toml spells it: braces are placeholders unless doubled.
fn esc(script: &str) -> String {
    script.replace('{', "{{").replace('}', "}}")
}

struct Rig {
    f: Fleet,
    sshd: TestSshd,
    base: PathBuf,
}

/// The configuration of a fleet with one worker (`remote-sh`, running a script given as the
/// worker's arguments on the host) and a host `build` that is not pinned yet.
fn rig(worker_script: &str, worker_extra: &str, cfg_tweak: impl FnOnce(Cfg) -> Cfg) -> Option<Rig> {
    let sshd = TestSshd::start()?;
    let f = Fleet::new();
    let cfg = cfg_tweak(Cfg::new().worker(
        "remote-sh",
        Path::new("sh"),
        &["-c", &esc(worker_script)],
        worker_extra,
    ));
    f.write_cfg(&cfg);
    let base = sshd.dir.join("remote-base");
    Some(Rig { f, sshd, base })
}

impl Rig {
    fn host_add(&self) -> Out {
        self.host_add_with(&[])
    }

    fn host_add_with(&self, extra: &[&str]) -> Out {
        let port = self.sshd.port.to_string();
        let key = self.sshd.client_key.display().to_string();
        let base = self.base.display().to_string();
        let mut args = vec![
            "host",
            "add",
            "build",
            "--address",
            "127.0.0.1",
            "--port",
            &port,
            "--user",
            &self.sshd.user,
            "--identity",
            &key,
            "--base-dir",
            &base,
            "--worker",
            "sh",
            "--config",
        ];
        let cfg = self.f.config.display().to_string();
        args.push(&cfg);
        args.extend_from_slice(extra);
        self.f.cli(&args)
    }

    /// `fleet host trust build` on a terminal, typing `answer` at the prompt.
    fn trust_on_terminal(&self, answer: Option<&str>) -> (i32, String) {
        let state = self.f.state.display().to_string();
        let cfg = self.f.config.display().to_string();
        let home = self.f.home.display().to_string();
        run_in_pty(
            Path::new(FLEET_BIN),
            &[
                "--state-dir",
                &state,
                "host",
                "trust",
                "build",
                "--config",
                &cfg,
            ],
            &[("PATH", &self.f.path_env), ("HOME", &home), ("LANG", "C")],
            &answer
                .map(|a| vec![("to pin this key for host build: ", a)])
                .unwrap_or_default(),
            Duration::from_secs(60),
        )
    }

    fn trust(&self) {
        let (code, out) = self.trust_on_terminal(Some("yes\n"));
        assert_eq!(code, 0, "{out}");
    }

    fn remote_entries(&self) -> Vec<String> {
        fs::read_dir(&self.base)
            .map(|d| {
                d.flatten()
                    .flat_map(|run| {
                        fs::read_dir(run.path())
                            .map(|inner| {
                                inner
                                    .flatten()
                                    .map(|e| e.file_name().to_string_lossy().into_owned())
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

const WRITE_FILE: &str = "echo \"written on the host for $FLEET_TASK\" > remote-$FLEET_TASK.txt; \
printf '{\"version\":1,\"status\":\"done\",\"summary\":\"done remotely\"}' > .fleet/result.json";

// ---- the operator's steps ----------------------------------------------------------------------

#[test]
fn adding_listing_trusting_checking_and_removing_a_host() {
    let Some(r) = rig(WRITE_FILE, "", |c| c) else {
        return;
    };
    let added = r.host_add();
    assert_eq!(added.code, 0, "{added:?}");
    assert!(
        added.stdout.contains("fleet host trust build"),
        "{}",
        added.stdout
    );
    let text = fs::read_to_string(&r.f.config).unwrap();
    assert!(
        text.contains("[hosts.build]") && text.contains("allowed_workers = [\"sh\"]"),
        "{text}"
    );
    assert!(
        text.starts_with("[daemon]"),
        "the existing text comes first"
    );
    // The same name twice, and a bad address, are refused.
    assert_ne!(r.host_add().code, 0);
    let bad = r.f.cli(&[
        "host",
        "add",
        "other",
        "--address",
        "-oProxyCommand=x",
        "--user",
        "u",
        "--identity",
        &r.sshd.client_key.display().to_string(),
        "--base-dir",
        "/srv/f",
        "--config",
        &r.f.config.display().to_string(),
    ]);
    assert_eq!(bad.code, 7, "{bad:?}");
    assert!(!fs::read_to_string(&r.f.config).unwrap().contains("other"));

    let list = r.f.cli(&[
        "host",
        "list",
        "--config",
        &r.f.config.display().to_string(),
    ]);
    assert_eq!(list.code, 0);
    assert!(
        list.stdout.contains("build") && list.stdout.contains("not pinned"),
        "{}",
        list.stdout
    );

    // Trust needs a terminal: without one nothing is learned and nothing is pinned.
    let refused = r.f.cli(&[
        "host",
        "trust",
        "build",
        "--config",
        &r.f.config.display().to_string(),
    ]);
    assert_eq!(refused.code, 10, "{refused:?}");
    assert!(
        refused.stderr.contains("needs a terminal"),
        "{}",
        refused.stderr
    );
    assert!(!r.f.state.join("hosts").exists());

    // On a terminal: the fingerprint is shown and only `yes` pins it.
    let (code, out) = r.trust_on_terminal(Some("no\n"));
    assert_eq!(code, 10, "{out}");
    assert!(out.contains("not confirmed"), "{out}");
    assert!(!r.f.state.join("hosts/build.known_hosts").exists());
    let (code, out) = r.trust_on_terminal(Some("yes\n"));
    assert_eq!(code, 0, "{out}");
    let expected = std::process::Command::new("/usr/bin/ssh-keygen")
        .args(["-l", "-f"])
        .arg(r.sshd.dir.join("host_ed25519.pub"))
        .output()
        .unwrap();
    let fp = String::from_utf8_lossy(&expected.stdout)
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_string();
    assert!(out.contains(&fp), "the terminal showed {fp}: {out}");
    let pin = r.f.state.join("hosts/build.known_hosts");
    assert_eq!(mode_of(&pin) & 0o777, 0o600);
    let pinned = fs::read_to_string(&pin).unwrap();
    assert!(
        pinned.starts_with(&format!("127.0.0.1 {KEY_LINE} ")) || pinned.contains(KEY_LINE),
        "{pinned}"
    );
    // Same key again: nothing to confirm.
    let (code, out) = r.trust_on_terminal(None);
    assert_eq!(code, 0, "{out}");
    let _ = out;

    let list =
        r.f.cli(&[
            "host",
            "list",
            "--config",
            &r.f.config.display().to_string(),
            "--json",
        ])
        .json();
    let hosts = jarr(&list, "hosts");
    assert_eq!(hosts.len(), 1);
    assert!(jstr(&hosts[0], "pinned").contains(&fp), "{list:?}");

    let doctor = r.f.cli(&[
        "host",
        "doctor",
        "build",
        "--config",
        &r.f.config.display().to_string(),
    ]);
    assert_eq!(doctor.code, 0, "{doctor:?}");
    for want in [
        "connection",
        "quoting",
        "git",
        "base directory",
        "pinned key",
    ] {
        assert!(doctor.stdout.contains(want), "{want}: {}", doctor.stdout);
    }

    let removed = r.f.cli(&[
        "host",
        "remove",
        "build",
        "--config",
        &r.f.config.display().to_string(),
    ]);
    assert_eq!(removed.code, 0, "{removed:?}");
    assert!(!fs::read_to_string(&r.f.config)
        .unwrap()
        .contains("hosts.build"));
    assert!(!pin.exists(), "the pinned key is forgotten");
    assert_eq!(
        r.f.cli(&[
            "host",
            "remove",
            "build",
            "--config",
            &r.f.config.display().to_string()
        ])
        .code,
        4
    );
}

#[test]
fn doctor_says_what_is_wrong_with_an_unpinned_host_and_a_key_the_host_does_not_know() {
    let Some(r) = rig(WRITE_FILE, "", |c| c) else {
        return;
    };
    assert_eq!(r.host_add().code, 0);
    let cfg = r.f.config.display().to_string();
    let d = r.f.cli(&["host", "doctor", "build", "--config", &cfg]);
    assert_eq!(d.code, 13, "{d:?}");
    assert!(d.stdout.contains("fleet host trust build"), "{}", d.stdout);
    r.trust();
    // Another key file: the host refuses it.
    let text = fs::read_to_string(&r.f.config).unwrap().replace(
        &r.sshd.client_key.display().to_string(),
        &r.sshd.other_key.display().to_string(),
    );
    fs::write(&r.f.config, text).unwrap();
    let d = r.f.cli(&["host", "doctor", "build", "--config", &cfg]);
    assert_eq!(d.code, 13, "{d:?}");
    assert!(
        d.stdout.contains("refused the identity file"),
        "{}",
        d.stdout
    );
    // And the plain `fleet doctor` reports the host offline.
    let plain = r.f.cli(&["doctor", "--config", &cfg]);
    assert!(
        plain.stdout.contains("host build: pinned key"),
        "{}",
        plain.stdout
    );
}

// ---- a task on the host -------------------------------------------------------------------------

fn start_with_host(r: &mut Rig) {
    let added = r.host_add();
    assert_eq!(added.code, 0, "{added:?}");
    r.trust();
}

#[test]
fn a_remote_task_runs_on_the_host_comes_back_verified_and_merges_like_a_local_one() {
    let Some(mut r) = rig(
        WRITE_FILE,
        "timeout_secs = 90\nidle_timeout_secs = 60",
        |c| c,
    ) else {
        return;
    };
    start_with_host(&mut r);
    r.f.start();
    r.f.plan_and_start("r1", 1, &[t("a").worker("remote-sh").host("build")]);
    let task =
        r.f.wait_task("r1", "a", &["completed", "failed", "blocked"]);
    assert_eq!(
        jstr(&task, "status"),
        "completed",
        "{task:?}\n{}",
        r.f.diag()
    );
    assert_eq!(jstr(&task, "host"), "build");
    // The change was made on the host and is in the local task worktree and branch.
    let wt = r.f.worktree("r1", "a");
    assert_eq!(
        fs::read_to_string(wt.join("remote-a.txt")).unwrap(),
        "written on the host for a\n"
    );
    let head = jstr(&task, "head");
    assert_eq!(safe_git(&r.f.repo, &["rev-parse", "fleet/r1/a"]), head);
    assert_eq!(
        safe_git(&r.f.repo, &["log", "-1", "--format=%an", "fleet/r1/a"]),
        "Fleet Test",
        "the commit is fleet's own"
    );
    assert_eq!(jstr(&task, "summary"), "done remotely");
    // Nothing is left on the host or in the quarantine.
    assert_eq!(r.remote_entries(), Vec::<String>::new());
    assert_eq!(
        safe_git(&r.f.repo, &["for-each-ref", "refs/fleet-remote"]),
        ""
    );
    // The usual approval and merge: the integration branch gets the verified commit.
    let merged = merge_with_approval(&r.f, "r1/a");
    assert_eq!(merged.code, 0, "{merged:?}");
    let integration = safe_git(&r.f.repo, &["show", "fleet/r1/integration:remote-a.txt"]);
    assert_eq!(integration, "written on the host for a");
    // The attempt's stored output says what happened.
    let log = walk_files(&r.f.state.join("logs"))
        .iter()
        .map(|p| fs::read_to_string(p).unwrap_or_default())
        .collect::<String>();
    assert!(log.contains("verified and applied"), "{log}");
    r.f.shutdown();
}

#[test]
fn a_remote_worker_gets_only_the_hosts_explicit_variables_never_the_daemons_environment() {
    let secret = unique_token("remote-sentinel");
    let script = "env | sort > env-seen.txt; printf '{\"version\":1,\"status\":\"done\",\"summary\":\"env recorded\"}' > .fleet/result.json";
    let Some(mut r) = rig(
        script,
        "env_pass = [\"SENTINEL_TOKEN\"]\nenv_set = { LOCAL_ONLY = \"local-constant\" }",
        |c| c.allow_env(&["PATH", "HOME", "LANG", "TERM", "SENTINEL_TOKEN"]),
    ) else {
        return;
    };
    r.f.daemon_env
        .push(("SENTINEL_TOKEN".into(), secret.clone()));
    assert_eq!(
        r.host_add_with(&["--env", "API_BASE=https://api.invalid"])
            .code,
        0
    );
    r.trust();
    r.f.start();
    r.f.plan_and_start("r1", 1, &[t("a").worker("remote-sh").host("build")]);
    let task =
        r.f.wait_task("r1", "a", &["completed", "failed", "blocked"]);
    assert_eq!(
        jstr(&task, "status"),
        "completed",
        "{task:?}\n{}",
        r.f.diag()
    );
    let seen = fs::read_to_string(r.f.worktree("r1", "a").join("env-seen.txt")).unwrap();
    assert!(
        !seen.contains(&secret),
        "the daemon's secret reached the host:\n{seen}"
    );
    assert!(
        !seen.contains("local-constant") && !seen.contains("LOCAL_ONLY"),
        "{seen}"
    );
    assert!(seen.contains("API_BASE=https://api.invalid"), "{seen}");
    assert!(seen.contains("FLEET_TASK=a"), "{seen}");
    for leaked in ["HOME=", "SSH_", "XDG_", "LANG="] {
        assert!(!seen.contains(leaked), "{leaked}:\n{seen}");
    }
    // The same secret appears nowhere in what fleet stored.
    assert!(tree_contains(&r.f.state, &secret).is_none());
    r.f.shutdown();
}

#[test]
fn a_host_key_that_changed_fails_the_attempt_with_the_reason_and_sends_nothing() {
    let Some(mut r) = rig(WRITE_FILE, "", |c| c.set("max_retries", "0")) else {
        return;
    };
    start_with_host(&mut r);
    r.sshd.rotate_host_key();
    r.f.start();
    r.f.plan_and_start("r1", 1, &[t("a").worker("remote-sh").host("build")]);
    let task =
        r.f.wait_task("r1", "a", &["failed", "completed", "blocked"]);
    assert_eq!(jstr(&task, "status"), "failed", "{task:?}");
    let events = r.f.events_raw();
    assert!(
        events.contains("IDENTIFICATION HAS CHANGED"),
        "{}",
        r.f.diag()
    );
    assert!(events.contains("fleet host trust build"), "{events}");
    assert!(
        !r.base.exists() || r.remote_entries().is_empty(),
        "nothing was sent to the new host"
    );
    r.f.shutdown();
}

#[test]
fn an_unpinned_host_fails_the_attempt_and_the_task_says_to_trust_it() {
    let Some(mut r) = rig(WRITE_FILE, "", |c| c.set("max_retries", "0")) else {
        return;
    };
    assert_eq!(r.host_add().code, 0);
    r.f.start();
    r.f.plan_and_start("r1", 1, &[t("a").worker("remote-sh").host("build")]);
    let task =
        r.f.wait_task("r1", "a", &["failed", "completed", "blocked"]);
    assert_eq!(jstr(&task, "status"), "failed", "{task:?}");
    assert!(
        r.f.events_raw().contains("fleet host trust build"),
        "{}",
        r.f.diag()
    );
    r.f.shutdown();
}

#[test]
fn a_task_naming_a_host_that_is_not_configured_is_refused_when_it_is_added() {
    let Some(mut r) = rig(WRITE_FILE, "", |c| c) else {
        return;
    };
    r.f.start();
    let out =
        r.f.plan_only("r1", 1, &[t("a").worker("remote-sh").host("nowhere")]);
    assert_eq!(out.code, 7, "{out:?}");
    assert!(
        out.stderr.contains("is not defined in the configuration"),
        "{}",
        out.stderr
    );
    r.f.shutdown();
}

#[test]
fn a_remote_worker_that_hangs_is_stopped_by_the_idle_limit_and_dies_on_the_host() {
    let script = "echo $$ > /tmp/PIDFILE_PLACEHOLDER; trap '' TERM HUP; echo started; while :; do sleep 1; done";
    let Some(mut r) = rig(script, "idle_timeout_secs = 3\ntimeout_secs = 60", |c| {
        c.set("max_retries", "0").set("kill_grace_secs", "1")
    }) else {
        return;
    };
    // The worker records its own pid in a file next to the host's base directory.
    let pidfile = r.sshd.dir.join("worker.pid");
    let script = script.replace("/tmp/PIDFILE_PLACEHOLDER", &pidfile.display().to_string());
    let cfg = Cfg::new()
        .set("max_retries", "0")
        .set("kill_grace_secs", "1")
        .worker(
            "remote-sh",
            Path::new("sh"),
            &["-c", &esc(&script)],
            "idle_timeout_secs = 3\ntimeout_secs = 60",
        );
    r.f.write_cfg(&cfg);
    start_with_host(&mut r);
    r.f.start();
    r.f.plan_and_start("r1", 1, &[t("a").worker("remote-sh").host("build")]);
    let pid = wait_pid_file(&pidfile);
    let task =
        r.f.wait_task("r1", "a", &["failed", "completed", "blocked"]);
    assert_eq!(jstr(&task, "status"), "failed", "{task:?}\n{}", r.f.diag());
    assert!(
        r.f.events_raw().contains("idle limit") || jstr(&task, "failure").contains("silent"),
        "{}",
        r.f.diag()
    );
    assert!(
        wait_until(15_000, || !pid_alive(pid)),
        "the worker that ignores TERM and HUP is still running on the host"
    );
    assert!(
        wait_until(15_000, || r.remote_entries().is_empty()),
        "{:?}",
        r.remote_entries()
    );
    r.f.shutdown();
}

#[test]
fn a_daemon_that_died_leaves_a_remote_worker_and_worktree_that_the_next_start_cleans_up() {
    let Some(mut r) = rig(WRITE_FILE, "", |c| c) else {
        return;
    };
    let pidfile = r.sshd.dir.join("worker.pid");
    let mark = r.sshd.dir.join("second-attempt");
    // The first attempt hangs, ignoring TERM and HUP; the attempt the next daemon starts is quick.
    let script = format!(
        "if [ -e {mark} ]; then printf '{{\"version\":1,\"status\":\"done\",\"summary\":\"second\"}}' > .fleet/result.json; exit 0; fi; \
         touch {mark}; echo $$ > {pid}; trap '' TERM HUP; while :; do sleep 1; done",
        mark = mark.display(),
        pid = pidfile.display()
    );
    let cfg = Cfg::new().worker(
        "remote-sh",
        Path::new("sh"),
        &["-c", &esc(&script)],
        "timeout_secs = 120",
    );
    r.f.write_cfg(&cfg);
    start_with_host(&mut r);
    r.f.start();
    r.f.plan_and_start("r1", 1, &[t("a").worker("remote-sh").host("build")]);
    let pid = wait_pid_file(&pidfile);
    assert!(pid_alive(pid));
    assert_eq!(
        r.remote_entries().len(),
        2,
        "worktree and pid file: {:?}",
        r.remote_entries()
    );
    r.f.kill_daemon();
    // Closing the terminal does not stop a worker that ignores the hang-up.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        pid_alive(pid),
        "the test needs the leftover worker to survive the daemon"
    );
    r.f.start();
    assert!(
        wait_until(30_000, || !pid_alive(pid)),
        "the leftover worker was not stopped"
    );
    let task =
        r.f.wait_task("r1", "a", &["completed", "failed", "blocked"]);
    assert_eq!(
        jstr(&task, "status"),
        "completed",
        "{task:?}\n{}",
        r.f.diag()
    );
    assert!(
        wait_until(30_000, || r.remote_entries().is_empty()),
        "{:?}",
        r.remote_entries()
    );
    let events = r.f.events_raw();
    assert!(
        events.contains("leftover"),
        "the clean-up is reported:\n{events}"
    );
    r.f.shutdown();
}

#[test]
fn a_flood_of_output_from_the_host_is_bounded_like_local_output() {
    let script = "head -c 60000000 /dev/zero | tr '\\0' 'y'; echo; echo flood-over; \
                  printf '{\"version\":1,\"status\":\"done\",\"summary\":\"flooded\"}' > .fleet/result.json";
    let Some(mut r) = rig(script, "timeout_secs = 120\nidle_timeout_secs = 60", |c| {
        c.set("ring_bytes", "65536")
    }) else {
        return;
    };
    start_with_host(&mut r);
    r.f.start();
    r.f.plan_and_start("r1", 1, &[t("a").worker("remote-sh").host("build")]);
    let task =
        r.f.wait_task("r1", "a", &["completed", "failed", "blocked"]);
    assert_eq!(
        jstr(&task, "status"),
        "completed",
        "{task:?}\n{}",
        r.f.diag()
    );
    let logs: u64 = walk_files(&r.f.state.join("logs"))
        .iter()
        .map(|p| p.metadata().unwrap().len())
        .sum();
    assert!(
        logs < 2 * 1024 * 1024,
        "the stored output is bounded, {logs} bytes"
    );
    let events = fs::metadata(r.f.events_log_path()).unwrap().len();
    assert!(
        events < 4 * 1024 * 1024,
        "the event log is bounded, {events} bytes"
    );
    r.f.shutdown();
}

#[test]
fn quality_gates_run_locally_on_the_verified_commit_never_on_the_host() {
    let sshd_marker = "gate-ran-here";
    let Some(mut r) = rig(WRITE_FILE, "timeout_secs = 90", |c| c) else {
        return;
    };
    let gate = r.f.script(
        "gate.sh",
        &format!(
            "#!/bin/sh\ntest -f remote-a.txt && pwd > {}\n",
            r.f.tmp.join(sshd_marker).display()
        ),
    );
    let cfg = Cfg::new()
        .worker(
            "remote-sh",
            Path::new("sh"),
            &["-c", &esc(WRITE_FILE)],
            "timeout_secs = 90",
        )
        .gate("local-check", &gate, &[], "timeout_secs = 30");
    r.f.write_cfg(&cfg);
    start_with_host(&mut r);
    r.f.start();
    r.f.plan_and_start("r1", 1, &[t("a").worker("remote-sh").host("build")]);
    let task =
        r.f.wait_task("r1", "a", &["completed", "failed", "blocked"]);
    assert_eq!(
        jstr(&task, "status"),
        "completed",
        "{task:?}\n{}",
        r.f.diag()
    );
    let ran_in = fs::read_to_string(r.f.tmp.join(sshd_marker)).unwrap();
    assert_eq!(
        PathBuf::from(ran_in.trim()),
        r.f.worktree("r1", "a"),
        "the gate ran in the local worktree"
    );
    r.f.shutdown();
}
