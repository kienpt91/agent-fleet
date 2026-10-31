//! The worker contract on disk: `<worktree>/.fleet/{brief.md,result.json,question.json}`.
//!
//! The worker owns the worktree, so every path below it is untrusted: `.fleet` itself may be
//! replaced by a symlink and the files by FIFOs, links or huge files. Reads go through
//! `fsx::read_regular_in`, which handles all of those.

use crate::fsx::{self, FileKind, ReadError};
use crate::jobs::redact_block;
use crate::json;
use fleet_app::ports::ContractFiles;
use fleet_domain::contract::{ContractRead, WorkerQuestion, WorkerResult};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::limits::{MAX_BRIEF_FILE_BYTES, MAX_CONTRACT_FILE};
use fleet_domain::redact::Redactor;
use fleet_domain::sanitize::head_str;
use fleet_domain::value::Value;
use std::path::Path;
use std::sync::Arc;

const DIR_NAME: &str = ".fleet";
const RESULT_FILE: &str = "result.json";
const QUESTION_FILE: &str = "question.json";
const BRIEF_FILE: &str = "brief.md";

/// Reads and prepares the contract files. Free text a worker writes (result summary, question
/// text and choices) is redacted on the way in, so no secret reaches the log or other briefs.
#[derive(Clone, Debug)]
pub struct FsContractFiles {
    redactor: Arc<Redactor>,
}

impl FsContractFiles {
    pub fn new(redactor: Arc<Redactor>) -> FsContractFiles {
        FsContractFiles { redactor }
    }

    fn read<T>(
        &self,
        worktree: &str,
        name: &str,
        parse: fn(&Value) -> std::result::Result<T, String>,
    ) -> ContractRead<T> {
        let dir = Path::new(worktree).join(DIR_NAME);
        let bytes = match fsx::read_regular_in(&dir, name, MAX_CONTRACT_FILE) {
            Ok(b) => b,
            Err(ReadError::Missing) => return ContractRead::Absent,
            Err(e) => return ContractRead::Invalid(format!("{name}: {e}")),
        };
        let max = usize::try_from(MAX_CONTRACT_FILE).unwrap_or(usize::MAX);
        let value = match json::parse_value(&bytes, max) {
            Ok(v) => v,
            Err(text) => return ContractRead::Invalid(format!("{name}: {text}")),
        };
        match parse(&value) {
            Ok(v) => ContractRead::Valid(v),
            Err(text) => ContractRead::Invalid(format!("{name}: {text}")),
        }
    }
}

impl ContractFiles for FsContractFiles {
    fn prepare(&self, worktree: &str, brief: &str) -> Result<()> {
        let root = Path::new(worktree);
        if !fsx::is_real_dir(root) {
            return Err(FleetError::invalid(format!(
                "worktree {worktree} is not a directory"
            )));
        }
        let dir = root.join(DIR_NAME);
        // A symlink, file or FIFO named `.fleet` (left by a previous attempt) is unlinked, never
        // followed, so nothing outside the worktree is touched.
        if !matches!(fsx::lstat_kind(&dir), None | Some(FileKind::Dir)) {
            fsx::remove_nofollow(&dir)
                .map_err(|e| FleetError::io("cannot replace .fleet in the worktree", &e))?;
        }
        fsx::ensure_private_dir(&dir)?;

        for name in [RESULT_FILE, QUESTION_FILE] {
            let path = dir.join(name);
            match fsx::lstat_kind(&path) {
                None => {}
                Some(FileKind::Dir) => {
                    return Err(FleetError::invalid(format!(
                        ".fleet/{name} is a directory; remove it before the task can run"
                    )))
                }
                Some(_) => fsx::remove_nofollow(&path).map_err(|e| {
                    FleetError::io(&format!("cannot remove stale .fleet/{name}"), &e)
                })?,
            }
        }

        let brief = head_str(brief, MAX_BRIEF_FILE_BYTES);
        fsx::write_private_atomic(&dir.join(BRIEF_FILE), brief.as_bytes())
    }

