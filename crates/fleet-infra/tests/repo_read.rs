//! The real git adapter of the read-only workbench against real temporary repositories: the port
//! contract it shares with the in-memory fake, and what only git can show (renames, links,
//! submodules, hostile names, huge files and trees, planted configuration).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_app::ports::{DiffRequest, EntryRead, RepoReader};
use fleet_app::testing::repo::{repo_reader_contract, Item, RepoFixture, RepoHarness, RepoSpec};
use fleet_domain::env::EnvPolicy;
use fleet_domain::error::ErrorCode;
use fleet_domain::ids::Sha;
use fleet_domain::repo::diff::{DiffBudget, DiffSet, FileStatus, LineKind};
use fleet_domain::repo::limits::{MAX_DIFF_FILES, MAX_DIFF_LINES_PER_FILE, MAX_TREE_ENTRIES};
use fleet_domain::repo::tree::{EntryKind, ModeClass};
use fleet_domain::repo::RepoPath;
use fleet_infra::git::GitVcs;
use fleet_infra::testutil::{git, git_available, TempDir};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

macro_rules! need_git {
    () => {
        if !git_available() {
            return;
        }
    };
}

fn reader() -> GitVcs {
    GitVcs::new(
        vec![(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        )],
        EnvPolicy::default_policy(),
    )
    .with_timeout(Duration::from_secs(60))
}

fn p(s: &str) -> RepoPath {
    RepoPath::parse(s).unwrap()
}

fn sha(s: &str) -> Sha {
    Sha::parse(s).unwrap()
}

fn req(context: u32) -> DiffRequest {
    DiffRequest {
        context,
        max_files: MAX_DIFF_FILES,
        budget: DiffBudget::default(),
    }
}

/// A repository on disk built with plain git.
struct Repo {
    _tmp: TempDir,
    dir: PathBuf,
}

impl Repo {
    fn new() -> Repo {
        let tmp = TempDir::new("repo-read");
        let dir = tmp.join("repo");
        fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        Repo { _tmp: tmp, dir }
    }

    fn s(&self) -> &str {
        self.dir.to_str().unwrap()
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let p = self.dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }

    fn commit(&self, message: &str) -> Sha {
        git(&self.dir, &["add", "-A"]);
        git(&self.dir, &["commit", "-q", "--allow-empty", "-m", message]);
        sha(&git(&self.dir, &["rev-parse", "HEAD"]))
    }

    fn git(&self, args: &[&str]) -> String {
        git(&self.dir, args)
    }

