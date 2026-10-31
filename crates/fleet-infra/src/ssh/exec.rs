//! Running one `ssh` client to completion without a terminal: input from a file or memory, output
//! to memory or to a file, every byte counted, every wait bounded.
//!
//! The child gets an empty environment plus the explicit one given, its own process group, and is
//! listed in the daemon's job tracker (with whatever it starts) for as long as it runs, so the
//! origin checks of the control socket and the dashboard refuse it. A client that stops moving
//! bytes for `stall`, runs longer than `total` or produces more output than allowed is stopped (TERM,
//! then KILL for the whole group) and reported as such: a hung connection ends an attempt, it
//! never hangs the daemon.

use crate::jobs::{JobTracker, Sweeper};
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(20);
const KILL_GRACE: Duration = Duration::from_secs(2);
const READER_GRACE: Duration = Duration::from_secs(2);
/// Most of ssh's own messages kept.
pub const STDERR_MAX: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub enum Input {
    None,
    /// A file of ours, streamed to the remote command's standard input.
    File(PathBuf),
    Bytes(Vec<u8>),
}

#[derive(Clone, Debug)]
pub enum Output {
    /// Kept in memory up to `max` bytes.
    Capture { max: usize },
    /// Written to a new private file up to `max` bytes.
    File { path: PathBuf, max: u64 },
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The most the whole call may take.
    pub total: Duration,
    /// The longest time without a byte moving in either direction.
    pub stall: Duration,
}

#[derive(Clone, Debug)]
pub struct Spec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub input: Input,
    pub output: Output,
    pub limits: Limits,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ending {
    /// The client ended by itself: its exit code, or the signal that ended it.
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    TimedOut,
    Stalled,
    /// More output than allowed.
    OutputLimit,
    SpawnFailed(String),
}

#[derive(Clone, Debug)]
pub struct Done {
    pub ending: Ending,
    /// The captured output (empty when it went to a file).
    pub stdout: Vec<u8>,
    /// Bytes of output seen, kept or not.
    pub stdout_len: u64,
    /// ssh's own messages and the remote command's standard error, clipped.
    pub stderr: Vec<u8>,
}

impl Done {
    pub fn success(&self) -> bool {
        self.ending
            == Ending::Exited {
                code: Some(0),
                signal: None,
            }
    }

    pub fn code(&self) -> Option<i32> {
        match self.ending {
            Ending::Exited { code, .. } => code,
            _ => None,
        }
    }

    fn failed(why: impl Into<String>) -> Done {
        Done {
            ending: Ending::SpawnFailed(why.into()),
            stdout: Vec::new(),
            stdout_len: 0,
            stderr: Vec::new(),
        }
    }
}

struct Sink {
    kept: Mutex<Vec<u8>>,
    seen: AtomicU64,
    overflow: AtomicBool,
    done: AtomicBool,
}

impl Sink {
    fn new() -> Arc<Sink> {
        Arc::new(Sink {
            kept: Mutex::new(Vec::new()),
            seen: AtomicU64::new(0),
            overflow: AtomicBool::new(false),
            done: AtomicBool::new(false),
        })
    }
}

