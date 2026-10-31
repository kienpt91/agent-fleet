//! The control socket server.
//!
//! One Unix stream socket in the private state directory. The directory (mode 0700, owned by the
//! current user) is the first barrier; the peer credentials the kernel reports for each
//! connection are the second. Every request line is bounded and validated before it reaches the
//! handler, and a handler that panics or fails produces an error reply, never a dead server.

use crate::protocol::{
    decode_resize, encode_err, encode_ok, parse_request, read_frame, read_line_bounded,
    write_frame, KIND_EXIT, KIND_INPUT, KIND_OUTPUT, KIND_RESIZE, MAX_RESPONSE_BYTES,
};
use crate::statedir::{create_token_file, remove_token_file, token_path, tokens_equal};
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::limits::{MAX_CONNECTIONS, MAX_FRAME_PAYLOAD, MAX_MESSAGE_BYTES};
use fleet_domain::sanitize::{head_str, Mode, Sanitizer};
use fleet_domain::value::Value;
use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// How often the accept loop looks at the stop flag.
const POLL: Duration = Duration::from_millis(20);
/// How long a client has to deliver one complete request line: counted from the accept for the
/// first request and from the end of the previous reply for the next ones. It is a deadline for
/// the whole line, not a timeout per read, so a client that trickles bytes cannot hold a slot.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a refused client may keep sending before the connection is closed anyway.
const REFUSAL_DRAIN: Duration = Duration::from_millis(250);
/// Most bytes read from a refused client while draining.
const REFUSAL_DRAIN_BYTES: usize = 64 * 1024;
/// A client that does not read its replies is dropped after this long.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Consecutive accept failures (about five seconds) after which the server gives up.
const MAX_ACCEPT_FAILURES: u32 = 250;
/// How long `serve` lets open connections finish writing a reply before it closes them.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(300);
/// How long `serve` waits for connection threads after asking them to stop.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// The attach forwarder looks at its stop flag this often while the session is quiet.
const FORWARD_POLL: Duration = Duration::from_millis(100);

/// True where the kernel reports the peer's credentials for Unix sockets.
const CREDENTIALS_SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "macos"));

/// Who is on the other end of a connection, as far as the kernel says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerInfo {
    pub uid: Option<u32>,
    pub pid: Option<u32>,
}

#[cfg(target_os = "linux")]
pub fn peer_info(stream: &UnixStream) -> PeerInfo {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    match getsockopt(stream, PeerCredentials) {
        Ok(cred) => PeerInfo {
            uid: Some(cred.uid()),
            pid: u32::try_from(cred.pid()).ok(),
        },
        Err(_) => PeerInfo {
            uid: None,
            pid: None,
        },
    }
}

#[cfg(target_os = "macos")]
pub fn peer_info(stream: &UnixStream) -> PeerInfo {
    use nix::sys::socket::{
        getsockopt,
        sockopt::{LocalPeerCred, LocalPeerPid},
    };
    PeerInfo {
        uid: getsockopt(stream, LocalPeerCred).ok().map(|c| c.uid()),
        pid: getsockopt(stream, LocalPeerPid)
            .ok()
            .and_then(|p| u32::try_from(p).ok()),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn peer_info(_stream: &UnixStream) -> PeerInfo {
    PeerInfo {
        uid: None,
        pid: None,
    }
}

/// Whether a peer with this uid may talk to the daemon.
///
/// A known uid must be the daemon's own. When the platform exposes no credentials (`None`) the
/// answer is yes: there the 0700 state directory is the only barrier. Supported platforms treat
/// a missing uid as a failure in [`serve`](Server::serve) before this decision is made.
pub fn accept_peer(peer_uid: Option<u32>, my_uid: u32) -> bool {
    match peer_uid {
        Some(uid) => uid == my_uid,
        None => true,
    }
}

fn peer_allowed(peer: &PeerInfo, my_uid: u32) -> bool {
    if CREDENTIALS_SUPPORTED && peer.uid.is_none() {
        return false;
    }
    accept_peer(peer.uid, my_uid)
}

/// What a handler answers with.
pub enum HandlerReply {
    Data(Value),
    /// The reply to `task.attach`: after `data` the connection carries binary frames wired to
    /// `session`.
    Attach {
        data: Value,
        session: Box<dyn AttachSession>,
    },
}

pub trait RequestHandler: Send + Sync {
    /// Decides once per connection, right after it is accepted and before any request is read,
    /// whether this peer may talk to the daemon at all. An error is sent to the peer as the only
    /// reply and the connection is closed; `call` is never reached.
    fn admit(&self, peer: &PeerInfo) -> Result<()>;

    fn call(&self, peer: &PeerInfo, cmd: &str, args: Value) -> Result<HandlerReply>;
}

/// Reads from a socket under one deadline for everything read until the deadline is replaced or
/// cleared, instead of a fresh timeout per `read` call.
struct DeadlineReader {
    stream: UnixStream,
    deadline: Option<Instant>,
}

impl DeadlineReader {
    fn new(stream: UnixStream) -> DeadlineReader {
        DeadlineReader {
            stream,
            deadline: None,
        }
    }

    fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = Some(deadline);
    }

    /// For the attach phase, where the terminal may stay quiet for hours.
    fn clear_deadline(&mut self) {
        self.deadline = None;
        let _ = self.stream.set_read_timeout(None);
    }
}

impl Read for DeadlineReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(deadline) = self.deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the request was not complete in time",
                ));
            }
            self.stream.set_read_timeout(Some(remaining))?;
        }
        self.stream.read(buf)
    }
}

/// The daemon side of an attached terminal. Dropping it detaches; the worker keeps running.
pub trait AttachSession: Send {
    /// A fresh channel of raw output bytes. The channel disconnecting means the session ended.
    fn output(&mut self) -> Receiver<Vec<u8>>;
    fn write_input(&self, bytes: &[u8]) -> io::Result<()>;
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()>;
}

fn my_uid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn mode_of(meta: &fs::Metadata) -> u32 {
    meta.permissions().mode() & 0o777
}

/// The directory holding the socket must be a real directory (not a symlink) that only this user
/// can enter.
fn check_private_dir(dir: &Path, uid: u32) -> Result<()> {
    let meta = fs::symlink_metadata(dir)
        .map_err(|e| FleetError::io(&format!("the state directory {}", dir.display()), &e))?;
    if !meta.file_type().is_dir() {
        return Err(FleetError::forbidden(format!(
            "{} is not a real directory (symlinks are refused)",
            dir.display()
        )));
    }
    if meta.uid() != uid {
        return Err(FleetError::forbidden(format!(
            "{} is owned by another user",
            dir.display()
        )));
    }
    if mode_of(&meta) != 0o700 {
        return Err(FleetError::forbidden(format!(
            "{} must have mode 0700 but has {:04o}",
            dir.display(),
            mode_of(&meta)
        )));
    }
    Ok(())
}

fn check_socket_file(socket: &Path, uid: u32) -> Result<()> {
    let meta = fs::symlink_metadata(socket).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            FleetError::new(
                ErrorCode::DaemonDown,
                format!("no control socket at {}", socket.display()),
            )
        } else {
            FleetError::io(&format!("the socket {}", socket.display()), &e)
        }
    })?;
    if !meta.file_type().is_socket() {
        return Err(FleetError::forbidden(format!(
            "{} is not a socket (symlinks are refused)",
            socket.display()
        )));
    }
    if meta.uid() != uid {
        return Err(FleetError::forbidden(format!(
            "{} is owned by another user",
            socket.display()
        )));
    }
    if mode_of(&meta) != 0o600 {
        return Err(FleetError::forbidden(format!(
            "{} must have mode 0600 but has {:04o}",
            socket.display(),
            mode_of(&meta)
        )));
    }
    Ok(())
}

