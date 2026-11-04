//! Remote worktrees against a real OpenSSH server started for the test (see
//! `fleet_infra::testutil::sshd`): host key pinning, authentication, the quoting of remote commands
//! through a real login shell, forwarding and environment, timeouts, kill escalation, and the
//! verification of what a remote host returns. Run with `scripts/ssh-env.sh` in the `fleet-ssh`
//! image; without sshd the tests are skipped (and fail when `FLEET_SSH_TESTS=1`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_app::hosts::HostDef;
use fleet_app::ports::{ContractFiles, Identity, Signal, Vcs};
use fleet_app::remote::{
    CollectRequest, Collected, LaunchRequest, PrepareRequest, RemoteHost, RemoteTask,
};
use fleet_domain::contract::{ContractRead, ResultStatus};
use fleet_domain::env::EnvPolicy;
use fleet_domain::error::Result;
use fleet_domain::ids::{task_branch, HostName, RunId, Sha, TaskId};
use fleet_domain::redact::Redactor;
use fleet_domain::remote::{worker_env, RemoteCall, RemoteLayout, DEFAULT_REMOTE_PATH};
use fleet_infra::contract_fs::FsContractFiles;
use fleet_infra::git::GitVcs;
use fleet_infra::jobs::JobTracker;
use fleet_infra::ssh::client::{SshClient, Timing};
use fleet_infra::ssh::exec::{self, Done, Ending, Input, Limits, Output, Spec};
use fleet_infra::ssh::host::SshRemoteHost;
use fleet_infra::ssh::trust;
use fleet_infra::testutil::sshd::TestSshd;
use fleet_infra::testutil::{git, make_repo, wait_until, TempDir};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const SSH: &str = "/usr/bin/ssh";

struct Fx {
    sshd: TestSshd,
    state: TempDir,
    _repo_dir: TempDir,
    repo: String,
    tracker: JobTracker,
}

fn rid(s: &str) -> RunId {
    RunId::parse(s).unwrap()
}
fn tid(s: &str) -> TaskId {
    TaskId::parse(s).unwrap()
}
fn rt(host: &str, run: &str, task: &str) -> RemoteTask {
    RemoteTask {
        host: HostName::parse(host).unwrap(),
        run: rid(run),
        task: tid(task),
    }
}

fn author() -> Identity {
    Identity {
        name: "fleet".into(),
        email: "fleet@localhost".into(),
    }
}

fn short(total_s: u64, stall_s: u64) -> Timing {
    Timing {
        total: Duration::from_secs(total_s),
        stall: Duration::from_secs(stall_s),
    }
}

impl Fx {
    fn new() -> Option<Fx> {
        let sshd = TestSshd::start()?;
        let state = TempDir::new("ssh-state");
        let repo_dir = TempDir::new("ssh-repo");
        let repo = make_repo(&repo_dir).display().to_string();
        Some(Fx {
            sshd,
            state,
            _repo_dir: repo_dir,
            repo,
            tracker: JobTracker::new(),
        })
    }

    fn vcs(&self) -> GitVcs {
        let path = std::env::var("PATH").unwrap_or_default();
        GitVcs::new(vec![("PATH".into(), path)], EnvPolicy::default_policy())
            .with_tracker(self.tracker.clone())
    }

    fn host(&self, workers: &[&str]) -> HostDef {
        self.sshd.host_def("build", workers)
    }

    fn client(&self, hosts: Vec<HostDef>) -> SshClient {
        let map: BTreeMap<HostName, HostDef> =
            hosts.into_iter().map(|h| (h.name.clone(), h)).collect();
        let mut c = SshClient::new(
            PathBuf::from(SSH),
            map,
            self.state.path().to_path_buf(),
            self.tracker.clone(),
        );
        c.timeouts.quick = short(30, 20);
        c.timeouts.prepare = short(60, 30);
        c.timeouts.collect = short(60, 30);
        c.timeouts.cleanup = short(30, 15);
        c
    }

    fn remote(&self, hosts: Vec<HostDef>) -> SshRemoteHost {
        SshRemoteHost::new(self.client(hosts), self.vcs(), author())
    }

    fn trust(&self, remote: &SshRemoteHost, host: &HostDef) {
        let scanned = trust::scan(remote.client(), host, Duration::from_secs(20)).unwrap();
        trust::pin(remote.client(), host, &scanned).unwrap();
    }

    fn worktree(&self, run: &str, task: &str) -> String {
        let vcs = self.vcs();
        let base = vcs.resolve_base(&self.repo, "HEAD").unwrap();
        let wt = vcs
            .prepare_worktree(
                &self.repo,
                &rid(run),
                &tid(task),
                &task_branch(run, task),
                &base.sha,
            )
            .unwrap();
        FsContractFiles::new(Arc::new(Redactor::builtin()))
            .prepare(&wt, "# the brief\nplease do the thing\n")
            .unwrap();
        wt
    }

    fn remote_dir(&self, host: &HostDef, run: &str, task: &str) -> PathBuf {
        PathBuf::from(
            RemoteLayout::new(&host.base_dir)
                .unwrap()
                .worktree(&rid(run), &tid(task)),
        )
    }
}

/// Runs the worker of `task` on the host through the command the adapter built, in a terminal
/// that is not a real one (`-tt` forces a remote pty).
fn run_worker(
    remote: &SshRemoteHost,
    host: &HostDef,
    task: &RemoteTask,
    program: &str,
    args: &[&str],
    extra_env: &[(&str, &str)],
    total_s: u64,
) -> Done {
    let dir = remote.remote_dir(task).unwrap();
    let mut env = worker_env(&host.env, &task.run, &task.task, &dir);
    for (k, v) in extra_env {
        env.push((k.to_string(), v.to_string()));
    }
    let cmd = remote
        .launch_command(&LaunchRequest {
            task: task.clone(),
            program: program.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            env,
        })
        .unwrap();
    exec::run(
        &Spec {
            program: PathBuf::from(cmd.program),
            args: cmd.args,
            env: cmd.env,
            input: Input::None,
            output: Output::Capture {
                max: 4 * 1024 * 1024,
            },
            limits: Limits {
                total: Duration::from_secs(total_s),
                stall: Duration::from_secs(total_s),
            },
        },
        &JobTracker::new(),
    )
}

fn collect(remote: &SshRemoteHost, task: &RemoteTask, wt: &str, sent: &Sha) -> Result<Collected> {
    let id = remote
        .begin_collect(&CollectRequest {
            task: task.clone(),
            worktree: wt.to_string(),
            sent: sent.clone(),
            attempt: 1,
        })
        .unwrap();
    let mut answer = None;
    assert!(
        wait_until(60_000, || {
            answer = remote.poll_collect(id);
            answer.is_some()
        }),
        "collect did not finish"
    );
    answer.unwrap()
}

fn prepare(
    remote: &SshRemoteHost,
    task: &RemoteTask,
    wt: &str,
) -> Result<fleet_app::remote::Prepared> {
    remote.prepare(&PrepareRequest {
        task: task.clone(),
        attempt: 1,
        worktree: wt.to_string(),
    })
}

fn connections(fx: &Fx) -> usize {
    fx.sshd.log().matches("Connection from").count()
}

fn output_text(done: &Done) -> String {
    String::from_utf8_lossy(&done.stdout).replace('\r', "")
}

