//! The daemon: wires the adapters into the orchestrator and serves the control socket.
//!
//! Threads: one engine thread owns the orchestrator and handles every command and tick in order
//! (so the application layer needs no locking); the socket server runs connection threads that
//! forward requests to the engine over a channel and wait for the answer. Writes into worker
//! terminals (`send`, `attach`) happen on connection threads, so a worker that does not read its
//! input can never block the engine.

use crate::clock::SystemClock;
use crate::contract_fs::FsContractFiles;
use crate::digest::Sha256Digest;
use crate::eventlog::FileEventStore;
use crate::git::GitVcs;
use crate::jobs::ThreadJobRunner;
use crate::logs::FileOutputLogs;
use crate::procid;
use crate::pty::{PtySessionHost, SessionIo, SessionRegistry};
use crate::server::{AttachSession, HandlerReply, PeerInfo, RequestHandler, Server};
use crate::statedir::{DaemonLock, StateLayout};
use crate::undo::AgentUndo;
use crate::web::{WebBackend, WebOptions, WebServer};
use fleet_app::api::{parse_command, Command, Reply};
use fleet_app::config::AppConfig;
use fleet_app::orchestrator::Orchestrator;
use fleet_app::ports::{EventPublisher, Ports, StoredEvent};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::limits::MAX_CONCURRENT_JOBS;
use fleet_domain::supervision::{is_worker_origin, LiveSession, ProcInfo};
use fleet_domain::value::Value;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How often the engine ticks when no request arrives.
const TICK: Duration = Duration::from_millis(50);
/// A request that the engine does not answer within this time fails instead of hanging.
const ENGINE_REPLY_TIMEOUT: Duration = Duration::from_secs(300);
/// Upper bound for a graceful shutdown beyond the configured kill grace.
const SHUTDOWN_EXTRA: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct DaemonOptions {
    pub state_dir: PathBuf,
    pub config_path: Option<PathBuf>,
    /// The local web dashboard; off when `None`.
    pub web: Option<WebOptions>,
}

/// Where the dashboard listens and the secret that opens it. The token lives in memory only: it
/// is printed once by [`run_daemon`] and never stored.
#[derive(Clone)]
pub struct WebInfo {
    pub addr: SocketAddr,
    pub token: String,
    pub login_url: String,
    pub allow_remote: bool,
}

impl std::fmt::Debug for WebInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebInfo")
            .field("addr", &self.addr)
            .field("token", &"<hidden>")
            .finish()
    }
}

enum EngineMsg {
    Call {
        cmd: Command,
        reply: Sender<Result<Reply>>,
    },
    Live {
        reply: Sender<Vec<LiveSession>>,
    },
}

struct NullPublisher;
impl EventPublisher for NullPublisher {
    fn publish(&mut self, _events: &[StoredEvent]) {}
}

/// A running daemon (for the foreground command and for tests).
pub struct DaemonHandle {
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    engine: Option<std::thread::JoinHandle<()>>,
    server: Option<std::thread::JoinHandle<()>>,
    web_thread: Option<std::thread::JoinHandle<()>>,
    web: Option<WebInfo>,
    _lock: DaemonLock,
}

impl DaemonHandle {
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The dashboard's address and token, when it is on.
    pub fn web(&self) -> Option<&WebInfo> {
        self.web.as_ref()
    }

    /// Asks the daemon to stop (same effect as the `shutdown` command, but immediate for tests).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Blocks until the daemon has stopped.
    pub fn wait(mut self) {
        if let Some(e) = self.engine.take() {
            let _ = e.join();
        }
        self.stop.store(true, Ordering::SeqCst);
        if let Some(s) = self.server.take() {
            let _ = s.join();
        }
        if let Some(w) = self.web_thread.take() {
            let _ = w.join();
        }
        crate::server::remove_socket(&self.socket);
        crate::statedir::remove_token_file(&crate::statedir::token_path(&self.socket));
    }
}

