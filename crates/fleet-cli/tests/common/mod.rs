//! Shared helpers of the end-to-end tests: a guard around a real `fleet daemon` child process, a
//! runner for the real `fleet` binary, builders for configuration and plan files, and a few
//! helpers to look at git, files and processes.
//!
//! Every child process gets an explicit, nearly empty environment (`PATH`, `HOME`, `LANG`, plus
//! `LLVM_PROFILE_FILE` when coverage is being collected); the environment of the test runner is
//! never inherited.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]

use fleet_domain::value::Value;
use fleet_infra::json::parse_value;
use fleet_infra::testutil::{git, make_repo, wait_until, TempDir};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The real binary under test.
pub const FLEET_BIN: &str = env!("CARGO_BIN_EXE_fleet");

/// One minute is ample for any CLI call; a longer one means a hang.
const CLI_TIMEOUT: Duration = Duration::from_secs(120);
/// Generous default for "eventually" conditions.
pub const WAIT_MS: u64 = 40_000;

// ---- examples -------------------------------------------------------------------------------

/// A file below `examples/` of the workspace, as a canonical path.
pub fn example(rel: &str) -> PathBuf {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(rel);
    fs::canonicalize(&p).unwrap_or_else(|e| panic!("example {} is missing: {e}", p.display()))
}

// ---- running the binary ----------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    /// stdout parsed as one JSON document.
    pub fn json(&self) -> Value {
        parse_json(&self.stdout)
    }

    pub fn stderr_first_line(&self) -> &str {
        self.stderr.lines().next().unwrap_or("")
    }
}

pub fn parse_json(text: &str) -> Value {
    parse_value(text.as_bytes(), 32 * 1024 * 1024)
        .unwrap_or_else(|e| panic!("not valid JSON ({e}): {text}"))
}

/// Runs a prepared command to completion with both pipes drained; kills it and panics after
/// `timeout`.
pub fn run_cmd(mut cmd: Command, timeout: Duration) -> Out {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn child");
    let mut so = child.stdout.take().expect("stdout pipe");
    let mut se = child.stderr.take().expect("stderr pipe");
    let t_out = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = so.read_to_end(&mut b);
        b
    });
    let t_err = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = se.read_to_end(&mut b);
        b
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(s) => break s,
            None if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("command {cmd:?} did not finish within {timeout:?}");
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let stdout = t_out.join().unwrap_or_default();
    let stderr = t_err.join().unwrap_or_default();
    Out {
        code: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    }
}

/// The runner's own `PATH`, which is the only thing needed from its environment (to find git,
/// sh and friends).
pub fn system_path() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_string())
}

/// A `fleet` command with an explicit environment.
pub fn fleet_command(path_env: &str, home: &Path) -> Command {
    let mut c = Command::new(FLEET_BIN);
    c.env_clear()
        .env("PATH", path_env)
        .env("HOME", home)
        .env("LANG", "C");
    if let Ok(p) = std::env::var("LLVM_PROFILE_FILE") {
        c.env("LLVM_PROFILE_FILE", p);
    }
    c
}

// ---- toml builders ---------------------------------------------------------------------------

