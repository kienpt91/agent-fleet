//! The read-only workbench: the tree, a file, an image and a diff of the commits the daemon
//! recorded for a task, read from the **real repository** (ADR 16), never from the worktree a
//! worker can write to.
//!
//! A client names a task and one of its commits (`head`, `base`, or the id of a commit the daemon
//! recorded for that task); it never names a ref or a path on disk. The orchestrator resolves the
//! task to a [`RepoTarget`] (cheap, from its own state); the heavy reading happens here, on the
//! caller's thread, through the [`RepoReader`] port, so a slow repository never stalls the engine.
//! Results are documents ([`Value`]) with their own schema names, bounded by the limits of
//! `fleet_domain::repo::limits`, and cached by (commit, path, range) in a small bounded cache.

use crate::ports::{DiffRequest, EntryRead, RepoReader};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::{show, Sha};
use fleet_domain::repo::diff::{DiffBudget, DiffFile, DiffSet, Hunk, LineKind};
use fleet_domain::repo::limits::{
    DEFAULT_DIFF_CONTEXT, MAX_DIFF_CONTEXT, MAX_DIFF_FILES, MAX_FILE_VIEW_BYTES, MAX_IMAGE_BYTES,
    MAX_MARKDOWN_BYTES, MAX_TREE_ENTRIES,
};
use fleet_domain::repo::markdown;
use fleet_domain::repo::tree::{sort_entries, EntryKind};
use fleet_domain::repo::view::{image_info, FileContent, FileView, ImageKind, ImageLimits};
use fleet_domain::repo::RepoPath;
use fleet_domain::value::Value;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// The `subject` of the error that says a file is not an image (the dashboard answers it with
/// 415, unsupported media type, and every other invalid request with 400).
pub const NOT_AN_IMAGE: &str = "not_an_image";

/// The commits the daemon recorded for a task, and where its repository is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoTarget {
    /// `run/task`.
    pub task: String,
    pub repo: String,
    /// The run's base commit.
    pub base: Sha,
    /// The commit of the task's finished work, once it has one.
    pub head: Option<Sha>,
    /// The merge commit on the integration branch, once the task was merged.
    pub merged: Option<Sha>,
}

/// Which commit a request means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitSel {
    Head,
    Base,
    /// A commit id; it must be one the daemon recorded for the task.
    Sha(Sha),
}

impl CommitSel {
    /// `head`, `base` or a full commit id. Nothing else: no ref names, no revision syntax.
    pub fn parse(s: &str) -> Result<CommitSel> {
        match s {
            "head" => Ok(CommitSel::Head),
            "base" => Ok(CommitSel::Base),
            other => Sha::parse(other).map(CommitSel::Sha).map_err(|_| {
                FleetError::invalid(format!(
                    "commit must be `head`, `base` or a full commit id, not {}",
                    show(other)
                ))
            }),
        }
    }

    pub fn text(&self) -> String {
        match self {
            CommitSel::Head => "head".to_string(),
            CommitSel::Base => "base".to_string(),
            CommitSel::Sha(s) => s.as_str().to_string(),
        }
    }
}

impl RepoTarget {
    /// The commit a selector stands for, if the daemon recorded it for this task.
    pub fn resolve(&self, sel: &CommitSel) -> Result<Sha> {
        match sel {
            CommitSel::Head => self.head.clone().ok_or_else(|| {
                FleetError::not_found(format!(
                    "task {} has no recorded commit yet (its worker has not finished)",
                    self.task
                ))
            }),
            CommitSel::Base => Ok(self.base.clone()),
            CommitSel::Sha(s) => {
                let known = *s == self.base
                    || self.head.as_ref() == Some(s)
                    || self.merged.as_ref() == Some(s);
                if known {
                    Ok(s.clone())
                } else {
                    Err(FleetError::not_found(format!(
                        "{} is not a commit the daemon recorded for task {}",
                        show(s.as_str()),
                        self.task
                    )))
                }
            }
        }
    }
}

