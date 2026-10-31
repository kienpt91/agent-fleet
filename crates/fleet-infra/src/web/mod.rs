//! The local web dashboard: a small HTTP/1.1 server on `std::net` that serves a hand-written page
//! and a JSON API over the same use cases the command line uses.
//!
//! Threads, not async (see ADR 3): one accept loop and one thread per connection, bounded by a
//! connection cap. Every connection carries exactly one request and is then closed, except
//! server-sent event streams, which have their own cap, lifetime and byte budget.
//!
//! Defences, in the order a request meets them (the threat model is in ADR 10 and SECURITY.md):
//! 1. connection cap, then **admission**: the process at the other end of the TCP connection is
//!    identified and refused when it is a worker or was started by one (the rule of the control
//!    socket), or cannot be identified;
//! 2. bounded, deadline-limited parsing of one request;
//! 3. strict **Host** check (DNS rebinding);
//! 4. for `/api`: `Sec-Fetch-Site`, then **authentication** (bearer token or session cookie,
//!    constant-time, failed attempts rate limited), then for state-changing requests **Origin**,
//!    **CSRF token** and a JSON content type;
//! 5. strictly validated JSON bodies, mapped onto the daemon's own commands.

pub mod assets;
pub mod auth;
pub mod http;
pub mod peer;
mod routes;

use self::auth::{AuthLimits, WebAuth};
use self::http::{finish, read_request, HttpError, Limits, Response};
use self::peer::PeerIdentity;
use crate::eventlog::VerifyReport;
use crate::server::PeerInfo;
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::value::Value;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// What the dashboard needs from the daemon. The daemon implements it with the same handler the
/// control socket uses, so commands, validation and the worker-origin rule are shared.
pub trait WebBackend: Send + Sync {
    /// The control socket's admission rule, applied to a process that holds the client's end of a
    /// connection: an error refuses it (a worker, a descendant of one, or an unreadable process).
    fn admit(&self, peer: &PeerInfo) -> Result<()>;
    /// Runs one daemon command (the same names as on the control socket).
    fn call(&self, cmd: &str, args: Value) -> Result<Value>;
    /// Verifies the event log's hash chain.
    fn log_status(&self) -> Result<VerifyReport>;
    /// The `fleet doctor` checklist as a `fleet.doctor/1` document.
    fn health(&self) -> Value;
}

#[derive(Clone, Copy, Debug)]
pub struct WebLimits {
    pub http: Limits,
    pub auth: AuthLimits,
    /// Open connections, event streams included.
    pub max_connections: usize,
    /// Open event streams.
    pub max_streams: usize,
    /// An event stream ends (and the browser reconnects) after this long.
    pub stream_lifetime: Duration,
    /// An event stream ends after sending this many bytes of output.
    pub stream_bytes: usize,
    /// How often a stream looks for new output.
    pub stream_poll: Duration,
    /// A comment is sent on a quiet stream this often, which also notices a vanished client.
    pub keepalive: Duration,
    /// Bytes of output read from the daemon per poll.
    pub tail_chunk: usize,
    /// Health and log-verification results are reused for this long.
    pub cache_ttl: Duration,
}

impl Default for WebLimits {
    fn default() -> WebLimits {
        WebLimits {
            http: Limits::default(),
            auth: AuthLimits::default(),
            max_connections: 32,
            max_streams: 4,
            stream_lifetime: Duration::from_secs(600),
            stream_bytes: 4 * 1024 * 1024,
            stream_poll: Duration::from_millis(250),
            keepalive: Duration::from_secs(15),
            tail_chunk: 16 * 1024,
            cache_ttl: Duration::from_secs(5),
        }
    }
}

#[derive(Clone, Debug)]
pub struct WebOptions {
    pub addr: SocketAddr,
    /// Allows binding an address that is not loopback. The caller prints a warning.
    pub allow_remote: bool,
    pub limits: WebLimits,
}