/// A TOML basic string.
pub fn q(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn qp(p: &Path) -> String {
    q(&p.to_string_lossy())
}

/// Builder of a `fleet.toml`.
#[derive(Clone)]
pub struct Cfg {
    daemon: Vec<(String, String)>,
    env_allow: Vec<String>,
    workers: Vec<(String, String)>,
    gates: Vec<String>,
    tail: String,
}

impl Cfg {
    pub fn new() -> Cfg {
        let d = |k: &str, v: &str| (k.to_string(), v.to_string());
        Cfg {
            daemon: vec![
                d("max_sessions", "4"),
                d("max_retries", "2"),
                d("kill_grace_secs", "1"),
                d("author_name", &q("Fleet Test")),
                d("author_email", &q("fleet@example.invalid")),
            ],
            env_allow: ["PATH", "HOME", "LANG", "TERM"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            workers: Vec::new(),
            gates: Vec::new(),
            tail: String::new(),
        }
    }

    /// Sets a `[daemon]` key to a raw TOML value, replacing an earlier one.
    pub fn set(mut self, key: &str, raw: &str) -> Cfg {
        self.daemon.retain(|(k, _)| k != key);
        self.daemon.push((key.to_string(), raw.to_string()));
        self
    }

    pub fn allow_env(mut self, names: &[&str]) -> Cfg {
        self.env_allow = names.iter().map(|s| s.to_string()).collect();
        self
    }

    /// Adds a worker; the first one added is the default worker. `extra` holds raw
    /// `key = value` lines (timeouts).
    pub fn worker(mut self, name: &str, command: &Path, args: &[&str], extra: &str) -> Cfg {
        let args: Vec<String> = args.iter().map(|a| q(a)).collect();
        let block = format!(
            "[workers.{name}]\ncommand = {}\nargs = [{}]\n{extra}\n",
            qp(command),
            args.join(", ")
        );
        self.workers.push((name.to_string(), block));
        self
    }

    pub fn gate(mut self, name: &str, command: &Path, args: &[&str], extra: &str) -> Cfg {
        let args: Vec<String> = args.iter().map(|a| q(a)).collect();
        self.gates.push(format!(
            "[[gates]]\nname = {}\ncommand = {}\nargs = [{}]\n{extra}\n",
            q(name),
            qp(command),
            args.join(", ")
        ));
        self
    }

    /// Text appended verbatim at the end (further tables).
    pub fn raw(mut self, text: &str) -> Cfg {
        self.tail.push_str(text);
        self.tail.push('\n');
        self
    }

    pub fn render(&self) -> String {
        let mut out = String::from("[daemon]\n");
        for (k, v) in &self.daemon {
            out.push_str(&format!("{k} = {v}\n"));
        }
        if !self.daemon.iter().any(|(k, _)| k == "default_worker") {
            if let Some((name, _)) = self.workers.first() {
                out.push_str(&format!("default_worker = {}\n", q(name)));
            }
        }
        let allow: Vec<String> = self.env_allow.iter().map(|a| q(a)).collect();
        out.push_str(&format!("\n[env]\nallow = [{}]\n\n", allow.join(", ")));
        for (_, block) in &self.workers {
            out.push_str(block);
            out.push('\n');
        }
        for g in &self.gates {
            out.push_str(g);
            out.push('\n');
        }
        out.push_str(&self.tail);
        out
    }
}

/// The configuration of the scripted demo: the fake agent and the changelog gate.
pub fn demo_cfg() -> Cfg {
    Cfg::new()
        .worker(
            "scripted",
            &example("agents/fake-agent.sh"),
            &["{task}"],
            "timeout_secs = 60\nidle_timeout_secs = 30",
        )
        .gate(
            "changelog",
            &example("agents/check-changelog.sh"),
            &[],
            "timeout_secs = 30",
        )
}

/// One task of a plan.
#[derive(Clone)]
pub struct T {
    pub id: String,
    pub spec: String,
    pub after: Vec<String>,
    pub worker: Option<String>,
}

pub fn t(id: &str) -> T {
    T {
        id: id.to_string(),
        spec: String::new(),
        after: Vec::new(),
        worker: None,
    }
}

impl T {
    pub fn after(mut self, deps: &[&str]) -> T {
        self.after = deps.iter().map(|d| d.to_string()).collect();
        self
    }

    pub fn worker(mut self, name: &str) -> T {
        self.worker = Some(name.to_string());
        self
    }

    pub fn spec(mut self, text: &str) -> T {
        self.spec = text.to_string();
        self
    }
}

/// A plan file with a `[run]` table.
pub fn plan_toml(repo: &Path, run_id: &str, max_parallel: u32, tasks: &[T]) -> String {
    let mut out = format!(
        "[run]\nobjective = {}\nrepo = {}\nid = {}\nmax_parallel = {max_parallel}\n",
        q("end-to-end test run"),
        qp(repo),
        q(run_id)
    );
    for task in tasks {
        out.push_str(&format!(
            "\n[[tasks]]\nid = {}\ntitle = {}\n",
            q(&task.id),
            q(&format!("Task {}", task.id))
        ));
        if !task.spec.is_empty() {
            out.push_str(&format!("spec = {}\n", q(&task.spec)));
        }
        if !task.after.is_empty() {
            let a: Vec<String> = task.after.iter().map(|d| q(d)).collect();
            out.push_str(&format!("after = [{}]\n", a.join(", ")));
        }
        if let Some(w) = &task.worker {
            out.push_str(&format!("worker = {}\n", q(w)));
        }
    }
    out
}

// ---- json helpers ----------------------------------------------------------------------------

pub fn jstr<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

pub fn jnum(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

pub fn jarr<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).unwrap_or(&[])
}

pub fn jbool(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Sorted key names of a JSON object.
pub fn keys(v: &Value) -> Vec<String> {
    v.as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// A task object of a `fleet status --json` document.
pub fn find_task<'a>(status: &'a Value, run: &str, task: &str) -> Option<&'a Value> {
    jarr(status, "runs")
        .iter()
        .find(|r| jstr(r, "id") == run)
        .and_then(|r| jarr(r, "tasks").iter().find(|t| jstr(t, "id") == task))
}

// ---- processes and files -----------------------------------------------------------------------

/// True while the process exists and is not a zombie.
pub fn pid_alive(pid: u32) -> bool {
    let proc_stat = format!("/proc/{pid}/stat");
    if Path::new("/proc/self/stat").exists() {
        return match fs::read_to_string(&proc_stat) {
            Ok(text) => {
                let state = text
                    .rsplit(')')
                    .next()
                    .and_then(|rest| rest.trim_start().chars().next());
                !matches!(state, Some('Z') | Some('X') | None)
            }
            Err(_) => false,
        };
    }
    match Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
    {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            !s.is_empty() && !s.starts_with('Z')
        }
        _ => false,
    }
}

