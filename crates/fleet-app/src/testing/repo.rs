//! An in-memory [`RepoReader`] and the contract every reader (the fake and the real git adapter)
//! must satisfy. The fake keeps full snapshots per commit and produces its diffs through the
//! domain's own patch reader, so the contract also exercises what the domain makes of a patch.

use crate::ports::{DiffRequest, DirListing, EntryRead, EntryStat, RepoReader};
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::ids::Sha;
use fleet_domain::repo::diff::{
    assemble_file, parse_file_patch, DiffBudget, DiffSet, FileChange, FileStatus, LineKind, NumStat,
};
use fleet_domain::repo::tree::{EntryKind, ModeClass, TreeEntry};
use fleet_domain::repo::RepoPath;
use std::collections::{BTreeMap, BTreeSet};

/// What a path holds in a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    File(Vec<u8>),
    Exec(Vec<u8>),
    Symlink(String),
    /// A gitlink: the commit it records.
    Submodule(String),
}

impl Item {
    fn kind(&self) -> EntryKind {
        match self {
            Item::File(_) | Item::Exec(_) => EntryKind::File,
            Item::Symlink(_) => EntryKind::Symlink,
            Item::Submodule(_) => EntryKind::Submodule,
        }
    }

    fn mode(&self) -> u32 {
        match self {
            Item::File(_) => 0o100_644,
            Item::Exec(_) => 0o100_755,
            Item::Symlink(_) => 0o120_000,
            Item::Submodule(_) => 0o160_000,
        }
    }

    fn bytes(&self) -> &[u8] {
        match self {
            Item::File(b) | Item::Exec(b) => b,
            Item::Symlink(t) => t.as_bytes(),
            Item::Submodule(_) => &[],
        }
    }
}

/// A repository: a list of commits, each a full snapshot of its files.
#[derive(Clone, Debug, Default)]
pub struct RepoSpec {
    pub commits: Vec<Vec<(String, Item)>>,
}

/// A repository a reader was built over: the reader, the repository's name for it and the ids of
/// the commits, in the order of the spec.
pub struct RepoFixture {
    pub reader: Box<dyn RepoReader>,
    pub repo: String,
    pub commits: Vec<Sha>,
}

pub trait RepoHarness {
    fn build(&mut self, spec: &RepoSpec) -> RepoFixture;
}

// ---- the fake ------------------------------------------------------------------------------------

#[derive(Default)]
pub struct MemoryRepo {
    snapshots: BTreeMap<String, BTreeMap<String, Item>>,
}

impl MemoryRepo {
    pub fn new() -> MemoryRepo {
        MemoryRepo::default()
    }

    /// Records a commit.
    pub fn commit(&mut self, sha: &Sha, files: Vec<(String, Item)>) {
        self.snapshots
            .insert(sha.as_str().to_string(), files.into_iter().collect());
    }

    fn snapshot(&self, commit: &Sha) -> Result<&BTreeMap<String, Item>> {
        self.snapshots
            .get(commit.as_str())
            .ok_or_else(|| FleetError::not_found(format!("commit {commit} does not exist")))
    }
}

fn stat_of(item: &Item) -> EntryStat {
    EntryStat {
        kind: item.kind(),
        mode: ModeClass::from_mode(item.mode()),
        size: match item {
            Item::Submodule(_) => None,
            other => Some(other.bytes().len() as u64),
        },
        oid: match item {
            Item::Submodule(s) => s.clone(),
            _ => "0".repeat(40),
        },
    }
}

