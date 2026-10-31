//! Running verification contracts: the adapter side of `neo-verify`.
//!
//! This module observes (starts a process, reads a file, reports what git found) and the domain
//! judges (`neo_verify::judge`).
//!
//! **What runs where.** The checks do not run in the task's worktree. The commit that was
//! recorded for the task is exported into a fresh directory of its own, in a private directory
//! outside the state directory (`git checkout-index` from a temporary index: the worktree's files,
//! ignored and untracked files, index and local git state are never read), commands run with that
//! directory as their working directory (nothing above it leads to the state directory), and the directory is removed afterwards. So planted uncommitted files, a Makefile
//! that only exists in the worktree and a worker-local git configuration are never executed, and
//! what is proven is the commit that will be merged. Each job has a home directory and a
//! temporary directory of its own, empty, in place of the operator's.
//!
//! **How a command runs.** Like approved commands and quality gates: no shell, an absolute program
//! path that is not inside the task's worktree, its repository or the export
//! ([`trusted_program`]), the operator's environment allowlist (minus everything that would lead
//! to the operator's home or to fleet's state) and nothing else, a `PATH` cut down to absolute
//! directories that are not in a repository and that no group or other user may write to (a
//! directory the operator owns, such as `~/.local/bin`, is kept, and a worker running as the
//! operator can write there), no stdin, its own
//! process group, a timeout and a cap on captured output; processes that leave the group are
//! tracked and stopped too (see `Sweeper` in `jobs`). Everything that is reported back is redacted
//! first. Files are opened below the export root without following any symlink
//! ([`open_beneath`]).
//!
//! **What this is not.** The processes still run as the operator's user and can use the network and
//! every file that user can read. The export removes what the *worker* planted in the worktree and
//! gives the commands no home to read; it does not make them safe to run against hostile code.

use crate::digest::Sha256Digest;
use crate::fsx::{self, open_beneath, read_open, ReadError};
use crate::git::GitVcs;
use crate::jobs::{
    redact_block, redacted_tail, run_bounded_tracked, trusted_program, JobContext, RunSpec,
};
use crate::json;
use fleet_app::ports::{VerifiedContract, VerifyDone, VerifyJob};
use fleet_domain::approval::Digest;
use fleet_domain::error::Result;
use fleet_domain::limits::MAX_GATE_OUTPUT;
use fleet_domain::quality::ExitKind;
use fleet_domain::status::Verdict;
use neo_verify::contract::{Check, CheckKind, FileAssertion};
use neo_verify::judge::{judge, CheckReport, FileObservation, Observation};
use neo_verify::limits::{JOB_BUDGET_MS, MAX_FILE_BYTES};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// What verification needs besides processes: a git to export with and a private place to export
/// to.
pub struct VerifyHost {
    git: GitVcs,
    scratch: PathBuf,
}

/// Name prefix of the directory of one job below the scratch directory.
const JOB_DIR_PREFIX: &str = "job-";

impl VerifyHost {
    /// Prepares `scratch` (a private directory outside the state directory, see
    /// `statedir::verify_scratch_dir`, below a private directory of the same user) and removes what
    /// an earlier daemon run left in it.
    pub fn new(git: GitVcs, scratch: PathBuf) -> Result<VerifyHost> {
        if let Some(parent) = scratch.parent() {
            fsx::ensure_private_dir(parent)?;
        }
        fsx::ensure_private_dir(&scratch)?;
        if let Ok(entries) = fs::read_dir(&scratch) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(JOB_DIR_PREFIX)
                {
                    remove_tree(&entry.path());
                }
            }
        }
        Ok(VerifyHost { git, scratch })
    }
}

/// Removes a directory tree that a check may have left read-only, never following a symlink:
/// every directory is opened with `O_NOFOLLOW | O_DIRECTORY` (relative to its parent's descriptor)
/// and made writable through that descriptor, so a directory that is replaced by a link while
/// this runs is not a path to chmod. A directory that is unreadable (mode 000) is made accessible
/// by `fchmodat` without following links; where the platform cannot do that it is left, and
/// `remove_dir_all` (which does not follow links either) reports nothing and removes what it can.
fn remove_tree(path: &Path) {
    use nix::fcntl::{open, openat, OFlag};
    use nix::sys::stat::{fchmod, fchmodat, FchmodatFlags, Mode};
    use std::os::fd::{AsFd, OwnedFd};

    const FLAGS: OFlag = OFlag::from_bits_truncate(
        OFlag::O_RDONLY.bits()
            | OFlag::O_NOFOLLOW.bits()
            | OFlag::O_DIRECTORY.bits()
            | OFlag::O_CLOEXEC.bits(),
    );
    let private = Mode::from_bits_truncate(0o700);

    fn open_dir(
        parent: Option<&OwnedFd>,
        path: &Path,
        name: &std::ffi::OsStr,
        flags: OFlag,
        private: Mode,
    ) -> Option<OwnedFd> {
        let try_open = || match parent {
            Some(p) => openat(p.as_fd(), name, flags, Mode::empty()),
            None => open(path, flags, Mode::empty()),
        };
        match try_open() {
            Ok(fd) => Some(fd),
            Err(nix::errno::Errno::EACCES) => {
                let chmod = match parent {
                    Some(p) => fchmodat(
                        p.as_fd(),
                        Path::new(name),
                        private,
                        FchmodatFlags::NoFollowSymlink,
                    ),
                    None => fchmodat(
                        nix::fcntl::AT_FDCWD,
                        path,
                        private,
                        FchmodatFlags::NoFollowSymlink,
                    ),
                };
                chmod.ok()?;
                try_open().ok()
            }
            Err(_) => None,
        }
    }

    fn relax(parent: Option<&OwnedFd>, path: &Path, flags: OFlag, private: Mode, depth: usize) {
        let Some(name) = path.file_name() else { return };
        let Some(fd) = open_dir(parent, path, name, flags, private) else {
            return;
        };
        if fchmod(fd.as_fd(), private).is_err() || depth >= 256 {
            return;
        }
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            relax(Some(&fd), &entry.path(), flags, private, depth + 1);
        }
    }
    relax(None, path, FLAGS, private, 0);
    let _ = fs::remove_dir_all(path);
}

