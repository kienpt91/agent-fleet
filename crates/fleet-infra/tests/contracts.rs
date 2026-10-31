//! The real adapters must satisfy the port contracts that the in-memory fakes satisfy
//! (`crates/fleet-app/tests/contracts_memory.rs`). Everything runs in temporary directories.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_app::ports::{ContractFiles, EventStore, JobRunner, OutputLogs, Vcs, WorktreeState};
use fleet_app::testing::contracts::{
    contract_files_contract, event_store_contract, output_logs_contract, verify_jobs_contract,
    ContractFile, ContractHarness, Plant, VerifyHarness, PLANT_MARKER, VALID_QUESTION_JSON,
    VALID_RESULT_JSON,
};
use fleet_app::testing::mail_contracts::{
    inbox_contract, outbox_contract, MailHarness, OutboxPlant,
};
use fleet_domain::env::EnvPolicy;
use fleet_domain::ids::{RunId, Sha, TaskId};
use fleet_domain::redact::Redactor;
use fleet_infra::contract_fs::FsContractFiles;
use fleet_infra::digest::Sha256Digest;
use fleet_infra::eventlog::FileEventStore;
use fleet_infra::git::GitVcs;
use fleet_infra::jobs::ThreadJobRunner;
use fleet_infra::logs::FileOutputLogs;
use fleet_infra::testutil::{git, make_repo, TempDir};
use fleet_infra::verify::VerifyHost;
use nix::sys::stat::Mode;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ---- event store ----------------------------------------------------------------------------

fn run_event_store_contract(log: PathBuf) {
    event_store_contract(&mut || {
        let store = FileEventStore::open(&log, Box::new(Sha256Digest)).expect("open the event log");
        Box::new(store) as Box<dyn EventStore>
    });
}

#[test]
fn file_event_store_satisfies_the_contract() {
    let dir = TempDir::new("contract-events");
    run_event_store_contract(dir.join("events.log"));
}

#[test]
fn file_event_store_satisfies_the_contract_in_a_nested_directory() {
    let dir = TempDir::new("contract-events-nested");
    let nested = dir.join("state/daemon/events");
    fs::create_dir_all(&nested).unwrap();
    run_event_store_contract(nested.join("events.log"));
    assert!(
        nested.join("events.log").is_file(),
        "the log is created where it was asked for"
    );
    assert!(
        !dir.exists("events.log"),
        "nothing is written outside the given path"
    );
}

// ---- output logs ----------------------------------------------------------------------------

fn run_output_logs_contract(root: PathBuf) {
    output_logs_contract(&mut || {
        Box::new(FileOutputLogs::new(root.clone())) as Box<dyn OutputLogs>
    });
}

fn largest_file(dir: &Path) -> u64 {
    let mut largest = 0;
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let meta = fs::symlink_metadata(entry.path()).unwrap();
        largest = largest.max(if meta.is_dir() {
            largest_file(&entry.path())
        } else {
            meta.len()
        });
    }
    largest
}

#[test]
fn file_output_logs_satisfy_the_contract() {
    let dir = TempDir::new("contract-logs");
    let root = dir.join("logs");
    run_output_logs_contract(root.clone());
    // The suite stores 3 MiB of output; the adapter keeps no more than 1 MiB of it.
    assert!(
        largest_file(dir.path()) <= 1024 * 1024,
        "stored output is bounded"
    );
}

#[test]
fn file_output_logs_in_different_directories_are_isolated() {
    let first = TempDir::new("contract-logs-a");
    let second = TempDir::new("contract-logs-b");
    run_output_logs_contract(first.join("logs"));

    // What the suite left in the first directory is invisible through the second.
    let probe = FileOutputLogs::new(second.join("logs"));
    for (run, task) in [("r1", "t1"), ("r2", "big"), ("r6", "t1")] {
        let (r, t) = (
            fleet_domain::ids::RunId::parse(run).unwrap(),
            fleet_domain::ids::TaskId::parse(task).unwrap(),
        );
        assert_eq!(
            probe.tail(&r, &t, usize::MAX),
            None,
            "{run}/{task} leaked between directories"
        );
    }
    run_output_logs_contract(second.join("logs"));
}

// ---- contract files -------------------------------------------------------------------------

/// A worktree `<tmp>/wt` with the worker files planted by plain file system calls.
struct FsHarness {
    dir: TempDir,
    files: FsContractFiles,
}

impl FsHarness {
    fn new() -> FsHarness {
        let dir = TempDir::new("contract-files");
        fs::create_dir(dir.join("wt")).unwrap();
        FsHarness {
            dir,
            files: FsContractFiles::new(Arc::new(Redactor::builtin())),
        }
    }

