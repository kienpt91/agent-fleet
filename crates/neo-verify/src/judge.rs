//! Judging: turning what an adapter observed into a verdict. All the rules of "did this check
//! pass" are here and pure; running the process, reading the file or asking git is the adapter's
//! job and produces an [`Observation`].

use crate::contract::{Check, CheckId, CheckKind, FileAssertion};
use crate::limits::{MAX_CHANGED_PATHS, MAX_FILE_BYTES, MAX_PATHS_SHOWN};
use fleet_domain::ids::show;
use fleet_domain::quality::{evaluate_refcheck, ExitKind};
use fleet_domain::sanitize::flatten_line;
use fleet_domain::status::Verdict;
use fleet_domain::value::Value;

const SUMMARY_WIDTH: usize = 300;

/// What running one check came to, before it is recorded as evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckReport {
    pub check: CheckId,
    pub verdict: Verdict,
    pub exit_code: Option<i64>,
    pub summary: String,
    /// Redacted output of the check (bounded again when it becomes evidence).
    pub output: String,
}

/// What the adapter saw of one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileObservation {
    Missing,
    /// A directory, a symlink or something else that is not a regular file.
    NotRegular,
    /// Larger than the cap for reading; it was not read.
    TooLarge,
    Unreadable(String),
    /// A regular file is there (the answer for `exists`).
    Regular,
    /// Its SHA-256, lowercase hex (the answer for `sha256`).
    Hashed(String),
    /// Its content as UTF-8 text (the answer for `contains`).
    Text(String),
    /// Its content is not UTF-8.
    NotText,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Observation {
    /// A command ran. `stdout_json` is stdout parsed as one JSON document (`refcheck-json`).
    Process {
        exit: ExitKind,
        output: String,
        stdout_json: Option<Result<Value, String>>,
    },
    File(FileObservation),
    /// Paths the work changed, relative to the worktree; `truncated` when there were too many to
    /// list.
    Changed {
        paths: Vec<String>,
        truncated: bool,
    },
    /// The check could not be carried out at all (the adapter failed before observing anything).
    Failed(String),
}

fn report(
    check: &Check,
    verdict: Verdict,
    exit_code: Option<i64>,
    summary: &str,
    output: String,
) -> CheckReport {
    CheckReport {
        check: check.id.clone(),
        verdict,
        exit_code,
        summary: flatten_line(summary, SUMMARY_WIDTH),
        output,
    }
}

fn not_exited(exit: &ExitKind) -> Option<String> {
    match exit {
        ExitKind::Code(_) => None,
        ExitKind::Signal(n) => Some(format!("killed by signal {n}")),
        ExitKind::TimedOut => Some("timed out".to_string()),
        ExitKind::OutputLimit => {
            Some("printed more output than allowed and was stopped".to_string())
        }
        ExitKind::SpawnFailed(why) => Some(format!("could not start: {why}")),
    }
}

