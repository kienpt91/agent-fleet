//! Running verification contracts: the adapter side of `neo-verify`.
//!
//! This module observes (starts a process, reads a file, reports what git found) and the domain
//! judges (`neo_verify::judge`). Commands run exactly like approved commands and quality gates: no
//! shell, an absolute program path that is not inside the task's worktree or its repository
//! ([`trusted_program`]), the allowlisted environment the job carries and nothing else, no stdin,
//! their own process group, a timeout and a cap on captured output. Everything that is reported
//! back is redacted first. Files are opened below the worktree root without following any
//! symlink ([`open_beneath`]).

use crate::digest::Sha256Digest;
use crate::fsx::{open_beneath, read_open, ReadError};
use crate::jobs::{
    redact_block, redacted_tail, run_bounded_tracked, trusted_program, JobContext, RunSpec,
};
use crate::json;
use fleet_app::ports::{VerifiedContract, VerifyDone, VerifyJob};
use fleet_domain::approval::Digest;
use fleet_domain::limits::MAX_GATE_OUTPUT;
use fleet_domain::quality::ExitKind;
use fleet_domain::status::Verdict;
use neo_verify::contract::{Check, CheckKind, FileAssertion};
use neo_verify::judge::{judge, CheckReport, FileObservation, Observation};
use neo_verify::limits::MAX_FILE_BYTES;
use std::path::Path;
use std::time::{Duration, Instant};

/// All checks of one job together may take this long; checks that would start after it are
/// reported as errors instead of being run.
const JOB_BUDGET: Duration = Duration::from_secs(60 * 60);

/// Runs every check of every target of `job`, in order, and reports on all of them (also after a
/// failure: the retry brief lists every failing check).
pub fn run_verify(job: &VerifyJob, ctx: &JobContext) -> VerifyDone {
    run_verify_within(job, ctx, JOB_BUDGET)
}