/// A worker that ignores TERM, HUP and INT and creates the file named by `$1` once those traps are
/// in place (`sh -c STUBBORN_WORKER sh <file>`).
const STUBBORN_WORKER: &str =
    "trap '' TERM HUP INT; : > \"$1\"; echo up; while :; do sleep 1; done";

/// Waits until the stubborn worker has installed its traps (it created `marker`). The launcher
/// writes the pid file before it hands over to the worker, so that file says nothing about the traps:
/// a TERM sent as soon as it appears can still kill a worker that is about to ignore it. The wait is
/// generous and only ever as long as the machine is slow.
fn wait_for_traps(marker: &std::path::Path) {
    assert!(
        wait_until(60_000, || marker.exists()),
        "the worker did not report within 60 s that it ignores TERM and HUP: {} was not created",
        marker.display()
    );
}

// ---- host key pinning ---------------------------------------------------------------------------

#[test]
fn first_use_without_a_pin_is_refused_before_any_connection() {
    let Some(fx) = Fx::new() else { return };
    let host = fx.host(&["sh"]);
    let remote = fx.remote(vec![host.clone()]);
    let wt = fx.worktree("r1", "t1");
    let before = connections(&fx);
    let err = prepare(&remote, &rt("build", "r1", "t1"), &wt).unwrap_err();
    assert!(
        err.message.contains("fleet host trust build"),
        "{}",
        err.message
    );
    assert_eq!(connections(&fx), before, "no connection was made");
    assert!(!fx.remote_dir(&host, "r1", "t1").exists());
    // The launch command cannot be built either.
    let launch = remote.launch_command(&LaunchRequest {
        task: rt("build", "r1", "t1"),
        program: "sh".into(),
        args: vec![],
        env: vec![],
    });
    assert!(launch.is_err());
}

#[test]
fn the_scan_shows_the_servers_key_and_pinning_is_a_separate_private_step() {
    let Some(fx) = Fx::new() else { return };
    let host = fx.host(&["sh"]);
    let remote = fx.remote(vec![host.clone()]);
    let scanned = trust::scan(remote.client(), &host, Duration::from_secs(20)).unwrap();
    assert_eq!(
        format!("{} {}", scanned.key.key_type, scanned.key.base64),
        fx.sshd.host_key()
    );
    // The fingerprint is what `ssh-keygen -l` prints for the server's public key.
    let out = std::process::Command::new("/usr/bin/ssh-keygen")
        .args(["-l", "-f"])
        .arg(fx.sshd.dir.join("host_ed25519.pub"))
        .output()
        .unwrap();
    let printed = String::from_utf8_lossy(&out.stdout);
    assert!(
        printed.contains(&scanned.key.fingerprint()),
        "{printed} vs {}",
        scanned.key.fingerprint()
    );
    // Scanning pins nothing.
    assert!(remote.client().pinned(&host).unwrap().is_none());
    assert!(!remote.client().pin_file(&host.name).exists());
    trust::pin(remote.client(), &host, &scanned).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let pin = remote.client().pin_file(&host.name);
    assert_eq!(
        std::fs::metadata(&pin).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(remote.client().pinned(&host).unwrap().unwrap(), scanned.key);
    // No scratch files stay behind.
    assert!(std::fs::read_dir(fx.state.path().join("remote"))
        .map(|d| d
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().starts_with("scan-")))
        .unwrap_or(true));
}

#[test]
fn a_changed_host_key_is_refused_with_a_clear_error_and_nothing_is_sent() {
    let Some(mut fx) = Fx::new() else { return };
    let host = fx.host(&["sh"]);
    let remote = fx.remote(vec![host.clone()]);
    fx.trust(&remote, &host);
    let wt = fx.worktree("r1", "t1");
    fx.sshd.rotate_host_key();
    let err = prepare(&remote, &rt("build", "r1", "t1"), &wt).unwrap_err();
    assert!(
        err.message.contains("IDENTIFICATION HAS CHANGED"),
        "{}",
        err.message
    );
    assert!(
        err.message.contains("fleet host trust build"),
        "{}",
        err.message
    );
    assert!(
        !fx.remote_dir(&host, "r1", "t1").exists(),
        "nothing was sent"
    );
    // A scan reports the new key; only an explicit trust replaces the pin.
    let new = trust::scan(remote.client(), &host, Duration::from_secs(20)).unwrap();
    assert_ne!(
        Some(new.key.clone()),
        remote.client().pinned(&host).unwrap()
    );
    assert!(
        prepare(&remote, &rt("build", "r1", "t1"), &wt).is_err(),
        "still refused after the scan"
    );
    trust::pin(remote.client(), &host, &new).unwrap();
    let sent = prepare(&remote, &rt("build", "r1", "t1"), &wt).unwrap();
    assert!(sent.dir.ends_with("/r1/t1"));
}

#[test]
fn a_key_the_server_does_not_accept_is_refused() {
    let Some(fx) = Fx::new() else { return };
    let mut host = fx.host(&["sh"]);
    host.identity_file = fx.sshd.other_key.display().to_string();
    let remote = fx.remote(vec![host.clone()]);
    fx.trust(&remote, &host);
    let wt = fx.worktree("r1", "t1");
    let err = prepare(&remote, &rt("build", "r1", "t1"), &wt).unwrap_err();
    assert!(
        err.message.contains("refused the identity file"),
        "{}",
        err.message
    );
}

#[test]
fn an_unreadable_identity_file_is_reported_as_such() {
    let Some(fx) = Fx::new() else { return };
    let mut host = fx.host(&["sh"]);
    host.identity_file = fx.sshd.dir.join("missing_key").display().to_string();
    let remote = fx.remote(vec![host.clone()]);
    fx.trust(&remote, &host);
    let wt = fx.worktree("r1", "t1");
    let err = prepare(&remote, &rt("build", "r1", "t1"), &wt).unwrap_err();
    assert!(
        err.message.contains("refused") || err.message.contains("identity"),
        "{}",
        err.message
    );
}

// ---- the protocol -------------------------------------------------------------------------------

fn prepared_task(
    fx: &Fx,
    workers: &[&str],
) -> (
    SshRemoteHost,
    HostDef,
    RemoteTask,
    String,
    fleet_app::remote::Prepared,
) {
    let host = fx.host(workers);
    let remote = fx.remote(vec![host.clone()]);
    fx.trust(&remote, &host);
    let wt = fx.worktree("r1", "t1");
    let task = rt("build", "r1", "t1");
    let prepared = prepare(&remote, &task, &wt).unwrap();
    (remote, host, task, wt, prepared)
}

#[test]
fn the_remote_worktree_holds_the_commit_the_brief_and_no_history() {
    let Some(fx) = Fx::new() else { return };
    let (_remote, host, _task, _wt, prepared) = prepared_task(&fx, &["sh"]);
    let dir = fx.remote_dir(&host, "r1", "t1");
    assert_eq!(prepared.dir, dir.display().to_string());
    assert_eq!(
        std::fs::read_to_string(dir.join("README.md")).unwrap(),
        "# fixture\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("src/lib.txt")).unwrap(),
        "one\ntwo\nthree\n"
    );
    assert!(std::fs::read_to_string(dir.join(".fleet/brief.md"))
        .unwrap()
        .contains("please do the thing"));
    let log = git(&dir, &["log", "--format=%H"]);
    assert_eq!(log.lines().count(), 1, "one root commit, no history: {log}");
    assert_eq!(log.trim(), prepared.sent.as_str());
    assert_eq!(
        git(&dir, &["status", "--porcelain"]),
        "",
        ".fleet is excluded from the remote repository"
    );
    // The pin and the work files are private.
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o077,
        0
    );
}