/// A read-only question about a task's repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoQuery {
    Tree {
        task: String,
        at: CommitSel,
        path: RepoPath,
    },
    File {
        task: String,
        at: CommitSel,
        path: RepoPath,
    },
    /// The bytes of an image.
    Blob {
        task: String,
        at: CommitSel,
        path: RepoPath,
    },
    Markdown {
        task: String,
        at: CommitSel,
        path: RepoPath,
    },
    Diff {
        task: String,
        base: CommitSel,
        head: CommitSel,
        context: u32,
    },
}

impl RepoQuery {
    /// The task the question is about, as the client wrote it.
    pub fn task(&self) -> &str {
        match self {
            RepoQuery::Tree { task, .. }
            | RepoQuery::File { task, .. }
            | RepoQuery::Blob { task, .. }
            | RepoQuery::Markdown { task, .. }
            | RepoQuery::Diff { task, .. } => task,
        }
    }

    /// Context lines of a diff request: the default when none was given, never more than the cap.
    pub fn clamp_context(requested: Option<u64>) -> u32 {
        requested.map_or(DEFAULT_DIFF_CONTEXT, |n| {
            u32::try_from(n.min(u64::from(MAX_DIFF_CONTEXT))).unwrap_or(MAX_DIFF_CONTEXT)
        })
    }
}

/// A question together with what the orchestrator resolved for it. The transport answers it with
/// [`RepoBrowser::run`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoRequest {
    pub target: RepoTarget,
    pub query: RepoQuery,
}

/// The bytes of an image the blob API may send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoImage {
    pub kind: ImageKind,
    /// The size the file's header declares (within the browser's limits, see `ImageLimits`).
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RepoReply {
    Doc(Value),
    Image(RepoImage),
}

// ---- the cache -----------------------------------------------------------------------------------

/// Documents kept for reuse. A commit never changes, so a document is valid for as long as it
/// is kept; the cache only bounds memory: a document larger than `MAX_ENTRY` is not kept, and the
/// oldest ones go first when `MAX_ENTRIES` or `MAX_BYTES` is exceeded.
struct Cache {
    docs: BTreeMap<String, (Value, usize)>,
    order: VecDeque<String>,
    bytes: usize,
}

const MAX_ENTRY: usize = 2 * 1024 * 1024;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_ENTRIES: usize = 64;

/// A rough size of a document in memory: enough to bound a cache, not exact.
pub fn approx_size(v: &Value) -> usize {
    match v {
        Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_) => 16,
        Value::Str(s) => 24 + s.len(),
        Value::Array(items) => 24 + items.iter().map(approx_size).sum::<usize>(),
        Value::Object(m) => {
            24 + m
                .iter()
                .map(|(k, v)| 24 + k.len() + approx_size(v))
                .sum::<usize>()
        }
    }
}

impl Cache {
    fn new() -> Cache {
        Cache {
            docs: BTreeMap::new(),
            order: VecDeque::new(),
            bytes: 0,
        }
    }

    fn get(&self, key: &str) -> Option<Value> {
        self.docs.get(key).map(|(v, _)| v.clone())
    }