fn read_into_memory(mut pipe: impl Read, sink: &Sink, max: usize, stop_on_overflow: bool) {
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                sink.seen.fetch_add(n as u64, Ordering::SeqCst);
                let mut kept = sink.kept.lock().unwrap_or_else(PoisonError::into_inner);
                let take = max.saturating_sub(kept.len()).min(n);
                if let Some(head) = chunk.get(..take) {
                    kept.extend_from_slice(head);
                }
                if take < n {
                    sink.overflow.store(true, Ordering::SeqCst);
                    if stop_on_overflow {
                        break;
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    sink.done.store(true, Ordering::SeqCst);
}

fn read_into_file(mut pipe: impl Read, sink: &Sink, mut file: File, max: u64) {
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let total = sink.seen.fetch_add(n as u64, Ordering::SeqCst) + n as u64;
                if total > max {
                    sink.overflow.store(true, Ordering::SeqCst);
                    break;
                }
                let Some(data) = chunk.get(..n) else { break };
                if file.write_all(data).is_err() {
                    sink.overflow.store(true, Ordering::SeqCst);
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let _ = file.flush();
    sink.done.store(true, Ordering::SeqCst);
}

fn feed(mut stdin: std::process::ChildStdin, input: Input, sent: Arc<AtomicU64>) {
    let mut write = |data: &[u8]| -> bool {
        if stdin.write_all(data).is_err() {
            return false;
        }
        sent.fetch_add(data.len() as u64, Ordering::SeqCst);
        true
    };
    match input {
        Input::None => {}
        Input::Bytes(bytes) => {
            for part in bytes.chunks(64 * 1024) {
                if !write(part) {
                    break;
                }
            }
        }
        Input::File(path) => {
            let Ok(mut file) = OpenOptions::new()
                .read(true)
                .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
                .open(&path)
            else {
                return;
            };
            let mut chunk = [0u8; 64 * 1024];
            loop {
                match file.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let Some(data) = chunk.get(..n) else { break };
                        if !write(data) {
                            break;
                        }
                    }
                }
            }
        }
    }
    // Dropping stdin ends the remote command's input.
}

fn signal_group(pid: u32, signal: Signal) {
    let Ok(raw) = i32::try_from(pid) else { return };
    if raw > 1 {
        let _ = killpg(Pid::from_raw(raw), signal);
    }
}

fn stop(child: &mut std::process::Child, pid: u32) {
    signal_group(pid, Signal::SIGTERM);
    let end = Instant::now() + KILL_GRACE;
    while Instant::now() < end {
        if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
            break;
        }
        thread::sleep(POLL);
    }
    signal_group(pid, Signal::SIGKILL);
    let _ = child.kill();
    let _ = child.wait();
}

/// Runs the client once. Never panics, never uses a shell.
pub fn run(spec: &Spec, tracker: &JobTracker) -> Done {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .current_dir("/")
        .stdin(match spec.input {
            Input::None => Stdio::null(),
            _ => Stdio::piped(),
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let file = match &spec.output {
        Output::File { path, .. } => match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
            .open(path)
        {
            Ok(f) => Some(f),
            Err(e) => return Done::failed(format!("cannot create the output file: {e}")),
        },
        Output::Capture { .. } => None,
    };
    let mut child = match crate::jobs::spawn_retrying_busy(&mut command) {
        Ok(c) => c,
        Err(e) => return Done::failed(format!("cannot start ssh: {e}")),
    };
    let pid = child.id();
    let _tracked = tracker.track(pid);
    let mut sweeper = Sweeper::new(tracker, pid);

    let sent = Arc::new(AtomicU64::new(0));
    let writer: Option<JoinHandle<()>> = child.stdin.take().and_then(|stdin| {
        let input = spec.input.clone();
        let sent = Arc::clone(&sent);
        thread::Builder::new()
            .name("fleet-ssh-in".into())
            .spawn(move || feed(stdin, input, sent))
            .ok()
    });
    let out = Sink::new();
    let err = Sink::new();
    let (Some(out_pipe), Some(err_pipe)) = (child.stdout.take(), child.stderr.take()) else {
        stop(&mut child, pid);
        return Done::failed("the output pipes are missing");
    };
    let out_reader = {
        let sink = Arc::clone(&out);
        match (&spec.output, file) {
            (Output::File { max, .. }, Some(file)) => {
                let max = *max;
                thread::Builder::new()
                    .name("fleet-ssh-out".into())
                    .spawn(move || read_into_file(out_pipe, &sink, file, max))
            }
            (Output::Capture { max }, _) => {
                let max = *max;
                thread::Builder::new()
                    .name("fleet-ssh-out".into())
                    .spawn(move || read_into_memory(out_pipe, &sink, max, true))
            }
            _ => unreachable_output(),
        }
    };
    let err_reader = {
        let sink = Arc::clone(&err);
        thread::Builder::new()
            .name("fleet-ssh-err".into())
            .spawn(move || read_into_memory(err_pipe, &sink, STDERR_MAX, false))
    };
    let (Ok(out_reader), Ok(err_reader)) = (out_reader, err_reader) else {
        stop(&mut child, pid);
        return Done::failed("cannot start the reader threads");
    };

    let start = Instant::now();
    let mut last_progress = start;
    let mut moved = 0u64;
    let ending = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                break Ending::Exited {
                    code: status.code(),
                    signal: status.signal(),
                }
            }
            Ok(None) => {}
            Err(e) => {
                stop(&mut child, pid);
                break Ending::SpawnFailed(format!("waiting for ssh failed: {e}"));
            }
        }
        if out.overflow.load(Ordering::SeqCst) {
            stop(&mut child, pid);
            break Ending::OutputLimit;
        }
        let now = Instant::now();
        if now.duration_since(start) >= spec.limits.total {
            sweeper.sample(true);
            stop(&mut child, pid);
            break Ending::TimedOut;
        }
        let total_moved = sent.load(Ordering::SeqCst)
            + out.seen.load(Ordering::SeqCst)
            + err.seen.load(Ordering::SeqCst);
        if total_moved != moved {
            moved = total_moved;
            last_progress = now;
        } else if now.duration_since(last_progress) >= spec.limits.stall {
            sweeper.sample(true);
            stop(&mut child, pid);
            break Ending::Stalled;
        }
        sweeper.sample(false);
        thread::sleep(POLL);
    };
    // Whatever the client left in its group, or started elsewhere, goes with it.
    signal_group(pid, Signal::SIGKILL);
    sweeper.kill_all();

    let end = Instant::now() + READER_GRACE;
    while !(out.done.load(Ordering::SeqCst) && err.done.load(Ordering::SeqCst))
        && Instant::now() < end
    {
        thread::sleep(Duration::from_millis(5));
    }
    for (reader, sink) in [(out_reader, &out), (err_reader, &err)] {
        if sink.done.load(Ordering::SeqCst) {
            let _ = reader.join();
        }
    }
    if let Some(w) = writer {
        if w.is_finished() {
            let _ = w.join();
        }
    }
    let overflowed = out.overflow.load(Ordering::SeqCst);
    let ending = match ending {
        Ending::Exited { .. } if overflowed => Ending::OutputLimit,
        other => other,
    };
    let stdout = std::mem::take(&mut *out.kept.lock().unwrap_or_else(PoisonError::into_inner));
    let stderr = std::mem::take(&mut *err.kept.lock().unwrap_or_else(PoisonError::into_inner));
    Done {
        ending,
        stdout,
        stdout_len: out.seen.load(Ordering::SeqCst),
        stderr,
    }
}