#[test]
fn a_task_preparing_twice_replaces_the_stale_remote_worktree() {
    let Some(fx) = Fx::new() else { return };
    let (remote, _host, task, wt, first) = prepared_task(&fx, &["sh"]);
    let second = prepare(&remote, &task, &wt).unwrap();
    assert_eq!(first.dir, second.dir);
}

/// The clean-up of an abandoned attempt runs in the background and ends with its local part (the
/// quarantine refs, the list of remote worktrees). The next attempt of the same task may be prepared
/// before it ends: it must not lose what it made to the clean-up of the attempt before it. A shim for
/// `rm` holds the host's removal command open after the directory is gone, which is what a slow
/// host or a busy machine does, so that the order does not depend on timing.
#[test]
fn the_cleanup_of_an_abandoned_attempt_leaves_the_next_attempt_of_the_task_alone() {
    let Some(fx) = Fx::new() else { return };
    let shims = fx.sshd.dir.join("shims");
    std::fs::create_dir_all(&shims).unwrap();
    let gate = fx.sshd.dir.join("rm-gate");
    let held = fx.sshd.dir.join("rm-held");
    let shim = shims.join("rm");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\n/bin/rm \"$@\"; rc=$?\n\
             if [ \"$1\" = -rf ] && [ -e '{gate}' ]; then : > '{held}'; \
             while [ -e '{gate}' ]; do sleep 0.02; done; fi\nexit $rc\n",
            gate = gate.display(),
            held = held.display(),
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut host = fx.host(&["sh"]);
    host.env = vec![(
        "PATH".to_string(),
        format!("{}:{DEFAULT_REMOTE_PATH}", shims.display()),
    )];
    let remote = fx.remote(vec![host.clone()]);
    fx.trust(&remote, &host);
    let wt = fx.worktree("r1", "t1");
    let task = rt("build", "r1", "t1");
    let repo = std::path::Path::new(&fx.repo);
    let refs = || {
        git(
            repo,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/fleet-remote",
            ],
        )
    };
    let markers = || {
        std::fs::read_dir(fx.state.path().join("remote/live"))
            .map(|d| d.count())
            .unwrap_or(0)
    };

    prepare(&remote, &task, &wt).unwrap();
    // Abandon the attempt; its clean-up removes the remote directory and then stays in the removal
    // command until the gate is opened.
    std::fs::write(&gate, "").unwrap();
    remote.abandon(&task);
    assert!(
        wait_until(60_000, || held.exists()),
        "the clean-up did not reach the host within 60 s"
    );
    assert!(!fx.remote_dir(&host, "r1", "t1").exists());

    // The next attempt of the task is prepared while that clean-up is still running.
    let second = prepare(&remote, &task, &wt).unwrap();
    let sent_ref = format!("refs/fleet-remote/r1/t1/sent {}", second.sent.as_str());
    assert!(refs().contains(&sent_ref), "{}", refs());
    assert_eq!(markers(), 1);

    // The clean-up of the first attempt ends now.
    std::fs::remove_file(&gate).unwrap();
    assert!(
        wait_until(60_000, || remote.running_jobs() == 0),
        "the clean-up did not end within 60 s"
    );
    assert!(
        refs().contains(&sent_ref),
        "the clean-up of the first attempt removed the quarantine ref of the second: {:?}",
        refs()
    );
    assert_eq!(
        markers(),
        1,
        "the clean-up of the first attempt removed the second attempt's entry in the list of remote worktrees"
    );
    assert!(fx.remote_dir(&host, "r1", "t1").exists());

    // The second attempt ends the normal way and leaves nothing behind.
    let got = collect(&remote, &task, &wt, &second.sent).unwrap();
    assert!(
        got.notes.iter().any(|n| n.contains("was removed")),
        "{:?}",
        got.notes
    );
    assert_eq!(refs(), "");
    assert_eq!(markers(), 0);
    assert!(!fx.remote_dir(&host, "r1", "t1").exists());
}

#[test]
fn work_and_contract_files_come_back_verified_and_the_remote_is_cleaned() {
    let Some(fx) = Fx::new() else { return };
    let (remote, host, task, wt, prepared) = prepared_task(&fx, &["sh"]);
    let before = git(std::path::Path::new(&wt), &["rev-parse", "HEAD"]);
    let script = "echo changed >> README.md; echo new > added.txt; mkdir -p sub; echo deep > sub/x; \
                  printf '{\"version\":1,\"status\":\"done\",\"summary\":\"all good\"}' > .fleet/result.json";
    let done = run_worker(&remote, &host, &task, "sh", &["-c", script], &[], 60);
    assert!(done.success(), "{:?} {}", done.ending, output_text(&done));
    let got = collect(&remote, &task, &wt, &prepared.sent).unwrap();
    let head = got.head.expect("the tree changed");
    // The task branch has one new local commit on top of the old head, authored by fleet.
    assert_eq!(
        git(std::path::Path::new(&wt), &["rev-parse", "HEAD"]),
        head.as_str()
    );
    assert_eq!(
        git(std::path::Path::new(&wt), &["rev-parse", "HEAD~1"]),
        before
    );
    assert_eq!(
        git(std::path::Path::new(&wt), &["log", "-1", "--format=%an"]),
        "fleet"
    );
    assert_eq!(
        std::fs::read_to_string(format!("{wt}/README.md")).unwrap(),
        "# fixture\nchanged\n"
    );
    assert_eq!(
        std::fs::read_to_string(format!("{wt}/added.txt")).unwrap(),
        "new\n"
    );
    assert_eq!(
        std::fs::read_to_string(format!("{wt}/sub/x")).unwrap(),
        "deep\n"
    );
    assert_eq!(
        git(std::path::Path::new(&wt), &["status", "--porcelain"]),
        ""
    );
    // The contract file is where the orchestrator reads it.
    let files = FsContractFiles::new(Arc::new(Redactor::builtin()));
    match files.read_result(&wt) {
        ContractRead::Valid(r) => {
            assert_eq!(r.status, ResultStatus::Done);
            assert_eq!(r.summary, "all good");
        }
        other => panic!("{other:?}"),
    }
    // Remote and local leftovers are gone.
    assert!(!fx.remote_dir(&host, "r1", "t1").exists());
    assert!(!fx
        .remote_dir(&host, "r1", "t1")
        .with_extension("pid")
        .exists());
    assert!(
        got.notes.iter().any(|n| n.contains("verified and applied")),
        "{:?}",
        got.notes
    );
    assert!(
        got.notes.iter().any(|n| n.contains("was removed")),
        "{:?}",
        got.notes
    );
    let repo = std::path::Path::new(&fx.repo);
    assert_eq!(
        git(repo, &["for-each-ref", "refs/fleet-remote"]),
        "",
        "quarantine refs are dropped"
    );
    let work = fx.state.path().join("remote/work");
    assert_eq!(std::fs::read_dir(&work).map(|d| d.count()).unwrap_or(0), 0);
    assert_eq!(
        std::fs::read_dir(fx.state.path().join("remote/live"))
            .map(|d| d.count())
            .unwrap_or(0),
        0
    );
}