/// Judges one check against what was observed. An observation of the wrong shape, which only a
/// bug in an adapter can produce, is an `error`, never a pass.
pub fn judge(check: &Check, observed: &Observation) -> CheckReport {
    if let Observation::Failed(why) = observed {
        return report(
            check,
            Verdict::Error,
            None,
            &format!("could not run the check: {why}"),
            String::new(),
        );
    }
    match (&check.kind, observed) {
        (CheckKind::Command { expect_exit, .. }, Observation::Process { exit, output, .. }) => {
            if let Some(why) = not_exited(exit) {
                return report(check, Verdict::Error, None, &why, output.clone());
            }
            let code = match exit {
                ExitKind::Code(c) => *c,
                _ => -1,
            };
            let (verdict, summary) = if code == *expect_exit {
                (Verdict::Passed, format!("exit code {code}"))
            } else {
                (
                    Verdict::Failed,
                    format!("exit code {code}, expected {expect_exit}"),
                )
            };
            report(
                check,
                verdict,
                Some(i64::from(code)),
                &summary,
                output.clone(),
            )
        }
        (
            CheckKind::RefcheckJson { .. },
            Observation::Process {
                exit,
                output,
                stdout_json,
            },
        ) => {
            let parsed = match stdout_json {
                Some(Ok(v)) => Ok(v),
                Some(Err(why)) => Err(why.as_str()),
                None => Err("stdout was not captured"),
            };
            let outcome = evaluate_refcheck(check.id.as_str(), parsed, exit, output);
            let (verdict, code) = match exit {
                ExitKind::Code(c) => (
                    if outcome.passed {
                        Verdict::Passed
                    } else {
                        Verdict::Failed
                    },
                    Some(i64::from(*c)),
                ),
                _ => (Verdict::Error, None),
            };
            report(check, verdict, code, &outcome.summary, outcome.output)
        }
        (CheckKind::File { assertion, .. }, Observation::File(file)) => {
            judge_file(check, assertion, file)
        }
        (CheckKind::DiffOnly { allow, deny }, Observation::Changed { paths, truncated }) => {
            if *truncated || paths.len() > MAX_CHANGED_PATHS {
                return report(
                    check,
                    Verdict::Error,
                    None,
                    &format!("more than {MAX_CHANGED_PATHS} changed paths; the diff constraint cannot be judged"),
                    String::new(),
                );
            }
            // A denied path fails whatever `allow` says; without `allow` everything else is fine.
            let denied = |p: &str| deny.iter().any(|g| g.matches(p));
            let outside = |p: &str| !allow.is_empty() && !allow.iter().any(|g| g.matches(p));
            let offending: Vec<(&String, bool)> = paths
                .iter()
                .filter_map(|p| {
                    if denied(p) {
                        Some((p, true))
                    } else if outside(p) {
                        Some((p, false))
                    } else {
                        None
                    }
                })
                .collect();
            if offending.is_empty() {
                return report(
                    check,
                    Verdict::Passed,
                    None,
                    &format!("{} changed path(s), all allowed", paths.len()),
                    String::new(),
                );
            }
            let listed: Vec<String> = offending
                .iter()
                .take(MAX_PATHS_SHOWN)
                .map(|(p, is_denied)| {
                    if *is_denied {
                        format!("{} (denied)", show(p))
                    } else {
                        show(p)
                    }
                })
                .collect();
            let more = offending.len().saturating_sub(MAX_PATHS_SHOWN);
            let mut output = listed.join("\n");
            if more > 0 {
                output.push_str(&format!("\n... and {more} more"));
            }
            let n_denied = offending.iter().filter(|(_, d)| *d).count();
            let n_outside = offending.len().saturating_sub(n_denied);
            let summary = match (n_denied, n_outside) {
                (0, n) => format!("{n} changed path(s) outside the allowed globs"),
                (n, 0) => format!("{n} changed path(s) match a denied glob"),
                (d, n) => format!(
                    "{d} changed path(s) match a denied glob and {n} outside the allowed globs"
                ),
            };
            report(check, Verdict::Failed, None, &summary, output)
        }
        _ => report(
            check,
            Verdict::Error,
            None,
            "internal error: the observation does not fit the check",
            String::new(),
        ),
    }
}