fn process_env() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect()
}

/// Reads configuration from the explicit path, else from `<state>/fleet.toml` when it exists,
/// else uses defaults. The repository is never consulted: configuration is code, and a
/// repository's contents are not trusted to name commands.
fn load_config(
    opts: &DaemonOptions,
    layout: &StateLayout,
    env: Vec<(String, String)>,
) -> Result<crate::config_file::LoadedConfig> {
    match &opts.config_path {
        Some(p) => crate::config_file::load_config(p, env),
        None => {
            if crate::fsx::lstat_kind(&layout.default_config).is_some() {
                crate::config_file::load_config(&layout.default_config, env)
            } else {
                Ok(crate::config_file::default_config(env))
            }
        }
    }
}

/// Starts the daemon and returns once the socket is accepting connections.
pub fn start_daemon(opts: DaemonOptions) -> Result<DaemonHandle> {
    let layout = StateLayout::new(opts.state_dir.clone());
    layout.prepare()?;
    let lock = DaemonLock::acquire(&layout.lock)?;
    let env = process_env();
    let loaded = load_config(&opts, &layout, env.clone())?;
    let app: AppConfig = loaded.app;
    let path_env = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.clone());
    let undo_env: Vec<(String, String)> = env
        .iter()
        .filter(|(k, _)| matches!(k.as_str(), "HOME" | "LANG" | "TMPDIR" | "USER"))
        .cloned()
        .collect();

    // One redactor for everything that is stored or shown: terminal output, gate and command
    // output, and what workers write into their result and question files.
    let redactor = Arc::new(loaded.redactor);
    let registry = SessionRegistry::default();

    // The handler is shared by the control socket and the dashboard, so both apply the same
    // admission rule and the same command validation. The dashboard is bound before anything
    // irreversible happens, so a port that is taken or a refused address stops the start cleanly.
    let (tx, rx) = channel::<EngineMsg>();
    let handler = Arc::new(DaemonHandler {
        tx,
        registry: registry.clone(),
        layout: layout.clone(),
        started: Instant::now(),
    });
    let web = match &opts.web {
        Some(web_opts) => Some(WebServer::bind(
            web_opts,
            Arc::new(DaemonWeb {
                handler: Arc::clone(&handler),
                layout: layout.clone(),
                config: opts.config_path.clone(),
                env: env.clone(),
            }),
        )?),
        None => None,
    };

    let ports = Ports {
        clock: Box::new(SystemClock),
        store: Box::new(FileEventStore::open(
            &layout.events,
            Box::new(Sha256Digest),
        )?),
        vcs: Box::new(GitVcs::new(env.clone(), app.env.clone())),
        sessions: Box::new(PtySessionHost::new(
            loaded.ring_bytes,
            Arc::clone(&redactor),
            registry.clone(),
        )),
        contract: Box::new(FsContractFiles::new(Arc::clone(&redactor))),
        jobs: Box::new(ThreadJobRunner::new(
            MAX_CONCURRENT_JOBS,
            Arc::clone(&redactor),
        )),
        logs: Box::new(FileOutputLogs::new(layout.logs_dir.clone())),
        undo: Box::new(AgentUndo::detect_with_env(path_env.as_deref(), undo_env)),
        digest: Box::new(Sha256Digest),
        publisher: Box::new(NullPublisher),
    };
    let kill_grace_ms = app.kill_grace_ms;
    let mut orch = Orchestrator::new(app, ports)?;
    orch.startup(u64::from(std::process::id()), VERSION)?;

    let stop = Arc::new(AtomicBool::new(false));
    let server = Server::bind(&layout.socket, handler)?;
    let socket = layout.socket.clone();

    let engine_stop = Arc::clone(&stop);
    let engine = std::thread::Builder::new()
        .name("fleet-engine".to_string())
        .spawn(move || engine_loop(orch, rx, &engine_stop, kill_grace_ms))
        .map_err(|e| FleetError::internal(format!("could not start the engine thread: {e}")))?;

    let serve_stop = Arc::clone(&stop);
    let server_thread = std::thread::Builder::new()
        .name("fleet-server".to_string())
        .spawn(move || {
            let _ = server.serve(&serve_stop);
        })
        .map_err(|e| FleetError::internal(format!("could not start the server thread: {e}")))?;

    let (web_thread, web_info) = match (web, &opts.web) {
        (Some(web), Some(web_opts)) => {
            let info = WebInfo {
                addr: web.local_addr(),
                token: web.token().to_string(),
                login_url: web.login_url(),
                allow_remote: web_opts.allow_remote,
            };
            let web_stop = Arc::clone(&stop);
            let thread = std::thread::Builder::new()
                .name("fleet-web-server".to_string())
                .spawn(move || web.serve(&web_stop))
                .map_err(|e| {
                    FleetError::internal(format!("could not start the web server thread: {e}"))
                })?;
            (Some(thread), Some(info))
        }
        _ => (None, None),
    };

    Ok(DaemonHandle {
        socket,
        stop,
        engine: Some(engine),
        server: Some(server_thread),
        web_thread,
        web: web_info,
        _lock: lock,
    })
}

