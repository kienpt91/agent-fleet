//! `fleet doctor` and the web dashboard's health panel: a checklist of everything fleet depends on.
//!
//! Only `fail` makes the command exit unhealthy. Things that are merely absent (daemon not
//! running, no config file, no undo tool) are `info`.

use crate::client::Client;
use crate::config_file::load_config;
use crate::eventlog::verify_file;
use crate::git::check_git_version;
use crate::jobs::JobTracker;
use crate::server::verify_socket_permissions;
use crate::statedir::{verify_token_file, StateLayout};
use crate::undo::AgentUndo;
use fleet_app::ports::UndoTool;
use fleet_domain::error::ErrorCode;
use fleet_domain::sanitize::sanitize_text;
use fleet_domain::value::Value;
use std::fmt::Write as _;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Single-line, terminal-safe text.
fn clean(s: &str) -> String {
    sanitize_text(s.as_bytes())
        .chars()
        .map(|c| {
            if matches!(c, '\n' | '\r' | '\t') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Multi-line, terminal-safe text.
fn multiline(s: &str) -> String {
    sanitize_text(s.as_bytes())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Info,
    Fail,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Info => "info",
            Status::Fail => "fail",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
}

impl Check {
    pub fn new(name: impl Into<String>, status: Status, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            status,
            detail: detail.into(),
        }
    }
}

pub struct DoctorInput {
    pub layout: StateLayout,
    /// Where the children of the checks (`git --version`) are listed. The daemon passes its shared tracker, so that
    /// the health panel's probes are refused as origin like every other child it starts; `fleet doctor`, a process
    /// of its own, passes a fresh one.
    pub tracker: JobTracker,
    /// `--config`; when absent the default config file is used if it exists.
    pub config: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

/// Runs every check. Never panics and never changes anything on disk.
pub fn run_checks(input: &DoctorInput) -> Vec<Check> {
    let mut checks = vec![
        check_platform(std::env::consts::OS),
        check_git(&input.env, &input.tracker),
        check_state_dir(&input.layout.root, current_uid()),
        check_socket(&input.layout.socket),
        check_token(&input.layout.token, &input.layout.socket),
        check_daemon(&input.layout.socket),
        check_events(&input.layout.events),
    ];
    checks.extend(check_config(
        input.config.as_deref(),
        &input.layout.default_config,
        &input.env,
    ));
    checks.extend(crate::ssh::doctor::config_checks(
        input.config.as_deref(),
        &input.layout,
        &input.env,
        &input.tracker,
    ));
    checks.push(check_undo(&input.env));
    checks.push(check_peer_credentials());
    checks.push(check_pty());
    checks
}

pub fn is_healthy(checks: &[Check]) -> bool {
    !checks.iter().any(|c| c.status == Status::Fail)
}

pub fn render_text(checks: &[Check]) -> String {
    let mut out = String::new();
    for c in checks {
        let tag = match c.status {
            Status::Ok => "ok  ",
            Status::Info => "info",
            Status::Fail => "FAIL",
        };
        let _ = writeln!(out, "[{tag}] {}: {}", clean(&c.name), clean(&c.detail));
    }
    let failed = checks.iter().filter(|c| c.status == Status::Fail).count();
    if failed == 0 {
        out.push_str("healthy\n");
    } else {
        let _ = writeln!(out, "unhealthy: {failed} check(s) failed");
    }
    out
}

pub fn to_value(checks: &[Check]) -> Value {
    Value::object([
        ("schema", Value::str("fleet.doctor/1")),
        ("ok", Value::Bool(is_healthy(checks))),
        (
            "checks",
            Value::array(checks.iter().map(|c| {
                Value::object([
                    ("name", Value::str(clean(&c.name))),
                    ("status", Value::str(c.status.as_str())),
                    ("detail", Value::str(multiline(&c.detail))),
                ])
            })),
        ),
    ])
}

pub fn check_platform(os: &str) -> Check {
    match os {
        "linux" | "macos" => Check::new("platform", Status::Ok, os),
        other => Check::new(
            "platform",
            Status::Fail,
            format!("{other} is not supported (linux and macos are)"),
        ),
    }
}

fn check_git(env: &[(String, String)], tracker: &JobTracker) -> Check {
    match check_git_version(
        env,
        &fleet_domain::env::EnvPolicy::default_policy(),
        tracker,
    ) {
        Ok(v) => Check::new("git", Status::Ok, v),
        Err(e) => Check::new("git", Status::Fail, e.message),
    }
}

/// The effective user id of this process.
fn current_uid() -> Option<u32> {
    Some(nix::unistd::geteuid().as_raw())
}

pub fn mode_is_private(mode: u32) -> bool {
    mode & 0o777 == 0o700
}

pub fn check_state_dir(root: &Path, uid: Option<u32>) -> Check {
    let name = "state dir";
    let meta = match std::fs::symlink_metadata(root) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Check::new(
                name,
                Status::Info,
                "not created yet (the daemon creates it)",
            );
        }
        Err(e) => return Check::new(name, Status::Fail, format!("{}: {e}", root.display())),
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Check::new(
            name,
            Status::Fail,
            format!("{} is not a plain directory", root.display()),
        );
    }
    let mode = meta.permissions().mode();
    if !mode_is_private(mode) {
        return Check::new(
            name,
            Status::Fail,
            format!(
                "{} has mode {:o}, expected 700 (run: chmod 700 on it)",
                root.display(),
                mode & 0o777
            ),
        );
    }
    if let Some(uid) = uid {
        if meta.uid() != uid {
            return Check::new(
                name,
                Status::Fail,
                format!(
                    "{} is owned by uid {}, not by you",
                    root.display(),
                    meta.uid()
                ),
            );
        }
    }
    Check::new(name, Status::Ok, format!("{} (mode 700)", root.display()))
}

fn check_socket(socket: &Path) -> Check {
    let name = "socket permissions";
    if std::fs::symlink_metadata(socket).is_err() {
        return Check::new(name, Status::Info, "no socket (daemon not running)");
    }
    match verify_socket_permissions(socket) {
        Ok(()) => Check::new(name, Status::Ok, socket.display().to_string()),
        Err(e) => Check::new(name, Status::Fail, e.message),
    }
}

/// The capability token next to the socket: present and private while the daemon runs. The value
/// is never read here, let alone shown.
fn check_token(token: &Path, socket: &Path) -> Check {
    let name = "control token";
    let token_exists = std::fs::symlink_metadata(token).is_ok();
    if std::fs::symlink_metadata(socket).is_err() {
        return if token_exists {
            Check::new(
                name,
                Status::Info,
                "left over from a daemon that is not running",
            )
        } else {
            Check::new(name, Status::Info, "none (daemon not running)")
        };
    }
    match verify_token_file(token) {
        Ok(()) => Check::new(name, Status::Ok, format!("{} (mode 0600)", token.display())),
        Err(e) => Check::new(name, Status::Fail, e.message),
    }
}

fn check_daemon(socket: &Path) -> Check {
    let name = "daemon";
    if std::fs::symlink_metadata(socket).is_err() {
        return Check::new(name, Status::Info, "not running");
    }
    let mut client = match Client::connect(socket) {
        Ok(c) => c,
        Err(e) if e.code == ErrorCode::DaemonDown => {
            return Check::new(name, Status::Info, "not running (stale socket)");
        }
        Err(e) => return Check::new(name, Status::Fail, e.message),
    };
    match client.call("ping", Value::object(Vec::<(String, Value)>::new())) {
        Ok(_) => Check::new(name, Status::Ok, "running and answering"),
        Err(e) => Check::new(name, Status::Fail, format!("not answering: {}", e.message)),
    }
}

fn check_events(path: &Path) -> Check {
    let name = "event log";
    if std::fs::symlink_metadata(path).is_err() {
        return Check::new(name, Status::Info, "no log yet");
    }
    match verify_file(path) {
        Ok(r) => Check::new(
            name,
            Status::Ok,
            format!("{} records, hash chain intact", r.records),
        ),
        Err(e) => Check::new(name, Status::Fail, e.message),
    }
}

fn is_executable_file(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Finds `command` the way a child would: a path with a slash is used as is, a bare name is
/// searched on `path_var`.
pub fn resolve_command(command: &str, path_var: &str) -> Option<PathBuf> {
    if command.contains('/') {
        let p = PathBuf::from(command);
        return is_executable_file(&p).then_some(p);
    }
    path_var
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(command))
        .find(|p| is_executable_file(p))
}

fn check_config(
    explicit: Option<&Path>,
    default_path: &Path,
    env: &[(String, String)],
) -> Vec<Check> {
    let name = "config";
    let (path, was_explicit) = match explicit {
        Some(p) => (p, true),
        None => (default_path, false),
    };
    if std::fs::symlink_metadata(path).is_err() {
        return vec![if was_explicit {
            Check::new(
                name,
                Status::Fail,
                format!("{} does not exist", path.display()),
            )
        } else {
            Check::new(
                name,
                Status::Info,
                "no fleet.toml (built-in defaults, no workers)",
            )
        }];
    }
    let loaded = match load_config(path, env.to_vec()) {
        Ok(l) => l,
        Err(e) => return vec![Check::new(name, Status::Fail, e.message)],
    };
    let app = &loaded.app;
    let mut out = vec![Check::new(
        name,
        Status::Ok,
        format!(
            "{}: {} workers, {} gates",
            path.display(),
            app.workers.len(),
            app.gates.len()
        ),
    )];
    for def in app.workers.values() {
        let child_env = app
            .env
            .build(&app.parent_env, &def.env_pass, &def.env_set, &[]);
        let path_var = child_env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map_or("", |(_, v)| v.as_str());
        let detail = match resolve_command(&def.command, path_var) {
            Some(p) => format!("{} -> {}", def.command, p.display()),
            None => format!("`{}` not found on the allowlisted PATH", def.command),
        };
        out.push(Check::new(
            format!("worker {}", def.name),
            Status::Info,
            detail,
        ));
    }
    out
}

fn check_undo(env: &[(String, String)]) -> Check {
    let path = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.as_str());
    if AgentUndo::detect(path).available() {
        Check::new("agent-undo", Status::Ok, "found on PATH (fleet undo works)")
    } else {
        Check::new(
            "agent-undo",
            Status::Info,
            "unavailable (optional; `fleet undo` reports unavailable)",
        )
    }
}