pub(crate) fn run_verify_within(job: &VerifyJob, ctx: &JobContext, budget: Duration) -> VerifyDone {
    let started = Instant::now();
    let dir = Path::new(&job.worktree);
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
                    run_check(check, job, dir, ctx, left)
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

/// The report of a job whose thread panicked: every check is an error, never silence.
pub(crate) fn panic_result(job: &VerifyJob, why: &str) -> VerifyDone {
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

fn run_check(
    check: &Check,
    job: &VerifyJob,
    dir: &Path,
    ctx: &JobContext,
    left: Duration,
) -> CheckReport {
    let observed = if left.is_zero() {
        Observation::Failed("skipped: the time budget of this verification is spent".to_string())
    } else {
        observe(check, job, dir, ctx, left)
    };
    let mut report = judge(check, &observed);
    // Whatever was printed, and any text that quotes it, leaves this module redacted.
    report.summary = redact_block(&ctx.redactor, &report.summary);
    report.output = redact_block(&ctx.redactor, &report.output);
    report
}

fn observe(
    check: &Check,
    job: &VerifyJob,
    dir: &Path,
    ctx: &JobContext,
    left: Duration,
) -> Observation {
    match &check.kind {
        CheckKind::Command {
            program,
            args,
            timeout_ms,
            ..
        } => run_process(program, args, *timeout_ms, false, job, dir, ctx, left),
        CheckKind::RefcheckJson {
            program,
            args,
            timeout_ms,
        } => run_process(program, args, *timeout_ms, true, job, dir, ctx, left),
        CheckKind::File { path, assertion } => {
            Observation::File(observe_file(dir, path, assertion))
        }
        CheckKind::DiffOnly { .. } => Observation::Changed {
            paths: job.state.changed.clone(),
            truncated: job.state.truncated,
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn run_process(
    program: &str,
    args: &[String],
    timeout_ms: u64,
    structured: bool,
    job: &VerifyJob,
    dir: &Path,
    ctx: &JobContext,
    left: Duration,
) -> Observation {
    let program = match trusted_program(Path::new(program), dir) {
        Ok(p) => p,
        Err(why) => {
            let why = why.strip_prefix("could not start: ").unwrap_or(&why);
            return Observation::Process {
                exit: ExitKind::SpawnFailed(why.to_string()),
                output: String::new(),
                stdout_json: None,
            };
        }
    };
    let allowed = u64::try_from(left.as_millis()).unwrap_or(u64::MAX);
    let result = run_bounded_tracked(
        &RunSpec {
            program: program.display().to_string(),
            args: args.to_vec(),
            cwd: dir.to_path_buf(),
            env: job.env.clone(),
            timeout_ms: timeout_ms.min(allowed),
            max_output: MAX_GATE_OUTPUT,
        },
        &ctx.tracker,
    );
    // The whole capture is redacted before it is cut to its tail and judged.
    let output = redacted_tail(&result, &ctx.redactor);
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
    use crate::testutil::TempDir;
    use fleet_app::ports::{VerifyOrigin, VerifyTarget, WorktreeState};
    use fleet_domain::ids::{GoalId, RunId, Sha, TaskId};
    use fleet_domain::redact::Redactor;
    use fleet_domain::value::Value;
    use neo_verify::contract::VerificationContract;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn rd() -> JobContext {
        JobContext {
            redactor: Arc::new(Redactor::builtin()),
            tracker: JobTracker::new(),
        }
    }

    fn token() -> String {
        format!("ghp_{}", "a1B2".repeat(10))
    }

    struct Fx {
        dir: TempDir,
        worktree: PathBuf,
    }

    impl Fx {
        fn new() -> Fx {
            let dir = TempDir::new("verify");
            dir.write("repo/.fleet-worktrees/r1/t1/.keep", "");
            let worktree = dir.join("repo/.fleet-worktrees/r1/t1");
            Fx { dir, worktree }
        }

        fn script(&self, name: &str, body: &str) -> String {
            self.dir
                .script(&format!("tools/{name}"), &format!("#!/bin/sh\n{body}\n"))
                .display()
                .to_string()
        }

        fn job(&self, contract: Value, changed: &[&str]) -> VerifyJob {
            let contract = VerificationContract::from_value(&contract).unwrap();
            VerifyJob {
                run: RunId::parse("r1").unwrap(),
                task: TaskId::parse("t1").unwrap(),
                attempt: 1,
                origin: VerifyOrigin::Review,
                worktree: self.worktree.display().to_string(),
                state: WorktreeState {
                    head: Sha::parse(&"e".repeat(40)).unwrap(),
                    tree: Sha::parse(&"f".repeat(40)).unwrap(),
                    changed: changed.iter().map(|c| c.to_string()).collect(),
                    truncated: false,
                    dirty: false,
                },
                env: vec![("PATH".into(), "/usr/bin:/bin".into())],
                targets: vec![VerifyTarget {
                    goal: GoalId::parse("g").unwrap(),
                    contract_id: contract.id(&Sha256Digest),
                    contract,
                }],
            }
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

    fn run(job: &VerifyJob) -> Vec<CheckReport> {
        run_verify(job, &rd()).results.remove(0).reports
    }

    fn verdicts(reports: &[CheckReport]) -> Vec<Verdict> {
        reports.iter().map(|r| r.verdict).collect()
    }

    #[test]
    fn commands_are_judged_by_their_exit_code() {
        let fx = Fx::new();
        let ok = fx.script("ok.sh", "echo fine");
        let bad = fx.script("bad.sh", "echo broken >&2; exit 3");
        let job = fx.job(
            doc(vec![
                cmd("ok", &ok, &[], vec![]),
                cmd("bad", &bad, &[], vec![]),
                cmd(
                    "expects-three",
                    &bad,
                    &[],
                    vec![("expect_exit", Value::int(3))],
                ),
            ]),
            &[],
        );
        let done = run_verify(&job, &rd());
        assert_eq!(done.commit.as_str(), "e".repeat(40));
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
        // every check is run and reported, in contract order
        let ids: Vec<&str> = reports.iter().map(|r| r.check.as_str()).collect();
        assert_eq!(ids, ["ok", "bad", "expects-three"]);
    }

    #[test]
    fn commands_run_in_the_worktree_with_exactly_the_given_environment() {
        let fx = Fx::new();
        let show = fx.script("show.sh", "pwd; echo \"HOME=${HOME-unset} SECRET=${SECRET_TOKEN-unset} FLEET=${FLEET_SOCKET-unset}\"; env | wc -l");
        let mut job = fx.job(doc(vec![cmd("show", &show, &[], vec![])]), &[]);
        job.env = vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("FLEET_TASK".into(), "t1".into()),
        ];
        // the child starts from an empty environment: even what the test process itself has
        // (HOME, for one) is not passed on unless the job lists it
        let reports = run(&job);
        let out = &reports[0].output;
        assert!(out.contains(&fx.worktree.display().to_string()), "{out}");
        assert!(out.contains("HOME=unset SECRET=unset FLEET=unset"), "{out}");
        assert!(!out.contains("daemon-secret"));
    }

    #[test]
    fn a_program_must_be_absolute_and_outside_the_worktree_and_repository() {
        let fx = Fx::new();
        fx.dir.script(
            "repo/.fleet-worktrees/r1/t1/evil.sh",
            "#!/bin/sh\ntouch ../../../../ran\n",
        );
        fx.dir
            .script("repo/tools/also-evil.sh", "#!/bin/sh\ntouch ../ran\n");
        let in_worktree = fx.worktree.join("evil.sh").display().to_string();
        let in_repo = fx.dir.join("repo/tools/also-evil.sh").display().to_string();
        let job = fx.job(
            doc(vec![
                cmd("in-worktree", &in_worktree, &[], vec![]),
                cmd("in-repo", &in_repo, &[], vec![]),
                cmd("missing", "/nonexistent/tool", &[], vec![]),
            ]),
            &[],
        );
        let reports = run(&job);
        assert_eq!(verdicts(&reports), [Verdict::Error; 3]);
        assert!(
            reports[0].summary.contains("refused"),
            "{}",
            reports[0].summary
        );
        assert!(reports[1].summary.contains("refused"));
        assert!(reports[2].summary.contains("could not start"));
        assert!(!fx.dir.exists("ran") && !fx.dir.exists("repo/ran"));
    }

    #[test]
    fn a_slow_command_is_stopped_with_its_whole_process_group() {
        let fx = Fx::new();
        let marker = fx.dir.join("child-survived");
        let slow = fx.script(
            "slow.sh",
            &format!("(sleep 3; touch {}) &\nsleep 30", marker.display()),
        );
        let job = fx.job(
            doc(vec![cmd(
                "slow",
                &slow,
                &[],
                vec![("timeout_ms", Value::int(300))],
            )]),
            &[],
        );
        let started = Instant::now();
        let reports = run(&job);
        assert_eq!(reports[0].verdict, Verdict::Error);
        assert_eq!(reports[0].summary, "timed out");
        assert!(started.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(3500));
        assert!(!marker.exists(), "the grandchild was killed with the group");
    }

    #[test]
    fn a_flood_of_output_is_stopped_and_bounded() {
        let fx = Fx::new();
        let flood = fx.script("flood.sh", "yes x");
        let job = fx.job(doc(vec![cmd("flood", &flood, &[], vec![])]), &[]);
        let reports = run(&job);
        assert_eq!(reports[0].verdict, Verdict::Error);
        assert!(reports[0].summary.contains("more output than allowed"));
        assert!(reports[0].output.len() < 70_000);
    }

    #[test]
    fn output_and_summaries_are_redacted_before_they_are_reported() {
        let fx = Fx::new();
        let leaky = fx.script(
            "leaky.sh",
            "echo \"token=$1\"; echo \"again $1\" >&2; exit 1",
        );
        let job = fx.job(doc(vec![cmd("leaky", &leaky, &[&token()], vec![])]), &[]);
        let reports = run(&job);
        let r = &reports[0];
        assert!(
            !r.output.contains(&token()) && !r.summary.contains(&token()),
            "{r:?}"
        );
        assert!(r.output.contains("[REDACTED:github-token]"), "{r:?}");
        // and what is turned into evidence cannot carry terminal control sequences either
        let esc = fx.script("esc.sh", "printf '\\033]0;owned\\007ok\\n'");
        let job = fx.job(doc(vec![cmd("esc", &esc, &[], vec![])]), &[]);
        let evidence_text = &run(&job)[0].output;
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
            evidence_text.clone(),
            job.state.head.clone(),
            job.state.tree.clone(),
            false,
            1,
        );
        assert!(!e.output().contains('\u{1b}') && !e.output().contains('\u{7}'));
    }

    #[test]
    fn refcheck_json_uses_the_same_judgement_as_the_gate() {
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
        let job = fx.job(
            doc(vec![
                refcheck("clean", &clean),
                refcheck("dirty", &dirty),
                refcheck("junk", &junk),
            ]),
            &[],
        );
        let reports = run(&job);
        assert_eq!(
            verdicts(&reports),
            [Verdict::Passed, Verdict::Failed, Verdict::Failed],
            "{reports:?}"
        );
    }

    #[test]
    fn file_assertions_read_below_the_root_without_following_links() {
        let fx = Fx::new();
        fx.dir
            .write("repo/.fleet-worktrees/r1/t1/a.txt", "hello world\n");
        fx.dir.write("repo/.fleet-worktrees/r1/t1/sub/b.bin", "x");
        fx.dir.write("outside/secret.txt", "top secret\n");
        std::os::unix::fs::symlink(
            fx.dir.join("outside/secret.txt"),
            fx.worktree.join("link.txt"),
        )
        .unwrap();
        std::os::unix::fs::symlink(fx.dir.join("outside"), fx.worktree.join("linkdir")).unwrap();
        std::fs::write(fx.worktree.join("binary"), [0xff, 0xfe, 0x00]).unwrap();
        std::fs::write(
            fx.worktree.join("big"),
            vec![b'a'; (MAX_FILE_BYTES + 1) as usize],
        )
        .unwrap();
        let sha_of_a = Sha256Digest.sha256_hex(b"hello world\n");
        let job = fx.job(
            doc(vec![
                file("exists", "a.txt", "exists", None),
                file("nested", "sub/b.bin", "exists", None),
                file("missing", "nope.txt", "exists", None),
                file("dir", "sub", "exists", None),
                file("link", "link.txt", "exists", None),
                file(
                    "through-link",
                    "linkdir/secret.txt",
                    "contains",
                    Some("top secret"),
                ),
                file("sha-ok", "a.txt", "sha256", Some(&sha_of_a)),
                file("sha-bad", "a.txt", "sha256", Some(&"0".repeat(64))),
                file("has", "a.txt", "contains", Some("lo wor")),
                file("has-not", "a.txt", "contains", Some("goodbye")),
                file("binary", "binary", "contains", Some("x")),
                file("big-hash", "big", "sha256", Some(&"0".repeat(64))),
                file("big-exists", "big", "exists", None),
            ]),
            &[],
        );
        let reports = run(&job);
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
            ("through-link", Verdict::Failed),
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
    fn the_diff_constraint_judges_the_paths_the_job_carries() {
        let fx = Fx::new();
        let allow = |globs: &[&str]| {
            Value::object([
                ("id", Value::str("scope")),
                ("kind", Value::str("diff")),
                ("allow", Value::array(globs.iter().map(|g| Value::str(*g)))),
            ])
        };
        let ok = fx.job(doc(vec![allow(&["src/**"])]), &["src/a.rs", "src/x/y.rs"]);
        assert_eq!(run(&ok)[0].verdict, Verdict::Passed);
        let bad = fx.job(doc(vec![allow(&["src/**"])]), &["src/a.rs", "Cargo.toml"]);
        let r = &run(&bad)[0];
        assert_eq!(r.verdict, Verdict::Failed);
        assert!(r.output.contains("Cargo.toml"));
        let mut cut = fx.job(doc(vec![allow(&["src/**"])]), &["src/a.rs"]);
        cut.state.truncated = true;
        assert_eq!(run(&cut)[0].verdict, Verdict::Error);
    }

    #[test]
    fn checks_after_the_time_budget_are_not_run() {
        let fx = Fx::new();
        let touch = fx.script(
            "touch.sh",
            &format!("touch {}", fx.dir.join("ran").display()),
        );
        let job = fx.job(doc(vec![cmd("late", &touch, &[], vec![])]), &[]);
        let done = run_verify_within(&job, &rd(), Duration::ZERO);
        let r = &done.results[0].reports[0];
        assert_eq!(r.verdict, Verdict::Error);
        assert!(r.summary.contains("time budget"), "{}", r.summary);
        assert!(!fx.dir.exists("ran"));
    }

    #[test]
    fn a_panicked_job_reports_every_check_as_an_error() {
        let fx = Fx::new();
        let job = fx.job(
            doc(vec![
                file("a", "a", "exists", None),
                file("b", "b", "exists", None),
            ]),
            &[],
        );
        let done = panic_result(&job, "boom");
        assert_eq!(done.results[0].reports.len(), 2);
        assert!(done.results[0]
            .reports
            .iter()
            .all(|r| r.verdict == Verdict::Error && r.summary == "boom"));
    }

    #[test]
    fn several_goals_run_in_order() {
        let fx = Fx::new();
        let mut job = fx.job(doc(vec![file("a", "nope", "exists", None)]), &[]);
        let second =
            VerificationContract::from_value(&doc(vec![file("b", "nope", "exists", None)]))
                .unwrap();
        job.targets.push(VerifyTarget {
            goal: GoalId::parse("h").unwrap(),
            contract_id: second.id(&Sha256Digest),
            contract: second,
        });
        let done = run_verify(&job, &rd());
        let goals: Vec<&str> = done.results.iter().map(|r| r.goal.as_str()).collect();
        assert_eq!(goals, ["g", "h"]);
    }
}