/// Runs the daemon in the foreground until a `shutdown` command arrives.
pub fn run_daemon(opts: DaemonOptions) -> Result<()> {
    let handle = start_daemon(opts)?;
    eprintln!(
        "fleet daemon: pid {} listening on {} (stop with `fleet shutdown`)",
        std::process::id(),
        handle.socket().display()
    );
    if let Some(web) = handle.web() {
        // Printed to the operator's terminal only, once. The token is not written to any file,
        // log or event, and workers never see it.
        eprintln!(
            "fleet web: dashboard on http://{}/",
            crate::web::url_host(web.addr)
        );
        if web.allow_remote && !web.addr.ip().to_canonical().is_loopback() {
            eprintln!("fleet web: {}", crate::web::remote_warning(web.addr));
        }
        eprintln!(
            "fleet web: login URL (shown only now, valid until the daemon stops): {}",
            web.login_url
        );
    }
    handle.wait();
    Ok(())
}

fn engine_loop(
    mut orch: Orchestrator,
    rx: Receiver<EngineMsg>,
    stop: &AtomicBool,
    kill_grace_ms: u64,
) {
    let mut shutdown_deadline: Option<Instant> = None;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match rx.recv_timeout(TICK) {
            Ok(EngineMsg::Call { cmd, reply }) => {
                let result = orch.handle(cmd);
                let _ = reply.send(result);
            }
            Ok(EngineMsg::Live { reply }) => {
                let _ = reply.send(orch.live_sessions());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        orch.tick();
        if orch.shutdown_requested() {
            let deadline = *shutdown_deadline.get_or_insert_with(|| {
                Instant::now()
                    + Duration::from_millis(kill_grace_ms.saturating_mul(2))
                    + SHUTDOWN_EXTRA
            });
            if orch.live_count() == 0 || Instant::now() >= deadline {
                break;
            }
        }
    }
    stop.store(true, Ordering::SeqCst);
}

struct DaemonHandler {
    tx: Sender<EngineMsg>,
    registry: SessionRegistry,
    layout: StateLayout,
    started: Instant,
}

impl DaemonHandler {
    fn ask<T>(&self, build: impl FnOnce(Sender<T>) -> EngineMsg) -> Result<T> {
        let (reply_tx, reply_rx) = channel();
        self.tx
            .send(build(reply_tx))
            .map_err(|_| FleetError::internal("the daemon is shutting down"))?;
        reply_rx.recv_timeout(ENGINE_REPLY_TIMEOUT).map_err(|_| {
            FleetError::new(
                fleet_domain::error::ErrorCode::Timeout,
                "the daemon did not answer in time",
            )
        })
    }

    /// Refuses a peer that is, or was started by, a worker, and one that cannot be inspected.
    /// Workers run as the same user as the operator, so the socket's file mode cannot keep them
    /// out; this check covers the direct path (a worker, or a child of a worker, connecting to
    /// the socket). It runs at accept (`admit`, binding the decision to the connection) and
    /// again per request.
    fn check_origin(&self, peer: &PeerInfo) -> Result<()> {
        let live = self.ask(|reply| EngineMsg::Live { reply })?;
        decide_origin(peer, &live, procid::ancestry)
    }

    /// A repository that contains the state directory would let workers read the daemon's
    /// files, including the control socket, from inside their own worktree.
    fn check_repo(&self, cmd: &Command) -> Result<()> {
        let repo = match cmd {
            Command::CreateRun(args) => Some(args.repo.as_str()),
            Command::LoadPlan {
                new_run: Some(args),
                ..
            } => Some(args.repo.as_str()),
            _ => None,
        };
        match repo {
            Some(r) => self.layout.reject_inside(&[PathBuf::from(r)]),
            None => Ok(()),
        }
    }

    fn convert(&self, reply: Reply, size: Option<(u16, u16)>) -> Result<HandlerReply> {
        match reply {
            Reply::Data(v) => Ok(HandlerReply::Data(self.decorate(v))),
            Reply::Input {
                session,
                bytes,
                data,
            } => {
                let io = self.registry.get(session).ok_or_else(|| {
                    FleetError::conflict("the session ended before the input could be delivered")
                })?;
                io.write_input(&bytes).map_err(|e| match e.kind() {
                    std::io::ErrorKind::WouldBlock => FleetError::unavailable(e.to_string()),
                    std::io::ErrorKind::BrokenPipe => FleetError::conflict(
                        "the session ended before the input could be delivered",
                    ),
                    _ => FleetError::internal(format!("could not write to the session: {e}")),
                })?;
                Ok(HandlerReply::Data(data))
            }
            Reply::Attach { session, data } => {
                let io = self.registry.get(session).ok_or_else(|| {
                    FleetError::conflict("the session ended before it could be attached")
                })?;
                if let Some((cols, rows)) = size {
                    // The resize makes the agent redraw its screen for the attaching terminal.
                    let _ = io.resize(cols, rows);
                }
                Ok(HandlerReply::Attach {
                    data,
                    session: Box::new(PtyAttach { io }),
                })
            }
        }
    }

    /// Fills in values only the daemon process knows.
    fn decorate(&self, mut v: Value) -> Value {
        if v.get("schema").and_then(Value::as_str) == Some("fleet.status/1") {
            if let Value::Object(map) = &mut v {
                if let Some(Value::Object(daemon)) = map.get_mut("daemon") {
                    daemon.insert(
                        "rss_kb".to_string(),
                        crate::memory::rss_kb().map_or(Value::Null, Value::int),
                    );
                    daemon.insert(
                        "output_bytes".to_string(),
                        Value::int(self.registry.output_bytes()),
                    );
                    daemon.insert(
                        "uptime_ms".to_string(),
                        Value::int(
                            u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
                        ),
                    );
                }
            }
        }
        v
    }
}

/// Decides whether the process behind a connection may talk to the daemon. It fails closed: a
/// peer whose pid the kernel did not report, or that cannot be found in the process table any
/// more (it exited after connecting, which is how a worker's child could pipeline requests and
/// vanish before they were judged), is refused like a worker or a process one of them started.
/// `ancestry` lists a process and its ancestors, nearest first, and is empty when the process
/// cannot be inspected.
fn decide_origin(
    peer: &PeerInfo,
    live: &[LiveSession],
    ancestry: impl FnOnce(u32) -> Vec<ProcInfo>,
) -> Result<()> {
    let Some(pid) = peer.pid else {
        return Err(FleetError::forbidden(
            "the connecting process could not be identified; refusing the connection",
        ));
    };
    let chain = ancestry(pid);
    if chain.first().map(|p| p.pid) != Some(pid) {
        return Err(FleetError::forbidden(
            "the connecting process has exited or cannot be inspected; refusing the connection",
        ));
    }
    if is_worker_origin(&chain, live) {
        return Err(FleetError::forbidden(
            "requests from worker processes are refused; approvals and decisions are made by a person",
        ));
    }
    Ok(())
}

fn term_size(args: &Value) -> Option<(u16, u16)> {
    let cols = u16::try_from(args.get("cols")?.as_u64()?).ok()?;
    let rows = u16::try_from(args.get("rows")?.as_u64()?).ok()?;
    Some((cols, rows))
}

impl RequestHandler for DaemonHandler {
    fn admit(&self, peer: &PeerInfo) -> Result<()> {
        self.check_origin(peer)
    }

    fn call(&self, peer: &PeerInfo, cmd: &str, args: Value) -> Result<HandlerReply> {
        // Admission already ran when the connection was accepted; this is the cheap re-check
        // that a peer which has since turned out to be (or to be started by) a worker, or has
        // vanished, gets nothing more.
        self.check_origin(peer)?;
        self.execute(cmd, args)
    }
}

impl DaemonHandler {
    /// Validates and runs one command. The caller has already decided that its peer is admitted.
    fn execute(&self, cmd: &str, args: Value) -> Result<HandlerReply> {
        let command = parse_command(cmd, &args)?;
        self.check_repo(&command)?;
        let size = if matches!(command, Command::Attach { .. }) {
            term_size(&args)
        } else {
            None
        };
        let reply = self.ask(|reply| EngineMsg::Call {
            cmd: command,
            reply,
        })??;
        self.convert(reply, size)
    }
}

/// The commands the dashboard may run. Everything else (starting runs, stopping or cancelling
/// tasks, attaching, shutting the daemon down) stays on the control socket.
pub const WEB_COMMANDS: &[&str] = &[
    "status",
    "events",
    "task.tail",
    "task.send",
    "gate.resolve",
    "approval.decide",
    "merge",
];

/// The dashboard's view of the daemon: the same handler as the control socket, restricted to
/// [`WEB_COMMANDS`].
struct DaemonWeb {
    handler: Arc<DaemonHandler>,
    layout: StateLayout,
    config: Option<PathBuf>,
    env: Vec<(String, String)>,
}

impl WebBackend for DaemonWeb {
    fn admit(&self, peer: &PeerInfo) -> Result<()> {
        self.handler.check_origin(peer)
    }

    fn call(&self, cmd: &str, args: Value) -> Result<Value> {
        if !WEB_COMMANDS.contains(&cmd) {
            return Err(FleetError::forbidden(format!(
                "the dashboard does not run {}",
                fleet_domain::ids::show(cmd)
            )));
        }
        match self.handler.execute(cmd, args)? {
            HandlerReply::Data(v) => Ok(v),
            HandlerReply::Attach { .. } => Err(FleetError::forbidden(
                "the dashboard cannot attach to a terminal",
            )),
        }
    }

    fn process_alive(&self, pid: u32) -> bool {
        procid::proc_info(pid).is_some()
    }

    fn log_status(&self) -> Result<crate::eventlog::VerifyReport> {
        crate::eventlog::verify_file(&self.layout.events)
    }

    fn health(&self) -> Value {
        let checks = crate::doctor::run_checks(&crate::doctor::DoctorInput {
            layout: self.layout.clone(),
            config: self.config.clone(),
            env: self.env.clone(),
        });
        crate::doctor::to_value(&checks)
    }
}

/// An attached client's view of a session's terminal.
struct PtyAttach {
    io: Arc<SessionIo>,
}

impl AttachSession for PtyAttach {
    fn output(&mut self) -> Receiver<Vec<u8>> {
        self.io.subscribe()
    }

    fn write_input(&self, bytes: &[u8]) -> std::io::Result<()> {
        self.io.write_input(bytes)
    }

    fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        self.io.resize(cols, rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_domain::error::ErrorCode;

    fn peer(pid: Option<u32>) -> PeerInfo {
        PeerInfo { uid: Some(0), pid }
    }

    fn info(pid: u32, ppid: u32, pgid: u32) -> ProcInfo {
        ProcInfo { pid, ppid, pgid }
    }

    const WORKER: LiveSession = LiveSession {
        pid: 500,
        pgid: 500,
    };

    /// The process table of a pretend machine: `fleet` (900) run by a shell (800), and `fleet`
    /// (901) started by the worker's own shell script (600) below the worker (500).
    fn table(pid: u32) -> Vec<ProcInfo> {
        match pid {
            900 => vec![info(900, 800, 800), info(800, 100, 800)],
            901 => vec![info(901, 600, 500), info(600, 500, 500), info(500, 1, 500)],
            // a process that left its worker's tree and was adopted by init
            950 => vec![info(950, 1, 950)],
            500 => vec![info(500, 1, 500)],
            _ => Vec::new(),
        }
    }

    fn refused(result: Result<()>) -> String {
        let err = result.expect_err("the connection must be refused");
        assert_eq!(err.code, ErrorCode::Forbidden);
        err.message
    }

    #[test]
    fn an_operator_process_is_admitted_whether_or_not_workers_are_running() {
        assert!(decide_origin(&peer(Some(900)), &[WORKER], table).is_ok());
        assert!(decide_origin(&peer(Some(900)), &[], table).is_ok());
    }

    #[test]
    fn a_worker_and_everything_it_started_is_refused() {
        for pid in [901, 500] {
            let message = refused(decide_origin(&peer(Some(pid)), &[WORKER], table));
            assert!(message.contains("worker processes"), "{message}");
        }
    }

    #[test]
    fn a_real_process_that_already_exited_is_refused_and_a_live_one_is_not() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        let message = refused(decide_origin(&peer(Some(pid)), &[], procid::ancestry));
        assert!(message.contains("exited"), "{message}");
        assert!(decide_origin(&peer(Some(std::process::id())), &[], procid::ancestry).is_ok());
    }

    #[test]
    fn a_peer_that_cannot_be_identified_is_refused_even_with_no_worker_running() {
        // no pid from the kernel
        let message = refused(decide_origin(&peer(None), &[], table));
        assert!(message.contains("identified"), "{message}");
        // the process is already gone: the original bypass (pipeline requests, then exit)
        for live in [&[][..], &[WORKER][..]] {
            let message = refused(decide_origin(&peer(Some(777)), live, table));
            assert!(message.contains("exited"), "{message}");
        }
        // a table that answers about some other process is not an answer about the peer
        let wrong = |_pid: u32| vec![info(123, 1, 123)];
        refused(decide_origin(&peer(Some(777)), &[WORKER], wrong));
    }

    #[test]
    fn the_ancestry_source_is_asked_exactly_once_about_the_peer() {
        let asked = std::cell::RefCell::new(Vec::new());
        let result = decide_origin(&peer(Some(900)), &[WORKER], |pid| {
            asked.borrow_mut().push(pid);
            table(pid)
        });
        assert!(result.is_ok());
        assert_eq!(*asked.borrow(), vec![900]);
    }

    #[test]
    fn a_process_that_left_the_workers_tree_is_the_documented_gap() {
        // Detaching completely (setsid and adoption by init) before connecting is not caught by
        // the origin check; the token and the per-worker state directory are what stand in
        // the way. This test pins the behaviour so a change to it is a decision, not an accident.
        assert!(decide_origin(&peer(Some(950)), &[WORKER], table).is_ok());
    }
}