static JOB_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The private directories of one job; removed when it is dropped.
struct JobDir {
    root: PathBuf,
}

impl JobDir {
    fn create(host: &VerifyHost) -> Result<JobDir> {
        let n = JOB_COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let root = host.scratch.join(format!(
            "{JOB_DIR_PREFIX}{}-{n}-{nanos}",
            std::process::id()
        ));
        let dir = JobDir { root };
        for sub in ["", "tree", "home", "tmp"] {
            fsx::ensure_private_dir(&dir.root.join(sub))?;
        }
        Ok(dir)
    }

    fn tree(&self) -> PathBuf {
        self.root.join("tree")
    }
    fn home(&self) -> PathBuf {
        self.root.join("home")
    }
    fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }
    fn index(&self) -> PathBuf {
        self.root.join("index")
    }
}

impl Drop for JobDir {
    fn drop(&mut self) {
        remove_tree(&self.root);
    }
}

/// Variables that always come from fleet, never from the operator's allowlist: they would lead to
/// the operator's home or configuration, to fleet's state, or to git behaviour.
fn replaced(name: &str) -> bool {
    matches!(
        name,
        "HOME"
            | "TMPDIR"
            | "TMP"
            | "TEMP"
            | "PATH"
            | "PWD"
            | "OLDPWD"
            | "SHELL"
            | "USER"
            | "LOGNAME"
    ) || name.starts_with("XDG_")
        || name.starts_with("FLEET_")
        || name.starts_with("GIT_")
}

/// True when nobody but the owner can write to `dir`.
fn private_to_owner(dir: &Path) -> bool {
    fs::metadata(dir).is_ok_and(|m| m.is_dir() && m.permissions().mode() & 0o022 == 0)
}

/// The directories of `path` that checks may take programs from: absolute, existing, outside every
/// directory in `untrusted` (the worktree, its repository, the scratch area) and not writable by
/// group or others.
pub(crate) fn trusted_path(path: &str, untrusted: &[PathBuf]) -> Vec<String> {
    path.split(':')
        .filter(|e| Path::new(e).is_absolute())
        .filter_map(|e| fs::canonicalize(e).ok().map(|c| (e, c)))
        .filter(|(_, c)| !untrusted.iter().any(|u| c.starts_with(u)))
        .filter(|(_, c)| private_to_owner(c))
        .map(|(e, _)| e.to_string())
        .collect()
}

/// The environment of the commands of one job: the operator's allowlist without what leads to the
/// operator's home, configuration or fleet's state, plus a home, a temporary directory and
/// configuration directories inside the job directory, a reduced `PATH` and git settings that keep
/// git from reading anything outside the export.
pub(crate) fn isolated_env(
    base: &[(String, String)],
    home: &Path,
    tmp: &Path,
    ceiling: &Path,
    untrusted: &[PathBuf],
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> =
        base.iter().filter(|(k, _)| !replaced(k)).cloned().collect();
    let path = base
        .iter()
        .rev()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| trusted_path(v, untrusted).join(":"))
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "/usr/bin:/bin".to_string());
    let h = home.display().to_string();
    let set = |env: &mut Vec<(String, String)>, k: &str, v: String| env.push((k.to_string(), v));
    set(&mut env, "PATH", path);
    set(&mut env, "HOME", h.clone());
    set(&mut env, "TMPDIR", tmp.display().to_string());
    set(&mut env, "XDG_CONFIG_HOME", format!("{h}/.config"));
    set(&mut env, "XDG_CACHE_HOME", format!("{h}/.cache"));
    set(&mut env, "XDG_DATA_HOME", format!("{h}/.local/share"));
    set(&mut env, "XDG_STATE_HOME", format!("{h}/.local/state"));
    set(
        &mut env,
        "GIT_CEILING_DIRECTORIES",
        ceiling.display().to_string(),
    );
    set(&mut env, "GIT_CONFIG_GLOBAL", "/dev/null".to_string());
    set(&mut env, "GIT_CONFIG_SYSTEM", "/dev/null".to_string());
    set(&mut env, "GIT_CONFIG_NOSYSTEM", "1".to_string());
    set(&mut env, "GIT_TERMINAL_PROMPT", "0".to_string());
    env.sort();
    env
}

/// Runs every check of every target of `job`, in order, and reports on all of them (also after a
/// failure: the retry brief lists every failing check).
pub fn run_verify(job: &VerifyJob, ctx: &JobContext) -> VerifyDone {
    run_verify_within(job, ctx, Duration::from_millis(JOB_BUDGET_MS))
}

/// What one job's checks run in.
struct Run<'a> {
    job: &'a VerifyJob,
    ctx: &'a JobContext,
    /// The export: the working directory of commands and the root of file assertions.
    tree: PathBuf,
    env: Vec<(String, String)>,
    /// Directories whose programs are never run: the worktree, its repository, the export.
    untrusted: Vec<PathBuf>,
}