fn check_peer_credentials() -> Check {
    let detail = if cfg!(target_os = "linux") {
        "SO_PEERCRED supported: the daemon identifies the connecting process"
    } else if cfg!(target_os = "macos") {
        "LOCAL_PEERCRED supported: the connecting user is verified; process lookup uses ps"
    } else {
        "not supported on this platform: only the socket's 0600 mode protects the daemon"
    };
    Check::new("peer credentials", Status::Info, detail)
}

fn check_pty() -> Check {
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/ptmx")
    {
        Ok(_) => Check::new("pty", Status::Info, "/dev/ptmx can be opened"),
        Err(e) => Check::new(
            "pty",
            Status::Info,
            format!("cannot open /dev/ptmx: {e}; worker sessions may fail to start"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn chmod(p: &Path, mode: u32) {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn the_git_probe_of_the_health_checks_is_listed_in_the_shared_tracker() {
        // the daemon's health panel runs the doctor's checks on a web thread: the `git --version` child must
        // be registered like every other child the daemon starts, so that it is refused as origin
        let t = TempDir::new("doc-tracker");
        let bin = t.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        t.script("bin/git", "#!/bin/sh\nsleep 1\necho 'git version 2.50.0'\n");
        let env = vec![("PATH".to_string(), bin.to_str().unwrap().to_string())];
        let tracker = JobTracker::new();
        let seen = tracker.clone();
        let probe = std::thread::spawn(move || check_git(&env, &tracker));
        assert!(
            crate::testutil::wait_until(3_000, || !seen.live().is_empty()),
            "the git child was never listed in the tracker"
        );
        let check = probe.join().unwrap();
        assert_eq!(check.status, Status::Ok, "{}", check.detail);
        assert!(seen.live().is_empty(), "the entry goes with the child");
    }

    #[test]
    fn platform_table() {
        for (os, status) in [
            ("linux", Status::Ok),
            ("macos", Status::Ok),
            ("windows", Status::Fail),
            ("freebsd", Status::Fail),
        ] {
            assert_eq!(check_platform(os).status, status, "{os}");
        }
        assert!(check_platform("windows").detail.contains("windows"));
    }

    #[test]
    fn mode_rule() {
        assert!(mode_is_private(0o40700));
        assert!(!mode_is_private(0o40750));
        assert!(!mode_is_private(0o40705));
        assert!(!mode_is_private(0o40500));
    }

    #[test]
    fn state_dir_missing_is_info() {
        let t = TempDir::new("doc-missing");
        let c = check_state_dir(&t.join("nope"), Some(0));
        assert_eq!(c.status, Status::Info);
        assert!(c.detail.contains("not created yet"));
    }

    #[test]
    fn state_dir_modes() {
        let t = TempDir::new("doc-mode");
        let d = t.join("state");
        std::fs::create_dir(&d).unwrap();
        chmod(&d, 0o700);
        let me = std::fs::metadata(&d).unwrap().uid();
        assert_eq!(check_state_dir(&d, Some(me)).status, Status::Ok);
        assert_eq!(check_state_dir(&d, None).status, Status::Ok);
        chmod(&d, 0o755);
        let c = check_state_dir(&d, Some(me));
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("755"));
        chmod(&d, 0o700);
    }

    #[test]
    fn state_dir_owned_by_someone_else_fails() {
        let t = TempDir::new("doc-owner");
        let d = t.join("state");
        std::fs::create_dir(&d).unwrap();
        chmod(&d, 0o700);
        let me = std::fs::metadata(&d).unwrap().uid();
        let c = check_state_dir(&d, Some(me.wrapping_add(1)));
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("owned by"));
    }

    #[test]
    fn state_dir_that_is_a_symlink_or_file_fails() {
        let t = TempDir::new("doc-link");
        let real = t.join("real");
        std::fs::create_dir(&real).unwrap();
        chmod(&real, 0o700);
        let link = t.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(check_state_dir(&link, None).status, Status::Fail);
        let file = t.write("file", "x");
        assert_eq!(check_state_dir(&file, None).status, Status::Fail);
    }

    #[test]
    fn resolve_command_searches_only_the_given_path() {
        let t = TempDir::new("doc-resolve");
        let bin = t.script("bin/agentx", "#!/bin/sh\n");
        t.write("bin/plain", "data");
        let dir = t.join("bin");
        let path_var = format!("{}::/nonexistent-dir", dir.display());
        assert_eq!(resolve_command("agentx", &path_var), Some(bin.clone()));
        assert_eq!(resolve_command("plain", &path_var), None, "not executable");
        assert_eq!(resolve_command("agentx", "/nonexistent-dir"), None);
        assert_eq!(resolve_command("agentx", ""), None);
        assert_eq!(
            resolve_command(bin.to_str().unwrap(), ""),
            Some(bin),
            "a path with a slash ignores PATH"
        );
        assert_eq!(resolve_command("./missing", &path_var), None);
    }

    #[test]
    fn missing_explicit_config_fails_but_missing_default_is_info() {
        let t = TempDir::new("doc-config");
        let missing = t.join("fleet.toml");
        let explicit = check_config(Some(&missing), &missing, &[]);
        assert_eq!(explicit.len(), 1);
        assert_eq!(explicit[0].status, Status::Fail);
        let default = check_config(None, &missing, &[]);
        assert_eq!(default[0].status, Status::Info);
    }

    #[test]
    fn token_check_follows_the_socket_and_the_file_mode() {
        let t = TempDir::new("doc-token");
        let socket = t.join("fleet.sock");
        let token = t.join("token");
        // nothing there: the daemon is simply not running
        assert_eq!(check_token(&token, &socket).status, Status::Info);
        // a token without a socket is a leftover of a daemon that did not clean up
        let secret = crate::statedir::create_token_file(&token).unwrap();
        let c = check_token(&token, &socket);
        assert_eq!(c.status, Status::Info);
        assert!(c.detail.contains("not running"), "{}", c.detail);
        // with a socket, the token must exist and be private
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let c = check_token(&token, &socket);
        assert_eq!(c.status, Status::Ok, "{}", c.detail);
        assert!(c.detail.contains("0600"), "{}", c.detail);
        chmod(&token, 0o640);
        let c = check_token(&token, &socket);
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("0600"), "{}", c.detail);
        chmod(&token, 0o600);
        std::fs::remove_file(&token).unwrap();
        assert_eq!(check_token(&token, &socket).status, Status::Fail);
        // a symlink in place of the token is refused
        let target = t.write("elsewhere", &secret);
        chmod(&target, 0o600);
        std::os::unix::fs::symlink(&target, &token).unwrap();
        assert_eq!(check_token(&token, &socket).status, Status::Fail);
    }

    #[test]
    fn the_token_value_is_never_part_of_a_check() {
        let t = TempDir::new("doc-token-secret");
        let socket = t.join("fleet.sock");
        let token = t.join("token");
        let secret = crate::statedir::create_token_file(&token).unwrap();
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        for mode in [0o600, 0o644] {
            chmod(&token, mode);
            let c = check_token(&token, &socket);
            let shown = render_text(std::slice::from_ref(&c)) + &to_value(&[c]).canonical();
            assert!(!shown.contains(&secret), "{shown}");
        }
    }

    #[test]
    fn peer_credentials_and_pty_are_informational() {
        assert_eq!(check_peer_credentials().status, Status::Info);
        assert_eq!(check_pty().status, Status::Info);
    }

    fn sample() -> Vec<Check> {
        vec![
            Check::new("platform", Status::Ok, "macos"),
            Check::new("daemon", Status::Info, "not running"),
            Check::new("git", Status::Fail, "git \u{1b}[31m missing"),
        ]
    }

    #[test]
    fn health_follows_failures_only() {
        assert!(!is_healthy(&sample()));
        assert!(is_healthy(&sample()[..2]));
        assert!(is_healthy(&[]));
    }

    #[test]
    fn text_output_lists_each_check_and_a_verdict() {
        let out = render_text(&sample());
        assert!(out.contains("[ok  ] platform: macos\n"));
        assert!(out.contains("[info] daemon: not running\n"));
        assert!(out.contains("[FAIL] git:"));
        assert!(out.ends_with("unhealthy: 1 check(s) failed\n"));
        assert!(!out.contains('\u{1b}'), "check text is sanitized");
        assert!(render_text(&sample()[..2]).ends_with("healthy\n"));
    }

    #[test]
    fn json_document_shape() {
        let v = to_value(&sample());
        assert_eq!(
            v.get("schema").and_then(Value::as_str),
            Some("fleet.doctor/1")
        );
        assert_eq!(v.get("ok").and_then(Value::as_bool), Some(false));
        let checks = v.get("checks").and_then(Value::as_array).unwrap();
        assert_eq!(checks.len(), 3);
        let first = checks.first().unwrap();
        assert_eq!(first.get("name").and_then(Value::as_str), Some("platform"));
        assert_eq!(first.get("status").and_then(Value::as_str), Some("ok"));
        assert_eq!(first.get("detail").and_then(Value::as_str), Some("macos"));
        let last = checks.last().unwrap();
        assert_eq!(last.get("status").and_then(Value::as_str), Some("fail"));
        assert!(!last
            .get("detail")
            .and_then(Value::as_str)
            .unwrap()
            .contains('\u{1b}'));
        let ok = to_value(&sample()[..2]);
        assert_eq!(ok.get("ok").and_then(Value::as_bool), Some(true));
    }
}