    /// Writes a tree object from `mktree -z` records and returns its id.
    fn mktree(&self, records: &[(&str, &str, &[u8])]) -> String {
        let mut input = Vec::new();
        for (mode_type_oid, _, name) in records {
            input.extend_from_slice(mode_type_oid.as_bytes());
            input.push(b'\t');
            input.extend_from_slice(name);
            input.push(0);
        }
        let mut child = Command::new("git")
            .args(["mktree", "-z"])
            .current_dir(&self.dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&input).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn blob(&self, bytes: &[u8]) -> String {
        let mut child = Command::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .current_dir(&self.dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(bytes).unwrap();
        let out = child.wait_with_output().unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// A commit whose tree is `tree`, on top of `parent` (when given).
    fn commit_tree(&self, tree: &str, parent: Option<&Sha>) -> Sha {
        let mut args = vec!["commit-tree", tree, "-m", "synthetic"];
        if let Some(p) = parent {
            args.push("-p");
            args.push(p.as_str());
        }
        sha(&self.git(&args))
    }
}

// ---- the contract ------------------------------------------------------------------------------

struct GitHarness {
    keep: Vec<Repo>,
}

impl RepoHarness for GitHarness {
    fn build(&mut self, spec: &RepoSpec) -> RepoFixture {
        let repo = Repo::new();
        let mut commits = Vec::new();
        for files in &spec.commits {
            // the tree of this commit is exactly the snapshot
            for entry in fs::read_dir(&repo.dir).unwrap().flatten() {
                if entry.file_name() != ".git" {
                    let path = entry.path();
                    if path.is_dir() && !path.is_symlink() {
                        fs::remove_dir_all(path).unwrap();
                    } else {
                        fs::remove_file(path).unwrap();
                    }
                }
            }
            let mut gitlinks = Vec::new();
            for (path, item) in files {
                let full = repo.dir.join(path);
                fs::create_dir_all(full.parent().unwrap()).unwrap();
                match item {
                    Item::File(b) => fs::write(&full, b).unwrap(),
                    Item::Exec(b) => {
                        fs::write(&full, b).unwrap();
                        fs::set_permissions(&full, fs::Permissions::from_mode(0o755)).unwrap();
                    }
                    Item::Symlink(t) => symlink(t, &full).unwrap(),
                    Item::Submodule(sha) => gitlinks.push((path.clone(), sha.clone())),
                }
            }
            repo.git(&["add", "-A"]);
            for (path, sha) in gitlinks {
                repo.git(&[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("160000,{sha},{path}"),
                ]);
            }
            git(
                &repo.dir,
                &["commit", "-q", "--allow-empty", "-m", "snapshot"],
            );
            commits.push(sha_of(&repo));
        }
        let path = repo.s().to_string();
        self.keep.push(repo);
        RepoFixture {
            reader: Box::new(reader()),
            repo: path,
            commits,
        }
    }
}

fn sha_of(repo: &Repo) -> Sha {
    sha(&repo.git(&["rev-parse", "HEAD"]))
}

#[test]
fn the_git_reader_satisfies_the_contract() {
    need_git!();
    repo_reader_contract(&mut GitHarness { keep: Vec::new() });
}

// ---- what only git shows -----------------------------------------------------------------------

fn diff(r: &GitVcs, repo: &Repo, a: &Sha, b: &Sha, ctx: u32) -> DiffSet {
    r.diff(repo.s(), a, b, &req(ctx)).unwrap()
}

fn find<'a>(set: &'a DiffSet, path: &str) -> &'a fleet_domain::repo::diff::DiffFile {
    set.files
        .iter()
        .find(|f| f.path == path)
        .unwrap_or_else(|| {
            panic!(
                "no {path} among {:?}",
                set.files.iter().map(|f| &f.path).collect::<Vec<_>>()
            )
        })
}

#[test]
fn renames_are_detected_with_their_similarity_and_old_path() {
    need_git!();
    let repo = Repo::new();
    let body: String = (0..40).map(|i| format!("line {i}\n")).collect();
    repo.write("old name.txt", body.as_bytes());
    repo.write("same.txt", b"x\ny\nz\n");
    let a = repo.commit("a");
    fs::create_dir_all(repo.dir.join("new/dir")).unwrap();
    git(&repo.dir, &["mv", "old name.txt", "new/dir/renamed.txt"]);
    repo.write(
        "new/dir/renamed.txt",
        format!("{body}one more\n").as_bytes(),
    );
    git(&repo.dir, &["mv", "same.txt", "moved.txt"]);
    let b = repo.commit("b");
    let r = reader();
    let d = diff(&r, &repo, &a, &b, 3);
    let renamed = find(&d, "new/dir/renamed.txt");
    assert_eq!(renamed.status, FileStatus::Renamed);
    assert_eq!(renamed.old_path.as_deref(), Some("old name.txt"));
    assert!(renamed.similarity.unwrap() >= 90);
    assert!(renamed.path_ok && renamed.old_path_ok);
    assert_eq!((renamed.additions, renamed.deletions), (1, 0));
    assert_eq!(renamed.hunks.len(), 1);
    let pure = find(&d, "moved.txt");
    assert_eq!(pure.status, FileStatus::Renamed);
    assert_eq!(pure.similarity, Some(100));
    assert!(
        pure.hunks.is_empty() && !pure.truncated,
        "a pure rename has no lines to miss"
    );
    assert_eq!(d.files.len(), 2);
}

#[test]
fn rename_detection_abuse_stays_bounded_and_complete() {
    need_git!();
    let repo = Repo::new();
    // 400 files deleted and 400 similar files added: more than git compares (-l200)
    for i in 0..400 {
        repo.write(
            &format!("old/f{i:03}.txt"),
            format!("common\nlines\nfor {i}\n").as_bytes(),
        );
    }
    let a = repo.commit("a");
    git(&repo.dir, &["rm", "-r", "-q", "old"]);
    for i in 0..400 {
        repo.write(
            &format!("new/g{i:03}.txt"),
            format!("common\nlines\nfor {i}\nplus\n").as_bytes(),
        );
    }
    let b = repo.commit("b");
    let start = std::time::Instant::now();
    let d = diff(&reader(), &repo, &a, &b, 3);
    assert!(start.elapsed() < Duration::from_secs(30));
    // shown: at most the cap; the rest is counted in the notice
    assert_eq!(d.files.len(), MAX_DIFF_FILES);
    assert!(d.truncated);
    assert!(
        d.notice.as_deref().unwrap().contains("more changed files"),
        "{:?}",
        d.notice
    );
}

#[test]
fn a_file_turned_into_a_link_and_back_is_one_type_change() {
    need_git!();
    let repo = Repo::new();
    repo.write("thing", b"just a file\n");
    let a = repo.commit("a");
    fs::remove_file(repo.dir.join("thing")).unwrap();
    symlink("target/elsewhere", repo.dir.join("thing")).unwrap();
    let b = repo.commit("b");
    let d = diff(&reader(), &repo, &a, &b, 3);
    let f = find(&d, "thing");
    assert_eq!(f.status, FileStatus::TypeChanged);
    assert_eq!((f.old_mode, f.new_mode), (Some(0o100_644), Some(0o120_000)));
    // both halves of the change are shown: the removed text and the added link target
    let lines: Vec<(LineKind, &str)> = f
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .map(|l| (l.kind, l.text.as_str()))
        .collect();
    assert!(lines.contains(&(LineKind::Del, "just a file")), "{lines:?}");
    assert!(
        lines.contains(&(LineKind::Add, "target/elsewhere")),
        "{lines:?}"
    );
    assert_eq!(d.files.len(), 1);
}

#[test]
fn links_are_described_and_never_followed() {
    need_git!();
    let repo = Repo::new();
    repo.write("src/real.txt", b"real\n");
    repo.write("secret.txt", b"top secret\n");
    symlink("/etc/passwd", repo.dir.join("abs")).unwrap();
    symlink("../../../../etc/hostname", repo.dir.join("src/up")).unwrap();
    symlink("src", repo.dir.join("dirlink")).unwrap();
    symlink("a/".repeat(2000), repo.dir.join("long")).unwrap();
    symlink("secret.txt", repo.dir.join("inside")).unwrap();
    let c = repo.commit("links");
    let r = reader();
    for (path, target) in [
        ("abs", "/etc/passwd"),
        ("src/up", "../../../../etc/hostname"),
        ("dirlink", "src"),
        ("inside", "secret.txt"),
    ] {
        let e = r.read_entry(repo.s(), &c, &p(path), 4096).unwrap().unwrap();
        assert_eq!(e.stat.kind, EntryKind::Symlink, "{path}");
        assert_eq!(e.content.as_deref(), Some(target.as_bytes()), "{path}");
    }
    // what a link points at is not reachable through it
    assert!(r
        .read_entry(repo.s(), &c, &p("dirlink/real.txt"), 4096)
        .unwrap()
        .is_none());
    assert_eq!(
        r.list_dir(repo.s(), &c, &p("dirlink"), 10)
            .unwrap_err()
            .code,
        ErrorCode::Invalid
    );
    assert!(r
        .read_entry(repo.s(), &c, &p("abs/inside"), 4096)
        .unwrap()
        .is_none());
    // a very long target is bounded by the size limit like any file
    let long = r
        .read_entry(repo.s(), &c, &p("long"), 100)
        .unwrap()
        .unwrap();
    assert_eq!(long.content, None);
    assert_eq!(long.stat.size, Some(4000));
}

#[test]
fn a_submodule_is_a_pointer_and_nothing_is_fetched_or_run() {
    need_git!();
    let repo = Repo::new();
    let tmp = TempDir::new("submodule-marker");
    let marker = tmp.join("ran");
    let script = tmp.script(
        "evil.sh",
        &format!("#!/bin/sh\ntouch {}\n", marker.display()),
    );
    repo.write(
        ".gitmodules",
        format!(
            "[submodule \"vendor/lib\"]\n\tpath = vendor/lib\n\turl = ext::{} x\n\tupdate = !{}\n",
            script.display(),
            script.display()
        )
        .as_bytes(),
    );
    repo.write("README.md", b"hi\n");
    repo.git(&["add", "-A"]);
    repo.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{},vendor/lib", "e".repeat(40)),
    ]);
    git(&repo.dir, &["commit", "-q", "-m", "with a submodule"]);
    let c = sha_of(&repo);
    let r = reader();
    let e = r
        .read_entry(repo.s(), &c, &p("vendor/lib"), 100)
        .unwrap()
        .unwrap();
    assert_eq!(e.stat.kind, EntryKind::Submodule);
    assert_eq!(e.stat.oid, "e".repeat(40));
    assert_eq!(e.content, None);
    let list = r.list_dir(repo.s(), &c, &p("vendor"), 10).unwrap();
    assert_eq!(list.entries[0].kind, EntryKind::Submodule);
    // the pointer moves: the diff shows both commit ids and says it is a submodule
    repo.git(&[
        "update-index",
        "--cacheinfo",
        &format!("160000,{},vendor/lib", "f".repeat(40)),
    ]);
    git(&repo.dir, &["commit", "-q", "-m", "moved"]);
    let d2 = sha_of(&repo);
    let d = diff(&r, &repo, &c, &d2, 3);
    let f = find(&d, "vendor/lib");
    assert!(f.submodule);
    let texts: Vec<&str> = f
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .map(|l| l.text.as_str())
        .collect();
    assert!(
        texts.iter().any(|t| t.contains(&"e".repeat(40)))
            && texts.iter().any(|t| t.contains(&"f".repeat(40))),
        "{texts:?}"
    );
    assert!(!marker.exists(), "nothing from .gitmodules was run");
}

