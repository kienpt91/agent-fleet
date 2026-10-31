//! Git adapter: every VCS operation runs the real `git` binary as a child process.
//!
//! Repositories are untrusted input: whoever can write `.git/config`, `.git/hooks` or
//! `.gitattributes` could make git execute programs. Every invocation therefore starts from an
//! empty environment, pins a fixed set of dangerous settings with `-c`, and additionally
//! neutralizes every config key whose *name* is chosen by the config author (filter, diff, merge
//! driver, pager and credential names) by listing the config first (`git config --list` runs
//! nothing) and overriding those keys through `GIT_CONFIG_COUNT`, which also copes with names that
//! contain `=` or other characters `-c` cannot carry.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use fleet_app::ports::{
    BaseCommit, Identity, IntegrationMerge, IntegrationResult, MergeOutcome, Vcs,
};
use fleet_domain::env::EnvPolicy;
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::ids::{is_invisible, show, validate_ref_name, RunId, Sha, TaskId};
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;

/// Hard cap per captured stream of one git call.
const MAX_OUTPUT: usize = 16 * 1024 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(120);
/// How long collecting the pipes may take after git itself exited (a straggler that escaped the
/// process group could otherwise hold them open forever).
const PIPE_GRACE: Duration = Duration::from_secs(5);
const MAX_OVERRIDES: usize = 2048;
const MAX_EXCLUDE_FILE: u64 = 1024 * 1024;
const MAX_MESSAGE: usize = 4 * 1024;
const MAX_CONFLICT_FILES: usize = 200;
const MAX_MERGE_BRANCHES: usize = 64;
const MIN_GIT: (u32, u32) = (2, 39);
const WORKTREES: &str = ".fleet-worktrees";
const INTEGRATION_DIR: &str = "_integration";

/// Settings pinned on every call, before the subcommand.
const STATIC_CONFIG: [(&str, &str); 23] = [
    ("core.hooksPath", "/dev/null"),
    ("core.fsmonitor", "false"),
    ("core.pager", "cat"),
    ("core.editor", "true"),
    ("core.sshCommand", "false"),
    ("core.askPass", "false"),
    ("core.attributesFile", "/dev/null"),
    ("credential.helper", ""),
    ("protocol.allow", "never"),
    ("protocol.ext.allow", "never"),
    ("commit.gpgsign", "false"),
    ("tag.gpgsign", "false"),
    ("gc.auto", "0"),
    ("maintenance.auto", "false"),
    ("diff.external", ""),
    ("merge.renormalize", "false"),
    ("merge.verifySignatures", "false"),
    ("core.symlinks", "true"),
    ("rerere.enabled", "false"),
    ("gpg.program", "false"),
    ("gpg.openpgp.program", "false"),
    ("gpg.x509.program", "false"),
    ("gpg.ssh.program", "false"),
];

const FIXED_ENV: [(&str, &str); 8] = [
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_OPTIONAL_LOCKS", "0"),
    ("GIT_ATTR_NOSYSTEM", "1"),
    ("GIT_PAGER", "cat"),
    ("LC_ALL", "C"),
];

type Override = (Vec<u8>, Vec<u8>);

struct Out {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Out {
    fn ok(&self) -> bool {
        self.code == Some(0)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout)
            .trim_end_matches('\n')
            .to_string()
    }
}