    fn name(file: ContractFile) -> &'static str {
        match file {
            ContractFile::Result => "result.json",
            ContractFile::Question => "question.json",
        }
    }

    fn valid_text(file: ContractFile) -> &'static str {
        match file {
            ContractFile::Result => VALID_RESULT_JSON,
            ContractFile::Question => VALID_QUESTION_JSON,
        }
    }

    /// Removes whatever is at `path` without following a link.
    fn clear(path: &Path) {
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => fs::remove_dir_all(path).unwrap(),
            Ok(_) => fs::remove_file(path).unwrap(),
            Err(_) => {}
        }
    }
}

impl ContractHarness for FsHarness {
    fn files(&self) -> &dyn ContractFiles {
        &self.files
    }

    fn worktree(&self) -> String {
        self.dir.join("wt").to_str().unwrap().to_string()
    }

    fn plant(&mut self, file: ContractFile, kind: Plant) {
        let fleet_dir = self.dir.join("wt/.fleet");
        fs::create_dir_all(&fleet_dir).unwrap();
        let path = fleet_dir.join(Self::name(file));
        Self::clear(&path);
        match kind {
            Plant::Valid => fs::write(&path, Self::valid_text(file)).unwrap(),
            Plant::Malformed => {
                fs::write(
                    &path,
                    format!("{{\"version\":1, \"{PLANT_MARKER}\": [unterminated"),
                )
                .unwrap();
            }
            Plant::WrongShape => {
                let text = match file {
                    ContractFile::Result => {
                        format!(
                            r#"{{"version":1,"status":"{PLANT_MARKER}","summary":["not","text"]}}"#
                        )
                    }
                    ContractFile::Question => {
                        format!(
                            r#"{{"version":1,"kind":"{PLANT_MARKER}","text":"{PLANT_MARKER}"}}"#
                        )
                    }
                };
                fs::write(&path, text).unwrap();
            }
            Plant::Oversize => {
                let filler = "x".repeat(200 * 1024);
                let text = match file {
                    ContractFile::Result => {
                        format!(
                            r#"{{"version":1,"status":"done","summary":"{PLANT_MARKER}{filler}"}}"#
                        )
                    }
                    ContractFile::Question => {
                        format!(
                            r#"{{"version":1,"kind":"question","text":"{PLANT_MARKER}{filler}"}}"#
                        )
                    }
                };
                fs::write(&path, text).unwrap();
            }
            Plant::Symlink => {
                // The target is a valid file outside the worktree: a reader that followed the
                // link would report a valid result or question.
                let outside = self.dir.join("outside");
                fs::create_dir_all(&outside).unwrap();
                let target = outside.join(Self::name(file));
                fs::write(&target, Self::valid_text(file)).unwrap();
                symlink(&target, &path).unwrap();
            }
            Plant::Fifo => nix::unistd::mkfifo(&path, Mode::S_IRWXU).unwrap(),
            Plant::Directory => fs::create_dir(&path).unwrap(),
        }
    }

    fn plant_stale_files(&mut self) {
        self.plant(ContractFile::Result, Plant::Valid);
        self.plant(ContractFile::Question, Plant::Valid);
    }
}

#[test]
fn fs_contract_files_satisfy_the_contract() {
    let mut harness = FsHarness::new();
    contract_files_contract(&mut harness);
    // The worktree is the only place the adapter wrote to, and the outside files are intact.
    for file in [ContractFile::Result, ContractFile::Question] {
        let outside = harness.dir.join("outside").join(FsHarness::name(file));
        assert_eq!(
            fs::read_to_string(outside).unwrap(),
            FsHarness::valid_text(file),
            "outside file was modified"
        );
    }
}

// ---- verification jobs ------------------------------------------------------------------------

/// A real repository with a fleet worktree whose committed files are exactly the ones the suite
/// asks for; verification jobs export that commit.
struct FsVerifyHarness {
    dir: TempDir,
    wt: PathBuf,
    vcs: GitVcs,
    base: Sha,
}

fn verify_git() -> GitVcs {
    GitVcs::new(
        vec![(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        )],
        EnvPolicy::default_policy(),
    )
}

impl FsVerifyHarness {
    fn new() -> FsVerifyHarness {
        let dir = TempDir::new("contract-verify");
        let repo = make_repo(&dir);
        let vcs = verify_git();
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
        FsVerifyHarness { dir, wt, vcs, base }
    }
}

impl VerifyHarness for FsVerifyHarness {
    fn runner(&mut self) -> Box<dyn JobRunner> {
        let host = VerifyHost::new(verify_git(), self.dir.join("scratch")).unwrap();
        Box::new(ThreadJobRunner::new(4, Arc::new(Redactor::builtin())).with_verification(host))
    }