#[test]
fn a_worker_that_changed_nothing_returns_no_head() {
    let Some(fx) = Fx::new() else { return };
    let (remote, host, task, wt, prepared) = prepared_task(&fx, &["sh"]);
    let done = run_worker(&remote, &host, &task, "sh", &["-c", "true"], &[], 60);
    assert!(done.success());
    let got = collect(&remote, &task, &wt, &prepared.sent).unwrap();
    assert_eq!(got.head, None);
    assert!(!fx.remote_dir(&host, "r1", "t1").exists());
}

#[test]
fn a_question_file_comes_back_too() {
    let Some(fx) = Fx::new() else { return };
    let (remote, host, task, wt, prepared) = prepared_task(&fx, &["sh"]);
    let script = "printf '{\"version\":1,\"kind\":\"question\",\"text\":\"Which database?\"}' > .fleet/question.json";
    run_worker(&remote, &host, &task, "sh", &["-c", script], &[], 60);
    collect(&remote, &task, &wt, &prepared.sent).unwrap();
    let files = FsContractFiles::new(Arc::new(Redactor::builtin()));
    assert!(matches!(files.read_question(&wt), ContractRead::Valid(_)));
}

// ---- quoting through a real login shell -----------------------------------------------------------

#[test]
fn hostile_arguments_reach_the_remote_program_unchanged() {
    let Some(fx) = Fx::new() else { return };
    let host = fx.host(&["sh"]);
    let remote = fx.remote(vec![host.clone()]);
    fx.trust(&remote, &host);
    let nasty: Vec<String> = [
        "plain",
        "with space",
        "it's",
        "\"double\"",
        "$(touch /tmp/fleet-pwned)",
        "`touch /tmp/fleet-pwned`",
        "a;b&c|d",
        "*",
        "~",
        "a=b",
        "\\",
        "\\'",
        "line1\nline2",
        "tab\there",
        "-n",
        "--",
        "",
        "é日本",
        "${HOME}",
        "'; id; '",
        "\n; rm -rf /\n",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let call = RemoteCall::RoundTrip { args: &nasty };
    let done = remote
        .client()
        .run(
            &host,
            &call,
            Input::None,
            Output::Capture { max: 1 << 20 },
            short(30, 20),
        )
        .unwrap();
    assert!(
        done.success(),
        "{:?} {}",
        done.ending,
        String::from_utf8_lossy(&done.stderr)
    );
    let mut expected: Vec<u8> = Vec::new();
    for a in &nasty {
        expected.extend_from_slice(a.as_bytes());
        expected.push(0);
    }
    expected.extend_from_slice(format!("Linux\n{}\n", nix::unistd::geteuid().as_raw()).as_bytes());
    assert_eq!(done.stdout, expected);
    assert!(!std::path::Path::new("/tmp/fleet-pwned").exists());
}

#[test]
fn random_hostile_arguments_survive_the_real_shell() {
    let Some(fx) = Fx::new() else { return };
    let host = fx.host(&["sh"]);
    let remote = fx.remote(vec![host.clone()]);
    fx.trust(&remote, &host);
    const PIECES: &[&str] = &[
        "'", "\"", "\\", "$", "`", ";", "&", "|", "<", ">", "(", ")", "{", "}", "*", "?", "!", "#",
        "~", " ", "\n", "\t", "=", "%", "é", "a", "b", "-", "/", "$(id)", "`id`", "${IFS}", "''",
        "\\'", "\r",
    ];
    let mut seed = 0x1234_5678_9abc_def0u64;
    let mut next = move |n: usize| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((seed >> 33) as usize) % n
    };
    for round in 0..12 {
        let args: Vec<String> = (0..8)
            .map(|_| {
                (0..next(10))
                    .map(|_| PIECES[next(PIECES.len())])
                    .collect::<String>()
            })
            .collect();
        let call = RemoteCall::RoundTrip { args: &args };
        let done = remote
            .client()
            .run(
                &host,
                &call,
                Input::None,
                Output::Capture { max: 1 << 20 },
                short(30, 20),
            )
            .unwrap();
        assert!(done.success(), "round {round}: {:?}", done.ending);
        let parts: Vec<&[u8]> = done.stdout.split(|b| *b == 0).collect();
        for (i, a) in args.iter().enumerate() {
            assert_eq!(parts[i], a.as_bytes(), "round {round} argument {i}");
        }
        assert_eq!(
            parts[args.len()],
            format!("Linux\n{}\n", nix::unistd::geteuid().as_raw()).as_bytes()
        );
    }
}

// ---- forwarding and environment -----------------------------------------------------------------------

#[test]
fn forwarding_agent_display_and_environment_do_not_reach_the_remote() {
    let Some(fx) = Fx::new() else { return };
    let host = fx.host(&["sh", "env"]);
    let remote = fx.remote(vec![host.clone()]);
    fx.trust(&remote, &host);
    // A live agent and a display on the local side; the server would accept all of it.
    let agent_sock = fx.state.path().join("agent.sock");
    let agent = std::process::Command::new("/usr/bin/ssh-agent")
        .args(["-D", "-a"])
        .arg(&agent_sock)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let mut agent = agent.ok();
    assert!(
        wait_until(5_000, || agent_sock.exists()),
        "the test agent did not start"
    );
    let task = rt("build", "r1", "t1");
    let wt = fx.worktree("r1", "t1");
    prepare(&remote, &task, &wt).unwrap();
    let cmd = remote
        .launch_command(&LaunchRequest {
            task: task.clone(),
            program: "env".to_string(),
            args: vec![],
            env: vec![("ONLY".to_string(), "explicit".to_string())],
        })
        .unwrap();
    let (mut args, mut env) = (cmd.args, cmd.env);
    // Hostile local environment (what a user's shell would have): agent, display, locale, secrets.
    env.extend([
        (
            "SSH_AUTH_SOCK".to_string(),
            agent_sock.display().to_string(),
        ),
        ("DISPLAY".to_string(), ":99".to_string()),
        ("LANG".to_string(), "de_DE.UTF-8".to_string()),
        ("LC_TIME".to_string(), "de_DE.UTF-8".to_string()),
        (
            "SENTINEL_SECRET".to_string(),
            "hunter2-sentinel".to_string(),
        ),
    ]);
    // A forwarding request on the command line is overridden by ClearAllForwardings.
    let port = fleet_infra::testutil::sshd::free_port();
    let at = args.iter().position(|a| a == "-p").unwrap();
    args.splice(at..at, ["-R".to_string(), format!("{port}:127.0.0.1:1")]);
    let done = exec::run(
        &Spec {
            program: PathBuf::from(SSH),
            args,
            env,
            input: Input::None,
            output: Output::Capture { max: 1 << 20 },
            limits: Limits {
                total: Duration::from_secs(60),
                stall: Duration::from_secs(60),
            },
        },
        &JobTracker::new(),
    );
    let text = output_text(&done);
    assert!(
        done.success(),
        "{:?} {text} {}",
        done.ending,
        String::from_utf8_lossy(&done.stderr)
    );
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    assert!(lines.contains(&"ONLY=explicit"), "{lines:?}");
    for leaked in [
        "SSH_AUTH_SOCK",
        "DISPLAY",
        "LANG=",
        "LC_TIME",
        "SENTINEL_SECRET",
        "hunter2",
        "SSH_CONNECTION",
        "SSH_CLIENT",
        "XAUTHORITY",
    ] {
        assert!(
            !text.contains(leaked),
            "{leaked} reached the remote worker: {text}"
        );
    }
    // The reverse forward was not set up: nothing listens on the port the server would have opened.
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
        "a forwarded port is open"
    );
    let log = fx.sshd.log();
    assert!(
        !log.contains("agent-forward") && !log.contains("auth-agent"),
        "{log}"
    );
    assert!(!log.contains("x11-req"), "{log}");
    if let Some(a) = agent.as_mut() {
        let _ = a.kill();
        let _ = a.wait();
    }
}