/// Text from git or from file names, made safe to show in an error or a log line.
fn safe_text(s: &str, max: usize) -> String {
    let mut out: String = s
        .chars()
        .take(max)
        .map(|c| {
            if c.is_control() || is_invisible(c) {
                '?'
            } else {
                c
            }
        })
        .collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

fn show_path(p: &Path) -> String {
    show(&p.to_string_lossy())
}

fn path_str(p: &Path) -> Result<&str> {
    p.to_str()
        .ok_or_else(|| FleetError::invalid("path is not valid UTF-8"))
}

/// Commit and merge messages: bounded, no control characters other than newlines.
fn clean_message(message: &str, default: &str) -> String {
    let mut out = String::new();
    for c in message.chars() {
        let c = if (c.is_control() && c != '\n') || is_invisible(c) {
            ' '
        } else {
            c
        };
        if out.len() + c.len_utf8() > MAX_MESSAGE {
            break;
        }
        out.push(c);
    }
    let t = out.trim();
    if t.is_empty() {
        default.to_string()
    } else {
        t.to_string()
    }
}

fn validate_identity(id: &Identity) -> Result<()> {
    let bad = |s: &str| {
        s.is_empty() || s.len() > 200 || s.chars().any(|c| c.is_control() || c == '<' || c == '>')
    };
    if bad(&id.name) || bad(&id.email) {
        return Err(FleetError::invalid("commit identity is not valid"));
    }
    Ok(())
}

/// A fleet branch: a valid ref name below `fleet/`.
fn validate_fleet_branch(branch: &str) -> Result<()> {
    validate_ref_name(branch)?;
    if !branch.starts_with("fleet/") {
        return Err(FleetError::invalid(format!(
            "branch {} is outside the fleet/ namespace",
            show(branch)
        )));
    }
    Ok(())
}

fn insert_override(
    map: &mut BTreeMap<Vec<u8>, Vec<u8>>,
    section: &str,
    sub: &[u8],
    var: &str,
    val: &str,
) {
    let mut key = section.as_bytes().to_vec();
    key.push(b'.');
    key.extend_from_slice(sub);
    key.push(b'.');
    key.extend_from_slice(var.as_bytes());
    map.insert(key, val.as_bytes().to_vec());
}

/// Turns the output of `git config --list -z` into overrides that disable every program the
/// config could name through a user-chosen key.
fn dynamic_overrides(list: &[u8]) -> Result<Vec<Override>> {
    let mut map: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for entry in list.split(|b| *b == 0) {
        if entry.is_empty() {
            continue;
        }
        let key = match entry.iter().position(|b| *b == b'\n') {
            Some(p) => entry.get(..p).unwrap_or(entry),
            None => entry,
        };
        let (Some(first), Some(last)) = (
            key.iter().position(|b| *b == b'.'),
            key.iter().rposition(|b| *b == b'.'),
        ) else {
            continue;
        };
        let (Some(section), Some(_var)) = (key.get(..first), key.get(last + 1..)) else {
            continue;
        };
        let sub = if first < last {
            key.get(first + 1..last)
        } else {
            None
        };
        let section = section.to_ascii_lowercase();
        match (section.as_slice(), sub) {
            (b"filter", Some(n)) => {
                for v in ["clean", "smudge", "process"] {
                    insert_override(&mut map, "filter", n, v, "");
                }
                insert_override(&mut map, "filter", n, "required", "false");
            }
            (b"diff", Some(n)) => {
                insert_override(&mut map, "diff", n, "command", "");
                insert_override(&mut map, "diff", n, "textconv", "");
            }
            (b"merge", Some(n)) => {
                insert_override(&mut map, "merge", n, "driver", "false");
                insert_override(&mut map, "merge", n, "name", "");
            }
            (b"credential", Some(n)) => insert_override(&mut map, "credential", n, "helper", ""),
            (b"gpg", Some(n)) => insert_override(&mut map, "gpg", n, "program", "false"),
            (b"pager", None) => {
                map.insert(key.to_vec(), b"cat".to_vec());
            }
            _ => {}
        }
        if map.len() > MAX_OVERRIDES {
            return Err(FleetError::limit(
                "the repository config is too large to harden",
            ));
        }
    }
    Ok(map.into_iter().collect())
}

fn read_capped<R: Read>(mut r: R, cap: usize, over: &AtomicBool) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let take = n.min(cap.saturating_sub(buf.len()));
                if let Some(part) = chunk.get(..take) {
                    buf.extend_from_slice(part);
                }
                if take < n {
                    over.store(true, Ordering::SeqCst);
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    buf
}

fn kill_group(child: &mut Child, pgid: Pid) {
    let _ = killpg(pgid, Signal::SIGKILL);
    let _ = child.kill();
    let _ = child.wait();
}

fn is_executable(p: &Path) -> bool {
    fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn not_found_kind(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::NotFound
}

/// A directory fleet creates inside the repository must be a real directory, never a symlink
/// someone planted to redirect worktrees elsewhere.
fn ensure_real_dir(p: &Path) -> Result<()> {
    let check = || -> Result<()> {
        let meta =
            fs::symlink_metadata(p).map_err(|e| FleetError::io("cannot stat directory", &e))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(FleetError::forbidden(format!(
                "{} is not a real directory",
                show_path(p)
            )));
        }
        Ok(())
    };
    match fs::symlink_metadata(p) {
        Ok(_) => check(),
        Err(e) if not_found_kind(&e) => {
            match fs::DirBuilder::new().mode(0o700).create(p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(FleetError::io("cannot create directory", &e)),
            }
            check()
        }
        Err(e) => Err(FleetError::io("cannot stat directory", &e)),
    }
}

pub struct GitVcs {
    env: Vec<(String, String)>,
    timeout: Duration,
    max_output: usize,
}

impl GitVcs {
    pub fn new(parent_env: Vec<(String, String)>, policy: EnvPolicy) -> GitVcs {
        let fixed: Vec<(String, String)> = FIXED_ENV
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        GitVcs {
            env: policy.build(&parent_env, &[], &[], &fixed),
            timeout: GIT_TIMEOUT,
            max_output: MAX_OUTPUT,
        }
    }

    #[cfg(test)]
    fn with_limits(mut self, timeout: Duration, max_output: usize) -> GitVcs {
        self.timeout = timeout;
        self.max_output = max_output;
        self
    }

    fn find_git(&self) -> Result<PathBuf> {
        let path = self
            .env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        for dir in path.split(':') {
            let dir = Path::new(dir);
            if !dir.is_absolute() {
                continue;
            }
            let candidate = dir.join("git");
            if is_executable(&candidate) {
                return Ok(candidate);
            }
        }
        Err(FleetError::unavailable(
            "git was not found on the allowlisted PATH",
        ))
    }

    /// Runs git once with the static hardening and the given dynamic overrides.
    fn run_raw(
        &self,
        cwd: &Path,
        ident: Option<&Identity>,
        overrides: &[Override],
        args: &[&str],
    ) -> Result<Out> {
        let mut cmd = Command::new(self.find_git()?);
        cmd.env_clear();
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        if !overrides.is_empty() {
            cmd.env("GIT_CONFIG_COUNT", overrides.len().to_string());
            for (i, (k, v)) in overrides.iter().enumerate() {
                cmd.env(format!("GIT_CONFIG_KEY_{i}"), OsStr::from_bytes(k));
                cmd.env(format!("GIT_CONFIG_VALUE_{i}"), OsStr::from_bytes(v));
            }
        }
        for (k, v) in STATIC_CONFIG {
            cmd.arg("-c").arg(format!("{k}={v}"));
        }
        if let Some(id) = ident {
            cmd.arg("-c").arg(format!("user.name={}", id.name));
            cmd.arg("-c").arg(format!("user.email={}", id.email));
        }
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);

        let mut child = cmd
            .spawn()
            .map_err(|e| FleetError::io("cannot start git", &e))?;
        let pgid = match i32::try_from(child.id()) {
            Ok(p) => Pid::from_raw(p),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(FleetError::internal("git process id out of range"));
            }
        };
        let over = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
        let (Some(so), Some(se)) = (child.stdout.take(), child.stderr.take()) else {
            kill_group(&mut child, pgid);
            return Err(FleetError::internal("git pipes are missing"));
        };
        let cap = self.max_output;
        let spawn_reader = |is_out: bool, pipe: Box<dyn Read + Send>| {
            let tx = tx.clone();
            let over = Arc::clone(&over);
            thread::Builder::new()
                .name("fleet-git-pipe".to_string())
                .spawn(move || {
                    let buf = read_capped(pipe, cap, &over);
                    let _ = tx.send((is_out, buf));
                })
        };
        if let Err(e) =
            spawn_reader(true, Box::new(so)).and_then(|_| spawn_reader(false, Box::new(se)))
        {
            kill_group(&mut child, pgid);
            return Err(FleetError::io("cannot start a reader thread", &e));
        }
        drop(tx);

        let start = Instant::now();
        let mut delay = Duration::from_millis(1);
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) => {}
                Err(e) => {
                    kill_group(&mut child, pgid);
                    return Err(FleetError::io("cannot wait for git", &e));
                }
            }
            if over.load(Ordering::SeqCst) {
                kill_group(&mut child, pgid);
                return Err(FleetError::limit(format!(
                    "git produced more than {} bytes of output",
                    self.max_output
                )));
            }
            if start.elapsed() >= self.timeout {
                kill_group(&mut child, pgid);
                return Err(FleetError::new(
                    ErrorCode::Timeout,
                    format!("git did not finish within {} s", self.timeout.as_secs()),
                ));
            }
            thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_millis(20));
        };
        // Anything git left behind in its group must not outlive the call.
        let _ = killpg(pgid, Signal::SIGKILL);

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        for _ in 0..2 {
            match rx.recv_timeout(PIPE_GRACE) {
                Ok((true, buf)) => stdout = buf,
                Ok((false, buf)) => stderr = buf,
                Err(_) => {
                    return Err(FleetError::new(
                        ErrorCode::Timeout,
                        "git output pipes stayed open after git exited",
                    ))
                }
            }
        }
        if over.load(Ordering::SeqCst) {
            return Err(FleetError::limit(format!(
                "git produced more than {} bytes of output",
                self.max_output
            )));
        }
        Ok(Out {
            code: status.code(),
            stdout,
            stderr,
        })
    }

    /// Runs git with the config of `cwd` neutralized (see the module comment).
    fn run(&self, cwd: &Path, ident: Option<&Identity>, args: &[&str]) -> Result<Out> {
        let listing = self.run_raw(cwd, None, &[], &["config", "--list", "-z"])?;
        let overrides = if listing.ok() {
            dynamic_overrides(&listing.stdout)?
        } else {
            Vec::new()
        };
        self.run_raw(cwd, ident, &overrides, args)
    }

    fn run_ok(&self, cwd: &Path, ident: Option<&Identity>, args: &[&str]) -> Result<Out> {
        let out = self.run(cwd, ident, args)?;
        if out.ok() {
            return Ok(out);
        }
        Err(self.failure(args, &out))
    }

    fn failure(&self, args: &[&str], out: &Out) -> FleetError {
        let detail = String::from_utf8_lossy(&out.stderr);
        let detail = if detail.trim().is_empty() {
            String::from_utf8_lossy(&out.stdout).into_owned()
        } else {
            detail.into_owned()
        };
        let sub = args.first().copied().unwrap_or("?");
        let code = out.code.map_or("signal".to_string(), |c| c.to_string());
        FleetError::internal(format!(
            "git {} failed ({code}): {}",
            safe_text(sub, 32),
            safe_text(detail.trim(), 240)
        ))
    }

    fn exit_code(&self, cwd: &Path, args: &[&str]) -> Result<i32> {
        let out = self.run(cwd, None, args)?;
        out.code
            .ok_or_else(|| FleetError::internal("git was terminated by a signal"))
    }

    /// The version line of the installed git; fails when it is older than 2.39.
    fn version(&self) -> Result<String> {
        let out = self.run_raw(Path::new("/"), None, &[], &["--version"])?;
        if !out.ok() {
            return Err(self.failure(&["--version"], &out));
        }
        let text = out.text();
        let numbers: Vec<u32> = text
            .split_whitespace()
            .nth(2)
            .unwrap_or("")
            .split('.')
            .take(2)
            .map_while(|p| p.parse().ok())
            .collect();
        let (Some(&major), Some(&minor)) = (numbers.first(), numbers.get(1)) else {
            return Err(FleetError::unavailable(format!(
                "cannot read the git version from {}",
                show(&text)
            )));
        };
        if (major, minor) < MIN_GIT {
            return Err(FleetError::unavailable(format!(
                "git {}.{} or newer is required, found {}",
                MIN_GIT.0,
                MIN_GIT.1,
                safe_text(&text, 80)
            )));
        }
        Ok(safe_text(&text, 120))
    }

    // ---- repository checks -------------------------------------------------------------

    /// The canonical path of a directory given as `repo`; a symlink is refused so a registered
    /// repository cannot be swapped for another one underneath fleet.
    fn repo_dir(&self, repo: &str) -> Result<PathBuf> {
        if repo.is_empty() || repo.contains('\0') {
            return Err(FleetError::invalid("repository path is empty or invalid"));
        }
        let p = Path::new(repo);
        let meta = fs::symlink_metadata(p).map_err(|_| {
            FleetError::not_found(format!("repository {} does not exist", show(repo)))
        })?;
        if meta.file_type().is_symlink() {
            return Err(FleetError::forbidden(format!(
                "repository path {} is a symbolic link",
                show(repo)
            )));
        }
        if !meta.is_dir() {
            return Err(FleetError::invalid(format!(
                "repository path {} is not a directory",
                show(repo)
            )));
        }
        fs::canonicalize(p).map_err(|e| FleetError::io("cannot resolve the repository path", &e))
    }

    fn canonical_line(&self, line: &str) -> Result<PathBuf> {
        fs::canonicalize(line).map_err(|e| FleetError::io("cannot resolve a git path", &e))
    }

    /// A main work tree root: not a subdirectory, not a linked worktree, not a symlink.
    fn open_repo(&self, repo: &str) -> Result<PathBuf> {
        let dir = self.repo_dir(repo)?;
        let top = self.run(&dir, None, &["rev-parse", "--show-toplevel"])?;
        if !top.ok() {
            return Err(FleetError::invalid(format!(
                "{} is not a git work tree",
                show(repo)
            )));
        }
        if self.canonical_line(&top.text())? != dir {
            return Err(FleetError::invalid(format!(
                "{} is not the top level of a git work tree",
                show(repo)
            )));
        }
        let dirs = self.run_ok(
            &dir,
            None,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-dir",
                "--git-common-dir",
            ],
        )?;
        let text = dirs.text();
        let mut lines = text.lines();
        let (Some(git_dir), Some(common)) = (lines.next(), lines.next()) else {
            return Err(FleetError::internal("unexpected output from git rev-parse"));
        };
        if self.canonical_line(git_dir)? != self.canonical_line(common)? {
            return Err(FleetError::forbidden(format!(
                "{} is a linked worktree, not a main repository",
                show(repo)
            )));
        }
        Ok(dir)
    }

    /// A worktree fleet manages: below `.fleet-worktrees` and its own top level.
    fn check_worktree(&self, worktree: &str) -> Result<PathBuf> {
        if worktree.is_empty() || worktree.contains('\0') {
            return Err(FleetError::invalid("worktree path is empty or invalid"));
        }
        let p = Path::new(worktree);
        let meta = fs::symlink_metadata(p).map_err(|_| {
            FleetError::not_found(format!("worktree {} does not exist", show(worktree)))
        })?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(FleetError::forbidden(format!(
                "worktree {} is not a real directory",
                show(worktree)
            )));
        }
        let canon =
            fs::canonicalize(p).map_err(|e| FleetError::io("cannot resolve worktree", &e))?;
        if !canon
            .components()
            .any(|c| c.as_os_str() == OsStr::new(WORKTREES))
        {
            return Err(FleetError::forbidden(format!(
                "{} is not a fleet worktree",
                show(worktree)
            )));
        }
        let top = self.run_ok(&canon, None, &["rev-parse", "--show-toplevel"])?;
        if self.canonical_line(&top.text())? != canon {
            return Err(FleetError::forbidden(format!(
                "{} is not the top level of a worktree",
                show(worktree)
            )));
        }
        Ok(canon)
    }

    fn head_sha(&self, cwd: &Path) -> Result<Sha> {
        let out = self.run_ok(
            cwd,
            None,
            &["rev-parse", "--verify", "--end-of-options", "HEAD^{commit}"],
        )?;
        Sha::parse(out.text().trim())
    }

    fn ref_exists(&self, repo: &Path, full_ref: &str) -> Result<bool> {
        Ok(self.exit_code(
            repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                full_ref,
            ],
        )? == 0)
    }

    /// Adds fleet's directories to the shared `info/exclude` (idempotent).
    fn register_excludes(&self, repo: &Path) -> Result<()> {
        let common = self
            .run_ok(
                repo,
                None,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )?
            .text();
        let info = Path::new(&common).join("info");
        ensure_real_dir(&info)?;
        let file = info.join("exclude");
        let mut existing = String::new();
        match fs::symlink_metadata(&file) {
            Ok(m) => {
                if m.file_type().is_symlink() || !m.is_file() {
                    return Err(FleetError::forbidden(format!(
                        "{} is not a regular file",
                        show_path(&file)
                    )));
                }
                let f = fs::File::open(&file)
                    .map_err(|e| FleetError::io("cannot read info/exclude", &e))?;
                let mut raw = Vec::new();
                f.take(MAX_EXCLUDE_FILE + 1)
                    .read_to_end(&mut raw)
                    .map_err(|e| FleetError::io("cannot read info/exclude", &e))?;
                if raw.len() as u64 > MAX_EXCLUDE_FILE {
                    return Err(FleetError::limit("info/exclude is too large"));
                }
                existing = String::from_utf8_lossy(&raw).into_owned();
            }
            Err(e) if not_found_kind(&e) => {}
            Err(e) => return Err(FleetError::io("cannot stat info/exclude", &e)),
        }
        let missing: Vec<&str> = [WORKTREES, ".fleet/"]
            .into_iter()
            .filter(|want| !existing.lines().any(|l| l.trim() == *want))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let mut content = existing;
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        for line in missing {
            content.push_str(line);
            content.push('\n');
        }
        let tmp = info.join("exclude.fleet-tmp");
        let _ = fs::remove_file(&tmp);
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| FleetError::io("cannot write info/exclude", &e))?;
        f.write_all(content.as_bytes())
            .and_then(|_| f.sync_all())
            .map_err(|e| FleetError::io("cannot write info/exclude", &e))?;
        fs::rename(&tmp, &file).map_err(|e| FleetError::io("cannot replace info/exclude", &e))
    }

    /// Registered worktrees with the branch each has checked out.
    fn worktree_branches(&self, repo: &Path) -> Result<Vec<(PathBuf, Option<String>)>> {
        let out = self.run_ok(repo, None, &["worktree", "list", "--porcelain", "-z"])?;
        let mut list: Vec<(PathBuf, Option<String>)> = Vec::new();
        for field in out.stdout.split(|b| *b == 0) {
            if let Some(path) = field.strip_prefix(b"worktree ") {
                list.push((PathBuf::from(OsStr::from_bytes(path).to_os_string()), None));
            } else if let Some(branch) = field.strip_prefix(b"branch refs/heads/") {
                if let Some(last) = list.last_mut() {
                    last.1 = Some(String::from_utf8_lossy(branch).into_owned());
                }
            }
        }
        Ok(list)
    }

    /// Creates or reuses the worktree at `path` for `branch`. Returns whether the branch had to
    /// be created (only possible when `base` is given).
    fn add_worktree(
        &self,
        repo: &Path,
        path: &Path,
        branch: &str,
        base: Option<&Sha>,
    ) -> Result<bool> {
        let registered = self.worktree_branches(repo)?;
        let entry = registered.iter().find(|(p, _)| p == path);
        match fs::symlink_metadata(path) {
            Ok(m) => {
                if m.file_type().is_symlink() {
                    return Err(FleetError::forbidden(format!(
                        "{} is a symbolic link",
                        show_path(path)
                    )));
                }
                return match entry {
                    Some((_, Some(b))) if b == branch => Ok(false),
                    Some(_) => Err(FleetError::conflict(format!(
                        "{} is checked out on a different branch",
                        show_path(path)
                    ))),
                    None => Err(FleetError::conflict(format!(
                        "{} exists but is not a worktree of this repository",
                        show_path(path)
                    ))),
                };
            }
            Err(e) if not_found_kind(&e) => {
                if entry.is_some() {
                    // Registered but the directory is gone: git refuses to add it again until
                    // the stale record is pruned.
                    self.run_ok(repo, None, &["worktree", "prune"])?;
                }
            }
            Err(e) => return Err(FleetError::io("cannot stat worktree path", &e)),
        }
        let path_s = path_str(path)?;
        let full = format!("refs/heads/{branch}");
        if self.ref_exists(repo, &full)? {
            self.run_ok(repo, None, &["worktree", "add", "--quiet", path_s, branch])?;
            return Ok(false);
        }
        let Some(base) = base else {
            return Err(FleetError::not_found(format!(
                "branch {} does not exist",
                show(branch)
            )));
        };
        self.run_ok(
            repo,
            None,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                branch,
                path_s,
                base.as_str(),
            ],
        )?;
        Ok(true)
    }

    fn worktrees_dir(&self, repo: &Path, run: &RunId) -> Result<PathBuf> {
        let root = repo.join(WORKTREES);
        ensure_real_dir(&root)?;
        let rundir = root.join(run.as_str());
        ensure_real_dir(&rundir)?;
        self.register_excludes(repo)?;
        Ok(rundir)
    }

    // ---- merging -----------------------------------------------------------------------

    fn is_clean(&self, wt: &Path) -> Result<bool> {
        if self.exit_code(wt, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"])? == 0 {
            return Ok(false);
        }
        let status = self.run_ok(wt, None, &["status", "--porcelain", "--untracked-files=no"])?;
        Ok(status.stdout.is_empty())
    }

    fn conflict_files(&self, wt: &Path) -> Result<Vec<String>> {
        let out = self.run_ok(
            wt,
            None,
            &[
                "diff",
                "--name-only",
                "--diff-filter=U",
                "-z",
                "--no-ext-diff",
                "--no-textconv",
            ],
        )?;
        Ok(out
            .stdout
            .split(|b| *b == 0)
            .filter(|f| !f.is_empty())
            .take(MAX_CONFLICT_FILES)
            .map(|f| safe_text(&String::from_utf8_lossy(f), 400))
            .collect())
    }

    /// Aborts a conflicted merge and proves the tree is clean again, never leaving a half merge.
    fn abort_merge(&self, wt: &Path, ident: &Identity) -> Result<()> {
        let _ = self.run(wt, Some(ident), &["merge", "--abort"]);
        if !self.is_clean(wt)? {
            self.run_ok(wt, None, &["reset", "--hard", "HEAD"])?;
            if !self.is_clean(wt)? {
                return Err(FleetError::internal(
                    "could not restore a clean worktree after a merge conflict",
                ));
            }
        }
        Ok(())
    }

    fn merge_one(
        &self,
        wt: &Path,
        ident: &Identity,
        ff_flag: &str,
        message: Option<&str>,
        branch: &str,
    ) -> Result<MergeOutcome> {
        let full = format!("refs/heads/{branch}");
        let mut args = vec![
            "merge",
            "--no-verify",
            "--no-gpg-sign",
            "--no-verify-signatures",
            "--no-edit",
            ff_flag,
        ];
        if let Some(m) = message {
            args.push("-m");
            args.push(m);
        }
        args.push(&full);
        let out = self.run(wt, Some(ident), &args)?;
        if out.ok() {
            return Ok(MergeOutcome::Merged {
                sha: self.head_sha(wt)?,
            });
        }
        if self.exit_code(wt, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"])? != 0 {
            // Not a conflict (unrelated histories, unmergeable state, ...).
            return Err(self.failure(&args, &out));
        }
        let files = self.conflict_files(wt)?;
        self.abort_merge(wt, ident)?;
        Ok(MergeOutcome::Conflict { files })
    }
}

