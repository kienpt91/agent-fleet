//! The real adapters must satisfy the port contracts that the in-memory fakes satisfy
//! (`crates/fleet-app/tests/contracts_memory.rs`). Everything runs in temporary directories.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_app::ports::{ContractFiles, EventStore, OutputLogs};
use fleet_app::testing::contracts::{
    contract_files_contract, event_store_contract, output_logs_contract, ContractFile,
    ContractHarness, Plant, PLANT_MARKER, VALID_QUESTION_JSON, VALID_RESULT_JSON,
};
use fleet_domain::redact::Redactor;
use fleet_infra::contract_fs::FsContractFiles;
use fleet_infra::digest::Sha256Digest;
use fleet_infra::eventlog::FileEventStore;
use fleet_infra::logs::FileOutputLogs;
use fleet_infra::testutil::TempDir;
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