#[test]
fn the_remote_worker_sees_only_explicit_variables_and_no_local_secret() {
    let Some(fx) = Fx::new() else { return };
    let mut host = fx.host(&["sh"]);
    host.env = vec![("API_BASE".to_string(), "https://api.invalid".to_string())];
    let remote = fx.remote(vec![host.clone()]);
    fx.trust(&remote, &host);
    let wt = fx.worktree("r1", "t1");
    let task = rt("build", "r1", "t1");
    prepare(&remote, &task, &wt).unwrap();
    std::env::set_var("SENTINEL_LOCAL_SECRET", "sentinel-9f3a");
    let done = run_worker(&remote, &host, &task, "sh", &["-c", "env | sort"], &[], 60);
    let text = output_text(&done);
    assert!(done.success(), "{text}");
    let mut vars: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| l.contains('='))
        .collect();
    vars.retain(|l| {
        !l.starts_with("PWD=")
            && !l.starts_with("SHLVL=")
            && !l.starts_with("_=")
            && !l.starts_with("OLDPWD=")
    });
    let worktree = format!(
        "FLEET_WORKTREE={}",
        fx.remote_dir(&host, "r1", "t1").display()
    );
    assert_eq!(
        vars,
        [
            "API_BASE=https://api.invalid",
            "FLEET_RUN=r1",
            "FLEET_TASK=t1",
            worktree.as_str(),
            "PATH=/usr/local/bin:/usr/bin:/bin",
            "TERM=xterm-256color",
        ],
        "{text}"
    );
    assert!(!text.contains("sentinel-9f3a"));
}

// ---- a worker that misbehaves --------------------------------------------------------------------------

#[test]
fn a_hostile_remote_that_returns_unrelated_history_or_forbidden_paths_is_refused() {
    let Some(fx) = Fx::new() else { return };
    for (label, script, expect) in [
        (
            "unrelated history",
            "git checkout -q --orphan other && git rm -rfq . && echo evil > evil.txt && git add -A && git commit -qm evil",
            "does not build on",
        ),
        (
            "a committed .fleet path",
            "mkdir -p .fleet && echo x > .fleet/evil && git add -f .fleet/evil && git commit -qm x",
            "touches",
        ),
        (
            "history rewritten below the sent commit",
            "git update-ref -d refs/heads/work; git checkout -q --orphan w2 && git add -A && git commit -qm again",
            "does not build on",
        ),
    ] {
        let (remote, host, task, wt, prepared) = prepared_task(&fx, &["sh"]);
        // The remote worker is `sh`; the git commands run in its worktree under a plain environment.
        let env = [
            ("GIT_AUTHOR_NAME", "x"), ("GIT_AUTHOR_EMAIL", "x@x"), ("GIT_COMMITTER_NAME", "x"),
            ("GIT_COMMITTER_EMAIL", "x@x"), ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ];
        let done = run_worker(&remote, &host, &task, "sh", &["-c", script], &env, 60);
        assert!(done.success(), "{label}: {} {:?}", output_text(&done), String::from_utf8_lossy(&done.stderr));
        let old = git(std::path::Path::new(&wt), &["rev-parse", "HEAD"]);
        let err = collect(&remote, &task, &wt, &prepared.sent).unwrap_err();
        assert!(err.message.contains(expect), "{label}: {}", err.message);
        assert_eq!(git(std::path::Path::new(&wt), &["rev-parse", "HEAD"]), old, "{label}: branch untouched");
        assert_eq!(git(std::path::Path::new(&wt), &["status", "--porcelain"]), "", "{label}");
        assert!(!std::path::Path::new(&wt).join("evil.txt").exists(), "{label}");
        assert_eq!(git(std::path::Path::new(&fx.repo), &["for-each-ref", "refs/fleet-remote"]), "", "{label}");
        assert!(!fx.remote_dir(&host, "r1", "t1").exists(), "{label}: the remote is cleaned up all the same");
    }
}

#[test]
fn a_connection_that_never_answers_ends_as_a_timed_out_call_not_a_hang() {
    let Some(fx) = Fx::new() else { return };
    // A server that accepts and says nothing.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
    let _hold = std::thread::spawn(move || {
        let mut held = Vec::new();
        while let Ok((s, _)) = silent.accept() {
            held.push(s);
        }
    });
    let mut host = fx.host(&["sh"]);
    host.port = port;
    let remote = {
        let mut c = fx.client(vec![host.clone()]);
        c.timeouts.quick = short(30, 2);
        c.timeouts.prepare = short(30, 2);
        SshRemoteHost::new(c, fx.vcs(), author())
    };
    // Pin whatever the real server's key is for this host entry, as an operator would have done.
    let key = fleet_infra::ssh::known_hosts::HostKey {
        key_type: "ssh-ed25519".into(),
        base64: fx.sshd.host_key().split(' ').nth(1).unwrap().to_string(),
    };
    let scanned = trust::Scanned {
        key,
        field: fleet_infra::ssh::known_hosts::host_field(&host.address, host.port),
    };
    trust::pin(remote.client(), &host, &scanned).unwrap();
    let wt = fx.worktree("r1", "t1");
    let t = std::time::Instant::now();
    let err = prepare(&remote, &rt("build", "r1", "t1"), &wt).unwrap_err();
    assert!(t.elapsed() < Duration::from_secs(20), "{:?}", t.elapsed());
    assert!(
        err.message.contains("timed out") || err.message.contains("silent"),
        "{}",
        err.message
    );
    // The host is then treated as down for a while: the next attempt fails at once.
    let t = std::time::Instant::now();
    let err = prepare(&remote, &rt("build", "r1", "t1"), &wt).unwrap_err();
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    assert!(err.message.contains("not trying again"), "{}", err.message);
}

