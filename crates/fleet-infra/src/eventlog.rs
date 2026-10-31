//! Append-only, hash-chained event log in a file.
//!
//! One JSON object per line: `{"seq","ts","prev","body","hash"}`. The chain (see
//! `fleet_domain::chain`) detects edits, deletions, reordering and insertions. Truncation of the
//! tail is caught by a small sidecar (`<log stem>.head`) that records the record count and last
//! hash after every append. The sidecar is created together with the log (0 records, the genesis
//! hash), so a log with records and no sidecar is reported as tampered. Like the log it is
//! writable by anyone with the same user id, so this is tamper evidence against accidents and
//! careless edits, not against a determined attacker.

use crate::digest::Sha256Digest;
use crate::fsx::{self, FileKind, ReadError};
use crate::json;
use fleet_app::ports::{EventStore, ReplayInfo, SendDigest, StoredEvent};
use fleet_domain::approval::Digest;
use fleet_domain::chain::{seal, ChainVerifier, Link};
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::events::DomainEvent;
use fleet_domain::limits::{MAX_EVENT_LINE, MAX_EVENT_LOG_BYTES};
use fleet_domain::value::Value;
use nix::fcntl::OFlag;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyReport {
    pub records: u64,
    pub head_hash: String,
}

fn tampered(message: impl Into<String>) -> FleetError {
    FleetError::new(ErrorCode::Tampered, message)
}

fn head_path(log: &Path) -> PathBuf {
    log.with_extension("head")
}

struct Head {
    records: u64,
    hash: String,
}

fn read_head(path: &Path) -> Result<Option<Head>> {
    let bytes = match fsx::read_regular(path, 256) {
        Ok(b) => b,
        // `scan` decides what a missing head means: only a log without records may lack it.
        Err(ReadError::Missing) => return Ok(None),
        Err(e) => return Err(tampered(format!("event log head file {e}"))),
    };
    let bad = || tampered("event log head file is malformed");
    let text = std::str::from_utf8(&bytes).map_err(|_| bad())?;
    let mut parts = text.split_whitespace();
    let (Some(records), Some(hash), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(bad());
    };
    let records: u64 = records.parse().map_err(|_| bad())?;
    if hash.len() != 64 || !hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(bad());
    }
    Ok(Some(Head {
        records,
        hash: hash.to_string(),
    }))
}

/// Opens an existing log for reading without following a symlink and without blocking.
fn open_log_read(path: &Path) -> Result<(File, u64)> {
    match fsx::lstat_kind(path) {
        Some(FileKind::File) => {}
        Some(FileKind::Symlink) => {
            return Err(tampered(format!(
                "event log {} is a symlink; refusing to use it",
                path.display()
            )))
        }
        Some(_) => {
            return Err(tampered(format!(
                "event log {} is not a regular file",
                path.display()
            )))
        }
        None => {
            return Err(FleetError::not_found(format!(
                "event log {} does not exist",
                path.display()
            )))
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
        .open(path)
        .map_err(|e| FleetError::io(&format!("cannot open event log {}", path.display()), &e))?;
    let meta = file
        .metadata()
        .map_err(|e| FleetError::io("cannot examine the event log", &e))?;
    if !meta.is_file() {
        return Err(tampered("event log is not a regular file"));
    }
    Ok((file, meta.len()))
}

fn parse_link(line: &[u8], index: u64) -> std::result::Result<Link, String> {
    let bad = |why: &str| format!("record {index} is damaged: {why}");
    let v = json::parse_value(line, MAX_EVENT_LINE).map_err(|e| bad(&e))?;
    let seq = v
        .get("seq")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad("no seq"))?;
    let ts_ms = v
        .get("ts")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad("no ts"))?;
    let prev = v
        .get("prev")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("no prev"))?;
    let hash = v
        .get("hash")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("no hash"))?;
    let body = v.get("body").ok_or_else(|| bad("no body"))?;
    Ok(Link {
        seq,
        ts_ms,
        prev: prev.to_string(),
        body: body.clone(),
        hash: hash.to_string(),
    })
}

