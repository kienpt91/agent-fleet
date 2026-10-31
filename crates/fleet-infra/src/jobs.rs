//! Background jobs: quality gates and approved commands, run off the orchestrator's thread.
//!
//! [`run_bounded`] is the one place where this crate runs a child on behalf of the daemon without
//! a pty (gates, approved commands and the optional undo tool). It never uses a shell, gives the
//! child an empty environment plus an explicit list, a null stdin, its own process group and hard
//! caps on time and captured output. [`ThreadJobRunner`] runs jobs on plain OS threads.

use fleet_app::ports::{GateSpec, Job, JobDone, JobRunner};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::show;
use fleet_domain::limits::MAX_GATE_OUTPUT;
use fleet_domain::quality::{evaluate_exit, evaluate_refcheck, ExitKind, GateFormat, GateOutcome};
use fleet_domain::sanitize::{sanitize_text, tail_str};
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How often a running child is checked for exit, timeout and output overflow.
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Time a child gets between SIGTERM and SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// How long to wait for the capture threads after the child is gone. A descendant that left the
/// process group (setsid) can keep the pipe open; its output is then cut off instead of waited for.
const READER_GRACE: Duration = Duration::from_secs(2);
/// Bytes of output text handed to the gate evaluation.
const OUTPUT_TEXT_BYTES: usize = 64 * 1024;
/// Bytes of command output kept in the result.
const COMMAND_OUTPUT_BYTES: usize = 4 * 1024;
const STDERR_SEPARATOR: &str = "\n--- stderr ---\n";

