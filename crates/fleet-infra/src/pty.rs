//! Worker sessions: a command running in its own pty, session and process group, with a bounded
//! output buffer.
//!
//! Input reaches a session through [`SessionIo::write_input`], which the control socket calls
//! for `send` and `attach`, and through [`PtySessionHost::notify`], the single sanitized line the
//! operator may switch on with `[messages] notify_input`; nothing else in the daemon writes to a
//! worker's terminal. It goes
//! through a bounded queue to a writer thread per session, so a worker that never reads its
//! stdin yields an error (`the worker is not reading its input`) instead of blocking a control
//! connection. Output is read by one thread per session into the domain's `OutputBuffer`
//! (sanitized, redacted, capped), so a worker that prints without limit cannot grow the
//! daemon's memory.

use crate::procid;
use fleet_app::ports::{
    SessionHost, SessionId, SessionOutput, SessionReading, SessionSpec, SessionState, Signal,
    SignalOutcome, Spawned,
};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::limits::ATTACH_QUEUE_CHUNKS;
use fleet_domain::output::{Chunk, OutputBuffer};
use fleet_domain::redact::Redactor;
use fleet_domain::supervision::{LiveSession, ProcessIdentity};
use nix::sys::signal::Signal as NixSignal;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const DEFAULT_ROWS: u16 = 40;
const DEFAULT_COLS: u16 = 120;
/// How long `release` waits for the reader thread to drain the last output of an exited session.
const DRAIN_WAIT: Duration = Duration::from_millis(500);

/// The longest line `notify` types (the fixed sentence and an id are far shorter).
const MAX_NOTICE_BYTES: usize = 120;
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        // A reader thread that panicked must not take the daemon down with it.
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Most chunks waiting for the writer thread of one session.
const INPUT_QUEUE_CHUNKS: usize = 64;
/// Largest chunk of the queue; longer input is split.
const INPUT_CHUNK_BYTES: usize = 4096;
/// The refusal for input that does not fit the queue.
const NOT_READING: &str = "the worker is not reading its input";

#[derive(Default)]
struct InputState {
    chunks: VecDeque<Vec<u8>>,
    closed: bool,
}

/// A bounded queue of input for one session. `push` never blocks; the writer thread of the
/// session is the only consumer.
struct InputQueue {
    state: Mutex<InputState>,
    ready: Condvar,
}

impl InputQueue {
    fn new() -> InputQueue {
        InputQueue {
            state: Mutex::new(InputState::default()),
            ready: Condvar::new(),
        }
    }

