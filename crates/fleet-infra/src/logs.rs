//! Stored session output, one file per task attempt.

use crate::fsx::{self, FileKind};
use fleet_app::ports::OutputLogs;
use fleet_domain::error::Result;
use fleet_domain::ids::{RunId, TaskId};
use fleet_domain::sanitize::tail_str;
use nix::fcntl::OFlag;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

/// Longest output kept per attempt; older text is dropped.
const MAX_STORED_BYTES: usize = 1024 * 1024;

pub struct FileOutputLogs {
    dir: PathBuf,
}

impl FileOutputLogs {
    pub fn new(dir: PathBuf) -> FileOutputLogs {
        FileOutputLogs { dir }
    }

    fn run_dir(&self, run: &RunId) -> PathBuf {
        self.dir.join(run.as_str())
    }

    /// Attempt numbers stored for the task. `<task>-<n>.log` with a numeric `n` only, so a task
    /// whose id is a prefix of another's (`a` and `a-1`) never picks up foreign files.
    fn attempts(&self, run: &RunId, task: &TaskId) -> Vec<(u32, PathBuf)> {
        let run_dir = self.run_dir(run);
        if !fsx::is_real_dir(&run_dir) {
            return Vec::new();
        }
        let Ok(entries) = fs::read_dir(&run_dir) else {
            return Vec::new();
        };
        let prefix = format!("{}-", task.as_str());
        entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_str()?;
                let attempt: u32 = name
                    .strip_prefix(&prefix)?
                    .strip_suffix(".log")?
                    .parse()
                    .ok()?;
                Some((attempt, entry.path()))
            })
            .collect()
    }
}

impl OutputLogs for FileOutputLogs {
    fn save(&mut self, run: &RunId, task: &TaskId, attempt: u32, text: &str) -> Result<()> {
        fsx::ensure_private_dir(&self.dir)?;
        let run_dir = self.run_dir(run);
        fsx::ensure_private_dir(&run_dir)?;
        let path = run_dir.join(format!("{}-{attempt}.log", task.as_str()));
        fsx::write_private_atomic(&path, tail_str(text, MAX_STORED_BYTES).as_bytes())
    }

    fn tail(&self, run: &RunId, task: &TaskId, max: usize) -> Option<String> {
        let (_, path) = self
            .attempts(run, task)
            .into_iter()
            .max_by_key(|(n, _)| *n)?;
        read_tail(&path, max)
    }

    fn remove(&mut self, run: &RunId, task: &TaskId) {
        for (_, path) in self.attempts(run, task) {
            // Best effort: a log that cannot be removed is only stale output.
            let _ = fsx::remove_nofollow(&path);
        }
        let run_dir = self.run_dir(run);
        if fsx::lstat_kind(&run_dir) == Some(FileKind::Dir) {
            // Fails (and is ignored) while other tasks still have logs.
            let _ = fs::remove_dir(&run_dir);
        }
    }
}