pub fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Waits for a file that holds a pid and returns the pid.
pub fn wait_pid_file(path: &Path) -> u32 {
    assert!(
        wait_until(WAIT_MS, || read_pid(path).is_some()),
        "no pid appeared in {}",
        path.display()
    );
    read_pid(path).unwrap()
}

pub fn wait_file(path: &Path) {
    assert!(
        wait_until(WAIT_MS, || path.exists()),
        "{} never appeared",
        path.display()
    );
}

/// All regular files below `dir`, never following symlinks.
pub fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(meta) = fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.is_dir() {
            out.extend(walk_files(&p));
        } else if meta.is_file() {
            out.push(p);
        }
    }
    out
}

/// The first regular file below `dir` whose bytes contain `needle`.
pub fn tree_contains(dir: &Path, needle: &str) -> Option<PathBuf> {
    let n = needle.as_bytes();
    walk_files(dir).into_iter().find(|p| {
        fs::read(p)
            .map(|b| b.windows(n.len()).any(|w| w == n))
            .unwrap_or(false)
    })
}

/// A value that is unique per call, for sentinel secrets.
pub fn unique_token(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{}-{nanos}", std::process::id())
}

/// `git` for reading the fixture repository, with the settings that plain git would otherwise
/// take from a repository that has hostile config planted in it switched off.
pub fn safe_git(dir: &Path, args: &[&str]) -> String {
    let mut full = vec![
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "filter.leak.clean=",
        "-c",
        "filter.leak.smudge=",
        "-c",
        "filter.leak.required=false",
    ];
    full.extend_from_slice(args);
    git(dir, &full)
}

/// Mode bits (`& 0o777`) of a path.
pub fn mode_of(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::symlink_metadata(p)
        .unwrap_or_else(|e| panic!("stat {}: {e}", p.display()))
        .permissions()
        .mode()
        & 0o777
}

pub fn script(dir: &TempDir, rel: &str, body: &str) -> PathBuf {
    dir.script(rel, &format!("#!/bin/sh\n{body}\n"))
}

// ---- the fixture -------------------------------------------------------------------------------

/// A temporary world: a git repository, a state directory, a configuration and (once started)
/// a real daemon. Dropping it kills the daemon and every worker process group it started.
pub struct Fleet {
    pub tmp: TempDir,
    pub repo: PathBuf,
    pub state: PathBuf,
    pub home: PathBuf,
    pub config: PathBuf,
    pub path_env: String,
    /// Extra variables of the daemon's environment only.
    pub daemon_env: Vec<(String, String)>,
    child: Option<Child>,
}