impl Vcs for GitVcs {
    fn resolve_base(&self, repo: &str, base_ref: &str) -> Result<BaseCommit> {
        let dir = self.open_repo(repo)?;
        if base_ref != "HEAD" {
            validate_ref_name(base_ref)?;
        }
        if self.exit_code(
            &dir,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                "HEAD^{commit}",
            ],
        )? != 0
        {
            return Err(FleetError::invalid(format!(
                "repository {} has no commits",
                show(repo)
            )));
        }
        let spec = format!("{base_ref}^{{commit}}");
        let out = self.run(
            &dir,
            None,
            &["rev-parse", "--verify", "--end-of-options", &spec],
        )?;
        if !out.ok() {
            return Err(FleetError::not_found(format!(
                "base ref {} does not name a commit",
                show(base_ref)
            )));
        }
        Ok(BaseCommit {
            ref_name: base_ref.to_string(),
            sha: Sha::parse(out.text().trim())?,
        })
    }

    fn prepare_worktree(
        &self,
        repo: &str,
        run: &RunId,
        task: &TaskId,
        branch: &str,
        base: &Sha,
    ) -> Result<String> {
        validate_fleet_branch(branch)?;
        let dir = self.open_repo(repo)?;
        let rundir = self.worktrees_dir(&dir, run)?;
        let path = rundir.join(task.as_str());
        self.add_worktree(&dir, &path, branch, Some(base))?;
        Ok(path_str(&path)?.to_string())
    }

    fn merge_into_worktree(
        &self,
        worktree: &str,
        branches: &[String],
        identity: &Identity,
    ) -> Result<MergeOutcome> {
        validate_identity(identity)?;
        if branches.len() > MAX_MERGE_BRANCHES {
            return Err(FleetError::limit("too many branches to merge"));
        }
        for b in branches {
            validate_ref_name(b)?;
        }
        let wt = self.check_worktree(worktree)?;
        for b in branches {
            if let conflict @ MergeOutcome::Conflict { .. } =
                self.merge_one(&wt, identity, "--ff", None, b)?
            {
                return Ok(conflict);
            }
        }
        Ok(MergeOutcome::Merged {
            sha: self.head_sha(&wt)?,
        })
    }

    fn commit_all(
        &self,
        worktree: &str,
        message: &str,
        identity: &Identity,
    ) -> Result<Option<Sha>> {
        validate_identity(identity)?;
        let wt = self.check_worktree(worktree)?;
        let message = clean_message(message, "fleet: task changes");
        self.run_ok(&wt, None, &["add", "-A"])?;
        // Fleet's own directory must never reach a commit, even when something force-added it.
        self.run_ok(&wt, None, &["reset", "-q", "--", ".fleet"])?;
        match self.exit_code(&wt, &["diff", "--cached", "--quiet", "--no-ext-diff"])? {
            0 => return Ok(None),
            1 => {}
            _ => {
                return Err(FleetError::internal("git diff --cached failed"));
            }
        }
        self.run_ok(
            &wt,
            Some(identity),
            &["commit", "--no-verify", "--no-gpg-sign", "-m", &message],
        )?;
        Ok(Some(self.head_sha(&wt)?))
    }

    fn branch_head(&self, repo: &str, branch: &str) -> Result<Sha> {
        validate_ref_name(branch)?;
        let dir = self.repo_dir(repo)?;
        let spec = format!("refs/heads/{branch}^{{commit}}");
        let out = self.run(
            &dir,
            None,
            &["rev-parse", "--verify", "--end-of-options", &spec],
        )?;
        if !out.ok() {
            return Err(FleetError::not_found(format!(
                "branch {} does not exist",
                show(branch)
            )));
        }
        Sha::parse(out.text().trim())
    }

    fn merge_into_integration(&self, req: &IntegrationMerge) -> Result<IntegrationResult> {
        validate_identity(&req.identity)?;
        validate_fleet_branch(&req.integration_branch)?;
        validate_fleet_branch(&req.task_branch)?;
        let dir = self.open_repo(&req.repo)?;
        let rundir = self.worktrees_dir(&dir, &req.run)?;
        let path = rundir.join(INTEGRATION_DIR);
        let created = self.add_worktree(&dir, &path, &req.integration_branch, Some(&req.base))?;
        let message = clean_message(&req.message, "fleet: merge task");
        let outcome = self.merge_one(
            &path,
            &req.identity,
            "--no-ff",
            Some(&message),
            &req.task_branch,
        )?;
        Ok(IntegrationResult { outcome, created })
    }

    fn remove_worktree(
        &self,
        repo: &str,
        worktree: &str,
        branch: &str,
        delete_branch: bool,
    ) -> Result<()> {
        if delete_branch {
            validate_ref_name(branch)?;
        }
        if worktree.contains('\0') {
            return Err(FleetError::invalid("worktree path is invalid"));
        }
        let dir = self.open_repo(repo)?;
        let root = dir.join(WORKTREES);
        let wt = Path::new(worktree);
        if !wt.is_absolute() {
            return Err(FleetError::forbidden("worktree path must be absolute"));
        }
        match fs::symlink_metadata(wt) {
            Ok(m) => {
                if m.file_type().is_symlink() {
                    return Err(FleetError::forbidden(format!(
                        "worktree {} is a symbolic link",
                        show(worktree)
                    )));
                }
                let canon = fs::canonicalize(wt)
                    .map_err(|e| FleetError::io("cannot resolve worktree", &e))?;
                if !canon.starts_with(&root) || canon == root {
                    return Err(FleetError::forbidden(format!(
                        "worktree {} is outside {WORKTREES}",
                        show(worktree)
                    )));
                }
                self.run_ok(
                    &dir,
                    None,
                    &["worktree", "remove", "--force", path_str(&canon)?],
                )?;
            }
            Err(e) if not_found_kind(&e) => {
                let plain = wt
                    .components()
                    .all(|c| !matches!(c, Component::ParentDir | Component::CurDir));
                if !plain || !wt.starts_with(&root) || wt == root {
                    return Err(FleetError::forbidden(format!(
                        "worktree {} is outside {WORKTREES}",
                        show(worktree)
                    )));
                }
            }
            Err(e) => return Err(FleetError::io("cannot stat worktree", &e)),
        }
        self.run_ok(&dir, None, &["worktree", "prune"])?;
        if delete_branch
            && branch.starts_with("fleet/")
            && self.ref_exists(&dir, &format!("refs/heads/{branch}"))?
        {
            self.run_ok(&dir, None, &["branch", "-D", branch])?;
        }
        Ok(())
    }

    fn is_merged(&self, repo: &str, branch: &str, into: &str) -> Result<bool> {
        validate_ref_name(branch)?;
        let target = if into == "HEAD" {
            "HEAD".to_string()
        } else {
            validate_ref_name(into)?;
            format!("refs/heads/{into}")
        };
        let dir = self.repo_dir(repo)?;
        let source = format!("refs/heads/{branch}");
        let out = self.run(
            &dir,
            None,
            &["merge-base", "--is-ancestor", &source, &target],
        )?;
        match out.code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(self.failure(&["merge-base"], &out)),
        }
    }
}