    fn read_question(&self, worktree: &str) -> ContractRead<WorkerQuestion> {
        match self.read(worktree, QUESTION_FILE, WorkerQuestion::from_value) {
            ContractRead::Valid(mut q) => {
                // The action's command and arguments stay exact: an approval is bound to them.
                q.text = redact_block(&self.redactor, &q.text);
                for choice in &mut q.choices {
                    *choice = redact_block(&self.redactor, choice);
                }
                ContractRead::Valid(q)
            }
            other => other,
        }
    }

    fn read_result(&self, worktree: &str) -> ContractRead<WorkerResult> {
        match self.read(worktree, RESULT_FILE, WorkerResult::from_value) {
            ContractRead::Valid(mut r) => {
                r.summary = redact_block(&self.redactor, &r.summary);
                ContractRead::Valid(r)
            }
            other => other,
        }
    }

    fn question_present(&self, worktree: &str) -> bool {
        let path = Path::new(worktree).join(DIR_NAME).join(QUESTION_FILE);
        fsx::lstat_kind(&path).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{wait_until, TempDir};
    use fleet_domain::contract::{QuestionKind, ResultStatus};
    use nix::sys::stat::Mode;
    use std::fs;
    use std::os::unix::fs::{symlink, MetadataExt};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    const GOOD_RESULT: &str = r#"{"version":1,"status":"done","summary":"all good"}"#;
    const GOOD_QUESTION: &str =
        r#"{"version":1,"kind":"question","text":"which db?","choices":["a","b"]}"#;

    fn files() -> FsContractFiles {
        FsContractFiles::new(Arc::new(Redactor::builtin()))
    }

    fn wt(t: &TempDir) -> String {
        t.path().to_str().unwrap().to_string()
    }

    fn invalid_text<T: std::fmt::Debug>(r: ContractRead<T>) -> String {
        match r {
            ContractRead::Invalid(s) => s,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn reads_valid_files() {
        let t = TempDir::new("contract-valid");
        t.write(".fleet/result.json", GOOD_RESULT);
        t.write(".fleet/question.json", GOOD_QUESTION);
        let c = files();
        match c.read_result(&wt(&t)) {
            ContractRead::Valid(r) => {
                assert_eq!(r.status, ResultStatus::Done);
                assert_eq!(r.summary, "all good");
            }
            other => panic!("{other:?}"),
        }
        match c.read_question(&wt(&t)) {
            ContractRead::Valid(q) => {
                assert_eq!(q.kind, QuestionKind::Question);
                assert_eq!(q.choices, vec!["a", "b"]);
            }
            other => panic!("{other:?}"),
        }
        assert!(c.question_present(&wt(&t)));
    }

    #[test]
    fn missing_files_are_absent() {
        let t = TempDir::new("contract-missing");
        let c = files();
        assert_eq!(c.read_result(&wt(&t)), ContractRead::Absent);
        assert_eq!(c.read_question(&wt(&t)), ContractRead::Absent);
        assert!(!c.question_present(&wt(&t)));
        fs::create_dir(t.join(".fleet")).unwrap();
        assert_eq!(c.read_result(&wt(&t)), ContractRead::Absent);
    }

    #[test]
    fn malformed_json_and_wrong_shapes_are_invalid() {
        let t = TempDir::new("contract-bad");
        let c = files();
        t.write(".fleet/result.json", "{not json");
        assert!(invalid_text(c.read_result(&wt(&t))).starts_with("result.json: "));
        t.write(".fleet/result.json", r#"{"version":1,"status":"maybe"}"#);
        assert!(invalid_text(c.read_result(&wt(&t))).contains("status"));
        t.write(".fleet/result.json", "[1,2]");
        assert!(invalid_text(c.read_result(&wt(&t))).starts_with("result.json: "));
        t.write(".fleet/result.json", "{} trailing");
        assert!(invalid_text(c.read_result(&wt(&t))).starts_with("result.json: "));
        t.write(".fleet/question.json", r#"{"version":1,"kind":"question"}"#);
        assert!(invalid_text(c.read_question(&wt(&t))).starts_with("question.json: "));
        // question_present only looks at existence, not validity.
        assert!(c.question_present(&wt(&t)));
    }

    #[test]
    fn oversized_files_are_invalid_without_being_parsed() {
        let t = TempDir::new("contract-big");
        let big = format!(
            r#"{{"version":1,"status":"done","summary":"{}"}}"#,
            "x".repeat(usize::try_from(MAX_CONTRACT_FILE).unwrap())
        );
        t.write(".fleet/result.json", &big);
        let text = invalid_text(files().read_result(&wt(&t)));
        assert!(text.contains("larger than"), "{text}");
    }

    #[test]
    fn a_fifo_named_result_json_does_not_block() {
        let t = TempDir::new("contract-fifo");
        fs::create_dir(t.join(".fleet")).unwrap();
        nix::unistd::mkfifo(&t.join(".fleet/result.json"), Mode::S_IRWXU).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let path = wt(&t);
        let handle = std::thread::spawn(move || {
            let r = files().read_result(&path);
            flag.store(true, Ordering::SeqCst);
            r
        });
        assert!(
            wait_until(3000, || done.load(Ordering::SeqCst)),
            "blocked on a FIFO"
        );
        assert!(invalid_text(handle.join().unwrap()).starts_with("result.json: "));
    }

    #[test]
    fn a_symlinked_result_is_not_followed() {
        let t = TempDir::new("contract-link");
        let big = t.join("big");
        fs::File::create(&big)
            .unwrap()
            .set_len(256 * 1024 * 1024)
            .unwrap();
        fs::create_dir(t.join(".fleet")).unwrap();
        symlink(&big, t.join(".fleet/result.json")).unwrap();
        let text = invalid_text(files().read_result(&wt(&t)));
        assert_eq!(text, "result.json: is a symlink");

        // Even a link to a perfectly valid file elsewhere is refused.
        let good = t.write("elsewhere.json", GOOD_RESULT);
        fs::remove_file(t.join(".fleet/result.json")).unwrap();
        symlink(&good, t.join(".fleet/result.json")).unwrap();
        assert!(matches!(
            files().read_result(&wt(&t)),
            ContractRead::Invalid(_)
        ));
    }

    #[test]
    fn a_symlinked_fleet_directory_reads_nothing_outside() {
        let t = TempDir::new("contract-dirlink");
        let work = t.join("work");
        let outside = t.join("outside");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("result.json"), GOOD_RESULT).unwrap();
        fs::write(outside.join("question.json"), GOOD_QUESTION).unwrap();
        symlink(&outside, work.join(".fleet")).unwrap();
        let w = work.to_str().unwrap();
        let c = files();
        assert!(matches!(c.read_result(w), ContractRead::Invalid(_)));
        assert!(matches!(c.read_question(w), ContractRead::Invalid(_)));
    }

    #[test]
    fn prepare_creates_fleet_dir_and_private_brief() {
        let t = TempDir::new("contract-prepare");
        files().prepare(&wt(&t), "# brief\n").unwrap();
        assert_eq!(t.read(".fleet/brief.md"), "# brief\n");
        assert_eq!(
            fs::metadata(t.join(".fleet")).unwrap().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(t.join(".fleet/brief.md")).unwrap().mode() & 0o777,
            0o600
        );
        // Rewriting replaces the brief.
        files().prepare(&wt(&t), "second").unwrap();
        assert_eq!(t.read(".fleet/brief.md"), "second");
    }

    #[test]
    fn prepare_truncates_an_oversized_brief_on_a_char_boundary() {
        let t = TempDir::new("contract-brief-cap");
        let brief = "\u{e9}".repeat(MAX_BRIEF_FILE_BYTES);
        files().prepare(&wt(&t), &brief).unwrap();
        let written = fs::read(t.join(".fleet/brief.md")).unwrap();
        assert!(written.len() <= MAX_BRIEF_FILE_BYTES);
        assert!(String::from_utf8(written).is_ok());
    }

    #[test]
    fn prepare_removes_stale_contract_files() {
        let t = TempDir::new("contract-stale");
        t.write(".fleet/result.json", GOOD_RESULT);
        t.write(".fleet/question.json", GOOD_QUESTION);
        let c = files();
        c.prepare(&wt(&t), "b").unwrap();
        assert_eq!(c.read_result(&wt(&t)), ContractRead::Absent);
        assert!(!c.question_present(&wt(&t)));
    }

    #[test]
    fn prepare_unlinks_stale_links_without_touching_their_targets() {
        let t = TempDir::new("contract-stale-link");
        let victim = t.write("victim.txt", "keep");
        fs::create_dir(t.join(".fleet")).unwrap();
        symlink(&victim, t.join(".fleet/result.json")).unwrap();
        nix::unistd::mkfifo(&t.join(".fleet/question.json"), Mode::S_IRWXU).unwrap();
        files().prepare(&wt(&t), "b").unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        assert!(!t.exists(".fleet/result.json"));
        assert!(!t.exists(".fleet/question.json"));
    }

    #[test]
    fn prepare_replaces_a_symlinked_fleet_without_writing_through_it() {
        let t = TempDir::new("contract-fleet-link");
        let work = t.join("work");
        let outside = t.join("outside");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("result.json"), "outside").unwrap();
        symlink(&outside, work.join(".fleet")).unwrap();
        files().prepare(work.to_str().unwrap(), "brief").unwrap();
        assert!(fsx::is_real_dir(&work.join(".fleet")));
        assert!(work.join(".fleet/brief.md").exists());
        // Nothing was written into, or removed from, the link target.
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
        assert_eq!(
            fs::read_to_string(outside.join("result.json")).unwrap(),
            "outside"
        );

        // A plain file named `.fleet` is replaced too.
        let t2 = TempDir::new("contract-fleet-file");
        t2.write(".fleet", "junk");
        files().prepare(&wt(&t2), "brief").unwrap();
        assert!(fsx::is_real_dir(&t2.join(".fleet")));
    }

    #[test]
    fn prepare_rejects_bad_worktrees_and_directory_results() {
        let t = TempDir::new("contract-prepare-bad");
        let c = files();
        assert!(c.prepare(t.join("missing").to_str().unwrap(), "b").is_err());
        let file = t.write("f", "x");
        assert!(c.prepare(file.to_str().unwrap(), "b").is_err());
        fs::create_dir_all(t.join(".fleet/result.json")).unwrap();
        let err = c.prepare(&wt(&t), "b").unwrap_err();
        assert!(err.message.contains("result.json"));
        assert!(t.exists(".fleet/result.json"));
    }

    /// Shaped like a GitHub token, built at run time so no secret-looking literal is in the code.
    fn token() -> String {
        format!("ghp_{}", "a1B2".repeat(10))
    }

    #[test]
    fn result_summary_is_redacted() {
        let t = TempDir::new("contract-redact-result");
        let body = format!(
            r#"{{"version":1,"status":"done","summary":"deployed with {}"}}"#,
            token()
        );
        t.write(".fleet/result.json", &body);
        match files().read_result(&wt(&t)) {
            ContractRead::Valid(r) => {
                assert_eq!(r.summary, "deployed with [REDACTED:github-token]");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn question_text_and_choices_are_redacted_but_the_action_stays_exact() {
        let t = TempDir::new("contract-redact-question");
        let tok = token();
        let body = format!(
            r#"{{"version":1,"kind":"question","text":"use {tok}?","choices":["yes {tok}","no"]}}"#
        );
        t.write(".fleet/question.json", &body);
        match files().read_question(&wt(&t)) {
            ContractRead::Valid(q) => {
                assert_eq!(q.text, "use [REDACTED:github-token]?");
                assert_eq!(q.choices, vec!["yes [REDACTED:github-token]", "no"]);
            }
            other => panic!("{other:?}"),
        }
    }
}