#[test]
fn a_jump_host_hop_uses_the_same_strict_options_and_its_processes_are_refused_as_origin() {
    let Some(fx) = Fx::new() else { return };
    let mut jump = fx.sshd.host_def("jump", &[]);
    jump.allowed_workers = vec![];
    let mut target = fx.host(&["sh"]);
    // Another name for the same server: ssh refuses a jump host that is the target itself.
    target.address = "localhost".to_string();
    target.proxy_jump = Some(jump.name.clone());
    let remote = fx.remote(vec![jump.clone(), target.clone()]);
    // The target cannot be trusted before its jump host is.
    let err = trust::scan(remote.client(), &target, Duration::from_secs(20)).unwrap_err();
    assert!(
        err.message.contains("fleet host trust jump"),
        "{}",
        err.message
    );
    fx.trust(&remote, &jump);
    fx.trust(&remote, &target);
    let wt = fx.worktree("r1", "t1");
    let task = rt("build", "r1", "t1");
    prepare(&remote, &task, &wt).unwrap();
    // A worker that runs a while: look at the client and what it started while it runs.
    let tracker = fx.tracker.clone();
    let layout = target.layout().unwrap();
    let allowed = target.allowed_workers.clone();
    let args = ["-c".to_string(), "sleep 4".to_string()];
    let watcher = {
        let tracker = tracker.clone();
        std::thread::spawn(move || {
            assert!(wait_until(10_000, || !tracker.live().is_empty()));
            let pid = tracker.live()[0].pid;
            let mut seen = vec![pid];
            wait_until(8_000, || {
                let table = fleet_infra::procid::process_table().unwrap_or_default();
                seen = std::iter::once(pid)
                    .chain(fleet_infra::procid::descendants_in(&table, pid))
                    .collect();
                seen.len() > 1
            });
            let live = tracker.live();
            let refused: Vec<bool> = seen
                .iter()
                .map(|p| {
                    let chain = fleet_infra::procid::lineage(*p).chain;
                    fleet_domain::supervision::is_worker_origin(&chain, &live)
                })
                .collect();
            (seen, refused)
        })
    };
    let call = RemoteCall::Launch {
        layout: &layout,
        run: &task.run,
        task: &task.task,
        program: "sh",
        allowed: &allowed,
        args: &args,
    };
    let done = remote
        .client()
        .run(
            &target,
            &call,
            Input::None,
            Output::Capture { max: 4096 },
            short(30, 20),
        )
        .unwrap();
    assert!(
        done.success(),
        "{:?} {}",
        done.ending,
        String::from_utf8_lossy(&done.stderr)
    );
    let (seen, refused) = watcher.join().unwrap();
    assert!(
        seen.len() > 1,
        "the jump hop started its own ssh process: {seen:?}"
    );
    assert!(
        refused.iter().all(|r| *r),
        "every process of the call is refused as origin: {seen:?} {refused:?}"
    );
    assert!(tracker.live().is_empty(), "and none is listed afterwards");
    // The hop is as strict as the main connection: the server saw two logins with our key only.
    let log = fx.sshd.log();
    assert!(log.matches("Accepted publickey").count() >= 2, "{log}");
}

#[test]
fn a_remote_worker_that_ignores_term_and_hangup_is_stopped_by_the_remote_kill() {
    let Some(fx) = Fx::new() else { return };
    let (remote, host, task, _wt, _prepared) = prepared_task(&fx, &["sh"]);
    let pidfile = fx.remote_dir(&host, "r1", "t1").with_extension("pid");
    let marker = fx.sshd.dir.join("traps-installed");
    let marker_arg = marker.display().to_string();
    let remote = Arc::new(remote);
    let worker = {
        let remote = Arc::clone(&remote);
        let host = host.clone();
        let task = task.clone();
        std::thread::spawn(move || {
            run_worker(
                &remote,
                &host,
                &task,
                "sh",
                &["-c", STUBBORN_WORKER, "sh", &marker_arg],
                &[],
                40,
            )
        })
    };
    assert!(wait_until(15_000, || std::fs::read_to_string(&pidfile)
        .is_ok_and(|t| t.split_whitespace().count() == 2)));
    let text = std::fs::read_to_string(&pidfile).unwrap();
    let pid: i32 = text.split_whitespace().next().unwrap().parse().unwrap();
    let alive = |p: i32| nix::sys::signal::kill(nix::unistd::Pid::from_raw(p), None).is_ok();
    assert!(alive(pid));
    wait_for_traps(&marker);
    // TERM: ignored by the worker.
    remote.signal(&task, Signal::Term);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(alive(pid), "the worker ignores TERM");
    // KILL: the process group goes.
    remote.signal(&task, Signal::Kill);
    assert!(
        wait_until(10_000, || !alive(pid)),
        "the remote worker survived KILL"
    );
    let done = worker.join().unwrap();
    assert!(!done.success() || matches!(done.ending, Ending::Exited { .. }));
}

#[test]
fn reconciliation_removes_what_a_dead_daemon_left_and_says_so() {
    let Some(fx) = Fx::new() else { return };
    let (remote, host, _task, _wt, _prepared) = prepared_task(&fx, &["sh"]);
    let dir = fx.remote_dir(&host, "r1", "t1");
    assert!(dir.exists());
    // A new daemon (a new adapter over the same state directory) cleans up.
    let again = fx.remote(vec![host.clone()]);
    let lines = again.reconcile();
    assert!(lines.iter().any(|l| l.contains("was removed")), "{lines:?}");
    assert!(!dir.exists());
    assert!(again.reconcile().is_empty(), "nothing is left to report");
    drop(remote);
}

#[test]
fn an_oversized_bundle_is_refused_before_anything_is_sent() {
    let Some(fx) = Fx::new() else { return };
    let mut host = fx.host(&["sh"]);
    host.max_bundle_bytes = 1024 * 1024;
    let remote = fx.remote(vec![host.clone()]);
    fx.trust(&remote, &host);
    let wt = fx.worktree("r1", "t1");
    // Incompressible content larger than the limit, committed on the task branch.
    let mut data = vec![0u8; 2 * 1024 * 1024];
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    for b in &mut data {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x >> 24) as u8;
    }
    std::fs::write(format!("{wt}/big.bin"), &data).unwrap();
    git(std::path::Path::new(&wt), &["add", "-A"]);
    git(std::path::Path::new(&wt), &["commit", "-q", "-m", "big"]);
    let before = connections(&fx);
    let err = prepare(&remote, &rt("build", "r1", "t1"), &wt).unwrap_err();
    assert!(err.message.contains("bundle"), "{}", err.message);
    assert_eq!(connections(&fx), before);
    assert!(!fx.remote_dir(&host, "r1", "t1").exists());
}

// ---- the whole path: terminal session, sanitizer, bounds, supervision -------------------------------

mod sessions {
    use super::*;
    use fleet_app::ports::{SessionHost, SessionId, SessionSpec, SessionState, SignalOutcome};
    use fleet_app::remote::RemoteSessions;
    use fleet_infra::pty::{PtySessionHost, SessionRegistry};

    pub struct Rig {
        pub sessions: RemoteSessions,
        pub registry: SessionRegistry,
        pub host: HostDef,
    }

    pub fn rig(fx: &Fx, workers: &[&str], ring: usize, redactor: Redactor) -> Rig {
        let mut host = fx.host(workers);
        host.env = vec![("API_BASE".to_string(), "https://api.invalid".to_string())];
        let remote = fx.remote(vec![host.clone()]);
        fx.trust(&remote, &host);
        let registry = SessionRegistry::default();
        let pty = PtySessionHost::new(ring, Arc::new(redactor), registry.clone());
        let hosts: BTreeMap<HostName, HostDef> =
            std::iter::once((host.name.clone(), host.clone())).collect();
        Rig {
            sessions: RemoteSessions::new(Box::new(pty), Arc::new(remote), hosts),
            registry,
            host,
        }
    }

    pub fn spec(fx: &Fx, rig: &Rig, program: &str, args: &[&str]) -> SessionSpec {
        let wt = fx.worktree("r1", "t1");
        let dir = rig.host.layout().unwrap().worktree(&rid("r1"), &tid("t1"));
        SessionSpec {
            run: rid("r1"),
            task: tid("t1"),
            attempt: 1,
            program: program.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
            cwd: wt,
            env: worker_env(&rig.host.env, &rid("r1"), &tid("t1"), &dir),
            host: Some(rig.host.name.clone()),
        }
    }