/// Reads at most `max` bytes from the end of a regular file, never following a symlink and
/// never blocking on a FIFO. A cut inside a UTF-8 sequence is moved forward to a boundary.
fn read_tail(path: &std::path::Path, max: usize) -> Option<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let want = u64::try_from(max).unwrap_or(u64::MAX).min(meta.len());
    file.seek(SeekFrom::Start(meta.len().saturating_sub(want)))
        .ok()?;
    let mut buf = Vec::with_capacity(usize::try_from(want).unwrap_or(0));
    // `take` keeps the read bounded even if the file grows meanwhile.
    file.take(want).read_to_end(&mut buf).ok()?;
    let skip = buf
        .iter()
        .take_while(|b| (0x80..0xC0).contains(*b))
        .count()
        .min(3);
    let bytes = buf.get(if want < meta.len() { skip } else { 0 }..)?;
    Some(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::os::unix::fs::{symlink, MetadataExt};

    fn ids(run: &str, task: &str) -> (RunId, TaskId) {
        (RunId::parse(run).unwrap(), TaskId::parse(task).unwrap())
    }

    #[test]
    fn save_then_tail() {
        let t = TempDir::new("logs-basic");
        let mut logs = FileOutputLogs::new(t.join("logs"));
        let (r, k) = ids("r1", "t1");
        assert_eq!(logs.tail(&r, &k, 100), None);
        logs.save(&r, &k, 1, "hello world").unwrap();
        assert_eq!(logs.tail(&r, &k, 100).as_deref(), Some("hello world"));
        assert_eq!(logs.tail(&r, &k, 5).as_deref(), Some("world"));
        assert_eq!(logs.tail(&r, &k, 0).as_deref(), Some(""));
        let file = t.join("logs/r1/t1-1.log");
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(t.join("logs/r1")).unwrap().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn the_newest_attempt_wins_numerically() {
        let t = TempDir::new("logs-attempt");
        let mut logs = FileOutputLogs::new(t.join("logs"));
        let (r, k) = ids("r1", "t1");
        logs.save(&r, &k, 2, "second").unwrap();
        logs.save(&r, &k, 10, "tenth").unwrap();
        logs.save(&r, &k, 9, "ninth").unwrap();
        assert_eq!(logs.tail(&r, &k, 100).as_deref(), Some("tenth"));
    }

    #[test]
    fn tasks_with_prefix_ids_do_not_mix() {
        let t = TempDir::new("logs-prefix");
        let mut logs = FileOutputLogs::new(t.join("logs"));
        let (r, a) = ids("r1", "a");
        let (_, a1) = ids("r1", "a-1");
        logs.save(&r, &a1, 3, "belongs to a-1").unwrap();
        assert_eq!(logs.tail(&r, &a, 100), None);
        logs.save(&r, &a, 1, "belongs to a").unwrap();
        assert_eq!(logs.tail(&r, &a, 100).as_deref(), Some("belongs to a"));
        logs.remove(&r, &a);
        assert_eq!(logs.tail(&r, &a1, 100).as_deref(), Some("belongs to a-1"));
    }

    #[test]
    fn saved_text_is_bounded_and_cut_on_a_char_boundary() {
        let t = TempDir::new("logs-bound");
        let mut logs = FileOutputLogs::new(t.join("logs"));
        let (r, k) = ids("r1", "t1");
        let text = format!("START{}", "\u{e9}".repeat(MAX_STORED_BYTES));
        logs.save(&r, &k, 1, &text).unwrap();
        let size = fs::metadata(t.join("logs/r1/t1-1.log")).unwrap().len() as usize;
        assert!(size <= MAX_STORED_BYTES);
        let got = logs.tail(&r, &k, usize::MAX).unwrap();
        assert!(!got.contains("START"));
        assert!(!got.contains('\u{fffd}'));
    }

    #[test]
    fn tail_never_splits_a_character() {
        let t = TempDir::new("logs-split");
        let mut logs = FileOutputLogs::new(t.join("logs"));
        let (r, k) = ids("r1", "t1");
        logs.save(&r, &k, 1, "ab\u{e9}\u{e9}\u{e9}").unwrap();
        // 5 bytes would start in the middle of the second e-acute.
        let got = logs.tail(&r, &k, 5).unwrap();
        assert_eq!(got, "\u{e9}\u{e9}");
        assert!(!got.contains('\u{fffd}'));
    }

    #[test]
    fn tail_reads_only_what_was_asked() {
        let t = TempDir::new("logs-huge");
        let logs = FileOutputLogs::new(t.join("logs"));
        let (r, k) = ids("r1", "t1");
        fs::create_dir_all(t.join("logs/r1")).unwrap();
        // A sparse 1 GiB file planted by someone else: only `max` bytes come back.
        let f = fs::File::create(t.join("logs/r1/t1-1.log")).unwrap();
        f.set_len(1 << 30).unwrap();
        assert_eq!(logs.tail(&r, &k, 16).unwrap().len(), 16);
    }

    #[test]
    fn remove_deletes_the_tasks_files_only() {
        let t = TempDir::new("logs-remove");
        let mut logs = FileOutputLogs::new(t.join("logs"));
        let (r, k) = ids("r1", "t1");
        let (_, other) = ids("r1", "t2");
        logs.save(&r, &k, 1, "one").unwrap();
        logs.save(&r, &k, 2, "two").unwrap();
        logs.save(&r, &other, 1, "keep").unwrap();
        logs.remove(&r, &k);
        assert_eq!(logs.tail(&r, &k, 100), None);
        assert_eq!(logs.tail(&r, &other, 100).as_deref(), Some("keep"));
        logs.remove(&r, &other);
        assert!(!t.exists("logs/r1"), "an empty run directory is removed");
        // Removing something that does not exist is harmless.
        logs.remove(&r, &k);
    }

    #[test]
    fn a_symlinked_log_directory_is_refused() {
        let t = TempDir::new("logs-link");
        let outside = t.join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, t.join("logs")).unwrap();
        let mut logs = FileOutputLogs::new(t.join("logs"));
        let (r, k) = ids("r1", "t1");
        assert!(logs.save(&r, &k, 1, "x").is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);

        // The same for a symlinked run directory.
        let t2 = TempDir::new("logs-runlink");
        let outside2 = t2.join("outside");
        fs::create_dir_all(&outside2).unwrap();
        fs::write(outside2.join("t1-1.log"), "secret").unwrap();
        fs::create_dir(t2.join("logs")).unwrap();
        symlink(&outside2, t2.join("logs/r1")).unwrap();
        let mut logs2 = FileOutputLogs::new(t2.join("logs"));
        assert!(logs2.save(&r, &k, 2, "x").is_err());
        assert_eq!(logs2.tail(&r, &k, 100), None);
        assert!(!outside2.join("t1-2.log").exists());
    }

    #[test]
    fn a_symlinked_log_file_is_not_followed() {
        let t = TempDir::new("logs-filelink");
        let secret = t.write("secret.txt", "do not leak");
        fs::create_dir_all(t.join("logs/r1")).unwrap();
        symlink(&secret, t.join("logs/r1/t1-1.log")).unwrap();
        let logs = FileOutputLogs::new(t.join("logs"));
        let (r, k) = ids("r1", "t1");
        assert_eq!(logs.tail(&r, &k, 100), None);
    }
}