    fn worktree(&self) -> String {
        self.wt.display().to_string()
    }

    fn state(&self) -> WorktreeState {
        self.vcs
            .worktree_state(self.wt.to_str().unwrap(), &self.base)
            .unwrap()
    }

    fn place(&mut self, present: &[&str]) {
        git(&self.wt, &["rm", "-r", "-q", "--ignore-unmatch", "."]);
        for file in present {
            fs::write(self.wt.join(file), "x").unwrap();
        }
        git(&self.wt, &["add", "-A"]);
        git(
            &self.wt,
            &["commit", "-q", "--allow-empty", "-m", "place files"],
        );
    }
}

#[test]
fn thread_job_runner_satisfies_the_verify_jobs_contract() {
    verify_jobs_contract(&mut FsVerifyHarness::new());
}

// ---- messages between agents ----------------------------------------------------------------

/// The outbox reader and the inbox writer on real directories.
struct FsMailHarness {
    wt: TempDir,
    elsewhere: TempDir,
    outbox: fleet_infra::msgfs::FsOutbox,
    inbox: fleet_infra::msgfs::FsInbox,
}

impl FsMailHarness {
    fn new() -> FsMailHarness {
        let wt = TempDir::new("contract-mail");
        fs::create_dir_all(wt.join(".fleet")).unwrap();
        FsMailHarness {
            wt,
            elsewhere: TempDir::new("contract-mail-elsewhere"),
            outbox: fleet_infra::msgfs::FsOutbox::new().with_settle(std::time::Duration::ZERO),
            inbox: fleet_infra::msgfs::FsInbox::new(),
        }
    }
}

const VALID_MESSAGE: &str =
    r#"{"version":1,"channel":"talk","to":"b","kind":"info","subject":"s","body":"b"}"#;

impl MailHarness for FsMailHarness {
    fn outbox(&self) -> &dyn fleet_app::ports::OutboxReader {
        &self.outbox
    }
    fn inbox(&self) -> &dyn fleet_app::ports::InboxWriter {
        &self.inbox
    }
    fn worktree(&self) -> String {
        self.wt.path().to_str().unwrap().to_string()
    }
    fn plant_outbox(&mut self, name: &str, what: OutboxPlant) {
        let dir = self.wt.join(".fleet/outbox");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = fs::remove_file(&path);
        match what {
            OutboxPlant::Valid => fs::write(&path, VALID_MESSAGE).unwrap(),
            OutboxPlant::Malformed => fs::write(&path, format!("{{{PLANT_MARKER}")).unwrap(),
            OutboxPlant::Oversized => fs::write(
                &path,
                format!(r#"{{"version":1,"body":"{}"}}"#, "x".repeat(40_000)),
            )
            .unwrap(),
            OutboxPlant::Link => {
                let target = self.elsewhere.write("secret.json", VALID_MESSAGE);
                symlink(target, &path).unwrap();
            }
            OutboxPlant::Fifo => nix::unistd::mkfifo(&path, Mode::S_IRWXU).unwrap(),
            OutboxPlant::Directory => fs::create_dir(&path).unwrap(),
        }
    }
    fn outbox_names(&self) -> Vec<String> {
        fs::read_dir(self.wt.join(".fleet/outbox"))
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| n.ends_with(".json"))
                    .collect()
            })
            .unwrap_or_default()
    }
    fn replace_outbox_with_link(&mut self) {
        let _ = fs::remove_dir_all(self.wt.join(".fleet/outbox"));
        symlink(self.elsewhere.path(), self.wt.join(".fleet/outbox")).unwrap();
    }
    fn inbox_files(&self) -> std::collections::BTreeMap<String, String> {
        fs::read_dir(self.wt.join(".fleet/inbox"))
            .unwrap()
            .flatten()
            .map(|e| {
                let name = e.file_name().into_string().unwrap();
                let text = fs::read_to_string(e.path()).unwrap();
                (name, text)
            })
            .collect()
    }
    fn plant_inbox_stray(&mut self, name: &str) {
        fs::write(self.wt.join(".fleet/inbox").join(name), "forged\n").unwrap();
    }
    fn replace_inbox_with_link(&mut self) {
        let _ = fs::remove_dir_all(self.wt.join(".fleet/inbox"));
        symlink(self.elsewhere.path(), self.wt.join(".fleet/inbox")).unwrap();
    }
    fn link_target_untouched(&self) -> bool {
        fs::read_dir(self.elsewhere.path())
            .unwrap()
            .next()
            .is_none()
    }
}

#[test]
fn file_outbox_satisfies_the_contract() {
    outbox_contract(&mut FsMailHarness::new());
}

#[test]
fn file_inbox_satisfies_the_contract() {
    inbox_contract(&mut FsMailHarness::new());
}