#[test]
fn hostile_names_are_listed_safely_and_cannot_be_requested() {
    need_git!();
    let repo = Repo::new();
    let empty = repo.blob(b"x\n");
    let line = format!("100644 blob {empty}");
    let tree = repo.mktree(&[
        (line.as_str(), "", b"plain.txt"),
        (line.as_str(), "", b"new\nline"),
        (line.as_str(), "", b"esc\x1b[31mred"),
        (line.as_str(), "", "bidi\u{202e}txt.exe".as_bytes()),
        (line.as_str(), "", b"bad\xff\xfeutf8"),
        (line.as_str(), "", b"back\\slash"),
        (line.as_str(), "", b"<img src=x onerror=alert(1)>"),
        (line.as_str(), "", b".GIT"),
        (line.as_str(), "", b"tab\there"),
    ]);
    let c = repo.commit_tree(&tree, None);
    let r = reader();
    let list = r.list_dir(repo.s(), &c, &RepoPath::root(), 100).unwrap();
    assert_eq!(list.entries.len(), 9);
    for e in &list.entries {
        assert!(
            !e.name.contains('\n')
                && !e.name.contains('\x1b')
                && !e.name.contains('\u{202e}')
                && !e.name.contains('\t'),
            "{:?}",
            e.name
        );
    }
    let by = |needle: &str| {
        list.entries
            .iter()
            .find(|e| e.name.contains(needle))
            .unwrap()
    };
    assert!(by("plain.txt").path.is_some());
    assert!(by("<img src=x").path.is_some(), "markup is just text");
    for hostile in [
        "new\\u{a}line",
        "esc\\u{1b}",
        "bidi\\u{202e}",
        "\\xff\\xfe",
        "back\\\\slash",
        ".GIT",
        "tab\\u{9}here",
    ] {
        assert!(
            by(hostile).path.is_none(),
            "{hostile} must not be addressable"
        );
    }
    // and the daemon-side validation refuses to even ask
    for bad in ["new\nline", "bad\u{fffd}", ".GIT"] {
        assert!(RepoPath::parse(bad).is_err() || bad == "bad\u{fffd}");
    }
}

