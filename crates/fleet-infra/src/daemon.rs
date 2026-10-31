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
use fleet_app::api::{parse_command, Command, Reply};
use fleet_app::config::AppConfig;
use fleet_app::orchestrator::Orchestrator;
use fleet_app::ports::{EventPublisher, Ports, StoredEvent};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::limits::MAX_CONCURRENT_JOBS;
use fleet_domain::supervision::{is_worker_origin, LiveSession};
use fleet_domain::value::Value;
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
    _lock: DaemonLock,
}

impl DaemonHandle {
    pub fn socket(&self) -> &Path {
        &self.socket
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
        crate::server::remove_socket(&self.socket);
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

    let registry = SessionRegistry::default();
    let ports = Ports {
        clock: Box::new(SystemClock),
        store: Box::new(FileEventStore::open(
            &layout.events,
            Box::new(Sha256Digest),
        )?),
        vcs: Box::new(GitVcs::new(env.clone(), app.env.clone())),
        sessions: Box::new(PtySessionHost::new(
            loaded.ring_bytes,
            Arc::new(loaded.redactor),
            registry.clone(),
        )),
        contract: Box::new(FsContractFiles),
        jobs: Box::new(ThreadJobRunner::new(MAX_CONCURRENT_JOBS)),
        logs: Box::new(FileOutputLogs::new(layout.logs_dir.clone())),
        undo: Box::new(AgentUndo::detect_with_env(path_env.as_deref(), undo_env)),
        digest: Box::new(Sha256Digest),
        publisher: Box::new(NullPublisher),
    };
    let kill_grace_ms = app.kill_grace_ms;
    let mut orch = Orchestrator::new(app, ports)?;
    orch.startup(u64::from(std::process::id()), VERSION)?;

    let (tx, rx) = channel::<EngineMsg>();
    let stop = Arc::new(AtomicBool::new(false));
    let handler = Arc::new(DaemonHandler {
        tx,
        registry,
        layout: layout.clone(),
        started: Instant::now(),
    });
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

    Ok(DaemonHandle {
        socket,
        stop,
        engine: Some(engine),
        server: Some(server_thread),
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

    /// Refuses requests from a worker or from anything it started. Workers run as the same user
    /// as the operator, so the socket's file mode cannot keep them out; this check covers the
    /// direct path (a worker, or a child of a worker, connecting to the socket).
    fn reject_worker_origin(&self, peer: &PeerInfo) -> Result<()> {
        let Some(pid) = peer.pid else { return Ok(()) };
        let live = self.ask(|reply| EngineMsg::Live { reply })?;
        if live.is_empty() {
            return Ok(());
        }
        if is_worker_origin(&procid::ancestry(pid), &live) {
            return Err(FleetError::forbidden(
                "requests from worker processes are refused; approvals and decisions are made by a person",
            ));
        }
        Ok(())
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
                io.write_input(&bytes).map_err(|e| {
                    FleetError::internal(format!("could not write to the session: {e}"))
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

fn term_size(args: &Value) -> Option<(u16, u16)> {
    let cols = u16::try_from(args.get("cols")?.as_u64()?).ok()?;
    let rows = u16::try_from(args.get("rows")?.as_u64()?).ok()?;
    Some((cols, rows))
}

impl RequestHandler for DaemonHandler {
    fn call(&self, peer: &PeerInfo, cmd: &str, args: Value) -> Result<HandlerReply> {
        self.reject_worker_origin(peer)?;
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