pub(crate) fn run_verify_within(job: &VerifyJob, ctx: &JobContext, budget: Duration) -> VerifyDone {
    let Some(host) = &ctx.verify else {
        return failed_run(job, "verification is not configured in this daemon");
    };
    let dir = match JobDir::create(host) {
        Ok(d) => d,
        Err(e) => {
            return failed_run(
                job,
                &format!(
                    "could not prepare a directory for the checks: {}",
                    e.message
                ),
            )
        }
    };
    let exported =
        host.git
            .export_commit(&job.worktree, &job.state.head, &dir.tree(), &dir.index());
    match exported {
        Err(e) => {
            return failed_run(
                job,
                &format!(
                    "could not export commit {}: {}",
                    job.state.head.as_str().get(..12).unwrap_or_default(),
                    e.message
                ),
            )
        }
        Ok(x) if x.tree != job.state.tree => {
            return failed_run(
                job,
                "the tree of the commit is not the one the job was started with",
            )
        }
        Ok(_) => {}
    }
    let mut untrusted = vec![PathBuf::from(&job.worktree), dir.root.clone()];
    if let Ok(real) = fs::canonicalize(&job.worktree) {
        untrusted.push(real.clone());
        untrusted.extend(
            real.ancestors()
                .find(|a| a.file_name().is_some_and(|n| n == ".fleet-worktrees"))
                .and_then(Path::parent)
                .map(Path::to_path_buf),
        );
    }
    untrusted.push(host.scratch.clone());
    let untrusted: Vec<PathBuf> = untrusted
        .into_iter()
        .map(|p| fs::canonicalize(&p).unwrap_or(p))
        .collect();
    let env = isolated_env(&job.env, &dir.home(), &dir.tmp(), &dir.root, &untrusted);
    let run = Run {
        job,
        ctx,
        tree: dir.tree(),
        env,
        untrusted,
    };
    let started = Instant::now();
    let results = job
        .targets
        .iter()
        .map(|target| VerifiedContract {
            goal: target.goal.clone(),
            contract_id: target.contract_id.clone(),
            reports: target
                .contract
                .checks()
                .iter()
                .map(|check| {
                    let left = budget.saturating_sub(started.elapsed());
                    run_check(check, &run, left)
                })
                .collect(),
        })
        .collect();
    VerifyDone {
        run: job.run.clone(),
        task: job.task.clone(),
        attempt: job.attempt,
        origin: job.origin,
        commit: job.state.head.clone(),
        tree: job.state.tree.clone(),
        dirty: job.state.dirty,
        results,
    }
}