/// `Output::File` always comes with an opened file; this only exists to satisfy the match.
fn unreachable_output() -> std::io::Result<JoinHandle<()>> {
    Err(std::io::Error::other("no output sink"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn sh(script: &str, input: Input, output: Output, total_ms: u64, stall_ms: u64) -> Spec {
        Spec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script.into()],
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            input,
            output,
            limits: Limits {
                total: Duration::from_millis(total_ms),
                stall: Duration::from_millis(stall_ms),
            },
        }
    }

    fn capture(script: &str, input: Input) -> Done {
        run(
            &sh(
                script,
                input,
                Output::Capture { max: 1024 * 1024 },
                20_000,
                20_000,
            ),
            &JobTracker::new(),
        )
    }

    #[test]
    fn output_error_text_and_exit_code_come_back() {
        let done = capture("printf out; printf err >&2; exit 3", Input::None);
        assert_eq!(
            done.ending,
            Ending::Exited {
                code: Some(3),
                signal: None
            }
        );
        assert_eq!(done.stdout, b"out");
        assert_eq!(done.stderr, b"err");
        assert!(!done.success());
        assert!(capture("true", Input::None).success());
    }

    #[test]
    fn input_from_memory_and_from_a_file_reaches_the_command_whole() {
        let big = vec![b'x'; 300_000];
        let done = capture("wc -c", Input::Bytes(big.clone()));
        assert_eq!(String::from_utf8_lossy(&done.stdout).trim(), "300000");
        let dir = TempDir::new("exec-in");
        let p = dir.write("in.bin", &"y".repeat(200_000));
        let done = capture("wc -c", Input::File(p));
        assert_eq!(String::from_utf8_lossy(&done.stdout).trim(), "200000");
    }

    #[test]
    fn the_environment_is_exactly_what_was_given() {
        let done = run(
            &Spec {
                env: vec![("ONLY".into(), "this".into())],
                ..sh(
                    "/usr/bin/env | grep -v '^PWD='",
                    Input::None,
                    Output::Capture { max: 4096 },
                    5_000,
                    5_000,
                )
            },
            &JobTracker::new(),
        );
        assert_eq!(String::from_utf8_lossy(&done.stdout).trim(), "ONLY=this");
    }

    #[test]
    fn output_beyond_the_cap_stops_the_client() {
        let done = run(
            &sh(
                "yes | head -c 10000000",
                Input::None,
                Output::Capture { max: 10_000 },
                20_000,
                20_000,
            ),
            &JobTracker::new(),
        );
        assert_eq!(done.ending, Ending::OutputLimit);
        assert!(done.stdout.len() <= 10_000);
    }

    #[test]
    fn output_to_a_file_is_capped_and_private() {
        let dir = TempDir::new("exec-out");
        let path = dir.join("out.bin");
        let done = run(
            &sh(
                "head -c 5000 /dev/zero",
                Input::None,
                Output::File {
                    path: path.clone(),
                    max: 6000,
                },
                20_000,
                20_000,
            ),
            &JobTracker::new(),
        );
        assert!(done.success());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 5000);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let path2 = dir.join("out2.bin");
        let done = run(
            &sh(
                "head -c 50000 /dev/zero",
                Input::None,
                Output::File {
                    path: path2.clone(),
                    max: 6000,
                },
                20_000,
                20_000,
            ),
            &JobTracker::new(),
        );
        assert_eq!(done.ending, Ending::OutputLimit);
        assert!(std::fs::metadata(&path2).unwrap().len() <= 6000);
        // An existing file is never overwritten.
        let done = run(
            &sh(
                "true",
                Input::None,
                Output::File { path, max: 10 },
                5_000,
                5_000,
            ),
            &JobTracker::new(),
        );
        assert!(matches!(done.ending, Ending::SpawnFailed(_)));
    }

    #[test]
    fn a_silent_client_is_stopped_as_stalled_and_a_long_one_as_timed_out() {
        let t = std::time::Instant::now();
        let done = run(
            &sh(
                "sleep 30",
                Input::None,
                Output::Capture { max: 10 },
                20_000,
                300,
            ),
            &JobTracker::new(),
        );
        assert_eq!(done.ending, Ending::Stalled);
        assert!(t.elapsed() < Duration::from_secs(10));
        // Output keeps a client alive under the stall limit but not beyond the total.
        let done = run(
            &sh(
                "while :; do echo x; sleep 0.1; done",
                Input::None,
                Output::Capture { max: 1 << 20 },
                800,
                5_000,
            ),
            &JobTracker::new(),
        );
        assert_eq!(done.ending, Ending::TimedOut);
    }

    #[test]
    fn the_group_is_registered_while_it_runs_and_killed_with_the_client() {
        let tracker = JobTracker::new();
        let dir = TempDir::new("exec-grp");
        let pidfile = dir.join("child.pid");
        let script = format!("sleep 60 & echo $! > {}; wait", pidfile.display());
        let t2 = tracker.clone();
        let handle = thread::spawn(move || {
            run(
                &sh(
                    &script,
                    Input::None,
                    Output::Capture { max: 10 },
                    20_000,
                    700,
                ),
                &t2,
            )
        });
        crate::testutil::wait_until(5_000, || !tracker.live().is_empty());
        assert_eq!(tracker.live().len(), 1);
        let done = handle.join().unwrap();
        assert_eq!(done.ending, Ending::Stalled);
        assert!(tracker.live().is_empty(), "unregistered when over");
        let child: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        crate::testutil::wait_until(3_000, || {
            nix::sys::signal::kill(Pid::from_raw(child), None).is_err()
        });
        assert!(
            nix::sys::signal::kill(Pid::from_raw(child), None).is_err(),
            "the group was killed"
        );
    }

    #[test]
    fn a_missing_program_is_a_spawn_failure() {
        let mut spec = sh("true", Input::None, Output::Capture { max: 10 }, 1000, 1000);
        spec.program = PathBuf::from("/nonexistent/ssh");
        assert!(matches!(
            run(&spec, &JobTracker::new()).ending,
            Ending::SpawnFailed(_)
        ));
    }
}