    pub fn wait_exit(rig: &mut Rig, id: SessionId, secs: u64) -> SessionState {
        let mut last = SessionState::Running;
        wait_until(secs * 1000, || {
            last = rig.sessions.poll(id);
            last != SessionState::Running
        });
        last
    }

    #[test]
    fn a_remote_worker_runs_in_a_local_terminal_with_sanitized_bounded_redacted_output() {
        let Some(fx) = Fx::new() else { return };
        let mut redactor = Redactor::builtin();
        redactor.add_env_values(vec![(
            "DEPLOY_TOKEN".to_string(),
            "remote-secret-value-1234".to_string(),
        )]);
        let mut rig = rig(&fx, &["sh"], 64 * 1024, redactor);
        // Hostile terminal text, a secret, then far more output than the ring keeps.
        let script =
            "printf 'start\\033]52;c;ZXZpbA==\\a\\033]0;owned\\a\\033[2J\\033[31mred\\033[0m\\n'; \
                      echo 'token remote-secret-value-1234'; \
                      head -c 40000000 /dev/zero | tr '\\0' 'x'; echo; echo finished-marker";
        let spec = spec(&fx, &rig, "sh", &["-c", script]);
        let spawned = rig.sessions.spawn(&spec).unwrap();
        let state = wait_exit(&mut rig, spawned.id, 90);
        assert!(
            matches!(state, SessionState::Exited { code: Some(0), .. }),
            "{state:?}"
        );
        let snapshot = rig.sessions.snapshot(spawned.id, 10 * 1024 * 1024).unwrap();
        assert!(
            snapshot.len() <= 80 * 1024,
            "ring is bounded: {} bytes",
            snapshot.len()
        );
        assert!(
            snapshot.contains("finished-marker"),
            "the end of the output is kept"
        );
        let all = rig.sessions.release(spawned.id);
        assert!(all.text.len() <= 80 * 1024 + 4096, "{}", all.text.len());
        for hostile in ["\u{1b}", "ZXZpbA", "owned"] {
            assert!(
                !all.text.contains(hostile),
                "{hostile:?} reached the stored output"
            );
        }
        assert!(
            !all.text.contains("remote-secret-value-1234"),
            "the secret is redacted"
        );
        assert!(
            all.text.contains("brought back") || all.text.contains("no change"),
            "{}",
            &all.text[all.text.len().saturating_sub(400)..]
        );
        assert!(rig.registry.output_bytes() < 200 * 1024 * 1024);
        assert!(!fx.remote_dir(&rig.host, "r1", "t1").exists(), "cleaned up");
    }

    #[test]
    fn stopping_a_remote_worker_that_ignores_term_and_hangup_kills_it_on_the_host_too() {
        let Some(fx) = Fx::new() else { return };
        let mut rig = rig(&fx, &["sh"], 64 * 1024, Redactor::builtin());
        let marker = fx.sshd.dir.join("traps-installed");
        let spec = spec(
            &fx,
            &rig,
            "sh",
            &["-c", STUBBORN_WORKER, "sh", &marker.display().to_string()],
        );
        let spawned = rig.sessions.spawn(&spec).unwrap();
        // The local ssh client is a live session: the origin checks refuse it and what it starts.
        assert!(
            rig.sessions
                .live()
                .iter()
                .any(|l| l.pid == spawned.identity.pid),
            "{:?}",
            rig.sessions.live()
        );
        let pidfile = fx.remote_dir(&rig.host, "r1", "t1").with_extension("pid");
        assert!(wait_until(20_000, || std::fs::read_to_string(&pidfile)
            .is_ok_and(|t| t.split_whitespace().count() == 2)));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let alive = |p: i32| nix::sys::signal::kill(nix::unistd::Pid::from_raw(p), None).is_ok();
        assert!(alive(pid));
        // Only a worker that already ignores TERM and HUP can show that TERM alone leaves it running.
        wait_for_traps(&marker);
        // The supervisor's sequence: TERM, a grace period, KILL.
        assert_eq!(
            rig.sessions.signal(spawned.id, Signal::Term),
            SignalOutcome::Sent
        );
        std::thread::sleep(Duration::from_millis(2500));
        assert!(
            alive(pid),
            "TERM (and the hang-up of the ended ssh client) stopped a worker that ignores both"
        );
        // KILL cannot be ignored: the worker on the host goes, and the session ends.
        let _ = rig.sessions.signal(spawned.id, Signal::Kill);
        let state = wait_exit(&mut rig, spawned.id, 60);
        assert!(matches!(state, SessionState::Exited { .. }), "{state:?}");
        assert!(
            wait_until(10_000, || !alive(pid)),
            "the worker on the host survived KILL"
        );
        let out = rig.sessions.release(spawned.id);
        assert!(out.text.contains("was removed"), "{}", out.text);
        assert!(!fx.remote_dir(&rig.host, "r1", "t1").exists());
    }

    #[test]
    fn the_worker_environment_in_the_terminal_holds_no_local_secret() {
        let Some(fx) = Fx::new() else { return };
        let mut rig = rig(&fx, &["sh"], 64 * 1024, Redactor::builtin());
        let mut spec = spec(&fx, &rig, "sh", &["-c", "env | sort"]);
        // Whatever a careless caller put into the spec's environment is exactly what is sent: the
        // orchestrator builds it from the host's explicit values only (tested in fleet-app).
        spec.env.retain(|(k, _)| k != "SENTINEL_LOCAL");
        let spawned = rig.sessions.spawn(&spec).unwrap();
        let state = wait_exit(&mut rig, spawned.id, 60);
        assert!(
            matches!(state, SessionState::Exited { code: Some(0), .. }),
            "{state:?}"
        );
        let text = rig.sessions.release(spawned.id).text;
        assert!(text.contains("API_BASE=https://api.invalid"), "{text}");
        assert!(text.contains("FLEET_TASK=t1"));
        for leaked in ["HOME=", "USER=", "SSH_", "SENTINEL", "LANG=", "XDG_"] {
            assert!(!text.contains(leaked), "{leaked} in {text}");
        }
    }

    #[test]
    fn a_frozen_server_ends_the_attempt_through_the_keepalive_not_a_hang() {
        let Some(fx) = Fx::new() else { return };
        let mut rig = rig(&fx, &["sh"], 64 * 1024, Redactor::builtin());
        rig.host.server_alive_secs = 1;
        rig.host.server_alive_count = 2;
        // Rebuild the rig's remote with short keepalives (the host entry is part of the client).
        let remote = fx.remote(vec![rig.host.clone()]);
        let registry = SessionRegistry::default();
        let pty = PtySessionHost::new(64 * 1024, Arc::new(Redactor::builtin()), registry.clone());
        let hosts: BTreeMap<HostName, HostDef> =
            std::iter::once((rig.host.name.clone(), rig.host.clone())).collect();
        rig.sessions = RemoteSessions::new(Box::new(pty), Arc::new(remote), hosts);
        let spec = spec(&fx, &rig, "sh", &["-c", "echo connected; sleep 600"]);
        let spawned = rig.sessions.spawn(&spec).unwrap();
        assert!(wait_until(20_000, || rig
            .sessions
            .snapshot(spawned.id, 4096)
            .is_some_and(|t| t.contains("connected"))));
        // Freeze the whole server: it answers nothing, including keepalives.
        let sshd_pid = std::fs::read_to_string(fx.sshd.dir.join("sshd.pid"))
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        let children: Vec<i32> = {
            let table = fleet_infra::procid::process_table().unwrap_or_default();
            fleet_infra::procid::descendants_in(&table, sshd_pid as u32)
                .into_iter()
                .map(|p| p as i32)
                .collect()
        };
        for p in std::iter::once(sshd_pid).chain(children.iter().copied()) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(p),
                nix::sys::signal::Signal::SIGSTOP,
            );
        }
        let started = std::time::Instant::now();
        let state = wait_exit(&mut rig, spawned.id, 90);
        for p in std::iter::once(sshd_pid).chain(children.iter().copied()) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(p),
                nix::sys::signal::Signal::SIGCONT,
            );
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(p),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        assert!(
            matches!(state, SessionState::Exited { code: Some(c), .. } if c != 0),
            "{state:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "{:?}",
            started.elapsed()
        );
        let out = rig.sessions.release(spawned.id);
        assert!(
            out.text.contains("could not be brought back")
                || out.text.contains("could not be collected"),
            "{}",
            out.text
        );
    }
}