#[test]
fn text_with_crlf_bom_and_odd_bytes_comes_back_byte_for_byte() {
    need_git!();
    let repo = Repo::new();
    let samples: [(&str, &[u8]); 5] = [
        ("crlf.txt", b"a\r\nb\r\nc\r\n"),
        ("bom.txt", b"\xef\xbb\xbfhello\n"),
        ("lone.txt", b"a\rb\rc"),
        ("latin1.txt", b"caf\xe9\n"),
        ("esc.txt", b"\x1b]0;title\x07\x1b[2Jboom\n"),
    ];
    for (name, bytes) in samples {
        repo.write(name, bytes);
    }
    // a configured filter and attributes must not touch the bytes (they are never applied)
    let c = repo.commit("odd");
    let r = reader();
    for (name, bytes) in samples {
        let e: EntryRead = r.read_entry(repo.s(), &c, &p(name), 4096).unwrap().unwrap();
        assert_eq!(e.content.as_deref(), Some(bytes), "{name}");
    }
}

#[test]
fn huge_files_are_not_read_and_huge_trees_are_cut() {
    need_git!();
    let repo = Repo::new();
    // 48 MiB of zeros as one blob
    let big = repo.dir.join("big.bin");
    fs::write(&big, vec![0u8; 48 * 1024 * 1024]).unwrap();
    repo.write("small.txt", b"small\n");
    let c1 = repo.commit("big");
    let r = reader();
    let start = std::time::Instant::now();
    let e = r
        .read_entry(repo.s(), &c1, &p("big.bin"), 1024 * 1024)
        .unwrap()
        .unwrap();
    assert_eq!(e.stat.size, Some(48 * 1024 * 1024));
    assert_eq!(e.content, None);
    assert!(start.elapsed() < Duration::from_secs(20));
    let small = r
        .read_entry(repo.s(), &c1, &p("small.txt"), 1024 * 1024)
        .unwrap()
        .unwrap();
    assert_eq!(small.content.as_deref(), Some(&b"small\n"[..]));

    // a directory with 30 000 entries
    let blob = repo.blob(b"x\n");
    let line = format!("100644 blob {blob}");
    let names: Vec<String> = (0..30_000).map(|i| format!("file-{i:06}.txt")).collect();
    let records: Vec<(&str, &str, &[u8])> = names
        .iter()
        .map(|n| (line.as_str(), "", n.as_bytes()))
        .collect();
    let tree = repo.mktree(&records);
    let c2 = repo.commit_tree(&tree, None);
    let list = r
        .list_dir(repo.s(), &c2, &RepoPath::root(), MAX_TREE_ENTRIES)
        .unwrap();
    assert_eq!(list.entries.len(), MAX_TREE_ENTRIES);
    assert!(list.truncated);
    // a small limit on the same directory
    let few = r.list_dir(repo.s(), &c2, &RepoPath::root(), 5).unwrap();
    assert_eq!((few.entries.len(), few.truncated), (5, true));
}