/// Checks the socket and its directory the way the server requires them: directory 0700 and ours,
/// socket 0600 and ours, no symlinks. Used by `fleet doctor` and by tests.
pub fn verify_socket_permissions(socket: &Path) -> Result<()> {
    let uid = my_uid();
    let parent = socket
        .parent()
        .ok_or_else(|| FleetError::invalid("the socket path has no parent directory"))?;
    check_private_dir(parent, uid)?;
    check_socket_file(socket, uid)
}

/// Removes the socket file, but only if it is a socket; never follows or deletes anything else.
pub fn remove_socket(socket: &Path) {
    if let Ok(meta) = fs::symlink_metadata(socket) {
        if meta.file_type().is_socket() {
            let _ = fs::remove_file(socket);
        }
    }
}

/// State shared between the accept loop and connection threads.
struct Shared {
    active: AtomicUsize,
    closing: AtomicBool,
    next_id: AtomicU64,
    /// A clone of every open connection so that stopping the server can shut them down.
    streams: Mutex<HashMap<u64, UnixStream>>,
}

/// Counts a connection while it lives and forgets its stream when it ends.
struct ConnGuard {
    shared: Arc<Shared>,
    id: u64,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        lock(&self.shared.streams).remove(&self.id);
        self.shared.active.fetch_sub(1, Ordering::AcqRel);
    }
}

pub struct Server {
    listener: UnixListener,
    path: PathBuf,
    handler: Arc<dyn RequestHandler>,
    my_uid: u32,
    /// The capability token every request but `ping` must carry.
    token: Arc<str>,
    request_timeout: Duration,
    shared: Arc<Shared>,
}

