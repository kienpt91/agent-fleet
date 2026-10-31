//! Compiled context packs and handoff packets on disk, `<dir>/<run>/<task>/`.
//!
//! Per task directory (0700, files 0600, written atomically):
//!
//! ```text
//! pack-000001-<first 16 hex of the digest>.md      the rendered pack
//! pack-000001-<first 16 hex of the digest>.json    its document (digest and manifest)
//! handoff-000001-<first 16 hex>.json                a handoff packet
//! ```
//!
//! The sequence number orders what was saved; saving a digest that is already stored replaces it
//! and puts it at the newest place. The newest 8 packs and 8 handoffs of a task are kept, and the
//! whole directory is held under 64 MiB (oldest files first, never what was just written).
//! Symlinks are never followed; files whose names do not match the scheme are ignored; a file that
//! is too large, damaged or whose digest disagrees with its name is an error, not a panic.

use crate::fsx::{self, FileKind};
use crate::json;
use fleet_app::ports::{ContextStore, StoredPack};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::{RunId, TaskId};
use fleet_domain::value::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAX_PACK_TEXT: u64 = 2 * 1024 * 1024;
const MAX_DOCUMENT: u64 = 256 * 1024;
const MAX_HANDOFF: u64 = 256 * 1024;
const KEPT_PER_TASK: usize = 8;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const DIGEST_LEN: usize = 64;
const SHORT_LEN: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    PackText,
    PackDoc,
    Handoff,
}

#[derive(Clone, Debug)]
struct Entry {
    kind: Kind,
    seq: u64,
    short: String,
    path: PathBuf,
}

fn file_name(kind: Kind, seq: u64, short: &str) -> String {
    match kind {
        Kind::PackText => format!("pack-{seq:06}-{short}.md"),
        Kind::PackDoc => format!("pack-{seq:06}-{short}.json"),
        Kind::Handoff => format!("handoff-{seq:06}-{short}.json"),
    }
}

/// `(kind, seq, short digest)` of a file name of the scheme, `None` for anything else.
fn parse_name(name: &str) -> Option<(Kind, u64, String)> {
    let (kind, rest) = if let Some(r) = name.strip_prefix("pack-") {
        if let Some(s) = r.strip_suffix(".md") {
            (Kind::PackText, s)
        } else {
            (Kind::PackDoc, r.strip_suffix(".json")?)
        }
    } else {
        (
            Kind::Handoff,
            name.strip_prefix("handoff-")?.strip_suffix(".json")?,
        )
    };
    let (seq_text, short) = rest.split_once('-')?;
    if seq_text.len() < 6 || !seq_text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let seq: u64 = seq_text.parse().ok()?;
    if short.len() != SHORT_LEN || !is_lower_hex(short) {
        return None;
    }
    // Only the canonical spelling of a name counts.
    (file_name(kind, seq, short) == name).then(|| (kind, seq, short.to_string()))
}

fn is_lower_hex(s: &str) -> bool {
    s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn valid_digest(s: &str) -> bool {
    s.len() == DIGEST_LEN && is_lower_hex(s)
}

fn short_of(digest: &str) -> &str {
    digest.get(..SHORT_LEN).unwrap_or(digest)
}

fn safe_component(s: &str) -> Result<&str> {
    if s.is_empty() || s == "." || s == ".." || s.contains(['/', '\0', '\\']) {
        return Err(FleetError::invalid(
            "an id is not usable as a directory name",
        ));
    }
    Ok(s)
}

fn validate_prefix(prefix: &str) -> Result<()> {
    if prefix.len() < 8 || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(FleetError::invalid(
            "a digest is given as at least 8 hexadecimal digits",
        ));
    }
    Ok(())
}