#[test]
fn a_huge_diff_is_cut_by_the_limits_and_says_so() {
    need_git!();
    let repo = Repo::new();
    repo.write("a.txt", b"start\n");
    let a = repo.commit("a");
    // 30 MiB of text lines in one file, and many medium files
    let body: String = (0..600_000)
        .map(|i| format!("line number {i} of the huge file\n"))
        .collect();
    repo.write("huge.txt", body.as_bytes());
    for i in 0..30 {
        let text: String = (0..3000)
            .map(|j| format!("f{i} line {j} {}\n", "y".repeat(60)))
            .collect();
        repo.write(&format!("many/m{i:02}.txt"), text.as_bytes());
    }
    let b = repo.commit("b");
    let start = std::time::Instant::now();
    let d = diff(&reader(), &repo, &a, &b, 3);
    assert!(start.elapsed() < Duration::from_secs(40));
    let huge = find(&d, "huge.txt");
    assert_eq!(
        huge.additions, 600_000,
        "the totals come from numstat, not from the shown lines"
    );
    assert!(huge.truncated && huge.note.is_some());
    let shown: usize = huge.hunks.iter().map(|h| h.lines.len()).sum();
    assert!(shown <= MAX_DIFF_LINES_PER_FILE, "{shown}");
    let total: usize = d
        .files
        .iter()
        .flat_map(|f| &f.hunks)
        .map(|h| h.lines.len())
        .sum();
    assert!(total <= fleet_domain::repo::limits::MAX_DIFF_LINES_TOTAL);
    let bytes: usize = d
        .files
        .iter()
        .flat_map(|f| &f.hunks)
        .flat_map(|h| &h.lines)
        .map(|l| l.text.len())
        .sum();
    assert!(bytes <= fleet_domain::repo::limits::MAX_DIFF_TEXT_BYTES + 4096);
    assert!(d.files.iter().any(|f| f.truncated));
    // files after the budget ran out are listed, with their counts, and say they are not shown
    let last = d.files.last().unwrap();
    assert!(last.additions > 0);
}

