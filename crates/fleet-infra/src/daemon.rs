//! The daemon: wires the adapters into the orchestrator and serves the control socket.
//!
//! Threads: one engine thread owns the orchestrator and handles every command and tick in order
//! (so the application layer needs no locking); the socket server runs connection threads that
//! forward requests to the engine over a channel and wait for the answer. Writes into worker
//! terminals (`send`, `attach`) happen on connection threads, so a worker that does not read its
//! input can never block the engine.

use crate::clock::SystemClock;
use crate::context_store::FileContextStore;
use crate::contract_fs::FsContractFiles;
use crate::digest::Sha256Digest;
use crate::eventlog::FileEventStore;
use crate::git::GitVcs;
use crate::jobs::{JobTracker, ThreadJobRunner};
use crate::knowledge::FileKnowledge;
use crate::logs::FileOutputLogs;
use crate::msgfs::{FsInbox, FsOutbox, FsRedactor};
use crate::procid;
use crate::pty::{PtySessionHost, SessionIo, SessionRegistry};
use crate::server::{AttachSession, HandlerReply, PeerInfo, RequestHandler, Server};
use crate::statedir::{DaemonLock, StateLayout};
use crate::undo::AgentUndo;
use crate::verify::VerifyHost;
use crate::web::{WebBackend, WebImage, WebOptions, WebServer};
use fleet_app::api::{parse_command, Command, Reply};
use fleet_app::browse::{RepoBrowser, RepoImage, RepoReply};
use fleet_app::config::AppConfig;
use fleet_app::orchestrator::Orchestrator;
use fleet_app::ports::{EventPublisher, Ports, StoredEvent};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::limits::MAX_CONCURRENT_JOBS;
use fleet_domain::supervision::{is_started_by_daemon, is_worker_origin, Lineage, LiveSession};
use fleet_domain::value::Value;
use std::io::IsTerminal;
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
/// A git call that reads a repository for the dashboard gives up after this long.
const REPO_READ_TIMEOUT: Duration = Duration::from_secs(20);

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
    pub print_login_url: bool,
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
    // The children of gates, approved commands, verification checks, the undo tool and git are
    // all listed here; the origin checks of the control socket and of the dashboard refuse them.
    let tracker = JobTracker::new();

    // The handler is shared by the control socket and the dashboard, so both apply the same
    // admission rule and the same command validation. The dashboard is bound before anything
    // irreversible happens, so a port that is taken or a refused address stops the start cleanly.
    let (tx, rx) = channel::<EngineMsg>();
    let handler = Arc::new(DaemonHandler {
        tx,
        registry: registry.clone(),
        layout: layout.clone(),
        started: Instant::now(),
        // reads the real repository with the same hardened git as everything else, and its
        // children are listed with the tracker, so the origin checks refuse them too
        browser: RepoBrowser::new(Arc::new(
            GitVcs::new(env.clone(), app.env.clone())
                .with_tracker(tracker.clone())
                .with_timeout(REPO_READ_TIMEOUT),
        ))
        .with_image_limits(app.images),
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

    // The forge integration (off by default): `gh` is resolved once, here. Its children are listed
    // in the same tracker as every other child the daemon starts.
    let forge_jobs = app.forge.enabled.then(|| {
        crate::forge_jobs::build(&app.forge, &env, Arc::clone(&redactor), tracker.clone())
    });
    let ports = Ports {
        clock: Box::new(SystemClock),
        store: Box::new(FileEventStore::open(
            &layout.events,
            Box::new(Sha256Digest),
        )?),
        vcs: Box::new(GitVcs::new(env.clone(), app.env.clone()).with_tracker(tracker.clone())),
        sessions: Box::new(PtySessionHost::new(
            loaded.ring_bytes,
            Arc::clone(&redactor),
            registry.clone(),
        )),
        contract: Box::new(FsContractFiles::new(Arc::clone(&redactor))),
        outbox: Box::new(FsOutbox::new()),
        inbox: Box::new(FsInbox::new()),
        redact: Box::new(FsRedactor::new(Arc::clone(&redactor))),
        jobs: Box::new(
            ThreadJobRunner::new(MAX_CONCURRENT_JOBS, Arc::clone(&redactor))
                .with_tracker(tracker.clone())
                .with_verification(VerifyHost::new(
                    GitVcs::new(env.clone(), app.env.clone()).with_tracker(tracker.clone()),
                    layout.verify_dir.clone(),
                )?),
        ),
        logs: Box::new(FileOutputLogs::new(layout.logs_dir.clone())),
        undo: Box::new(
            AgentUndo::detect_with_env(path_env.as_deref(), undo_env).with_tracker(tracker),
        ),
        digest: Box::new(Sha256Digest),
        publisher: Box::new(NullPublisher),
        context: Box::new(FileContextStore::new(layout.context_dir.clone())),
        knowledge: Box::new(FileKnowledge::new(
            app.context.knowledge.clone().map(PathBuf::from),
        )),
    };
    let kill_grace_ms = app.kill_grace_ms;
    // Context packs are redacted with the same rules as everything else the daemon stores.
    let mut app = app;
    app.redactor = Arc::clone(&redactor);
    let mut orch = Orchestrator::new(app, ports)?;
    if let Some(jobs) = forge_jobs {
        orch.attach_forge(jobs);
    }
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
                print_login_url: web_opts.print_login_url,
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

/// What the daemon says about the dashboard when it starts. The login URL carries the web token,
/// so it is shown only on a terminal (or when the operator asked for it): stderr that is a file,
/// a pipe or a journal would keep it where every process of the same user can read it.
fn web_banner(web: &WebInfo, stderr_is_terminal: bool) -> Vec<String> {
    let mut lines = vec![format!(
        "fleet web: dashboard on http://{}/",
        crate::web::url_host(web.addr)
    )];
    if web.allow_remote && !web.addr.ip().to_canonical().is_loopback() {
        lines.push(format!(
            "fleet web: {}",
            crate::web::remote_warning(web.addr)
        ));
    }
    if stderr_is_terminal || web.print_login_url {
        lines.push(format!(
            "fleet web: login URL (shown only now, valid until the daemon stops): {}",
            web.login_url
        ));
    } else {
        lines.push(
            "fleet web: the login URL was not printed because stderr is not a terminal (it would be kept in whatever receives it); run the daemon in a terminal or add --web-print-login-url"
                .to_string(),
        );
    }
    lines
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
        for line in web_banner(web, std::io::stderr().is_terminal()) {
            eprintln!("{line}");
        }
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
    /// Answers the questions about a task's repository (read from the real repository, on the
    /// calling thread).
    browser: RepoBrowser,
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
        // The engine's answer is every live worker session plus every child a job, the undo tool
        // or git has running (the shared `JobTracker`), also the descendants that left their
        // group; see `Orchestrator::live_sessions`.
        let live = self.ask(|reply| EngineMsg::Live { reply })?;
        decide_origin(peer, &live, std::process::id(), procid::lineage)
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
            Reply::Repo(req) => match self.browser.run(&req.target, &req.query)? {
                RepoReply::Doc(v) => Ok(HandlerReply::Data(v)),
                RepoReply::Image(_) => Err(FleetError::invalid(
                    "images are served by the dashboard only; use repo.file for a description",
                )),
            },
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
    daemon_pid: u32,
    lineage: impl FnOnce(u32) -> Lineage,
) -> Result<()> {
    let Some(pid) = peer.pid else {
        return Err(FleetError::forbidden(
            "the connecting process could not be identified; refusing the connection",
        ));
    };
    let Lineage { chain, complete } = lineage(pid);
    if chain.first().map(|p| p.pid) != Some(pid) {
        return Err(FleetError::forbidden(
            "the connecting process has exited or cannot be inspected; refusing the connection",
        ));
    }
    if is_worker_origin(&chain, live) || is_started_by_daemon(&chain, daemon_pid) {
        return Err(FleetError::forbidden(
            "requests from worker processes are refused; approvals and decisions are made by a person",
        ));
    }
    // The walk found no worker and not the daemon, but only a walk that reached the root of the
    // process tree can say the daemon is not further up: a deep chain (a check that nests 70
    // shells and detaches the leaf) or an ancestor that vanished mid-read is refused, as an
    // unreadable peer is.
    if !complete {
        return Err(FleetError::forbidden(
            "the ancestry of the connecting process could not be read to its root (it is very deep or changed while it was read); refusing the connection, try again",
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
    /// Validates one command and has the engine decide on it. The caller has already decided that
    /// its peer is admitted.
    fn dispatch(&self, cmd: &str, args: &Value) -> Result<(Reply, Option<(u16, u16)>)> {
        let command = parse_command(cmd, args)?;
        self.check_repo(&command)?;
        let size = if matches!(command, Command::Attach { .. }) {
            term_size(args)
        } else {
            None
        };
        let reply = self.ask(|reply| EngineMsg::Call {
            cmd: command,
            reply,
        })??;
        Ok((reply, size))
    }

    /// Runs one command. A repository question is answered here, on the caller's thread, not on
    /// the engine's: the engine only said which commits it may be about.
    fn execute(&self, cmd: &str, args: Value) -> Result<HandlerReply> {
        let (reply, size) = self.dispatch(cmd, &args)?;
        self.convert(reply, size)
    }

    /// `repo.blob`: the bytes of an image, for the dashboard. Any other command is refused.
    fn execute_image(&self, args: Value) -> Result<RepoImage> {
        let (reply, _) = self.dispatch("repo.blob", &args)?;
        match reply {
            Reply::Repo(req) => match self.browser.run(&req.target, &req.query)? {
                RepoReply::Image(i) => Ok(i),
                RepoReply::Doc(_) => Err(FleetError::internal(
                    "the image request produced a document",
                )),
            },
            _ => Err(FleetError::internal(
                "the image request was not a repository request",
            )),
        }
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
    // the goal ledger and verification, read only: `goal.add`, `goal.attach`, `goal.status` and
    // `verify` (which starts processes) are not offered
    "goal.list",
    "goal.show",
    "goal.remaining",
    "verify.status",
    // the repository at the commits recorded for a task, read only: file names, contents and
    // diffs are shown as text, and only images (by magic number) are ever sent as bytes
    "repo.tree",
    "repo.file",
    "repo.blob",
    "repo.markdown",
    "repo.diff",
    // what the operator's `github`, `issue` and `pr` commands fetched and linked, from memory:
    // nothing that starts `gh`, reads a ticket, makes a task or a pull request is offered
    "forge.status",
    "forge.snapshot",
    "forge.links",
    // fan-out groups, read only: `group.fanout` (creates tasks) and `group.pick` (merges) stay on
    // the control socket
    "group.list",
    "group.show",
    "group.compare",
    // messages between agents, read only: `msg.send`, `msg.mute` and `msg.unmute` are not offered
    "msg.list",
    "msg.show",
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

    fn image(&self, args: Value) -> Result<WebImage> {
        let image = self.handler.execute_image(args)?;
        Ok(WebImage {
            content_type: image.kind.content_type(),
            bytes: image.bytes,
            width: image.width,
            height: image.height,
        })
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
    use fleet_domain::supervision::ProcInfo;

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
    /// The pid of the pretend daemon in the process table below.
    const DAEMON: u32 = 100;

    /// The process table of a pretend machine: `fleet` (900) run by a shell (800), and `fleet`
    /// (901) started by the worker's own shell script (600) below the worker (500).
    fn table(pid: u32) -> Lineage {
        Lineage {
            chain: chain_of(pid),
            complete: true,
        }
    }

    fn chain_of(pid: u32) -> Vec<ProcInfo> {
        match pid {
            900 => vec![info(900, 800, 800), info(800, 100, 800)],
            901 => vec![info(901, 600, 500), info(600, 500, 500), info(500, 1, 500)],
            // a process that left its worker's tree and was adopted by init
            950 => vec![info(950, 1, 950)],
            // a quality gate (700) started by the daemon (100) and the script it runs (701)
            700 => vec![info(700, DAEMON, 700), info(DAEMON, 80, 80)],
            701 => vec![
                info(701, 700, 700),
                info(700, DAEMON, 700),
                info(DAEMON, 80, 80),
            ],
            // a member of the gate's process group that was adopted by init
            702 => vec![info(702, 1, 700)],
            // a client that is the daemon process itself (tests run the daemon in-process)
            100 => vec![info(DAEMON, 80, 80)],
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
        assert!(decide_origin(&peer(Some(900)), &[WORKER], DAEMON, table).is_ok());
        assert!(decide_origin(&peer(Some(900)), &[], DAEMON, table).is_ok());
    }

    #[test]
    fn the_dashboard_offers_the_goal_ledger_and_verification_read_only() {
        let args = |pairs: &[(&str, &str)]| {
            Value::object(pairs.iter().map(|(k, v)| (k.to_string(), Value::str(*v))))
        };
        for (cmd, a) in [
            ("goal.list", args(&[])),
            ("goal.show", args(&[("goal", "g")])),
            ("goal.remaining", args(&[])),
            ("verify.status", args(&[("target", "r1/a")])),
            ("group.list", args(&[])),
            ("group.show", args(&[("group", "fan1")])),
            ("group.compare", args(&[("group", "fan1")])),
            ("msg.list", args(&[])),
            ("msg.show", args(&[("id", "m1")])),
        ] {
            assert!(WEB_COMMANDS.contains(&cmd), "{cmd}");
            let parsed = parse_command(cmd, &a).unwrap();
            assert!(
                fleet_app::api::is_read_only(&parsed),
                "{cmd} changes nothing"
            );
        }
        // everything that changes the ledger, starts a process, creates or merges a group's work
        // or controls the daemon stays on the socket
        for cmd in [
            "goal.add",
            "goal.attach",
            "goal.status",
            "verify",
            "group.fanout",
            "group.pick",
            "run.create",
            "task.add",
            "plan.load",
            "run.start",
            "task.stop",
            "task.retry",
            "task.cancel",
            "task.attach",
            "cleanup",
            "undo",
            "shutdown",
            // messages: the operator's send and mute stay on the control socket
            "msg.send",
            "msg.mute",
            "msg.unmute",
        ] {
            assert!(!WEB_COMMANDS.contains(&cmd), "{cmd} must not be offered");
        }
        // and everything on the list is either read only or one of the decisions a person makes
        for cmd in WEB_COMMANDS {
            assert!(
                matches!(
                    *cmd,
                    "task.send" | "gate.resolve" | "approval.decide" | "merge"
                ) || cmd.starts_with("goal.")
                    || cmd.starts_with("repo.")
                    || cmd.starts_with("group.")
                    || matches!(
                        *cmd,
                        "status"
                            | "events"
                            | "task.tail"
                            | "verify.status"
                            | "forge.status"
                            | "forge.snapshot"
                            | "forge.links"
                            | "msg.list"
                            | "msg.show"
                    ),
                "{cmd}"
            );
        }
        // the group commands on the list change nothing, whatever they are called
        for cmd in WEB_COMMANDS.iter().filter(|c| c.starts_with("group.")) {
            let a = args(&[("group", "fan1")]);
            let parsed = parse_command(cmd, &a).unwrap();
            assert!(fleet_app::api::is_read_only(&parsed), "{cmd}");
        }
    }

    #[test]
    fn the_dashboard_offers_the_repository_views_read_only_and_nothing_that_writes() {
        let task = Value::object([("task", Value::str("r1/a"))]);
        let repo_commands: Vec<&&str> = WEB_COMMANDS
            .iter()
            .filter(|c| c.starts_with("repo."))
            .collect();
        assert_eq!(
            repo_commands,
            [
                &"repo.tree",
                &"repo.file",
                &"repo.blob",
                &"repo.markdown",
                &"repo.diff"
            ]
        );
        for cmd in repo_commands {
            let parsed = parse_command(cmd, &task).unwrap();
            assert!(
                fleet_app::api::is_read_only(&parsed),
                "{cmd} must change nothing"
            );
        }
        // there is no way to name a command that writes a file or starts a process in the repository
        for cmd in [
            "repo.write",
            "repo.edit",
            "repo.checkout",
            "repo.commit",
            "repo.exec",
            "repo.target",
            "repo.refs",
        ] {
            assert!(!WEB_COMMANDS.contains(&cmd), "{cmd}");
            assert!(parse_command(cmd, &task).is_err(), "{cmd} must not parse");
        }
    }

    #[test]
    fn the_dashboard_offers_the_forge_views_read_only_and_nothing_that_reaches_github() {
        for cmd in ["forge.status", "forge.snapshot", "forge.links"] {
            assert!(WEB_COMMANDS.contains(&cmd), "{cmd}");
            let parsed = parse_command(cmd, &Value::Null).unwrap();
            assert!(
                fleet_app::api::is_read_only(&parsed),
                "{cmd} changes nothing"
            );
        }
        for cmd in [
            "issue.list",
            "issue.show",
            "issue.to-task",
            "pr.list",
            "pr.show",
            "pr.status",
            "pr.create",
            "forge.result",
        ] {
            assert!(!WEB_COMMANDS.contains(&cmd), "{cmd} must not be offered");
        }
    }

    fn web_info(allow_remote: bool, print: bool, addr: &str) -> WebInfo {
        let addr: SocketAddr = addr.parse().unwrap();
        WebInfo {
            addr,
            token: "t".repeat(64),
            login_url: format!("http://{addr}/#token={}", "t".repeat(64)),
            allow_remote,
            print_login_url: print,
        }
    }

    #[test]
    fn the_login_url_is_printed_only_to_a_terminal_or_when_asked() {
        let info = web_info(false, false, "127.0.0.1:7878");
        let on_terminal = web_banner(&info, true).join("\n");
        assert!(on_terminal.contains("#token="), "{on_terminal}");
        assert!(on_terminal.contains("shown only now"));
        let piped = web_banner(&info, false).join("\n");
        assert!(!piped.contains("#token="), "{piped}");
        assert!(!piped.contains(&"t".repeat(64)));
        assert!(piped.contains("--web-print-login-url"), "{piped}");
        assert!(piped.contains("dashboard on http://127.0.0.1:7878/"));
        let asked = web_banner(&web_info(false, true, "127.0.0.1:7878"), false).join("\n");
        assert!(asked.contains("#token="), "{asked}");
    }

    #[test]
    fn the_remote_warning_is_printed_for_non_loopback_addresses_only() {
        assert!(!web_banner(&web_info(true, false, "127.0.0.1:1"), true)
            .join("\n")
            .contains("no TLS"));
        assert!(web_banner(&web_info(true, false, "0.0.0.0:1"), true)
            .join("\n")
            .contains("no TLS"));
    }

    #[test]
    fn a_lineage_that_was_not_read_to_its_root_is_refused() {
        // nothing in the chain says worker or daemon, but the walk stopped before init: a check that
        // nests more processes than the cap and detaches the leaf, or a parent that vanished
        let cut = |pid: u32| Lineage {
            chain: vec![info(pid, 4000, pid), info(4000, 4001, 4000)],
            complete: false,
        };
        let message = refused(decide_origin(&peer(Some(910)), &[], DAEMON, cut));
        assert!(
            message.contains("could not be read to its root"),
            "{message}"
        );
        // the same chain, read to the root, is an ordinary process of the operator's
        let whole = |pid: u32| Lineage {
            chain: vec![info(pid, 4000, pid), info(4000, 1, 4000), info(1, 0, 1)],
            complete: true,
        };
        assert!(decide_origin(&peer(Some(910)), &[], DAEMON, whole).is_ok());
        // a cut walk that already shows a worker or the daemon says so, whichever rule is first
        let cut_but_known = |pid: u32| Lineage {
            chain: vec![info(pid, DAEMON, pid), info(DAEMON, 4001, 80)],
            complete: false,
        };
        assert!(
            refused(decide_origin(&peer(Some(911)), &[], DAEMON, cut_but_known))
                .contains("worker processes")
        );
    }

    #[test]
    fn a_daemon_that_is_init_of_its_namespace_still_refuses_what_it_started() {
        // a container entrypoint: pid 1. A gate it started, and what init adopted, are its own...
        let in_container = |pid: u32| match pid {
            700 => Lineage {
                chain: vec![info(700, 1, 700), info(1, 0, 1)],
                complete: true,
            },
            // ...and a process that came in from outside (docker exec) has a parent outside
            901 => Lineage {
                chain: vec![info(901, 0, 901)],
                complete: true,
            },
            _ => Lineage {
                chain: Vec::new(),
                complete: false,
            },
        };
        let message = refused(decide_origin(&peer(Some(700)), &[], 1, in_container));
        assert!(message.contains("worker processes"), "{message}");
        assert!(decide_origin(&peer(Some(901)), &[], 1, in_container).is_ok());
    }

    #[test]
    fn everything_the_daemon_started_is_refused_even_with_no_worker_running() {
        // a gate, the script a gate runs, and a leftover of a gate's group that init adopted
        let gate_group = [LiveSession {
            pid: 700,
            pgid: 700,
        }];
        for pid in [700, 701] {
            let message = refused(decide_origin(&peer(Some(pid)), &[], DAEMON, table));
            assert!(message.contains("worker processes"), "{pid}: {message}");
        }
        let message = refused(decide_origin(&peer(Some(702)), &gate_group, DAEMON, table));
        assert!(message.contains("worker processes"), "{message}");
        // without the group registry the adopted leftover would pass: the registry is what stops it
        assert!(decide_origin(&peer(Some(702)), &[], DAEMON, table).is_ok());
        // the operator and the daemon process itself are not affected
        assert!(decide_origin(&peer(Some(900)), &gate_group, DAEMON, table).is_ok());
        assert!(decide_origin(&peer(Some(100)), &gate_group, DAEMON, table).is_ok());
    }

    #[test]
    fn a_worker_and_everything_it_started_is_refused() {
        for pid in [901, 500] {
            let message = refused(decide_origin(&peer(Some(pid)), &[WORKER], DAEMON, table));
            assert!(message.contains("worker processes"), "{message}");
        }
    }

    #[test]
    fn a_real_process_that_already_exited_is_refused_and_a_live_one_is_not() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        let message = refused(decide_origin(
            &peer(Some(pid)),
            &[],
            DAEMON,
            procid::lineage,
        ));
        assert!(message.contains("exited"), "{message}");
        assert!(decide_origin(
            &peer(Some(std::process::id())),
            &[],
            DAEMON,
            procid::lineage
        )
        .is_ok());
    }

    #[test]
    fn a_peer_that_cannot_be_identified_is_refused_even_with_no_worker_running() {
        // no pid from the kernel
        let message = refused(decide_origin(&peer(None), &[], DAEMON, table));
        assert!(message.contains("identified"), "{message}");
        // the process is already gone: the original bypass (pipeline requests, then exit)
        for live in [&[][..], &[WORKER][..]] {
            let message = refused(decide_origin(&peer(Some(777)), live, DAEMON, table));
            assert!(message.contains("exited"), "{message}");
        }
        // a table that answers about some other process is not an answer about the peer
        let wrong = |_pid: u32| Lineage {
            chain: vec![info(123, 1, 123)],
            complete: true,
        };
        refused(decide_origin(&peer(Some(777)), &[WORKER], DAEMON, wrong));
    }

    #[test]
    fn the_ancestry_source_is_asked_exactly_once_about_the_peer() {
        let asked = std::cell::RefCell::new(Vec::new());
        let result = decide_origin(&peer(Some(900)), &[WORKER], DAEMON, |pid| {
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
        assert!(decide_origin(&peer(Some(950)), &[WORKER], DAEMON, table).is_ok());
    }
}