fn pick_unique<T>(found: Vec<T>, prefix: &str) -> Result<Option<T>> {
    let mut found = found.into_iter();
    let first = found.next();
    if found.next().is_some() {
        return Err(FleetError::invalid(format!(
            "the digest prefix {prefix} matches more than one saved item"
        )));
    }
    Ok(first)
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn read_bytes(path: &Path, max: u64) -> Result<Vec<u8>> {
    fsx::read_regular(path, max)
        .map_err(|e| FleetError::invalid(format!("stored context file {} {e}", name_of(path))))
}

fn read_text(path: &Path, max: u64) -> Result<String> {
    String::from_utf8(read_bytes(path, max)?).map_err(|_| {
        FleetError::invalid(format!(
            "stored context file {} is not valid UTF-8",
            name_of(path)
        ))
    })
}

/// A stored JSON file with a `digest` field that must agree with the short digest in its name.
fn read_document(path: &Path, max: u64, short: &str) -> Result<(String, Value)> {
    let bytes = read_bytes(path, max)?;
    let limit = usize::try_from(max).unwrap_or(usize::MAX);
    let value = json::parse_value(&bytes, limit).map_err(|e| {
        FleetError::invalid(format!(
            "stored context file {} is damaged: {e}",
            name_of(path)
        ))
    })?;
    let digest = value
        .get("digest")
        .and_then(Value::as_str)
        .filter(|d| valid_digest(d) && short_of(d) == short)
        .ok_or_else(|| {
            FleetError::invalid(format!(
                "stored context file {} has no digest matching its name",
                name_of(path)
            ))
        })?
        .to_string();
    Ok((digest, value))
}

fn io_error(what: &str, e: &std::io::Error) -> FleetError {
    FleetError::io(what, e)
}

/// One pack on disk: both of its files.
struct PackFiles {
    seq: u64,
    short: String,
    text: PathBuf,
    doc: PathBuf,
}

pub struct FileContextStore {
    dir: PathBuf,
    max_total: u64,
}

impl FileContextStore {
    pub fn new(dir: PathBuf) -> FileContextStore {
        FileContextStore {
            dir,
            max_total: MAX_TOTAL_BYTES,
        }
    }

    /// Another bound for the whole directory (tests).
    #[must_use]
    pub fn with_max_total_bytes(mut self, max: u64) -> FileContextStore {
        self.max_total = max;
        self
    }

    fn run_dir(&self, run: &RunId) -> Result<PathBuf> {
        Ok(self.dir.join(safe_component(run.as_str())?))
    }

    fn task_dir(&self, run: &RunId, task: &TaskId) -> Result<PathBuf> {
        Ok(self.run_dir(run)?.join(safe_component(task.as_str())?))
    }

    /// The task's directory when it exists; `None` when it does not; an error for a symlink or a
    /// file in its place.
    fn existing_task_dir(&self, run: &RunId, task: &TaskId) -> Result<Option<PathBuf>> {
        let run_dir = self.run_dir(run)?;
        let task_dir = run_dir.join(safe_component(task.as_str())?);
        for p in [&run_dir, &task_dir] {
            match fsx::lstat_kind(p) {
                None => return Ok(None),
                Some(FileKind::Dir) => {}
                Some(_) => {
                    return Err(FleetError::invalid(format!(
                        "{} is not a directory",
                        name_of(p)
                    )))
                }
            }
        }
        Ok(Some(task_dir))
    }

    fn prepare_task_dir(&self, run: &RunId, task: &TaskId) -> Result<PathBuf> {
        let run_dir = self.run_dir(run)?;
        let task_dir = run_dir.join(safe_component(task.as_str())?);
        fsx::ensure_private_dir(&self.dir)?;
        fsx::ensure_private_dir(&run_dir)?;
        fsx::ensure_private_dir(&task_dir)?;
        Ok(task_dir)
    }

    fn scan(dir: &Path) -> Result<Vec<Entry>> {
        let read = match fs::read_dir(dir) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_error("cannot read the context directory", &e)),
        };
        let mut out = Vec::new();
        for entry in read.flatten() {
            // `DirEntry::file_type` does not follow symlinks.
            if !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let name = entry.file_name();
            let Some((kind, seq, short)) = name.to_str().and_then(parse_name) else {
                continue;
            };
            out.push(Entry {
                kind,
                seq,
                short,
                path: entry.path(),
            });
        }
        Ok(out)
    }

    /// Complete packs (text and document), oldest first.
    fn packs(entries: &[Entry]) -> Vec<PackFiles> {
        type Pair = (Option<PathBuf>, Option<PathBuf>);
        let mut map: BTreeMap<(u64, String), Pair> = BTreeMap::new();
        for e in entries {
            let slot = map.entry((e.seq, e.short.clone())).or_default();
            match e.kind {
                Kind::PackText => slot.0 = Some(e.path.clone()),
                Kind::PackDoc => slot.1 = Some(e.path.clone()),
                Kind::Handoff => {}
            }
        }
        map.into_iter()
            .filter_map(|((seq, short), (text, doc))| {
                Some(PackFiles {
                    seq,
                    short,
                    text: text?,
                    doc: doc?,
                })
            })
            .collect()
    }

    fn handoffs(entries: &[Entry]) -> Vec<&Entry> {
        let mut v: Vec<&Entry> = entries.iter().filter(|e| e.kind == Kind::Handoff).collect();
        v.sort_by(|a, b| (a.seq, &a.short).cmp(&(b.seq, &b.short)));
        v
    }

    fn next_seq(entries: &[Entry], pack: bool) -> Result<u64> {
        let max = entries
            .iter()
            .filter(|e| (e.kind == Kind::Handoff) != pack)
            .map(|e| e.seq)
            .max()
            .unwrap_or(0);
        max.checked_add(1)
            .ok_or_else(|| FleetError::internal("the context sequence is exhausted"))
    }

    /// Keeps the newest few of a task; everything else (and half-written packs) goes, best effort.
    fn prune(dir: &Path) {
        let Ok(entries) = Self::scan(dir) else {
            return;
        };
        let complete = Self::packs(&entries);
        let keep_from = complete.len().saturating_sub(KEPT_PER_TASK);
        let mut doomed: Vec<PathBuf> = Vec::new();
        for p in complete.iter().take(keep_from) {
            doomed.push(p.text.clone());
            doomed.push(p.doc.clone());
        }
        for e in &entries {
            if e.kind != Kind::Handoff
                && !complete
                    .iter()
                    .any(|p| p.seq == e.seq && p.short == e.short)
            {
                doomed.push(e.path.clone());
            }
        }
        let handoffs = Self::handoffs(&entries);
        let old = handoffs.len().saturating_sub(KEPT_PER_TASK);
        doomed.extend(handoffs.iter().take(old).map(|e| e.path.clone()));
        for path in doomed {
            let _ = fsx::remove_nofollow(&path);
        }
    }

    /// Holds the whole directory under the bound, oldest files first, never the `keep` files.
    fn enforce_total(&self, keep: &[PathBuf]) {
        let mut files: Vec<(SystemTime, u64, PathBuf, u64)> = Vec::new();
        let mut total: u64 = 0;
        for run in subdirs(&self.dir) {
            for task in subdirs(&run) {
                let Ok(entries) = Self::scan(&task) else {
                    continue;
                };
                for e in entries {
                    let Ok(meta) = fs::symlink_metadata(&e.path) else {
                        continue;
                    };
                    total = total.saturating_add(meta.len());
                    let when = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    files.push((when, e.seq, e.path, meta.len()));
                }
            }
        }
        if total <= self.max_total {
            return;
        }
        files.sort();
        for (_, _, path, len) in files {
            if total <= self.max_total {
                break;
            }
            if keep.contains(&path) {
                continue;
            }
            if fsx::remove_nofollow(&path).is_ok() {
                total = total.saturating_sub(len);
                if let Some(parent) = path.parent() {
                    // Empty directories go too; non-empty ones refuse, which is fine.
                    let _ = fs::remove_dir(parent);
                    if let Some(grand) = parent.parent() {
                        let _ = fs::remove_dir(grand);
                    }
                }
            }
        }
    }

    fn read_pack(files: &PackFiles) -> Result<StoredPack> {
        let (digest, document) = read_document(&files.doc, MAX_DOCUMENT, &files.short)?;
        let text = read_text(&files.text, MAX_PACK_TEXT)?;
        Ok(StoredPack {
            digest,
            text,
            document,
        })
    }
}