    fn put(&mut self, key: String, doc: &Value) {
        let size = approx_size(doc);
        if size > MAX_ENTRY || self.docs.contains_key(&key) {
            return;
        }
        while !self.order.is_empty()
            && (self.docs.len() >= MAX_ENTRIES || self.bytes + size > MAX_BYTES)
        {
            if let Some(old) = self.order.pop_front() {
                if let Some((_, s)) = self.docs.remove(&old) {
                    self.bytes = self.bytes.saturating_sub(s);
                }
            }
        }
        self.bytes += size;
        self.order.push_back(key.clone());
        self.docs.insert(key, (doc.clone(), size));
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---- the service ---------------------------------------------------------------------------------

pub struct RepoBrowser {
    reader: Arc<dyn RepoReader>,
    images: ImageLimits,
    cache: Mutex<Cache>,
}

fn obj(schema: &str, pairs: Vec<(&str, Value)>) -> Value {
    let mut all: Vec<(String, Value)> = vec![("schema".to_string(), Value::str(schema))];
    all.extend(pairs.into_iter().map(|(k, v)| (k.to_string(), v)));
    Value::object(all)
}

fn opt_str(s: Option<&str>) -> Value {
    s.map_or(Value::Null, Value::str)
}

fn opt_int(n: Option<u64>) -> Value {
    n.map_or(Value::Null, Value::int)
}

impl RepoBrowser {
    pub fn new(reader: Arc<dyn RepoReader>) -> RepoBrowser {
        RepoBrowser {
            reader,
            images: ImageLimits::default(),
            cache: Mutex::new(Cache::new()),
        }
    }

    /// Another limit on the size of the images that are shown (clamped to the hard caps).
    pub fn with_image_limits(mut self, limits: ImageLimits) -> RepoBrowser {
        self.images = limits;
        self
    }

    /// Documents currently cached and their approximate size (for tests and diagnostics).
    pub fn cached(&self) -> (usize, usize) {
        let c = lock(&self.cache);
        (c.docs.len(), c.bytes)
    }

    /// Answers a question. The repository is only read, within the caps of the domain.
    pub fn run(&self, target: &RepoTarget, query: &RepoQuery) -> Result<RepoReply> {
        match query {
            RepoQuery::Tree { at, path, .. } => {
                let commit = target.resolve(at)?;
                // the selector is part of the key: the document says which one it was asked with
                let key = format!(
                    "tree\0{}\0{}\0{}\0{}\0{}",
                    target.repo,
                    target.task,
                    at.text(),
                    commit,
                    path
                );
                self.cached_doc(key, || self.tree(target, at, &commit, path))
            }
            RepoQuery::File { at, path, .. } => {
                let commit = target.resolve(at)?;
                // the selector is part of the key: the document says which one it was asked with
                let key = format!(
                    "file\0{}\0{}\0{}\0{}\0{}",
                    target.repo,
                    target.task,
                    at.text(),
                    commit,
                    path
                );
                self.cached_doc(key, || self.file(target, at, &commit, path))
            }
            RepoQuery::Blob { at, path, .. } => {
                let commit = target.resolve(at)?;
                self.blob(&target.repo, &commit, path).map(RepoReply::Image)
            }
            RepoQuery::Diff {
                base,
                head,
                context,
                ..
            } => {
                let base_sha = target.resolve(base)?;
                let head_sha = target.resolve(head)?;
                let key = format!(
                    "diff\0{}\0{}\0{}\0{}\0{}",
                    target.repo, target.task, base_sha, head_sha, context
                );
                self.cached_doc(key, || self.diff(target, &base_sha, &head_sha, *context))
            }
            RepoQuery::Markdown { at, path, .. } => {
                let commit = target.resolve(at)?;
                // the selector is part of the key: the document says which one it was asked with
                let key = format!(
                    "markdown\0{}\0{}\0{}\0{}\0{}",
                    target.repo,
                    target.task,
                    at.text(),
                    commit,
                    path
                );
                self.cached_doc(key, || self.markdown(target, at, &commit, path))
            }
        }
    }

    fn cached_doc(&self, key: String, build: impl FnOnce() -> Result<Value>) -> Result<RepoReply> {
        if let Some(v) = lock(&self.cache).get(&key) {
            return Ok(RepoReply::Doc(v));
        }
        let doc = build()?;
        lock(&self.cache).put(key, &doc);
        Ok(RepoReply::Doc(doc))
    }

    fn tree(
        &self,
        target: &RepoTarget,
        at: &CommitSel,
        commit: &Sha,
        path: &RepoPath,
    ) -> Result<Value> {
        let listing = self
            .reader
            .list_dir(&target.repo, commit, path, MAX_TREE_ENTRIES)?;
        let mut entries = listing.entries;
        entries.truncate(MAX_TREE_ENTRIES);
        sort_entries(&mut entries);
        let truncated = listing.truncated;
        Ok(obj(
            "fleet.repo.tree/1",
            vec![
                ("task", Value::str(&target.task)),
                ("at", Value::str(at.text())),
                ("commit", Value::str(commit.as_str())),
                ("path", Value::str(path.as_str())),
                (
                    "entries",
                    Value::array(entries.iter().map(|e| {
                        Value::object([
                            ("name", Value::str(&e.name)),
                            ("path", opt_str(e.path.as_ref().map(RepoPath::as_str))),
                            ("kind", Value::str(e.kind.as_str())),
                            ("size", opt_int(e.size)),
                            ("mode", Value::str(e.mode.as_str())),
                        ])
                    })),
                ),
                ("truncated", Value::Bool(truncated)),
                ("limit", Value::int(MAX_TREE_ENTRIES as u64)),
            ],
        ))
    }

    /// Reads an entry that is not a directory, within the larger of the text and image limits.
    fn read_file(&self, repo: &str, commit: &Sha, path: &RepoPath) -> Result<EntryRead> {
        let read = self
            .reader
            .read_entry(repo, commit, path, MAX_IMAGE_BYTES.max(MAX_FILE_VIEW_BYTES))?
            .ok_or_else(|| {
                FleetError::not_found(format!(
                    "{} does not exist at that commit",
                    show(path.as_str())
                ))
            })?;
        if read.stat.kind == EntryKind::Dir {
            return Err(FleetError::invalid(format!(
                "{} is a directory; list it instead",
                show(path.as_str())
            )));
        }
        Ok(read)
    }

    fn file(
        &self,
        target: &RepoTarget,
        at: &CommitSel,
        commit: &Sha,
        path: &RepoPath,
    ) -> Result<Value> {
        let read = self.read_file(&target.repo, commit, path)?;
        let view = FileView::build(
            path,
            read.stat.kind,
            read.stat.mode,
            read.stat.size,
            &read.stat.oid,
            read.content.as_deref(),
            &self.images,
        );
        Ok(file_doc(target, at, commit, &view))
    }

    fn markdown(
        &self,
        target: &RepoTarget,
        at: &CommitSel,
        commit: &Sha,
        path: &RepoPath,
    ) -> Result<Value> {
        let read = self.read_file(&target.repo, commit, path)?;
        let view = FileView::build(
            path,
            read.stat.kind,
            read.stat.mode,
            read.stat.size,
            &read.stat.oid,
            read.content.as_deref(),
            &self.images,
        );
        let bytes = match (&read.content, view.markdown) {
            (Some(b), true) => b,
            _ => {
                return Err(FleetError::invalid(format!(
                    "{} is not rendered as Markdown (it must be a text file named .md or .markdown of at most {MAX_MARKDOWN_BYTES} bytes)",
                    show(path.as_str())
                )))
            }
        };
        let lossy = String::from_utf8(bytes.clone()).is_err();
        let source = String::from_utf8_lossy(bytes);
        let doc = markdown::parse(&source, &path.parent());
        let notice = match (lossy, doc.truncated) {
            (true, true) => Some(
                "Invalid UTF-8 was replaced. The document is long: only the first part is shown.",
            ),
            (true, false) => Some("Invalid UTF-8 was replaced."),
            (false, true) => Some("The document is long: only the first part is shown."),
            (false, false) => None,
        };
        Ok(obj(
            "fleet.repo.markdown/1",
            vec![
                ("task", Value::str(&target.task)),
                ("at", Value::str(at.text())),
                ("commit", Value::str(commit.as_str())),
                ("path", Value::str(path.as_str())),
                ("truncated", Value::Bool(doc.truncated)),
                ("notice", opt_str(notice)),
                ("blocks", doc.to_value()),
            ],
        ))
    }

    fn blob(&self, repo: &str, commit: &Sha, path: &RepoPath) -> Result<RepoImage> {
        let read = self.read_file(repo, commit, path)?;
        if read.stat.kind != EntryKind::File {
            return Err(
                FleetError::invalid("only images are served, and this is not a file")
                    .with_subject(NOT_AN_IMAGE),
            );
        }
        let Some(bytes) = read.content else {
            return Err(FleetError::limit(format!(
                "the file is larger than the {MAX_IMAGE_BYTES} bytes the image viewer shows"
            )));
        };
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(FleetError::limit(format!(
                "the file is larger than the {MAX_IMAGE_BYTES} bytes the image viewer shows"
            )));
        }
        match image_info(&bytes) {
            // a few bytes can declare a canvas that makes a browser allocate gigabytes: the file
            // is not served at all when its declared size is above the limits
            Some(i) if !self.images.allows(i.width, i.height) => Err(FleetError::limit(format!(
                "the image declares {} x {} pixels, above the limits of {} per side and {} pixels",
                i.width,
                i.height,
                self.images.max_side(),
                self.images.max_pixels()
            ))),
            Some(i) => Ok(RepoImage {
                kind: i.kind,
                width: i.width,
                height: i.height,
                bytes,
            }),
            None => Err(FleetError::invalid(
                "only png, jpeg, gif and webp images are served (decided by the file's first bytes)",
            )
            .with_subject(NOT_AN_IMAGE)),
        }
    }

    fn diff(&self, target: &RepoTarget, base: &Sha, head: &Sha, context: u32) -> Result<Value> {
        let req = DiffRequest {
            context: context.min(MAX_DIFF_CONTEXT),
            max_files: MAX_DIFF_FILES,
            budget: DiffBudget::default(),
        };
        let set = self.reader.diff(&target.repo, base, head, &req)?;
        Ok(diff_doc(target, &set))
    }
}

// ---- documents -----------------------------------------------------------------------------------

fn file_doc(target: &RepoTarget, at: &CommitSel, commit: &Sha, view: &FileView) -> Value {
    let (kind_view, lines, line_count, truncated, clipped, lossy, crlf) = match &view.content {
        FileContent::Text(t) => (
            "text",
            t.lines().map(Value::str).collect::<Vec<_>>(),
            t.line_count,
            t.truncated,
            t.clipped,
            t.lossy,
            t.crlf,
        ),
        FileContent::Binary => ("binary", Vec::new(), 0, false, 0, false, false),
        FileContent::TooLarge => ("too_large", Vec::new(), 0, false, 0, false, false),
        FileContent::Image(_) => ("image", Vec::new(), 0, false, 0, false, false),
        FileContent::ImageTooBig(..) => ("image_too_large", Vec::new(), 0, false, 0, false, false),
        FileContent::Symlink { .. } => ("symlink", Vec::new(), 0, false, 0, false, false),
        FileContent::Submodule { .. } => ("submodule", Vec::new(), 0, false, 0, false, false),
    };
    let (target_text, target_path) = match &view.content {
        FileContent::Symlink {
            target,
            target_path,
        } => (
            Value::str(target),
            opt_str(target_path.as_ref().map(RepoPath::as_str)),
        ),
        _ => (Value::Null, Value::Null),
    };
    let image = match &view.content {
        FileContent::Image(i) | FileContent::ImageTooBig(i, _) => Value::str(i.kind.as_str()),
        _ => Value::Null,
    };
    let (width, height) = match &view.content {
        FileContent::Image(i) | FileContent::ImageTooBig(i, _) => {
            (Value::int(i.width), Value::int(i.height))
        }
        _ => (Value::Null, Value::Null),
    };
    let (max_side, max_pixels) = match &view.content {
        FileContent::ImageTooBig(_, l) => (Value::int(l.max_side()), Value::int(l.max_pixels())),
        _ => (Value::Null, Value::Null),
    };
    let submodule_commit = match &view.content {
        FileContent::Submodule { commit: Some(c) } => Value::str(c.as_str()),
        _ => Value::Null,
    };
    obj(
        "fleet.repo.file/1",
        vec![
            ("task", Value::str(&target.task)),
            ("at", Value::str(at.text())),
            ("commit", Value::str(commit.as_str())),
            ("path", Value::str(view.path.as_str())),
            ("kind", Value::str(view.kind.as_str())),
            ("size", opt_int(view.size)),
            ("mode", Value::str(view.mode.as_str())),
            ("view", Value::str(kind_view)),
            ("class", Value::str(view.class())),
            ("image", image),
            ("width", width),
            ("height", height),
            ("max_side", max_side),
            ("max_pixels", max_pixels),
            ("lines", Value::array(lines)),
            ("line_count", Value::int(line_count as u64)),
            ("truncated", Value::Bool(truncated)),
            ("clipped", Value::int(clipped as u64)),
            ("lossy", Value::Bool(lossy)),
            ("crlf", Value::Bool(crlf)),
            ("target", target_text),
            ("target_path", target_path),
            ("submodule_commit", submodule_commit),
            ("markdown", Value::Bool(view.markdown)),
            ("limit", Value::int(MAX_FILE_VIEW_BYTES as u64)),
            ("notice", Value::Null),
        ],
    )
}

fn mode_text(mode: Option<u32>) -> Value {
    mode.map_or(Value::Null, |m| Value::str(format!("{m:06o}")))
}

fn line_doc(l: &fleet_domain::repo::diff::DiffLine) -> Value {
    let mut pairs: Vec<(&str, Value)> = vec![
        ("k", Value::str(l.kind.as_str())),
        ("o", l.old.map_or(Value::Null, Value::int)),
        ("n", l.new.map_or(Value::Null, Value::int)),
        ("t", Value::str(&l.text)),
    ];
    if l.cr {
        pairs.push(("cr", Value::Bool(true)));
    }
    if l.no_newline {
        pairs.push(("nonl", Value::Bool(true)));
    }
    if !l.marks.is_empty() && l.kind != LineKind::Context {
        pairs.push((
            "w",
            Value::array(
                l.marks
                    .iter()
                    .map(|(s, e)| Value::array([Value::int(*s), Value::int(*e)])),
            ),
        ));
    }
    Value::object(pairs)
}

fn hunk_doc(h: &Hunk) -> Value {
    Value::object([
        ("old_start", Value::int(h.old_start)),
        ("old_count", Value::int(h.old_count)),
        ("new_start", Value::int(h.new_start)),
        ("new_count", Value::int(h.new_count)),
        ("section", Value::str(&h.section)),
        ("lines", Value::array(h.lines.iter().map(line_doc))),
    ])
}

fn diff_file_doc(f: &DiffFile) -> Value {
    Value::object([
        ("path", Value::str(&f.path)),
        ("old_path", opt_str(f.old_path.as_deref())),
        ("path_ok", Value::Bool(f.path_ok)),
        ("old_path_ok", Value::Bool(f.old_path_ok)),
        ("status", Value::str(f.status.as_str())),
        (
            "similarity",
            f.similarity
                .map_or(Value::Null, |s| Value::int(u64::from(s))),
        ),
        ("old_mode", mode_text(f.old_mode)),
        ("new_mode", mode_text(f.new_mode)),
        ("binary", Value::Bool(f.binary)),
        ("submodule", Value::Bool(f.submodule)),
        ("additions", Value::int(f.additions)),
        ("deletions", Value::int(f.deletions)),
        ("truncated", Value::Bool(f.truncated)),
        ("note", opt_str(f.note.as_deref())),
        ("hunks", Value::array(f.hunks.iter().map(hunk_doc))),
    ])
}

fn diff_doc(target: &RepoTarget, set: &DiffSet) -> Value {
    let stats = set.stats();
    obj(
        "fleet.repo.diff/1",
        vec![
            ("task", Value::str(&target.task)),
            ("base", Value::str(set.base.as_str())),
            ("head", Value::str(set.head.as_str())),
            ("context", Value::int(set.context)),
            (
                "stats",
                Value::object([
                    ("files", Value::int(stats.files as u64)),
                    ("additions", Value::int(stats.additions)),
                    ("deletions", Value::int(stats.deletions)),
                    ("binary", Value::int(stats.binary as u64)),
                ]),
            ),
            ("truncated", Value::Bool(set.truncated)),
            ("notice", opt_str(set.notice.as_deref())),
            ("files", Value::array(set.files.iter().map(diff_file_doc))),
        ],
    )
}