/// What to run. `env` is the complete environment of the child.
#[derive(Clone, Debug)]
pub struct RunSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub timeout_ms: u64,
    /// Cap for each of stdout and stderr.
    pub max_output: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunResult {
    pub exit: ExitKind,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

fn spawn_failed(why: impl Into<String>) -> RunResult {
    RunResult {
        exit: ExitKind::SpawnFailed(why.into()),
        stdout: Vec::new(),
        stderr: Vec::new(),
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Finds the executable for `program`. A name with a '/' is used as given (relative to `cwd`);
/// a bare name is searched only in the `PATH` entry of `env`, never in the daemon's own `PATH`.
/// Empty `PATH` entries are skipped; relative ones resolve against `cwd`, the directory the child
/// runs in.
pub fn resolve_program(program: &str, cwd: &Path, env: &[(String, String)]) -> Option<PathBuf> {
    if program.is_empty() {
        return None;
    }
    if program.contains('/') {
        let given = Path::new(program);
        let full = if given.is_absolute() {
            given.to_path_buf()
        } else {
            cwd.join(given)
        };
        return is_executable_file(&full).then_some(full);
    }
    // The last entry wins, the same as when the entries are applied to the child's environment.
    let path = env.iter().rev().find(|(k, _)| k == "PATH").map(|(_, v)| v)?;
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| {
            let dir = Path::new(dir);
            if dir.is_absolute() {
                dir.join(program)
            } else {
                cwd.join(dir).join(program)
            }
        })
        .find(|candidate| is_executable_file(candidate))
}

/// Output captured from one pipe, capped.
#[derive(Default)]
struct Capture {
    buf: Mutex<Vec<u8>>,
    overflow: AtomicBool,
    done: AtomicBool,
}

/// Reads `pipe` to the end. Bytes beyond `max` are dropped (and flagged) but still read, so a
/// child that keeps writing never blocks on a full pipe while it is being stopped.
fn drain(mut pipe: impl Read, capture: &Capture, max: usize) {
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let Some(data) = chunk.get(..n) else { break };
                let mut buf = lock(&capture.buf);
                let take = max.saturating_sub(buf.len()).min(data.len());
                if let Some(head) = data.get(..take) {
                    buf.extend_from_slice(head);
                }
                if take < data.len() {
                    capture.overflow.store(true, Ordering::SeqCst);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    capture.done.store(true, Ordering::SeqCst);
}

fn spawn_reader<R: Read + Send + 'static>(
    pipe: R,
    capture: Arc<Capture>,
    max: usize,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("fleet-capture".into())
        .spawn(move || drain(pipe, &capture, max))
}

/// Sends `signal` to the process group led by `pid`. Errors are ignored: the group may be gone.
fn signal_group(pid: u32, signal: Signal) {
    // Never signal group 0 (our own group) or an id that does not fit.
    let Ok(raw) = i32::try_from(pid) else { return };
    if raw <= 1 {
        return;
    }
    let _ = killpg(Pid::from_raw(raw), signal);
}

/// Kills the whole group and reaps the leader.
fn terminate(child: &mut Child, pid: u32) {
    signal_group(pid, Signal::SIGKILL);
    let _ = child.kill();
    let _ = child.wait();
}

/// SIGTERM, a grace period for the leader to exit, then SIGKILL for the group.
fn stop_gracefully(child: &mut Child, pid: u32) {
    signal_group(pid, Signal::SIGTERM);
    let end = Instant::now() + KILL_GRACE;
    while Instant::now() < end {
        match child.try_wait() {
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Ok(Some(_)) | Err(_) => break,
        }
    }
    terminate(child, pid);
}

fn status_kind(status: ExitStatus) -> ExitKind {
    if let Some(code) = status.code() {
        ExitKind::Code(code)
    } else if let Some(sig) = status.signal() {
        ExitKind::Signal(sig)
    } else {
        ExitKind::Code(-1)
    }
}

/// Runs one program to completion within the limits of `spec`. Never panics, never uses a shell.
pub fn run_bounded(spec: &RunSpec) -> RunResult {
    let Some(program) = resolve_program(&spec.program, &spec.cwd, &spec.env) else {
        return spawn_failed(format!(
            "program {} not found or not executable",
            show(&spec.program)
        ));
    };
    let mut command = Command::new(&program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group, so timeouts and leftovers can be cleaned up as a unit.
        .process_group(0);
    let mut child = match spawn_retrying_busy(&mut command) {
        Ok(child) => child,
        Err(e) => return spawn_failed(e.to_string()),
    };
    let pid = child.id();

    let (Some(out_pipe), Some(err_pipe)) = (child.stdout.take(), child.stderr.take()) else {
        terminate(&mut child, pid);
        return spawn_failed("output pipes are missing");
    };
    let out = Arc::new(Capture::default());
    let err = Arc::new(Capture::default());
    let out_reader = spawn_reader(out_pipe, Arc::clone(&out), spec.max_output);
    let err_reader = spawn_reader(err_pipe, Arc::clone(&err), spec.max_output);
    let (Ok(out_reader), Ok(err_reader)) = (out_reader, err_reader) else {
        // A reader that did start ends by itself once the pipe closes with the child.
        terminate(&mut child, pid);
        return spawn_failed("could not start the output reader threads");
    };

    let deadline = Instant::now().checked_add(Duration::from_millis(spec.timeout_ms));
    let exit = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Members of the group that outlived the leader must not run on after the job
                // is "done". The leader is reaped at this point, so in theory its pid could be
                // reused by a new group within the microseconds between the reap and this call;
                // that window is accepted, the alternative (never cleaning up) is worse.
                signal_group(pid, Signal::SIGKILL);
                break status_kind(status);
            }
            Ok(None) => {}
            Err(e) => {
                terminate(&mut child, pid);
                break ExitKind::SpawnFailed(format!("waiting for the process failed: {e}"));
            }
        }
        if out.overflow.load(Ordering::SeqCst) || err.overflow.load(Ordering::SeqCst) {
            terminate(&mut child, pid);
            break ExitKind::OutputLimit;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            stop_gracefully(&mut child, pid);
            break ExitKind::TimedOut;
        }
        thread::sleep(POLL_INTERVAL);
    };

    let reader_end = Instant::now() + READER_GRACE;
    while !(out.done.load(Ordering::SeqCst) && err.done.load(Ordering::SeqCst))
        && Instant::now() < reader_end
    {
        thread::sleep(Duration::from_millis(5));
    }
    for (reader, capture) in [(out_reader, &out), (err_reader, &err)] {
        // A reader still blocked on a pipe held by an escaped process is left to end on its own.
        if capture.done.load(Ordering::SeqCst) {
            let _ = reader.join();
        }
    }

    let overflowed = out.overflow.load(Ordering::SeqCst) || err.overflow.load(Ordering::SeqCst);
    let exit = match exit {
        // Output cut at the cap is not the complete output, even if the process ended by itself.
        ExitKind::Code(_) | ExitKind::Signal(_) if overflowed => ExitKind::OutputLimit,
        other => other,
    };
    let stdout = std::mem::take(&mut *lock(&out.buf));
    let stderr = std::mem::take(&mut *lock(&err.buf));
    RunResult { exit, stdout, stderr }
}

/// Starts the command, retrying briefly while the executable is still open for writing in another
/// thread (`ETXTBSY`): a script that was just written can briefly be held open by a concurrent fork.
fn spawn_retrying_busy(command: &mut Command) -> std::io::Result<std::process::Child> {
    let mut attempts = 0;
    loop {
        match command.spawn() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 20 => {
                attempts += 1;
                thread::sleep(Duration::from_millis(25));
            }
            other => return other,
        }
    }
}

/// Last `max` bytes of `bytes`.
fn last_bytes(bytes: &[u8], max: usize) -> &[u8] {
    bytes.split_at(bytes.len().saturating_sub(max)).1
}

/// Lossy text of stdout (+ stderr under a separator), at most the last 64 KiB.
fn combined_text(result: &RunResult) -> String {
    let mut bytes = last_bytes(&result.stdout, OUTPUT_TEXT_BYTES).to_vec();
    if !result.stderr.is_empty() {
        bytes.extend_from_slice(STDERR_SEPARATOR.as_bytes());
        bytes.extend_from_slice(last_bytes(&result.stderr, OUTPUT_TEXT_BYTES));
    }
    let text = String::from_utf8_lossy(&bytes);
    tail_str(&text, OUTPUT_TEXT_BYTES).to_string()
}