    /// All of `bytes` or nothing: a refused input is never half-queued.
    fn push(&self, bytes: &[u8]) -> std::io::Result<()> {
        let needed = bytes.len().div_ceil(INPUT_CHUNK_BYTES);
        if needed > INPUT_QUEUE_CHUNKS {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the input is too large",
            ));
        }
        let mut state = lock(&self.state);
        if state.closed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the session has ended",
            ));
        }
        if state.chunks.len().saturating_add(needed) > INPUT_QUEUE_CHUNKS {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                NOT_READING,
            ));
        }
        for chunk in bytes.chunks(INPUT_CHUNK_BYTES) {
            state.chunks.push_back(chunk.to_vec());
        }
        drop(state);
        self.ready.notify_one();
        Ok(())
    }

    /// Refuses further input, forgets what is queued and wakes the writer thread.
    fn close(&self) {
        let mut state = lock(&self.state);
        state.closed = true;
        state.chunks.clear();
        drop(state);
        self.ready.notify_all();
    }

    /// The next chunk, waiting for one; `None` once the queue is closed.
    fn next(&self) -> Option<Vec<u8>> {
        let mut state = lock(&self.state);
        loop {
            if state.closed {
                return None;
            }
            if let Some(chunk) = state.chunks.pop_front() {
                return Some(chunk);
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

/// The only code that writes to the worker's terminal. It blocks in `write` when the worker
/// does not read; the queue in front of it is what keeps everybody else from blocking too.
fn run_input_writer(queue: &InputQueue, mut writer: Box<dyn Write + Send>) {
    while let Some(chunk) = queue.next() {
        if writer
            .write_all(&chunk)
            .and_then(|()| writer.flush())
            .is_err()
        {
            break;
        }
    }
    queue.close();
}

/// The terminal side of a session, shared with the reader thread and the control socket.
pub struct SessionIo {
    out: Mutex<OutputBuffer>,
    input: Arc<InputQueue>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    started: Instant,
    /// Milliseconds since `started` at the last output.
    last_output_ms: AtomicU64,
    subscriber: Mutex<Option<SyncSender<Vec<u8>>>>,
    eof: AtomicBool,
    attach_dropped: AtomicU64,
    /// Raw bytes read from all sessions of the daemon (shared with the registry).
    total_bytes: Arc<AtomicU64>,
}

impl SessionIo {
    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Called by the reader thread for every chunk read from the terminal.
    fn ingest(&self, raw: &[u8]) {
        self.last_output_ms
            .store(self.elapsed_ms(), Ordering::Relaxed);
        self.total_bytes
            .fetch_add(raw.len() as u64, Ordering::Relaxed);
        {
            let mut sub = lock(&self.subscriber);
            if let Some(tx) = sub.as_ref() {
                match tx.try_send(raw.to_vec()) {
                    Ok(()) => {}
                    // A slow attached client loses output; it never slows the worker down.
                    Err(TrySendError::Full(_)) => {
                        self.attach_dropped
                            .fetch_add(raw.len() as u64, Ordering::Relaxed);
                    }
                    Err(TrySendError::Disconnected(_)) => *sub = None,
                }
            }
        }
        lock(&self.out).push(raw);
    }

    fn end_of_stream(&self) {
        lock(&self.out).finish();
        self.eof.store(true, Ordering::SeqCst);
        // Nobody reads input of an ended session; drop what is queued and refuse the rest.
        self.input.close();
        // Dropping the sender tells an attached client that the session is over.
        *lock(&self.subscriber) = None;
    }

    /// Output produced from now on, as raw terminal bytes (for `attach`). Replaces a previous
    /// subscriber: one attached client at a time.
    pub fn subscribe(&self) -> Receiver<Vec<u8>> {
        let (tx, rx) = sync_channel(ATTACH_QUEUE_CHUNKS);
        if self.eof.load(Ordering::SeqCst) {
            return rx;
        }
        *lock(&self.subscriber) = Some(tx);
        rx
    }

    /// Queues keystrokes or text for the terminal and returns at once. The only way input
    /// reaches a worker. A writer thread per session does the blocking write, so a worker that
    /// does not read its stdin can never hold a control connection or the engine. When the
    /// queue is full the input is refused whole (`WouldBlock`, "the worker is not reading its
    /// input"); once the session has ended it is `BrokenPipe`.
    pub fn write_input(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.input.push(bytes)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        let size = PtySize {
            rows: rows.clamp(1, 500),
            cols: cols.clamp(1, 1000),
            pixel_width: 0,
            pixel_height: 0,
        };
        lock(&self.master)
            .resize(size)
            .map_err(|e| std::io::Error::other(e.to_string()))
    }

    /// Bytes an attached client missed because it was too slow.
    pub fn attach_dropped_bytes(&self) -> u64 {
        self.attach_dropped.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self, max: usize) -> String {
        lock(&self.out).snapshot(max)
    }

    pub fn read_since(&self, offset: u64, max: usize) -> Chunk {
        lock(&self.out).read_since(offset, max)
    }

    pub fn retained_bytes(&self) -> usize {
        lock(&self.out).retained_bytes()
    }

    pub fn raw_bytes(&self) -> u64 {
        lock(&self.out).raw_bytes()
    }
}

impl Drop for SessionIo {
    fn drop(&mut self) {
        // Lets the writer thread end even when the worker never read what is queued.
        self.input.close();
    }
}

/// Lookup of live sessions' terminals, shared with the control socket (which runs on other
/// threads than the orchestrator), plus a counter of all output bytes ever read.
#[derive(Clone, Default)]
pub struct SessionRegistry {
    sessions: Arc<Mutex<BTreeMap<u64, Arc<SessionIo>>>>,
    total_bytes: Arc<AtomicU64>,
}

impl SessionRegistry {
    /// Raw terminal bytes read from every session since the daemon started.
    pub fn output_bytes(&self) -> u64 {
        self.total_bytes.load(Ordering::Relaxed)
    }

    pub fn get(&self, id: SessionId) -> Option<Arc<SessionIo>> {
        lock(&self.sessions).get(&id.0).cloned()
    }

    fn insert(&self, id: SessionId, io: Arc<SessionIo>) {
        lock(&self.sessions).insert(id.0, io);
    }

    fn remove(&self, id: SessionId) {
        lock(&self.sessions).remove(&id.0);
    }

    pub fn len(&self) -> usize {
        lock(&self.sessions).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

struct Session {
    child: Box<dyn Child + Send + Sync>,
    identity: ProcessIdentity,
    io: Arc<SessionIo>,
    exited: Option<SessionState>,
}

/// The `SessionHost` port on real ptys.
pub struct PtySessionHost {
    sessions: BTreeMap<u64, Session>,
    registry: SessionRegistry,
    next_id: u64,
    ring_bytes: usize,
    redactor: Arc<Redactor>,
}

impl PtySessionHost {
    pub fn new(
        ring_bytes: usize,
        redactor: Arc<Redactor>,
        registry: SessionRegistry,
    ) -> PtySessionHost {
        PtySessionHost {
            sessions: BTreeMap::new(),
            registry,
            next_id: 0,
            ring_bytes,
            redactor,
        }
    }

    pub fn registry(&self) -> SessionRegistry {
        self.registry.clone()
    }
}

/// Starts the command, retrying briefly while the executable is still open for writing in another
/// thread (`ETXTBSY`), which happens when a script was written an instant ago.
fn spawn_retrying_busy(
    slave: &dyn portable_pty::SlavePty,
    cmd: &CommandBuilder,
) -> Result<Box<dyn Child + Send + Sync>, impl std::fmt::Display + std::fmt::Debug> {
    let mut attempts = 0;
    loop {
        match slave.spawn_command(cmd.clone()) {
            Err(e) if attempts < 20 && format!("{e:#}").contains("Text file busy") => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(25));
            }
            other => return other,
        }
    }
}

fn map_exit(status: &portable_pty::ExitStatus) -> SessionState {
    match status.signal() {
        Some(name) => SessionState::Exited {
            code: None,
            signal: Some(name.to_string()),
        },
        None => SessionState::Exited {
            code: Some(i64::from(status.exit_code())),
            signal: None,
        },
    }
}

fn to_nix(signal: Signal) -> NixSignal {
    match signal {
        Signal::Term => NixSignal::SIGTERM,
        Signal::Kill => NixSignal::SIGKILL,
    }
}

impl SessionHost for PtySessionHost {
    fn spawn(&mut self, spec: &SessionSpec) -> Result<Spawned> {
        let cwd = std::path::Path::new(&spec.cwd);
        let program =
            crate::jobs::resolve_program(&spec.program, cwd, &spec.env).ok_or_else(|| {
                FleetError::not_found(format!(
                    "worker command {} was not found on the worker's PATH",
                    fleet_domain::ids::show(&spec.program)
                ))
            })?;
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: DEFAULT_ROWS,
                cols: DEFAULT_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| FleetError::internal(format!("could not open a pty: {e}")))?;
        let mut cmd = CommandBuilder::new(&program);
        cmd.args(&spec.args);
        cmd.cwd(&spec.cwd);
        // Nothing is inherited from the daemon: only the environment the application layer built.
        cmd.env_clear();
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let mut child = spawn_retrying_busy(&*pair.slave, &cmd)
            .map_err(|e| FleetError::internal(format!("could not start the worker: {e}")))?;
        drop(pair.slave);
        let pid = child
            .process_id()
            .ok_or_else(|| FleetError::internal("the worker has no process id"))?;
        let Some(token) = procid::start_token(pid) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(FleetError::internal(
                "could not read the worker's start time",
            ));
        };
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| FleetError::internal(format!("could not read the pty: {e}")));
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| FleetError::internal(format!("could not write the pty: {e}")));
        let (mut reader, writer) = match (reader, writer) {
            (Ok(r), Ok(w)) => (r, w),
            (Err(e), _) | (_, Err(e)) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };
        let input = Arc::new(InputQueue::new());
        let io = Arc::new(SessionIo {
            out: Mutex::new(OutputBuffer::new(
                self.ring_bytes,
                Arc::clone(&self.redactor),
            )),
            input: Arc::clone(&input),
            master: Mutex::new(pair.master),
            started: Instant::now(),
            last_output_ms: AtomicU64::new(0),
            subscriber: Mutex::new(None),
            eof: AtomicBool::new(false),
            attach_dropped: AtomicU64::new(0),
            total_bytes: Arc::clone(&self.registry.total_bytes),
        });
        let reader_io = Arc::clone(&io);
        let spawned_thread = std::thread::Builder::new()
            .name(format!("pty-reader-{}", self.next_id.saturating_add(1)))
            .spawn(move || {
                let mut buf = vec![0u8; 16 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Some(chunk) = buf.get(..n) {
                                reader_io.ingest(chunk);
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                reader_io.end_of_stream();
            });
        if let Err(e) = spawned_thread {
            let _ = procid::signal_verified(&ProcessIdentity::new(pid, token), NixSignal::SIGKILL);
            let _ = child.wait();
            return Err(FleetError::internal(format!(
                "could not start the output reader: {e}"
            )));
        }
        let writer_queue = Arc::clone(&input);
        let writer_thread = std::thread::Builder::new()
            .name(format!("pty-writer-{}", self.next_id.saturating_add(1)))
            .spawn(move || run_input_writer(&writer_queue, writer));
        if let Err(e) = writer_thread {
            let _ = procid::signal_verified(&ProcessIdentity::new(pid, token), NixSignal::SIGKILL);
            let _ = child.wait();
            return Err(FleetError::internal(format!(
                "could not start the input writer: {e}"
            )));
        }
        self.next_id = self.next_id.saturating_add(1);
        let id = SessionId(self.next_id);
        let identity = ProcessIdentity::new(pid, token);
        self.registry.insert(id, Arc::clone(&io));
        self.sessions.insert(
            id.0,
            Session {
                child,
                identity: identity.clone(),
                io,
                exited: None,
            },
        );
        Ok(Spawned { id, identity })
    }

    fn poll(&mut self, id: SessionId) -> SessionState {
        let Some(s) = self.sessions.get_mut(&id.0) else {
            return SessionState::Exited {
                code: None,
                signal: None,
            };
        };
        if let Some(done) = &s.exited {
            return done.clone();
        }
        match s.child.try_wait() {
            Ok(None) => SessionState::Running,
            Ok(Some(status)) => {
                let state = map_exit(&status);
                // The leader is reaped; stop anything it left running in its group.
                procid::kill_group_leftovers(s.identity.pid);
                s.exited = Some(state.clone());
                state
            }
            Err(_) => {
                let state = SessionState::Exited {
                    code: None,
                    signal: None,
                };
                procid::kill_group_leftovers(s.identity.pid);
                s.exited = Some(state.clone());
                state
            }
        }
    }

    fn reading(&self, id: SessionId) -> Option<SessionReading> {
        let s = self.sessions.get(&id.0)?;
        let elapsed = s.io.elapsed_ms();
        let last = s.io.last_output_ms.load(Ordering::Relaxed);
        Some(SessionReading {
            elapsed_ms: elapsed,
            idle_ms: elapsed.saturating_sub(last),
        })
    }

    fn signal(&mut self, id: SessionId, signal: Signal) -> SignalOutcome {
        match self.sessions.get(&id.0) {
            Some(s) if s.exited.is_none() => procid::signal_verified(&s.identity, to_nix(signal)),
            _ => SignalOutcome::Gone,
        }
    }

    fn snapshot(&self, id: SessionId, max: usize) -> Option<String> {
        self.sessions.get(&id.0).map(|s| s.io.snapshot(max))
    }

    fn read_since(&self, id: SessionId, offset: u64, max: usize) -> Option<Chunk> {
        self.sessions
            .get(&id.0)
            .map(|s| s.io.read_since(offset, max))
    }

    fn release(&mut self, id: SessionId) -> SessionOutput {
        self.registry.remove(id);
        let Some(mut s) = self.sessions.remove(&id.0) else {
            return SessionOutput {
                text: String::new(),
                stripped: 0,
            };
        };
        if s.exited.is_none() {
            // Released while still running (give-up path): make sure it is stopped.
            let _ = procid::signal_verified(&s.identity, NixSignal::SIGKILL);
            let _ = s.child.try_wait();
        }
        procid::kill_group_leftovers(s.identity.pid);
        // The reader thread may still be draining the last bytes.
        let deadline = Instant::now() + DRAIN_WAIT;
        while !s.io.eof.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let text = lock(&s.io.out).read_since(0, usize::MAX).text;
        let stripped = lock(&s.io.out).stripped_sequences();
        SessionOutput { text, stripped }
    }

    fn live(&self) -> Vec<LiveSession> {
        self.sessions
            .values()
            .filter(|s| s.exited.is_none())
            .map(|s| LiveSession {
                pid: s.identity.pid,
                pgid: s.identity.pid,
            })
            .collect()
    }

    fn start_token(&self, pid: u32) -> Option<String> {
        procid::start_token(pid)
    }

    fn terminate_orphan(&mut self, identity: &ProcessIdentity, grace_ms: u64) -> SignalOutcome {
        procid::terminate_group_blocking(identity, grace_ms)
    }

    fn notify(&mut self, id: SessionId, line: &str) -> Result<()> {
        let io = self
            .registry
            .get(id)
            .ok_or_else(|| FleetError::conflict("the session is not running"))?;
        // Only printable ASCII, bounded: whatever the caller passed, no escape sequence, control
        // character or invisible character reaches the terminal. No Enter either: the notice only
        // sits in the input line, so a message a peer timed for a confirmation prompt or a
        // half-typed command can never submit or confirm anything.
        let bytes: Vec<u8> = line
            .bytes()
            .filter(|b| (0x20..0x7f).contains(b))
            .take(MAX_NOTICE_BYTES)
            .collect();
        io.write_input(&bytes).map_err(|e| match e.kind() {
            std::io::ErrorKind::WouldBlock => {
                FleetError::unavailable("the worker is not reading its input")
            }
            _ => FleetError::conflict("the session ended before the notice could be typed"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{wait_until, TempDir};
    use fleet_domain::ids::{RunId, TaskId};

    fn host() -> PtySessionHost {
        PtySessionHost::new(
            64 * 1024,
            Arc::new(Redactor::builtin()),
            SessionRegistry::default(),
        )
    }

    fn spec(dir: &TempDir, script: &str) -> SessionSpec {
        SessionSpec {
            run: RunId::parse("r1").unwrap(),
            task: TaskId::parse("t1").unwrap(),
            attempt: 1,
            program: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            cwd: dir.path().to_string_lossy().to_string(),
            env: vec![
                ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                ("HOME".to_string(), dir.path().to_string_lossy().to_string()),
                ("TERM".to_string(), "xterm".to_string()),
            ],
        }
    }

    fn wait_exit(h: &mut PtySessionHost, id: SessionId) -> SessionState {
        let mut last = SessionState::Running;
        assert!(
            wait_until(10_000, || {
                last = h.poll(id);
                last != SessionState::Running
            }),
            "session did not exit"
        );
        last
    }

    #[test]
    fn runs_a_command_and_captures_sanitized_output() {
        let dir = TempDir::new("pty");
        let mut h = host();
        let s = h
            .spawn(&spec(
                &dir,
                "printf 'hello \\033[31mred\\033[0m\\n'; exit 3",
            ))
            .unwrap();
        assert_eq!(
            wait_exit(&mut h, s.id),
            SessionState::Exited {
                code: Some(3),
                signal: None
            }
        );
        let out = h.release(s.id);
        assert!(out.text.contains("hello red"), "{:?}", out.text);
        assert!(!out.text.contains('\u{1b}'));
        assert!(out.stripped >= 2);
    }

    #[test]
    fn a_notice_is_printable_ascii_bounded_and_never_presses_enter() {
        let dir = TempDir::new("pty");
        let mut h = host();
        // raw mode, so what arrives is exactly what was typed (a line discipline would hold a
        // line back until Enter, and echo it)
        let s = h
            .spawn(&spec(
                &dir,
                "stty raw -echo min 0 time 10; touch ready; sleep 1; dd bs=1 count=400 of=got.txt 2>/dev/null",
            ))
            .unwrap();
        assert!(wait_until(5000, || dir.exists("ready")));
        let hostile = format!(
            "[fleet] new message m1 \u{1b}]52;c;AAAA\u{7}\u{1b}[2J\r\n\u{202e}in .fleet/inbox {}",
            "x".repeat(400)
        );
        h.notify(s.id, &hostile).unwrap();
        wait_exit(&mut h, s.id);
        let typed = dir.read("got.txt");
        assert!(
            typed.starts_with("[fleet] new message m1 ]52;c;AAAA[2Jin .fleet/inbox x"),
            "{typed:?}"
        );
        assert_eq!(typed.len(), MAX_NOTICE_BYTES, "bounded: {typed:?}");
        assert!(
            typed.bytes().all(|b| (0x20..0x7f).contains(&b)),
            "no control character, no Enter: {typed:?}"
        );
        h.release(s.id);
    }

    #[test]
    fn a_notice_for_a_session_that_is_gone_is_an_error() {
        let dir = TempDir::new("pty");
        let mut h = host();
        let s = h.spawn(&spec(&dir, "exit 0")).unwrap();
        wait_exit(&mut h, s.id);
        h.release(s.id);
        assert!(h.notify(s.id, "[fleet] hi").is_err());
    }

    #[test]
    fn the_worker_runs_in_its_own_session_and_process_group() {
        let dir = TempDir::new("pty");
        let mut h = host();
        let s = h
            .spawn(&spec(
                &dir,
                "ps -o pid=,pgid=,sid= -p $$ > ids.txt; sleep 1",
            ))
            .unwrap();
        assert!(wait_until(5000, || dir.exists("ids.txt")
            && !dir.read("ids.txt").trim().is_empty()));
        let ids: Vec<u32> = dir
            .read("ids.txt")
            .split_whitespace()
            .filter_map(|x| x.parse().ok())
            .collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(
            ids[0], ids[1],
            "pgid equals pid: the worker leads its group"
        );
        assert_eq!(
            ids[0], ids[2],
            "sid equals pid: the worker leads its session"
        );
        assert_eq!(ids[0], s.identity.pid);
        assert_ne!(ids[0], std::process::id());
        let _ = h.signal(s.id, Signal::Kill);
        wait_exit(&mut h, s.id);
        h.release(s.id);
    }

    #[test]
    fn the_environment_is_exactly_what_was_given() {
        let dir = TempDir::new("pty");
        let mut h = host();
        let mut sp = spec(&dir, "env | sort > env.txt");
        sp.env.push(("ONLY_ME".to_string(), "yes".to_string()));
        let s = h.spawn(&sp).unwrap();
        wait_exit(&mut h, s.id);
        h.release(s.id);
        let env = dir.read("env.txt");
        assert!(env.contains("ONLY_ME=yes"));
        for forbidden in [
            "SENTINEL",
            "FLEET_SOCKET",
            "FLEET_STATE",
            "SSH_AUTH_SOCK",
            "AWS_",
        ] {
            assert!(
                !env.contains(forbidden),
                "{forbidden} leaked into the worker: {env}"
            );
        }
    }

    #[test]
    fn a_missing_command_is_a_not_found_error() {
        let dir = TempDir::new("pty");
        let mut h = host();
        let mut sp = spec(&dir, "true");
        sp.program = "definitely-not-installed-xyz".to_string();
        assert_eq!(
            h.spawn(&sp).unwrap_err().code,
            fleet_domain::error::ErrorCode::NotFound
        );
    }

    #[test]
    fn output_is_bounded_no_matter_how_much_the_worker_prints() {
        let dir = TempDir::new("pty");
        let mut h = PtySessionHost::new(
            4096,
            Arc::new(Redactor::builtin()),
            SessionRegistry::default(),
        );
        let s = h
            .spawn(&spec(&dir, "i=0; while [ $i -lt 20000 ]; do echo \"line $i xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"; i=$((i+1)); done"))
            .unwrap();
        wait_exit(&mut h, s.id);
        let out = h.release(s.id);
        assert!(out.text.len() <= 4096, "{}", out.text.len());
        assert!(out.text.contains("line 19999"), "the newest output is kept");
        assert!(
            !out.text.contains("line 0 "),
            "the oldest output was dropped"
        );
    }

    #[test]
    fn secrets_are_redacted_before_they_are_stored() {
        let dir = TempDir::new("pty");
        let mut h = host();
        let token = format!("ghp_{}", "a1B2".repeat(10));
        let s = h
            .spawn(&spec(&dir, &format!("echo token {token}")))
            .unwrap();
        wait_exit(&mut h, s.id);
        let out = h.release(s.id);
        assert!(!out.text.contains(&token));
        assert!(out.text.contains("[REDACTED:github-token]"), "{}", out.text);
    }

    #[test]
    fn hostile_terminal_sequences_never_reach_the_stored_output() {
        let dir = TempDir::new("pty");
        let mut h = host();
        let s = h
            .spawn(&spec(
                &dir,
                "printf 'a\\033]52;c;AAAA\\007b\\033[2Jc\\342\\200\\256d\\033]0;title\\033\\\\e\\n'",
            ))
            .unwrap();
        wait_exit(&mut h, s.id);
        let out = h.release(s.id);
        assert!(
            !out.text.contains('\u{1b}') && !out.text.contains('\u{202e}'),
            "{:?}",
            out.text
        );
        assert!(out.text.contains("abc"), "{:?}", out.text);
    }

    #[test]
    fn input_reaches_the_worker_only_through_write_input() {
        let dir = TempDir::new("pty");
        let mut h = host();
        let s = h
            .spawn(&spec(&dir, "read answer; echo got:$answer"))
            .unwrap();
        let io = h.registry().get(s.id).unwrap();
        io.write_input(b"forty-two\r").unwrap();
        wait_exit(&mut h, s.id);
        let out = h.release(s.id);
        assert!(out.text.contains("got:forty-two"), "{:?}", out.text);
    }

    #[test]
    fn input_longer_than_one_chunk_arrives_whole_and_in_order() {
        let dir = TempDir::new("pty");
        let mut h = host();
        // raw mode first, so the line discipline neither edits nor limits what is written
        let s = h
            .spawn(&spec(
                &dir,
                "stty raw -echo; echo ready; head -c 10000 > got.bin",
            ))
            .unwrap();
        assert!(wait_until(10_000, || h
            .snapshot(s.id, 100)
            .unwrap_or_default()
            .contains("ready")));
        let sent: Vec<u8> = (0..10_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let io = h.registry().get(s.id).unwrap();
        io.write_input(&sent).unwrap();
        wait_exit(&mut h, s.id);
        h.release(s.id);
        assert_eq!(std::fs::read(dir.join("got.bin")).unwrap(), sent);
    }

    #[test]
    fn input_to_a_worker_that_never_reads_is_refused_quickly_and_nothing_else_blocks() {
        let dir = TempDir::new("pty");
        let mut h = host();
        // Raw mode, no echo: the terminal's input buffer fills and then pushes back. In canonical
        // mode some kernels drop the overflow of a long line instead, and nothing would push back.
        let s = h
            .spawn(&spec(&dir, "stty raw -echo; exec sleep 60"))
            .unwrap();
        let io = h.registry().get(s.id).unwrap();

        let started = Instant::now();
        let mut refused = 0;
        for i in 0..1000 {
            let call = Instant::now();
            if let Err(e) = io.write_input(&[b'x'; INPUT_CHUNK_BYTES]) {
                assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock, "input {i}: {e}");
                assert_eq!(e.to_string(), "the worker is not reading its input");
                refused += 1;
            }
            assert!(
                call.elapsed() < Duration::from_secs(2),
                "input {i} blocked for {:?}",
                call.elapsed()
            );
        }
        assert!(
            refused > 0,
            "4 MB of input into a worker that reads nothing was all accepted"
        );
        assert!(started.elapsed() < Duration::from_secs(20));
        // the memory held for the worker is bounded by the queue, not by what was sent
        assert!(lock(&io.input.state).chunks.len() <= INPUT_QUEUE_CHUNKS);

        // everything but writing still works while the input is jammed
        io.resize(100, 30).unwrap();
        assert!(h.snapshot(s.id, 100).is_some());
        assert_eq!(h.poll(s.id), SessionState::Running);

        // the worker can still be stopped, and afterwards input says the session ended
        assert_eq!(h.signal(s.id, Signal::Kill), SignalOutcome::Sent);
        wait_exit(&mut h, s.id);
        assert!(
            wait_until(10_000, || matches!(
                io.write_input(b"x"),
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe
            )),
            "input to an ended session must fail as ended, not as jammed"
        );
        h.release(s.id);
    }

    #[test]
    fn a_refused_input_is_never_half_queued_and_oversized_input_is_invalid() {
        let queue = InputQueue::new();
        let chunk = vec![1u8; INPUT_CHUNK_BYTES];
        // 63 chunks queued; a 2-chunk input does not fit the one free slot and leaves it free
        for _ in 0..INPUT_QUEUE_CHUNKS - 1 {
            queue.push(&chunk).unwrap();
        }
        let two = vec![2u8; INPUT_CHUNK_BYTES + 1];
        let err = queue.push(&two).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(lock(&queue.state).chunks.len(), INPUT_QUEUE_CHUNKS - 1);
        queue.push(&chunk).unwrap();
        assert_eq!(
            queue.push(&[0]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        // chunks come out in the order they went in, then a closed queue ends the writer
        assert_eq!(queue.next().as_deref(), Some(chunk.as_slice()));
        queue.close();
        assert_eq!(queue.next(), None);
        assert_eq!(
            queue.push(b"x").unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
        let big = vec![0u8; INPUT_QUEUE_CHUNKS * INPUT_CHUNK_BYTES + 1];
        assert_eq!(
            InputQueue::new().push(&big).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn term_then_kill_stops_a_worker_that_ignores_term_including_its_children() {
        let dir = TempDir::new("pty");
        let mut h = host();
        let s = h
            .spawn(&spec(
                &dir,
                "trap '' TERM; sleep 60 & echo $! > child.pid; while :; do sleep 1; done",
            ))
            .unwrap();
        assert!(wait_until(5000, || dir.exists("child.pid")));
        assert_eq!(h.signal(s.id, Signal::Term), SignalOutcome::Sent);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(h.poll(s.id), SessionState::Running, "TERM is ignored");
        assert_eq!(h.signal(s.id, Signal::Kill), SignalOutcome::Sent);
        let state = wait_exit(&mut h, s.id);
        assert!(
            matches!(
                state,
                SessionState::Exited {
                    signal: Some(_),
                    ..
                }
            ),
            "{state:?}"
        );
        h.release(s.id);
        let child: u32 = dir.read("child.pid").trim().parse().unwrap();
        assert!(
            wait_until(5000, || procid::start_token(child).is_none()
                || std::fs::read_to_string(format!("/proc/{child}/stat"))
                    .is_ok_and(|s| s.contains(") Z "))),
            "the child in the worker's group must be gone"
        );
    }

    #[test]
    fn a_signal_for_a_session_whose_pid_was_reused_is_refused() {
        let dir = TempDir::new("pty");
        let mut h = host();
        let s = h.spawn(&spec(&dir, "sleep 30")).unwrap();
        // forge a different identity for the same pid by tampering with the stored token
        if let Some(sess) = h.sessions.get_mut(&s.id.0) {
            sess.identity.start_token.push('0');
        }
        assert!(matches!(
            h.signal(s.id, Signal::Kill),
            SignalOutcome::Refused(_)
        ));
        assert_eq!(h.poll(s.id), SessionState::Running);
        if let Some(sess) = h.sessions.get_mut(&s.id.0) {
            sess.identity.start_token.pop();
        }
        assert_eq!(h.signal(s.id, Signal::Kill), SignalOutcome::Sent);
        wait_exit(&mut h, s.id);
        h.release(s.id);
    }

    #[test]
    fn idle_time_resets_with_output() {
        let dir = TempDir::new("pty");
        let mut h = host();
        let s = h
            .spawn(&spec(&dir, "sleep 0.3; echo hi; sleep 30"))
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let early = h.reading(s.id).unwrap();
        assert!(early.idle_ms >= early.elapsed_ms.saturating_sub(50));
        assert!(wait_until(5000, || h
            .snapshot(s.id, 100)
            .unwrap_or_default()
            .contains("hi")));
        let r = h.reading(s.id).unwrap();
        assert!(r.idle_ms < r.elapsed_ms);
        let _ = h.signal(s.id, Signal::Kill);
        wait_exit(&mut h, s.id);
        h.release(s.id);
    }

    #[test]
    fn live_lists_only_running_sessions() {
        let dir = TempDir::new("pty");
        let mut h = host();
        let s = h.spawn(&spec(&dir, "sleep 30")).unwrap();
        assert_eq!(
            h.live(),
            vec![LiveSession {
                pid: s.identity.pid,
                pgid: s.identity.pid
            }]
        );
        let _ = h.signal(s.id, Signal::Kill);
        wait_exit(&mut h, s.id);
        assert!(h.live().is_empty());
        h.release(s.id);
        assert!(h.registry().is_empty());
    }

    #[test]
    fn attach_subscribers_get_live_output_and_see_the_end() {
        let dir = TempDir::new("pty");
        let mut h = host();
        let s = h
            .spawn(&spec(&dir, "sleep 0.3; echo streamed; sleep 0.3"))
            .unwrap();
        let rx = h.registry().get(s.id).unwrap().subscribe();
        let mut seen = String::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(chunk) => seen.push_str(&String::from_utf8_lossy(&chunk)),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        assert!(seen.contains("streamed"), "{seen:?}");
        wait_exit(&mut h, s.id);
        h.release(s.id);
    }

    #[test]
    fn a_slow_attach_client_cannot_slow_the_worker() {
        let dir = TempDir::new("pty");
        let mut h = host();
        let s = h
            .spawn(&spec(
                &dir,
                "i=0; while [ $i -lt 30000 ]; do echo \"flood $i\"; i=$((i+1)); done",
            ))
            .unwrap();
        let io = h.registry().get(s.id).unwrap();
        let _rx = io.subscribe(); // never read
        let started = Instant::now();
        wait_exit(&mut h, s.id);
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(
            io.attach_dropped_bytes() > 0,
            "the unread subscriber lost output instead of blocking the worker"
        );
        h.release(s.id);
    }

    #[test]
    fn resize_reaches_the_terminal() {
        let dir = TempDir::new("pty");
        let mut h = host();
        let s = h
            .spawn(&spec(&dir, "sleep 0.5; stty size > size.txt"))
            .unwrap();
        h.registry().get(s.id).unwrap().resize(100, 30).unwrap();
        wait_exit(&mut h, s.id);
        h.release(s.id);
        assert_eq!(dir.read("size.txt").trim(), "30 100");
    }

    #[test]
    fn orphan_termination_checks_identity() {
        let dir = TempDir::new("pty");
        let mut h = host();
        let s = h.spawn(&spec(&dir, "sleep 30")).unwrap();
        let wrong = ProcessIdentity::new(s.identity.pid, "0");
        assert!(matches!(
            h.terminate_orphan(&wrong, 100),
            SignalOutcome::Refused(_)
        ));
        assert_eq!(h.poll(s.id), SessionState::Running);
        assert_eq!(h.terminate_orphan(&s.identity, 500), SignalOutcome::Sent);
        wait_exit(&mut h, s.id);
        h.release(s.id);
    }
}
