//! Git adapter: every VCS operation runs the real `git` binary as a child process.
//!
//! Repositories are untrusted input: whoever can write `.git/config`, `.git/hooks` or
//! `.gitattributes` could make git execute programs. Every invocation therefore starts from an
//! empty environment, pins a fixed set of dangerous settings with `-c`, and additionally
//! neutralizes every config key whose *name* is chosen by the config author (filter, diff, merge
//! driver, pager and credential names) by listing the config first (`git config --list` runs
//! nothing) and overriding those keys through `GIT_CONFIG_COUNT`, which also copes with names that
//! contain `=` or other characters `-c` cannot carry.

use std::collections::{BTreeMap, BTreeSet};
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

use crate::jobs::{JobTracker, Sweeper};
use fleet_app::ports::{
    BaseCommit, Identity, IntegrationMerge, IntegrationResult, MergeOutcome, TreeFile, TreeListing,
    Vcs, WorktreeState,
};
use fleet_domain::env::EnvPolicy;
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::ids::{
    integration_branch, is_invisible, show, task_branch, validate_ref_name, RunId, Sha, TaskId,
};
use fleet_fanout::diff::DiffStat;
use neo_verify::limits::MAX_CHANGED_PATHS;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;

mod push;
mod read;
pub use push::GitPublisher;

/// Hard cap per captured stream of one git call.
const MAX_OUTPUT: usize = 16 * 1024 * 1024;

/// Largest blob `read_blob` reads at all.
const MAX_BLOB_READ: u64 = 1024 * 1024;

/// The regular files of a `git ls-tree -r -z -l` listing (`<mode> <type> <object> <size>\t<path>`
/// records): blobs of mode 100644 and 100755 only, paths that are valid UTF-8 only, at most `max`.
fn parse_tree_listing(raw: &[u8], max: usize) -> Result<TreeListing> {
    let mut listing = TreeListing::default();
    for rec in raw.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let Some(tab) = rec.iter().position(|b| *b == b'\t') else {
            return Err(FleetError::internal(
                "git listed a tree entry without a path",
            ));
        };
        let (meta, path) = rec.split_at(tab);
        let path = path.get(1..).unwrap_or_default();
        let meta = String::from_utf8_lossy(meta);
        let mut fields = meta.split_whitespace();
        let (Some(mode), Some(kind), Some(object), Some(size)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(FleetError::internal("git listed an unreadable tree entry"));
        };
        if kind != "blob" || !(mode == "100644" || mode == "100755") {
            continue;
        }
        let Ok(size) = size.parse::<u64>() else {
            continue;
        };
        let Ok(path) = std::str::from_utf8(path) else {
            continue;
        };
        if listing.files.len() >= max {
            listing.truncated = true;
            break;
        }
        listing.files.push(TreeFile {
            path: path.to_string(),
            size,
            blob: Sha::parse(object)?,
        });
    }
    Ok(listing)
}

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
const STATIC_CONFIG: [(&str, &str); 24] = [
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
    // Replacement objects would let a ref in the shared repository make a commit mean another
    // tree than the one it has: fleet looks at the objects as they are.
    ("core.useReplaceRefs", "false"),
    ("rerere.enabled", "false"),
    ("gpg.program", "false"),
    ("gpg.openpgp.program", "false"),
    ("gpg.x509.program", "false"),
    ("gpg.ssh.program", "false"),
];

const FIXED_ENV: [(&str, &str); 9] = [
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
    ("GIT_OPTIONAL_LOCKS", "0"),
    ("GIT_ATTR_NOSYSTEM", "1"),
    ("GIT_NO_REPLACE_OBJECTS", "1"),
    ("GIT_PAGER", "cat"),
    ("LC_ALL", "C"),
];

type Override = (Vec<u8>, Vec<u8>);

/// How much output of one git call is kept.
#[derive(Clone, Copy)]
struct Cap {
    /// Bytes kept per stream.
    bytes: usize,
    /// More output stops git and returns what fits (flagged), instead of being an error.
    truncate: bool,
}

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

/// A fleet worktree whose `.git` pointer was checked against the repository it belongs to: it
/// points at `<repo>/.git/worktrees/<name>`, that entry points back, and its `commondir` is the
/// repository's own. Git commands that read state which evidence or a merge depends on are run
/// with these paths in the environment (`GIT_DIR`, `GIT_COMMON_DIR`, `GIT_WORK_TREE`), so the
/// pointer file, which the worker can rewrite at any time, is not consulted again.
struct Worktree {
    path: PathBuf,
    repo: PathBuf,
    admin: PathBuf,
    common: PathBuf,
    /// The directory name below `.fleet-worktrees/<run>/`.
    name: String,
    run: String,
    /// The branch the worktree has checked out; `None` when its head is detached.
    branch: Option<String>,
}

impl Worktree {
    /// The branch fleet created for this worktree: `fleet/<run>/<task>`, or the integration
    /// branch for the integration worktree.
    fn expected_branch(&self) -> String {
        if self.name == INTEGRATION_DIR {
            integration_branch(&self.run)
        } else {
            task_branch(&self.run, &self.name)
        }
    }