fn run_gate(worktree: &Path, gate: &GateSpec) -> GateOutcome {
    let result = run_bounded(&RunSpec {
        program: gate.program.clone(),
        args: gate.args.clone(),
        cwd: worktree.to_path_buf(),
        env: gate.env.clone(),
        timeout_ms: gate.timeout_ms,
        max_output: MAX_GATE_OUTPUT,
    });
    let text = combined_text(&result);
    match gate.format {
        GateFormat::Exit => evaluate_exit(&gate.name, &result.exit, &text),
        GateFormat::RefcheckJson => {
            let parsed = crate::json::parse_value(&result.stdout, MAX_GATE_OUTPUT);
            evaluate_refcheck(
                &gate.name,
                parsed.as_ref().map_err(|e| e.as_str()),
                &result.exit,
                &text,
            )
        }
    }
}

fn command_summary(exit: &ExitKind) -> String {
    match exit {
        ExitKind::Code(n) => format!("exit code {n}"),
        ExitKind::Signal(n) => format!("killed by signal {n}"),
        ExitKind::TimedOut => "timed out".to_string(),
        ExitKind::OutputLimit => "output limit".to_string(),
        ExitKind::SpawnFailed(why) => format!("could not start: {}", sanitize_text(why.as_bytes())),
    }
}

/// Runs a job to completion on the calling thread.
fn execute(job: &Job) -> JobDone {
    match job {
        Job::Gates {
            run,
            task,
            attempt,
            worktree,
            gates,
        } => {
            let dir = PathBuf::from(worktree);
            // Every gate runs, also after a failure: the retry brief lists all of them.
            let outcomes = gates.iter().map(|gate| run_gate(&dir, gate)).collect();
            JobDone::Gates {
                run: run.clone(),
                task: task.clone(),
                attempt: *attempt,
                outcomes,
            }
        }
        Job::Command {
            approval,
            worktree,
            program,
            args,
            env,
            timeout_ms,
        } => {
            let result = run_bounded(&RunSpec {
                program: program.clone(),
                args: args.clone(),
                cwd: PathBuf::from(worktree),
                env: env.clone(),
                timeout_ms: *timeout_ms,
                max_output: MAX_GATE_OUTPUT,
            });
            let text = sanitize_text(combined_text(&result).as_bytes());
            JobDone::Command {
                approval: approval.clone(),
                exit_code: match result.exit {
                    ExitKind::Code(n) => Some(i64::from(n)),
                    _ => None,
                },
                summary: command_summary(&result.exit),
                output: tail_str(&text, COMMAND_OUTPUT_BYTES).to_string(),
            }
        }
    }
}

/// The result reported when a job's thread panicked: a failure, never silence.
fn panic_result(job: &Job) -> JobDone {
    const WHY: &str = "internal error: the job panicked";
    match job {
        Job::Gates {
            run,
            task,
            attempt,
            gates,
            ..
        } => JobDone::Gates {
            run: run.clone(),
            task: task.clone(),
            attempt: *attempt,
            outcomes: gates
                .iter()
                .map(|g| GateOutcome {
                    name: g.name.clone(),
                    passed: false,
                    summary: WHY.to_string(),
                    output: String::new(),
                })
                .collect(),
        },
        Job::Command { approval, .. } => JobDone::Command {
            approval: approval.clone(),
            exit_code: None,
            summary: WHY.to_string(),
            output: String::new(),
        },
    }
}