fn judge_file(check: &Check, assertion: &FileAssertion, file: &FileObservation) -> CheckReport {
    let mk = |verdict: Verdict, summary: &str| report(check, verdict, None, summary, String::new());
    match (assertion, file) {
        (_, FileObservation::Missing) => mk(Verdict::Failed, "the file does not exist"),
        (_, FileObservation::NotRegular) => mk(Verdict::Failed, "the path is not a regular file"),
        (_, FileObservation::TooLarge) => mk(
            Verdict::Error,
            &format!("the file is larger than {MAX_FILE_BYTES} bytes and was not read"),
        ),
        (_, FileObservation::Unreadable(why)) => mk(
            Verdict::Error,
            &format!("the file could not be read: {why}"),
        ),
        (FileAssertion::Exists, FileObservation::Regular) => mk(Verdict::Passed, "the file exists"),
        (FileAssertion::Sha256(expected), FileObservation::Hashed(got)) => {
            if got == expected {
                mk(Verdict::Passed, "the file's sha256 matches")
            } else {
                mk(
                    Verdict::Failed,
                    &format!("the file's sha256 is {}, expected {expected}", show(got)),
                )
            }
        }
        (FileAssertion::Contains(needle), FileObservation::Text(text)) => {
            if text.contains(needle.as_str()) {
                mk(Verdict::Passed, "the file contains the text")
            } else {
                mk(Verdict::Failed, "the file does not contain the text")
            }
        }
        (FileAssertion::Contains(_), FileObservation::NotText) => {
            mk(Verdict::Error, "the file is not valid UTF-8 text")
        }
        _ => mk(
            Verdict::Error,
            "internal error: the observation does not fit the assertion",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::VerificationContract;
    use crate::glob::Glob;

    fn check(kind: CheckKind) -> Check {
        Check {
            id: CheckId::parse("c").unwrap(),
            mandatory: true,
            kind,
        }
    }

    fn command(expect: i32) -> Check {
        check(CheckKind::Command {
            program: "/bin/true".into(),
            args: vec![],
            expect_exit: expect,
            timeout_ms: 1000,
        })
    }

    fn process(exit: ExitKind) -> Observation {
        Observation::Process {
            exit,
            output: "out".into(),
            stdout_json: None,
        }
    }

    #[test]
    fn command_exit_codes() {
        let r = judge(&command(0), &process(ExitKind::Code(0)));
        assert_eq!((r.verdict, r.exit_code), (Verdict::Passed, Some(0)));
        assert_eq!(r.output, "out");
        let r = judge(&command(0), &process(ExitKind::Code(1)));
        assert_eq!((r.verdict, r.exit_code), (Verdict::Failed, Some(1)));
        assert_eq!(r.summary, "exit code 1, expected 0");
        let r = judge(&command(3), &process(ExitKind::Code(3)));
        assert_eq!(r.verdict, Verdict::Passed);
        let r = judge(&command(3), &process(ExitKind::Code(0)));
        assert_eq!(r.verdict, Verdict::Failed);
    }

    #[test]
    fn a_command_that_did_not_exit_is_an_error_not_a_failure() {
        for (exit, text) in [
            (ExitKind::Signal(9), "killed by signal 9"),
            (ExitKind::TimedOut, "timed out"),
            (ExitKind::OutputLimit, "more output than allowed"),
            (
                ExitKind::SpawnFailed("no such file".into()),
                "could not start: no such file",
            ),
        ] {
            let r = judge(&command(0), &process(exit));
            assert_eq!(r.verdict, Verdict::Error);
            assert_eq!(r.exit_code, None);
            assert!(r.summary.contains(text), "{}", r.summary);
        }
    }

    fn refcheck() -> Check {
        check(CheckKind::RefcheckJson {
            program: "/bin/true".into(),
            args: vec![],
            timeout_ms: 1000,
        })
    }

    fn doc(errors: i64) -> Value {
        Value::object([
            ("version", Value::int(1)),
            ("summary", Value::object([("errors", Value::int(errors))])),
            ("findings", Value::array([])),
        ])
    }

    fn refcheck_obs(exit: ExitKind, json: Option<Result<Value, String>>) -> Observation {
        Observation::Process {
            exit,
            output: "raw".into(),
            stdout_json: json,
        }
    }

    #[test]
    fn refcheck_reuses_the_gate_judgement() {
        let r = judge(
            &refcheck(),
            &refcheck_obs(ExitKind::Code(0), Some(Ok(doc(0)))),
        );
        assert_eq!(r.verdict, Verdict::Passed);
        let r = judge(
            &refcheck(),
            &refcheck_obs(ExitKind::Code(1), Some(Ok(doc(2)))),
        );
        assert_eq!((r.verdict, r.exit_code), (Verdict::Failed, Some(1)));
        let r = judge(
            &refcheck(),
            &refcheck_obs(ExitKind::Code(0), Some(Err("bad json".into()))),
        );
        assert_eq!(r.verdict, Verdict::Failed);
        let r = judge(&refcheck(), &refcheck_obs(ExitKind::Code(0), None));
        assert_eq!(r.verdict, Verdict::Failed);
        let r = judge(&refcheck(), &refcheck_obs(ExitKind::TimedOut, None));
        assert_eq!((r.verdict, r.exit_code), (Verdict::Error, None));
    }

    fn file(a: FileAssertion) -> Check {
        check(CheckKind::File {
            path: "f".into(),
            assertion: a,
        })
    }

    fn file_verdict(a: FileAssertion, o: FileObservation) -> Verdict {
        judge(&file(a), &Observation::File(o)).verdict
    }

    #[test]
    fn file_assertions() {
        use FileAssertion as A;
        use FileObservation as O;
        let hex = "ab".repeat(32);
        let table: Vec<(A, O, Verdict)> = vec![
            (A::Exists, O::Regular, Verdict::Passed),
            (A::Exists, O::Missing, Verdict::Failed),
            (A::Exists, O::NotRegular, Verdict::Failed),
            (A::Exists, O::Unreadable("denied".into()), Verdict::Error),
            (
                A::Sha256(hex.clone()),
                O::Hashed(hex.clone()),
                Verdict::Passed,
            ),
            (
                A::Sha256(hex.clone()),
                O::Hashed("cd".repeat(32)),
                Verdict::Failed,
            ),
            (A::Sha256(hex.clone()), O::TooLarge, Verdict::Error),
            (A::Sha256(hex.clone()), O::Missing, Verdict::Failed),
            (
                A::Contains("needle".into()),
                O::Text("a needle here".into()),
                Verdict::Passed,
            ),
            (
                A::Contains("needle".into()),
                O::Text("hay".into()),
                Verdict::Failed,
            ),
            (A::Contains("needle".into()), O::NotText, Verdict::Error),
            (A::Contains("needle".into()), O::TooLarge, Verdict::Error),
            // an adapter bug is an error, never a pass
            (A::Exists, O::Hashed(hex.clone()), Verdict::Error),
            (A::Contains("x".into()), O::Regular, Verdict::Error),
            (A::Sha256(hex), O::Text("x".into()), Verdict::Error),
        ];
        for (a, o, want) in table {
            assert_eq!(file_verdict(a.clone(), o.clone()), want, "{a:?} {o:?}");
        }
    }

    #[test]
    fn a_wrong_sha_is_quoted() {
        let r = judge(
            &file(FileAssertion::Sha256("ab".repeat(32))),
            &Observation::File(FileObservation::Hashed("\u{1b}[31m".into())),
        );
        assert!(!r.summary.contains('\u{1b}'), "{:?}", r.summary);
    }

    fn diff(globs: &[&str]) -> Check {
        diff_with(globs, &[])
    }

    fn diff_with(allow: &[&str], deny: &[&str]) -> Check {
        let globs = |items: &[&str]| items.iter().map(|g| Glob::new(g).unwrap()).collect();
        check(CheckKind::DiffOnly {
            allow: globs(allow),
            deny: globs(deny),
        })
    }

    fn changed(paths: &[&str], truncated: bool) -> Observation {
        Observation::Changed {
            paths: paths.iter().map(|p| p.to_string()).collect(),
            truncated,
        }
    }

    #[test]
    fn diff_constraint() {
        let c = diff(&["src/**", "*.md"]);
        let r = judge(
            &c,
            &changed(&["src/a.rs", "src/b/c.rs", "README.md"], false),
        );
        assert_eq!(r.verdict, Verdict::Passed);
        let r = judge(&c, &changed(&[], false));
        assert_eq!(r.verdict, Verdict::Passed, "no change is within any scope");
        let r = judge(
            &c,
            &changed(&["src/a.rs", "Cargo.toml", "docs/x.md"], false),
        );
        assert_eq!(r.verdict, Verdict::Failed);
        assert!(r.output.contains("\"Cargo.toml\""));
        assert!(r.output.contains("\"docs/x.md\""));
        assert!(!r.output.contains("src/a.rs"));
        let r = judge(&c, &changed(&["src/a.rs"], true));
        assert_eq!(r.verdict, Verdict::Error);
    }

    #[test]
    fn a_deny_list_alone_forbids_those_paths_and_nothing_else() {
        let c = diff_with(&[], &["Cargo.lock", "crates/*/Cargo.toml", "dogfood/**"]);
        let r = judge(
            &c,
            &changed(&["src/a.rs", "docs/x.md", "Cargo.toml"], false),
        );
        assert_eq!(r.verdict, Verdict::Passed, "{r:?}");
        assert_eq!(judge(&c, &changed(&[], false)).verdict, Verdict::Passed);
        for denied in [
            "Cargo.lock",
            "crates/neo-verify/Cargo.toml",
            "dogfood/plan.toml",
            "dogfood/a/b/c",
        ] {
            let r = judge(&c, &changed(&["src/a.rs", denied], false));
            assert_eq!(r.verdict, Verdict::Failed, "{denied}");
            assert!(
                r.output.contains(&format!("\"{denied}\" (denied)")),
                "{}",
                r.output
            );
            assert!(!r.output.contains("src/a.rs"));
            assert_eq!(r.summary, "1 changed path(s) match a denied glob");
        }
        // a glob is not a prefix: only what it says is denied
        let r = judge(
            &c,
            &changed(
                &["Cargo.lock.bak", "crates/Cargo.toml", "dogfood2/x"],
                false,
            ),
        );
        assert_eq!(r.verdict, Verdict::Passed, "{r:?}");
        // `dir/**` also matches a path that is just `dir` (a file where the directory was)
        let r = judge(&c, &changed(&["dogfood"], false));
        assert_eq!(r.verdict, Verdict::Failed, "{r:?}");
    }

    #[test]
    fn deny_wins_over_allow_and_both_kinds_of_offence_are_counted() {
        let c = diff_with(&["src/**"], &["src/secret/**", "src/lock.rs"]);
        let r = judge(&c, &changed(&["src/a.rs", "src/deep/b.rs"], false));
        assert_eq!(r.verdict, Verdict::Passed);
        // allowed by `allow`, forbidden by `deny`
        let r = judge(&c, &changed(&["src/secret/key.rs"], false));
        assert_eq!(r.verdict, Verdict::Failed);
        assert_eq!(r.summary, "1 changed path(s) match a denied glob");
        // outside `allow`: the old wording
        let r = judge(&c, &changed(&["docs/a.md"], false));
        assert_eq!(r.summary, "1 changed path(s) outside the allowed globs");
        assert!(!r.output.contains("(denied)"));
        // both at once
        let r = judge(
            &c,
            &changed(
                &["src/lock.rs", "src/secret/x", "docs/a.md", "src/ok.rs"],
                false,
            ),
        );
        assert_eq!(r.verdict, Verdict::Failed);
        assert_eq!(
            r.summary,
            "2 changed path(s) match a denied glob and 1 outside the allowed globs"
        );
        assert!(r.output.contains("\"src/lock.rs\" (denied)"));
        assert!(r.output.contains("\"docs/a.md\""));
        assert!(!r.output.contains("src/ok.rs"));
    }

    #[test]
    fn a_deny_glob_is_case_sensitive_like_every_glob() {
        let c = diff_with(&[], &["SECURITY.md"]);
        assert_eq!(
            judge(&c, &changed(&["security.md"], false)).verdict,
            Verdict::Passed,
            "case folding is the job of a separate check, not of the matcher"
        );
        assert_eq!(
            judge(&c, &changed(&["SECURITY.md"], false)).verdict,
            Verdict::Failed
        );
    }

    #[test]
    fn a_deny_only_check_is_not_fooled_by_a_truncated_or_oversized_diff() {
        let c = diff_with(&[], &["Cargo.lock"]);
        assert_eq!(
            judge(&c, &changed(&["a"], true)).verdict,
            Verdict::Error,
            "a diff that was cut could hide the denied path"
        );
        let too_many = Observation::Changed {
            paths: vec!["a".into(); MAX_CHANGED_PATHS + 1],
            truncated: false,
        };
        assert_eq!(judge(&c, &too_many).verdict, Verdict::Error);
    }

    #[test]
    fn the_diff_report_lists_a_bounded_number_of_paths_and_escapes_them() {
        let c = diff(&["src/**"]);
        let many: Vec<String> = (0..25).map(|i| format!("bad{i}")).collect();
        let obs = Observation::Changed {
            paths: many,
            truncated: false,
        };
        let r = judge(&c, &obs);
        assert_eq!(r.output.lines().count(), MAX_PATHS_SHOWN + 1);
        assert!(r.output.contains("and 15 more"));
        let hostile = Observation::Changed {
            paths: vec!["\u{1b}]52;c;AAAA\u{7}".into()],
            truncated: false,
        };
        let r = judge(&c, &hostile);
        assert!(!r.output.contains('\u{1b}') && !r.output.contains('\u{7}'));
        let too_many = Observation::Changed {
            paths: vec!["a".into(); MAX_CHANGED_PATHS + 1],
            truncated: false,
        };
        assert_eq!(judge(&c, &too_many).verdict, Verdict::Error);
    }

    #[test]
    fn adapter_failures_and_mismatched_observations_are_errors() {
        let r = judge(&command(0), &Observation::Failed("disk on fire".into()));
        assert_eq!(r.verdict, Verdict::Error);
        assert!(r.summary.contains("disk on fire"));
        assert_eq!(
            judge(&command(0), &changed(&[], false)).verdict,
            Verdict::Error
        );
        assert_eq!(
            judge(&diff(&["a"]), &process(ExitKind::Code(0))).verdict,
            Verdict::Error
        );
        assert_eq!(
            judge(&file(FileAssertion::Exists), &process(ExitKind::Code(0))).verdict,
            Verdict::Error
        );
    }

    #[test]
    fn summaries_are_single_line_and_bounded() {
        let r = judge(
            &command(0),
            &process(ExitKind::SpawnFailed(format!(
                "a\nb\u{1b}[2J{}",
                "x".repeat(2000)
            ))),
        );
        assert!(!r.summary.contains('\n') && !r.summary.contains('\u{1b}'));
        assert!(r.summary.len() <= SUMMARY_WIDTH + 4);
    }

    #[test]
    fn contracts_parsed_from_documents_judge_the_same_way() {
        let doc = crate::contract::tests::doc();
        let c = VerificationContract::from_value(&doc).unwrap();
        let scope = c.check(&CheckId::parse("scope").unwrap()).unwrap();
        assert_eq!(
            judge(scope, &changed(&["src/x.rs"], false)).verdict,
            Verdict::Passed
        );
    }
}
