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
use fleet_domain::redact::{Redactor, StreamRedactor};
use fleet_domain::sanitize::{sanitize_text, tail_str};
use fleet_domain::supervision::LiveSession;
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

/// The process groups of the children the daemon has started on behalf of workers and not yet
/// cleaned up: quality gates, approved commands, verification checks, the undo tool and git. The
/// daemon treats every process in one of them, and every process one of them started (also one
/// that left the group: see [`Sweeper`]), like a worker: the code these run is made of files a
/// worker wrote, or is steered by them. There is one tracker per daemon, cloned into every
/// adapter that starts children (clones share the list), and it is what both the control socket
/// and the web dashboard consult, through `JobRunner::live`, next to the live worker sessions.
#[derive(Clone, Default)]
pub struct JobTracker(Arc<Mutex<Vec<(u32, u32)>>>);

/// Most entries remembered. More than this many concurrent children and descendants is not a
/// normal daemon; the origin check still refuses everything below the daemon in the process tree.
const MAX_TRACKED: usize = 8192;

impl JobTracker {
    pub fn new() -> JobTracker {
        JobTracker::default()
    }

    /// The groups alive now (each child leads its own group, so pid and pgid are the same).
    pub fn live(&self) -> Vec<LiveSession> {
        lock(&self.0)
            .iter()
            .map(|(pid, pgid)| LiveSession {
                pid: *pid,
                pgid: *pgid,
            })
            .collect()
    }

    /// A child that leads its own group.
    pub(crate) fn track(&self, pid: u32) -> Tracked {
        self.track_entry(pid, pid)
    }

    /// A process of a job that is known only by its pid (a descendant that left the group).
    fn track_loose(&self, pid: u32) -> Tracked {
        self.track_entry(pid, 0)
    }

    fn track_entry(&self, pid: u32, pgid: u32) -> Tracked {
        {
            let mut all = lock(&self.0);
            if all.len() < MAX_TRACKED {
                all.push((pid, pgid));
            }
        }
        Tracked {
            tracker: self.clone(),
            entry: (pid, pgid),
        }
    }
}

/// Keeps a child registered until the job has dealt with its whole process group.
pub(crate) struct Tracked {
    tracker: JobTracker,
    entry: (u32, u32),
}

impl Drop for Tracked {
    fn drop(&mut self) {
        let mut all = lock(&self.tracker.0);
        if let Some(i) = all.iter().position(|p| *p == self.entry) {
            all.swap_remove(i);
        }
    }
}

/// How often the descendants of a running job child are looked up.
const SWEEP_INTERVAL: Duration = Duration::from_millis(200);

/// Follows the processes a job child starts, so that those that leave its process group
/// (`setsid`, a double fork) can still be stopped when the check is over and are treated as
/// untrusted origin meanwhile.
///
/// What this does and does not cover: a descendant is found when a sample sees it below the child.
/// Samples are taken every 200 ms while the child runs (the first one 200 ms after it started)
/// and once more just before a child that ran out of time is stopped. There is none after a
/// normal exit: the child is gone by then, and so is the parent link that would lead to its
/// descendants. A process that detaches *and* whose whole chain of parents is gone before the next
/// sample is not found (`setsid` plus a parent that exits within the first 200 ms); neither is
/// one that starts after the last sample. The group kill of the child still takes every process
/// that stayed in the group. When the process table cannot be read (an unreadable `/proc`, a
/// `ps` that fails or hangs) the sweeper finds nothing: it says so once on stderr and carries on
/// with the group kill. Without cgroups or a sandbox there is no complete answer; run checks in a
/// container or as another user when that matters.
pub(crate) struct Sweeper<'a> {
    tracker: &'a JobTracker,
    root: u32,
    seen: Vec<(u32, String, Tracked)>,
    last: Option<Instant>,
    /// The process table could not be read at some sample (reported once).
    blind: bool,
}