impl Fleet {
    pub fn new() -> Fleet {
        let tmp = TempDir::new("e");
        let repo = make_repo(&tmp);
        let home = tmp.join("h");
        fs::create_dir_all(&home).expect("create home");
        let state = tmp.join("s");
        let config = tmp.join("fleet.toml");
        Fleet {
            tmp,
            repo,
            state,
            home,
            config,
            path_env: system_path(),
            daemon_env: Vec::new(),
            child: None,
        }
    }

    /// A fleet whose configuration is already written.
    pub fn with_cfg(cfg: &Cfg) -> Fleet {
        let f = Fleet::new();
        f.write_cfg(cfg);
        f
    }

    pub fn write_cfg(&self, cfg: &Cfg) {
        fs::write(&self.config, cfg.render()).expect("write fleet.toml");
    }

    /// Writes a worker script into the temp dir and returns its path.
    pub fn script(&self, rel: &str, body: &str) -> PathBuf {
        script(&self.tmp, rel, body)
    }

    // ---- commands ----

    pub fn command(&self) -> Command {
        fleet_command(&self.path_env, &self.home)
    }

    /// Runs `fleet --state-dir <state> <args...>`.
    pub fn cli(&self, args: &[&str]) -> Out {
        let mut c = self.command();
        c.arg("--state-dir").arg(&self.state).args(args);
        run_cmd(c, CLI_TIMEOUT)
    }

    /// Like `cli`, asserting a zero exit code.
    pub fn ok(&self, args: &[&str]) -> Out {
        let out = self.cli(args);
        assert_eq!(
            out.code,
            0,
            "fleet {args:?} failed: {out:?}\n{}",
            self.daemon_log()
        );
        out
    }

    /// `fleet <args> --json`, asserting success; returns the parsed document.
    pub fn json(&self, args: &[&str]) -> Value {
        let mut a: Vec<&str> = args.to_vec();
        a.push("--json");
        self.ok(&a).json()
    }

    /// `fleet <args> --json` that is expected to fail; returns (exit code, error document).
    pub fn json_err(&self, args: &[&str]) -> (i32, Value) {
        let mut a: Vec<&str> = args.to_vec();
        a.push("--json");
        let out = self.cli(&a);
        assert_ne!(
            out.code, 0,
            "fleet {args:?} unexpectedly succeeded: {out:?}"
        );
        let v = out.json();
        (out.code, v)
    }

    pub fn status(&self) -> Value {
        self.json(&["status"])
    }

    pub fn gates(&self) -> Vec<Value> {
        jarr(&self.json(&["gate", "list"]), "gates").to_vec()
    }

    pub fn approvals(&self) -> Vec<Value> {
        jarr(&self.json(&["approval", "list"]), "approvals").to_vec()
    }

    /// The task document of a run's task.
    pub fn task(&self, run: &str, task: &str) -> Value {
        let st = self.status();
        find_task(&st, run, task)
            .unwrap_or_else(|| panic!("no task {run}/{task} in {}", st.canonical()))
            .clone()
    }

    pub fn recent_lines(&self) -> Vec<String> {
        let v = self.json(&["events", "--limit", "100"]);
        jarr(&v, "events")
            .iter()
            .map(|e| jstr(e, "line").to_string())
            .collect()
    }

    pub fn worktree(&self, run: &str, task: &str) -> PathBuf {
        self.repo.join(".fleet-worktrees").join(run).join(task)
    }

    pub fn events_log_path(&self) -> PathBuf {
        self.state.join("events.log")
    }

    /// The raw text of the audit log.
    pub fn events_raw(&self) -> String {
        fs::read(self.events_log_path())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    }

    pub fn daemon_log(&self) -> String {
        fs::read_to_string(self.tmp.join("d.err")).unwrap_or_default()
    }

    /// Daemon stderr and the status document, for assertion messages.
    pub fn diag(&self) -> String {
        let st = self.cli(&["status", "--json"]);
        format!(
            "--- daemon stderr ---\n{}\n--- status ---\n{}{}",
            self.daemon_log(),
            st.stdout,
            st.stderr
        )
    }

    // ---- waiting ----