/// Real subdirectories of `dir` (symlinks are not followed), best effort.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    read.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect()
}

impl ContextStore for FileContextStore {
    fn save_pack(&mut self, run: &RunId, task: &TaskId, pack: &StoredPack) -> Result<()> {
        if !valid_digest(&pack.digest) {
            return Err(FleetError::invalid(
                "a pack digest is 64 lowercase hexadecimal digits",
            ));
        }
        if pack.document.get("digest").and_then(Value::as_str) != Some(pack.digest.as_str()) {
            return Err(FleetError::invalid(
                "the digest of the pack document differs from the pack digest",
            ));
        }
        let document = pack.document.canonical();
        if u64::try_from(pack.text.len()).unwrap_or(u64::MAX) > MAX_PACK_TEXT
            || u64::try_from(document.len()).unwrap_or(u64::MAX) > MAX_DOCUMENT
        {
            return Err(FleetError::invalid("the pack is too large to be stored"));
        }
        let dir = self.prepare_task_dir(run, task)?;
        let entries = Self::scan(&dir)?;
        let seq = Self::next_seq(&entries, true)?;
        let short = short_of(&pack.digest);
        let text_path = dir.join(file_name(Kind::PackText, seq, short));
        let doc_path = dir.join(file_name(Kind::PackDoc, seq, short));
        fsx::write_private_atomic(&text_path, pack.text.as_bytes())?;
        if let Err(e) = fsx::write_private_atomic(&doc_path, document.as_bytes()) {
            let _ = fsx::remove_nofollow(&text_path);
            return Err(e);
        }
        // The same digest saved before is replaced: only the new copy stays.
        for e in entries
            .iter()
            .filter(|e| e.kind != Kind::Handoff && e.short == short)
        {
            let _ = fsx::remove_nofollow(&e.path);
        }
        Self::prune(&dir);
        self.enforce_total(&[text_path, doc_path]);
        Ok(())
    }