    /// The environment that pins git to this worktree and its repository.
    fn env(&self) -> [(&'static str, &OsStr); 3] {
        [
            ("GIT_DIR", self.admin.as_os_str()),
            ("GIT_COMMON_DIR", self.common.as_os_str()),
            ("GIT_WORK_TREE", self.path.as_os_str()),
        ]
    }
}

/// Most bytes read from the small pointer files of a worktree.
const MAX_POINTER_FILE: u64 = 4096;

/// Files that make objects come from somewhere else than the repository's own store, or history
/// differ from what its commits say. With any of them fleet cannot tell what a commit contains.
const FOREIGN_OBJECT_SOURCES: [(&str, &str); 3] = [
    ("objects/info/alternates", "alternates"),
    ("objects/info/http-alternates", "http-alternates"),
    ("info/grafts", "grafts"),
];

fn pointer_error(what: &str) -> FleetError {
    FleetError::forbidden(format!(
        "the .git file of this worktree {what}; it must point at its own entry in the repository (the worktree was changed, or is not a fleet worktree)"
    ))
}

/// A bounded, regular, UTF-8 file of the git administration, without its trailing line break.
fn read_pointer_file(path: &Path, what: &str) -> Result<String> {
    let bytes = crate::fsx::read_regular(path, MAX_POINTER_FILE).map_err(|_| {
        pointer_error(&format!(
            "has {what} that is missing, linked or not a plain file"
        ))
    })?;
    let text = String::from_utf8(bytes)
        .map_err(|_| pointer_error(&format!("has {what} that is not text")))?;
    Ok(text.strip_suffix('\n').unwrap_or(&text).to_string())
}

pub struct GitVcs {
    env: Vec<(String, String)>,
    timeout: Duration,
    max_output: usize,
    /// Where the process groups of git's children are listed while they run (see `JobTracker`).
    tracker: JobTracker,
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
            tracker: JobTracker::new(),
        }
    }

    /// Lists the children of this adapter in the daemon's shared tracker, so the origin checks
    /// refuse them (and what they start) like the children of every other job.
    pub fn with_tracker(mut self, tracker: JobTracker) -> GitVcs {
        self.tracker = tracker;
        self
    }

    /// How long one git call may run. The adapter used for reading repositories for the dashboard
    /// gives itself less than the 120 seconds that merges and commits may take.
    pub fn with_timeout(mut self, timeout: Duration) -> GitVcs {
        self.timeout = timeout;
        self
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
        self.run_raw_env(cwd, ident, overrides, &[], args)
    }

    /// One run of git whose output is capped at `cap.bytes` per stream: more is an error, or
    /// (when `cap.truncate`) git is stopped and the part that fits is returned, flagged.
    fn run_raw_capped(
        &self,
        cwd: &Path,
        ident: Option<&Identity>,
        overrides: &[Override],
        extra_env: &[(&str, &OsStr)],
        args: &[&str],
        cap: Cap,
    ) -> Result<(Out, bool)> {
        let mut cmd = Command::new(self.find_git()?);
        cmd.env_clear();
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        for (k, v) in extra_env {
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

        // A script that was just written can still be open for writing in a concurrent fork
        // (ETXTBSY); starting is retried briefly, as for every other child the daemon starts.
        let mut child = crate::jobs::spawn_retrying_busy(&mut cmd)
            .map_err(|e| FleetError::io("cannot start git", &e))?;
        let _group = self.tracker.track(child.id());
        let mut sweeper = Sweeper::new(&self.tracker, child.id());
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
            sweeper.kill_all();
            return Err(FleetError::internal("git pipes are missing"));
        };
        let cap_bytes = cap.bytes;
        let spawn_reader = |is_out: bool, pipe: Box<dyn Read + Send>| {
            let tx = tx.clone();
            let over = Arc::clone(&over);
            thread::Builder::new()
                .name("fleet-git-pipe".to_string())
                .spawn(move || {
                    let buf = read_capped(pipe, cap_bytes, &over);
                    let _ = tx.send((is_out, buf));
                })
        };
        if let Err(e) =
            spawn_reader(true, Box::new(so)).and_then(|_| spawn_reader(false, Box::new(se)))
        {
            kill_group(&mut child, pgid);
            sweeper.kill_all();
            return Err(FleetError::io("cannot start a reader thread", &e));
        }
        drop(tx);

        let start = Instant::now();
        let mut delay = Duration::from_millis(1);
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) => {}
                Err(e) => {
                    kill_group(&mut child, pgid);
                    sweeper.kill_all();
                    return Err(FleetError::io("cannot wait for git", &e));
                }
            }
            if over.load(Ordering::SeqCst) {
                kill_group(&mut child, pgid);
                sweeper.kill_all();
                if !cap.truncate {
                    return Err(FleetError::limit(format!(
                        "git produced more than {} bytes of output",
                        cap.bytes
                    )));
                }
                // the caller wants what fits: git is stopped and the prefix is returned
                break None;
            }
            if start.elapsed() >= self.timeout {
                kill_group(&mut child, pgid);
                sweeper.kill_all();
                return Err(FleetError::new(
                    ErrorCode::Timeout,
                    format!("git did not finish within {} s", self.timeout.as_secs()),
                ));
            }
            sweeper.sample(false);
            thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_millis(20));
        };
        // Anything git left behind in its group, or that left the group, must not outlive the call.
        let _ = killpg(pgid, Signal::SIGKILL);
        sweeper.kill_all();

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
        let truncated = over.load(Ordering::SeqCst);
        if truncated && !cap.truncate {
            return Err(FleetError::limit(format!(
                "git produced more than {} bytes of output",
                cap.bytes
            )));
        }
        Ok((
            Out {
                code: status.and_then(|s| s.code()),
                stdout,
                stderr,
            },
            truncated,
        ))
    }

    /// [`GitVcs::run_raw`] with extra environment variables for this one call.
    fn run_raw_env(
        &self,
        cwd: &Path,
        ident: Option<&Identity>,
        overrides: &[Override],
        extra_env: &[(&str, &OsStr)],
        args: &[&str],
    ) -> Result<Out> {
        let cap = Cap {
            bytes: self.max_output,
            truncate: false,
        };
        self.run_raw_capped(cwd, ident, overrides, extra_env, args, cap)
            .map(|(out, _)| out)
    }

    /// Runs git with the config of `cwd` neutralized (see the module comment).
    fn run(&self, cwd: &Path, ident: Option<&Identity>, args: &[&str]) -> Result<Out> {
        self.run_env(cwd, ident, &[], args)
    }

    fn run_env(
        &self,
        cwd: &Path,
        ident: Option<&Identity>,
        extra_env: &[(&str, &OsStr)],
        args: &[&str],
    ) -> Result<Out> {
        let listing = self.run_raw_env(cwd, None, &[], extra_env, &["config", "--list", "-z"])?;
        let overrides = if listing.ok() {
            dynamic_overrides(&listing.stdout)?
        } else {
            Vec::new()
        };
        self.run_raw_env(cwd, ident, &overrides, extra_env, args)
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
        let common = self.canonical_line(common)?;
        if self.canonical_line(git_dir)? != common {
            return Err(FleetError::forbidden(format!(
                "{} is a linked worktree, not a main repository",
                show(repo)
            )));
        }
        self.guard_object_sources(&common)?;
        Ok(dir)
    }

    /// Refuses a repository whose objects can come from elsewhere (alternates) or whose history
    /// is rewritten by grafts: evidence names a commit and a tree, and both must mean what the
    /// repository's own store says.
    fn guard_object_sources(&self, common: &Path) -> Result<()> {
        for (rel, what) in FOREIGN_OBJECT_SOURCES {
            if fs::symlink_metadata(common.join(rel)).is_ok() {
                return Err(FleetError::forbidden(format!(
                    "the repository uses {what} ({}): fleet cannot tell what a commit contains when objects come from elsewhere or history is rewritten; run `git repack -a -d` and remove the file",
                    show_path(&common.join(rel))
                )));
            }
        }
        Ok(())
    }

    /// A worktree fleet manages: `<repo>/.fleet-worktrees/<run>/<name>`, its own top level, and
    /// with a `.git` pointer that is what git wrote when fleet created it (see [`Worktree`]).
    /// Anything else is refused, with a message that names what is wrong.
    fn check_worktree(&self, worktree: &str) -> Result<Worktree> {
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
        let not_ours =
            || FleetError::forbidden(format!("{} is not a fleet worktree", show(worktree)));
        let canon_for_parts = canon.clone();
        let parts: Vec<&OsStr> = canon_for_parts
            .components()
            .map(|c| c.as_os_str())
            .collect();
        let at = parts
            .iter()
            .rposition(|c| *c == OsStr::new(WORKTREES))
            .ok_or_else(not_ours)?;
        let (Some(run), Some(name), None) =
            (parts.get(at + 1), parts.get(at + 2), parts.get(at + 3))
        else {
            return Err(not_ours());
        };
        let (Some(run), Some(name)) = (run.to_str(), name.to_str()) else {
            return Err(not_ours());
        };
        let repo_path: PathBuf = parts.iter().take(at).collect();
        let repo = self.open_repo(path_str(&repo_path)?)?;
        let common = {
            let out = self.run_ok(
                &repo,
                None,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )?;
            self.canonical_line(&out.text())?
        };

        // The pointer: a plain file saying `gitdir: <repo>/.git/worktrees/<entry>`, exactly.
        let text = read_pointer_file(&canon.join(".git"), "a `.git` entry")?;
        let target = text
            .strip_prefix("gitdir: ")
            .filter(|t| !t.is_empty() && !t.contains('\n') && !t.contains('\0'))
            .ok_or_else(|| pointer_error("is not a `gitdir:` line"))?;
        let target = Path::new(target);
        if !target.is_absolute() {
            return Err(pointer_error("is not an absolute path"));
        }
        let admin = fs::canonicalize(target)
            .map_err(|_| pointer_error("names an entry that does not exist"))?;
        if admin != target {
            return Err(pointer_error("goes through a link or a relative component"));
        }
        if admin.parent() != Some(common.join("worktrees").as_path())
            || admin.file_name().is_none()
            || !crate::fsx::is_real_dir(&admin)
        {
            return Err(pointer_error(
                "does not name an entry below this repository's `.git/worktrees`",
            ));
        }
        // The entry must belong to this worktree and to this repository.
        let back = read_pointer_file(&admin.join("gitdir"), "its entry's `gitdir`")?;
        if fs::canonicalize(&back).ok().as_deref() != Some(canon.join(".git").as_path()) {
            return Err(pointer_error(
                "names an entry that belongs to another worktree",
            ));
        }
        let commondir = read_pointer_file(&admin.join("commondir"), "its entry's `commondir`")?;
        if fs::canonicalize(admin.join(&commondir)).ok().as_deref() != Some(common.as_path()) {
            return Err(pointer_error(
                "names an entry that belongs to another repository",
            ));
        }
        let head = read_pointer_file(&admin.join("HEAD"), "its entry's `HEAD`")?;
        let branch = match head.strip_prefix("ref: ") {
            Some(r) => Some(
                r.strip_prefix("refs/heads/")
                    .filter(|b| validate_ref_name(b).is_ok())
                    .ok_or_else(|| pointer_error("is on something other than a branch"))?
                    .to_string(),
            ),
            None => None,
        };
        // What git itself makes of it must agree.
        let dirs = self.run_env_ok(
            &canon,
            &[
                ("GIT_DIR", admin.as_os_str()),
                ("GIT_COMMON_DIR", common.as_os_str()),
                ("GIT_WORK_TREE", canon.as_os_str()),
            ],
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-dir",
                "--git-common-dir",
                "--show-toplevel",
            ],
        )?;
        let text = dirs.text();
        let mut lines = text.lines();
        let (Some(a), Some(c), Some(t)) = (lines.next(), lines.next(), lines.next()) else {
            return Err(FleetError::internal("unexpected output from git rev-parse"));
        };
        if fs::canonicalize(t).ok().as_deref() != Some(canon.as_path()) {
            return Err(FleetError::forbidden(format!(
                "{} is not the top level of a worktree",
                show(worktree)
            )));
        }
        if fs::canonicalize(a).ok().as_deref() != Some(admin.as_path())
            || fs::canonicalize(c).ok().as_deref() != Some(common.as_path())
        {
            return Err(pointer_error("is not understood the way fleet read it"));
        }
        Ok(Worktree {
            path: canon,
            repo,
            admin,
            common,
            name: name.to_string(),
            run: run.to_string(),
            branch,
        })
    }

    /// The commit `branch` points at in the repository itself, ignoring replacement objects.
    fn real_branch_head(&self, repo: &Path, branch: &str) -> Result<Sha> {
        validate_ref_name(branch)?;
        let spec = format!("refs/heads/{branch}^{{commit}}");
        let out = self.run(
            repo,
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

    /// The tree of a commit in the repository itself.
    fn real_tree_of(&self, repo: &Path, commit: &Sha) -> Result<Sha> {
        let spec = format!("{}^{{tree}}", commit.as_str());
        let out = self.run_ok(
            repo,
            None,
            &["rev-parse", "--verify", "--end-of-options", &spec],
        )?;
        Sha::parse(out.text().trim())
    }

    /// The worktree's branch, which must be the one fleet made for it: commits made on any other
    /// branch are not the work that evidence and merges are about.
    fn task_branch_of(&self, w: &Worktree) -> Result<String> {
        let expected = w.expected_branch();
        match &w.branch {
            Some(b) if *b == expected => Ok(expected),
            Some(b) => Err(FleetError::forbidden(format!(
                "the worktree is on branch {} instead of {}; its work is not on the branch that is merged",
                show(b),
                show(&expected)
            ))),
            None => Err(FleetError::forbidden(format!(
                "the worktree has a detached head instead of branch {}; its work is not on the branch that is merged",
                show(&expected)
            ))),
        }
    }

    /// Runs git in the worktree with the pinned directories of `w`.
    fn run_in(&self, w: &Worktree, ident: Option<&Identity>, args: &[&str]) -> Result<Out> {
        self.run_env(&w.path, ident, &w.env(), args)
    }

    fn run_in_ok(&self, w: &Worktree, ident: Option<&Identity>, args: &[&str]) -> Result<Out> {
        let out = self.run_in(w, ident, args)?;
        if out.ok() {
            return Ok(out);
        }
        Err(self.failure(args, &out))
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

    /// Merges `branch`. With `approved`, the branch must still point at that commit (it is what a
    /// human approved) and exactly that commit is merged, not whatever the name points to by the
    /// time git resolves it.
    fn merge_one(
        &self,
        wt: &Path,
        ident: &Identity,
        ff_flag: &str,
        message: Option<&str>,
        branch: &str,
        approved: Option<&Sha>,
    ) -> Result<MergeOutcome> {
        let full = format!("refs/heads/{branch}");
        let target = match approved {
            Some(sha) => {
                let spec = format!("{full}^{{commit}}");
                let out = self.run(
                    wt,
                    None,
                    &["rev-parse", "--verify", "--end-of-options", &spec],
                )?;
                if !out.ok() || out.text().trim() != sha.as_str() {
                    return Err(FleetError::conflict(format!(
                        "the branch {} moved since it was approved; it no longer points at {}",
                        show(branch),
                        show(sha.as_str())
                    )));
                }
                sha.as_str().to_string()
            }
            None => full,
        };
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
        args.push(&target);
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

/// What exporting a commit left behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Export {
    /// The tree object of the exported commit.
    pub tree: Sha,
    /// Symlinks of the commit that were removed because their target is not a plain descent
    /// below their own directory (absolute, or containing `..`), so that nothing in the export
    /// can point outside it.
    pub removed_links: Vec<String>,
}

/// Most entries examined when an export is confined.
const MAX_EXPORT_ENTRIES: usize = 500_000;

impl GitVcs {
    /// Writes the files of `commit` (a commit of the repository `worktree` belongs to) into the
    /// empty directory `dest`, using the temporary index file `index`. Nothing of the worktree is
    /// read: not its files, its index, its ignored or untracked files, nor its local state. Git
    /// runs hardened like everywhere else (no hooks, no filters that are not neutralized, no
    /// attributes file). The repository's own index and files are not touched.
    ///
    /// Symlinks whose target is absolute or contains `..` are removed from the export (and
    /// reported): a link can only point further down than where it lies, so nothing can reach
    /// the operator's files through the export.
    pub fn export_commit(
        &self,
        worktree: &str,
        commit: &Sha,
        dest: &Path,
        index: &Path,
    ) -> Result<Export> {
        let w = self.check_worktree(worktree)?;
        // The commit and its tree are read from the repository, not through the worktree.
        let spec_commit = format!("{}^{{commit}}", commit.as_str());
        let resolved = self.run(
            &w.repo,
            None,
            &["rev-parse", "--verify", "--end-of-options", &spec_commit],
        )?;
        if !resolved.ok() || resolved.text().trim() != commit.as_str() {
            return Err(FleetError::not_found(format!(
                "commit {} does not exist",
                commit.as_str()
            )));
        }
        let tree = self.real_tree_of(&w.repo, commit)?;
        if !crate::fsx::is_real_dir(dest) {
            return Err(FleetError::invalid("the export directory does not exist"));
        }
        if fs::read_dir(dest)
            .map_err(|e| FleetError::io("cannot read the export directory", &e))?
            .next()
            .is_some()
        {
            return Err(FleetError::invalid("the export directory is not empty"));
        }
        let prefix = {
            let mut p = dest.as_os_str().to_os_string();
            p.push("/");
            p
        };
        let prefix_text = prefix
            .to_str()
            .ok_or_else(|| FleetError::invalid("the export directory is not valid UTF-8"))?
            .to_string();
        let pinned = w.env();
        let env = [
            pinned[0],
            pinned[1],
            pinned[2],
            ("GIT_INDEX_FILE", index.as_os_str()),
        ];
        self.run_env_ok(
            &w.path,
            &env,
            &[
                "-c",
                "core.sparseCheckout=false",
                "read-tree",
                "--end-of-options",
                commit.as_str(),
            ],
        )?;
        // What was read into the index is the tree evidence will name.
        let written = self.run_env_ok(&w.path, &env, &["write-tree"])?;
        if written.text().trim() != tree.as_str() {
            return Err(FleetError::internal(
                "the tree that was read for the export is not the tree of the commit",
            ));
        }
        self.run_env_ok(
            &w.path,
            &env,
            &[
                "-c",
                "core.sparseCheckout=false",
                "checkout-index",
                "--all",
                &format!("--prefix={prefix_text}"),
            ],
        )?;
        let _ = fs::remove_file(index);
        let removed_links = confine_links(dest)?;
        Ok(Export {
            tree,
            removed_links,
        })
    }

    fn run_env_ok(&self, cwd: &Path, env: &[(&str, &OsStr)], args: &[&str]) -> Result<Out> {
        let out = self.run_env(cwd, None, env, args)?;
        if out.ok() {
            return Ok(out);
        }
        Err(self.failure(args, &out))
    }
}

/// Removes every symlink below `root` whose target is not a plain descent (only normal path
/// components) and returns their paths relative to `root`. Symlinks are never followed.
fn confine_links(root: &Path) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    let mut seen = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir)
            .map_err(|e| FleetError::io("cannot read the exported directory", &e))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| FleetError::io("cannot read the exported directory", &e))?;
            seen += 1;
            if seen > MAX_EXPORT_ENTRIES {
                return Err(FleetError::limit(format!(
                    "the commit has more than {MAX_EXPORT_ENTRIES} files"
                )));
            }
            let path = entry.path();
            let ft = entry
                .file_type()
                .map_err(|e| FleetError::io("cannot examine an exported file", &e))?;
            if ft.is_symlink() {
                let target = fs::read_link(&path)
                    .map_err(|e| FleetError::io("cannot read an exported symlink", &e))?;
                let plain = target
                    .components()
                    .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
                if !plain || target.as_os_str().is_empty() {
                    fs::remove_file(&path)
                        .map_err(|e| FleetError::io("cannot remove an exported symlink", &e))?;
                    let rel = path.strip_prefix(root).unwrap_or(&path);
                    removed.push(safe_text(&rel.to_string_lossy(), 200));
                }
            } else if ft.is_dir() {
                stack.push(path);
            }
        }
    }
    removed.sort();
    Ok(removed)
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
        let w = self.check_worktree(worktree)?;
        let wt = w.path.clone();
        for b in branches {
            if let conflict @ MergeOutcome::Conflict { .. } =
                self.merge_one(&wt, identity, "--ff", None, b, None)?
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
        let w = self.check_worktree(worktree)?;
        let branch = self.task_branch_of(&w)?;
        let message = clean_message(message, "fleet: task changes");
        self.run_in_ok(&w, None, &["add", "-A"])?;
        // Fleet's own directory must never reach a commit, even when something force-added it.
        self.run_in_ok(&w, None, &["reset", "-q", "--", ".fleet"])?;
        match self
            .run_in(&w, None, &["diff", "--cached", "--quiet", "--no-ext-diff"])?
            .code
        {
            Some(0) => return Ok(None),
            Some(1) => {}
            _ => {
                return Err(FleetError::internal("git diff --cached failed"));
            }
        }
        self.run_in_ok(
            &w,
            Some(identity),
            &["commit", "--no-verify", "--no-gpg-sign", "-m", &message],
        )?;
        // The commit that was made, as the repository itself has it on the task's branch.
        Ok(Some(self.real_branch_head(&w.repo, &branch)?))
    }

    fn branch_head(&self, repo: &str, branch: &str) -> Result<Sha> {
        validate_ref_name(branch)?;
        let dir = self.open_repo(repo)?;
        self.real_branch_head(&dir, branch)
    }

    fn commit_tree(&self, repo: &str, commit: &Sha) -> Result<Sha> {
        let dir = self.open_repo(repo)?;
        self.real_tree_of(&dir, commit)
    }

    fn tree_files(&self, repo: &str, commit: &Sha, max: usize) -> Result<TreeListing> {
        let dir = self.open_repo(repo)?;
        let out = self.run_ok(
            &dir,
            None,
            &["ls-tree", "-r", "-z", "-l", "--full-tree", commit.as_str()],
        )?;
        parse_tree_listing(&out.stdout, max)
    }

    fn read_blob(&self, repo: &str, blob: &Sha, max: usize) -> Result<Vec<u8>> {
        let dir = self.open_repo(repo)?;
        let size = self.run(&dir, None, &["cat-file", "-s", blob.as_str()])?;
        if !size.ok() {
            return Err(FleetError::not_found(format!(
                "object {} does not exist in the repository",
                blob.as_str()
            )));
        }
        let size: u64 = size
            .text()
            .trim()
            .parse()
            .map_err(|_| FleetError::internal("git reported an unreadable object size"))?;
        if size > MAX_BLOB_READ {
            return Err(FleetError::limit(format!(
                "object {} is larger than {MAX_BLOB_READ} bytes",
                blob.as_str()
            )));
        }
        let out = self.run(&dir, None, &["cat-file", "blob", blob.as_str()])?;
        if !out.ok() {
            return Err(FleetError::not_found(format!(
                "blob {} does not exist in the repository",
                blob.as_str()
            )));
        }
        let mut bytes = out.stdout;
        bytes.truncate(max);
        Ok(bytes)
    }

    fn changed_paths(
        &self,
        repo: &str,
        base: &Sha,
        head: &Sha,
        max: usize,
    ) -> Result<(Vec<String>, bool)> {
        let dir = self.open_repo(repo)?;
        let diff = self.run_ok(
            &dir,
            None,
            &[
                "diff",
                "--name-only",
                "-z",
                "--no-ext-diff",
                "--no-renames",
                base.as_str(),
                head.as_str(),
                "--",
            ],
        )?;
        let mut paths: BTreeSet<String> = BTreeSet::new();
        for raw in diff.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            paths.insert(String::from_utf8_lossy(raw).into_owned());
        }
        let truncated = paths.len() > max;
        Ok((paths.into_iter().take(max).collect(), truncated))
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
            Some(&req.head),
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
        // An empty path means the worktree is already gone (only the branch is left to delete).
        if !worktree.is_empty() {
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

    fn worktree_state(&self, worktree: &str, base: &Sha) -> Result<WorktreeState> {
        let w = self.check_worktree(worktree)?;
        // Commit and tree come from the repository's branch, never from what the worktree's own
        // git directory says: this is what a merge brings in.
        let branch = self.task_branch_of(&w)?;
        let head = self.real_branch_head(&w.repo, &branch)?;
        let tree = self.real_tree_of(&w.repo, &head)?;
        // What the commits changed since the base: this, not the working state, is what a merge
        // brings in. Every path is listed, `.fleet/` ones included.
        let diff = self.run_ok(
            &w.repo,
            None,
            &[
                "diff",
                "--name-only",
                "-z",
                "--no-ext-diff",
                "--no-renames",
                base.as_str(),
                head.as_str(),
                "--",
            ],
        )?;
        let mut paths: BTreeSet<String> = BTreeSet::new();
        for raw in diff.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            paths.insert(String::from_utf8_lossy(raw).into_owned());
        }
        let truncated = paths.len() > MAX_CHANGED_PATHS;
        // Uncommitted work: staged, modified or untracked files (ignored ones are not work and
        // are not in any commit), apart from fleet's own directory.
        let status = self.run_in_ok(
            &w,
            None,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--no-renames",
                "--untracked-files=normal",
                "--ignore-submodules=none",
            ],
        )?;
        let dirty = status
            .stdout
            .split(|b| *b == 0)
            .filter(|entry| entry.len() > 3)
            .any(|entry| {
                let path = entry.get(3..).unwrap_or_default();
                path != b".fleet" && !path.starts_with(b".fleet/")
            });
        Ok(WorktreeState {
            head,
            tree,
            changed: paths.into_iter().take(MAX_CHANGED_PATHS).collect(),
            truncated,
            dirty,
        })
    }

    fn diff_summary(&self, repo: &str, base: &Sha, head: &Sha) -> Result<DiffStat> {
        // Asked of the repository, like every other fact a decision rests on: the objects, not
        // what a worktree's own git directory says. Renames are detected (a renamed file is one
        // change that touches two paths); external diff drivers and text conversion are off.
        let dir = self.open_repo(repo)?;
        let run = |mode: &str| {
            self.run_ok(
                &dir,
                None,
                &[
                    "diff",
                    mode,
                    "-z",
                    "-M",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    base.as_str(),
                    head.as_str(),
                    "--",
                ],
            )
        };
        let names = run("--name-status")?;
        let counts = run("--numstat")?;
        crate::git_diff::parse(&names.stdout, &counts.stdout)
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
            let head = self
                .vcs
                .branch_head(self.repo_s(), &format!("fleet/{run}/{task}"))
                .unwrap();
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
                    head,
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

    // ---- worktree state --------------------------------------------------------------------

    #[test]
    fn worktree_state_lists_what_the_commits_changed_and_whether_work_is_uncommitted() {
        need_git!();
        let fx = Fx::new();
        let base = fx.base();
        let wt = fx.prepare("r1", "t1");
        // nothing yet
        let s = fx.vcs.worktree_state(&wt, &base).unwrap();
        assert_eq!(s.head, base);
        assert!(s.changed.is_empty() && !s.truncated && !s.dirty, "{s:?}");
        assert_eq!(
            s.tree.as_str(),
            git(Path::new(&wt), &["rev-parse", "HEAD^{tree}"])
        );
        // committed work: the paths of the commits, nothing else
        let head = fx.edit_and_commit(&wt, "src/committed.txt", "a\n");
        fx.edit_and_commit(&wt, "docs/new.md", "b\n");
        let s = fx.vcs.worktree_state(&wt, &base).unwrap();
        assert_eq!(s.changed, ["docs/new.md", "src/committed.txt"]);
        assert!(!s.dirty);
        assert_ne!(s.head, head);
        // uncommitted work is not a change of the commit, but makes the worktree dirty
        fs::write(Path::new(&wt).join("README.md"), "changed\n").unwrap();
        let s = fx.vcs.worktree_state(&wt, &base).unwrap();
        assert_eq!(s.changed, ["docs/new.md", "src/committed.txt"]);
        assert!(s.dirty, "a modified tracked file");
        git(Path::new(&wt), &["checkout", "--", "README.md"]);
        fs::write(Path::new(&wt).join("untracked.txt"), "u\n").unwrap();
        assert!(
            fx.vcs.worktree_state(&wt, &base).unwrap().dirty,
            "an untracked file"
        );
        fs::remove_file(Path::new(&wt).join("untracked.txt")).unwrap();
        fs::write(Path::new(&wt).join("staged.txt"), "s\n").unwrap();
        git(Path::new(&wt), &["add", "staged.txt"]);
        assert!(
            fx.vcs.worktree_state(&wt, &base).unwrap().dirty,
            "a staged file"
        );
        git(Path::new(&wt), &["reset", "-q", "--", "staged.txt"]);
        fs::remove_file(Path::new(&wt).join("staged.txt")).unwrap();
        assert!(!fx.vcs.worktree_state(&wt, &base).unwrap().dirty);
    }

    #[test]
    fn fleets_own_directory_and_ignored_files_do_not_make_a_worktree_dirty() {
        need_git!();
        let fx = Fx::new();
        let base = fx.base();
        let wt = fx.prepare("r1", "t1");
        fx.edit_and_commit(&wt, ".gitignore", "ignored.log\n");
        fs::create_dir_all(Path::new(&wt).join(".fleet")).unwrap();
        fs::write(Path::new(&wt).join(".fleet/brief.md"), "brief\n").unwrap();
        fs::write(Path::new(&wt).join("ignored.log"), "x").unwrap();
        let s = fx.vcs.worktree_state(&wt, &base).unwrap();
        assert!(!s.dirty, "{s:?}");
        assert_eq!(s.changed, [".gitignore"]);
    }

    #[test]
    fn a_path_under_the_fleet_directory_that_the_worker_committed_is_a_change() {
        need_git!();
        let fx = Fx::new();
        let base = fx.base();
        let wt = fx.prepare("r1", "t1");
        fs::create_dir_all(Path::new(&wt).join(".fleet")).unwrap();
        fs::write(Path::new(&wt).join(".fleet/planted.sh"), "echo hi\n").unwrap();
        fs::write(Path::new(&wt).join("a.txt"), "a\n").unwrap();
        git(Path::new(&wt), &["add", "-f", ".fleet/planted.sh", "a.txt"]);
        git(Path::new(&wt), &["commit", "-q", "-m", "worker commit"]);
        let s = fx.vcs.worktree_state(&wt, &base).unwrap();
        assert_eq!(s.changed, [".fleet/planted.sh", "a.txt"]);
        assert!(!s.dirty);
    }

    #[test]
    fn file_names_are_reported_verbatim() {
        need_git!();
        let fx = Fx::new();
        let base = fx.base();
        let wt = fx.prepare("r1", "t1");
        fx.edit_and_commit(&wt, "odd name\twith tab.txt", "x");
        fx.edit_and_commit(&wt, "naïve.txt", "x");
        let s = fx.vcs.worktree_state(&wt, &base).unwrap();
        assert_eq!(s.changed, ["naïve.txt", "odd name\twith tab.txt"]);
    }

    fn head_of(fx: &Fx) -> Sha {
        Sha::parse(&git(&fx.repo, &["rev-parse", "HEAD"])).unwrap()
    }

    fn commit_files(fx: &Fx, files: &[(&str, &str)]) -> Sha {
        for (name, content) in files {
            let p = fx.repo.join(name);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(p, content).unwrap();
        }
        git(&fx.repo, &["add", "-A"]);
        git(&fx.repo, &["commit", "-q", "-m", "more"]);
        head_of(fx)
    }

    #[test]
    fn tree_files_lists_regular_files_sorted_with_sizes_and_blobs() {
        need_git!();
        let fx = Fx::new();
        let head = commit_files(&fx, &[("b/run.sh", "#!/bin/sh\n"), ("a.txt", "hello")]);
        let list = fx.vcs.tree_files(fx.repo_s(), &head, 100).unwrap();
        assert!(!list.truncated);
        let paths: Vec<&str> = list.files.iter().map(|f| f.path.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        assert_eq!(paths, sorted);
        assert!(paths.contains(&"README.md") && paths.contains(&"src/lib.txt"));
        let a = list.files.iter().find(|f| f.path == "a.txt").unwrap();
        assert_eq!(a.size, 5);
        assert_eq!(a.blob.as_str(), git(&fx.repo, &["rev-parse", "HEAD:a.txt"]));
        let run = list.files.iter().find(|f| f.path == "b/run.sh").unwrap();
        assert_eq!(run.size, 10);
    }

    #[test]
    fn tree_files_lists_executables_and_skips_symlinks_and_gitlinks() {
        need_git!();
        let fx = Fx::new();
        std::os::unix::fs::symlink("README.md", fx.repo.join("link")).unwrap();
        let script = fx.repo.join("tool.sh");
        fs::write(&script, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        git(&fx.repo, &["add", "-A"]);
        let gitlink = format!("160000,{},sub", "1".repeat(40));
        git(
            &fx.repo,
            &["update-index", "--add", "--cacheinfo", &gitlink],
        );
        git(&fx.repo, &["commit", "-q", "-m", "special"]);
        let head = head_of(&fx);
        let list = fx.vcs.tree_files(fx.repo_s(), &head, 100).unwrap();
        let paths: Vec<&str> = list.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"tool.sh"), "{paths:?}");
        assert!(!paths.contains(&"link"), "{paths:?}");
        assert!(!paths.contains(&"sub"), "{paths:?}");
    }

    #[test]
    fn tree_files_reports_odd_names_verbatim() {
        need_git!();
        let fx = Fx::new();
        let head = commit_files(
            &fx,
            &[
                ("with space.txt", "x"),
                ("new\nline.txt", "x"),
                ("naïve/日本.txt", "x"),
            ],
        );
        let list = fx.vcs.tree_files(fx.repo_s(), &head, 100).unwrap();
        let paths: Vec<&str> = list.files.iter().map(|f| f.path.as_str()).collect();
        for want in ["with space.txt", "new\nline.txt", "naïve/日本.txt"] {
            assert!(paths.contains(&want), "{want:?} in {paths:?}");
        }
    }

    #[test]
    fn tree_files_truncates_at_max() {
        need_git!();
        let fx = Fx::new();
        let head = commit_files(&fx, &[("a", "1"), ("b", "2"), ("c", "3")]);
        let total = fx.vcs.tree_files(fx.repo_s(), &head, 100).unwrap();
        let n = total.files.len();
        assert!(n >= 5 && !total.truncated);
        let exact = fx.vcs.tree_files(fx.repo_s(), &head, n).unwrap();
        assert_eq!(exact.files.len(), n);
        assert!(!exact.truncated);
        let cut = fx.vcs.tree_files(fx.repo_s(), &head, 2).unwrap();
        assert_eq!(cut.files, total.files[..2]);
        assert!(cut.truncated);
    }

    #[test]
    fn tree_files_of_an_unknown_commit_is_an_error() {
        need_git!();
        let fx = Fx::new();
        let missing = Sha::parse(&"1".repeat(40)).unwrap();
        assert!(fx.vcs.tree_files(fx.repo_s(), &missing, 10).is_err());
    }

    #[test]
    fn parse_tree_listing_skips_what_is_not_a_plain_file() {
        let sha = "a".repeat(40);
        let mut raw = Vec::new();
        for (meta, path) in [
            (format!("100644 blob {sha}       3"), &b"ok.txt"[..]),
            (format!("120000 blob {sha}       3"), b"link"),
            (format!("160000 commit {sha}       -"), b"sub"),
            (format!("100644 blob {sha}       3"), b"bad\xff.txt"),
            (format!("100755 blob {sha}      12"), b"z.sh"),
        ] {
            raw.extend_from_slice(meta.as_bytes());
            raw.push(b'\t');
            raw.extend_from_slice(path);
            raw.push(0);
        }
        let list = parse_tree_listing(&raw, 10).unwrap();
        let got: Vec<(&str, u64)> = list
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.size))
            .collect();
        assert_eq!(got, [("ok.txt", 3), ("z.sh", 12)]);
        assert!(!list.truncated);
        assert!(parse_tree_listing(b"garbage", 10).is_err());
    }

    #[test]
    fn read_blob_returns_truncates_and_refuses() {
        need_git!();
        let fx = Fx::new();
        let big = "x".repeat(1024 * 1024 + 1);
        let head = commit_files(&fx, &[("small.txt", "0123456789"), ("big.txt", &big)]);
        let list = fx.vcs.tree_files(fx.repo_s(), &head, 100).unwrap();
        let blob_of = |name: &str| {
            list.files
                .iter()
                .find(|f| f.path == name)
                .unwrap()
                .blob
                .clone()
        };
        let small = blob_of("small.txt");
        assert_eq!(
            fx.vcs.read_blob(fx.repo_s(), &small, 100).unwrap(),
            b"0123456789"
        );
        assert_eq!(fx.vcs.read_blob(fx.repo_s(), &small, 4).unwrap(), b"0123");
        let err = fx
            .vcs
            .read_blob(fx.repo_s(), &blob_of("big.txt"), 10)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Limit, "{err:?}");
        let missing = Sha::parse(&"1".repeat(40)).unwrap();
        let err = fx.vcs.read_blob(fx.repo_s(), &missing, 10).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound, "{err:?}");
    }

    #[test]
    fn changed_paths_lists_added_modified_and_deleted_sorted() {
        need_git!();
        let fx = Fx::new();
        let base = head_of(&fx);
        fs::write(fx.repo.join("README.md"), "changed\n").unwrap();
        fs::remove_file(fx.repo.join("src/lib.txt")).unwrap();
        let head = commit_files(&fx, &[("z new.txt", "1"), ("a/new.txt", "2")]);
        let (paths, truncated) = fx
            .vcs
            .changed_paths(fx.repo_s(), &base, &head, 100)
            .unwrap();
        assert_eq!(
            paths,
            ["README.md", "a/new.txt", "src/lib.txt", "z new.txt"]
        );
        assert!(!truncated);
        let (cut, truncated) = fx.vcs.changed_paths(fx.repo_s(), &base, &head, 2).unwrap();
        assert_eq!(cut, ["README.md", "a/new.txt"]);
        assert!(truncated);
        let (none, truncated) = fx
            .vcs
            .changed_paths(fx.repo_s(), &head, &head, 100)
            .unwrap();
        assert!(none.is_empty() && !truncated);
    }

    #[test]
    fn context_reads_describe_the_repository_even_when_a_worktree_is_forged() {
        need_git!();
        let fx = Fx::new();
        let base = fx.base();
        let wt = fx.prepare("r1", "t1");
        let work = fx.edit_and_commit(&wt, "work.txt", "real work\n");
        let f = forge(&fx, &wt, "fleet/r1/t1");
        let list = fx.vcs.tree_files(fx.repo_s(), &work, 100).unwrap();
        let paths: Vec<&str> = list.files.iter().map(|x| x.path.as_str()).collect();
        assert!(paths.contains(&"work.txt") && !paths.contains(&"forged.txt"));
        let blob = list.files.iter().find(|x| x.path == "work.txt").unwrap();
        assert_eq!(
            fx.vcs.read_blob(fx.repo_s(), &blob.blob, 100).unwrap(),
            b"real work\n"
        );
        let (changed, _) = fx
            .vcs
            .changed_paths(fx.repo_s(), &base, &work, 100)
            .unwrap();
        assert_eq!(changed, ["work.txt"]);
        // a worktree is not a repository for these calls
        assert!(fx.vcs.tree_files(&wt, &work, 10).is_err());
        let _ = f.gitdir;
    }

    #[test]
    fn worktree_state_refuses_what_is_not_a_fleet_worktree() {
        need_git!();
        let fx = Fx::new();
        let base = fx.base();
        let err = fx.vcs.worktree_state(fx.repo_s(), &base).unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        let err = fx.vcs.worktree_state("/nonexistent/wt", &base).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    // ---- exporting a commit ----------------------------------------------------------------

    struct Exp {
        fx: Fx,
        wt: String,
        dest: PathBuf,
        index: PathBuf,
    }

    fn exp() -> Exp {
        let fx = Fx::new();
        let wt = fx.prepare("r1", "t1");
        let dest = fx.tmp.join("export/tree");
        fs::create_dir_all(&dest).unwrap();
        let index = fx.tmp.join("export/index");
        Exp {
            fx,
            wt,
            dest,
            index,
        }
    }

    impl Exp {
        fn export(&self, commit: &Sha) -> Result<Export> {
            self.fx
                .vcs
                .export_commit(&self.wt, commit, &self.dest, &self.index)
        }

        fn head(&self) -> Sha {
            self.fx
                .vcs
                .worktree_state(&self.wt, &self.fx.base())
                .unwrap()
                .head
        }
    }

    #[test]
    fn an_export_holds_exactly_the_committed_files() {
        need_git!();
        let e = exp();
        e.fx.edit_and_commit(&e.wt, "src/new.txt", "new\n");
        let head = e.head();
        // everything that is not in the commit: modified, untracked, ignored, staged
        fs::write(
            Path::new(&e.wt).join("README.md"),
            "modified, not committed\n",
        )
        .unwrap();
        fs::write(Path::new(&e.wt).join("untracked.txt"), "u\n").unwrap();
        fs::write(
            Path::new(&e.wt).join("Makefile"),
            "test:\n\ttouch planted\n",
        )
        .unwrap();
        fs::write(Path::new(&e.wt).join("staged.txt"), "s\n").unwrap();
        git(Path::new(&e.wt), &["add", "staged.txt"]);
        let before_index = git(Path::new(&e.wt), &["status", "--porcelain"]);
        let out = e.export(&head).unwrap();
        assert_eq!(
            out.tree.as_str(),
            git(Path::new(&e.wt), &["rev-parse", "HEAD^{tree}"])
        );
        assert!(out.removed_links.is_empty());
        assert_eq!(
            fs::read_to_string(e.dest.join("README.md")).unwrap(),
            "# fixture\n"
        );
        assert_eq!(
            fs::read_to_string(e.dest.join("src/new.txt")).unwrap(),
            "new\n"
        );
        assert_eq!(
            fs::read_to_string(e.dest.join("src/lib.txt")).unwrap(),
            "one\ntwo\nthree\n"
        );
        for absent in ["untracked.txt", "Makefile", "staged.txt", ".git", ".fleet"] {
            assert!(
                !e.dest.join(absent).exists(),
                "{absent} is not part of the commit"
            );
        }
        // the worktree and its index were not touched, and the temporary index is gone
        assert_eq!(
            git(Path::new(&e.wt), &["status", "--porcelain"]),
            before_index
        );
        assert!(!e.index.exists());
    }

    #[test]
    fn an_export_keeps_file_modes_and_an_older_commit_is_exported_as_it_was() {
        need_git!();
        let e = exp();
        let first =
            e.fx.edit_and_commit(&e.wt, "run.sh", "#!/bin/sh\necho first\n");
        fs::write(Path::new(&e.wt).join("run.sh"), "#!/bin/sh\necho second\n").unwrap();
        fs::set_permissions(
            Path::new(&e.wt).join("run.sh"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        git(Path::new(&e.wt), &["add", "-A"]);
        git(Path::new(&e.wt), &["commit", "-q", "-m", "second"]);
        let second = e.head();
        e.export(&first).unwrap();
        assert_eq!(
            fs::read_to_string(e.dest.join("run.sh")).unwrap(),
            "#!/bin/sh\necho first\n"
        );
        assert_eq!(
            fs::metadata(e.dest.join("run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
        fs::remove_dir_all(&e.dest).unwrap();
        fs::create_dir_all(&e.dest).unwrap();
        e.export(&second).unwrap();
        assert_eq!(
            fs::read_to_string(e.dest.join("run.sh")).unwrap(),
            "#!/bin/sh\necho second\n"
        );
        assert_ne!(
            fs::metadata(e.dest.join("run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o100,
            0
        );
    }

    #[test]
    fn symlinks_that_could_leave_the_export_are_removed() {
        need_git!();
        let e = exp();
        let wt = Path::new(&e.wt);
        fs::create_dir_all(wt.join("sub")).unwrap();
        fs::write(wt.join("sub/real.txt"), "r\n").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", wt.join("abs-link")).unwrap();
        std::os::unix::fs::symlink("../outside", wt.join("sub/up-link")).unwrap();
        std::os::unix::fs::symlink("sub/..", wt.join("dot-dot-link")).unwrap();
        std::os::unix::fs::symlink("sub/real.txt", wt.join("down-link")).unwrap();
        std::os::unix::fs::symlink("sub", wt.join("dir-link")).unwrap();
        git(wt, &["add", "-A"]);
        git(wt, &["commit", "-q", "-m", "links"]);
        let out = e.export(&e.head()).unwrap();
        assert_eq!(
            out.removed_links,
            ["abs-link", "dot-dot-link", "sub/up-link"]
        );
        for gone in ["abs-link", "dot-dot-link", "sub/up-link"] {
            assert!(fs::symlink_metadata(e.dest.join(gone)).is_err(), "{gone}");
        }
        for kept in ["down-link", "dir-link"] {
            assert!(
                fs::symlink_metadata(e.dest.join(kept))
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{kept}"
            );
        }
        assert_eq!(fs::read_to_string(e.dest.join("down-link")).unwrap(), "r\n");
    }

    #[test]
    fn export_refuses_unknown_commits_dirty_destinations_and_foreign_directories() {
        need_git!();
        let e = exp();
        let missing = Sha::parse(&"1".repeat(40)).unwrap();
        assert!(e.export(&missing).is_err());
        fs::write(e.dest.join("already"), "x").unwrap();
        let err = e.export(&e.head()).unwrap_err();
        assert!(err.message.contains("not empty"), "{}", err.message);
        fs::remove_file(e.dest.join("already")).unwrap();
        let err =
            e.fx.vcs
                .export_commit(e.fx.repo_s(), &e.head(), &e.dest, &e.index)
                .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden, "only fleet worktrees");
        let err =
            e.fx.vcs
                .export_commit(&e.wt, &e.head(), &e.fx.tmp.join("nope"), &e.index)
                .unwrap_err();
        assert!(err.message.contains("does not exist"), "{}", err.message);
    }

    #[test]
    fn planted_hooks_filters_and_local_config_do_not_run_during_an_export() {
        need_git!();
        let fx = Fx::with_attributes(Some("* filter=planted\n"));
        let script = fx.plant_script();
        fx.append_config(&format!(
            "[filter \"planted\"]\n\tsmudge = {script}\n\tclean = {script}\n\trequired = false\n[core]\n\tfsmonitor = {script}\n\thooksPath = /nonexistent\n"
        ));
        let wt = fx.prepare("r1", "t1");
        let dest = fx.tmp.join("export/tree");
        fs::create_dir_all(&dest).unwrap();
        let head = fx.vcs.worktree_state(&wt, &fx.base()).unwrap().head;
        fx.vcs
            .export_commit(&wt, &head, &dest, &fx.tmp.join("export/index"))
            .unwrap();
        assert!(!fx.triggered(), "a planted filter or fsmonitor ran");
        assert!(dest.join("README.md").exists());
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
                    head: fx.base(),
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

    fn merge_request(fx: &Fx, run: &str, task: &str, head: Sha) -> IntegrationMerge {
        IntegrationMerge {
            repo: fx.repo_s().to_string(),
            run: rid(run),
            task: tid(task),
            base: fx.base(),
            integration_branch: format!("fleet/{run}/integration"),
            task_branch: format!("fleet/{run}/{task}"),
            message: format!("merge {task}"),
            identity: ident(),
            head,
        }
    }

    #[test]
    fn integration_merges_exactly_the_approved_commit() {
        need_git!();
        let fx = Fx::new();
        let w1 = fx.prepare("r1", "t1");
        let approved = fx.edit_and_commit(&w1, "a.txt", "approved\n");
        let result = fx
            .vcs
            .merge_into_integration(&merge_request(&fx, "r1", "t1", approved.clone()))
            .unwrap();
        let MergeOutcome::Merged { sha } = result.outcome else {
            panic!("expected a merge");
        };
        let parents = git(
            &fx.repo,
            &["rev-list", "--parents", "-n", "1", sha.as_str()],
        );
        assert!(
            parents.ends_with(approved.as_str()),
            "the second parent must be the approved commit: {parents}"
        );
    }

    #[test]
    fn a_branch_that_moved_after_approval_is_not_merged() {
        need_git!();
        let fx = Fx::new();
        let w1 = fx.prepare("r1", "t1");
        let approved = fx.edit_and_commit(&w1, "a.txt", "approved\n");
        // The worker keeps working after the approval; the branch now points elsewhere.
        let moved = fx.edit_and_commit(&w1, "evil.txt", "unreviewed\n");
        assert_ne!(approved, moved);
        let err = fx
            .vcs
            .merge_into_integration(&merge_request(&fx, "r1", "t1", approved.clone()))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
        assert!(
            err.message.contains("moved since it was approved"),
            "{}",
            err.message
        );
        let integration = fx.repo.join(".fleet-worktrees/r1/_integration");
        assert_eq!(
            git(&integration, &["rev-parse", "HEAD"]),
            fx.base().as_str(),
            "nothing may have been merged"
        );
        assert!(!integration.join("a.txt").exists() && !integration.join("evil.txt").exists());
        assert_eq!(git(&integration, &["status", "--porcelain"]), "");
    }

    #[test]
    fn a_branch_reset_to_another_commit_or_deleted_is_not_merged() {
        need_git!();
        let fx = Fx::new();
        let w1 = fx.prepare("r1", "t1");
        let approved = fx.edit_and_commit(&w1, "a.txt", "approved\n");
        // Another commit is asserted as approved: refused although the branch exists.
        let err = fx
            .vcs
            .merge_into_integration(&merge_request(&fx, "r1", "t1", fx.base()))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
        // A branch that does not exist any more is refused the same way.
        let w2 = fx.prepare("r1", "t2");
        let sha = fx.edit_and_commit(&w2, "b.txt", "b\n");
        fx.vcs
            .remove_worktree(fx.repo_s(), &w2, "fleet/r1/t2", true)
            .unwrap();
        let err = fx
            .vcs
            .merge_into_integration(&merge_request(&fx, "r1", "t2", sha))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
        // The approved one still merges afterwards.
        let ok = fx
            .vcs
            .merge_into_integration(&merge_request(&fx, "r1", "t1", approved))
            .unwrap();
        assert!(matches!(ok.outcome, MergeOutcome::Merged { .. }));
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
    fn a_branch_can_be_deleted_after_its_worktree_was_removed() {
        need_git!();
        let fx = Fx::new();
        let w = fx.prepare("r1", "t1");
        fx.vcs
            .remove_worktree(fx.repo_s(), &w, "fleet/r1/t1", false)
            .unwrap();
        fx.vcs
            .remove_worktree(fx.repo_s(), "", "fleet/r1/t1", true)
            .unwrap();
        assert_eq!(git(&fx.repo, &["branch", "--list", "fleet/*"]), "");
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

    // ---- a worker that rewrites what its worktree points at ---------------------------------
    //
    // A linked worktree's `.git` is a plain file in the worker's own tree. Git does not check where
    // it points, so a worker can aim it at a repository it prepared (alternates onto the real object
    // store, a replace ref for the task's head). Anything read through that gitdir, a tree for
    // instance, then describes what the worker chose, not what the real branch holds and what a
    // merge brings in.

    /// A repository the worker made: its objects come from the real store through alternates, it
    /// has the task's branch checked out at the real head, and `refs/replace/<head>` names a
    /// commit with another tree (`forged.txt` instead of the real files). The worktree's files and
    /// index are those of the forged tree, so as far as the forged repository is concerned the
    /// worktree is clean.
    struct Forgery {
        real_head: Sha,
        real_tree: String,
        forged_tree: String,
        gitdir: PathBuf,
    }

    fn forge(fx: &Fx, wt: &str, branch: &str) -> Forgery {
        let real_head = Sha::parse(&git(
            fx.repo.as_path(),
            &["rev-parse", &format!("refs/heads/{branch}")],
        ))
        .unwrap();
        let real_tree = git(
            fx.repo.as_path(),
            &["rev-parse", &format!("{}^{{tree}}", real_head.as_str())],
        );
        let gitdir = fx.tmp.join("forged.git");
        fs::create_dir_all(&gitdir).unwrap();
        git(&gitdir, &["init", "-q", "--bare", "."]);
        git(&gitdir, &["config", "core.bare", "false"]);
        git(&gitdir, &["config", "core.worktree", wt]);
        let objects = fx.repo.join(".git/objects");
        fs::write(
            gitdir.join("objects/info/alternates"),
            format!("{}\n", objects.display()),
        )
        .unwrap();
        let file = fx.tmp.join("forged-file");
        fs::write(&file, "the forged content\n").unwrap();
        let blob = git(&gitdir, &["hash-object", "-w", file.to_str().unwrap()]);
        let mktree_input = fx.tmp.join("mktree-input");
        fs::write(&mktree_input, format!("100644 blob {blob}\tforged.txt\n")).unwrap();
        let tree = std::process::Command::new("sh")
            .arg("-c")
            .arg("git mktree < \"$1\"")
            .arg("sh")
            .arg(&mktree_input)
            .env("GIT_DIR", &gitdir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        let forged_tree = String::from_utf8(tree.stdout).unwrap().trim().to_string();
        assert_eq!(
            forged_tree.len(),
            40,
            "{}",
            String::from_utf8_lossy(&tree.stderr)
        );
        let replacement = git(&gitdir, &["commit-tree", "-m", "replacement", &forged_tree]);
        git(
            &gitdir,
            &[
                "update-ref",
                &format!("refs/heads/{branch}"),
                real_head.as_str(),
            ],
        );
        git(
            &gitdir,
            &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
        );
        git(
            &gitdir,
            &[
                "update-ref",
                &format!("refs/replace/{}", real_head.as_str()),
                &replacement,
            ],
        );
        // the worktree now says: my gitdir is the forged one
        fs::write(
            Path::new(wt).join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();
        // make its files and index those of the forged tree (the forged repository sees a clean tree)
        let wtp = Path::new(wt);
        for entry in fs::read_dir(wtp).unwrap().flatten() {
            let name = entry.file_name();
            if name == ".git" || name == ".fleet" {
                continue;
            }
            let p = entry.path();
            if p.is_dir() {
                fs::remove_dir_all(&p).unwrap();
            } else {
                fs::remove_file(&p).unwrap();
            }
        }
        fs::write(wtp.join("forged.txt"), "the forged content\n").unwrap();
        let out = std::process::Command::new("git")
            .args(["read-tree", real_head.as_str()])
            .env("GIT_DIR", &gitdir)
            .env("GIT_WORK_TREE", wt)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        Forgery {
            real_head,
            real_tree,
            forged_tree,
            gitdir,
        }
    }

    #[test]
    fn a_worker_that_repoints_its_gitdir_cannot_make_evidence_describe_another_tree() {
        need_git!();
        let fx = Fx::new();
        let base = fx.base();
        let wt = fx.prepare("r1", "t1");
        let branch = "fleet/r1/t1";
        // honest worktree: head and tree are the real branch's
        let honest = fx.vcs.worktree_state(&wt, &base).unwrap();
        let f = forge(&fx, &wt, branch);
        assert_eq!(honest.head, f.real_head);
        assert_eq!(honest.tree.as_str(), f.real_tree);
        assert_ne!(f.forged_tree, f.real_tree);
        // review: committing the worker's output through the forged gitdir is refused
        let err = fx.vcs.commit_all(&wt, "review", &ident()).unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden, "{err:?}");
        assert!(err.message.contains(".git"), "{}", err.message);
        // verification: neither the state nor an export may be read through it
        let err = fx.vcs.worktree_state(&wt, &base).unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden, "{err:?}");
        let dest = fx.tmp.join("export/tree");
        fs::create_dir_all(&dest).unwrap();
        let err = fx
            .vcs
            .export_commit(&wt, &f.real_head, &dest, &fx.tmp.join("export/index"))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden, "{err:?}");
        assert!(
            fs::read_dir(&dest).unwrap().next().is_none(),
            "nothing was exported"
        );
        // merging into it is refused as well
        let err = fx
            .vcs
            .merge_into_worktree(&wt, &["fleet/r1/other".to_string()], &ident())
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden, "{err:?}");
        // the real branch was not touched by any of it
        assert_eq!(
            fx.vcs.branch_head(fx.repo_s(), branch).unwrap(),
            f.real_head
        );
        let _ = f.gitdir;
    }

    #[test]
    fn every_other_way_of_pointing_a_worktree_somewhere_else_is_refused() {
        need_git!();
        let fx = Fx::new();
        let base = fx.base();
        let wt = fx.prepare("r1", "t1");
        let other = fx.prepare("r1", "t2");
        let pointer = Path::new(&wt).join(".git");
        let own = fs::read_to_string(&pointer).unwrap();
        assert!(fx.vcs.worktree_state(&wt, &base).is_ok());
        let refused = |what: &str| {
            let err = fx.vcs.worktree_state(&wt, &base).unwrap_err();
            assert_eq!(err.code, ErrorCode::Forbidden, "{what}: {err:?}");
        };
        // the entry of a sibling worktree
        let sibling = fs::read_to_string(Path::new(&other).join(".git")).unwrap();
        fs::write(&pointer, &sibling).unwrap();
        refused("the pointer of a sibling worktree");
        // a relative path, the main repository's own .git, a path with a link in it
        fs::write(&pointer, "gitdir: ../../../.git\n").unwrap();
        refused("a relative path");
        fs::write(
            &pointer,
            format!("gitdir: {}\n", fx.repo.join(".git").display()),
        )
        .unwrap();
        refused("the main .git directory");
        let link = fx.tmp.join("link-to-admin");
        let admin = own.trim().strip_prefix("gitdir: ").unwrap().to_string();
        std::os::unix::fs::symlink(&admin, &link).unwrap();
        fs::write(&pointer, format!("gitdir: {}\n", link.display())).unwrap();
        refused("a symbolic link to the right entry");
        // junk and a pointer that is not a regular file
        for junk in [
            "",
            "gitdir:",
            "gitdir: \n",
            "not a pointer\n",
            "gitdir: /nonexistent\n",
        ] {
            fs::write(&pointer, junk).unwrap();
            refused(junk);
        }
        fs::write(&pointer, "gitdir: /x\ngitdir: /y\n").unwrap();
        refused("two lines");
        fs::remove_file(&pointer).unwrap();
        std::os::unix::fs::symlink(own.trim().replace("gitdir: ", ""), &pointer).unwrap();
        refused("the pointer itself a symbolic link");
        fs::remove_file(&pointer).unwrap();
        fs::create_dir(&pointer).unwrap();
        refused("the pointer a directory");
        fs::remove_dir(&pointer).unwrap();
        // the right pointer works again
        fs::write(&pointer, &own).unwrap();
        assert!(fx.vcs.worktree_state(&wt, &base).is_ok());
        // the entry itself was changed: its back pointer, its commondir, its HEAD
        let adm = PathBuf::from(admin);
        let back = fs::read_to_string(adm.join("gitdir")).unwrap();
        fs::write(adm.join("gitdir"), "/somewhere/else/.git\n").unwrap();
        refused("an entry that does not point back");
        fs::write(adm.join("gitdir"), &back).unwrap();
        let common = fs::read_to_string(adm.join("commondir")).unwrap();
        fs::write(
            adm.join("commondir"),
            format!("{}\n", fx.tmp.join("forged.git").display()),
        )
        .unwrap();
        refused("a commondir elsewhere");
        fs::write(adm.join("commondir"), &common).unwrap();
        assert!(fx.vcs.worktree_state(&wt, &base).is_ok());
        let head = fs::read_to_string(adm.join("HEAD")).unwrap();
        fs::write(adm.join("HEAD"), "ref: refs/heads/fleet/r1/t2\n").unwrap();
        refused("a worktree on another task's branch");
        fs::write(adm.join("HEAD"), format!("{}\n", fx.base().as_str())).unwrap();
        refused("a detached head");
        fs::write(adm.join("HEAD"), &head).unwrap();
        assert!(fx.vcs.worktree_state(&wt, &base).is_ok());
    }

    #[test]
    fn replace_refs_never_change_what_fleet_sees() {
        need_git!();
        let fx = Fx::new();
        let base = fx.base();
        let wt = fx.prepare("r1", "t1");
        let head = fx.edit_and_commit(&wt, "src/new.txt", "new\n");
        let real_tree = git(
            fx.repo.as_path(),
            &[
                "--no-replace-objects",
                "rev-parse",
                &format!("{}^{{tree}}", head.as_str()),
            ],
        );
        // a replacement commit with a different tree, in the shared repository
        let blob_file = fx.tmp.join("other-file");
        fs::write(&blob_file, "replaced\n").unwrap();
        let blob = git(
            &fx.repo,
            &["hash-object", "-w", blob_file.to_str().unwrap()],
        );
        let tree_input = fx.tmp.join("tree-input");
        fs::write(&tree_input, format!("100644 blob {blob}\tREPLACED.txt\n")).unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg("git mktree < \"$1\"")
            .arg("sh")
            .arg(&tree_input)
            .current_dir(&fx.repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        let other_tree = String::from_utf8(out.stdout).unwrap().trim().to_string();
        let replacement = git(&fx.repo, &["commit-tree", "-m", "replacement", &other_tree]);
        git(
            &fx.repo,
            &[
                "update-ref",
                &format!("refs/replace/{}", head.as_str()),
                &replacement,
            ],
        );
        // fleet's git ignores it: the state, the head and the export are the real ones
        let s = fx.vcs.worktree_state(&wt, &base).unwrap();
        assert_eq!(s.head, head);
        assert_eq!(
            s.tree.as_str(),
            real_tree,
            "the tree of the commit, not of its replacement"
        );
        let dest = fx.tmp.join("export/tree");
        fs::create_dir_all(&dest).unwrap();
        let out = fx
            .vcs
            .export_commit(&wt, &head, &dest, &fx.tmp.join("export/index"))
            .unwrap();
        assert_eq!(out.tree.as_str(), real_tree);
        assert!(dest.join("src/new.txt").exists());
        assert!(!dest.join("REPLACED.txt").exists());
        assert_eq!(
            fx.vcs.branch_head(fx.repo_s(), "fleet/r1/t1").unwrap(),
            head
        );
        assert_eq!(
            fx.vcs.commit_tree(fx.repo_s(), &head).unwrap().as_str(),
            real_tree
        );
    }

    #[test]
    fn a_repository_with_alternates_or_grafts_is_refused() {
        need_git!();
        let fx = Fx::new();
        let base = fx.base();
        let wt = fx.prepare("r1", "t1");
        let objects = fx.repo.join(".git/objects/info");
        fs::create_dir_all(&objects).unwrap();
        for (what, path) in [
            ("alternates", objects.join("alternates")),
            ("http-alternates", objects.join("http-alternates")),
            ("grafts", fx.repo.join(".git/info/grafts")),
        ] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, format!("{}\n", fx.tmp.join("elsewhere").display())).unwrap();
            let err = fx.vcs.worktree_state(&wt, &base).unwrap_err();
            assert_eq!(err.code, ErrorCode::Forbidden, "{what}: {err:?}");
            assert!(err.message.contains(what), "{what}: {}", err.message);
            assert_eq!(
                fx.vcs
                    .prepare_worktree(fx.repo_s(), &rid("r1"), &tid("t9"), "fleet/r1/t9", &base)
                    .unwrap_err()
                    .code,
                ErrorCode::Forbidden,
                "{what}"
            );
            assert_eq!(
                fx.vcs.commit_tree(fx.repo_s(), &base).unwrap_err().code,
                ErrorCode::Forbidden,
                "{what}"
            );
            fs::remove_file(&path).unwrap();
            assert!(fx.vcs.worktree_state(&wt, &base).is_ok(), "{what} removed");
        }
    }

    #[test]
    fn message_cleaning_is_bounded_and_printable() {
        assert_eq!(clean_message("a\u{1}b\nc", "d"), "a b\nc");
        assert_eq!(clean_message("  \u{1}  ", "dflt"), "dflt");
        assert!(clean_message(&"é".repeat(10_000), "d").len() <= MAX_MESSAGE);
    }
}