// ---- the port contract -------------------------------------------------------------------------------

struct SshHarness<'a> {
    fx: &'a Fx,
    remote: SshRemoteHost,
    host: HostDef,
    wt: String,
}

impl fleet_app::testing::remote::RemoteHarness for SshHarness<'_> {
    fn host(&self) -> &dyn RemoteHost {
        &self.remote
    }
    fn task(&self) -> RemoteTask {
        rt("build", "r1", "t1")
    }
    fn worktree(&self) -> String {
        self.wt.clone()
    }
    fn worker(&self, change: &fleet_app::testing::remote::WorkerChange) -> (String, Vec<String>) {
        use fleet_app::testing::remote::WorkerChange;
        let script = match change {
            WorkerChange::Nothing => "true".to_string(),
            WorkerChange::WriteFile { name, content } => {
                format!(
                    "printf '%s\\n' '{}' > {name}",
                    content.trim_end_matches('\n')
                )
            }
        };
        ("sh".to_string(), vec!["-c".to_string(), script])
    }
    fn worker_env(&self) -> Vec<(String, String)> {
        let dir = self.remote.remote_dir(&self.task()).unwrap();
        worker_env(&self.host.env, &rid("r1"), &tid("t1"), &dir)
    }
    fn run_local(
        &mut self,
        command: &fleet_app::remote::LocalCommand,
        _change: &fleet_app::testing::remote::WorkerChange,
    ) -> i32 {
        let done = exec::run(
            &Spec {
                program: PathBuf::from(&command.program),
                args: command.args.clone(),
                env: command.env.clone(),
                input: Input::None,
                output: Output::Capture { max: 1 << 20 },
                limits: Limits {
                    total: Duration::from_secs(60),
                    stall: Duration::from_secs(60),
                },
            },
            &JobTracker::new(),
        );
        done.code().unwrap_or(-1)
    }
    fn local_file(&self, relative: &str) -> Option<String> {
        std::fs::read_to_string(format!("{}/{relative}", self.wt)).ok()
    }
    fn local_head(&self) -> String {
        git(std::path::Path::new(&self.wt), &["rev-parse", "HEAD"])
    }
    fn remote_exists(&self) -> bool {
        self.fx.remote_dir(&self.host, "r1", "t1").exists()
    }
}

#[test]
fn the_ssh_adapter_satisfies_the_remote_host_contract() {
    let Some(fx) = Fx::new() else { return };
    let host = fx.host(&["sh"]);
    let remote = fx.remote(vec![host.clone()]);
    fx.trust(&remote, &host);
    let wt = fx.worktree("r1", "t1");
    let mut harness = SshHarness {
        fx: &fx,
        remote,
        host,
        wt,
    };
    fleet_app::testing::remote::remote_host_contract(&mut harness);
}

#[test]
fn a_secret_variable_is_in_no_command_line_and_the_file_it_travels_in_is_deleted() {
    let Some(fx) = Fx::new() else { return };
    let (remote, host, task, _wt, _prepared) = prepared_task(&fx, &["sh"]);
    let secret = "top-secret-argv-check-77"; // secret-scan: allow
    let dir = remote.remote_dir(&task).unwrap();
    let mut env = worker_env(&host.env, &task.run, &task.task, &dir);
    env.push(("API_KEY".to_string(), secret.to_string()));
    let cmd = remote
        .launch_command(&LaunchRequest {
            task: task.clone(),
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "printenv API_KEY; ls ..".to_string()],
            env,
        })
        .unwrap();
    assert!(
        !cmd.args.join(" ").contains(secret),
        "the secret is on the ssh command line: {:?}",
        cmd.args
    );
    let envfile = fx.remote_dir(&host, "r1", "t1").with_extension("env");
    assert!(
        envfile.exists(),
        "the variables wait in a file until the launcher reads them"
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&envfile).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let done = exec::run(
        &Spec {
            program: PathBuf::from(&cmd.program),
            args: cmd.args,
            env: cmd.env,
            input: Input::None,
            output: Output::Capture { max: 1 << 20 },
            limits: Limits {
                total: Duration::from_secs(60),
                stall: Duration::from_secs(60),
            },
        },
        &JobTracker::new(),
    );
    let text = output_text(&done);
    assert!(done.success(), "{text}");
    assert!(text.contains(secret), "the worker got the value: {text}");
    assert!(
        !text.contains("t1.env"),
        "the file is gone before the worker runs: {text}"
    );
    assert!(!envfile.exists());
}

#[test]
fn a_base_directory_that_others_can_write_is_refused_by_prepare_and_the_doctor_call() {
    let Some(fx) = Fx::new() else { return };
    let host = fx.host(&["sh"]);
    let remote = fx.remote(vec![host.clone()]);
    fx.trust(&remote, &host);
    let wt = fx.worktree("r1", "t1");
    std::fs::create_dir_all(&host.base_dir).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&host.base_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let err = prepare(&remote, &rt("build", "r1", "t1"), &wt).unwrap_err();
    assert!(
        err.message.contains("writable by others"),
        "{}",
        err.message
    );
    assert!(!fx.remote_dir(&host, "r1", "t1").exists());
    let done = remote
        .client()
        .run(
            &host,
            &RemoteCall::CheckBase {
                base: &host.base_dir,
            },
            Input::None,
            Output::Capture { max: 4096 },
            short(30, 20),
        )
        .unwrap();
    assert!(
        !done.success() && !done.stdout.starts_with(b"ok"),
        "{:?}",
        done
    );
    // The same directory made private works.
    std::fs::set_permissions(&host.base_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(prepare(&remote, &rt("build", "r1", "t1"), &wt).is_ok());
    // A run directory that others can write is refused too.
    remote.abandon(&rt("build", "r1", "t1"));
    assert!(wait_until(20_000, || !fx
        .remote_dir(&host, "r1", "t1")
        .exists()));
    std::fs::set_permissions(
        fx.remote_dir(&host, "r1", "t1").parent().unwrap(),
        std::fs::Permissions::from_mode(0o770),
    )
    .unwrap();
    let err = prepare(&remote, &rt("build", "r1", "t1"), &wt).unwrap_err();
    assert!(
        err.message.contains("writable by others"),
        "{}",
        err.message
    );
}