impl RepoReader for MemoryRepo {
    fn list_dir(
        &self,
        _repo: &str,
        commit: &Sha,
        dir: &RepoPath,
        limit: usize,
    ) -> Result<DirListing> {
        let snap = self.snapshot(commit)?;
        if snap.contains_key(dir.as_str()) {
            return Err(FleetError::invalid(format!("{dir} is not a directory")));
        }
        let prefix = if dir.is_root() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let mut children: BTreeMap<String, Option<&Item>> = BTreeMap::new();
        for (path, item) in snap.range(prefix.clone()..) {
            let Some(rest) = path.strip_prefix(&prefix) else {
                break;
            };
            match rest.split_once('/') {
                Some((first, _)) => {
                    children.insert(first.to_string(), None);
                }
                None => {
                    children.insert(rest.to_string(), Some(item));
                }
            }
        }
        if children.is_empty() && !dir.is_root() {
            return Err(FleetError::not_found(format!("{dir} does not exist")));
        }
        let truncated = children.len() > limit;
        let entries = children
            .into_iter()
            .take(limit)
            .map(|(name, item)| match item {
                None => TreeEntry::from_git(dir, name.as_bytes(), 0o040_000, None),
                Some(i) => TreeEntry::from_git(dir, name.as_bytes(), i.mode(), stat_of(i).size),
            })
            .collect();
        Ok(DirListing { entries, truncated })
    }

    fn read_entry(
        &self,
        _repo: &str,
        commit: &Sha,
        path: &RepoPath,
        max_bytes: usize,
    ) -> Result<Option<EntryRead>> {
        let snap = self.snapshot(commit)?;
        if let Some(item) = snap.get(path.as_str()) {
            let stat = stat_of(item);
            let content = match item {
                Item::Submodule(_) => None,
                other if other.bytes().len() <= max_bytes => Some(other.bytes().to_vec()),
                _ => None,
            };
            return Ok(Some(EntryRead { stat, content }));
        }
        let prefix = format!("{path}/");
        let is_dir = path.is_root() || snap.keys().any(|k| k.starts_with(&prefix));
        Ok(is_dir.then(|| EntryRead {
            stat: EntryStat {
                kind: EntryKind::Dir,
                mode: ModeClass::Other,
                size: None,
                oid: "0".repeat(40),
            },
            content: None,
        }))
    }

    fn diff(&self, _repo: &str, base: &Sha, head: &Sha, req: &DiffRequest) -> Result<DiffSet> {
        let old = self.snapshot(base)?;
        let new = self.snapshot(head)?;
        let paths: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
        let mut budget: DiffBudget = req.budget;
        let mut files = Vec::new();
        let mut left_out = 0usize;
        for path in paths {
            let (o, n) = (old.get(path), new.get(path));
            if o == n {
                continue;
            }
            if files.len() >= req.max_files {
                left_out += 1;
                continue;
            }
            files.push(file_diff(path, o, n, req.context, &mut budget));
        }
        let truncated = left_out > 0;
        Ok(DiffSet {
            base: base.clone(),
            head: head.clone(),
            context: req.context,
            files,
            truncated,
            notice: truncated.then(|| format!("{left_out} more changed files are not shown.")),
        })
    }
}

fn file_diff(
    path: &str,
    old: Option<&Item>,
    new: Option<&Item>,
    context: u32,
    budget: &mut DiffBudget,
) -> fleet_domain::repo::diff::DiffFile {
    let letter = match (old, new) {
        (None, _) => b'A',
        (_, None) => b'D',
        (Some(o), Some(n)) if o.kind() != n.kind() => b'T',
        _ => b'M',
    };
    let change = FileChange {
        status: letter,
        old_mode: old.map_or(0, Item::mode),
        new_mode: new.map_or(0, Item::mode),
        old_path: path.as_bytes().to_vec(),
        new_path: path.as_bytes().to_vec(),
        similarity: None,
    };
    let old_bytes = old.map_or(&[][..], Item::bytes);
    let new_bytes = new.map_or(&[][..], Item::bytes);
    let binary = old_bytes.contains(&0) || new_bytes.contains(&0);
    let (old_text, new_text) =
        if let (Some(Item::Submodule(a)), Some(Item::Submodule(b))) = (old, new) {
            (
                format!("Subproject commit {a}\n"),
                format!("Subproject commit {b}\n"),
            )
        } else {
            (
                String::from_utf8_lossy(old_bytes).into_owned(),
                String::from_utf8_lossy(new_bytes).into_owned(),
            )
        };
    let (hunks_text, adds, dels) = if binary {
        (String::new(), 0, 0)
    } else {
        unified(&old_text, &new_text, context)
    };
    let patch = if binary {
        format!("diff --git a/{path} b/{path}\nBinary files a/{path} and b/{path} differ\n")
    } else {
        format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n{hunks_text}")
    };
    let parsed = parse_file_patch(patch.as_bytes(), budget);
    let stat = NumStat {
        additions: (!binary).then_some(adds),
        deletions: (!binary).then_some(dels),
    };
    assemble_file(&change, Some(stat), Some(parsed))
}