impl<'a> Sweeper<'a> {
    /// The first look is one interval after the child started: a process that was just started
    /// has no descendants yet, and most children (every git call) are over before then.
    pub(crate) fn new(tracker: &'a JobTracker, root: u32) -> Sweeper<'a> {
        Sweeper {
            tracker,
            root,
            seen: Vec::new(),
            last: Some(Instant::now()),
            blind: false,
        }
    }

    /// True once a sample could not read the process table: what escaped the group since then
    /// has not been seen.
    #[cfg(test)]
    pub(crate) fn is_blind(&self) -> bool {
        self.blind
    }

    /// Looks for new descendants if the last look is older than the interval (or `force`).
    pub(crate) fn sample(&mut self, force: bool) {
        self.sample_from(force, crate::procid::process_table);
    }

    fn sample_from(&mut self, force: bool, table: impl FnOnce() -> Option<Vec<(u32, u32)>>) {
        if !force && self.last.is_some_and(|t| t.elapsed() < SWEEP_INTERVAL) {
            return;
        }
        self.last = Some(Instant::now());
        let Some(table) = table() else {
            if !self.blind {
                self.blind = true;
                eprintln!(
                    "fleet: the process table could not be read; processes that leave the process group of a check or a gate are not followed (the group is still killed)"
                );
            }
            return;
        };
        for pid in crate::procid::descendants_in(&table, self.root) {
            if self.seen.iter().any(|(p, _, _)| *p == pid) {
                continue;
            }
            if let Some(token) = crate::procid::start_token(pid) {
                self.seen.push((pid, token, self.tracker.track_loose(pid)));
            }
        }
    }

    /// Kills every process seen that is still the same process, then forgets them.
    pub(crate) fn kill_all(&mut self) {
        for (pid, token, _) in &self.seen {
            crate::procid::kill_if_same(*pid, token);
        }
        self.seen.clear();
    }
}

/// What a job needs from the adapter besides its own description.
#[derive(Clone)]
pub struct JobContext {
    pub redactor: Arc<Redactor>,
    pub tracker: JobTracker,
    /// What verification jobs export commits with and where; `None` where there is none (such a
    /// job then fails every check instead of running it in the worktree).
    pub verify: Option<Arc<crate::verify::VerifyHost>>,
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
/// Empty and relative `PATH` entries are skipped: a relative entry would resolve inside the
/// directory the child runs in, which a worker controls.
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
    let path = env
        .iter()
        .rev()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v)?;
    path.split(':')
        .map(Path::new)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable_file(candidate))
}

/// Name of the directory below a repository that holds the worktrees fleet creates.
const WORKTREES_DIR: &str = ".fleet-worktrees";

/// The repository root of a fleet worktree: the parent of its `.fleet-worktrees` ancestor.
fn repo_root_of(worktree: &Path) -> Option<PathBuf> {
    worktree
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == WORKTREES_DIR))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

/// Checks that `program` is not something a worker could have planted: its canonical path must
/// lie neither inside `worktree` nor inside the repository the worktree belongs to (the parent
/// of the `.fleet-worktrees` ancestor). Returns the canonical path to run, or the reason it was
/// refused.
pub fn trusted_program(program: &Path, worktree: &Path) -> std::result::Result<PathBuf, String> {
    if !program.is_absolute() {
        return Err("could not start: the program path must be absolute".to_string());
    }
    let canonical = fs::canonicalize(program).map_err(|e| {
        format!(
            "could not start: the program cannot be resolved ({})",
            e.kind()
        )
    })?;
    let mut roots: Vec<PathBuf> = vec![worktree.to_path_buf()];
    roots.extend(repo_root_of(worktree));
    if let Ok(real) = fs::canonicalize(worktree) {
        roots.extend(repo_root_of(&real));
        roots.push(real);
    }
    let inside = roots.into_iter().any(|root| {
        let root = fs::canonicalize(&root).unwrap_or(root);
        canonical.starts_with(root)
    });
    if inside {
        return Err(
            "refused: the program lies inside the task's worktree or its repository, where the \
             worker controls its contents; the command was not run"
                .to_string(),
        );
    }
    Ok(canonical)
}

/// Redacts a multi-line text line by line; lines of a private-key block are dropped.
pub fn redact_block(redactor: &Arc<Redactor>, text: &str) -> String {
    let mut stream = StreamRedactor::new(Arc::clone(redactor));
    let kept: Vec<_> = text.split('\n').filter_map(|l| stream.line(l)).collect();
    kept.join("\n")
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
    run_bounded_tracked(spec, &JobTracker::new())
}

/// [`run_bounded`], with the child's process group registered in `tracker` from the moment the
/// child exists until its group was killed and the output readers are done.
pub fn run_bounded_tracked(spec: &RunSpec, tracker: &JobTracker) -> RunResult {
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
    let _tracked = tracker.track(pid);
    let mut sweeper = Sweeper::new(tracker, pid);

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
            sweeper.sample(true);
            stop_gracefully(&mut child, pid);
            break ExitKind::TimedOut;
        }
        sweeper.sample(false);
        thread::sleep(POLL_INTERVAL);
    };
    // Whatever left the group is stopped too; the group itself was killed above.
    sweeper.kill_all();

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
    RunResult {
        exit,
        stdout,
        stderr,
    }
}