    fn load_pack(
        &self,
        run: &RunId,
        task: &TaskId,
        digest: Option<&str>,
    ) -> Result<Option<StoredPack>> {
        if let Some(prefix) = digest {
            validate_prefix(prefix)?;
        }
        let Some(dir) = self.existing_task_dir(run, task)? else {
            return Ok(None);
        };
        let packs = Self::packs(&Self::scan(&dir)?);
        let Some(prefix) = digest else {
            return match packs.last() {
                Some(p) => Self::read_pack(p).map(Some),
                None => Ok(None),
            };
        };
        let narrow = prefix.get(..SHORT_LEN).unwrap_or(prefix);
        let mut found = Vec::new();
        for p in packs.iter().filter(|p| p.short.starts_with(narrow)) {
            let pack = Self::read_pack(p)?;
            if pack.digest.starts_with(prefix) {
                found.push(pack);
            }
        }
        pick_unique(found, prefix)
    }

    fn list_packs(&self, run: &RunId, task: &TaskId) -> Result<Vec<String>> {
        let Some(dir) = self.existing_task_dir(run, task)? else {
            return Ok(Vec::new());
        };
        Self::packs(&Self::scan(&dir)?)
            .iter()
            .map(|p| read_document(&p.doc, MAX_DOCUMENT, &p.short).map(|(d, _)| d))
            .collect()
    }

    fn save_handoff(
        &mut self,
        run: &RunId,
        task: &TaskId,
        digest: &str,
        packet: &Value,
    ) -> Result<()> {
        if !valid_digest(digest) {
            return Err(FleetError::invalid(
                "a handoff digest is 64 lowercase hexadecimal digits",
            ));
        }
        if packet.get("digest").and_then(Value::as_str) != Some(digest) {
            return Err(FleetError::invalid(
                "the digest of the handoff packet differs from its digest",
            ));
        }
        let json = packet.canonical();
        if u64::try_from(json.len()).unwrap_or(u64::MAX) > MAX_HANDOFF {
            return Err(FleetError::invalid(
                "the handoff packet is too large to be stored",
            ));
        }
        let dir = self.prepare_task_dir(run, task)?;
        let entries = Self::scan(&dir)?;
        let seq = Self::next_seq(&entries, false)?;
        let short = short_of(digest);
        let path = dir.join(file_name(Kind::Handoff, seq, short));
        fsx::write_private_atomic(&path, json.as_bytes())?;
        for e in entries
            .iter()
            .filter(|e| e.kind == Kind::Handoff && e.short == short)
        {
            let _ = fsx::remove_nofollow(&e.path);
        }
        Self::prune(&dir);
        self.enforce_total(&[path]);
        Ok(())
    }