    /// Polls `fleet status --json` until `pred` holds.
    pub fn wait_for(&self, what: &str, ms: u64, mut pred: impl FnMut(&Value) -> bool) -> Value {
        let start = Instant::now();
        loop {
            let out = self.cli(&["status", "--json"]);
            if out.code == 0 {
                if let Ok(v) = parse_value(out.stdout.as_bytes(), 32 * 1024 * 1024) {
                    if pred(&v) {
                        return v;
                    }
                }
            }
            if start.elapsed() > Duration::from_millis(ms) {
                panic!("timed out waiting for {what}\n{}", self.diag());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Waits until the task has one of the statuses.
    pub fn wait_task(&self, run: &str, task: &str, wanted: &[&str]) -> Value {
        let what = format!("{run}/{task} to become {wanted:?}");
        let st = self.wait_for(&what, WAIT_MS, |st| {
            find_task(st, run, task).is_some_and(|t| wanted.contains(&jstr(t, "status")))
        });
        find_task(&st, run, task).unwrap().clone()
    }

    /// Waits until the task is blocked for the given reason (`question`, `approval`, ...).
    pub fn wait_blocked(&self, run: &str, task: &str, kind: &str) -> Value {
        let what = format!("{run}/{task} to be blocked on {kind}");
        let st = self.wait_for(&what, WAIT_MS, |st| {
            find_task(st, run, task).is_some_and(|t| {
                jstr(t, "status") == "blocked"
                    && t.get("block").map(|b| jstr(b, "kind")) == Some(kind)
            })
        });
        find_task(&st, run, task).unwrap().clone()
    }

    /// Waits until the task has a live session.
    pub fn wait_live(&self, run: &str, task: &str) -> Value {
        let what = format!("{run}/{task} to have a live session");
        let st = self.wait_for(&what, WAIT_MS, |st| {
            find_task(st, run, task).is_some_and(|t| jbool(t, "live"))
        });
        find_task(&st, run, task).unwrap().clone()
    }

    // ---- plan and run helpers ----

    /// Writes a plan file, loads it and starts the run.
    pub fn plan_and_start(&self, run: &str, max_parallel: u32, tasks: &[T]) {
        let file = self.tmp.write(
            &format!("plan-{run}.toml"),
            &plan_toml(&self.repo, run, max_parallel, tasks),
        );
        self.ok(&["plan", file.to_str().unwrap()]);
        self.ok(&["start", "--run", run]);
    }

    /// Writes a plan file and loads it without starting.
    pub fn plan_only(&self, run: &str, max_parallel: u32, tasks: &[T]) -> Out {
        let file = self.tmp.write(
            &format!("plan-{run}.toml"),
            &plan_toml(&self.repo, run, max_parallel, tasks),
        );
        self.cli(&["plan", file.to_str().unwrap()])
    }

    // ---- the daemon ----

    fn daemon_command(&self) -> Command {
        let mut c = self.command();
        c.arg("daemon").arg("--state-dir").arg(&self.state);
        if self.config.exists() {
            c.arg("--config").arg(&self.config);
        }
        for (k, v) in &self.daemon_env {
            c.env(k, v);
        }
        c
    }

    /// Starts `fleet daemon` as a child and waits until it answers.
    pub fn start(&mut self) {
        assert!(self.child.is_none(), "a daemon is already running");
        let err = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.tmp.join("d.err"))
            .expect("open daemon log");
        let mut c = self.daemon_command();
        c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(err);
        self.child = Some(c.spawn().expect("spawn the daemon"));
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().expect("try_wait") {
                self.child = None;
                panic!("the daemon exited early ({status}):\n{}", self.daemon_log());
            }
            if self.cli(&["status"]).code == 0 {
                return;
            }
            if start.elapsed() > Duration::from_secs(30) {
                panic!("the daemon never answered:\n{}", self.daemon_log());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Runs a daemon in the foreground that is expected to refuse to start.
    pub fn daemon_refused(&self) -> Out {
        let c = self.daemon_command();
        run_cmd(c, Duration::from_secs(30))
    }

    pub fn daemon_pid(&self) -> u32 {
        self.child.as_ref().expect("no daemon running").id()
    }

    /// SIGKILL: the daemon gets no chance to clean up.
    pub fn kill_daemon(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Waits for the daemon child to exit; returns its exit code.
    pub fn wait_daemon_exit(&mut self, ms: u64) -> Option<i32> {
        let start = Instant::now();
        loop {
            let status = self.child.as_mut()?.try_wait().expect("try_wait");
            if let Some(s) = status {
                self.child = None;
                return Some(s.code().unwrap_or(-1));
            }
            if start.elapsed() > Duration::from_millis(ms) {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// `fleet shutdown`, then waits for the daemon to exit; returns its exit code.
    pub fn shutdown(&mut self) -> i32 {
        self.ok(&["shutdown"]);
        self.wait_daemon_exit(40_000).unwrap_or_else(|| {
            panic!(
                "the daemon did not exit after shutdown\n{}",
                self.daemon_log()
            )
        })
    }

    /// Pids of every worker session recorded in the audit log.
    pub fn recorded_worker_pids(&self) -> Vec<u32> {
        let mut pids = Vec::new();
        for line in self.events_raw().lines() {
            if !line.contains("\"type\":\"task_dispatched\"") {
                continue;
            }
            if let Some(i) = line.find("\"pid\":") {
                let digits: String = line[i + 6..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(p) = digits.parse::<u32>() {
                    if p > 1 {
                        pids.push(p);
                    }
                }
            }
        }
        pids
    }
}

impl Drop for Fleet {
    fn drop(&mut self) {
        self.kill_daemon();
        for pid in self.recorded_worker_pids() {
            // Workers lead their own session and process group.
            let _ = Command::new("sh")
                .args([
                    "-c",
                    "kill -s KILL -- \"-$1\" 2>/dev/null; kill -s KILL \"$1\" 2>/dev/null; true",
                    "sh",
                    &pid.to_string(),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// The full scripted demo up to the point where all four tasks are completed and nothing is
/// merged. Returns the running fleet.
pub fn demo_until_completed() -> Fleet {
    let mut f = Fleet::with_cfg(&demo_cfg());
    f.start();
    let plan = demo_plan_file(&f);
    f.ok(&["plan", plan.to_str().unwrap()]);
    f.ok(&["start", "--run", "r1", "--max-parallel", "2"]);
    f.wait_for("the api question", WAIT_MS, |st| {
        !jarr(st, "gates").is_empty()
    });
    f.ok(&["gate", "resolve", "g1", "--answer", "axum"]);
    f.wait_for("all four tasks completed", 60_000, |st| {
        jarr(st, "runs")
            .iter()
            .any(|r| r.get("counts").map(|c| jnum(c, "completed")) == Some(4))
    });
    f
}

/// `examples/plan.example.toml` with the repository filled in and the run id pinned.
pub fn demo_plan_file(f: &Fleet) -> PathBuf {
    let text = fs::read_to_string(example("plan.example.toml")).expect("read the example plan");
    assert!(
        text.contains("repo = \".\""),
        "the example plan changed shape"
    );
    let text = text.replace("repo = \".\"", &format!("repo = {}", qp(&f.repo)));
    let text = text.replacen("[run]\n", "[run]\nid = \"r1\"\n", 1);
    f.tmp.write("plan-demo.toml", &text)
}

/// Merges a task through the approval round trip.
pub fn merge_with_approval(f: &Fleet, task: &str) -> Out {
    let first = f.cli(&["merge", task]);
    assert_eq!(first.code, 5, "first merge of {task}: {first:?}");
    let id = first
        .stderr
        .lines()
        .find_map(|l| l.strip_prefix("run: fleet approve "))
        .unwrap_or_else(|| panic!("no approval id in {first:?}"))
        .trim()
        .to_string();
    f.ok(&["approve", &id]);
    f.ok(&["merge", task])
}

/// Replaces `@NAME@` placeholders in a script body with paths (keeps `${...}` shell syntax
/// free of `format!` escaping).
pub fn fill(body: &str, pairs: &[(&str, &Path)]) -> String {
    let mut out = body.to_string();
    for (name, path) in pairs {
        out = out.replace(&format!("@{name}@"), &path.to_string_lossy());
    }
    out
}

/// A configuration with a single worker `w` that runs `script <task>`.
pub fn simple_cfg(script: &Path, extra: &str) -> Cfg {
    Cfg::new().worker("w", script, &["{task}"], extra)
}