impl WebOptions {
    pub fn new(addr: SocketAddr, allow_remote: bool) -> WebOptions {
        WebOptions {
            addr,
            allow_remote,
            limits: WebLimits::default(),
        }
    }
}

/// Parses the value of `--web`: `host:port`, `[v6]:port`, a bare port (loopback), or `host` with
/// an ephemeral port. Only IP literals and `localhost` are accepted, so a name cannot resolve to
/// something else later.
pub fn parse_bind(text: &str) -> Result<SocketAddr> {
    let bad = || {
        FleetError::usage(format!(
            "--web expects <ip>:<port>, <port> or <ip> (for example 127.0.0.1:7878), not {}",
            fleet_domain::ids::show(text)
        ))
    };
    let loopback = IpAddr::from([127, 0, 0, 1]);
    if text.is_empty() {
        return Err(bad());
    }
    if let Ok(port) = text.parse::<u16>() {
        return Ok(SocketAddr::new(loopback, port));
    }
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Some(port) = text.strip_prefix("localhost:") {
        return port
            .parse::<u16>()
            .map(|p| SocketAddr::new(loopback, p))
            .map_err(|_| bad());
    }
    if text == "localhost" {
        return Ok(SocketAddr::new(loopback, 0));
    }
    if let Ok(ip) = text.trim_matches(['[', ']']).parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, 0));
    }
    Err(bad())
}

/// Refuses a non-loopback address unless remote access was asked for explicitly.
pub fn check_bind(addr: SocketAddr, allow_remote: bool) -> Result<()> {
    if addr.ip().to_canonical().is_loopback() || allow_remote {
        return Ok(());
    }
    Err(FleetError::invalid(format!(
        "refusing to serve the web dashboard on {addr}: it is not a loopback address. The dashboard has no TLS; prefer an SSH tunnel to 127.0.0.1. To serve it anyway pass --web-allow-remote"
    )))
}

/// The text printed when remote access is on.
pub fn remote_warning(addr: SocketAddr) -> String {
    format!(
        "WARNING: the web dashboard listens on {addr}, which is not loopback. There is no TLS: the token, the session cookie and every page travel in clear text, and anyone who can reach this port can try to guess the token or read what passes. Prefer `ssh -L` to a loopback address."
    )
}

/// A cache for results that are slow to compute and not secret-sensitive to share.
struct Cached {
    slot: Mutex<Option<(Instant, Value)>>,
}

impl Cached {
    fn new() -> Cached {
        Cached {
            slot: Mutex::new(None),
        }
    }

    /// The cached value when it is younger than `ttl`, else a fresh one. Computing happens with
    /// the lock held, so concurrent requests wait for one computation instead of starting many.
    fn get(&self, ttl: Duration, compute: impl FnOnce() -> Value) -> Value {
        let mut slot = lock(&self.slot);
        if let Some((at, v)) = slot.as_ref() {
            if at.elapsed() < ttl {
                return v.clone();
            }
        }
        let v = compute();
        *slot = Some((Instant::now(), v.clone()));
        v
    }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) struct Shared {
    pub backend: Arc<dyn WebBackend>,
    pub auth: WebAuth,
    pub limits: WebLimits,
    pub bound: SocketAddr,
    pub allow_remote: bool,
    pub stop: AtomicBool,
    active: AtomicUsize,
    pub streams_active: AtomicUsize,
    next_id: AtomicU64,
    open: Mutex<HashMap<u64, TcpStream>>,
    health: Cached,
    log: Cached,
}

impl Shared {
    pub fn cached_health(&self) -> Value {
        self.health
            .get(self.limits.cache_ttl, || self.backend.health())
    }

    pub fn cached_log(&self) -> Value {
        self.log.get(self.limits.cache_ttl, || {
            routes::log_document(self.backend.log_status())
        })
    }
}