impl Server {
    /// Binds the control socket. The parent directory must already exist, owned by this user with
    /// mode 0700. A leftover socket file from a dead daemon is replaced; a socket somebody
    /// answers on is a conflict, and anything that is not a socket is never touched.
    pub fn bind(socket: &Path, handler: Arc<dyn RequestHandler>) -> Result<Server> {
        let uid = my_uid();
        let parent = socket
            .parent()
            .ok_or_else(|| FleetError::invalid("the socket path has no parent directory"))?;
        check_private_dir(parent, uid)?;

        match fs::symlink_metadata(socket) {
            Ok(meta) => {
                if !meta.file_type().is_socket() {
                    return Err(FleetError::conflict(format!(
                        "{} exists and is not a socket; refusing to replace it",
                        socket.display()
                    )));
                }
                match UnixStream::connect(socket) {
                    Ok(_) => {
                        return Err(FleetError::conflict(format!(
                            "a daemon is already listening on {}",
                            socket.display()
                        )))
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                        ) =>
                    {
                        fs::remove_file(socket)
                            .map_err(|e| FleetError::io("removing the stale control socket", &e))?;
                    }
                    Err(e) => {
                        return Err(FleetError::io("probing the existing control socket", &e))
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(FleetError::io("inspecting the control socket path", &e)),
        }

        // The token is created only now: a daemon that is already listening (conflict above)
        // keeps its own. Whatever fails from here on takes the new token away again.
        let token_file = token_path(socket);
        let token = create_token_file(&token_file)?;
        let bound = UnixListener::bind(socket).map_err(|e| match e.kind() {
            io::ErrorKind::InvalidInput => FleetError::invalid(format!(
                "cannot bind {}: the path is too long for a Unix socket",
                socket.display()
            )),
            _ => FleetError::io("binding the control socket", &e),
        });
        let listener = match bound {
            Ok(l) => l,
            Err(e) => {
                remove_token_file(&token_file);
                return Err(e);
            }
        };
        // The umask decides the mode bind creates; fix it explicitly, then check what is on disk.
        let finish = || -> Result<()> {
            fs::set_permissions(socket, fs::Permissions::from_mode(0o600))
                .map_err(|e| FleetError::io("restricting the control socket", &e))?;
            check_socket_file(socket, uid)?;
            listener
                .set_nonblocking(true)
                .map_err(|e| FleetError::io("configuring the control socket", &e))
        };
        if let Err(e) = finish() {
            remove_socket(socket);
            remove_token_file(&token_file);
            return Err(e);
        }
        Ok(Server {
            listener,
            path: socket.to_path_buf(),
            handler,
            my_uid: uid,
            token: Arc::from(token),
            request_timeout: REQUEST_TIMEOUT,
            shared: Arc::new(Shared {
                active: AtomicUsize::new(0),
                closing: AtomicBool::new(false),
                next_id: AtomicU64::new(0),
                streams: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn local_path(&self) -> &Path {
        &self.path
    }

    /// Accepts connections until `stop` is set, then closes every open connection.
    pub fn serve(&self, stop: &AtomicBool) -> Result<()> {
        let mut failures = 0u32;
        let outcome = loop {
            if stop.load(Ordering::Acquire) {
                break Ok(());
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    failures = 0;
                    self.dispatch(stream);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    failures += 1;
                    if failures > MAX_ACCEPT_FAILURES {
                        break Err(FleetError::io("accepting control connections", &e));
                    }
                    thread::sleep(POLL);
                }
            }
        };
        self.shared.closing.store(true, Ordering::Release);
        self.close_connections();
        outcome
    }

    fn close_connections(&self) {
        // A connection whose request was already answered by the engine may still be writing the
        // reply: the `shutdown` command itself is the case (the engine stops right after it
        // answers). Give those a moment to finish before the rest is closed.
        let grace = Instant::now() + SHUTDOWN_GRACE;
        while self.shared.active.load(Ordering::Acquire) > 0 && Instant::now() < grace {
            thread::sleep(Duration::from_millis(5));
        }
        for stream in lock(&self.shared.streams).values() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        while self.shared.active.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn dispatch(&self, stream: UnixStream) {
        let prior = self.shared.active.fetch_add(1, Ordering::AcqRel);
        let guard = ConnGuard {
            shared: Arc::clone(&self.shared),
            id: self.shared.next_id.fetch_add(1, Ordering::Relaxed),
        };
        if prior >= MAX_CONNECTIONS {
            reject(
                stream,
                &FleetError::limit(format!(
                    "too many control connections (at most {MAX_CONNECTIONS})"
                )),
            );
            return;
        }
        match stream.try_clone() {
            Ok(clone) => {
                lock(&self.shared.streams).insert(guard.id, clone);
            }
            Err(_) => return,
        }
        let handler = Arc::clone(&self.handler);
        let cfg = ConnConfig {
            uid: self.my_uid,
            token: Arc::clone(&self.token),
            request_timeout: self.request_timeout,
        };
        // If the thread cannot start, the closure (and with it the guard) is dropped and the
        // connection closes.
        let _ = thread::Builder::new()
            .name("fleet-conn".to_string())
            .spawn(move || serve_connection(stream, &*handler, &cfg, &guard));
    }
}

fn reject(mut stream: UnixStream, err: &FleetError) {
    // Accepted sockets may inherit O_NONBLOCK from the listener on BSD-derived systems.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    let _ = send_line(&mut stream, encode_err(err));
}

fn send_line(w: &mut impl Write, mut line: String) -> io::Result<()> {
    line.push('\n');
    w.write_all(line.as_bytes())
}

fn send_ok(w: &mut impl Write, data: &Value) -> io::Result<()> {
    let line = encode_ok(data);
    if line.len() > MAX_RESPONSE_BYTES {
        return send_line(
            w,
            encode_err(&FleetError::limit(format!(
                "the reply is larger than {MAX_RESPONSE_BYTES} bytes"
            ))),
        );
    }
    send_line(w, line)
}

/// What a connection thread needs besides the stream and the handler.
struct ConnConfig {
    uid: u32,
    token: Arc<str>,
    request_timeout: Duration,
}

/// Sends `err` as the only reply and ends the connection. The write side is shut down first
/// (the client reads the refusal followed by end of stream) and what the client still sends is
/// read and dropped for a moment, so closing a socket with unread data cannot turn into a reset
/// that destroys the refusal before the client reads it. Both the time and the amount are
/// bounded, so a refused client does not hold the connection slot.
fn refuse(stream: &mut UnixStream, err: &FleetError) {
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let _ = send_line(stream, encode_err(err));
    let _ = stream.shutdown(Shutdown::Write);
    let deadline = Instant::now() + REFUSAL_DRAIN;
    let mut drained = 0usize;
    let mut buf = [0u8; 4096];
    let mut source: &UnixStream = stream;
    while drained < REFUSAL_DRAIN_BYTES {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || source.set_read_timeout(Some(remaining)).is_err() {
            break;
        }
        match source.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained = drained.saturating_add(n),
        }
    }
}

fn handler_failed() -> FleetError {
    FleetError::internal("the daemon failed while handling the request")
}

fn serve_connection(
    stream: UnixStream,
    handler: &dyn RequestHandler,
    cfg: &ConnConfig,
    guard: &ConnGuard,
) {
    let accepted = Instant::now();
    // Accepted sockets may inherit O_NONBLOCK from the listener on BSD-derived systems.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let peer = peer_info(&stream);
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut writer = stream;
    if !peer_allowed(&peer, cfg.uid) {
        refuse(
            &mut writer,
            &FleetError::forbidden("this daemon only serves the user that started it"),
        );
        return;
    }
    // Who may talk to the daemon is decided here, once, before a single request byte is read;
    // a refusal closes the connection at once. Nothing a peer does later (exiting, handing the
    // socket on) can change that decision.
    let admitted = catch_unwind(AssertUnwindSafe(|| handler.admit(&peer)))
        .unwrap_or_else(|_| Err(handler_failed()));
    if let Err(e) = admitted {
        refuse(&mut writer, &e);
        return;
    }
    let mut reader = BufReader::with_capacity(8 * 1024, DeadlineReader::new(read_half));
    // The first request is due `request_timeout` after the accept, later ones that long after
    // the previous reply.
    let mut due = accepted + cfg.request_timeout;
    loop {
        if guard.shared.closing.load(Ordering::Acquire) {
            return;
        }
        reader.get_mut().set_deadline(due);
        let line = match read_line_bounded(&mut reader, MAX_MESSAGE_BYTES) {
            Ok(Some(line)) => line,
            Ok(None) => return,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                // The rest of the line is unread, so the stream cannot be resynchronized.
                let _ = send_line(
                    &mut writer,
                    encode_err(&FleetError::invalid(format!("bad request: {e}"))),
                );
                return;
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                refuse(
                    &mut writer,
                    &FleetError::new(
                        ErrorCode::Timeout,
                        "no complete request arrived in time; closing the connection",
                    ),
                );
                return;
            }
            Err(_) => return,
        };
        let request = match parse_request(&line) {
            Ok(parsed) => parsed,
            Err(e) => {
                let _ = send_line(&mut writer, encode_err(&e));
                return;
            }
        };
        // `ping` only proves that something answers; everything else needs the token.
        if request.cmd != "ping"
            && !request
                .token
                .as_deref()
                .is_some_and(|presented| tokens_equal(presented, &cfg.token))
        {
            refuse(
                &mut writer,
                &FleetError::forbidden(
                    "a valid control token is required; `fleet` reads it from the state directory",
                ),
            );
            return;
        }
        let (cmd, args) = (request.cmd, request.args);
        let reply = match catch_unwind(AssertUnwindSafe(|| handler.call(&peer, &cmd, args))) {
            Ok(reply) => reply,
            Err(_) => Err(handler_failed()),
        };
        match reply {
            Ok(HandlerReply::Data(data)) => {
                if send_ok(&mut writer, &data).is_err() {
                    return;
                }
            }
            Ok(HandlerReply::Attach { data, session }) => {
                if send_ok(&mut writer, &data).is_err() {
                    return;
                }
                run_attach(reader, writer, session);
                return;
            }
            // A refusal ends the connection: whoever was refused gets nothing more from it.
            Err(e) if e.code == ErrorCode::Forbidden => {
                refuse(&mut writer, &e);
                return;
            }
            Err(e) => {
                if send_line(&mut writer, encode_err(&e)).is_err() {
                    return;
                }
            }
        }
        due = Instant::now() + cfg.request_timeout;
    }
}

/// Binary-frame phase of an attach connection. This thread reads client frames; a second thread
/// is the only writer and sends the session's output.
fn run_attach(
    mut reader: BufReader<DeadlineReader>,
    writer: UnixStream,
    mut session: Box<dyn AttachSession>,
) {
    // An attached terminal may be quiet for a long time: the request deadline ends here.
    reader.get_mut().clear_deadline();
    let output = session.output();
    let Ok(forward_stream) = writer.try_clone() else {
        return;
    };
    let done = Arc::new(AtomicBool::new(false));
    let forward_done = Arc::clone(&done);
    let Ok(forwarder) = thread::Builder::new()
        .name("fleet-attach-out".to_string())
        .spawn(move || forward_output(output, forward_stream, &forward_done))
    else {
        return;
    };

    loop {
        match read_frame(&mut reader) {
            Ok(Some((KIND_INPUT, bytes))) => {
                // A session that cannot take input has ended or is ending; its output channel
                // reports that, so a failed write is not a reason to cut the connection.
                let _ = session.write_input(&bytes);
            }
            Ok(Some((KIND_RESIZE, payload))) => match decode_resize(&payload) {
                Some((cols, rows)) => {
                    if cols > 0 && rows > 0 {
                        let _ = session.resize(cols, rows);
                    }
                }
                None => break,
            },
            // Detach, a closed connection, a bad frame or an unknown kind all end the attach.
            _ => break,
        }
    }
    done.store(true, Ordering::Release);
    drop(session);
    let _ = writer.shutdown(Shutdown::Both);
    let _ = forwarder.join();
}

fn forward_output(output: Receiver<Vec<u8>>, mut stream: UnixStream, done: &AtomicBool) {
    // One sanitizer for the whole attach: an escape sequence split across chunks is still caught.
    let mut sanitizer = Sanitizer::new(Mode::Passthrough);
    let mut text = String::new();
    loop {
        if done.load(Ordering::Acquire) {
            break;
        }
        match output.recv_timeout(FORWARD_POLL) {
            Ok(chunk) => {
                text.clear();
                sanitizer.feed(&chunk, &mut text);
                if write_text(&mut stream, KIND_OUTPUT, &text).is_err() {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // The client detached first: the session was dropped on purpose, not ended.
                if !done.load(Ordering::Acquire) {
                    text.clear();
                    sanitizer.finish(&mut text);
                    let _ = write_text(&mut stream, KIND_OUTPUT, &text)
                        .and_then(|()| write_text(&mut stream, KIND_EXIT, "session ended"));
                }
                break;
            }
        }
    }
    // Wakes the reading thread, which would otherwise wait for the client.
    let _ = stream.shutdown(Shutdown::Both);
}

/// Sends `text` as frames of at most `MAX_FRAME_PAYLOAD` bytes, cut on character boundaries.
fn write_text(stream: &mut UnixStream, kind: u8, text: &str) -> io::Result<()> {
    let mut rest = text;
    while !rest.is_empty() {
        let piece = head_str(rest, MAX_FRAME_PAYLOAD);
        if piece.is_empty() {
            break;
        }
        write_frame(stream, kind, piece.as_bytes())?;
        rest = rest.get(piece.len()..).unwrap_or("");
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A fake handler and a running server for the tests of this module and of the client.

    use super::*;
    use crate::testutil::TempDir;
    use std::sync::mpsc::{channel, Sender};
    use std::thread::JoinHandle;

    pub fn obj(pairs: &[(&str, Value)]) -> Value {
        Value::object(pairs.iter().map(|(k, v)| (k.to_string(), v.clone())))
    }

    /// Commands: `ping`, `echo` (returns its args), `fail` (conflict with subject), `boom`
    /// (panics), `peer` (reports the peer's uid), `attach` (one attach session, once).
    pub struct Fake {
        attach_rx: Mutex<Option<Receiver<Vec<u8>>>>,
        pub events: Arc<Mutex<Vec<String>>>,
        pub input: Arc<Mutex<Vec<u8>>>,
        /// While set, `admit` refuses every connection.
        pub refuse_admit: AtomicBool,
        /// Connections that reached `admit`, and requests that reached `call`.
        pub admits: AtomicUsize,
        pub calls: AtomicUsize,
    }

    impl Fake {
        /// The sender feeds the output of the attach session.
        pub fn new() -> (Arc<Fake>, Sender<Vec<u8>>) {
            let (tx, rx) = channel();
            let fake = Fake {
                attach_rx: Mutex::new(Some(rx)),
                events: Arc::new(Mutex::new(Vec::new())),
                input: Arc::new(Mutex::new(Vec::new())),
                refuse_admit: AtomicBool::new(false),
                admits: AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
            };
            (Arc::new(fake), tx)
        }

        pub fn has_event(&self, wanted: &str) -> bool {
            lock(&self.events).iter().any(|e| e == wanted)
        }
    }

    struct FakeSession {
        rx: Option<Receiver<Vec<u8>>>,
        events: Arc<Mutex<Vec<String>>>,
        input: Arc<Mutex<Vec<u8>>>,
    }

    impl AttachSession for FakeSession {
        fn output(&mut self) -> Receiver<Vec<u8>> {
            self.rx.take().unwrap_or_else(|| channel().1)
        }
        fn write_input(&self, bytes: &[u8]) -> io::Result<()> {
            lock(&self.input).extend_from_slice(bytes);
            lock(&self.events).push(format!("input:{}", bytes.len()));
            Ok(())
        }
        fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
            lock(&self.events).push(format!("resize:{cols}x{rows}"));
            Ok(())
        }
    }

    impl Drop for FakeSession {
        fn drop(&mut self) {
            lock(&self.events).push("dropped".to_string());
        }
    }

    impl RequestHandler for Fake {
        fn admit(&self, _peer: &PeerInfo) -> Result<()> {
            self.admits.fetch_add(1, Ordering::SeqCst);
            if self.refuse_admit.load(Ordering::SeqCst) {
                return Err(FleetError::forbidden("not welcome here"));
            }
            Ok(())
        }

        fn call(&self, peer: &PeerInfo, cmd: &str, args: Value) -> Result<HandlerReply> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match cmd {
                "ping" => Ok(HandlerReply::Data(obj(&[(
                    "schema",
                    Value::str("fleet.ping/1"),
                )]))),
                "echo" => Ok(HandlerReply::Data(obj(&[
                    ("schema", Value::str("fleet.echo/1")),
                    ("args", args),
                ]))),
                "peer" => Ok(HandlerReply::Data(obj(&[
                    ("schema", Value::str("fleet.peer/1")),
                    ("uid", peer.uid.map(Value::int).unwrap_or(Value::Null)),
                ]))),
                "fail" => Err(FleetError::conflict("not allowed now").with_subject("a1")),
                "forbid" => Err(FleetError::forbidden("go away")),
                // an answer that takes a while, as the engine's does around a shutdown
                "slow" => {
                    thread::sleep(Duration::from_millis(120));
                    Ok(HandlerReply::Data(obj(&[(
                        "schema",
                        Value::str("fleet.slow/1"),
                    )])))
                }
                "boom" => panic!("handler exploded"),
                "attach" | "task.attach" => {
                    let Some(rx) = lock(&self.attach_rx).take() else {
                        return Err(FleetError::conflict("already attached"));
                    };
                    Ok(HandlerReply::Attach {
                        data: obj(&[("schema", Value::str("fleet.attach/1"))]),
                        session: Box::new(FakeSession {
                            rx: Some(rx),
                            events: Arc::clone(&self.events),
                            input: Arc::clone(&self.input),
                        }),
                    })
                }
                _ => Err(FleetError::not_found("unknown command")),
            }
        }
    }

    /// A server bound in a fresh private directory and serving on its own thread.
    pub struct TestServer {
        pub _dir: TempDir,
        pub path: PathBuf,
        pub server: Arc<Server>,
        pub stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    /// A 0700 directory named `d` inside `dir`; the paths stay short enough for `sun_path`.
    pub fn private_dir(dir: &TempDir) -> PathBuf {
        let state = dir.join("d");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        state
    }

    impl TestServer {
        pub fn start(handler: Arc<dyn RequestHandler>) -> TestServer {
            TestServer::start_tuned(handler, |_| {})
        }

        /// Like `start`, after `tune` changed the server's settings.
        pub fn start_tuned(
            handler: Arc<dyn RequestHandler>,
            tune: impl FnOnce(&mut Server),
        ) -> TestServer {
            let dir = TempDir::new("sv");
            let path = private_dir(&dir).join("s");
            let mut server = Server::bind(&path, handler).unwrap();
            tune(&mut server);
            let server = Arc::new(server);
            let stop = Arc::new(AtomicBool::new(false));
            let thread = {
                let (server, stop) = (Arc::clone(&server), Arc::clone(&stop));
                thread::spawn(move || {
                    let _ = server.serve(&stop);
                })
            };
            TestServer {
                _dir: dir,
                path,
                server,
                stop,
                thread: Some(thread),
            }
        }

        pub fn shut_down(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.shut_down();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::protocol::KIND_DETACH;
    use crate::protocol::{encode_request, encode_resize, parse_response};
    use crate::testutil::{wait_until, TempDir};

    /// A plain line-oriented client with timeouts, so a server bug fails a test instead of
    /// hanging it.
    struct Raw {
        writer: UnixStream,
        reader: BufReader<UnixStream>,
        /// The token of the server at `path`, as `fleet` itself would read it.
        token: Option<String>,
    }

    impl Raw {
        fn connect(path: &Path) -> Raw {
            let stream = UnixStream::connect(path).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            Raw {
                reader: BufReader::new(stream.try_clone().unwrap()),
                writer: stream,
                token: crate::statedir::read_token(path).ok(),
            }
        }

        /// A client that does not present the token.
        fn connect_without_token(path: &Path) -> Raw {
            let mut raw = Raw::connect(path);
            raw.token = None;
            raw
        }

        fn send_line(&mut self, line: &str) {
            self.writer.write_all(line.as_bytes()).unwrap();
            self.writer.write_all(b"\n").unwrap();
        }

        fn read_reply(&mut self) -> Result<Value> {
            let line = read_line_bounded(&mut self.reader, MAX_RESPONSE_BYTES)
                .unwrap()
                .expect("a reply line");
            parse_response(&line)
        }

        fn call(&mut self, cmd: &str, args: Value) -> Result<Value> {
            self.send_line(&encode_request(cmd, &args, self.token.as_deref()));
            self.read_reply()
        }

        /// True when the server closed the connection (EOF or reset) without sending more.
        fn closed(&mut self) -> bool {
            let mut byte = [0u8; 1];
            !matches!(self.reader.read(&mut byte), Ok(n) if n > 0)
        }
    }

    fn start() -> TestServer {
        TestServer::start(Fake::new().0)
    }

    #[test]
    fn a_reply_that_is_being_produced_when_the_server_stops_is_still_delivered() {
        // `fleet shutdown` is answered by an engine that stops right afterwards; the daemon sets the
        // server's stop flag at that moment, and the reply must not be cut off
        let mut s = start();
        let mut c = Raw::connect(&s.path);
        c.send_line(&encode_request("slow", &Value::Null, c.token.as_deref()));
        thread::sleep(Duration::from_millis(30));
        s.stop.store(true, Ordering::Release);
        let reply = c.read_reply().expect("the reply arrives");
        assert_eq!(
            reply.get("schema").and_then(Value::as_str),
            Some("fleet.slow/1")
        );
        s.shut_down();
    }

    #[test]
    fn request_reply_round_trip_on_one_connection() {
        let ts = start();
        let mut c = Raw::connect(&ts.path);
        let pong = c.call("ping", Value::Null).unwrap();
        assert_eq!(
            pong.get("schema").and_then(Value::as_str),
            Some("fleet.ping/1")
        );
        let args = obj(&[("task", Value::str("r1/t1")), ("n", Value::int(7))]);
        let echoed = c.call("echo", args.clone()).unwrap();
        assert_eq!(echoed.get("args"), Some(&args));
        // the same connection keeps working after an error reply
        let err = c.call("fail", Value::Null).unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
        assert_eq!(err.message, "not allowed now");
        assert_eq!(err.subject.as_deref(), Some("a1"));
        let err = c.call("nope", Value::Null).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        assert!(c.call("ping", Value::Null).is_ok());
    }

    #[test]
    fn handler_sees_the_peer_uid() {
        let ts = start();
        let mut c = Raw::connect(&ts.path);
        let data = c.call("peer", Value::Null).unwrap();
        if CREDENTIALS_SUPPORTED {
            assert_eq!(
                data.get("uid").and_then(Value::as_u64),
                Some(u64::from(my_uid()))
            );
        }
    }

    #[test]
    fn oversized_request_gets_an_error_and_the_connection_closes() {
        let ts = start();
        let mut c = Raw::connect(&ts.path);
        let mut huge = vec![b'a'; MAX_MESSAGE_BYTES + 100];
        huge.push(b'\n');
        // The server may close before everything is written; that is fine.
        let _ = c.writer.write_all(&huge);
        let err = c.read_reply().unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);
        assert!(c.closed());
        // other connections are unaffected
        assert!(Raw::connect(&ts.path).call("ping", Value::Null).is_ok());
    }

    #[test]
    fn a_line_of_exactly_the_maximum_size_is_read() {
        let ts = start();
        let mut c = Raw::connect(&ts.path);
        // {"v":1,"cmd":"echo","args":{"k":"...."},"token":"<64 hex>"} padded to exactly
        // MAX_MESSAGE_BYTES
        let token = c.token.clone().unwrap();
        let frame = format!(r#"{{"v":1,"cmd":"echo","args":{{"k":""}},"token":"{token}"}}"#);
        let pad = "p".repeat(MAX_MESSAGE_BYTES - frame.len());
        let line = format!(r#"{{"v":1,"cmd":"echo","args":{{"k":"{pad}"}},"token":"{token}"}}"#);
        assert_eq!(line.len(), MAX_MESSAGE_BYTES);
        c.send_line(&line);
        assert!(c.read_reply().is_ok());
    }

    #[test]
    fn malformed_requests_get_invalid_and_close() {
        let ts = start();
        for bad in [
            "{not json}".as_bytes().to_vec(),
            br#"{"v":2,"cmd":"ping"}"#.to_vec(),
            br#"{"v":1,"cmd":"PING"}"#.to_vec(),
            br#"{"v":1,"cmd":"ping","args":[]}"#.to_vec(),
            vec![0xff, 0xfe, b'x'],
        ] {
            let mut c = Raw::connect(&ts.path);
            c.writer.write_all(&bad).unwrap();
            c.writer.write_all(b"\n").unwrap();
            let err = c.read_reply().unwrap_err();
            assert_eq!(err.code, ErrorCode::Invalid, "{bad:?}");
            assert!(c.closed(), "{bad:?}");
        }
    }

    #[test]
    fn handler_panic_becomes_an_internal_error_and_the_server_keeps_serving() {
        let ts = start();
        let mut c = Raw::connect(&ts.path);
        let err = c.call("boom", Value::Null).unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);
        assert!(
            !err.message.contains("exploded"),
            "panic text must not leak"
        );
        assert!(c.call("ping", Value::Null).is_ok());
        assert!(Raw::connect(&ts.path).call("ping", Value::Null).is_ok());
    }

    #[test]
    fn connection_limit_rejects_extras_and_recovers() {
        let ts = start();
        let mut open = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            let mut c = Raw::connect(&ts.path);
            assert!(c.call("ping", Value::Null).is_ok());
            open.push(c);
        }
        let mut extra = Raw::connect(&ts.path);
        let err = extra.read_reply().unwrap_err();
        assert_eq!(err.code, ErrorCode::Limit);
        assert!(extra.closed());

        drop(open.pop());
        let recovered = wait_until(3000, || {
            let mut c = Raw::connect(&ts.path);
            c.send_line(&encode_request("ping", &Value::Null, None));
            c.read_reply().is_ok()
        });
        assert!(recovered, "a freed slot must be usable again");
    }

    #[test]
    fn a_stale_socket_is_replaced() {
        let dir = TempDir::new("sv");
        let path = private_dir(&dir).join("s");
        drop(UnixListener::bind(&path).unwrap()); // leaves the file behind
        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_socket());
        let server = Server::bind(&path, Fake::new().0).unwrap();
        assert_eq!(server.local_path(), path.as_path());
        verify_socket_permissions(&path).unwrap();
    }

    #[test]
    fn a_live_socket_is_a_conflict_and_is_left_alone() {
        let ts = start();
        let err = Server::bind(&ts.path, Fake::new().0)
            .err()
            .expect("second bind fails");
        assert_eq!(err.code, ErrorCode::Conflict);
        assert!(err.message.contains("already listening"));
        // the first daemon still answers
        assert!(Raw::connect(&ts.path).call("ping", Value::Null).is_ok());
    }

    #[test]
    fn a_path_that_is_not_a_socket_is_never_replaced() {
        let dir = TempDir::new("sv");
        let state = private_dir(&dir);
        let file = state.join("s");
        fs::write(&file, "precious").unwrap();
        let err = Server::bind(&file, Fake::new().0).err().unwrap();
        assert_eq!(err.code, ErrorCode::Conflict);
        assert_eq!(fs::read_to_string(&file).unwrap(), "precious");

        // a symlink to a real socket is not followed either
        let target = state.join("t");
        let _listener = UnixListener::bind(&target).unwrap();
        let link = state.join("l");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = Server::bind(&link, Fake::new().0).err().unwrap();
        assert_eq!(err.code, ErrorCode::Conflict);
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn a_directory_that_others_can_enter_is_refused() {
        let dir = TempDir::new("sv");
        let state = private_dir(&dir);
        for mode in [0o755, 0o750, 0o701, 0o777] {
            fs::set_permissions(&state, fs::Permissions::from_mode(mode)).unwrap();
            let err = Server::bind(&state.join("s"), Fake::new().0).err().unwrap();
            assert_eq!(err.code, ErrorCode::Forbidden, "{mode:o}");
            assert!(!state.join("s").exists());
        }
    }

    #[test]
    fn a_symlinked_state_directory_is_refused() {
        let dir = TempDir::new("sv");
        let real = private_dir(&dir);
        let link = dir.join("ln");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = Server::bind(&link.join("s"), Fake::new().0).err().unwrap();
        assert_eq!(err.code, ErrorCode::Forbidden);
    }

    #[test]
    fn a_missing_directory_is_an_error() {
        let dir = TempDir::new("sv");
        assert!(Server::bind(&dir.join("none").join("s"), Fake::new().0).is_err());
    }

    #[test]
    fn socket_mode_is_0600_and_verification_catches_changes() {
        let ts = start();
        let meta = fs::symlink_metadata(&ts.path).unwrap();
        assert_eq!(mode_of(&meta), 0o600);
        verify_socket_permissions(&ts.path).unwrap();

        fs::set_permissions(&ts.path, fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(
            verify_socket_permissions(&ts.path).unwrap_err().code,
            ErrorCode::Forbidden
        );
        fs::set_permissions(&ts.path, fs::Permissions::from_mode(0o600)).unwrap();

        let parent = ts.path.parent().unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            verify_socket_permissions(&ts.path).unwrap_err().code,
            ErrorCode::Forbidden
        );
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
        verify_socket_permissions(&ts.path).unwrap();

        let missing = parent.join("absent");
        assert_eq!(
            verify_socket_permissions(&missing).unwrap_err().code,
            ErrorCode::DaemonDown
        );
    }

    #[test]
    fn remove_socket_only_removes_sockets() {
        let dir = TempDir::new("sv");
        let state = private_dir(&dir);
        let file = state.join("f");
        fs::write(&file, "x").unwrap();
        remove_socket(&file);
        assert!(file.exists());
        let sock = state.join("s");
        drop(UnixListener::bind(&sock).unwrap());
        remove_socket(&sock);
        assert!(!sock.exists());
        remove_socket(&sock); // absent: no-op
    }

    #[test]
    fn accept_peer_decision_table() {
        assert!(accept_peer(Some(501), 501));
        assert!(!accept_peer(Some(0), 501));
        assert!(!accept_peer(Some(502), 501));
        assert!(!accept_peer(Some(501), 0));
        assert!(accept_peer(None, 501));
        // supported platforms never run on the "no credentials" path
        let unknown = PeerInfo {
            uid: None,
            pid: None,
        };
        assert_eq!(peer_allowed(&unknown, 501), !CREDENTIALS_SUPPORTED);
        assert!(peer_allowed(
            &PeerInfo {
                uid: Some(501),
                pid: None
            },
            501
        ));
        assert!(!peer_allowed(
            &PeerInfo {
                uid: Some(0),
                pid: None
            },
            501
        ));
    }

    #[test]
    fn peer_info_reports_the_connecting_process() {
        if !CREDENTIALS_SUPPORTED {
            return;
        }
        let dir = TempDir::new("sv");
        let path = private_dir(&dir).join("s");
        let listener = UnixListener::bind(&path).unwrap();
        let _client = UnixStream::connect(&path).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        let info = peer_info(&accepted);
        assert_eq!(info.uid, Some(my_uid()));
        #[cfg(target_os = "linux")]
        assert_eq!(info.pid, Some(std::process::id()));
    }

    #[test]
    fn a_peer_with_another_uid_is_refused_before_any_request_is_read() {
        if !CREDENTIALS_SUPPORTED {
            return;
        }
        let dir = TempDir::new("sv");
        let path = private_dir(&dir).join("s");
        let mut server = Server::bind(&path, Fake::new().0).unwrap();
        // Pretend the daemon runs as a different user than the (real) peer.
        server.my_uid = my_uid().wrapping_add(1);
        let server = Arc::new(server);
        let stop = Arc::new(AtomicBool::new(false));
        let t = {
            let (server, stop) = (Arc::clone(&server), Arc::clone(&stop));
            thread::spawn(move || {
                let _ = server.serve(&stop);
            })
        };
        let mut c = Raw::connect(&path);
        let err = c.read_reply().unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        assert!(c.closed());
        stop.store(true, Ordering::Release);
        t.join().unwrap();
    }

    #[test]
    fn stopping_the_server_closes_open_connections() {
        let mut ts = start();
        let mut c = Raw::connect(&ts.path);
        assert!(c.call("ping", Value::Null).is_ok());
        ts.shut_down();
        assert!(c.closed());
        assert_eq!(ts.server.shared.active.load(Ordering::Acquire), 0);
    }

    // ---- the control token, admission and deadlines --------------------------------------------

    fn forbidden_and_closed(c: &mut Raw, what: &str) {
        let err = c.read_reply().unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden, "{what}: {err:?}");
        assert!(c.closed(), "{what}: the connection must be closed");
    }

    #[test]
    fn every_command_but_ping_needs_the_token() {
        let (fake, _tx) = Fake::new();
        let ts = TestServer::start(Arc::clone(&fake) as Arc<dyn RequestHandler>);
        for cmd in [
            "echo",
            "fail",
            "peer",
            "attach",
            "task.attach",
            "boom",
            "nope",
            "approve",
        ] {
            let mut c = Raw::connect_without_token(&ts.path);
            c.send_line(&encode_request(cmd, &Value::Null, None));
            forbidden_and_closed(&mut c, cmd);
        }
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            0,
            "no request without the token may reach the handler"
        );
        // ping answers without a token (it proves nothing but liveness) ...
        let mut c = Raw::connect_without_token(&ts.path);
        assert!(c.call("ping", Value::Null).is_ok());
        // ... but the same connection is still refused for anything else
        c.send_line(&encode_request("echo", &Value::Null, None));
        forbidden_and_closed(&mut c, "echo after ping");
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            1,
            "only the ping got through"
        );
        // with the token the very same commands work
        let mut c = Raw::connect(&ts.path);
        assert!(c.call("echo", Value::Null).is_ok());
    }

    #[test]
    fn a_wrong_or_malformed_token_is_refused_and_closes_the_connection() {
        let ts = start();
        let other = start();
        let real = crate::statedir::read_token(&ts.path).unwrap();
        let foreign = crate::statedir::read_token(&other.path).unwrap();
        assert_ne!(real, foreign, "every server has its own token");
        let mut almost = real.clone();
        almost.replace_range(63.., if real.ends_with('0') { "1" } else { "0" });
        for (what, token) in [
            ("another daemon's token", foreign),
            ("one character off", almost),
            ("a prefix", real[..63].to_string()),
            ("too long", format!("{real}0")),
            ("empty", String::new()),
            ("uppercase", real.to_uppercase()),
        ] {
            let mut c = Raw::connect_without_token(&ts.path);
            c.send_line(&encode_request("echo", &Value::Null, Some(&token)));
            forbidden_and_closed(&mut c, what);
        }
        // not a string at all: a malformed request, never accepted
        let mut c = Raw::connect_without_token(&ts.path);
        c.send_line(r#"{"v":1,"cmd":"echo","args":{},"token":12}"#);
        let err = c.read_reply().unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);
        assert!(c.closed());
        // the token inside the arguments is not the token
        let mut c = Raw::connect_without_token(&ts.path);
        let args = obj(&[("token", Value::str(real.as_str()))]);
        c.send_line(&encode_request("echo", &args, None));
        forbidden_and_closed(&mut c, "token in args");
        // and the real one still works afterwards
        assert!(Raw::connect(&ts.path).call("echo", Value::Null).is_ok());
    }

    #[test]
    fn bind_creates_a_fresh_private_token_and_a_new_bind_replaces_it() {
        let dir = TempDir::new("sv");
        let path = private_dir(&dir).join("s");
        let token_file = token_path(&path);
        let first = Server::bind(&path, Fake::new().0).unwrap();
        assert_eq!(mode_of(&fs::symlink_metadata(&token_file).unwrap()), 0o600);
        let first_token = crate::statedir::read_token(&path).unwrap();
        assert_eq!(first_token.len(), 64);
        // the first daemon dies without cleaning up; the next one must not reuse its token
        drop(first);
        let second = Server::bind(&path, Fake::new().0).unwrap();
        let second_token = crate::statedir::read_token(&path).unwrap();
        assert_ne!(first_token, second_token);
        assert_eq!(mode_of(&fs::symlink_metadata(&token_file).unwrap()), 0o600);
        drop(second);
    }

    #[test]
    fn a_failed_bind_never_replaces_the_token_of_a_live_daemon() {
        let ts = start();
        let before = crate::statedir::read_token(&ts.path).unwrap();
        assert!(Server::bind(&ts.path, Fake::new().0).is_err());
        assert_eq!(crate::statedir::read_token(&ts.path).unwrap(), before);
        // the live daemon keeps accepting its own token
        assert!(Raw::connect(&ts.path).call("echo", Value::Null).is_ok());
    }

    #[test]
    fn a_token_path_that_is_a_symlink_stops_the_server_from_binding() {
        let dir = TempDir::new("sv");
        let state = private_dir(&dir);
        let target = dir.write("elsewhere", "precious");
        std::os::unix::fs::symlink(&target, state.join("token")).unwrap();
        let err = Server::bind(&state.join("s"), Fake::new().0).err().unwrap();
        assert_eq!(err.code, ErrorCode::Conflict);
        assert_eq!(fs::read_to_string(&target).unwrap(), "precious");
        assert!(!state.join("s").exists(), "no socket without a token");
    }

    #[test]
    fn a_refused_admit_sends_one_forbidden_line_and_closes_before_any_request_is_read() {
        let (fake, _tx) = Fake::new();
        fake.refuse_admit.store(true, Ordering::SeqCst);
        let ts = TestServer::start(Arc::clone(&fake) as Arc<dyn RequestHandler>);
        // a client that says nothing at all is refused
        let mut quiet = Raw::connect(&ts.path);
        let err = quiet.read_reply().unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        assert_eq!(err.message, "not welcome here");
        assert!(quiet.closed());
        // a client that already pipelined requests (valid token, the worst case) still reads the
        // refusal instead of a reset, and none of its requests is handled
        let mut eager = Raw::connect(&ts.path);
        for _ in 0..3 {
            eager.send_line(&encode_request(
                "echo",
                &Value::Null,
                eager.token.as_deref(),
            ));
        }
        forbidden_and_closed(&mut eager, "pipelined");
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);

        // admission is per connection: lifting the refusal admits new connections
        fake.refuse_admit.store(false, Ordering::SeqCst);
        assert!(Raw::connect(&ts.path).call("echo", Value::Null).is_ok());
    }

    #[test]
    fn a_forbidden_reply_to_a_request_closes_the_connection_other_errors_do_not() {
        let ts = start();
        let mut c = Raw::connect(&ts.path);
        assert_eq!(
            c.call("fail", Value::Null).unwrap_err().code,
            ErrorCode::Conflict
        );
        assert!(
            c.call("ping", Value::Null).is_ok(),
            "an ordinary error keeps the connection"
        );
        let err = c.call("forbid", Value::Null).unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        assert_eq!(err.message, "go away");
        assert!(c.closed(), "a refusal must end the connection");
        assert!(Raw::connect(&ts.path).call("ping", Value::Null).is_ok());
    }

    #[test]
    fn admission_is_decided_once_per_connection_not_per_request() {
        let (fake, _tx) = Fake::new();
        let ts = TestServer::start(Arc::clone(&fake) as Arc<dyn RequestHandler>);
        let mut c = Raw::connect(&ts.path);
        for _ in 0..3 {
            assert!(c.call("echo", Value::Null).is_ok());
        }
        assert_eq!(fake.admits.load(Ordering::SeqCst), 1);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 3);
        // a refusal later does not reach into the connection that was admitted
        fake.refuse_admit.store(true, Ordering::SeqCst);
        assert!(c.call("echo", Value::Null).is_ok());
    }

    fn tuned(request_timeout: Duration) -> (TestServer, Arc<Fake>) {
        let (fake, _tx) = Fake::new();
        let ts = TestServer::start_tuned(Arc::clone(&fake) as Arc<dyn RequestHandler>, |s| {
            s.request_timeout = request_timeout;
        });
        (ts, fake)
    }

    #[test]
    fn a_client_that_trickles_one_request_cannot_hold_the_connection() {
        // A new byte every 100 ms satisfies any per-read timeout; only an overall deadline for
        // the request line ends this connection.
        let (ts, _fake) = tuned(Duration::from_millis(500));
        let mut c = Raw::connect(&ts.path);
        let started = Instant::now();
        let mut sent = 0;
        while sent < 40 {
            if c.writer.write_all(b"a").is_err() {
                break;
            }
            sent += 1;
            thread::sleep(Duration::from_millis(100));
        }
        assert!(
            started.elapsed() < Duration::from_millis(3000) && sent < 40,
            "the server kept the connection for {:?} ({sent} bytes accepted)",
            started.elapsed()
        );
        // the server said why before it closed
        assert_eq!(c.read_reply().unwrap_err().code, ErrorCode::Timeout);
        assert!(c.closed());
        // the slot is free again
        assert!(Raw::connect(&ts.path).call("ping", Value::Null).is_ok());
    }

    #[test]
    fn a_silent_connection_is_cut_off_after_the_first_deadline() {
        let (ts, _fake) = tuned(Duration::from_millis(300));
        let mut c = Raw::connect(&ts.path);
        let started = Instant::now();
        assert_eq!(c.read_reply().unwrap_err().code, ErrorCode::Timeout);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert!(c.closed());
    }

    #[test]
    fn every_request_gets_its_own_deadline_but_each_deadline_is_enforced() {
        let (ts, _fake) = tuned(Duration::from_millis(1500));
        let mut c = Raw::connect(&ts.path);
        // 0.9 s per request is inside one deadline each, though 1.8 s is beyond a single one
        thread::sleep(Duration::from_millis(900));
        assert!(c.call("echo", Value::Null).is_ok());
        thread::sleep(Duration::from_millis(900));
        assert!(c.call("echo", Value::Null).is_ok());
        // then silence: the deadline of the next request runs out
        assert_eq!(c.read_reply().unwrap_err().code, ErrorCode::Timeout);
        assert!(c.closed());
    }

    #[test]
    fn an_attached_terminal_may_stay_quiet_longer_than_the_request_deadline() {
        let (fake, _tx) = Fake::new();
        let ts = TestServer::start_tuned(Arc::clone(&fake) as Arc<dyn RequestHandler>, |s| {
            s.request_timeout = Duration::from_millis(400);
        });
        let mut c = attach(&ts);
        thread::sleep(Duration::from_millis(1200));
        write_frame(&mut c.writer, KIND_INPUT, b"still here").unwrap();
        assert!(wait_until(3000, || fake.has_event("input:10")));
        assert!(
            !fake.has_event("dropped"),
            "the attach must not have been closed"
        );
    }

    // ---- attach -----------------------------------------------------------------------------

    fn attach(ts: &TestServer) -> Raw {
        let mut c = Raw::connect(&ts.path);
        let data = c
            .call("attach", obj(&[("task", Value::str("t1"))]))
            .unwrap();
        assert_eq!(
            data.get("schema").and_then(Value::as_str),
            Some("fleet.attach/1")
        );
        c
    }

    /// Reads output frames until `marker` has been seen, returning all text received.
    fn read_output_until(c: &mut Raw, marker: &str) -> String {
        let mut text = String::new();
        while !text.contains(marker) {
            let (kind, payload) = read_frame(&mut c.reader).unwrap().expect("frame");
            assert_eq!(kind, KIND_OUTPUT);
            assert!(payload.len() <= MAX_FRAME_PAYLOAD);
            text.push_str(std::str::from_utf8(&payload).expect("frames carry UTF-8"));
        }
        text
    }

    #[test]
    fn attach_output_is_sanitized_but_colors_pass() {
        let (fake, tx) = Fake::new();
        let ts = TestServer::start(fake);
        let mut c = attach(&ts);
        tx.send(b"\x1b[31mred\x1b[0m plain ".to_vec()).unwrap();
        // OSC 52 (clipboard write) split over two chunks, then a bidi override and a title.
        tx.send(b"\x1b]52;c;SGVsbG8=".to_vec()).unwrap();
        tx.send(b"\x07after \xe2\x80\xaeevil \x1b]0;title\x07END".to_vec())
            .unwrap();
        let text = read_output_until(&mut c, "END");
        assert!(text.contains("\u{1b}[31mred\u{1b}[0m plain "), "{text:?}");
        assert!(text.contains("after "), "{text:?}");
        assert!(!text.contains("52;c"), "{text:?}");
        assert!(!text.contains("SGVsbG8"), "{text:?}");
        assert!(!text.contains("title"), "{text:?}");
        assert!(!text.contains('\u{202e}'), "{text:?}");
        assert!(!text.contains('\u{7}'), "{text:?}");
    }

    #[test]
    fn attach_output_is_split_into_frames_on_character_boundaries() {
        let (fake, tx) = Fake::new();
        let ts = TestServer::start(fake);
        let mut c = attach(&ts);
        // three-byte characters: the 32 KiB limit falls inside a character, so the split must
        // move back to a boundary
        let big = "€".repeat(40_000);
        let mut payload = big.clone().into_bytes();
        payload.extend_from_slice(b"END");
        tx.send(payload).unwrap();
        let mut frames = 0;
        let mut text = String::new();
        while !text.ends_with("END") {
            let (kind, bytes) = read_frame(&mut c.reader).unwrap().expect("frame");
            assert_eq!(kind, KIND_OUTPUT);
            assert!(bytes.len() <= MAX_FRAME_PAYLOAD);
            text.push_str(std::str::from_utf8(&bytes).expect("each frame is valid UTF-8"));
            frames += 1;
        }
        assert!(frames >= 4, "{frames}");
        assert_eq!(text, format!("{big}END"));
    }

    #[test]
    fn attach_input_and_resize_reach_the_session_and_detach_ends_it() {
        let (fake, _tx) = Fake::new();
        let ts = TestServer::start(Arc::clone(&fake) as Arc<dyn RequestHandler>);
        let mut c = attach(&ts);
        write_frame(&mut c.writer, KIND_INPUT, b"ls -l\n").unwrap();
        write_frame(&mut c.writer, KIND_RESIZE, &encode_resize(100, 30)).unwrap();
        write_frame(&mut c.writer, KIND_RESIZE, &encode_resize(0, 30)).unwrap(); // ignored
        write_frame(&mut c.writer, KIND_INPUT, b"x").unwrap();
        assert!(wait_until(3000, || fake.has_event("input:1")));
        assert!(fake.has_event("resize:100x30"));
        assert!(!fake.has_event("resize:0x30"));
        assert_eq!(*lock(&fake.input), b"ls -l\nx".to_vec());

        write_frame(&mut c.writer, KIND_DETACH, b"").unwrap();
        assert!(
            wait_until(3000, || fake.has_event("dropped")),
            "session released"
        );
        assert!(c.closed());
    }

    #[test]
    fn closing_the_connection_detaches_too() {
        let (fake, _tx) = Fake::new();
        let ts = TestServer::start(Arc::clone(&fake) as Arc<dyn RequestHandler>);
        let c = attach(&ts);
        drop(c);
        assert!(wait_until(3000, || fake.has_event("dropped")));
    }

    #[test]
    fn session_end_sends_an_exit_frame_then_closes() {
        let (fake, tx) = Fake::new();
        let ts = TestServer::start(fake);
        let mut c = attach(&ts);
        tx.send(b"bye".to_vec()).unwrap();
        drop(tx);
        let mut out = String::new();
        loop {
            let (kind, payload) = read_frame(&mut c.reader).unwrap().expect("exit frame");
            let text = String::from_utf8(payload).unwrap();
            if kind == KIND_EXIT {
                assert_eq!(text, "session ended");
                break;
            }
            assert_eq!(kind, KIND_OUTPUT);
            out.push_str(&text);
        }
        assert_eq!(out, "bye");
        assert!(read_frame(&mut c.reader).unwrap().is_none());
    }

    #[test]
    fn hostile_frames_end_the_attach_without_touching_the_session() {
        // unknown kind, oversized length, bad resize payload
        let hostile: [&[u8]; 3] = [
            &[9, 0, 0],
            &[KIND_INPUT, 0xff, 0xff],
            &[KIND_RESIZE, 0, 1, 7],
        ];
        for bytes in hostile {
            let (fake, _tx) = Fake::new();
            let ts = TestServer::start(Arc::clone(&fake) as Arc<dyn RequestHandler>);
            let mut c = attach(&ts);
            c.writer.write_all(bytes).unwrap();
            assert!(wait_until(3000, || fake.has_event("dropped")), "{bytes:?}");
            assert!(lock(&fake.events).iter().all(|e| e == "dropped"));
            assert!(c.closed());
        }
    }
}