/// The installed git's version line (used by `fleet doctor`); an error when git is missing or
/// older than 2.39.
pub fn check_git_version(parent_env: &[(String, String)], policy: &EnvPolicy) -> Result<String> {
    GitVcs::new(parent_env.to_vec(), policy.clone()).version()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::testutil::{git, git_available, make_repo, wait_until, TempDir};

    macro_rules! need_git {
        () => {
            if !git_available() {
                return;
            }
        };
    }

    fn ident() -> Identity {
        Identity {
            name: "Fleet Bot".to_string(),
            email: "bot@example.invalid".to_string(),
        }
    }

    fn vcs_with(extra: &[(&str, &str)]) -> GitVcs {
        let mut env = vec![(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        )];
        for (k, v) in extra {
            env.push((k.to_string(), v.to_string()));
        }
        GitVcs::new(env, EnvPolicy::default_policy())
    }

    fn rid(s: &str) -> RunId {
        RunId::parse(s).unwrap()
    }

    fn tid(s: &str) -> TaskId {
        TaskId::parse(s).unwrap()
    }

    struct Fx {
        tmp: TempDir,
        repo: PathBuf,
        vcs: GitVcs,
        marker: PathBuf,
    }

    impl Fx {
        fn new() -> Fx {
            Fx::with_attributes(None)
        }

        /// `attributes` is committed to the repository before anything hostile is planted.
        fn with_attributes(attributes: Option<&str>) -> Fx {
            let tmp = TempDir::new("git");
            let repo = make_repo(&tmp);
            if let Some(a) = attributes {
                fs::write(repo.join(".gitattributes"), a).unwrap();
                git(&repo, &["add", "-A"]);
                git(&repo, &["commit", "-q", "-m", "attributes"]);
            }
            let marker = tmp.join("marker");
            Fx {
                tmp,
                repo,
                vcs: vcs_with(&[("SENTINEL_VAR", "leaked")]),
                marker,
            }
        }

        fn repo_s(&self) -> &str {
            self.repo.to_str().unwrap()
        }

        /// A script that records that it ran and what it saw, then behaves like `cat`.
        fn plant_script(&self) -> String {
            let m = self.marker.display();
            let p = self.tmp.script(
                "plant.sh",
                &format!("#!/bin/sh\n: > '{m}'\necho \"$SENTINEL_VAR\" > '{m}.env'\ncat \"$@\" 2>/dev/null\nexit 0\n"),
            );
            p.to_str().unwrap().to_string()
        }

        fn append_config(&self, text: &str) {
            let mut f = fs::OpenOptions::new()
                .append(true)
                .open(self.repo.join(".git/config"))
                .unwrap();
            f.write_all(text.as_bytes()).unwrap();
        }

        fn triggered(&self) -> bool {
            self.tmp.exists("marker") || self.tmp.exists("marker.env")
        }

        fn base(&self) -> Sha {
            self.vcs.resolve_base(self.repo_s(), "HEAD").unwrap().sha
        }

        fn prepare(&self, run: &str, task: &str) -> String {
            self.vcs
                .prepare_worktree(
                    self.repo_s(),
                    &rid(run),
                    &tid(task),
                    &format!("fleet/{run}/{task}"),
                    &self.base(),
                )
                .unwrap()
        }

        fn edit_and_commit(&self, wt: &str, file: &str, content: &str) -> Sha {
            let p = Path::new(wt).join(file);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(p, content).unwrap();
            self.vcs
                .commit_all(wt, "task work", &ident())
                .unwrap()
                .expect("a commit")
        }

        fn integrate(&self, run: &str, task: &str) -> IntegrationResult {
            self.vcs
                .merge_into_integration(&IntegrationMerge {
                    repo: self.repo_s().to_string(),
                    run: rid(run),
                    task: tid(task),
                    base: self.base(),
                    integration_branch: format!("fleet/{run}/integration"),
                    task_branch: format!("fleet/{run}/{task}"),
                    message: format!("merge {task}"),
                    identity: ident(),
                })
                .unwrap()
        }
    }

    fn merge_head_exists(dir: &Path) -> bool {
        let p = git(dir, &["rev-parse", "--git-path", "MERGE_HEAD"]);
        let p = Path::new(&p);
        if p.is_absolute() {
            p.exists()
        } else {
            dir.join(p).exists()
        }
    }

    /// Drives every port method; with hostile config planted in the repository none of it may run.
    fn exercise(fx: &Fx) {
        let repo = fx.repo_s();
        let vcs = &fx.vcs;
        let w1 = fx.prepare("r1", "t1");
        let w2 = fx.prepare("r1", "t2");
        fx.edit_and_commit(&w1, "src/lib.txt", "one\nA\nthree\n");
        fx.edit_and_commit(&w1, "new.txt", "new file\n");
        fx.edit_and_commit(&w2, "src/lib.txt", "one\nB\nthree\n");
        let first = fx.integrate("r1", "t1");
        assert!(matches!(first.outcome, MergeOutcome::Merged { .. }));
        let second = fx.integrate("r1", "t2");
        assert!(matches!(second.outcome, MergeOutcome::Conflict { .. }));
        let w3 = fx.prepare("r1", "t3");
        let merged = vcs
            .merge_into_worktree(&w3, &["fleet/r1/t1".to_string()], &ident())
            .unwrap();
        assert!(matches!(merged, MergeOutcome::Merged { .. }));
        vcs.branch_head(repo, "fleet/r1/t1").unwrap();
        assert!(vcs
            .is_merged(repo, "fleet/r1/t1", "fleet/r1/integration")
            .unwrap());
        vcs.remove_worktree(repo, &w1, "fleet/r1/t1", true).unwrap();
    }

    fn vector(plant: impl FnOnce(&Fx), attributes: Option<&str>) {
        if !git_available() {
            return;
        }
        let fx = Fx::with_attributes(attributes);
        plant(&fx);
        exercise(&fx);
        assert!(!fx.triggered(), "a planted command ran");
    }

    // ---- hardening vectors: one test each ------------------------------------------------

    fn plant_hooks_in(dir: &Path, fx: &Fx) {
        for h in [
            "pre-commit",
            "post-checkout",
            "post-merge",
            "commit-msg",
            "pre-merge-commit",
            "prepare-commit-msg",
            "post-commit",
            "reference-transaction",
        ] {
            let m = fx.marker.display();
            fs::create_dir_all(dir).unwrap();
            let p = dir.join(h);
            fs::write(
                &p,
                format!("#!/bin/sh\n: > '{m}'\necho \"$SENTINEL_VAR\" > '{m}.env'\nexit 0\n"),
            )
            .unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn baseline_without_plants_works() {
        vector(|_| {}, None);
    }

    #[test]
    fn vector_git_hooks_dir() {
        vector(|fx| plant_hooks_in(&fx.repo.join(".git/hooks"), fx), None);
    }

    #[test]
    fn vector_configured_hooks_path() {
        vector(
            |fx| {
                let dir = fx.tmp.join("evil-hooks");
                plant_hooks_in(&dir, fx);
                fx.append_config(&format!("[core]\n\thooksPath = {}\n", dir.display()));
            },
            None,
        );
    }

    #[test]
    fn vector_fsmonitor() {
        vector(
            |fx| {
                let s = fx.plant_script();
                fx.append_config(&format!("[core]\n\tfsmonitor = {s}\n"));
            },
            None,
        );
    }

    #[test]
    fn vector_clean_smudge_process_filter() {
        vector(
            |fx| {
                let s = fx.plant_script();
                fx.append_config(&format!(
                    "[filter \"x\"]\n\tclean = {s}\n\tsmudge = {s}\n\tprocess = {s}\n\trequired = true\n"
                ));
            },
            Some("*.txt filter=x\n"),
        );
    }

    #[test]
    fn vector_filter_name_with_equals_sign() {
        vector(
            |fx| {
                let s = fx.plant_script();
                fx.append_config(&format!(
                    "[filter \"a=b\"]\n\tclean = {s}\n\tsmudge = {s}\n"
                ));
            },
            Some("*.txt filter=a=b\n"),
        );
    }

    #[test]
    fn vector_merge_driver() {
        vector(
            |fx| {
                let s = fx.plant_script();
                fx.append_config(&format!(
                    "[merge \"drv\"]\n\tname = x\n\tdriver = {s} %O %A %B\n"
                ));
            },
            Some("*.txt merge=drv\n"),
        );
    }

    #[test]
    fn vector_diff_textconv_and_command() {
        vector(
            |fx| {
                let s = fx.plant_script();
                fx.append_config(&format!(
                    "[diff \"tc\"]\n\ttextconv = {s}\n\tcommand = {s}\n[diff]\n\texternal = {s}\n"
                ));
            },
            Some("*.txt diff=tc\n"),
        );
    }

    #[test]
    fn vector_ssh_command() {
        vector(
            |fx| {
                let s = fx.plant_script();
                fx.append_config(&format!("[core]\n\tsshCommand = {s}\n\taskPass = {s}\n"));
            },
            None,
        );
    }

    #[test]
    fn vector_pager() {
        vector(
            |fx| {
                let s = fx.plant_script();
                fx.append_config(&format!(
                    "[core]\n\tpager = {s}\n[pager]\n\tdiff = {s}\n\tmerge = {s}\n\tstatus = {s}\n\tlog = {s}\n\tbranch = {s}\n"
                ));
            },
            None,
        );
    }

    #[test]
    fn vector_alias() {
        vector(
            |fx| {
                let s = fx.plant_script();
                fx.append_config(&format!(
                    "[alias]\n\tstatus = !{s}\n\tmerge = !{s}\n\tcommit = !{s}\n\tdiff = !{s}\n"
                ));
            },
            None,
        );
    }

    #[test]
    fn vector_credential_helper() {
        vector(
            |fx| {
                let s = fx.plant_script();
                fx.append_config(&format!(
                    "[credential]\n\thelper = {s}\n[credential \"https://example.invalid\"]\n\thelper = {s}\n"
                ));
            },
            None,
        );
    }

    #[test]
    fn vector_gpg_program_and_signing() {
        vector(
            |fx| {
                let s = fx.plant_script();
                fx.append_config(&format!(
                    "[commit]\n\tgpgsign = true\n[tag]\n\tgpgsign = true\n[user]\n\tsigningkey = k\n[gpg]\n\tprogram = {s}\n[gpg \"ssh\"]\n\tprogram = {s}\n[gpg \"custom\"]\n\tprogram = {s}\n"
                ));
            },
            None,
        );
    }

    // ---- isolation -----------------------------------------------------------------------

    #[test]
    fn user_checkout_is_never_touched() {
        need_git!();
        let fx = Fx::new();
        let head_before = git(&fx.repo, &["rev-parse", "HEAD"]);
        let branch_before = git(&fx.repo, &["symbolic-ref", "HEAD"]);
        let readme = fs::read_to_string(fx.repo.join("README.md")).unwrap();
        let lib = fs::read_to_string(fx.repo.join("src/lib.txt")).unwrap();
        exercise(&fx);
        // Repeated calls must not duplicate the exclude entries.
        fx.prepare("r1", "t1");
        fx.prepare("r1", "t9");
        assert_eq!(git(&fx.repo, &["rev-parse", "HEAD"]), head_before);
        assert_eq!(git(&fx.repo, &["symbolic-ref", "HEAD"]), branch_before);
        assert_eq!(git(&fx.repo, &["status", "--porcelain"]), "");
        assert_eq!(
            fs::read_to_string(fx.repo.join("README.md")).unwrap(),
            readme
        );
        assert_eq!(
            fs::read_to_string(fx.repo.join("src/lib.txt")).unwrap(),
            lib
        );
        assert_eq!(
            git(&fx.repo, &["rev-parse", "refs/heads/main"]),
            head_before
        );
        let exclude = fs::read_to_string(fx.repo.join(".git/info/exclude")).unwrap();
        for want in [".fleet-worktrees", ".fleet/"] {
            assert_eq!(
                exclude.lines().filter(|l| *l == want).count(),
                1,
                "{exclude}"
            );
        }
    }

    #[test]
    fn worktrees_live_under_the_repository_worktree_directory() {
        need_git!();
        let fx = Fx::new();
        let wt = fx.prepare("r1", "t1");
        assert_eq!(
            Path::new(&wt),
            fx.repo.join(".fleet-worktrees/r1/t1").as_path()
        );
        assert!(Path::new(&wt).join("README.md").exists());
        assert_eq!(
            git(Path::new(&wt), &["symbolic-ref", "--short", "HEAD"]),
            "fleet/r1/t1"
        );
    }

    #[test]
    fn exclude_file_keeps_existing_lines_and_adds_missing_newline() {
        need_git!();
        let fx = Fx::new();
        fs::create_dir_all(fx.repo.join(".git/info")).unwrap();
        fs::write(fx.repo.join(".git/info/exclude"), "*.log").unwrap();
        fx.prepare("r1", "t1");
        let exclude = fs::read_to_string(fx.repo.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude, "*.log\n.fleet-worktrees\n.fleet/\n");
    }

    // ---- name validation and hostile paths -----------------------------------------------

    #[test]
    fn hostile_base_refs_are_rejected_without_side_effects() {
        need_git!();
        let fx = Fx::new();
        let long = "a".repeat(300);
        for bad in [
            "--upload-pack=x",
            "-x",
            "a..b",
            "a\u{1}b",
            "a\nb",
            "x@{1}",
            "-main",
            "a b",
            "x~1",
            "",
            long.as_str(),
        ] {
            let err = fx.vcs.resolve_base(fx.repo_s(), bad).unwrap_err();
            assert_eq!(err.code, ErrorCode::Invalid, "{bad:?}");
            assert!(
                !err.message.chars().any(|c| c.is_control()),
                "message echoes raw bytes: {:?}",
                err.message
            );
        }
        assert!(!fx.repo.join(".fleet-worktrees").exists());
    }

    #[test]
    fn unknown_base_ref_is_not_found() {
        need_git!();
        let fx = Fx::new();
        let err = fx.vcs.resolve_base(fx.repo_s(), "nope").unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[test]
    fn resolve_base_returns_the_commit() {
        need_git!();
        let fx = Fx::new();
        let want = git(&fx.repo, &["rev-parse", "HEAD"]);
        for r in ["HEAD", "main"] {
            let b = fx.vcs.resolve_base(fx.repo_s(), r).unwrap();
            assert_eq!(b.sha.as_str(), want);
            assert_eq!(b.ref_name, r);
        }
    }

    #[test]
    fn hostile_branch_names_are_rejected_without_side_effects() {
        need_git!();
        let fx = Fx::new();
        let base = fx.base();
        for bad in [
            "-x",
            "../x",
            "a b",
            "main",
            "fleet/../x",
            "fleet/a\u{7}b",
            "--orphan",
        ] {
            let err = fx
                .vcs
                .prepare_worktree(fx.repo_s(), &rid("r1"), &tid("t1"), bad, &base)
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::Invalid, "{bad:?}");
        }
        assert!(!fx.repo.join(".fleet-worktrees").exists());
        assert_eq!(git(&fx.repo, &["branch", "--list"]), "* main");
        assert!(fx.vcs.branch_head(fx.repo_s(), "-x").is_err());
        assert!(fx.vcs.is_merged(fx.repo_s(), "-x", "main").is_err());
        assert!(fx.vcs.is_merged(fx.repo_s(), "main", "--all").is_err());
    }

    #[test]
    fn hostile_integration_branch_names_are_rejected() {
        need_git!();
        let fx = Fx::new();
        for (integration, task) in [
            ("-x", "fleet/r1/t1"),
            ("fleet/r1/integration", "../x"),
            ("main", "fleet/r1/t1"),
        ] {
            let err = fx
                .vcs
                .merge_into_integration(&IntegrationMerge {
                    repo: fx.repo_s().to_string(),
                    run: rid("r1"),
                    task: tid("t1"),
                    base: fx.base(),
                    integration_branch: integration.to_string(),
                    task_branch: task.to_string(),
                    message: "m".to_string(),
                    identity: ident(),
                })
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::Invalid);
        }
        assert!(!fx.repo.join(".fleet-worktrees").exists());
    }

    #[test]
    fn resolve_base_refuses_symlink_subdirectory_linked_worktree_and_non_repos() {
        need_git!();
        let fx = Fx::new();
        let link = fx.tmp.join("link");
        std::os::unix::fs::symlink(&fx.repo, &link).unwrap();
        let err = fx
            .vcs
            .resolve_base(link.to_str().unwrap(), "HEAD")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);

        let sub = fx.repo.join("src");
        assert!(fx.vcs.resolve_base(sub.to_str().unwrap(), "HEAD").is_err());

        let linked = fx.tmp.join("linked");
        git(
            &fx.repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "other",
                linked.to_str().unwrap(),
            ],
        );
        let err = fx
            .vcs
            .resolve_base(linked.to_str().unwrap(), "HEAD")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);

        let plain = fx.tmp.join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert!(fx
            .vcs
            .resolve_base(plain.to_str().unwrap(), "HEAD")
            .is_err());

        let err = fx
            .vcs
            .resolve_base(fx.tmp.join("missing").to_str().unwrap(), "HEAD")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        let file = fx.tmp.write("afile", "x");
        assert!(fx.vcs.resolve_base(file.to_str().unwrap(), "HEAD").is_err());
    }

    #[test]
    fn repository_without_commits_is_refused() {
        need_git!();
        let tmp = TempDir::new("git-empty");
        let repo = tmp.join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        let err = vcs_with(&[])
            .resolve_base(repo.to_str().unwrap(), "HEAD")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);
    }

    #[test]
    fn symlinked_worktree_directory_is_refused() {
        need_git!();
        let fx = Fx::new();
        let outside = fx.tmp.join("outside");
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, fx.repo.join(".fleet-worktrees")).unwrap();
        let err = fx
            .vcs
            .prepare_worktree(
                fx.repo_s(),
                &rid("r1"),
                &tid("t1"),
                "fleet/r1/t1",
                &fx.base(),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn worktree_operations_refuse_directories_that_are_not_fleet_worktrees() {
        need_git!();
        let fx = Fx::new();
        let err = fx.vcs.commit_all(fx.repo_s(), "m", &ident()).unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        let err = fx
            .vcs
            .merge_into_worktree(fx.repo_s(), &["main".to_string()], &ident())
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
    }

    // ---- conflicts -----------------------------------------------------------------------

    #[test]
    fn integration_conflict_aborts_and_later_merges_still_work() {
        need_git!();
        let fx = Fx::new();
        let w1 = fx.prepare("r1", "t1");
        let w2 = fx.prepare("r1", "t2");
        let w3 = fx.prepare("r1", "t3");
        fx.edit_and_commit(&w1, "src/lib.txt", "one\nA\nthree\n");
        fx.edit_and_commit(&w2, "src/lib.txt", "one\nB\nthree\n");
        fx.edit_and_commit(&w3, "other.txt", "independent\n");

        let first = fx.integrate("r1", "t1");
        assert!(first.created);
        let MergeOutcome::Merged { sha: after_first } = first.outcome else {
            panic!("first merge must succeed");
        };
        let integration = fx.repo.join(".fleet-worktrees/r1/_integration");
        assert_eq!(
            git(&integration, &["rev-parse", "HEAD"]),
            after_first.as_str()
        );

        let second = fx.integrate("r1", "t2");
        assert!(!second.created);
        assert_eq!(
            second.outcome,
            MergeOutcome::Conflict {
                files: vec!["src/lib.txt".to_string()]
            }
        );
        assert_eq!(
            git(&integration, &["rev-parse", "HEAD"]),
            after_first.as_str()
        );
        assert_eq!(git(&integration, &["status", "--porcelain"]), "");
        assert!(!merge_head_exists(&integration));
        assert_eq!(
            fs::read_to_string(integration.join("src/lib.txt")).unwrap(),
            "one\nA\nthree\n"
        );

        let third = fx.integrate("r1", "t3");
        assert!(matches!(third.outcome, MergeOutcome::Merged { .. }));
        assert!(integration.join("other.txt").exists());
        // The user's branch never moved.
        assert_eq!(
            git(&fx.repo, &["rev-parse", "refs/heads/main"]),
            fx.base().as_str()
        );
    }

    #[test]
    fn worktree_merge_conflict_leaves_the_worktree_clean() {
        need_git!();
        let fx = Fx::new();
        let w1 = fx.prepare("r1", "t1");
        let w4 = fx.prepare("r1", "t4");
        fx.edit_and_commit(&w1, "src/lib.txt", "one\nA\nthree\n");
        let own = fx.edit_and_commit(&w4, "src/lib.txt", "one\nZ\nthree\n");
        let outcome = fx
            .vcs
            .merge_into_worktree(&w4, &["fleet/r1/t1".to_string()], &ident())
            .unwrap();
        assert_eq!(
            outcome,
            MergeOutcome::Conflict {
                files: vec!["src/lib.txt".to_string()]
            }
        );
        let w4p = Path::new(&w4);
        assert_eq!(git(w4p, &["rev-parse", "HEAD"]), own.as_str());
        assert_eq!(git(w4p, &["status", "--porcelain"]), "");
        assert!(!merge_head_exists(w4p));
    }

    #[test]
    fn worktree_merge_fast_forwards_and_merges_several_branches() {
        need_git!();
        let fx = Fx::new();
        let w1 = fx.prepare("r1", "t1");
        let w2 = fx.prepare("r1", "t2");
        let w5 = fx.prepare("r1", "t5");
        let c1 = fx.edit_and_commit(&w1, "a.txt", "a\n");
        fx.edit_and_commit(&w2, "b.txt", "b\n");
        let ff = fx
            .vcs
            .merge_into_worktree(&w5, &["fleet/r1/t1".to_string()], &ident())
            .unwrap();
        assert_eq!(ff, MergeOutcome::Merged { sha: c1 });
        let both = fx
            .vcs
            .merge_into_worktree(&w5, &["fleet/r1/t2".to_string()], &ident())
            .unwrap();
        let MergeOutcome::Merged { sha } = both else {
            panic!("expected a merge");
        };
        assert_eq!(git(Path::new(&w5), &["rev-parse", "HEAD"]), sha.as_str());
        assert!(Path::new(&w5).join("a.txt").exists() && Path::new(&w5).join("b.txt").exists());
    }

    #[test]
    fn merging_unrelated_history_is_an_error_not_a_conflict() {
        need_git!();
        let fx = Fx::new();
        let w1 = fx.prepare("r1", "t1");
        git(&fx.repo, &["checkout", "-q", "--orphan", "fleet/r1/orphan"]);
        git(&fx.repo, &["rm", "-rfq", "."]);
        fs::write(fx.repo.join("o.txt"), "o\n").unwrap();
        git(&fx.repo, &["add", "-A"]);
        git(&fx.repo, &["commit", "-q", "-m", "orphan"]);
        git(&fx.repo, &["checkout", "-q", "main"]);
        let err = fx
            .vcs
            .merge_into_worktree(&w1, &["fleet/r1/orphan".to_string()], &ident())
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Internal);
        assert_eq!(git(Path::new(&w1), &["status", "--porcelain"]), "");
    }

    // ---- commit_all ----------------------------------------------------------------------

    #[test]
    fn commit_all_reports_nothing_to_commit_then_commits() {
        need_git!();
        let fx = Fx::new();
        let wt = fx.prepare("r1", "t1");
        assert_eq!(fx.vcs.commit_all(&wt, "nothing", &ident()).unwrap(), None);
        fs::write(Path::new(&wt).join("f.txt"), "x\n").unwrap();
        let sha = fx
            .vcs
            .commit_all(&wt, "add f\u{1}\u{7}", &ident())
            .unwrap()
            .unwrap();
        assert_eq!(fx.vcs.branch_head(fx.repo_s(), "fleet/r1/t1").unwrap(), sha);
        let wtp = Path::new(&wt);
        assert_eq!(git(wtp, &["log", "-1", "--format=%s"]), "add f");
        assert_eq!(
            git(wtp, &["log", "-1", "--format=%an <%ae>"]),
            "Fleet Bot <bot@example.invalid>"
        );
        assert_eq!(fx.vcs.commit_all(&wt, "again", &ident()).unwrap(), None);
    }

    #[test]
    fn commit_all_clips_long_messages() {
        need_git!();
        let fx = Fx::new();
        let wt = fx.prepare("r1", "t1");
        fs::write(Path::new(&wt).join("f.txt"), "x\n").unwrap();
        fx.vcs
            .commit_all(&wt, &"m".repeat(100_000), &ident())
            .unwrap()
            .unwrap();
        let body = git(Path::new(&wt), &["log", "-1", "--format=%B"]);
        assert!(body.len() <= MAX_MESSAGE, "{}", body.len());
    }

    #[test]
    fn fleet_directory_is_never_committed() {
        need_git!();
        let fx = Fx::new();
        let wt = fx.prepare("r1", "t1");
        let wtp = Path::new(&wt);
        fs::create_dir_all(wtp.join(".fleet")).unwrap();
        fs::write(wtp.join(".fleet/result.json"), "{}").unwrap();
        assert_eq!(
            fx.vcs
                .commit_all(&wt, "only fleet files", &ident())
                .unwrap(),
            None
        );

        // Even a worker that force-adds it cannot get it into a commit.
        fs::write(wtp.join(".fleet/other.json"), "{}").unwrap();
        git(wtp, &["add", "-f", ".fleet/other.json"]);
        fs::write(wtp.join("real.txt"), "real\n").unwrap();
        fx.vcs.commit_all(&wt, "work", &ident()).unwrap().unwrap();
        let tree = git(wtp, &["ls-tree", "-r", "--name-only", "HEAD"]);
        assert!(tree.lines().any(|l| l == "real.txt"), "{tree}");
        assert!(!tree.lines().any(|l| l.starts_with(".fleet")), "{tree}");
    }

    #[test]
    fn commit_hooks_are_not_run() {
        need_git!();
        let fx = Fx::new();
        let wt = fx.prepare("r1", "t1");
        plant_hooks_in(&fx.repo.join(".git/hooks"), &fx);
        fs::write(Path::new(&wt).join("f.txt"), "x\n").unwrap();
        fx.vcs.commit_all(&wt, "m", &ident()).unwrap().unwrap();
        assert!(!fx.triggered());
    }

    #[test]
    fn invalid_identity_is_rejected() {
        need_git!();
        let fx = Fx::new();
        let wt = fx.prepare("r1", "t1");
        for bad in [
            Identity {
                name: String::new(),
                email: "a@b".to_string(),
            },
            Identity {
                name: "x\ny".to_string(),
                email: "a@b".to_string(),
            },
            Identity {
                name: "x".to_string(),
                email: "<a@b>".to_string(),
            },
        ] {
            let err = fx.vcs.commit_all(&wt, "m", &bad).unwrap_err();
            assert_eq!(err.code, ErrorCode::Invalid);
        }
    }

    // ---- worktree lifecycle --------------------------------------------------------------

    #[test]
    fn prepare_worktree_is_idempotent_and_keeps_uncommitted_work() {
        need_git!();
        let fx = Fx::new();
        let a = fx.prepare("r1", "t1");
        fs::write(Path::new(&a).join("wip.txt"), "wip\n").unwrap();
        let b = fx.prepare("r1", "t1");
        assert_eq!(a, b);
        assert_eq!(
            fs::read_to_string(Path::new(&b).join("wip.txt")).unwrap(),
            "wip\n"
        );
    }

    #[test]
    fn prepare_worktree_reuses_the_branch_after_the_directory_was_removed() {
        need_git!();
        let fx = Fx::new();
        let a = fx.prepare("r1", "t1");
        let sha = fx.edit_and_commit(&a, "kept.txt", "kept\n");
        fs::remove_dir_all(&a).unwrap();
        let b = fx.prepare("r1", "t1");
        assert_eq!(a, b);
        assert_eq!(
            fs::read_to_string(Path::new(&b).join("kept.txt")).unwrap(),
            "kept\n"
        );
        assert_eq!(git(Path::new(&b), &["rev-parse", "HEAD"]), sha.as_str());
    }

    #[test]
    fn prepare_worktree_refuses_a_foreign_directory_at_the_path() {
        need_git!();
        let fx = Fx::new();
        fs::create_dir_all(fx.repo.join(".fleet-worktrees/r1/t1")).unwrap();
        let err = fx
            .vcs
            .prepare_worktree(
                fx.repo_s(),
                &rid("r1"),
                &tid("t1"),
                "fleet/r1/t1",
                &fx.base(),
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
    }

    #[test]
    fn remove_worktree_deletes_directory_and_fleet_branch_only() {
        need_git!();
        let fx = Fx::new();
        let w1 = fx.prepare("r1", "t1");
        fx.vcs
            .remove_worktree(fx.repo_s(), &w1, "fleet/r1/t1", true)
            .unwrap();
        assert!(!Path::new(&w1).exists());
        assert_eq!(git(&fx.repo, &["branch", "--list", "fleet/*"]), "");
        // Idempotent once everything is gone.
        fx.vcs
            .remove_worktree(fx.repo_s(), &w1, "fleet/r1/t1", true)
            .unwrap();

        let w2 = fx.prepare("r1", "t2");
        fx.vcs
            .remove_worktree(fx.repo_s(), &w2, "main", true)
            .unwrap();
        assert!(!Path::new(&w2).exists());
        assert_eq!(
            git(&fx.repo, &["rev-parse", "--verify", "refs/heads/main"]),
            fx.base().as_str()
        );

        let w3 = fx.prepare("r1", "t3");
        fx.vcs
            .remove_worktree(fx.repo_s(), &w3, "fleet/r1/t3", false)
            .unwrap();
        assert_eq!(
            git(&fx.repo, &["branch", "--list", "fleet/r1/t3"]).trim(),
            "fleet/r1/t3"
        );
    }

    #[test]
    fn remove_worktree_refuses_paths_outside_and_symlinks() {
        need_git!();
        let fx = Fx::new();
        let other = fx.tmp.join("other-wt");
        git(
            &fx.repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                other.to_str().unwrap(),
            ],
        );
        let err = fx
            .vcs
            .remove_worktree(fx.repo_s(), other.to_str().unwrap(), "side", true)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        assert!(other.join("README.md").exists());

        let w1 = fx.prepare("r1", "t1");
        let link = fx.repo.join(".fleet-worktrees/r1/link");
        std::os::unix::fs::symlink(&w1, &link).unwrap();
        let err = fx
            .vcs
            .remove_worktree(fx.repo_s(), link.to_str().unwrap(), "fleet/r1/t1", true)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        assert!(Path::new(&w1).join("README.md").exists());

        let sneaky = format!("{}/.fleet-worktrees/../README.md", fx.repo_s());
        assert_eq!(
            fx.vcs
                .remove_worktree(fx.repo_s(), &sneaky, "x", false)
                .unwrap_err()
                .code,
            ErrorCode::Forbidden
        );
        let sneaky = format!("{}/.fleet-worktrees/r1/../../nothing", fx.repo_s());
        assert_eq!(
            fx.vcs
                .remove_worktree(fx.repo_s(), &sneaky, "x", false)
                .unwrap_err()
                .code,
            ErrorCode::Forbidden
        );
        let root = fx.repo.join(".fleet-worktrees");
        assert_eq!(
            fx.vcs
                .remove_worktree(fx.repo_s(), root.to_str().unwrap(), "x", false)
                .unwrap_err()
                .code,
            ErrorCode::Forbidden
        );
        assert!(fx.repo.join("README.md").exists());
        assert!(fx
            .vcs
            .remove_worktree(fx.repo_s(), &w1, "-x", true)
            .is_err());
        assert!(Path::new(&w1).exists());
    }

    #[test]
    fn is_merged_follows_ancestry() {
        need_git!();
        let fx = Fx::new();
        let w1 = fx.prepare("r1", "t1");
        fx.edit_and_commit(&w1, "a.txt", "a\n");
        assert!(!fx
            .vcs
            .is_merged(fx.repo_s(), "fleet/r1/t1", "main")
            .unwrap());
        assert!(fx
            .vcs
            .is_merged(fx.repo_s(), "main", "fleet/r1/t1")
            .unwrap());
        assert!(fx
            .vcs
            .is_merged(fx.repo_s(), "fleet/r1/t1", "fleet/r1/t1")
            .unwrap());
        assert!(fx
            .vcs
            .is_merged(fx.repo_s(), "fleet/r1/missing", "main")
            .is_err());
        assert_eq!(
            fx.vcs
                .branch_head(fx.repo_s(), "fleet/r1/missing")
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
    }

    // ---- process handling ----------------------------------------------------------------

    fn fake_git(tmp: &TempDir, body: &str) -> GitVcs {
        let bin = tmp.join("bin");
        fs::create_dir_all(&bin).unwrap();
        tmp.script("bin/git", &format!("#!/bin/sh\n{body}\n"));
        GitVcs::new(
            vec![("PATH".to_string(), bin.to_str().unwrap().to_string())],
            EnvPolicy::default_policy(),
        )
    }

    fn pid_alive(pid: i32) -> bool {
        nix::sys::signal::kill(Pid::from_raw(pid), None).is_ok()
    }

    #[test]
    fn timeout_kills_git_and_its_children() {
        let tmp = TempDir::new("git-timeout");
        let pids = tmp.join("pids");
        let vcs = fake_git(
            &tmp,
            &format!(
                "echo $$ > '{p}'\n/bin/sleep 60 &\necho $! >> '{p}'\nwait",
                p = pids.display()
            ),
        )
        .with_limits(Duration::from_millis(1500), 1024 * 1024);
        let start = Instant::now();
        let err = vcs.version().unwrap_err();
        assert_eq!(err.code, ErrorCode::Timeout);
        assert!(start.elapsed() < Duration::from_secs(20));
        let text = fs::read_to_string(&pids).unwrap();
        let ids: Vec<i32> = text.lines().filter_map(|l| l.trim().parse().ok()).collect();
        assert_eq!(ids.len(), 2, "{text}");
        for id in ids {
            assert!(
                wait_until(3000, || !pid_alive(id)),
                "process {id} survived the timeout"
            );
        }
    }

    #[test]
    fn output_beyond_the_cap_is_an_error() {
        let tmp = TempDir::new("git-cap");
        let vcs = fake_git(
            &tmp,
            "while :; do echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; done",
        )
        .with_limits(Duration::from_secs(30), 4096);
        let start = Instant::now();
        let err = vcs.version().unwrap_err();
        assert_eq!(err.code, ErrorCode::Limit);
        assert!(start.elapsed() < Duration::from_secs(20));
    }

    #[test]
    fn missing_git_is_unavailable() {
        let tmp = TempDir::new("git-missing");
        let empty = tmp.join("empty");
        fs::create_dir_all(&empty).unwrap();
        let vcs = GitVcs::new(
            vec![("PATH".to_string(), empty.to_str().unwrap().to_string())],
            EnvPolicy::default_policy(),
        );
        assert_eq!(vcs.version().unwrap_err().code, ErrorCode::Unavailable);
    }

    #[test]
    fn git_runs_with_an_empty_environment_plus_the_allowlist() {
        let tmp = TempDir::new("git-env");
        let dump = tmp.join("env.txt");
        fake_git(
            &tmp,
            &format!(
                "/usr/bin/env > '{}'\necho 'git version 2.40.0'",
                dump.display()
            ),
        );
        let mut env = vec![
            (
                "PATH".to_string(),
                tmp.join("bin").to_str().unwrap().to_string(),
            ),
            ("SECRET_TOKEN".to_string(), "hunter2".to_string()),
            ("FLEET_SOCKET".to_string(), "/run/x.sock".to_string()),
            ("GIT_DIR".to_string(), "/elsewhere".to_string()),
        ];
        env.push(("HOME".to_string(), "/home/nobody".to_string()));
        let vcs_env = GitVcs::new(env, EnvPolicy::default_policy());
        assert_eq!(vcs_env.version().unwrap(), "git version 2.40.0");
        let dumped = fs::read_to_string(&dump).unwrap();
        assert!(dumped.contains("GIT_CONFIG_GLOBAL=/dev/null"), "{dumped}");
        assert!(dumped.contains("GIT_TERMINAL_PROMPT=0"), "{dumped}");
        assert!(dumped.contains("LC_ALL=C"), "{dumped}");
        for leaked in ["SECRET_TOKEN", "hunter2", "FLEET_SOCKET", "/elsewhere"] {
            assert!(!dumped.contains(leaked), "{leaked} leaked: {dumped}");
        }
    }

    #[test]
    fn git_version_check() {
        let tmp = TempDir::new("git-version");
        let old = fake_git(&tmp, "echo 'git version 2.30.1'");
        assert_eq!(old.version().unwrap_err().code, ErrorCode::Unavailable);
        let tmp2 = TempDir::new("git-version2");
        let ok = fake_git(&tmp2, "echo 'git version 2.39.5 (Apple Git-154)'");
        assert_eq!(ok.version().unwrap(), "git version 2.39.5 (Apple Git-154)");
        let tmp3 = TempDir::new("git-version3");
        let junk = fake_git(&tmp3, "echo 'not git'");
        assert!(junk.version().is_err());
        if git_available() {
            let env = vec![(
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            )];
            assert!(check_git_version(&env, &EnvPolicy::default_policy())
                .unwrap()
                .starts_with("git version"));
        }
    }

    // ---- dynamic overrides ---------------------------------------------------------------

    #[test]
    fn overrides_cover_user_named_keys_and_odd_names() {
        let list = b"filter.lfs.clean\ngit-lfs clean\0diff.x.textconv\ncat\0pager.log\nless\0credential.https://a.b/.helper\nstore\0merge.m.driver\nx\0gpg.ssh.program\n/bin/x\0filter.a=b.smudge\nx\0alias.s\n!x\0core.bare\nfalse\0Filter.UP.Clean\nx\0bare\0";
        let got: BTreeMap<String, String> = dynamic_overrides(list)
            .unwrap()
            .into_iter()
            .map(|(k, v)| (String::from_utf8(k).unwrap(), String::from_utf8(v).unwrap()))
            .collect();
        for (k, v) in [
            ("filter.lfs.clean", ""),
            ("filter.lfs.smudge", ""),
            ("filter.lfs.process", ""),
            ("filter.lfs.required", "false"),
            ("diff.x.textconv", ""),
            ("diff.x.command", ""),
            ("merge.m.driver", "false"),
            ("pager.log", "cat"),
            ("credential.https://a.b/.helper", ""),
            ("gpg.ssh.program", "false"),
            ("filter.a=b.process", ""),
            ("filter.UP.required", "false"),
        ] {
            assert_eq!(got.get(k).map(String::as_str), Some(v), "{k}");
        }
        assert!(!got
            .keys()
            .any(|k| k.starts_with("alias") || k.starts_with("core")));
    }

    #[test]
    fn overrides_are_bounded() {
        let mut list = Vec::new();
        for i in 0..(MAX_OVERRIDES + 10) {
            list.extend_from_slice(format!("pager.cmd{i}\nx\0").as_bytes());
        }
        assert_eq!(dynamic_overrides(&list).unwrap_err().code, ErrorCode::Limit);
    }

    #[test]
    fn message_cleaning_is_bounded_and_printable() {
        assert_eq!(clean_message("a\u{1}b\nc", "d"), "a b\nc");
        assert_eq!(clean_message("  \u{1}  ", "dflt"), "dflt");
        assert!(clean_message(&"é".repeat(10_000), "d").len() <= MAX_MESSAGE);
    }
}