struct ConnGuard {
    shared: Arc<Shared>,
    id: u64,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        lock(&self.shared.open).remove(&self.id);
        self.shared.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The bound dashboard. Call [`WebServer::serve`] on a thread of its own.
pub struct WebServer {
    listener: TcpListener,
    shared: Arc<Shared>,
}

/// How often the accept loop looks at the stop flag.
const POLL: Duration = Duration::from_millis(20);
/// How long `serve` waits for connection threads after asking them to stop.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

impl WebServer {
    /// Binds the listener (port 0 picks a free port) and generates the web token.
    pub fn bind(opts: &WebOptions, backend: Arc<dyn WebBackend>) -> Result<WebServer> {
        check_bind(opts.addr, opts.allow_remote)?;
        let listener = TcpListener::bind(opts.addr).map_err(|e| {
            FleetError::io(&format!("binding the web dashboard to {}", opts.addr), &e)
        })?;
        listener
            .set_nonblocking(true)
            .map_err(|e| FleetError::io("configuring the web listener", &e))?;
        let bound = listener
            .local_addr()
            .map_err(|e| FleetError::io("reading the web listener address", &e))?;
        let shared = Arc::new(Shared {
            backend,
            auth: WebAuth::new(opts.limits.auth)?,
            limits: opts.limits,
            bound,
            allow_remote: opts.allow_remote,
            stop: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            streams_active: AtomicUsize::new(0),
            next_id: AtomicU64::new(1),
            open: Mutex::new(HashMap::new()),
            health: Cached::new(),
            log: Cached::new(),
        });
        Ok(WebServer { listener, shared })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.shared.bound
    }

    /// The web token. It is held in memory only; the daemon prints it once at start.
    pub fn token(&self) -> &str {
        self.shared.auth.token()
    }

    /// The URL an operator opens once: the token travels in the fragment, which browsers never
    /// send to a server, and the page exchanges it for a cookie.
    pub fn login_url(&self) -> String {
        format!(
            "http://{}/#token={}",
            url_host(self.shared.bound),
            self.token()
        )
    }

    /// Open connections right now.
    pub fn active_connections(&self) -> usize {
        self.shared.active.load(Ordering::Acquire)
    }