/// The report of a job that could not run its checks at all: every check is an error, never
/// silence and never a pass.
pub(crate) fn failed_run(job: &VerifyJob, why: &str) -> VerifyDone {
    VerifyDone {
        run: job.run.clone(),
        task: job.task.clone(),
        attempt: job.attempt,
        origin: job.origin,
        commit: job.state.head.clone(),
        tree: job.state.tree.clone(),
        dirty: job.state.dirty,
        results: job
            .targets
            .iter()
            .map(|t| VerifiedContract {
                goal: t.goal.clone(),
                contract_id: t.contract_id.clone(),
                reports: t
                    .contract
                    .checks()
                    .iter()
                    .map(|c| CheckReport {
                        check: c.id.clone(),
                        verdict: Verdict::Error,
                        exit_code: None,
                        summary: why.to_string(),
                        output: String::new(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn run_check(check: &Check, run: &Run<'_>, left: Duration) -> CheckReport {
    let observed = if left.is_zero() {
        Observation::Failed("skipped: the time budget of this verification is spent".to_string())
    } else {
        observe(check, run, left)
    };
    let mut report = judge(check, &observed);
    // Whatever was printed, and any text that quotes it, leaves this module redacted.
    report.summary = redact_block(&run.ctx.redactor, &report.summary);
    report.output = redact_block(&run.ctx.redactor, &report.output);
    report
}

fn observe(check: &Check, run: &Run<'_>, left: Duration) -> Observation {
    match &check.kind {
        CheckKind::Command {
            program,
            args,
            timeout_ms,
            ..
        } => run_process(program, args, *timeout_ms, false, run, left),
        CheckKind::RefcheckJson {
            program,
            args,
            timeout_ms,
        } => run_process(program, args, *timeout_ms, true, run, left),
        CheckKind::File { path, assertion } => {
            Observation::File(observe_file(&run.tree, path, assertion))
        }
        CheckKind::DiffOnly { .. } => Observation::Changed {
            paths: run.job.state.changed.clone(),
            truncated: run.job.state.truncated,
        },
    }
}

fn run_process(
    program: &str,
    args: &[String],
    timeout_ms: u64,
    structured: bool,
    run: &Run<'_>,
    left: Duration,
) -> Observation {
    let refused = |why: String| Observation::Process {
        exit: ExitKind::SpawnFailed(why),
        output: String::new(),
        stdout_json: None,
    };
    // The program must not be something a worker could have planted: not in the worktree, not in
    // its repository, not in the export (a program of the export would be the worker's own).
    let mut resolved = PathBuf::from(program);
    for root in &run.untrusted {
        match trusted_program(&resolved, root) {
            Ok(p) => resolved = p,
            Err(why) => {
                let why = why.strip_prefix("could not start: ").unwrap_or(&why);
                return refused(why.to_string());
            }
        }
    }
    let allowed = u64::try_from(left.as_millis()).unwrap_or(u64::MAX);
    let result = run_bounded_tracked(
        &RunSpec {
            program: resolved.display().to_string(),
            args: args.to_vec(),
            cwd: run.tree.clone(),
            env: run.env.clone(),
            timeout_ms: timeout_ms.min(allowed),
            max_output: MAX_GATE_OUTPUT,
        },
        &run.ctx.tracker,
    );
    // The whole capture is redacted before it is cut to its tail and judged.
    let output = redacted_tail(&result, &run.ctx.redactor);
    let stdout_json = structured.then(|| json::parse_value(&result.stdout, MAX_GATE_OUTPUT));
    Observation::Process {
        exit: result.exit,
        output,
        stdout_json,
    }
}

fn observe_file(dir: &Path, path: &str, assertion: &FileAssertion) -> FileObservation {
    let file = match open_beneath(dir, path) {
        Ok(f) => f,
        Err(e) => return file_error(&e),
    };
    if matches!(assertion, FileAssertion::Exists) {
        return match file.metadata() {
            Ok(m) if m.is_file() => FileObservation::Regular,
            Ok(_) => FileObservation::NotRegular,
            Err(e) => FileObservation::Unreadable(e.kind().to_string()),
        };
    }
    let bytes = match read_open(file, MAX_FILE_BYTES) {
        Ok(b) => b,
        Err(e) => return file_error(&e),
    };
    match assertion {
        FileAssertion::Sha256(_) => FileObservation::Hashed(Sha256Digest.sha256_hex(&bytes)),
        _ => match String::from_utf8(bytes) {
            Ok(text) => FileObservation::Text(text),
            Err(_) => FileObservation::NotText,
        },
    }
}

fn file_error(e: &ReadError) -> FileObservation {
    match e {
        ReadError::Missing => FileObservation::Missing,
        ReadError::NotRegular(_) => FileObservation::NotRegular,
        ReadError::TooLarge { .. } => FileObservation::TooLarge,
        ReadError::Io(why) => FileObservation::Unreadable(why.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::JobTracker;
    use crate::testutil::{git, git_available, make_repo, TempDir};
    use fleet_app::ports::{Vcs, VerifyOrigin, VerifyTarget};
    use fleet_domain::env::EnvPolicy;
    use fleet_domain::ids::{GoalId, RunId, Sha, TaskId};
    use fleet_domain::redact::Redactor;
    use fleet_domain::value::Value;
    use neo_verify::contract::VerificationContract;
    use std::sync::Arc;

    macro_rules! need_git {
        () => {
            if !git_available() {
                return;
            }
        };
    }

    fn token() -> String {
        format!("ghp_{}", "a1B2".repeat(10))
    }

    fn new_git() -> GitVcs {
        let env = vec![(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        )];
        GitVcs::new(env, EnvPolicy::default_policy())
    }

    /// A repository with a fleet worktree of task `r1/t1`, a scratch directory and a job context
    /// whose verification host exports into it.
    struct Fx {
        dir: TempDir,
        wt: PathBuf,
        vcs: GitVcs,
        ctx: JobContext,
        scratch: PathBuf,
    }

    impl Fx {
        fn new() -> Fx {
            let dir = TempDir::new("verify");
            let repo = make_repo(&dir);
            let vcs = new_git();
            let base = vcs
                .resolve_base(repo.to_str().unwrap(), "HEAD")
                .unwrap()
                .sha;
            let wt = PathBuf::from(
                vcs.prepare_worktree(
                    repo.to_str().unwrap(),
                    &RunId::parse("r1").unwrap(),
                    &TaskId::parse("t1").unwrap(),
                    "fleet/r1/t1",
                    &base,
                )
                .unwrap(),
            );
            let scratch = dir.join("scratch");
            let host = VerifyHost::new(new_git(), scratch.clone()).unwrap();
            let ctx = JobContext {
                redactor: Arc::new(Redactor::builtin()),
                tracker: JobTracker::new(),
                verify: Some(Arc::new(host)),
            };
            Fx {
                dir,
                wt,
                vcs,
                ctx,
                scratch,
            }
        }

        /// Writes files into the worktree and commits them.
        fn commit(&self, files: &[(&str, &str)]) {
            for (path, content) in files {
                let p = self.wt.join(path);
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(&p, content).unwrap();
            }
            git(&self.wt, &["add", "-A"]);
            git(&self.wt, &["commit", "-q", "-m", "work"]);
        }

        fn script(&self, name: &str, body: &str) -> String {
            self.dir
                .script(&format!("tools/{name}"), &format!("#!/bin/sh\n{body}\n"))
                .display()
                .to_string()
        }

        fn base_env(&self) -> Vec<(String, String)> {
            vec![
                ("PATH".into(), "/usr/bin:/bin".into()),
                (
                    "HOME".into(),
                    self.dir.join("real-home").display().to_string(),
                ),
                ("LANG".into(), "C".into()),
            ]
        }

        fn job(&self, contract: Value) -> VerifyJob {
            let contract = VerificationContract::from_value(&contract).unwrap();
            let base = Sha::parse(&git(&self.wt, &["rev-parse", "main"])).unwrap();
            VerifyJob {
                run: RunId::parse("r1").unwrap(),
                task: TaskId::parse("t1").unwrap(),
                attempt: 1,
                origin: VerifyOrigin::Review,
                worktree: self.wt.display().to_string(),
                state: self
                    .vcs
                    .worktree_state(self.wt.to_str().unwrap(), &base)
                    .unwrap(),
                env: self.base_env(),
                targets: vec![VerifyTarget {
                    goal: GoalId::parse("g").unwrap(),
                    contract_id: contract.id(&Sha256Digest),
                    contract,
                }],
            }
        }

        fn run(&self, contract: Value) -> Vec<CheckReport> {
            run_verify(&self.job(contract), &self.ctx)
                .results
                .remove(0)
                .reports
        }
    }

    fn doc(checks: Vec<Value>) -> Value {
        Value::object([("version", Value::int(1)), ("checks", Value::array(checks))])
    }

    fn cmd(id: &str, program: &str, args: &[&str], extra: Vec<(&str, Value)>) -> Value {
        let mut pairs = vec![
            ("id", Value::str(id)),
            ("kind", Value::str("command")),
            ("program", Value::str(program)),
            ("args", Value::array(args.iter().map(|a| Value::str(*a)))),
        ];
        pairs.extend(extra);
        Value::object(pairs)
    }

    fn file(id: &str, path: &str, assertion: &str, value: Option<&str>) -> Value {
        let mut pairs = vec![
            ("id", Value::str(id)),
            ("kind", Value::str("file")),
            ("path", Value::str(path)),
            ("assert", Value::str(assertion)),
        ];
        if let Some(v) = value {
            pairs.push(("value", Value::str(v)));
        }
        Value::object(pairs)
    }

    fn verdicts(reports: &[CheckReport]) -> Vec<Verdict> {
        reports.iter().map(|r| r.verdict).collect()
    }

    fn scratch_is_empty(fx: &Fx) -> bool {
        fs::read_dir(&fx.scratch).unwrap().next().is_none()
    }

    // ---- what is checked: the commit, not the worktree ---------------------------------------

    #[test]
    fn checks_see_the_committed_files_and_nothing_the_worktree_has_on_top() {
        need_git!();
        let fx = Fx::new();
        fx.commit(&[
            ("answer.txt", "ready\n"),
            ("scripts/run.sh", "#!/bin/sh\necho committed\n"),
        ]);
        // uncommitted, untracked and ignored state of the worktree
        fs::write(fx.wt.join("answer.txt"), "tampered after the commit\n").unwrap();
        fs::write(fx.wt.join("untracked.txt"), "u\n").unwrap();
        fs::write(fx.wt.join(".gitignore"), "ignored.log\n").unwrap();
        fs::write(fx.wt.join("ignored.log"), "i\n").unwrap();
        let reports = fx.run(doc(vec![
            file("committed", "answer.txt", "contains", Some("ready")),
            file("not-tampered", "answer.txt", "contains", Some("tampered")),
            file("untracked", "untracked.txt", "exists", None),
            file("ignored", "ignored.log", "exists", None),
        ]));
        assert_eq!(
            verdicts(&reports),
            [
                Verdict::Passed,
                Verdict::Failed,
                Verdict::Failed,
                Verdict::Failed
            ],
            "{reports:?}"
        );
    }

    #[test]
    fn a_makefile_or_script_that_only_exists_in_the_worktree_is_never_run() {
        need_git!();
        let fx = Fx::new();
        // committed: a script that calls a helper; the helper only exists, planted, in the worktree
        fx.commit(&[("test.sh", "#!/bin/sh\n./helper.sh\n")]);
        fs::set_permissions(fx.wt.join("test.sh"), PermissionsExt::from_mode(0o755)).unwrap();
        git(&fx.wt, &["add", "-A"]);
        git(&fx.wt, &["commit", "-q", "-m", "mode"]);
        let marker = fx.dir.join("planted-ran");
        fs::write(
            fx.wt.join("helper.sh"),
            format!("#!/bin/sh\ntouch {}\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(fx.wt.join("helper.sh"), PermissionsExt::from_mode(0o755)).unwrap();
        fs::write(
            fx.wt.join("Makefile"),
            format!("test:\n\ttouch {}\n", marker.display()),
        )
        .unwrap();
        let runner = fx.script("sh-c.sh", "cd \"$1\" 2>/dev/null; exec ./test.sh");
        let reports = fx.run(doc(vec![cmd("test", &runner, &["."], vec![])]));
        assert_eq!(reports[0].verdict, Verdict::Failed, "{reports:?}");
        assert!(!marker.exists(), "the planted helper ran");
    }

    #[test]
    fn commands_run_in_a_private_export_that_is_removed_afterwards() {
        need_git!();
        let fx = Fx::new();
        fx.commit(&[("data.txt", "x\n")]);
        let show = fx.script(
            "show.sh",
            "pwd; ls; echo \"HOME=$HOME TMPDIR=$TMPDIR\"; ls -A \"$HOME\" | wc -l",
        );
        let reports = fx.run(doc(vec![cmd("show", &show, &[], vec![])]));
        let out = &reports[0].output;
        assert!(out.contains("/scratch/job-"), "{out}");
        assert!(out.contains("/tree"), "{out}");
        assert!(
            out.contains("data.txt") && out.contains("README.md"),
            "{out}"
        );
        assert!(
            !out.contains(fx.wt.to_str().unwrap()),
            "the worktree is not where it runs: {out}"
        );
        assert!(
            out.contains("/home TMPDIR=") && out.trim_end().ends_with('0'),
            "an empty home: {out}"
        );
        assert!(scratch_is_empty(&fx), "the job directory is gone");
    }

    // ---- the environment ---------------------------------------------------------------------------

    #[test]
    fn the_environment_is_the_allowlist_minus_what_leads_to_the_operator_with_a_home_of_its_own() {
        need_git!();
        let fx = Fx::new();
        let show = fx.script("env.sh", "env | sort");
        let mut job = fx.job(doc(vec![cmd("env", &show, &[], vec![])]));
        job.env = vec![
            (
                "PATH".into(),
                "/usr/bin:/bin:relative/bin::/nonexistent/dir".into(),
            ),
            ("HOME".into(), "/home/operator".into()),
            ("LANG".into(), "C".into()),
            ("TMPDIR".into(), "/var/tmp/operator".into()),
            ("XDG_CONFIG_HOME".into(), "/home/operator/.config".into()),
            ("FLEET_SOCKET".into(), "/run/fleet.sock".into()),
            ("GIT_DIR".into(), "/home/operator/repo/.git".into()),
            ("CARGO_HOME".into(), "/opt/cargo".into()),
        ];
        let done = run_verify(&job, &fx.ctx);
        let out = done.results[0].reports[0].output.clone();
        let line = |name: &str| {
            out.lines()
                .find_map(|l| l.strip_prefix(&format!("{name}=")).map(str::to_string))
        };
        assert!(
            line("HOME").is_some_and(|h| h.contains("/scratch/job-") && h.ends_with("/home")),
            "{out}"
        );
        assert!(
            line("TMPDIR").is_some_and(|h| h.ends_with("/tmp") && h.contains("/scratch/")),
            "{out}"
        );
        assert!(!out.contains("/home/operator"), "{out}");
        assert_eq!(
            line("PATH").as_deref(),
            Some("/usr/bin:/bin"),
            "relative and missing entries are dropped"
        );
        assert_eq!(line("LANG").as_deref(), Some("C"));
        assert_eq!(
            line("CARGO_HOME").as_deref(),
            Some("/opt/cargo"),
            "what the operator allowlists passes"
        );
        assert!(line("XDG_STATE_HOME").is_some_and(|v| v.contains("/scratch/job-")));
        assert!(line("FLEET_SOCKET").is_none());
        assert_eq!(line("GIT_DIR"), None);
        assert_eq!(line("GIT_CONFIG_GLOBAL").as_deref(), Some("/dev/null"));
        assert_eq!(line("GIT_CONFIG_NOSYSTEM").as_deref(), Some("1"));
        assert!(line("GIT_CEILING_DIRECTORIES").is_some_and(|v| v.contains("/scratch/job-")));
    }

    #[test]
    fn a_path_entry_that_is_writable_by_others_or_lies_in_a_repository_is_not_trusted() {
        let dir = TempDir::new("verify-path");
        fs::create_dir_all(dir.join("repo/bin")).unwrap();
        fs::create_dir_all(dir.join("open")).unwrap();
        fs::create_dir_all(dir.join("safe")).unwrap();
        fs::set_permissions(dir.join("open"), PermissionsExt::from_mode(0o777)).unwrap();
        fs::set_permissions(dir.join("safe"), PermissionsExt::from_mode(0o755)).unwrap();
        fs::set_permissions(dir.join("repo/bin"), PermissionsExt::from_mode(0o755)).unwrap();
        let path = format!(
            "{}:{}:{}:relative:/nonexistent",
            dir.join("safe").display(),
            dir.join("open").display(),
            dir.join("repo/bin").display()
        );
        let untrusted = vec![fs::canonicalize(dir.join("repo")).unwrap()];
        assert_eq!(
            trusted_path(&path, &untrusted),
            vec![dir.join("safe").display().to_string()]
        );
        // with nothing left, the environment falls back to the system directories
        let env = isolated_env(
            &[("PATH".into(), "relative".into())],
            Path::new("/h"),
            Path::new("/t"),
            Path::new("/c"),
            &[],
        );
        assert!(env.contains(&("PATH".to_string(), "/usr/bin:/bin".to_string())));
    }

    // ---- commands -------------------------------------------------------------------------------------

    #[test]
    fn commands_are_judged_by_their_exit_code() {
        need_git!();
        let fx = Fx::new();
        let ok = fx.script("ok.sh", "echo fine");
        let bad = fx.script("bad.sh", "echo broken >&2; exit 3");
        let done = run_verify(
            &fx.job(doc(vec![
                cmd("ok", &ok, &[], vec![]),
                cmd("bad", &bad, &[], vec![]),
                cmd(
                    "expects-three",
                    &bad,
                    &[],
                    vec![("expect_exit", Value::int(3))],
                ),
            ])),
            &fx.ctx,
        );
        assert_eq!(done.commit.as_str(), git(&fx.wt, &["rev-parse", "HEAD"]));
        assert_eq!(
            done.tree.as_str(),
            git(&fx.wt, &["rev-parse", "HEAD^{tree}"])
        );
        assert!(!done.dirty);
        assert_eq!(
            (done.run.as_str(), done.task.as_str(), done.attempt),
            ("r1", "t1", 1)
        );
        let reports = &done.results[0].reports;
        assert_eq!(
            verdicts(reports),
            [Verdict::Passed, Verdict::Failed, Verdict::Passed],
            "{reports:?}"
        );
        assert_eq!(reports[0].exit_code, Some(0));
        assert!(reports[0].output.contains("fine"));
        assert_eq!(reports[1].exit_code, Some(3));
        assert!(reports[1].output.contains("broken"));
        assert_eq!(reports[1].summary, "exit code 3, expected 0");
        let ids: Vec<&str> = reports.iter().map(|r| r.check.as_str()).collect();
        assert_eq!(ids, ["ok", "bad", "expects-three"]);
    }

    #[test]
    fn a_program_must_be_absolute_and_outside_the_worktree_the_repository_and_the_export() {
        need_git!();
        let fx = Fx::new();
        fx.commit(&[("tool.sh", "#!/bin/sh\necho committed tool\n")]);
        fs::set_permissions(fx.wt.join("tool.sh"), PermissionsExt::from_mode(0o755)).unwrap();
        let in_worktree = fx.wt.join("tool.sh").display().to_string();
        let repo = fx
            .wt
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        fs::write(repo.join("in-repo.sh"), "#!/bin/sh\ntouch ran\n").unwrap();
        fs::set_permissions(repo.join("in-repo.sh"), PermissionsExt::from_mode(0o755)).unwrap();
        let reports = fx.run(doc(vec![
            cmd("in-worktree", &in_worktree, &[], vec![]),
            cmd(
                "in-repo",
                &repo.join("in-repo.sh").display().to_string(),
                &[],
                vec![],
            ),
            cmd("missing", "/nonexistent/tool", &[], vec![]),
        ]));
        assert_eq!(verdicts(&reports), [Verdict::Error; 3], "{reports:?}");
        assert!(
            reports[0].summary.contains("refused"),
            "{}",
            reports[0].summary
        );
        assert!(reports[1].summary.contains("refused"));
        assert!(reports[2].summary.contains("could not start"));
        assert!(!repo.join("ran").exists() && !fx.wt.join("ran").exists());
    }

    #[test]
    fn a_slow_command_is_stopped_with_its_whole_process_group() {
        need_git!();
        let fx = Fx::new();
        let marker = fx.dir.join("child-survived");
        let slow = fx.script(
            "slow.sh",
            &format!("(sleep 3; touch {}) &\nsleep 30", marker.display()),
        );
        let started = Instant::now();
        let reports = fx.run(doc(vec![cmd(
            "slow",
            &slow,
            &[],
            vec![("timeout_ms", Value::int(300))],
        )]));
        assert_eq!(reports[0].verdict, Verdict::Error);
        assert_eq!(reports[0].summary, "timed out");
        assert!(started.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(3500));
        assert!(!marker.exists(), "the grandchild was killed with the group");
        assert!(scratch_is_empty(&fx));
    }

    #[test]
    fn a_flood_of_output_is_stopped_and_bounded() {
        need_git!();
        let fx = Fx::new();
        let flood = fx.script("flood.sh", "yes x");
        let reports = fx.run(doc(vec![cmd("flood", &flood, &[], vec![])]));
        assert_eq!(reports[0].verdict, Verdict::Error);
        assert!(reports[0].summary.contains("more output than allowed"));
        assert!(reports[0].output.len() < 70_000);
    }

    #[test]
    fn output_and_summaries_are_redacted_before_they_are_reported() {
        need_git!();
        let fx = Fx::new();
        let leaky = fx.script(
            "leaky.sh",
            "echo \"token=$1\"; echo \"again $1\" >&2; exit 1",
        );
        let reports = fx.run(doc(vec![cmd("leaky", &leaky, &[&token()], vec![])]));
        let r = &reports[0];
        assert!(
            !r.output.contains(&token()) && !r.summary.contains(&token()),
            "{r:?}"
        );
        assert!(r.output.contains("[REDACTED:github-token]"), "{r:?}");
        let esc = fx.script("esc.sh", "printf '\\033]0;owned\\007ok\\n'");
        let job = fx.job(doc(vec![cmd("esc", &esc, &[], vec![])]));
        let evidence_text = run_verify(&job, &fx.ctx).results[0].reports[0]
            .output
            .clone();
        assert!(evidence_text.contains("ok"));
        let e = neo_verify::evidence::Evidence::from_parts(
            fleet_domain::ids::EvidenceId::from_seq(1),
            GoalId::parse("g").unwrap(),
            RunId::parse("r1").unwrap(),
            TaskId::parse("t1").unwrap(),
            job.targets[0].contract_id.clone(),
            job.targets[0].contract.checks()[0].id.clone(),
            Verdict::Passed,
            None,
            "s".into(),
            evidence_text,
            job.state.head.clone(),
            job.state.tree.clone(),
            false,
            1,
        );
        assert!(!e.output().contains('\u{1b}') && !e.output().contains('\u{7}'));
    }

    #[test]
    fn refcheck_json_uses_the_same_judgement_as_the_gate() {
        need_git!();
        let fx = Fx::new();
        let clean = fx.script(
            "clean.sh",
            "echo '{\"version\":1,\"summary\":{\"errors\":0},\"findings\":[]}'",
        );
        let dirty = fx.script(
            "dirty.sh",
            "echo '{\"version\":1,\"summary\":{\"errors\":2},\"findings\":[]}'; exit 1",
        );
        let junk = fx.script("junk.sh", "echo not json");
        let refcheck = |id: &str, program: &str| {
            Value::object([
                ("id", Value::str(id)),
                ("kind", Value::str("refcheck-json")),
                ("program", Value::str(program)),
            ])
        };
        let reports = fx.run(doc(vec![
            refcheck("clean", &clean),
            refcheck("dirty", &dirty),
            refcheck("junk", &junk),
        ]));
        assert_eq!(
            verdicts(&reports),
            [Verdict::Passed, Verdict::Failed, Verdict::Failed],
            "{reports:?}"
        );
    }

    // ---- files and the diff ------------------------------------------------------------------------------

    #[test]
    fn file_assertions_read_the_export_without_following_links() {
        need_git!();
        let fx = Fx::new();
        fx.commit(&[("a.txt", "hello world\n"), ("sub/b.bin", "x")]);
        fs::write(fx.wt.join("binary"), [0xff, 0xfe, 0x00]).unwrap();
        fs::write(fx.wt.join("big"), vec![b'a'; (MAX_FILE_BYTES + 1) as usize]).unwrap();
        std::os::unix::fs::symlink("a.txt", fx.wt.join("link.txt")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", fx.wt.join("evil-link")).unwrap();
        git(&fx.wt, &["add", "-A"]);
        git(&fx.wt, &["commit", "-q", "-m", "more"]);
        let sha_of_a = Sha256Digest.sha256_hex(b"hello world\n");
        let reports = fx.run(doc(vec![
            file("exists", "a.txt", "exists", None),
            file("nested", "sub/b.bin", "exists", None),
            file("missing", "nope.txt", "exists", None),
            file("dir", "sub", "exists", None),
            file("link", "link.txt", "exists", None),
            file("evil-link", "evil-link", "contains", Some("root")),
            file("sha-ok", "a.txt", "sha256", Some(&sha_of_a)),
            file("sha-bad", "a.txt", "sha256", Some(&"0".repeat(64))),
            file("has", "a.txt", "contains", Some("lo wor")),
            file("has-not", "a.txt", "contains", Some("goodbye")),
            file("binary", "binary", "contains", Some("x")),
            file("big-hash", "big", "sha256", Some(&"0".repeat(64))),
            file("big-exists", "big", "exists", None),
        ]));
        let by_id = |id: &str| {
            reports
                .iter()
                .find(|r| r.check.as_str() == id)
                .unwrap()
                .verdict
        };
        for (id, want) in [
            ("exists", Verdict::Passed),
            ("nested", Verdict::Passed),
            ("missing", Verdict::Failed),
            ("dir", Verdict::Failed),
            ("link", Verdict::Failed),
            ("evil-link", Verdict::Failed),
            ("sha-ok", Verdict::Passed),
            ("sha-bad", Verdict::Failed),
            ("has", Verdict::Passed),
            ("has-not", Verdict::Failed),
            ("binary", Verdict::Error),
            ("big-hash", Verdict::Error),
            ("big-exists", Verdict::Passed),
        ] {
            assert_eq!(by_id(id), want, "{id}: {reports:?}");
        }
    }

    #[test]
    fn the_diff_constraint_judges_the_paths_of_the_commits_the_job_carries() {
        need_git!();
        let fx = Fx::new();
        fx.commit(&[("src/a.rs", "a\n"), ("src/x/y.rs", "y\n")]);
        let allow = |globs: &[&str]| {
            doc(vec![Value::object([
                ("id", Value::str("scope")),
                ("kind", Value::str("diff")),
                ("allow", Value::array(globs.iter().map(|g| Value::str(*g)))),
            ])])
        };
        assert_eq!(fx.run(allow(&["src/**"]))[0].verdict, Verdict::Passed);
        let r = &fx.run(allow(&["src/a.rs"]))[0];
        assert_eq!(r.verdict, Verdict::Failed);
        assert!(r.output.contains("src/x/y.rs"), "{}", r.output);
        let mut cut = fx.job(allow(&["src/**"]));
        cut.state.truncated = true;
        assert_eq!(
            run_verify(&cut, &fx.ctx).results[0].reports[0].verdict,
            Verdict::Error
        );
    }

    // ---- when it cannot run -------------------------------------------------------------------------------

    #[test]
    fn a_commit_that_cannot_be_exported_fails_every_check_and_runs_nothing() {
        need_git!();
        let fx = Fx::new();
        let marker = fx.dir.join("ran");
        let touch = fx.script("touch.sh", &format!("touch {}", marker.display()));
        let mut job = fx.job(doc(vec![
            cmd("a", &touch, &[], vec![]),
            file("b", "README.md", "exists", None),
        ]));
        job.state.head = Sha::parse(&"1".repeat(40)).unwrap();
        let done = run_verify(&job, &fx.ctx);
        assert!(done.results[0]
            .reports
            .iter()
            .all(|r| r.verdict == Verdict::Error));
        assert!(done.results[0].reports[0]
            .summary
            .contains("could not export commit"));
        assert!(!marker.exists());
        // a tree that is not the one the job was started with
        let mut job = fx.job(doc(vec![file("b", "README.md", "exists", None)]));
        job.state.tree = Sha::parse(&"2".repeat(40)).unwrap();
        let done = run_verify(&job, &fx.ctx);
        assert!(done.results[0].reports[0]
            .summary
            .contains("tree of the commit"));
        assert!(scratch_is_empty(&fx));
    }

    #[test]
    fn without_a_verification_host_nothing_runs() {
        need_git!();
        let fx = Fx::new();
        let marker = fx.dir.join("ran");
        let touch = fx.script("touch.sh", &format!("touch {}", marker.display()));
        let job = fx.job(doc(vec![cmd("a", &touch, &[], vec![])]));
        let ctx = JobContext {
            verify: None,
            ..fx.ctx.clone()
        };
        let done = run_verify(&job, &ctx);
        assert_eq!(done.results[0].reports[0].verdict, Verdict::Error);
        assert!(done.results[0].reports[0]
            .summary
            .contains("not configured"));
        assert!(!marker.exists());
    }

    #[test]
    fn checks_after_the_time_budget_are_not_run() {
        need_git!();
        let fx = Fx::new();
        let marker = fx.dir.join("ran");
        let touch = fx.script("touch.sh", &format!("touch {}", marker.display()));
        let job = fx.job(doc(vec![cmd("late", &touch, &[], vec![])]));
        let done = run_verify_within(&job, &fx.ctx, Duration::ZERO);
        let r = &done.results[0].reports[0];
        assert_eq!(r.verdict, Verdict::Error);
        assert!(r.summary.contains("time budget"), "{}", r.summary);
        assert!(!marker.exists());
    }

    #[test]
    fn a_failed_run_reports_every_check_as_an_error() {
        need_git!();
        let fx = Fx::new();
        let job = fx.job(doc(vec![
            file("a", "a", "exists", None),
            file("b", "b", "exists", None),
        ]));
        let done = failed_run(&job, "boom");
        assert_eq!(done.results[0].reports.len(), 2);
        assert!(done.results[0]
            .reports
            .iter()
            .all(|r| r.verdict == Verdict::Error && r.summary == "boom"));
    }

    #[test]
    fn several_goals_run_in_order_in_one_export() {
        need_git!();
        let fx = Fx::new();
        let mut job = fx.job(doc(vec![file("a", "nope", "exists", None)]));
        let second =
            VerificationContract::from_value(&doc(vec![file("b", "README.md", "exists", None)]))
                .unwrap();
        job.targets.push(VerifyTarget {
            goal: GoalId::parse("h").unwrap(),
            contract_id: second.id(&Sha256Digest),
            contract: second,
        });
        let done = run_verify(&job, &fx.ctx);
        let goals: Vec<&str> = done.results.iter().map(|r| r.goal.as_str()).collect();
        assert_eq!(goals, ["g", "h"]);
        assert_eq!(done.results[0].reports[0].verdict, Verdict::Failed);
        assert_eq!(done.results[1].reports[0].verdict, Verdict::Passed);
    }

    #[test]
    fn the_host_clears_what_an_earlier_daemon_left_behind() {
        let dir = TempDir::new("verify-host");
        let scratch = dir.join("scratch");
        fs::create_dir_all(scratch.join("job-1-2-3/tree/sub")).unwrap();
        fs::write(scratch.join("job-1-2-3/tree/sub/file"), "x").unwrap();
        fs::set_permissions(
            scratch.join("job-1-2-3/tree/sub"),
            PermissionsExt::from_mode(0o500),
        )
        .unwrap();
        fs::write(scratch.join("keep"), "x").unwrap();
        let _host = VerifyHost::new(new_git(), scratch.clone()).unwrap();
        assert!(!scratch.join("job-1-2-3").exists());
        assert!(scratch.join("keep").exists());
        assert_eq!(
            fs::metadata(&scratch).unwrap().permissions().mode() & 0o077,
            0
        );
    }

    #[test]
    fn removing_a_tree_makes_read_only_directories_writable_and_never_follows_links() {
        let t = TempDir::new("verify-remove");
        let outside = t.join("outside");
        fs::create_dir_all(outside.join("inner")).unwrap();
        fs::write(outside.join("inner/keep.txt"), "keep").unwrap();
        fs::set_permissions(&outside, PermissionsExt::from_mode(0o500)).unwrap();
        fs::set_permissions(outside.join("inner"), PermissionsExt::from_mode(0o500)).unwrap();

        let job = t.join("job-1");
        fs::create_dir_all(job.join("tree/a/b/c")).unwrap();
        fs::create_dir_all(job.join("tree/closed/deeper")).unwrap();
        fs::write(job.join("tree/a/b/c/file"), "x").unwrap();
        fs::write(job.join("tree/closed/deeper/file"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, job.join("tree/a/link")).unwrap();
        std::os::unix::fs::symlink(outside.join("inner"), job.join("tree/a/b/link")).unwrap();
        // what a build tool leaves behind: read-only directories, one nobody can open
        for d in ["tree/a/b/c", "tree/a/b", "tree/a", "tree/closed/deeper"] {
            fs::set_permissions(job.join(d), PermissionsExt::from_mode(0o500)).unwrap();
        }
        fs::set_permissions(job.join("tree/closed"), PermissionsExt::from_mode(0o000)).unwrap();

        remove_tree(&job);
        assert!(!job.exists(), "the job directory is gone");
        // nothing behind the links was touched or removed
        let mode = |p: PathBuf| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(outside.clone()), 0o500);
        assert_eq!(mode(outside.join("inner")), 0o500);
        assert_eq!(
            fs::read_to_string(outside.join("inner/keep.txt")).unwrap(),
            "keep"
        );

        // a link in place of the job directory is not followed and not removed through
        let target = t.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::set_permissions(&target, PermissionsExt::from_mode(0o500)).unwrap();
        let link = t.join("job-2");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        remove_tree(&link);
        assert_eq!(mode(target.clone()), 0o500, "the link target was chmodded");
        assert!(target.exists());
        fs::set_permissions(&outside, PermissionsExt::from_mode(0o700)).unwrap();
        fs::set_permissions(outside.join("inner"), PermissionsExt::from_mode(0o700)).unwrap();
        fs::set_permissions(&target, PermissionsExt::from_mode(0o700)).unwrap();
    }
}