    fn load_handoff(
        &self,
        run: &RunId,
        task: &TaskId,
        digest: Option<&str>,
    ) -> Result<Option<(String, Value)>> {
        if let Some(prefix) = digest {
            validate_prefix(prefix)?;
        }
        let Some(dir) = self.existing_task_dir(run, task)? else {
            return Ok(None);
        };
        let entries = Self::scan(&dir)?;
        let handoffs = Self::handoffs(&entries);
        let Some(prefix) = digest else {
            return match handoffs.last() {
                Some(e) => read_document(&e.path, MAX_HANDOFF, &e.short).map(Some),
                None => Ok(None),
            };
        };
        let narrow = prefix.get(..SHORT_LEN).unwrap_or(prefix);
        let mut found = Vec::new();
        for e in handoffs.iter().filter(|e| e.short.starts_with(narrow)) {
            let (d, v) = read_document(&e.path, MAX_HANDOFF, &e.short)?;
            if d.starts_with(prefix) {
                found.push((d, v));
            }
        }
        pick_unique(found, prefix)
    }

    fn remove(&mut self, run: &RunId, task: &TaskId) {
        let (Ok(run_dir), Ok(task_dir)) = (self.run_dir(run), self.task_dir(run, task)) else {
            return;
        };
        if fsx::lstat_kind(&run_dir) != Some(FileKind::Dir) {
            return;
        }
        if fsx::lstat_kind(&task_dir) == Some(FileKind::Dir) {
            // Removes links inside as links, never what they point to.
            let _ = fs::remove_dir_all(&task_dir);
        }
        // Fails (and is ignored) while other tasks still have packs.
        let _ = fs::remove_dir(&run_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::os::unix::fs::{symlink, MetadataExt};

    fn ids() -> (RunId, TaskId) {
        (RunId::parse("r1").unwrap(), TaskId::parse("t1").unwrap())
    }

    fn digest(seed: u32) -> String {
        format!("{seed:08x}").repeat(8)
    }

    fn pack(seed: u32) -> StoredPack {
        let d = digest(seed);
        StoredPack {
            text: format!("text {seed}"),
            document: Value::object([("digest", Value::str(&d)), ("n", Value::int(seed))]),
            digest: d,
        }
    }

    fn packet(seed: u32) -> Value {
        Value::object([
            ("digest", Value::str(digest(seed))),
            ("summary", Value::str("s")),
        ])
    }

    fn store(t: &TempDir) -> FileContextStore {
        FileContextStore::new(t.join("context"))
    }

    fn task_files(t: &TempDir) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(t.join("context/r1/t1"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn names_round_trip_and_hostile_names_do_not_parse() {
        let short = "0123456789abcdef";
        for kind in [Kind::PackText, Kind::PackDoc, Kind::Handoff] {
            let n = file_name(kind, 7, short);
            assert_eq!(parse_name(&n), Some((kind, 7, short.to_string())));
        }
        for bad in [
            "pack-1-0123456789abcdef.md",
            "pack-0000001-0123456789abcdef.md",
            "pack-000001-0123456789ABCDEF.md",
            "pack-000001-0123456789abcde.md",
            "pack-000001-0123456789abcdef.txt",
            "pack-+00001-0123456789abcdef.md",
            "pack-000001-0123456789abcdef.md.tmp",
            "../pack-000001-0123456789abcdef.md",
            ".pack-000001-0123456789abcdef.md.tmp.1.2",
            "handoff-000001-0123456789abcdef.md",
            "notes.md",
        ] {
            assert_eq!(parse_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn saves_loads_and_lists_in_order() {
        let t = TempDir::new("ctx-basic");
        let (r, k) = ids();
        let mut s = store(&t);
        assert_eq!(s.load_pack(&r, &k, None).unwrap(), None);
        assert!(s.list_packs(&r, &k).unwrap().is_empty());
        s.save_pack(&r, &k, &pack(1)).unwrap();
        s.save_pack(&r, &k, &pack(2)).unwrap();
        assert_eq!(s.load_pack(&r, &k, None).unwrap(), Some(pack(2)));
        assert_eq!(s.list_packs(&r, &k).unwrap(), vec![digest(1), digest(2)]);
        assert_eq!(
            s.load_pack(&r, &k, Some(&digest(1)[..8])).unwrap(),
            Some(pack(1))
        );
        assert_eq!(
            s.load_pack(&r, &k, Some(&digest(1))).unwrap(),
            Some(pack(1))
        );
        assert_eq!(s.load_pack(&r, &k, Some("deadbeef")).unwrap(), None);
        // a second handle on the same directory sees the same data
        assert_eq!(store(&t).load_pack(&r, &k, None).unwrap(), Some(pack(2)));
    }

    #[test]
    fn files_are_private_and_named_by_sequence_and_digest() {
        let t = TempDir::new("ctx-modes");
        let (r, k) = ids();
        let mut s = store(&t);
        s.save_pack(&r, &k, &pack(1)).unwrap();
        s.save_handoff(&r, &k, &digest(5), &packet(5)).unwrap();
        let short = &digest(1)[..16];
        let short5 = &digest(5)[..16];
        assert_eq!(
            task_files(&t),
            vec![
                format!("handoff-000001-{short5}.json"),
                format!("pack-000001-{short}.json"),
                format!("pack-000001-{short}.md"),
            ]
        );
        for dir in ["context", "context/r1", "context/r1/t1"] {
            assert_eq!(fs::metadata(t.join(dir)).unwrap().mode() & 0o777, 0o700);
        }
        for f in task_files(&t) {
            let m = fs::metadata(t.join(&format!("context/r1/t1/{f}"))).unwrap();
            assert_eq!(m.mode() & 0o777, 0o600, "{f}");
        }
    }

    #[test]
    fn prefixes_are_checked_and_ambiguity_is_an_error() {
        let t = TempDir::new("ctx-prefix");
        let (r, k) = ids();
        let mut s = store(&t);
        let a = format!("abcdef01{}", "1".repeat(56));
        let b = format!("abcdef01{}", "2".repeat(56));
        for d in [&a, &b] {
            let p = StoredPack {
                digest: d.clone(),
                text: "x".to_string(),
                document: Value::object([("digest", Value::str(d))]),
            };
            s.save_pack(&r, &k, &p).unwrap();
        }
        assert!(s.load_pack(&r, &k, Some("abcdef01")).is_err());
        assert!(s.load_pack(&r, &k, Some("abcdef0")).is_err());
        assert!(s.load_pack(&r, &k, Some("zzzzzzzz")).is_err());
        assert!(s.load_pack(&r, &k, Some("")).is_err());
        assert_eq!(
            s.load_pack(&r, &k, Some(&format!("abcdef01{}", "1".repeat(20))))
                .unwrap()
                .unwrap()
                .digest,
            a
        );
        assert!(s.load_pack(&r, &k, Some("../../etc")).is_err());
    }

    #[test]
    fn saving_a_known_digest_replaces_it_at_the_newest_place() {
        let t = TempDir::new("ctx-replace");
        let (r, k) = ids();
        let mut s = store(&t);
        s.save_pack(&r, &k, &pack(1)).unwrap();
        s.save_pack(&r, &k, &pack(2)).unwrap();
        let mut again = pack(1);
        again.text = "newer text".to_string();
        s.save_pack(&r, &k, &again).unwrap();
        assert_eq!(s.list_packs(&r, &k).unwrap(), vec![digest(2), digest(1)]);
        assert_eq!(s.load_pack(&r, &k, None).unwrap(), Some(again));
        assert_eq!(task_files(&t).len(), 4);
    }

    #[test]
    fn only_the_newest_eight_are_kept() {
        let t = TempDir::new("ctx-retention");
        let (r, k) = ids();
        let mut s = store(&t);
        for i in 1..=11 {
            s.save_pack(&r, &k, &pack(i)).unwrap();
            s.save_handoff(&r, &k, &digest(i), &packet(i)).unwrap();
        }
        let expect: Vec<String> = (4..=11).map(digest).collect();
        assert_eq!(s.list_packs(&r, &k).unwrap(), expect);
        assert_eq!(task_files(&t).len(), 24);
        assert_eq!(s.load_pack(&r, &k, Some(&digest(3)[..8])).unwrap(), None);
        assert_eq!(s.load_handoff(&r, &k, None).unwrap().unwrap().0, digest(11));
        assert_eq!(s.load_handoff(&r, &k, Some(&digest(3)[..8])).unwrap(), None);
        assert!(s
            .load_handoff(&r, &k, Some(&digest(4)[..8]))
            .unwrap()
            .is_some());
    }

    #[test]
    fn handoffs_save_and_load_by_prefix_and_newest() {
        let t = TempDir::new("ctx-handoff");
        let (r, k) = ids();
        let mut s = store(&t);
        assert_eq!(s.load_handoff(&r, &k, None).unwrap(), None);
        s.save_handoff(&r, &k, &digest(1), &packet(1)).unwrap();
        s.save_handoff(&r, &k, &digest(2), &packet(2)).unwrap();
        assert_eq!(
            s.load_handoff(&r, &k, None).unwrap(),
            Some((digest(2), packet(2)))
        );
        assert_eq!(
            s.load_handoff(&r, &k, Some(&digest(1)[..8])).unwrap(),
            Some((digest(1), packet(1)))
        );
        assert!(s.load_handoff(&r, &k, Some("abc")).is_err());
        // a packet that does not carry its own digest is refused
        assert!(s
            .save_handoff(&r, &k, &digest(3), &Value::object([("a", Value::int(1))]))
            .is_err());
        assert!(s.save_handoff(&r, &k, "XYZ", &packet(3)).is_err());
        assert!(s.save_handoff(&r, &k, &digest(3), &packet(4)).is_err());
    }

    #[test]
    fn remove_forgets_the_task_and_the_empty_run_directory() {
        let t = TempDir::new("ctx-remove");
        let (r, k) = ids();
        let other = TaskId::parse("t2").unwrap();
        let mut s = store(&t);
        s.save_pack(&r, &k, &pack(1)).unwrap();
        s.save_handoff(&r, &k, &digest(1), &packet(1)).unwrap();
        s.save_pack(&r, &other, &pack(2)).unwrap();
        s.remove(&r, &k);
        assert_eq!(s.load_pack(&r, &k, None).unwrap(), None);
        assert_eq!(s.load_handoff(&r, &k, None).unwrap(), None);
        assert!(t.exists("context/r1/t2"));
        s.remove(&r, &other);
        assert!(!t.exists("context/r1"));
        s.remove(&r, &other); // nothing there: fine
    }

    #[test]
    fn hostile_files_are_ignored_and_damaged_ones_are_errors() {
        let t = TempDir::new("ctx-hostile");
        let (r, k) = ids();
        let mut s = store(&t);
        s.save_pack(&r, &k, &pack(1)).unwrap();
        let base = "context/r1/t1";
        t.write(&format!("{base}/notes.md"), "x");
        t.write(&format!("{base}/pack-1-short.md"), "x");
        symlink(
            t.join("context"),
            t.join(&format!("{base}/pack-000009-{}.md", &digest(9)[..16])),
        )
        .unwrap();
        assert_eq!(s.list_packs(&r, &k).unwrap(), vec![digest(1)]);
        assert_eq!(s.load_pack(&r, &k, None).unwrap(), Some(pack(1)));

        // a damaged document
        let doc = format!("{base}/pack-000001-{}.json", &digest(1)[..16]);
        t.write(&doc, "{not json");
        assert!(s.load_pack(&r, &k, None).is_err());
        assert!(s.list_packs(&r, &k).is_err());
        // a document whose digest disagrees with its name
        t.write(&doc, &pack(2).document.canonical());
        assert!(s.load_pack(&r, &k, None).is_err());
        // an oversized document
        t.write(&doc, &"x".repeat(300 * 1024));
        let err = s.load_pack(&r, &k, None).unwrap_err();
        assert!(err.message.contains("larger than"), "{}", err.message);
        // an oversized text
        t.write(&doc, &pack(1).document.canonical());
        t.write(
            &format!("{base}/pack-000001-{}.md", &digest(1)[..16]),
            &"x".repeat(2 * 1024 * 1024 + 1),
        );
        assert!(s.load_pack(&r, &k, None).is_err());
    }

    #[test]
    fn a_symlinked_pack_file_or_directory_is_never_followed() {
        let t = TempDir::new("ctx-links");
        let (r, k) = ids();
        let mut s = store(&t);
        s.save_pack(&r, &k, &pack(1)).unwrap();
        let base = t.join("context/r1/t1");
        let text = base.join(format!("pack-000001-{}.md", &digest(1)[..16]));
        fs::remove_file(&text).unwrap();
        let outside = t.write("outside.md", "secret");
        symlink(&outside, &text).unwrap();
        // the link is not a pack file: the pack is incomplete and invisible
        assert_eq!(s.load_pack(&r, &k, None).unwrap(), None);

        // saving through a symlinked task directory is refused and writes nothing outside
        let t2 = TempDir::new("ctx-links2");
        fs::create_dir_all(t2.join("context/r1")).unwrap();
        fs::create_dir(t2.join("elsewhere")).unwrap();
        symlink(t2.join("elsewhere"), t2.join("context/r1/t1")).unwrap();
        let mut s2 = store(&t2);
        assert!(s2.save_pack(&r, &k, &pack(1)).is_err());
        assert!(s2.load_pack(&r, &k, None).is_err());
        assert_eq!(fs::read_dir(t2.join("elsewhere")).unwrap().count(), 0);
        s2.remove(&r, &k);
        assert!(t2.exists("elsewhere"));
    }

    #[test]
    fn bad_digests_and_oversized_packs_are_refused() {
        let t = TempDir::new("ctx-refuse");
        let (r, k) = ids();
        let mut s = store(&t);
        let mut p = pack(1);
        p.digest = "../../x".to_string();
        assert!(s.save_pack(&r, &k, &p).is_err());
        let mut p = pack(1);
        p.digest = "A".repeat(64);
        assert!(s.save_pack(&r, &k, &p).is_err());
        let mut p = pack(1);
        p.document = Value::object([("digest", Value::str(digest(2)))]);
        assert!(s.save_pack(&r, &k, &p).is_err());
        let mut p = pack(1);
        p.text = "x".repeat(2 * 1024 * 1024 + 1);
        assert!(s.save_pack(&r, &k, &p).is_err());
        assert!(!t.exists("context/r1/t1") || task_files(&t).is_empty());
    }

    #[test]
    fn the_whole_directory_is_bounded_oldest_first_never_the_new_file() {
        let t = TempDir::new("ctx-total");
        let (r, k) = ids();
        let other = TaskId::parse("t2").unwrap();
        let mut s = store(&t).with_max_total_bytes(3000);
        let big = |seed: u32| {
            let mut p = pack(seed);
            p.text = "y".repeat(1000);
            p
        };
        s.save_pack(&r, &other, &big(1)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        s.save_pack(&r, &k, &big(2)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        s.save_pack(&r, &k, &big(3)).unwrap();
        assert_eq!(s.load_pack(&r, &other, None).unwrap(), None, "oldest went");
        assert_eq!(s.load_pack(&r, &k, None).unwrap(), Some(big(3)));
        // a bound smaller than one pack still keeps what was just written
        let mut tiny = store(&t).with_max_total_bytes(1);
        tiny.save_pack(&r, &k, &big(4)).unwrap();
        assert_eq!(tiny.load_pack(&r, &k, None).unwrap(), Some(big(4)));
    }

    #[test]
    fn a_task_directory_that_is_a_file_is_an_error() {
        let t = TempDir::new("ctx-file");
        let (r, k) = ids();
        t.write("context/r1/t1", "junk");
        let mut s = store(&t);
        assert!(s.load_pack(&r, &k, None).is_err());
        assert!(s.save_pack(&r, &k, &pack(1)).is_err());
    }
}