struct Scan {
    records: u64,
    head_hash: String,
    /// Bytes of complete, verified lines.
    valid_len: u64,
    /// The file ends with an unterminated line that does not parse.
    partial_tail: bool,
}

/// Verifies the whole chain, reading one bounded line at a time, and calls `visit` per record.
/// Reads no more than `len` bytes, the size seen when the file was opened.
fn scan(
    file: File,
    len: u64,
    digest: &dyn Digest,
    head: Option<&Head>,
    visit: &mut dyn FnMut(&Link) -> Result<()>,
) -> Result<Scan> {
    let mut reader = BufReader::new(file.take(len));
    let mut verifier = ChainVerifier::new(digest);
    let mut buf: Vec<u8> = Vec::new();
    let mut valid_len = 0u64;
    let mut partial_tail = false;
    let limit = u64::try_from(MAX_EVENT_LINE)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    loop {
        buf.clear();
        let n = (&mut reader)
            .take(limit)
            .read_until(b'\n', &mut buf)
            .map_err(|e| FleetError::io("cannot read the event log", &e))?;
        if n == 0 {
            break;
        }
        let index = verifier.records().saturating_add(1);
        let terminated = buf.last() == Some(&b'\n');
        if terminated {
            buf.pop();
        } else if buf.len() > MAX_EVENT_LINE {
            return Err(tampered(format!(
                "record {index} is longer than {MAX_EVENT_LINE} bytes"
            )));
        } else {
            // End of file in the middle of a line. A crash during an append leaves a fragment
            // that does not parse; a complete record without its newline is not a crash
            // artifact of ours, so it is not silently accepted.
            if parse_link(&buf, index).is_ok() {
                return Err(tampered(format!(
                    "record {index} is not terminated by a newline"
                )));
            }
            partial_tail = true;
            break;
        }
        let link = parse_link(&buf, index).map_err(tampered)?;
        verifier
            .check(&link)
            .map_err(|fault| tampered(format!("event log: {fault}")))?;
        visit(&link)?;
        valid_len = valid_len.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
    }
    match head {
        Some(h) => verifier
            .check_head(h.records, &h.hash)
            .map_err(|fault| tampered(format!("event log: {fault}")))?,
        // The head exists from the moment the log does, so a log with records and no head
        // was cut back and had its head removed to hide it.
        None if verifier.records() > 0 => {
            return Err(tampered(format!(
                "event log has {} records but its head file is missing",
                verifier.records()
            )))
        }
        None => {}
    }
    Ok(Scan {
        records: verifier.records(),
        head_hash: verifier.head_hash().to_string(),
        valid_len,
        partial_tail,
    })
}

/// Verifies a log file without opening it for writing. Used by `fleet verify` and tests.
pub fn verify_file(path: &Path) -> Result<VerifyReport> {
    let head = read_head(&head_path(path))?;
    let (file, len) = open_log_read(path)?;
    let s = scan(file, len, &Sha256Digest, head.as_ref(), &mut |_| Ok(()))?;
    if s.partial_tail {
        return Err(tampered(format!(
            "event log ends with an incomplete record {}",
            s.records.saturating_add(1)
        )));
    }
    Ok(VerifyReport {
        records: s.records,
        head_hash: s.head_hash,
    })
}

pub struct FileEventStore {
    path: PathBuf,
    head_path: PathBuf,
    digest: Box<dyn SendDigest>,
    next_seq: u64,
    last_hash: String,
    size: u64,
}