    /// Accepts connections until `stop` is set, then closes every open connection.
    pub fn serve(&self, stop: &AtomicBool) {
        loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            match self.listener.accept() {
                Ok((stream, _)) => self.dispatch(stream),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => thread::sleep(POLL),
            }
        }
        self.shared.stop.store(true, Ordering::Release);
        for stream in lock(&self.shared.open).values() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        while self.shared.active.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn dispatch(&self, stream: TcpStream) {
        let prior = self.shared.active.fetch_add(1, Ordering::AcqRel);
        let guard = ConnGuard {
            shared: Arc::clone(&self.shared),
            id: self.shared.next_id.fetch_add(1, Ordering::Relaxed),
        };
        if prior >= self.shared.limits.max_connections {
            refuse_quickly(stream, 503, ErrorCode::Limit, "too many connections");
            return;
        }
        if stream.set_nonblocking(false).is_err() {
            return;
        }
        // One handle stays in the open-connection table (shutdown closes through it), one is kept
        // here to answer with when the connection thread cannot be started.
        let (Ok(table_handle), Ok(fallback)) = (stream.try_clone(), stream.try_clone()) else {
            refuse_quickly(
                stream,
                503,
                ErrorCode::Unavailable,
                "the connection could not be set up",
            );
            return;
        };
        lock(&self.shared.open).insert(guard.id, table_handle);
        let spawned = thread::Builder::new()
            .name("fleet-web".to_string())
            .spawn(move || {
                let mut stream = stream;
                let outcome =
                    catch_unwind(AssertUnwindSafe(|| serve_connection(&mut stream, &guard)));
                if outcome.is_err() {
                    // A bug must not look like a silent hang-up.
                    let r = routes::error_response(
                        500,
                        ErrorCode::Internal,
                        "the dashboard failed while handling the request",
                    );
                    let _ = r.write_to(&mut stream);
                    finish(&mut stream);
                }
            });
        if spawned.is_err() {
            refuse_quickly(
                fallback,
                503,
                ErrorCode::Unavailable,
                "the server has no thread for this connection",
            );
        }
    }
}

/// `127.0.0.1:7878` or `[::1]:7878`, as it appears in a URL.
pub fn url_host(addr: SocketAddr) -> String {
    match addr {
        SocketAddr::V4(a) => a.to_string(),
        SocketAddr::V6(a) => format!("[{}]:{}", a.ip(), a.port()),
    }
}

fn refuse_quickly(mut stream: TcpStream, status: u16, code: ErrorCode, message: &str) {
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    let _ = routes::error_response(status, code, message).write_to(&mut stream);
    let _ = stream.shutdown(Shutdown::Both);
}

/// Decides whether the process behind a connection may use the dashboard.
fn admit(shared: &Shared, peer: SocketAddr, local: SocketAddr) -> Result<()> {
    match peer::identify(peer, local) {
        PeerIdentity::NotLocal => Ok(()),
        PeerIdentity::Unknown => Err(FleetError::forbidden(
            "the connecting process could not be identified; refusing the connection",
        )),
        PeerIdentity::Local(pids) => judge_holders(
            &pids,
            |pid| {
                shared.backend.admit(&PeerInfo {
                    uid: None,
                    pid: Some(pid),
                })
            },
            |pid| crate::procid::proc_info(pid).is_some(),
        ),
    }
}

/// Every process that holds the client's end of a connection must pass `judge`. A holder that has
/// vanished by the time it is judged (a child that was forked and exec'd while the socket was
/// open, or a client that exited) is skipped, but only when another holder is there and passes:
/// nothing left to judge is a refusal, as for an unknown peer. A holder that is alive and refused
/// (a worker) refuses the connection whatever the others are.
fn judge_holders(
    pids: &[u32],
    mut judge: impl FnMut(u32) -> Result<()>,
    alive: impl Fn(u32) -> bool,
) -> Result<()> {
    let mut admitted = 0usize;
    for pid in pids {
        match judge(*pid) {
            Ok(()) => admitted += 1,
            Err(e) if alive(*pid) => return Err(e),
            Err(_) => {}
        }
    }
    if admitted == 0 {
        return Err(FleetError::forbidden(
            "the connecting process has exited or cannot be inspected; refusing the connection",
        ));
    }
    Ok(())
}

fn serve_connection(stream: &mut TcpStream, guard: &ConnGuard) {
    let shared = &guard.shared;
    let accepted = Instant::now();
    let _ = stream.set_write_timeout(Some(shared.limits.http.write_timeout));
    let _ = stream.set_nodelay(true);
    let (Ok(peer), Ok(local)) = (stream.peer_addr(), stream.local_addr()) else {
        return;
    };
    if let Err(e) = admit(shared, peer, local) {
        let r = routes::error_response(403, e.code, &e.message);
        let _ = r.write_to(stream);
        finish(stream);
        return;
    }
    let deadline = accepted + shared.limits.http.request_deadline;
    let request = match read_request(stream, &shared.limits.http, deadline) {
        Ok(r) => r,
        Err(e) => {
            let _ = http_error_response(&e).write_to(stream);
            finish(stream);
            return;
        }
    };
    match routes::handle(shared, &request, Instant::now()) {
        routes::Outcome::Reply(r) => {
            let _ = r.write_to(stream);
            finish(stream);
        }
        routes::Outcome::Stream(plan) => {
            routes::stream_task(shared, stream, &plan);
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

pub(crate) fn http_error_response(e: &HttpError) -> Response {
    let code = match e.status {
        405 => ErrorCode::Usage,
        408 => ErrorCode::Timeout,
        413 | 414 | 431 => ErrorCode::Limit,
        _ => ErrorCode::Invalid,
    };
    let mut r = routes::error_response(e.status, code, e.message);
    if e.status == 405 {
        r = r.with_header("Allow", "GET, POST");
    }
    r
}

/// The host part of a `Host` header that the dashboard answers to. See [`routes`] for the policy.
pub fn host_allowed(host: &str, bound: SocketAddr, allow_remote: bool) -> bool {
    let (name, port) = if let Some(rest) = host.strip_prefix('[') {
        let Some((ip, tail)) = rest.split_once(']') else {
            return false;
        };
        (format!("[{ip}]"), tail.strip_prefix(':'))
    } else {
        match host.split_once(':') {
            Some((n, p)) => (n.to_string(), Some(p)),
            None => (host.to_string(), None),
        }
    };
    let port_ok = match port {
        Some(p) => {
            !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_digit())
                && p.parse::<u16>().ok() == Some(bound.port())
        }
        None => bound.port() == 80,
    };
    if !port_ok || name.is_empty() {
        return false;
    }
    let bound_ip = bound.ip().to_canonical();
    if name.eq_ignore_ascii_case("localhost") {
        return bound_ip.is_loopback() || (allow_remote && bound_ip.is_unspecified());
    }
    let literal = name.trim_matches(['[', ']']);
    let Ok(ip) = literal.parse::<IpAddr>() else {
        return false;
    };
    // A bracketed name must be IPv6 and an unbracketed one must not contain a colon.
    if name.starts_with('[') != ip.is_ipv6() {
        return false;
    }
    let ip = ip.to_canonical();
    if bound_ip.is_unspecified() {
        return allow_remote;
    }
    ip == bound_ip
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn bind_text_forms() {
        assert_eq!(parse_bind("7878").unwrap(), sa("127.0.0.1:7878"));
        assert_eq!(parse_bind("0").unwrap(), sa("127.0.0.1:0"));
        assert_eq!(parse_bind("127.0.0.1:0").unwrap(), sa("127.0.0.1:0"));
        assert_eq!(parse_bind("127.0.0.1").unwrap(), sa("127.0.0.1:0"));
        assert_eq!(parse_bind("localhost").unwrap(), sa("127.0.0.1:0"));
        assert_eq!(parse_bind("localhost:9000").unwrap(), sa("127.0.0.1:9000"));
        assert_eq!(parse_bind("[::1]:80").unwrap(), sa("[::1]:80"));
        assert_eq!(parse_bind("::1").unwrap(), sa("[::1]:0"));
        assert_eq!(parse_bind("0.0.0.0:9").unwrap(), sa("0.0.0.0:9"));
        for bad in [
            "",
            "example.com",
            "example.com:80",
            "127.0.0.1:99999",
            "1.2.3",
            "-1",
            "localhost:x",
            "http://127.0.0.1",
        ] {
            assert!(parse_bind(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_loopback_is_bound_without_the_remote_flag() {
        for ok in [
            "127.0.0.1:0",
            "[::1]:0",
            "127.5.5.5:1",
            "[::ffff:127.0.0.1]:1",
        ] {
            assert!(check_bind(sa(ok), false).is_ok(), "{ok}");
        }
        for bad in [
            "0.0.0.0:0",
            "[::]:0",
            "192.168.1.5:80",
            "10.0.0.1:1",
            "8.8.8.8:1",
        ] {
            let e = check_bind(sa(bad), false).unwrap_err();
            assert!(e.message.contains("--web-allow-remote"), "{}", e.message);
            assert!(check_bind(sa(bad), true).is_ok(), "{bad}");
        }
        assert!(remote_warning(sa("0.0.0.0:80")).contains("no TLS"));
    }

    #[test]
    fn host_header_table() {
        let b = sa("127.0.0.1:7878");
        for ok in ["127.0.0.1:7878", "localhost:7878", "LOCALHOST:7878"] {
            assert!(host_allowed(ok, b, false), "{ok}");
        }
        for bad in [
            "",
            "127.0.0.1",
            "127.0.0.1:7879",
            "127.0.0.1:",
            "127.0.0.1:07878x",
            "evil.example:7878",
            "localhost.evil.example:7878",
            "127.0.0.1.evil.example:7878",
            "7878",
            ":7878",
            "[::1]:7878",
            "0.0.0.0:7878",
            "127.0.0.1:7878 ",
            "user@127.0.0.1:7878",
            "127.0.0.1:7878@evil",
            "[127.0.0.1]:7878",
            "localhost@evil:7878",
            "1.2.3.4:7878",
        ] {
            assert!(!host_allowed(bad, b, false), "{bad:?}");
        }
        // port 80 may omit the port
        assert!(host_allowed("127.0.0.1", sa("127.0.0.1:80"), false));
        assert!(!host_allowed("127.0.0.1", b, false));
        // IPv6 loopback
        let b6 = sa("[::1]:7878");
        assert!(host_allowed("[::1]:7878", b6, false));
        assert!(host_allowed("localhost:7878", b6, false));
        assert!(!host_allowed("::1:7878", b6, false));
        assert!(!host_allowed("127.0.0.1:7878", b6, false));
    }

    #[test]
    fn remote_binds_accept_only_ip_literals() {
        let wild = sa("0.0.0.0:7878");
        assert!(host_allowed("192.168.1.9:7878", wild, true));
        assert!(host_allowed("localhost:7878", wild, true));
        assert!(
            !host_allowed("my-host.local:7878", wild, true),
            "names allow DNS rebinding"
        );
        assert!(!host_allowed("192.168.1.9:1", wild, true));
        let lan = sa("192.168.1.9:7878");
        assert!(host_allowed("192.168.1.9:7878", lan, true));
        assert!(!host_allowed("192.168.1.10:7878", lan, true));
        assert!(!host_allowed("localhost:7878", lan, true));
    }

    #[test]
    fn a_vanished_holder_is_skipped_only_when_another_holder_passes() {
        let ok = |_pid: u32| -> Result<()> { Ok(()) };
        let refuse = |pid: u32| -> Result<()> {
            if pid == 1 {
                Err(FleetError::forbidden("worker"))
            } else {
                Ok(())
            }
        };
        let alive = |_pid: u32| true;
        let gone = |pid: u32| pid != 2;
        // everyone passes
        assert!(judge_holders(&[5, 6], ok, alive).is_ok());
        // a living holder that is refused refuses the connection, whatever the others are
        assert_eq!(
            judge_holders(&[5, 1], refuse, alive).unwrap_err().message,
            "worker"
        );
        assert_eq!(
            judge_holders(&[1, 5], refuse, alive).unwrap_err().message,
            "worker"
        );
        // a holder that is refused and gone is skipped when another one passes ...
        let judge = |pid: u32| -> Result<()> {
            if pid == 2 {
                Err(FleetError::forbidden("exited"))
            } else {
                Ok(())
            }
        };
        assert!(judge_holders(&[2, 5], judge, gone).is_ok());
        // ... but not when nobody is left
        let e = judge_holders(&[2], judge, gone).unwrap_err();
        assert!(e.message.contains("exited"), "{}", e.message);
        assert!(judge_holders(&[], ok, alive).is_err());
    }

    #[test]
    fn login_url_has_the_token_in_the_fragment() {
        struct Nop;
        impl WebBackend for Nop {
            fn admit(&self, _: &PeerInfo) -> Result<()> {
                Ok(())
            }
            fn call(&self, _: &str, _: Value) -> Result<Value> {
                Ok(Value::Null)
            }
            fn log_status(&self) -> Result<VerifyReport> {
                Err(FleetError::not_found("none"))
            }
            fn health(&self) -> Value {
                Value::Null
            }
        }
        let s = WebServer::bind(&WebOptions::new(sa("127.0.0.1:0"), false), Arc::new(Nop)).unwrap();
        let url = s.login_url();
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        assert!(url.contains("/#token="));
        assert!(url.ends_with(s.token()));
        assert!(WebServer::bind(&WebOptions::new(sa("0.0.0.0:0"), false), Arc::new(Nop)).is_err());
    }
}