/// Unified hunks (without file headers) of two texts, and the counts of added and removed lines.
fn unified(old: &str, new: &str, context: u32) -> (String, u32, u32) {
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    // longest common subsequence by dynamic programming: the fixtures are small
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    // ops: (' ' | '-' | '+', text)
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            ops.push((' ', a[i]));
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
            ops.push(('-', a[i]));
            i += 1;
        } else {
            ops.push(('+', b[j]));
            j += 1;
        }
    }
    let adds = ops.iter().filter(|o| o.0 == '+').count() as u32;
    let dels = ops.iter().filter(|o| o.0 == '-').count() as u32;
    let ctx = context as usize;
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, o)| o.0 != ' ')
        .map(|(k, _)| k)
        .collect();
    let mut out = String::new();
    let mut idx = 0;
    while idx < changed.len() {
        let mut first = changed[idx];
        let mut last = changed[idx];
        idx += 1;
        while idx < changed.len() && changed[idx] - last <= 2 * ctx + 1 {
            last = changed[idx];
            idx += 1;
        }
        first = first.saturating_sub(ctx);
        let last = (last + ctx).min(ops.len() - 1);
        // line numbers at `first`
        let old_start = ops[..first].iter().filter(|o| o.0 != '+').count();
        let new_start = ops[..first].iter().filter(|o| o.0 != '-').count();
        let slice = &ops[first..=last];
        let old_count = slice.iter().filter(|o| o.0 != '+').count();
        let new_count = slice.iter().filter(|o| o.0 != '-').count();
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            if old_count == 0 {
                old_start
            } else {
                old_start + 1
            },
            old_count,
            if new_count == 0 {
                new_start
            } else {
                new_start + 1
            },
            new_count
        ));
        for (sign, text) in slice {
            out.push(*sign);
            out.push_str(text.strip_suffix('\n').unwrap_or(text));
            out.push('\n');
            if !text.ends_with('\n') {
                out.push_str("\\ No newline at end of file\n");
            }
        }
    }
    (out, adds, dels)
}

/// Builds the reader of a [`RepoSpec`] in memory; commit ids are made up.
pub struct MemoryRepoHarness;

impl RepoHarness for MemoryRepoHarness {
    fn build(&mut self, spec: &RepoSpec) -> RepoFixture {
        let mut repo = MemoryRepo::new();
        let mut commits = Vec::new();
        for (i, files) in spec.commits.iter().enumerate() {
            let sha = Sha::parse(&format!("{:040x}", i + 1)).unwrap();
            repo.commit(&sha, files.clone());
            commits.push(sha);
        }
        RepoFixture {
            reader: Box::new(repo),
            repo: "/memory/repo".to_string(),
            commits,
        }
    }
}

// ---- the contract --------------------------------------------------------------------------------

fn f(path: &str, text: &str) -> (String, Item) {
    (path.to_string(), Item::File(text.as_bytes().to_vec()))
}

fn p(s: &str) -> RepoPath {
    RepoPath::parse(s).unwrap()
}

fn lines(n: usize, tag: &str) -> String {
    (1..=n).map(|i| format!("{tag} {i}\n")).collect()
}