impl FileEventStore {
    /// Opens (creating if needed) and fully verifies the log.
    pub fn open(path: &Path, digest: Box<dyn SendDigest>) -> Result<FileEventStore> {
        let head_path = head_path(path);
        match fsx::lstat_kind(path) {
            None => {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(OFlag::O_NOFOLLOW.bits())
                    .open(path)
                    .map_err(|e| {
                        FleetError::io(&format!("cannot create event log {}", path.display()), &e)
                    })?;
            }
            Some(FileKind::File) => {}
            Some(FileKind::Symlink) => {
                return Err(tampered(format!(
                    "event log {} is a symlink; refusing to use it",
                    path.display()
                )))
            }
            Some(_) => {
                return Err(tampered(format!(
                    "event log {} is not a regular file",
                    path.display()
                )))
            }
        }
        let head = read_head(&head_path)?;
        let (file, len) = open_log_read(path)?;
        let s = scan(file, len, &*digest, head.as_ref(), &mut |_| Ok(()))?;
        if head.is_none() {
            // Only a log without records can lack its head (scan refuses the others): it is
            // created together with the log, so that deleting it later is noticed.
            fsx::write_private_atomic(
                &head_path,
                format!("0 {}\n", fleet_domain::chain::GENESIS).as_bytes(),
            )?;
        }
        if s.partial_tail {
            // Drop the fragment of an interrupted append so the next record starts on a line.
            let f = OpenOptions::new()
                .write(true)
                .custom_flags(OFlag::O_NOFOLLOW.bits())
                .open(path)
                .map_err(|e| FleetError::io("cannot repair the event log", &e))?;
            f.set_len(s.valid_len)
                .and_then(|()| f.sync_data())
                .map_err(|e| FleetError::io("cannot repair the event log", &e))?;
        }
        Ok(FileEventStore {
            path: path.to_path_buf(),
            head_path,
            digest,
            next_seq: s.records.saturating_add(1),
            last_hash: s.head_hash,
            size: s.valid_len,
        })
    }

    fn write_head(&self, records: u64, hash: &str) -> Result<()> {
        fsx::write_private_atomic(&self.head_path, format!("{records} {hash}\n").as_bytes())
    }
}

fn link_line(link: &Link) -> String {
    json::to_line(&Value::object([
        ("seq", Value::int(link.seq)),
        ("ts", Value::int(link.ts_ms)),
        ("prev", Value::str(link.prev.clone())),
        ("body", link.body.clone()),
        ("hash", Value::str(link.hash.clone())),
    ]))
}

impl EventStore for FileEventStore {
    fn append(&mut self, ts_ms: u64, events: &[DomainEvent]) -> Result<u64> {
        if events.is_empty() {
            return Ok(self.next_seq.saturating_sub(1));
        }
        let mut batch = String::new();
        let mut seq = self.next_seq;
        let mut prev = self.last_hash.clone();
        for event in events {
            let link = seal(&*self.digest, seq, ts_ms, &prev, event.to_value());
            let line = link_line(&link);
            if line.len() > MAX_EVENT_LINE {
                return Err(FleetError::limit(format!(
                    "event record {seq} would exceed {MAX_EVENT_LINE} bytes"
                )));
            }
            batch.push_str(&line);
            batch.push('\n');
            seq = seq.saturating_add(1);
            prev = link.hash;
        }
        let batch_len = u64::try_from(batch.len()).unwrap_or(u64::MAX);
        if self.size.saturating_add(batch_len) > MAX_EVENT_LOG_BYTES {
            return Err(FleetError::limit(format!(
                "the event log reached its {MAX_EVENT_LOG_BYTES} byte limit"
            )));
        }

        let mut file = OpenOptions::new()
            .append(true)
            .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
            .open(&self.path)
            .map_err(|e| FleetError::io("cannot open the event log for appending", &e))?;
        let meta = file
            .metadata()
            .map_err(|e| FleetError::io("cannot examine the event log", &e))?;
        if !meta.is_file() || meta.len() != self.size {
            return Err(tampered(
                "the event log changed while the daemon was running",
            ));
        }
        // One write call for the whole batch; on failure cut back to the last good record so a
        // retry does not leave a torn line in the middle of the file.
        if let Err(e) = file
            .write_all(batch.as_bytes())
            .and_then(|()| file.sync_data())
        {
            let _ = file.set_len(self.size);
            return Err(FleetError::io("cannot append to the event log", &e));
        }
        self.next_seq = seq;
        self.last_hash = prev;
        self.size = self.size.saturating_add(batch_len);
        // The records are durable now. A failed head update only makes the head lag, which
        // the next append repairs and `check_head` tolerates, so it must not make the caller
        // believe the events were not stored.
        let _ = self.write_head(seq.saturating_sub(1), &self.last_hash);
        Ok(seq.saturating_sub(1))
    }