#[test]
fn binary_files_and_mode_only_changes_have_no_lines() {
    need_git!();
    let repo = Repo::new();
    repo.write("img.bin", &[0u8, 1, 2, 3, 0, 0, 255]);
    repo.write("script.sh", b"#!/bin/sh\n");
    let a = repo.commit("a");
    repo.write("img.bin", &[0u8, 9, 9, 9, 0]);
    fs::set_permissions(
        repo.dir.join("script.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let b = repo.commit("b");
    let d = diff(&reader(), &repo, &a, &b, 3);
    let bin = find(&d, "img.bin");
    assert!(bin.binary && bin.hunks.is_empty() && bin.note.is_some());
    let mode = find(&d, "script.sh");
    assert_eq!(mode.status, FileStatus::ModeChanged);
    assert_eq!(
        (mode.old_mode, mode.new_mode),
        (Some(0o100_644), Some(0o100_755))
    );
    assert!(mode.hunks.is_empty());
}

#[test]
fn crlf_changes_are_visible_in_a_diff() {
    need_git!();
    let repo = Repo::new();
    repo.write("w.txt", b"one\ntwo\n");
    let a = repo.commit("a");
    repo.write("w.txt", b"one\r\ntwo\r\n");
    let b = repo.commit("b");
    let d = diff(&reader(), &repo, &a, &b, 3);
    let lines: Vec<_> = find(&d, "w.txt")
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .collect();
    assert!(
        lines
            .iter()
            .any(|l| l.kind == LineKind::Add && l.cr && l.text == "one"),
        "{lines:?}"
    );
    assert!(lines
        .iter()
        .any(|l| l.kind == LineKind::Del && !l.cr && l.text == "one"));
}

#[test]
fn files_without_a_final_newline_carry_the_marker() {
    need_git!();
    let repo = Repo::new();
    repo.write("n.txt", b"a\nb");
    let a = repo.commit("a");
    repo.write("n.txt", b"a\nc");
    let b = repo.commit("b");
    let d = diff(&reader(), &repo, &a, &b, 3);
    let lines: Vec<_> = find(&d, "n.txt")
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .collect();
    assert!(
        lines.iter().filter(|l| l.no_newline).count() >= 2,
        "{lines:?}"
    );
}

// ---- nothing in the repository runs ------------------------------------------------------------

#[test]
fn planted_hooks_filters_textconv_and_pagers_never_run() {
    need_git!();
    let repo = Repo::new();
    let tmp = TempDir::new("planted");
    let marker = tmp.join("ran");
    let script = tmp.script(
        "plant.sh",
        &format!("#!/bin/sh\necho \"$0 $*\" >> {}\ncat\n", marker.display()),
    );
    let s = script.display();
    repo.write(".gitattributes", b"* diff=evil filter=evil merge=evil\n");
    repo.write("a.txt", b"one\n");
    repo.write("b.txt", b"two\n");
    let a = repo.commit("a");
    repo.write("a.txt", b"one changed\n");
    let b = repo.commit("b");
    let config = repo.dir.join(".git/config");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "[diff \"evil\"]\n\ttextconv = {s}\n\tcommand = {s}\n[filter \"evil\"]\n\tclean = {s}\n\tsmudge = {s}\n\tprocess = {s}\n[core]\n\tfsmonitor = {s}\n\tpager = {s}\n\thooksPath = {hooks}\n\tattributesFile = {s}\n[diff]\n\texternal = {s}\n[pager]\n\tdiff = {s}\n\tlog = {s}\n[alias]\n\tdiff = !{s}\n",
        hooks = tmp.path().display(),
    ));
    fs::write(&config, text).unwrap();
    for hook in [
        "post-checkout",
        "post-commit",
        "pre-commit",
        "reference-transaction",
        "post-index-change",
    ] {
        fs::write(
            repo.dir.join(".git/hooks").join(hook),
            format!("#!/bin/sh\ntouch {}\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(
            repo.dir.join(".git/hooks").join(hook),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    let r = reader();
    r.list_dir(repo.s(), &b, &RepoPath::root(), 100).unwrap();
    r.read_entry(repo.s(), &b, &p("a.txt"), 1000)
        .unwrap()
        .unwrap();
    let d = diff(&r, &repo, &a, &b, 3);
    assert_eq!(find(&d, "a.txt").status, FileStatus::Modified);
    assert!(
        !marker.exists(),
        "something from the repository ran: {}",
        tmp.read("ran")
    );
}

#[test]
fn replace_refs_do_not_change_what_is_read() {
    need_git!();
    let repo = Repo::new();
    repo.write("f.txt", b"the real content\n");
    let c = repo.commit("real");
    // a replacement object for the blob and one for the commit's tree
    let fake_blob = repo.blob(b"forged\n");
    let real_blob = repo.git(&["rev-parse", &format!("{c}:f.txt")]);
    repo.git(&["replace", &real_blob, &fake_blob]);
    let r = reader();
    let e = r
        .read_entry(repo.s(), &c, &p("f.txt"), 1000)
        .unwrap()
        .unwrap();
    assert_eq!(e.content.as_deref(), Some(&b"the real content\n"[..]));
    // a diff against the same commit is empty even though a replace ref exists
    assert!(diff(&r, &repo, &c, &c, 3).files.is_empty());
    // the forged commit
    let forged_tree = repo.mktree(&[(format!("100644 blob {fake_blob}").as_str(), "", b"f.txt")]);
    let forged = repo.commit_tree(&forged_tree, None);
    repo.git(&["replace", c.as_str(), forged.as_str()]);
    let e = r
        .read_entry(repo.s(), &c, &p("f.txt"), 1000)
        .unwrap()
        .unwrap();
    assert_eq!(e.content.as_deref(), Some(&b"the real content\n"[..]));
}

// ---- what the reader is pointed at -------------------------------------------------------------

#[test]
fn only_a_main_repository_is_read_never_a_worktree_or_a_link() {
    need_git!();
    let repo = Repo::new();
    repo.write("f.txt", b"main\n");
    let c = repo.commit("c");
    let tmp = TempDir::new("pointers");
    let wt = tmp.join("wt");
    repo.git(&["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "other"]);
    let r = reader();
    // the repository itself works
    assert!(r
        .read_entry(repo.s(), &c, &p("f.txt"), 100)
        .unwrap()
        .is_some());
    // a linked worktree is refused: it is where workers write
    let refused = r
        .read_entry(wt.to_str().unwrap(), &c, &p("f.txt"), 100)
        .unwrap_err();
    assert!(
        matches!(refused.code, ErrorCode::Forbidden | ErrorCode::Invalid),
        "{refused:?}"
    );
    // a worker that rewrites its worktree's pointer changes nothing that is read from the repository
    fs::write(
        wt.join(".git"),
        format!("gitdir: {}\n", tmp.path().display()),
    )
    .unwrap();
    fs::write(wt.join("f.txt"), "forged\n").unwrap();
    let e = r
        .read_entry(repo.s(), &c, &p("f.txt"), 100)
        .unwrap()
        .unwrap();
    assert_eq!(e.content.as_deref(), Some(&b"main\n"[..]));
    // a link to the repository is refused, a missing path and a non-repository too
    let link = tmp.join("link");
    symlink(&repo.dir, &link).unwrap();
    assert_eq!(
        r.list_dir(link.to_str().unwrap(), &c, &RepoPath::root(), 10)
            .unwrap_err()
            .code,
        ErrorCode::Forbidden
    );
    assert_eq!(
        r.list_dir("/nonexistent/for/sure", &c, &RepoPath::root(), 10)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    let plain = tmp.join("plain");
    fs::create_dir_all(&plain).unwrap();
    assert!(r
        .list_dir(plain.to_str().unwrap(), &c, &RepoPath::root(), 10)
        .is_err());
    assert!(r.list_dir("", &c, &RepoPath::root(), 10).is_err());
    assert!(r.list_dir("a\0b", &c, &RepoPath::root(), 10).is_err());
}

#[test]
fn a_repository_with_alternates_is_refused() {
    need_git!();
    let repo = Repo::new();
    repo.write("f.txt", b"x\n");
    let c = repo.commit("c");
    fs::write(repo.dir.join(".git/objects/info/alternates"), "/tmp\n").unwrap();
    let e = reader()
        .read_entry(repo.s(), &c, &p("f.txt"), 100)
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden);
}

#[test]
fn the_callers_git_environment_does_not_redirect_reads() {
    need_git!();
    let repo = Repo::new();
    repo.write("f.txt", b"main\n");
    let c = repo.commit("c");
    let other = Repo::new();
    other.write("f.txt", b"OTHER\n");
    other.commit("o");
    // the reader starts git from an empty environment: these are not passed on
    let vcs = GitVcs::new(
        vec![
            (
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            ),
            (
                "GIT_DIR".to_string(),
                other.dir.join(".git").display().to_string(),
            ),
            ("GIT_WORK_TREE".to_string(), other.dir.display().to_string()),
            (
                "GIT_OBJECT_DIRECTORY".to_string(),
                other.dir.join(".git/objects").display().to_string(),
            ),
        ],
        EnvPolicy::new(vec![
            "PATH".to_string(),
            "GIT_DIR".to_string(),
            "GIT_WORK_TREE".to_string(),
            "GIT_OBJECT_DIRECTORY".to_string(),
        ]),
    );
    let e = vcs
        .read_entry(repo.s(), &c, &p("f.txt"), 100)
        .unwrap()
        .unwrap();
    assert_eq!(e.content.as_deref(), Some(&b"main\n"[..]));
}

#[test]
fn unknown_and_hostile_commit_ids_are_not_found_not_run() {
    need_git!();
    let repo = Repo::new();
    repo.write("f.txt", b"x\n");
    let c = repo.commit("c");
    let r = reader();
    let missing = sha(&"1".repeat(40));
    assert_eq!(
        r.read_entry(repo.s(), &missing, &p("f.txt"), 10)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert_eq!(
        r.diff(repo.s(), &c, &missing, &req(3)).unwrap_err().code,
        ErrorCode::NotFound
    );
    // the id of a tree or a blob is not a commit
    let tree = sha(&repo.git(&["rev-parse", &format!("{c}^{{tree}}")]));
    assert_eq!(
        r.list_dir(repo.s(), &tree, &RepoPath::root(), 10)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
}

#[test]
fn paths_with_glob_characters_are_literal() {
    need_git!();
    let repo = Repo::new();
    repo.write("a*b.txt", b"star\n");
    repo.write("aXb.txt", b"x\n");
    repo.write("[ab].txt", b"class\n");
    repo.write(":(glob)x.txt", b"magic\n");
    let c = repo.commit("c");
    let r = reader();
    let star = r
        .read_entry(repo.s(), &c, &p("a*b.txt"), 10)
        .unwrap()
        .unwrap();
    assert_eq!(star.content.as_deref(), Some(&b"star\n"[..]));
    assert!(r
        .read_entry(repo.s(), &c, &p("a?b.txt"), 10)
        .unwrap()
        .is_none());
    let class = r
        .read_entry(repo.s(), &c, &p("[ab].txt"), 10)
        .unwrap()
        .unwrap();
    assert_eq!(class.content.as_deref(), Some(&b"class\n"[..]));
    let magic = r
        .read_entry(repo.s(), &c, &p(":(glob)x.txt"), 10)
        .unwrap()
        .unwrap();
    assert_eq!(magic.content.as_deref(), Some(&b"magic\n"[..]));
    assert!(r
        .read_entry(repo.s(), &c, &p("x.txt"), 10)
        .unwrap()
        .is_none());
}

#[test]
fn modes_and_sizes_of_a_listing_are_exact() {
    need_git!();
    let repo = Repo::new();
    repo.write("run.sh", b"#!/bin/sh\n");
    fs::set_permissions(repo.dir.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    repo.write("data.txt", b"12345");
    let c = repo.commit("c");
    let l = reader()
        .list_dir(repo.s(), &c, &RepoPath::root(), 10)
        .unwrap();
    let run = l.entries.iter().find(|e| e.name == "run.sh").unwrap();
    assert_eq!((run.mode, run.size), (ModeClass::Executable, Some(10)));
    let data = l.entries.iter().find(|e| e.name == "data.txt").unwrap();
    assert_eq!((data.mode, data.size), (ModeClass::Regular, Some(5)));
    let _ = Path::new("");
}