/// A repository with every kind of entry, three commits: the first has the files, the second
/// changes some of them, the third has more changed files than any diff shows.
fn fixture_spec() -> RepoSpec {
    let png: Vec<u8> = {
        let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        v.extend_from_slice(&[0u8; 25]);
        v
    };
    let first = vec![
        f("README.md", "# Title\n\nHello\n"),
        f("src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n"),
        f("src/lib/util.rs", &lines(30, "util")),
        f("docs/notes.txt", "one\ntwo\nthree\n"),
        (
            "run.sh".to_string(),
            Item::Exec(b"#!/bin/sh\necho hi\n".to_vec()),
        ),
        ("link".to_string(), Item::Symlink("src/main.rs".to_string())),
        (
            "bin/blob.dat".to_string(),
            Item::File(vec![0, 1, 2, 3, 0, 255]),
        ),
        ("img/logo.png".to_string(), Item::File(png)),
        ("vendor/lib".to_string(), Item::Submodule("a".repeat(40))),
        f("empty.txt", ""),
        f("big.txt", &lines(5000, "line")),
    ];
    let mut second = first.clone();
    second.retain(|(path, _)| path != "docs/notes.txt");
    for (path, item) in &mut second {
        match path.as_str() {
            "src/main.rs" => {
                *item =
                    Item::File(b"fn main() {\n    println!(\"hello\");\n    run();\n}\n".to_vec());
            }
            "run.sh" => *item = Item::File(b"#!/bin/sh\necho hi\n".to_vec()),
            "bin/blob.dat" => *item = Item::File(vec![0, 9, 9, 0]),
            "vendor/lib" => *item = Item::Submodule("b".repeat(40)),
            "link" => *item = Item::File(b"now a file\n".to_vec()),
            _ => {}
        }
    }
    second.push(f("new file.txt", "brand new\n"));
    let mut third = second.clone();
    for i in 0..(fleet_domain::repo::limits::MAX_DIFF_FILES + 25) {
        third.push(f(&format!("many/f{i:04}.txt"), &format!("{i}\n")));
    }
    RepoSpec {
        commits: vec![first, second, third],
    }
}

fn names(listing: &DirListing) -> Vec<String> {
    let mut v: Vec<String> = listing.entries.iter().map(|e| e.name.clone()).collect();
    v.sort();
    v
}