/// Starts the command, retrying briefly while the executable is still open for writing in another
/// thread (`ETXTBSY`): a script that was just written can briefly be held open by a concurrent fork.
pub(crate) fn spawn_retrying_busy(command: &mut Command) -> std::io::Result<std::process::Child> {
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
/// Lossy text of stdout (+ stderr under a separator), redacted line by line over the *whole*
/// capture (at most 4 MiB per stream) and only then cut to the last 64 KiB. Cutting first would
/// let a token or a private-key block that spans the cut lose the part the redaction rules
/// recognize.
pub(crate) fn redacted_tail(result: &RunResult, redactor: &Arc<Redactor>) -> String {
    let mut text = redact_block(redactor, &String::from_utf8_lossy(&result.stdout));
    if !result.stderr.is_empty() {
        text.push_str(STDERR_SEPARATOR);
        text.push_str(&redact_block(
            redactor,
            &String::from_utf8_lossy(&result.stderr),
        ));
    }
    tail_str(&text, OUTPUT_TEXT_BYTES).to_string()
}

fn run_gate(worktree: &Path, gate: &GateSpec, ctx: &JobContext) -> GateOutcome {
    let redactor = &ctx.redactor;
    let result = run_bounded_tracked(
        &RunSpec {
            program: gate.program.clone(),
            args: gate.args.clone(),
            cwd: worktree.to_path_buf(),
            env: gate.env.clone(),
            timeout_ms: gate.timeout_ms,
            max_output: MAX_GATE_OUTPUT,
        },
        &ctx.tracker,
    );
    // Redacted before evaluation, and the whole capture before it is cut to its tail, so a cut
    // can neither leave half a secret behind nor hide the start of a key block.
    let text = redacted_tail(&result, redactor);
    let mut outcome = match gate.format {
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
    };
    // The summary of a structured gate comes from its stdout, not from `text`.
    outcome.summary = redact_block(redactor, &outcome.summary);
    outcome.output = redact_block(redactor, &outcome.output);
    outcome
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
fn execute(job: &Job, ctx: &JobContext) -> JobDone {
    match job {
        Job::Verify(v) => JobDone::Verify(Box::new(crate::verify::run_verify(v, ctx))),
        Job::Gates {
            run,
            task,
            attempt,
            worktree,
            gates,
        } => {
            let dir = PathBuf::from(worktree);
            // Every gate runs, also after a failure: the retry brief lists all of them.
            let outcomes = gates.iter().map(|gate| run_gate(&dir, gate, ctx)).collect();
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
            let program = match trusted_program(Path::new(program), Path::new(worktree)) {
                Ok(path) => path,
                Err(why) => {
                    return JobDone::Command {
                        approval: approval.clone(),
                        exit_code: None,
                        summary: why,
                        output: String::new(),
                    };
                }
            };
            let result = run_bounded_tracked(
                &RunSpec {
                    program: program.display().to_string(),
                    args: args.clone(),
                    cwd: PathBuf::from(worktree),
                    env: env.clone(),
                    timeout_ms: *timeout_ms,
                    max_output: MAX_GATE_OUTPUT,
                },
                &ctx.tracker,
            );
            let text = sanitize_text(redacted_tail(&result, &ctx.redactor).as_bytes());
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
        Job::Verify(v) => JobDone::Verify(Box::new(crate::verify::failed_run(v, WHY))),
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
    ctx: JobContext,
    running: Arc<AtomicUsize>,
    tx: Sender<JobDone>,
    rx: Receiver<JobDone>,
}

type JobFn = fn(&Job, &JobContext) -> JobDone;

impl ThreadJobRunner {
    /// `redactor` is applied to everything gates and commands print before it is reported.
    pub fn new(max_concurrent: usize, redactor: Arc<Redactor>) -> ThreadJobRunner {
        let (tx, rx) = channel();
        ThreadJobRunner {
            max_concurrent,
            ctx: JobContext {
                redactor,
                tracker: JobTracker::new(),
                verify: None,
            },
            running: Arc::new(AtomicUsize::new(0)),
            tx,
            rx,
        }
    }

    /// Lists the children of this runner in `tracker`, the one the daemon's other adapters share.
    pub fn with_tracker(mut self, tracker: JobTracker) -> ThreadJobRunner {
        self.ctx.tracker = tracker;
        self
    }

    /// Lets this runner run verification jobs: commits are exported with `host`.
    pub fn with_verification(mut self, host: crate::verify::VerifyHost) -> ThreadJobRunner {
        self.ctx.verify = Some(Arc::new(host));
        self
    }

    fn start_with(&mut self, job: Job, exec: JobFn) -> Result<()> {
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
        let ctx = self.ctx.clone();
        let spawned = thread::Builder::new()
            .name("fleet-job".into())
            .spawn(move || {
                let guard = RunningGuard(counter);
                let done = catch_unwind(AssertUnwindSafe(|| exec(&job, &ctx)))
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

    fn live(&self) -> Vec<LiveSession> {
        self.ctx.tracker.live()
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

    #[test]
    fn a_sweeper_that_cannot_read_the_process_table_is_blind_and_still_works_afterwards() {
        let tracker = JobTracker::default();
        let mut sweeper = Sweeper::new(&tracker, std::process::id());
        assert!(!sweeper.is_blind());
        sweeper.sample_from(true, || None);
        assert!(sweeper.is_blind(), "an unreadable table must be noticed");
        // it does not stop following: a later readable sample finds a child of the root
        let mut child = StdCommand::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        sweeper.sample_from(true, || Some(vec![(pid, std::process::id())]));
        assert_eq!(sweeper.seen.len(), 1);
        sweeper.kill_all();
        let _ = child.wait();
    }

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

    #[test]
    fn the_tracker_lists_a_running_group_leader_until_the_job_has_ended() {
        let dir = TempDir::new("job-groups");
        // the script publishes its own pid (the group leader) so the test knows what to look for
        let script = dir.script(
            "s.sh",
            &format!(
                "#!/bin/sh\necho $$ > {}/pid\nsleep 1\n",
                dir.path().display()
            ),
        );
        let tracker = JobTracker::new();
        let job = std::thread::spawn({
            let (s, t) = (spec(&script, &[], &dir), tracker.clone());
            move || run_bounded_tracked(&s, &t)
        });
        assert!(
            wait_until(10_000, || dir.exists("pid")),
            "the script started"
        );
        let leader: u32 = dir.read("pid").trim().parse().unwrap();
        assert!(
            tracker
                .live()
                .iter()
                .any(|l| l.pid == leader && l.pgid == leader),
            "listed while it runs: {:?}",
            tracker.live()
        );
        job.join().unwrap();
        assert!(tracker.live().is_empty(), "gone after it ended");
    }

    #[test]
    fn clones_of_a_tracker_share_one_list_and_it_is_bounded() {
        let tracker = JobTracker::new();
        let other = tracker.clone();
        let guards: Vec<Tracked> = (0..MAX_TRACKED + 20)
            .map(|i| other.track(4_000_000 + i as u32))
            .collect();
        assert_eq!(tracker.live().len(), MAX_TRACKED);
        drop(guards);
        assert!(
            tracker.live().is_empty(),
            "every guard removed its entry (or had none)"
        );
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
            return stat
                .rsplit_once(") ")
                .is_some_and(|(_, rest)| !rest.starts_with('Z'));
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
    fn a_descendant_that_left_the_group_is_killed_when_the_check_is_over() {
        if !Path::new("/usr/bin/setsid").exists() && !Path::new("/bin/setsid").exists() {
            return;
        }
        let dir = TempDir::new("job-setsid");
        // the grandchild starts a session of its own: the group kill cannot reach it
        let script = dir.script(
            "s.sh",
            "#!/bin/sh\nsetsid sh -c 'echo $$ > \"$1\"; sleep 60' x \"$1\" &\nsleep 1\nexit 0\n",
        );
        let pidfile = dir.join("detached.pid");
        let tracker = JobTracker::new();
        let result =
            run_bounded_tracked(&spec(&script, &[pidfile.to_str().unwrap()], &dir), &tracker);
        assert_eq!(result.exit, ExitKind::Code(0));
        let pid = read_pid(&dir, "detached.pid").expect("the detached process wrote its pid");
        assert!(
            wait_until(5_000, || !process_alive(&pid)),
            "the detached descendant {pid} survived"
        );
        assert!(tracker.live().is_empty(), "nothing stays registered");
    }

    #[test]
    fn a_detached_descendant_counts_as_worker_origin_while_it_runs() {
        if !Path::new("/usr/bin/setsid").exists() && !Path::new("/bin/setsid").exists() {
            return;
        }
        let dir = TempDir::new("job-setsid-live");
        let script = dir.script(
            "s.sh",
            "#!/bin/sh\nsetsid sh -c 'echo $$ > \"$1\"; sleep 60' x \"$1\" &\ni=0; while [ ! -f go ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done\nexit 0\n",
        );
        let pidfile = dir.join("detached.pid");
        let tracker = JobTracker::new();
        let t2 = tracker.clone();
        let mut s = spec(&script, &[pidfile.to_str().unwrap()], &dir);
        s.cwd = dir.path().to_path_buf();
        let handle = thread::spawn(move || run_bounded_tracked(&s, &t2));
        assert!(wait_until(10_000, || dir.exists("detached.pid")));
        let pid: u32 = dir.read("detached.pid").trim().parse().unwrap();
        assert!(
            wait_until(5_000, || tracker
                .live()
                .iter()
                .any(|l| l.pid == pid && l.pgid == 0)),
            "{:?}",
            tracker.live()
        );
        dir.write("go", "");
        handle.join().unwrap();
        assert!(tracker.live().is_empty());
    }

    #[test]
    fn leftover_group_members_are_killed_after_the_leader_exits() {
        let dir = TempDir::new("job-leftover");
        let script = dir.script("s.sh", "#!/bin/sh\nsleep 60 &\necho $! > \"$1\"\nexit 0\n");
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
        // Entries that are missing, empty or relative are skipped, later ones still searched.
        let skipping = env(format!("::bin:/nonexistent:{}", other.display()));
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
            resolve_program(
                &other.join("tool").display().to_string(),
                Path::new("/"),
                &[]
            ),
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

    fn rd() -> Arc<Redactor> {
        Arc::new(Redactor::builtin())
    }

    fn ctx() -> JobContext {
        JobContext {
            redactor: rd(),
            tracker: JobTracker::new(),
            verify: None,
        }
    }

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
        run_gate(dir.path(), &g, &ctx())
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
        let o = run_one(
            "printf 'ok\\033]52;c;aGk=\\007 done\\n'; exit 1",
            GateFormat::Exit,
        );
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
        assert_eq!(run_gate(dir.path(), &slow, &ctx()).summary, "timed out");
        let mut missing = gate(&dir, "missing", "", GateFormat::Exit);
        missing.program = "/nonexistent/prog".to_string();
        let o = run_gate(dir.path(), &missing, &ctx());
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
        } = execute(&job, &ctx())
        else {
            panic!("expected a gates result");
        };
        assert_eq!((run.as_str(), task.as_str(), attempt), ("r1", "t1", 3));
        let shape: Vec<(&str, bool)> = outcomes
            .iter()
            .map(|o| (o.name.as_str(), o.passed))
            .collect();
        assert_eq!(
            shape,
            [("first", false), ("second", true), ("third", false)]
        );
        assert!(dir.exists("second-ran"));
    }

    #[test]
    fn gates_run_inside_the_worktree() {
        let dir = TempDir::new("job-cwd");
        let work = dir.join("wt");
        std::fs::create_dir_all(&work).unwrap();
        let g = gate(&dir, "pwd", "echo here > marker", GateFormat::Exit);
        let o = run_gate(&work, &g, &ctx());
        assert!(o.passed);
        assert!(work.join("marker").exists());
    }

    // ---- command jobs --------------------------------------------------------------------------

    /// A command job whose program lives next to, not inside, the worktree.
    fn command_job(dir: &TempDir, body: &str, timeout_ms: u64) -> Job {
        let script = dir.script("cmd.sh", &format!("#!/bin/sh\n{body}\n"));
        dir.write("wt/.keep", "");
        Job::Command {
            approval: ApprovalId::parse("a7").unwrap(),
            worktree: dir.join("wt").display().to_string(),
            program: script.display().to_string(),
            args: Vec::new(),
            env: env_path(),
            timeout_ms,
        }
    }

    #[test]
    fn command_job_reports_exit_code_and_sanitized_output() {
        let dir = TempDir::new("job-cmd");
        let job = command_job(
            &dir,
            "printf 'hello\\033[31m red\\033]0;title\\007\\n'; echo oops >&2; exit 3",
            10_000,
        );
        let JobDone::Command {
            approval,
            exit_code,
            summary,
            output,
        } = execute(&job, &ctx())
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
        let JobDone::Command { output, .. } = execute(&job, &ctx()) else {
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
        } = execute(&command_job(&dir, "sleep 30", 300), &ctx())
        else {
            panic!("expected a command result");
        };
        assert_eq!((exit_code, summary.as_str()), (None, "timed out"));

        let JobDone::Command {
            exit_code, summary, ..
        } = execute(&command_job(&dir, "kill -9 $$", 10_000), &ctx())
        else {
            panic!("expected a command result");
        };
        assert_eq!((exit_code, summary.as_str()), (None, "killed by signal 9"));

        let job = Job::Command {
            approval: ApprovalId::parse("a1").unwrap(),
            worktree: dir.join("wt").display().to_string(),
            program: "/nonexistent/prog".to_string(),
            args: Vec::new(),
            env: Vec::new(),
            timeout_ms: 1_000,
        };
        let JobDone::Command {
            exit_code, summary, ..
        } = execute(&job, &ctx())
        else {
            panic!("expected a command result");
        };
        assert_eq!(exit_code, None);
        assert!(summary.starts_with("could not start: "), "{summary}");
    }

    #[test]
    fn command_summary_texts() {
        assert_eq!(command_summary(&ExitKind::Code(0)), "exit code 0");
        assert_eq!(
            command_summary(&ExitKind::Signal(15)),
            "killed by signal 15"
        );
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

    // ---- process groups of running jobs count as worker origin ------------------------------

    fn live_pids(runner: &ThreadJobRunner) -> Vec<(u32, u32)> {
        runner.live().iter().map(|l| (l.pid, l.pgid)).collect()
    }

    #[test]
    fn the_children_of_running_jobs_are_listed_until_their_group_is_gone() {
        let dir = TempDir::new("job-live");
        let mut runner = ThreadJobRunner::new(4, rd());
        assert!(runner.live().is_empty());
        // a command job whose child leaves a grandchild behind in the group, then waits
        let job = command_job(
            &dir,
            "echo $$ > pid; (sleep 5 &) ; i=0; while [ ! -f go ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done",
            20_000,
        );
        let Job::Command { worktree, .. } = &job else {
            panic!("a command job")
        };
        let wt = PathBuf::from(worktree);
        runner.start(job).unwrap();
        assert!(wait_until(10_000, || wt.join("pid").exists()));
        let pid: u32 = std::fs::read_to_string(wt.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let live = live_pids(&runner);
        assert!(
            live.contains(&(pid, pid)),
            "the group leader is registered: {live:?}"
        );
        std::fs::write(wt.join("go"), "").unwrap();
        collect(&mut runner, 1);
        // the result is queued before the guard of the child is dropped; both happen right away
        assert!(wait_until(5_000, || runner.live().is_empty()));
        assert!(
            !crate::procid::group_exists(pid),
            "the group was killed before it was forgotten"
        );
    }

    #[test]
    fn gates_and_several_jobs_at_once_are_all_listed() {
        let dir = TempDir::new("job-live-gates");
        let mut runner = ThreadJobRunner::new(4, rd());
        let slow = gate(
            &dir,
            "slow",
            "echo $$ > pid-slow; i=0; while [ ! -f go ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done",
            GateFormat::Exit,
        );
        runner.start(gates_job(&dir, vec![slow])).unwrap();
        assert!(wait_until(10_000, || dir.exists("pid-slow")));
        let pid: u32 = dir.read("pid-slow").trim().parse().unwrap();
        assert!(live_pids(&runner).contains(&(pid, pid)));
        dir.write("go", "");
        collect(&mut runner, 1);
        assert!(wait_until(5_000, || runner.live().is_empty()));
    }

    #[test]
    fn a_child_that_cannot_start_is_never_listed() {
        let tracker = JobTracker::new();
        let dir = TempDir::new("job-live-none");
        let r = run_bounded_tracked(&spec(Path::new("/nonexistent/prog"), &[], &dir), &tracker);
        assert!(matches!(r.exit, ExitKind::SpawnFailed(_)));
        assert!(tracker.live().is_empty());
    }

    // ---- redaction happens on the whole capture, before the tail is cut ---------------------------

    /// What the code did before: cut the last 64 KiB, then redact.
    fn legacy_cut_then_redact(result: &RunResult) -> String {
        let text = String::from_utf8_lossy(&result.stdout);
        redact_block(&rd(), tail_str(&text, OUTPUT_TEXT_BYTES))
    }

    #[test]
    fn a_token_that_spans_the_start_of_the_kept_tail_is_still_redacted() {
        let token = token();
        // the kept window starts 12 bytes into the token
        let keep_after_token = OUTPUT_TEXT_BYTES - (token.len() - 12) - 1;
        let result = RunResult {
            exit: ExitKind::Code(0),
            stdout: format!(
                "{}\n{token}{}\n",
                "x".repeat(1000),
                "y".repeat(keep_after_token)
            )
            .into_bytes(),
            stderr: Vec::new(),
        };
        let fragment = &token[12..];
        assert!(
            legacy_cut_then_redact(&result).contains(fragment),
            "the scenario must leak with the old order (it is the finding)"
        );
        let text = redacted_tail(&result, &rd());
        assert!(!text.contains(fragment), "{}", &text[..80.min(text.len())]);
        assert!(!text.contains(&token));
        assert!(text.len() <= OUTPUT_TEXT_BYTES);
    }

    #[test]
    fn a_private_key_block_that_starts_before_the_kept_tail_is_dropped_whole() {
        let body = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo=\n".repeat(40);
        let block = format!(
            "-----BEGIN {} KEY-----\n{body}-----END {} KEY-----\n",
            "PRIVATE", "PRIVATE"
        );
        let filler = "z".repeat(OUTPUT_TEXT_BYTES);
        let result = RunResult {
            exit: ExitKind::Code(0),
            stdout: format!("{filler}\n{block}after\n").into_bytes(),
            stderr: format!("{filler}\n{block}err-after\n").into_bytes(),
        };
        let text = redacted_tail(&result, &rd());
        assert!(!text.contains("QUJDREVG"), "{text}");
        assert!(text.contains("err-after"));
    }

    #[test]
    fn runner_delivers_results_through_poll() {
        let dir = TempDir::new("job-runner");
        let gates = vec![
            gate(&dir, "bad", "exit 1", GateFormat::Exit),
            gate(&dir, "good", "exit 0", GateFormat::Exit),
        ];
        let mut runner = ThreadJobRunner::new(2, rd());
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
        let mut runner = ThreadJobRunner::new(1, rd());
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
            .start(gates_job(
                &dir,
                vec![gate(&dir, "c", "exit 0", GateFormat::Exit)],
            ))
            .unwrap();
        assert_eq!(collect(&mut runner, 1).len(), 1);
    }

    #[test]
    fn runner_with_zero_slots_refuses_everything() {
        let dir = TempDir::new("job-runner-zero");
        let mut runner = ThreadJobRunner::new(0, rd());
        let err = runner.start(gates_job(&dir, Vec::new())).unwrap_err();
        assert_eq!(err.code, fleet_domain::error::ErrorCode::Limit);
        assert_eq!(runner.running(), 0);
    }

    fn exploding(_: &Job, _: &JobContext) -> JobDone {
        panic!("simulated failure inside a job");
    }

    #[test]
    fn a_panicking_job_yields_a_failure_and_releases_its_slot() {
        let dir = TempDir::new("job-runner-panic");
        let mut runner = ThreadJobRunner::new(1, rd());
        let job = gates_job(
            &dir,
            vec![
                gate(&dir, "one", "exit 0", GateFormat::Exit),
                gate(&dir, "two", "exit 0", GateFormat::Exit),
            ],
        );
        runner.start_with(job, exploding).unwrap();
        let got = collect(&mut runner, 1);
        let JobDone::Gates {
            outcomes, attempt, ..
        } = &got[0]
        else {
            panic!("expected a gates result");
        };
        assert_eq!(*attempt, 1);
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes
            .iter()
            .all(|o| !o.passed && o.summary.contains("panicked")));
        assert!(wait_until(5_000, || runner.running() == 0));

        // A leaked slot would make this fail with a limit error.
        let cmd = command_job(&dir, "exit 0", 5_000);
        runner.start_with(cmd, exploding).unwrap();
        let got = collect(&mut runner, 1);
        let JobDone::Command {
            approval,
            exit_code,
            summary,
            ..
        } = &got[0]
        else {
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
        let mut runner = ThreadJobRunner::new(2, rd());
        runner
            .start(command_job(&dir, "echo hi; exit 4", 10_000))
            .unwrap();
        let got = collect(&mut runner, 1);
        let JobDone::Command {
            exit_code,
            summary,
            output,
            ..
        } = &got[0]
        else {
            panic!("expected a command result");
        };
        assert_eq!(*exit_code, Some(4));
        assert_eq!(summary, "exit code 4");
        assert_eq!(output.trim(), "hi");
    }

    // ---- program trust (a worker controls the worktree and its repository) ----------------------

    /// `<root>/repo/.fleet-worktrees/r1/t1` plus a trusted tool directory `<root>/tools`.
    fn fleet_layout(dir: &TempDir) -> (PathBuf, PathBuf) {
        dir.write("repo/.fleet-worktrees/r1/t1/.keep", "");
        dir.write("repo/tools/.keep", "");
        (dir.join("repo/.fleet-worktrees/r1/t1"), dir.join("repo"))
    }

    #[test]
    fn programs_inside_the_worktree_or_its_repository_are_refused() {
        let dir = TempDir::new("job-trust");
        let (worktree, repo) = fleet_layout(&dir);
        let in_worktree = dir.script("repo/.fleet-worktrees/r1/t1/evil.sh", "#!/bin/sh\n");
        let in_repo = dir.script("repo/tools/evil.sh", "#!/bin/sh\n");
        let in_sibling_worktree = dir.script("repo/.fleet-worktrees/r1/t2/evil.sh", "#!/bin/sh\n");
        for program in [&in_worktree, &in_repo, &in_sibling_worktree] {
            let err = trusted_program(program, &worktree).unwrap_err();
            assert!(err.starts_with("refused: "), "{err}");
        }
        // A symlink outside the repository that points into it is judged by its target.
        dir.write("outside/.keep", "");
        let link = dir.join("outside/looks-fine");
        std::os::unix::fs::symlink(&in_repo, &link).unwrap();
        assert!(trusted_program(&link, &worktree).is_err());
        // Neither a lookalike directory name nor a place outside the repository is refused.
        let lookalike = dir.script("repo-other/ok.sh", "#!/bin/sh\n");
        let elsewhere = dir.script("elsewhere/ok.sh", "#!/bin/sh\n");
        assert_eq!(
            trusted_program(&lookalike, &worktree).unwrap(),
            fs::canonicalize(&lookalike).unwrap()
        );
        assert!(trusted_program(&elsewhere, &worktree).is_ok());
        assert!(repo.exists());
    }

    #[test]
    fn a_worktree_outside_the_fleet_layout_only_protects_itself() {
        let dir = TempDir::new("job-trust-plain");
        dir.write("wt/.keep", "");
        let inside = dir.script("wt/evil.sh", "#!/bin/sh\n");
        let beside = dir.script("tool.sh", "#!/bin/sh\n");
        assert!(trusted_program(&inside, &dir.join("wt")).is_err());
        assert!(trusted_program(&beside, &dir.join("wt")).is_ok());
    }

    #[test]
    fn relative_and_unresolvable_programs_are_refused_with_a_clear_reason() {
        let dir = TempDir::new("job-trust-rel");
        dir.write("wt/.keep", "");
        let wt = dir.join("wt");
        let rel = trusted_program(Path::new("tool.sh"), &wt).unwrap_err();
        assert!(rel.contains("must be absolute"), "{rel}");
        let missing = trusted_program(Path::new("/nonexistent/prog"), &wt).unwrap_err();
        assert!(missing.starts_with("could not start: "), "{missing}");
    }

    #[test]
    fn a_command_planted_in_the_worktree_is_never_run() {
        let dir = TempDir::new("job-cmd-planted");
        let (worktree, _) = fleet_layout(&dir);
        let marker = dir.join("ran");
        let planted = dir.script(
            "repo/.fleet-worktrees/r1/t1/evil.sh",
            &format!("#!/bin/sh\necho x > '{}'\n", marker.display()),
        );
        let job = Job::Command {
            approval: ApprovalId::parse("a7").unwrap(),
            worktree: worktree.display().to_string(),
            program: planted.display().to_string(),
            args: Vec::new(),
            env: env_path(),
            timeout_ms: 5_000,
        };
        let JobDone::Command {
            exit_code,
            summary,
            output,
            ..
        } = execute(&job, &ctx())
        else {
            panic!("expected a command result");
        };
        assert_eq!(exit_code, None);
        assert!(summary.starts_with("refused: "), "{summary}");
        assert!(output.is_empty());
        assert!(!marker.exists());
    }

    #[test]
    fn a_relative_command_path_is_refused_even_when_it_exists_in_the_worktree() {
        let dir = TempDir::new("job-cmd-relative");
        dir.write("wt/.keep", "");
        dir.script("wt/evil.sh", "#!/bin/sh\ntouch ran\n");
        let job = Job::Command {
            approval: ApprovalId::parse("a7").unwrap(),
            worktree: dir.join("wt").display().to_string(),
            program: "./evil.sh".to_string(),
            args: Vec::new(),
            env: env_path(),
            timeout_ms: 5_000,
        };
        let JobDone::Command {
            exit_code, summary, ..
        } = execute(&job, &ctx())
        else {
            panic!("expected a command result");
        };
        assert_eq!(exit_code, None);
        assert!(summary.contains("must be absolute"), "{summary}");
        assert!(!dir.exists("wt/ran"));
    }

    // ---- relative PATH entries ------------------------------------------------------------------

    #[test]
    fn relative_path_entries_never_resolve_a_program_in_the_working_directory() {
        let dir = TempDir::new("job-relpath");
        dir.script("bin/tool", "#!/bin/sh\n");
        dir.script("tool", "#!/bin/sh\n");
        dir.script("trusted/tool", "#!/bin/sh\n");
        let env = |p: &str| vec![("PATH".to_string(), p.to_string())];
        let cwd = dir.path();
        assert_eq!(resolve_program("tool", cwd, &env(".")), None);
        assert_eq!(resolve_program("tool", cwd, &env("bin")), None);
        assert_eq!(resolve_program("tool", cwd, &env("./bin:..:")), None);
        let trusted = dir.join("trusted");
        // A later absolute entry is still used, after the relative ones were skipped.
        let mixed = format!(".:bin:{}", trusted.display());
        assert_eq!(
            resolve_program("tool", cwd, &env(&mixed)),
            Some(trusted.join("tool"))
        );
        // The same through the process runner: the planted `tool` in the cwd is not started.
        let mut s = spec(Path::new("tool"), &[], &dir);
        s.env = env(".");
        assert!(matches!(run_bounded(&s).exit, ExitKind::SpawnFailed(_)));
    }

    // ---- redaction at ingestion -----------------------------------------------------------------

    /// Shaped like a GitHub token, built at run time so no secret-looking literal is in the code.
    fn token() -> String {
        format!("ghp_{}", "a1B2".repeat(10))
    }

    #[test]
    fn gate_output_and_summary_are_redacted() {
        let dir = TempDir::new("job-redact-gate");
        let mut g = gate(
            &dir,
            "leaky",
            "echo \"token=$1\"; echo \"again $1\" >&2; exit 1",
            GateFormat::Exit,
        );
        g.args = vec![token()];
        let o = run_gate(dir.path(), &g, &ctx());
        assert!(!o.passed);
        assert!(
            !o.output.contains(&token()) && !o.summary.contains(&token()),
            "{o:?}"
        );
        assert!(o.output.contains("[REDACTED:github-token]"), "{o:?}");
        assert!(
            o.output.contains("token=") && o.output.contains("again "),
            "{o:?}"
        );
    }

    #[test]
    fn structured_gate_findings_are_redacted() {
        let dir = TempDir::new("job-redact-refcheck");
        let body = r#"printf '{"version":1,"summary":{"errors":1},"findings":[{"severity":"error","file":"a.rs","line":3,"message":"leaked %s"}]}\n' "$1"; exit 1"#;
        let mut g = gate(&dir, "refcheck", body, GateFormat::RefcheckJson);
        g.args = vec![token()];
        let o = run_gate(dir.path(), &g, &ctx());
        assert!(!o.passed);
        assert!(
            !o.output.contains(&token()) && !o.summary.contains(&token()),
            "{o:?}"
        );
        assert!(o.output.contains("leaked"), "{o:?}");
    }

    #[test]
    fn command_output_is_redacted_before_it_is_reported() {
        let dir = TempDir::new("job-redact-cmd");
        let mut job = command_job(&dir, "echo \"got $1\"; exit 0", 10_000);
        if let Job::Command { args, .. } = &mut job {
            args.push(token());
        }
        let JobDone::Command { output, .. } = execute(&job, &ctx()) else {
            panic!("expected a command result");
        };
        assert!(!output.contains(&token()), "{output}");
        assert!(output.contains("got [REDACTED:github-token]"), "{output}");
    }

    #[test]
    fn the_runner_hands_its_redactor_to_the_jobs() {
        let dir = TempDir::new("job-redact-runner");
        let mut job = command_job(&dir, "echo \"got $1\"", 10_000);
        if let Job::Command { args, .. } = &mut job {
            args.push(token());
        }
        let mut runner = ThreadJobRunner::new(1, rd());
        runner.start(job).unwrap();
        let got = collect(&mut runner, 1);
        let JobDone::Command { output, .. } = &got[0] else {
            panic!("expected a command result");
        };
        assert!(!output.contains(&token()), "{output}");
    }

    #[test]
    fn redact_block_keeps_line_structure_and_drops_key_blocks() {
        let text = "a\n\nb\n-----BEGIN PRIVATE KEY-----\nsecretbody\n-----END PRIVATE KEY-----\nc";
        assert_eq!(
            redact_block(&rd(), text),
            "a\n\nb\n[REDACTED:private-key]\nc"
        );
    }
}