    fn replay(&self, visit: &mut dyn FnMut(StoredEvent) -> Result<()>) -> Result<ReplayInfo> {
        let head = read_head(&self.head_path)?;
        let (file, len) = open_log_read(&self.path)?;
        // Never read past what this store itself wrote.
        let len = len.min(self.size);
        let s = scan(file, len, &*self.digest, head.as_ref(), &mut |link| {
            let event = DomainEvent::from_value(&link.body)
                .map_err(|e| tampered(format!("record {} is damaged: {e}", link.seq)))?;
            visit(StoredEvent {
                seq: link.seq,
                ts_ms: link.ts_ms,
                event,
            })
        })?;
        Ok(ReplayInfo { records: s.records })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::fs;
    use std::os::unix::fs::{symlink, MetadataExt};

    fn open(t: &TempDir) -> FileEventStore {
        FileEventStore::open(&t.join("events.log"), Box::new(Sha256Digest)).unwrap()
    }

    fn warn(n: usize) -> DomainEvent {
        DomainEvent::Warning {
            message: format!("event {n}"),
        }
    }

    fn replay_all(store: &FileEventStore) -> Result<Vec<StoredEvent>> {
        let mut out = Vec::new();
        store.replay(&mut |e| {
            out.push(e);
            Ok(())
        })?;
        Ok(out)
    }

    fn fill(t: &TempDir, n: usize) -> FileEventStore {
        let mut s = open(t);
        for i in 1..=n {
            s.append(1000 + i as u64, &[warn(i)]).unwrap();
        }
        s
    }

    fn lines(t: &TempDir) -> Vec<String> {
        t.read("events.log").lines().map(str::to_string).collect()
    }

    fn set_lines(t: &TempDir, lines: &[String]) {
        fs::write(t.join("events.log"), format!("{}\n", lines.join("\n"))).unwrap();
    }

    fn reopen_err(t: &TempDir) -> FleetError {
        FileEventStore::open(&t.join("events.log"), Box::new(Sha256Digest))
            .err()
            .expect("expected an error")
    }

    #[test]
    fn append_and_replay_round_trip() {
        let t = TempDir::new("evlog-rt");
        let mut s = open(&t);
        s.append(10, &[warn(1), warn(2)]).unwrap();
        s.append(20, &[warn(3)]).unwrap();
        let got = replay_all(&s).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(
            (got[0].seq, got[0].ts_ms, got[0].event.clone()),
            (1, 10, warn(1))
        );
        assert_eq!((got[2].seq, got[2].ts_ms), (3, 20));
        assert_eq!(
            fs::metadata(t.join("events.log")).unwrap().mode() & 0o777,
            0o600
        );
        let report = verify_file(&t.join("events.log")).unwrap();
        assert_eq!(report.records, 3);
        assert_eq!(report.head_hash.len(), 64);
    }

    #[test]
    fn reopening_continues_the_chain() {
        let t = TempDir::new("evlog-reopen");
        drop(fill(&t, 3));
        let mut s = open(&t);
        s.append(2000, &[warn(4)]).unwrap();
        let got = replay_all(&s).unwrap();
        assert_eq!(
            got.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        assert_eq!(verify_file(&t.join("events.log")).unwrap().records, 4);
    }

    #[test]
    fn a_batch_is_written_as_complete_lines_together() {
        let t = TempDir::new("evlog-batch");
        let mut s = open(&t);
        s.append(5, &[warn(1), warn(2), warn(3)]).unwrap();
        let text = t.read("events.log");
        assert!(text.ends_with('\n'));
        assert_eq!(text.lines().count(), 3);
        // An empty batch changes nothing.
        s.append(6, &[]).unwrap();
        assert_eq!(t.read("events.log"), text);
    }

    #[test]
    fn the_head_file_is_written_and_private() {
        let t = TempDir::new("evlog-head");
        let _s = fill(&t, 2);
        let head = t.read("events.head");
        let mut parts = head.split_whitespace();
        assert_eq!(parts.next(), Some("2"));
        assert_eq!(parts.next().map(str::len), Some(64));
        assert_eq!(
            fs::metadata(t.join("events.head")).unwrap().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn modifying_a_byte_in_the_middle_is_detected_and_named() {
        let t = TempDir::new("evlog-modify");
        drop(fill(&t, 5));
        let mut l = lines(&t);
        l[2] = l[2].replace("event 3", "event X");
        set_lines(&t, &l);
        let err = reopen_err(&t);
        assert_eq!(err.code, ErrorCode::Tampered);
        assert!(err.message.contains("record 3"), "{}", err.message);
        assert_eq!(
            verify_file(&t.join("events.log")).unwrap_err().code,
            ErrorCode::Tampered
        );
    }

    #[test]
    fn deleting_a_middle_line_is_detected() {
        let t = TempDir::new("evlog-delete");
        drop(fill(&t, 5));
        let mut l = lines(&t);
        l.remove(2);
        set_lines(&t, &l);
        let err = reopen_err(&t);
        assert_eq!(err.code, ErrorCode::Tampered);
        assert!(err.message.contains("record 4"), "{}", err.message);
    }

    #[test]
    fn swapping_two_lines_is_detected() {
        let t = TempDir::new("evlog-swap");
        drop(fill(&t, 5));
        let mut l = lines(&t);
        l.swap(1, 2);
        set_lines(&t, &l);
        assert_eq!(reopen_err(&t).code, ErrorCode::Tampered);
    }

    #[test]
    fn truncating_the_tail_is_caught_by_the_head_file() {
        let t = TempDir::new("evlog-trunc");
        drop(fill(&t, 5));
        let mut l = lines(&t);
        l.truncate(3);
        set_lines(&t, &l);
        let err = reopen_err(&t);
        assert_eq!(err.code, ErrorCode::Tampered);
        assert!(err.message.contains("3 records but 5"), "{}", err.message);
        assert_eq!(
            verify_file(&t.join("events.log")).unwrap_err().code,
            ErrorCode::Tampered
        );
        // Deleting the head as well must not hide the cut: see the next test.
    }

    #[test]
    fn a_new_log_gets_its_head_at_the_same_time() {
        let t = TempDir::new("evlog-newhead");
        drop(open(&t));
        assert_eq!(
            t.read("events.head"),
            format!("0 {}\n", fleet_domain::chain::GENESIS)
        );
        assert_eq!(
            fs::metadata(t.join("events.head")).unwrap().mode() & 0o777,
            0o600
        );
        let report = verify_file(&t.join("events.log")).unwrap();
        assert_eq!(report.records, 0);
        // A log that was cut back to nothing and lost its head is the empty log again: the
        // head is recreated, nothing else can be told.
        fs::remove_file(t.join("events.head")).unwrap();
        drop(open(&t));
        assert!(t.exists("events.head"));
    }

    #[test]
    fn cutting_the_log_at_a_record_boundary_and_deleting_the_head_is_detected() {
        let t = TempDir::new("evlog-cut-nohead");
        drop(fill(&t, 5));
        let mut l = lines(&t);
        l.truncate(3);
        set_lines(&t, &l);
        fs::remove_file(t.join("events.head")).unwrap();
        assert_eq!(reopen_err(&t).code, ErrorCode::Tampered);
        assert_eq!(
            verify_file(&t.join("events.log")).unwrap_err().code,
            ErrorCode::Tampered
        );
    }

    #[test]
    fn a_forged_line_with_a_wrong_hash_is_detected() {
        let t = TempDir::new("evlog-forge");
        drop(fill(&t, 2));
        let last = lines(&t).last().unwrap().clone();
        let forged = last
            .replace("\"seq\":2", "\"seq\":3")
            .replace("event 2", "event forged");
        let mut l = lines(&t);
        l.push(forged);
        set_lines(&t, &l);
        let err = reopen_err(&t);
        assert_eq!(err.code, ErrorCode::Tampered);
        assert!(err.message.contains("record 3"), "{}", err.message);
    }

    #[test]
    fn an_oversize_line_is_tampered_without_reading_it_all() {
        let t = TempDir::new("evlog-big");
        drop(fill(&t, 1));
        let mut text = t.read("events.log");
        text.push_str(&"x".repeat(MAX_EVENT_LINE * 2));
        text.push('\n');
        fs::write(t.join("events.log"), text).unwrap();
        let err = reopen_err(&t);
        assert_eq!(err.code, ErrorCode::Tampered);
        assert!(err.message.contains("longer than"), "{}", err.message);
    }

    #[test]
    fn a_symlinked_or_non_regular_log_is_refused() {
        let t = TempDir::new("evlog-link");
        let target = t.write("real.log", "");
        symlink(&target, t.join("events.log")).unwrap();
        let err = reopen_err(&t);
        assert_eq!(err.code, ErrorCode::Tampered);
        assert!(err.message.contains("symlink"));
        assert_eq!(
            verify_file(&t.join("events.log")).unwrap_err().code,
            ErrorCode::Tampered
        );
        assert_eq!(fs::read(&target).unwrap(), b"");

        let t2 = TempDir::new("evlog-dir");
        fs::create_dir(t2.join("events.log")).unwrap();
        assert_eq!(reopen_err(&t2).code, ErrorCode::Tampered);

        // A dangling symlink must not be created through.
        let t3 = TempDir::new("evlog-dangling");
        symlink(t3.join("elsewhere"), t3.join("events.log")).unwrap();
        assert!(FileEventStore::open(&t3.join("events.log"), Box::new(Sha256Digest)).is_err());
        assert!(!t3.exists("elsewhere"));
    }

    #[test]
    fn a_partial_last_line_from_a_crash_is_dropped() {
        let t = TempDir::new("evlog-partial");
        drop(fill(&t, 3));
        let mut text = t.read("events.log");
        let good_len = text.len();
        text.push_str("{\"seq\":4,\"ts\":1,\"pre");
        fs::write(t.join("events.log"), text).unwrap();
        let mut s = open(&t);
        assert_eq!(
            fs::metadata(t.join("events.log")).unwrap().len() as usize,
            good_len
        );
        s.append(9, &[warn(4)]).unwrap();
        assert_eq!(verify_file(&t.join("events.log")).unwrap().records, 4);
        assert_eq!(replay_all(&s).unwrap().len(), 4);
    }

    #[test]
    fn an_unterminated_but_complete_record_is_not_silently_accepted() {
        let t = TempDir::new("evlog-noeol");
        drop(fill(&t, 2));
        let text = t.read("events.log");
        fs::write(t.join("events.log"), text.trim_end()).unwrap();
        assert_eq!(reopen_err(&t).code, ErrorCode::Tampered);
    }

    #[test]
    fn a_partial_line_in_the_middle_is_tampered_not_repaired() {
        let t = TempDir::new("evlog-midpartial");
        drop(fill(&t, 3));
        let mut l = lines(&t);
        let cut = l[1].len() / 2;
        l[1].truncate(cut);
        set_lines(&t, &l);
        assert_eq!(reopen_err(&t).code, ErrorCode::Tampered);
    }

    #[test]
    fn an_unreadable_or_missing_head_is_tampered_once_the_log_has_records() {
        let t = TempDir::new("evlog-badhead");
        drop(fill(&t, 2));
        fs::write(t.join("events.head"), "garbage").unwrap();
        assert_eq!(reopen_err(&t).code, ErrorCode::Tampered);
        fs::remove_file(t.join("events.head")).unwrap();
        let err = reopen_err(&t);
        assert_eq!(err.code, ErrorCode::Tampered);
        assert!(
            err.message.contains("head file is missing"),
            "{}",
            err.message
        );
        assert!(
            !t.exists("events.head"),
            "opening must not recreate the head"
        );
        assert_eq!(
            verify_file(&t.join("events.log")).unwrap_err().code,
            ErrorCode::Tampered
        );
        // A head that lags by whole records (crash between log and head write) is accepted.
        let first = lines(&t)[0].clone();
        let hash = parse_link(first.as_bytes(), 1).unwrap().hash;
        fs::write(t.join("events.head"), format!("1 {hash}\n")).unwrap();
        assert_eq!(verify_file(&t.join("events.log")).unwrap().records, 2);
        fs::write(t.join("events.head"), format!("1 {}\n", "0".repeat(64))).unwrap();
        let _ = open(&t);
        // The same count with another hash is not.
        fs::write(t.join("events.head"), format!("2 {}\n", "0".repeat(64))).unwrap();
        assert_eq!(reopen_err(&t).code, ErrorCode::Tampered);
    }

    #[test]
    fn unknown_event_types_fail_replay() {
        let t = TempDir::new("evlog-unknown");
        drop(fill(&t, 1));
        // A correctly chained record whose body is not a known event.
        let link = seal(
            &Sha256Digest,
            2,
            5,
            &verify_file(&t.join("events.log")).unwrap().head_hash,
            Value::object([("type", Value::str("from_the_future"))]),
        );
        let mut l = lines(&t);
        l.push(link_line(&link));
        set_lines(&t, &l);
        // The head still lags the log by one record, which is accepted.
        let s = open(&t);
        let err = replay_all(&s).unwrap_err();
        assert_eq!(err.code, ErrorCode::Tampered);
        assert!(err.message.contains("record 2"), "{}", err.message);
    }

    #[test]
    fn the_size_limit_refuses_appends_and_leaves_the_chain_intact() {
        let t = TempDir::new("evlog-limit");
        let mut s = fill(&t, 1);
        s.size = MAX_EVENT_LOG_BYTES;
        let err = s.append(5, &[warn(2)]).unwrap_err();
        assert_eq!(err.code, ErrorCode::Limit);
        assert_eq!(s.next_seq, 2);
        assert_eq!(lines(&t).len(), 1);
    }

    #[test]
    fn a_record_that_could_not_be_reopened_is_refused_up_front() {
        let t = TempDir::new("evlog-bigrecord");
        let mut s = open(&t);
        let huge = DomainEvent::Warning {
            message: "y".repeat(MAX_EVENT_LINE),
        };
        let err = s.append(1, &[warn(1), huge]).unwrap_err();
        assert_eq!(err.code, ErrorCode::Limit);
        // Atomic: not even the first event of the batch was written.
        assert_eq!(t.read("events.log"), "");
        s.append(2, &[warn(2)]).unwrap();
        assert_eq!(verify_file(&t.join("events.log")).unwrap().records, 1);
    }

    #[test]
    fn appending_to_a_log_changed_behind_our_back_is_refused() {
        let t = TempDir::new("evlog-behind");
        let mut s = fill(&t, 2);
        let mut text = t.read("events.log");
        text.push_str("junk\n");
        fs::write(t.join("events.log"), text).unwrap();
        let err = s.append(9, &[warn(3)]).unwrap_err();
        assert_eq!(err.code, ErrorCode::Tampered);
    }
}