/// Decrements the running counter when the job thread ends, however it ends.
struct RunningGuard(Arc<AtomicUsize>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One OS thread per job, at most `max_concurrent` at a time. Results are delivered through a
/// channel and collected by [`JobRunner::poll`].
pub struct ThreadJobRunner {
    max_concurrent: usize,
    running: Arc<AtomicUsize>,
    tx: Sender<JobDone>,
    rx: Receiver<JobDone>,
}

impl ThreadJobRunner {
    pub fn new(max_concurrent: usize) -> ThreadJobRunner {
        let (tx, rx) = channel();
        ThreadJobRunner {
            max_concurrent,
            running: Arc::new(AtomicUsize::new(0)),
            tx,
            rx,
        }
    }

    fn start_with(&mut self, job: Job, exec: fn(&Job) -> JobDone) -> Result<()> {
        // Only this method increments, so the check and the increment cannot race.
        if self.running.load(Ordering::SeqCst) >= self.max_concurrent {
            return Err(FleetError::limit(format!(
                "{} jobs are already running",
                self.max_concurrent
            )));
        }
        self.running.fetch_add(1, Ordering::SeqCst);
        let counter = Arc::clone(&self.running);
        let tx = self.tx.clone();
        let spawned = thread::Builder::new().name("fleet-job".into()).spawn(move || {
            let guard = RunningGuard(counter);
            let done = catch_unwind(AssertUnwindSafe(|| exec(&job)))
                .unwrap_or_else(|_| panic_result(&job));
            // The result is queued before the job stops counting as running.
            let _ = tx.send(done);
            drop(guard);
        });
        if let Err(e) = spawned {
            // The closure never ran, so its guard did not decrement.
            self.running.fetch_sub(1, Ordering::SeqCst);
            return Err(FleetError::io("could not start a job thread", &e));
        }
        Ok(())
    }
}

impl JobRunner for ThreadJobRunner {
    fn start(&mut self, job: Job) -> Result<()> {
        self.start_with(job, execute)
    }

    fn poll(&mut self) -> Vec<JobDone> {
        self.rx.try_iter().collect()
    }

    fn running(&self) -> usize {
        self.running.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{wait_until, TempDir};
    use fleet_domain::ids::{ApprovalId, RunId, TaskId};
    use std::process::Command as StdCommand;

    const SAFE_PATH: &str = "/usr/bin:/bin";
    const CAP: usize = 64 * 1024;

    fn env_path() -> Vec<(String, String)> {
        vec![("PATH".to_string(), SAFE_PATH.to_string())]
    }

    fn spec(program: &Path, args: &[&str], dir: &TempDir) -> RunSpec {
        RunSpec {
            program: program.display().to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: dir.path().to_path_buf(),
            env: env_path(),
            timeout_ms: 10_000,
            max_output: CAP,
        }
    }

    fn run_script(body: &str) -> RunResult {
        let dir = TempDir::new("job-run");
        let script = dir.script("s.sh", &format!("#!/bin/sh\n{body}\n"));
        run_bounded(&spec(&script, &[], &dir))
    }

    /// A process that was killed but not yet reaped (a zombie, which an init-less container never
    /// reaps) is not running.
    fn process_alive(pid: &str) -> bool {
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            return stat.rsplit_once(") ").is_some_and(|(_, rest)| !rest.starts_with('Z'));
        }
        StdCommand::new("kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn read_pid(dir: &TempDir, rel: &str) -> Option<String> {
        let text = dir.read(rel);
        let pid = text.trim();
        (!pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit())).then(|| pid.to_string())
    }

    // ---- run_bounded -------------------------------------------------------------------------

    #[test]
    fn exit_codes_and_separate_streams_are_captured() {
        let ok = run_script("echo out; echo err >&2; exit 0");
        assert_eq!(ok.exit, ExitKind::Code(0));
        assert_eq!(ok.stdout, b"out\n");
        assert_eq!(ok.stderr, b"err\n");
        let bad = run_script("exit 7");
        assert_eq!(bad.exit, ExitKind::Code(7));
    }

    #[test]
    fn death_by_signal_is_reported_as_signal() {
        assert_eq!(run_script("kill -9 $$").exit, ExitKind::Signal(9));
    }

    #[test]
    fn stdin_is_null() {
        let r = run_script("if read line; then echo got; else echo eof; fi");
        assert_eq!(r.stdout, b"eof\n");
    }

    #[test]
    fn timeout_kills_the_group_including_a_term_ignoring_grandchild() {
        let dir = TempDir::new("job-timeout");
        let pidfile = dir.join("grandchild.pid");
        let script = dir.script(
            "s.sh",
            "#!/bin/sh\nsh -c 'trap \"\" TERM; echo $$ > \"$1\"; sleep 60' x \"$1\" &\nwait\n",
        );
        let mut s = spec(&script, &[pidfile.to_str().unwrap()], &dir);
        s.timeout_ms = 2_000;
        let started = Instant::now();
        let result = run_bounded(&s);
        assert_eq!(result.exit, ExitKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(8));
        let pid = read_pid(&dir, "grandchild.pid").expect("grandchild wrote its pid");
        assert!(
            wait_until(5_000, || !process_alive(&pid)),
            "grandchild {pid} survived the timeout"
        );
    }

    #[test]
    fn leftover_group_members_are_killed_after_the_leader_exits() {
        let dir = TempDir::new("job-leftover");
        let script = dir.script(
            "s.sh",
            "#!/bin/sh\nsleep 60 &\necho $! > \"$1\"\nexit 0\n",
        );
        let pidfile = dir.join("bg.pid");
        let result = run_bounded(&spec(&script, &[pidfile.to_str().unwrap()], &dir));
        assert_eq!(result.exit, ExitKind::Code(0));
        let pid = read_pid(&dir, "bg.pid").expect("background job wrote its pid");
        assert!(
            wait_until(5_000, || !process_alive(&pid)),
            "background process {pid} outlived its job"
        );
    }

    #[test]
    fn stdout_flood_is_stopped_at_the_cap() {
        let dir = TempDir::new("job-flood");
        let script = dir.script("s.sh", "#!/bin/sh\nexec yes\n");
        let started = Instant::now();
        let result = run_bounded(&spec(&script, &[], &dir));
        assert_eq!(result.exit, ExitKind::OutputLimit);
        assert!(result.stdout.len() <= CAP);
        assert!(!result.stdout.is_empty());
        assert!(started.elapsed() < Duration::from_secs(8));
    }

    #[test]
    fn stderr_flood_is_stopped_at_the_cap() {
        let dir = TempDir::new("job-flood-err");
        let script = dir.script("s.sh", "#!/bin/sh\nexec yes >&2\n");
        let result = run_bounded(&spec(&script, &[], &dir));
        assert_eq!(result.exit, ExitKind::OutputLimit);
        assert!(result.stderr.len() <= CAP);
    }

    #[test]
    fn output_exactly_at_the_cap_is_fine_one_more_byte_is_not() {
        let dir = TempDir::new("job-cap-edge");
        let script = dir.script("s.sh", "#!/bin/sh\nprintf '%s' \"$1\"\n");
        let mut s = spec(&script, &["0123456789"], &dir);
        s.max_output = 10;
        let exact = run_bounded(&s);
        assert_eq!(exact.exit, ExitKind::Code(0));
        assert_eq!(exact.stdout, b"0123456789");
        let mut s = spec(&script, &["0123456789X"], &dir);
        s.max_output = 10;
        let over = run_bounded(&s);
        assert_eq!(over.exit, ExitKind::OutputLimit);
        assert_eq!(over.stdout, b"0123456789");
    }

    #[test]
    fn environment_is_exactly_the_given_one() {
        let dir = TempDir::new("job-env");
        let script = dir.script("s.sh", "#!/bin/sh\nenv | sort > \"$1\"\n");
        let outfile = dir.join("env.txt");
        let mut s = spec(&script, &[outfile.to_str().unwrap()], &dir);
        s.env.push(("FLEET_TEST_VALUE".into(), "one two".into()));
        let result = run_bounded(&s);
        assert_eq!(result.exit, ExitKind::Code(0));
        let text = dir.read("env.txt");
        // The shell itself may add these; nothing else may appear.
        let shell_added = ["PWD", "OLDPWD", "SHLVL", "_", "__CF_USER_TEXT_ENCODING"];
        let keys: Vec<&str> = text
            .lines()
            .filter_map(|l| l.split_once('=').map(|(k, _)| k))
            .collect();
        assert!(keys.contains(&"PATH") && keys.contains(&"FLEET_TEST_VALUE"));
        for key in keys {
            assert!(
                key == "PATH" || key == "FLEET_TEST_VALUE" || shell_added.contains(&key),
                "unexpected variable {key} reached the child"
            );
        }
        assert!(text.contains("FLEET_TEST_VALUE=one two"));
    }

    #[test]
    fn program_is_resolved_in_the_given_path_only() {
        let dir = TempDir::new("job-resolve");
        dir.script("bin/mytool", "#!/bin/sh\necho mine\n");
        let bin = dir.join("bin");
        let mut s = spec(Path::new("mytool"), &[], &dir);
        s.env = vec![("PATH".into(), bin.display().to_string())];
        let found = run_bounded(&s);
        assert_eq!(found.exit, ExitKind::Code(0));
        assert_eq!(found.stdout, b"mine\n");

        // `sh` exists on this host's PATH but not in the child's: it must not be found.
        let mut s = spec(Path::new("sh"), &["-c", "true"], &dir);
        s.env = vec![("PATH".into(), bin.display().to_string())];
        assert!(matches!(run_bounded(&s).exit, ExitKind::SpawnFailed(_)));
        s.env = Vec::new();
        assert!(matches!(run_bounded(&s).exit, ExitKind::SpawnFailed(_)));
    }

    #[test]
    fn resolve_program_rules() {
        let dir = TempDir::new("job-resolve-rules");
        dir.script("bin/tool", "#!/bin/sh\n");
        dir.write("bin/plain", "not executable");
        dir.write("bin/adir/x", "");
        dir.script("other/tool", "#!/bin/sh\n");
        let env = |p: String| vec![("PATH".to_string(), p)];
        let bin = dir.join("bin");
        let other = dir.join("other");

        let first_wins = env(format!("{}:{}", bin.display(), other.display()));
        assert_eq!(
            resolve_program("tool", dir.path(), &first_wins),
            Some(bin.join("tool"))
        );
        // Entries that are missing or empty are skipped, later ones still searched.
        let skipping = env(format!("::/nonexistent:{}", other.display()));
        assert_eq!(
            resolve_program("tool", dir.path(), &skipping),
            Some(other.join("tool"))
        );
        let in_bin = env(bin.display().to_string());
        assert_eq!(resolve_program("plain", dir.path(), &in_bin), None);
        assert_eq!(resolve_program("adir", dir.path(), &in_bin), None);
        assert_eq!(resolve_program("", dir.path(), &in_bin), None);
        assert_eq!(resolve_program("tool", dir.path(), &[]), None);
        // With a slash the PATH is irrelevant and relative names resolve against cwd.
        assert_eq!(
            resolve_program("bin/tool", dir.path(), &[]),
            Some(bin.join("tool"))
        );
        assert_eq!(resolve_program("bin/plain", dir.path(), &[]), None);
        assert_eq!(
            resolve_program(&other.join("tool").display().to_string(), Path::new("/"), &[]),
            Some(other.join("tool"))
        );
    }

    #[test]
    fn spawn_failures_are_reported_not_raised() {
        let dir = TempDir::new("job-spawn");
        let missing = run_bounded(&spec(Path::new("/nonexistent/prog"), &[], &dir));
        assert!(matches!(missing.exit, ExitKind::SpawnFailed(_)));
        let plain = dir.write("plain.txt", "#!/bin/sh\n");
        let not_exec = run_bounded(&spec(&plain, &[], &dir));
        assert!(matches!(not_exec.exit, ExitKind::SpawnFailed(_)));
        let script = dir.script("s.sh", "#!/bin/sh\n");
        let mut s = spec(&script, &[], &dir);
        s.cwd = dir.join("no-such-dir");
        assert!(matches!(run_bounded(&s).exit, ExitKind::SpawnFailed(_)));
        assert!(missing.stdout.is_empty() && missing.stderr.is_empty());
    }

    #[test]
    fn arguments_are_not_interpreted_by_a_shell() {
        let dir = TempDir::new("job-noshell");
        let script = dir.script("s.sh", "#!/bin/sh\nprintf '%s|' \"$@\"\n");
        let hostile = "a b; touch pwned $(touch pwned2) `touch pwned3`";
        let result = run_bounded(&spec(&script, &[hostile, "*"], &dir));
        assert_eq!(result.stdout, format!("{hostile}|*|").into_bytes());
        assert!(!dir.exists("pwned") && !dir.exists("pwned2") && !dir.exists("pwned3"));
    }

    // ---- gates ---------------------------------------------------------------------------------

    fn gate(dir: &TempDir, name: &str, body: &str, format: GateFormat) -> GateSpec {
        let script = dir.script(&format!("{name}.sh"), &format!("#!/bin/sh\n{body}\n"));
        GateSpec {
            name: name.to_string(),
            program: script.display().to_string(),
            args: Vec::new(),
            env: env_path(),
            format,
            timeout_ms: 10_000,
        }
    }

    fn run_one(body: &str, format: GateFormat) -> GateOutcome {
        let dir = TempDir::new("job-gate");
        let g = gate(&dir, "g", body, format);
        run_gate(dir.path(), &g)
    }

    #[test]
    fn exit_gates_pass_on_zero_and_fail_on_one() {
        let pass = run_one("echo fine", GateFormat::Exit);
        assert!(pass.passed, "{pass:?}");
        assert!(pass.output.contains("fine"));
        let fail = run_one("echo boom; echo detail >&2; exit 1", GateFormat::Exit);
        assert!(!fail.passed);
        assert_eq!(fail.summary, "exit code 1");
        assert!(fail.output.contains("boom") && fail.output.contains("detail"));
    }

    #[test]
    fn gate_output_is_sanitized() {
        let o = run_one("printf 'ok\\033]52;c;aGk=\\007 done\\n'; exit 1", GateFormat::Exit);
        assert!(!o.output.contains('\u{1b}') && !o.output.contains('\u{7}'));
    }

    const CLEAN_DOC: &str = r#"echo '{"version":1,"summary":{"errors":0},"findings":[]}'"#;

    #[test]
    fn refcheck_passing_document_passes() {
        let o = run_one(CLEAN_DOC, GateFormat::RefcheckJson);
        assert!(o.passed, "{o:?}");
    }

    #[test]
    fn refcheck_document_with_errors_fails_with_findings() {
        let body = r#"echo '{"version":1,"summary":{"errors":1},"findings":[{"severity":"error","file":"a.rs","line":3,"message":"broken link"}]}'; exit 1"#;
        let o = run_one(body, GateFormat::RefcheckJson);
        assert!(!o.passed);
        assert_eq!(o.summary, "1 error(s)");
        assert!(o.output.contains("broken link"), "{o:?}");
    }

    #[test]
    fn refcheck_unparseable_output_fails() {
        let o = run_one("echo 'this is not json'", GateFormat::RefcheckJson);
        assert!(!o.passed);
        assert!(o.summary.contains("not valid JSON"), "{o:?}");
        assert!(o.output.contains("this is not json"));
    }

    #[test]
    fn refcheck_wrong_shape_fails() {
        let body = r#"echo '{"version":1,"summary":{"errors":"none"},"findings":[]}'"#;
        let o = run_one(body, GateFormat::RefcheckJson);
        assert!(!o.passed);
        assert!(o.summary.contains("refcheck contract"), "{o:?}");
    }

    #[test]
    fn refcheck_tool_killed_by_a_signal_fails_even_with_clean_stdout() {
        let body = format!("{CLEAN_DOC}\nkill -9 $$");
        let o = run_one(&body, GateFormat::RefcheckJson);
        assert!(!o.passed);
        assert_eq!(o.summary, "killed by signal 9");
    }

    #[test]
    fn refcheck_exit_code_contradicting_the_report_fails() {
        let body = format!("{CLEAN_DOC}\nexit 1");
        let o = run_one(&body, GateFormat::RefcheckJson);
        assert!(!o.passed);
        assert!(o.summary.contains("contradicts"), "{o:?}");
    }

    #[test]
    fn gate_timeout_and_spawn_failure_fail_the_gate() {
        let dir = TempDir::new("job-gate-bad");
        let mut slow = gate(&dir, "slow", "sleep 30", GateFormat::Exit);
        slow.timeout_ms = 300;
        assert_eq!(run_gate(dir.path(), &slow).summary, "timed out");
        let mut missing = gate(&dir, "missing", "", GateFormat::Exit);
        missing.program = "/nonexistent/prog".to_string();
        let o = run_gate(dir.path(), &missing);
        assert!(!o.passed);
        assert!(o.summary.starts_with("could not start"), "{o:?}");
    }

    #[test]
    fn all_gates_run_even_when_the_first_fails() {
        let dir = TempDir::new("job-all-gates");
        let marker = dir.join("second-ran");
        let gates = vec![
            gate(&dir, "first", "exit 1", GateFormat::Exit),
            gate(
                &dir,
                "second",
                &format!("echo ran > '{}'", marker.display()),
                GateFormat::Exit,
            ),
            gate(&dir, "third", "exit 2", GateFormat::Exit),
        ];
        let job = Job::Gates {
            run: RunId::parse("r1").unwrap(),
            task: TaskId::parse("t1").unwrap(),
            attempt: 3,
            worktree: dir.path().display().to_string(),
            gates,
        };
        let JobDone::Gates {
            run,
            task,
            attempt,
            outcomes,
        } = execute(&job)
        else {
            panic!("expected a gates result");
        };
        assert_eq!((run.as_str(), task.as_str(), attempt), ("r1", "t1", 3));
        let shape: Vec<(&str, bool)> = outcomes.iter().map(|o| (o.name.as_str(), o.passed)).collect();
        assert_eq!(shape, [("first", false), ("second", true), ("third", false)]);
        assert!(dir.exists("second-ran"));
    }

    #[test]
    fn gates_run_inside_the_worktree() {
        let dir = TempDir::new("job-cwd");
        let work = dir.join("wt");
        std::fs::create_dir_all(&work).unwrap();
        let g = gate(&dir, "pwd", "echo here > marker", GateFormat::Exit);
        let o = run_gate(&work, &g);
        assert!(o.passed);
        assert!(work.join("marker").exists());
    }

    // ---- command jobs --------------------------------------------------------------------------

    fn command_job(dir: &TempDir, body: &str, timeout_ms: u64) -> Job {
        let script = dir.script("cmd.sh", &format!("#!/bin/sh\n{body}\n"));
        Job::Command {
            approval: ApprovalId::parse("a7").unwrap(),
            worktree: dir.path().display().to_string(),
            program: script.display().to_string(),
            args: Vec::new(),
            env: env_path(),
            timeout_ms,
        }
    }

    #[test]
    fn command_job_reports_exit_code_and_sanitized_output() {
        let dir = TempDir::new("job-cmd");
        let job = command_job(&dir, "printf 'hello\\033[31m red\\033]0;title\\007\\n'; echo oops >&2; exit 3", 10_000);
        let JobDone::Command {
            approval,
            exit_code,
            summary,
            output,
        } = execute(&job)
        else {
            panic!("expected a command result");
        };
        assert_eq!(approval.as_str(), "a7");
        assert_eq!(exit_code, Some(3));
        assert_eq!(summary, "exit code 3");
        assert!(output.contains("hello") && output.contains("oops"));
        assert!(!output.contains('\u{1b}') && !output.contains('\u{7}'));
    }

    #[test]
    fn command_job_output_is_a_bounded_tail() {
        let dir = TempDir::new("job-cmd-tail");
        let job = command_job(&dir, "i=0; while [ $i -lt 2000 ]; do echo line-$i-padding-padding-padding; i=$((i+1)); done; echo THE-END", 20_000);
        let JobDone::Command { output, .. } = execute(&job) else {
            panic!("expected a command result");
        };
        assert!(output.len() <= COMMAND_OUTPUT_BYTES);
        assert!(output.trim_end().ends_with("THE-END"));
    }

    #[test]
    fn command_job_timeout_and_spawn_failure_have_no_exit_code() {
        let dir = TempDir::new("job-cmd-fail");
        let JobDone::Command {
            exit_code, summary, ..
        } = execute(&command_job(&dir, "sleep 30", 300))
        else {
            panic!("expected a command result");
        };
        assert_eq!((exit_code, summary.as_str()), (None, "timed out"));

        let JobDone::Command {
            exit_code, summary, ..
        } = execute(&command_job(&dir, "kill -9 $$", 10_000))
        else {
            panic!("expected a command result");
        };
        assert_eq!((exit_code, summary.as_str()), (None, "killed by signal 9"));

        let job = Job::Command {
            approval: ApprovalId::parse("a1").unwrap(),
            worktree: dir.path().display().to_string(),
            program: "/nonexistent/prog".to_string(),
            args: Vec::new(),
            env: Vec::new(),
            timeout_ms: 1_000,
        };
        let JobDone::Command {
            exit_code, summary, ..
        } = execute(&job)
        else {
            panic!("expected a command result");
        };
        assert_eq!(exit_code, None);
        assert!(summary.starts_with("could not start: "), "{summary}");
    }

    #[test]
    fn command_summary_texts() {
        assert_eq!(command_summary(&ExitKind::Code(0)), "exit code 0");
        assert_eq!(command_summary(&ExitKind::Signal(15)), "killed by signal 15");
        assert_eq!(command_summary(&ExitKind::TimedOut), "timed out");
        assert_eq!(command_summary(&ExitKind::OutputLimit), "output limit");
        let shown = command_summary(&ExitKind::SpawnFailed("no \u{1b}[31msuch".into()));
        assert!(shown.starts_with("could not start: no "), "{shown}");
        assert!(!shown.contains('\u{1b}'));
        assert!(shown.contains("such"));
    }

    // ---- runner ----------------------------------------------------------------------------------

    fn gates_job(dir: &TempDir, gates: Vec<GateSpec>) -> Job {
        Job::Gates {
            run: RunId::parse("r1").unwrap(),
            task: TaskId::parse("t1").unwrap(),
            attempt: 1,
            worktree: dir.path().display().to_string(),
            gates,
        }
    }

    fn collect(runner: &mut ThreadJobRunner, want: usize) -> Vec<JobDone> {
        let mut got = Vec::new();
        assert!(
            wait_until(15_000, || {
                got.extend(runner.poll());
                got.len() >= want
            }),
            "expected {want} results, got {}",
            got.len()
        );
        got
    }

    #[test]
    fn runner_delivers_results_through_poll() {
        let dir = TempDir::new("job-runner");
        let gates = vec![
            gate(&dir, "bad", "exit 1", GateFormat::Exit),
            gate(&dir, "good", "exit 0", GateFormat::Exit),
        ];
        let mut runner = ThreadJobRunner::new(2);
        assert!(runner.poll().is_empty());
        runner.start(gates_job(&dir, gates)).unwrap();
        let got = collect(&mut runner, 1);
        let JobDone::Gates { outcomes, .. } = &got[0] else {
            panic!("expected a gates result");
        };
        assert_eq!(outcomes.len(), 2);
        assert!(!outcomes[0].passed && outcomes[1].passed);
        assert!(wait_until(5_000, || runner.running() == 0));
        assert!(runner.poll().is_empty());
    }

    #[test]
    fn runner_refuses_more_jobs_than_its_limit_and_recovers() {
        let dir = TempDir::new("job-runner-limit");
        let release = dir.join("release");
        let waiting = |name: &str| {
            let mut g = gate(
                &dir,
                name,
                "while [ ! -e \"$1\" ]; do sleep 0.05; done",
                GateFormat::Exit,
            );
            g.args = vec![release.display().to_string()];
            g
        };
        let mut runner = ThreadJobRunner::new(1);
        runner.start(gates_job(&dir, vec![waiting("a")])).unwrap();
        assert_eq!(runner.running(), 1);
        let err = runner
            .start(gates_job(&dir, vec![waiting("b")]))
            .unwrap_err();
        assert_eq!(err.code, fleet_domain::error::ErrorCode::Limit);
        assert_eq!(runner.running(), 1);

        dir.write("release", "");
        let got = collect(&mut runner, 1);
        assert_eq!(got.len(), 1);
        assert!(wait_until(5_000, || runner.running() == 0));
        runner
            .start(gates_job(&dir, vec![gate(&dir, "c", "exit 0", GateFormat::Exit)]))
            .unwrap();
        assert_eq!(collect(&mut runner, 1).len(), 1);
    }

    #[test]
    fn runner_with_zero_slots_refuses_everything() {
        let dir = TempDir::new("job-runner-zero");
        let mut runner = ThreadJobRunner::new(0);
        let err = runner.start(gates_job(&dir, Vec::new())).unwrap_err();
        assert_eq!(err.code, fleet_domain::error::ErrorCode::Limit);
        assert_eq!(runner.running(), 0);
    }

    fn exploding(_: &Job) -> JobDone {
        panic!("simulated failure inside a job");
    }

    #[test]
    fn a_panicking_job_yields_a_failure_and_releases_its_slot() {
        let dir = TempDir::new("job-runner-panic");
        let mut runner = ThreadJobRunner::new(1);
        let job = gates_job(
            &dir,
            vec![
                gate(&dir, "one", "exit 0", GateFormat::Exit),
                gate(&dir, "two", "exit 0", GateFormat::Exit),
            ],
        );
        runner.start_with(job, exploding).unwrap();
        let got = collect(&mut runner, 1);
        let JobDone::Gates { outcomes, attempt, .. } = &got[0] else {
            panic!("expected a gates result");
        };
        assert_eq!(*attempt, 1);
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|o| !o.passed && o.summary.contains("panicked")));
        assert!(wait_until(5_000, || runner.running() == 0));

        // A leaked slot would make this fail with a limit error.
        let cmd = command_job(&dir, "exit 0", 5_000);
        runner.start_with(cmd, exploding).unwrap();
        let got = collect(&mut runner, 1);
        let JobDone::Command { approval, exit_code, summary, .. } = &got[0] else {
            panic!("expected a command result");
        };
        assert_eq!(approval.as_str(), "a7");
        assert_eq!(*exit_code, None);
        assert!(summary.contains("panicked"));
        assert!(wait_until(5_000, || runner.running() == 0));
    }

    #[test]
    fn runner_runs_command_jobs() {
        let dir = TempDir::new("job-runner-cmd");
        let mut runner = ThreadJobRunner::new(2);
        runner.start(command_job(&dir, "echo hi; exit 4", 10_000)).unwrap();
        let got = collect(&mut runner, 1);
        let JobDone::Command { exit_code, summary, output, .. } = &got[0] else {
            panic!("expected a command result");
        };
        assert_eq!(*exit_code, Some(4));
        assert_eq!(summary, "exit code 4");
        assert_eq!(output.trim(), "hi");
    }
}