/// What every [`RepoReader`] must do.
pub fn repo_reader_contract(h: &mut dyn RepoHarness) {
    let fx = h.build(&fixture_spec());
    let (r, repo) = (&*fx.reader, fx.repo.as_str());
    let (c1, c2, c3) = (&fx.commits[0], &fx.commits[1], &fx.commits[2]);

    // ---- directories
    let root = r.list_dir(repo, c1, &RepoPath::root(), 1000).unwrap();
    assert!(!root.truncated);
    assert_eq!(
        names(&root),
        [
            "README.md",
            "big.txt",
            "bin",
            "docs",
            "empty.txt",
            "img",
            "link",
            "run.sh",
            "src",
            "vendor"
        ]
    );
    let by = |n: &str| root.entries.iter().find(|e| e.name == n).unwrap();
    assert_eq!(by("src").kind, EntryKind::Dir);
    assert_eq!(by("src").size, None);
    assert_eq!(by("src").path.as_ref().map(RepoPath::as_str), Some("src"));
    assert_eq!(by("README.md").kind, EntryKind::File);
    assert_eq!(by("README.md").size, Some(15));
    assert_eq!(by("README.md").mode, ModeClass::Regular);
    assert_eq!(by("run.sh").mode, ModeClass::Executable);
    assert_eq!(by("link").kind, EntryKind::Symlink);
    assert_eq!(by("link").size, Some("src/main.rs".len() as u64));
    assert_eq!(by("vendor").kind, EntryKind::Dir);
    let vendor = r.list_dir(repo, c1, &p("vendor"), 10).unwrap();
    assert_eq!(vendor.entries.len(), 1);
    assert_eq!(vendor.entries[0].kind, EntryKind::Submodule);
    assert_eq!(vendor.entries[0].size, None);
    let sub = r.list_dir(repo, c1, &p("src"), 10).unwrap();
    assert_eq!(names(&sub), ["lib", "main.rs"]);
    assert_eq!(
        sub.entries
            .iter()
            .find(|e| e.name == "main.rs")
            .unwrap()
            .path
            .as_ref()
            .map(RepoPath::as_str),
        Some("src/main.rs")
    );
    let deep = r.list_dir(repo, c1, &p("src/lib"), 10).unwrap();
    assert_eq!(names(&deep), ["util.rs"]);
    // the limit cuts and says so
    let cut = r.list_dir(repo, c1, &RepoPath::root(), 3).unwrap();
    assert_eq!(cut.entries.len(), 3);
    assert!(cut.truncated);
    let exact = r.list_dir(repo, c1, &RepoPath::root(), 10).unwrap();
    assert!(!exact.truncated && exact.entries.len() == 10);
    // missing, not a directory, unknown commit
    assert_eq!(
        r.list_dir(repo, c1, &p("nope"), 10).unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(
        r.list_dir(repo, c1, &p("src/nope/deeper"), 10)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    let not_dir = r.list_dir(repo, c1, &p("README.md"), 10).unwrap_err();
    assert!(
        matches!(not_dir.code, ErrorCode::Invalid | ErrorCode::NotFound),
        "{not_dir:?}"
    );
    let unknown = Sha::parse(&"d".repeat(40)).unwrap();
    assert_eq!(
        r.list_dir(repo, &unknown, &RepoPath::root(), 10)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    // a commit's tree is as it was
    assert!(
        names(&r.list_dir(repo, c1, &p("docs"), 10).unwrap()).contains(&"notes.txt".to_string())
    );
    assert_eq!(
        r.list_dir(repo, c2, &p("docs"), 10).unwrap_err().code,
        ErrorCode::NotFound
    );

    // ---- entries
    let readme = r
        .read_entry(repo, c1, &p("README.md"), 1000)
        .unwrap()
        .unwrap();
    assert_eq!(readme.stat.kind, EntryKind::File);
    assert_eq!(readme.stat.size, Some(15));
    assert_eq!(readme.content.as_deref(), Some(&b"# Title\n\nHello\n"[..]));
    let big = r.read_entry(repo, c1, &p("big.txt"), 100).unwrap().unwrap();
    assert_eq!(big.stat.size, Some(lines(5000, "line").len() as u64));
    assert_eq!(
        big.content, None,
        "the size is checked before anything is read"
    );
    let empty = r
        .read_entry(repo, c1, &p("empty.txt"), 100)
        .unwrap()
        .unwrap();
    assert_eq!(
        (empty.stat.size, empty.content.as_deref()),
        (Some(0), Some(&b""[..]))
    );
    let exec = r.read_entry(repo, c1, &p("run.sh"), 100).unwrap().unwrap();
    assert_eq!(exec.stat.mode, ModeClass::Executable);
    let link = r.read_entry(repo, c1, &p("link"), 100).unwrap().unwrap();
    assert_eq!(link.stat.kind, EntryKind::Symlink);
    assert_eq!(
        link.content.as_deref(),
        Some(&b"src/main.rs"[..]),
        "a link's content is its target, never what it points at"
    );
    let module = r
        .read_entry(repo, c1, &p("vendor/lib"), 100)
        .unwrap()
        .unwrap();
    assert_eq!(module.stat.kind, EntryKind::Submodule);
    assert_eq!(module.stat.oid, "a".repeat(40));
    assert_eq!(module.content, None);
    let dir = r.read_entry(repo, c1, &p("src"), 100).unwrap().unwrap();
    assert_eq!((dir.stat.kind, dir.content), (EntryKind::Dir, None));
    let top = r
        .read_entry(repo, c1, &RepoPath::root(), 100)
        .unwrap()
        .unwrap();
    assert_eq!(top.stat.kind, EntryKind::Dir);
    assert!(r
        .read_entry(repo, c1, &p("nope.txt"), 100)
        .unwrap()
        .is_none());
    assert!(r
        .read_entry(repo, c1, &p("src/main.rs/inside"), 100)
        .unwrap()
        .is_none());
    assert!(
        r.read_entry(repo, c1, &p("link/inside"), 100)
            .unwrap()
            .is_none(),
        "a link is not followed"
    );
    assert_eq!(
        r.read_entry(repo, &unknown, &p("README.md"), 100)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    // the same path at another commit
    let old = r
        .read_entry(repo, c1, &p("src/main.rs"), 1000)
        .unwrap()
        .unwrap();
    let new = r
        .read_entry(repo, c2, &p("src/main.rs"), 1000)
        .unwrap()
        .unwrap();
    assert_ne!(old.content, new.content);

    // ---- diffs
    let req = DiffRequest {
        context: 3,
        max_files: fleet_domain::repo::limits::MAX_DIFF_FILES,
        budget: DiffBudget::default(),
    };
    let d = r.diff(repo, c1, c2, &req).unwrap();
    assert_eq!((&d.base, &d.head, d.context), (c1, c2, 3));
    assert!(!d.truncated);
    let file = |path: &str| {
        d.files.iter().find(|f| f.path == path).unwrap_or_else(|| {
            panic!(
                "no {path} in {:?}",
                d.files.iter().map(|f| &f.path).collect::<Vec<_>>()
            )
        })
    };
    assert_eq!(file("src/main.rs").status, FileStatus::Modified);
    assert_eq!(
        (file("src/main.rs").additions, file("src/main.rs").deletions),
        (2, 1)
    );
    let hunk = &file("src/main.rs").hunks[0];
    assert_eq!((hunk.old_start, hunk.new_start), (1, 1));
    let kinds: Vec<LineKind> = hunk.lines.iter().map(|l| l.kind).collect();
    assert_eq!(kinds.iter().filter(|k| **k == LineKind::Add).count(), 2);
    assert!(hunk
        .lines
        .iter()
        .any(|l| l.kind == LineKind::Del && l.text.contains("hi") && l.old == Some(2)));
    assert!(hunk
        .lines
        .iter()
        .any(|l| l.kind == LineKind::Add && l.text.contains("run();") && l.new == Some(3)));
    assert_eq!(file("docs/notes.txt").status, FileStatus::Deleted);
    assert_eq!(file("docs/notes.txt").deletions, 3);
    assert_eq!(file("new file.txt").status, FileStatus::Added);
    assert_eq!(file("new file.txt").hunks[0].lines[0].text, "brand new");
    assert!(file("new file.txt").path_ok);
    let binary = file("bin/blob.dat");
    assert!(binary.binary && binary.hunks.is_empty());
    assert_eq!(file("run.sh").status, FileStatus::ModeChanged);
    assert!(file("run.sh").hunks.is_empty());
    assert_eq!(
        (file("run.sh").old_mode, file("run.sh").new_mode),
        (Some(0o100_755), Some(0o100_644))
    );
    assert_eq!(file("link").status, FileStatus::TypeChanged);
    assert!(file("vendor/lib").submodule);
    assert_eq!(
        d.files
            .iter()
            .filter(|f| f.path == "big.txt" || f.path == "README.md")
            .count(),
        0,
        "unchanged files are not listed"
    );
    let stats = d.stats();
    assert_eq!(stats.files, d.files.len());
    assert_eq!(stats.binary, 1);
    // the diff of a commit with itself is empty
    assert!(r.diff(repo, c1, c1, &req).unwrap().files.is_empty());
    // context
    let wide = r
        .diff(repo, c1, c2, &DiffRequest { context: 0, ..req })
        .unwrap();
    let narrow = wide.files.iter().find(|f| f.path == "src/main.rs").unwrap();
    assert_eq!(
        narrow
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind == LineKind::Context)
            .count(),
        0
    );
    // unknown commit
    assert_eq!(
        r.diff(repo, &unknown, c2, &req).unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(
        r.diff(repo, c1, &unknown, &req).unwrap_err().code,
        ErrorCode::NotFound
    );
    // many files: capped and said so
    let many = r
        .diff(
            repo,
            c2,
            c3,
            &DiffRequest {
                max_files: 40,
                ..req
            },
        )
        .unwrap();
    assert_eq!(many.files.len(), 40);
    assert!(many.truncated && many.notice.is_some());
    // a small line budget cuts the lines and marks the files
    let tight = r
        .diff(
            repo,
            c1,
            c3,
            &DiffRequest {
                budget: DiffBudget {
                    lines_left: 5,
                    ..DiffBudget::default()
                },
                ..req
            },
        )
        .unwrap();
    let shown: usize = tight
        .files
        .iter()
        .flat_map(|f| &f.hunks)
        .map(|h| h.lines.len())
        .sum();
    assert!(shown <= 5, "{shown} lines shown");
    assert!(tight.files.iter().any(|f| f.truncated));
}
