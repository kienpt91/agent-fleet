//! Application tests: the read-only workbench. The orchestrator resolves a task to the commits it
//! recorded; `RepoBrowser` answers questions about them through the `RepoReader` port, here the
//! in-memory repository. Nothing is read from a worktree and nothing but recorded commits can be
//! named.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_app::api::{is_read_only, parse_command, Command, Reply};
use fleet_app::browse::{
    approx_size, CommitSel, RepoBrowser, RepoQuery, RepoReply, RepoRequest, RepoTarget,
};
use fleet_app::ports::{DiffRequest, DirListing, EntryRead, RepoReader};
use fleet_app::testing::repo::{Item, MemoryRepo};
use fleet_app::testing::Harness;
use fleet_domain::contract::{ContractRead, ResultStatus, WorkerResult};
use fleet_domain::error::{ErrorCode, Result};
use fleet_domain::ids::Sha;
use fleet_domain::repo::diff::DiffSet;
use fleet_domain::repo::view::ImageLimits;
use fleet_domain::repo::RepoPath;
use fleet_domain::value::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn s(x: &str) -> Value {
    Value::str(x)
}

fn sha(c: char) -> Sha {
    Sha::parse(&c.to_string().repeat(40)).unwrap()
}

fn query(cmd: &str, pairs: Vec<(&str, Value)>) -> Result<RepoQuery> {
    match parse_command(cmd, &Value::object(pairs))? {
        Command::Repo(q) => Ok(q),
        other => panic!("not a repo command: {other:?}"),
    }
}

fn file(path: &str, text: &str) -> (String, Item) {
    (path.to_string(), Item::File(text.as_bytes().to_vec()))
}

// ---- parsing ---------------------------------------------------------------------------------------

#[test]
fn repo_commands_parse_with_their_defaults() {
    let q = query("repo.tree", vec![("task", s("r1/a"))]).unwrap();
    assert_eq!(
        q,
        RepoQuery::Tree {
            task: "r1/a".to_string(),
            at: CommitSel::Head,
            path: RepoPath::root()
        }
    );
    let q = query(
        "repo.file",
        vec![
            ("task", s("a")),
            ("commit", s("base")),
            ("path", s("src/main.rs")),
        ],
    )
    .unwrap();
    assert!(
        matches!(&q, RepoQuery::File { at: CommitSel::Base, path, .. } if path.as_str() == "src/main.rs")
    );
    let id = "ab".repeat(20);
    let q = query(
        "repo.blob",
        vec![("task", s("a")), ("commit", s(&id)), ("path", s("x.png"))],
    )
    .unwrap();
    assert!(matches!(&q, RepoQuery::Blob { at: CommitSel::Sha(h), .. } if h.as_str() == id));
    assert!(matches!(
        query(
            "repo.markdown",
            vec![("task", s("a")), ("path", s("README.md"))]
        )
        .unwrap(),
        RepoQuery::Markdown { .. }
    ));
    let q = query("repo.diff", vec![("task", s("a"))]).unwrap();
    assert_eq!(
        q,
        RepoQuery::Diff {
            task: "a".to_string(),
            base: CommitSel::Base,
            head: CommitSel::Head,
            context: 3
        }
    );
    let q = query(
        "repo.diff",
        vec![("task", s("a")), ("context", Value::int(9999u64))],
    )
    .unwrap();
    assert!(matches!(q, RepoQuery::Diff { context: 25, .. }));
    let q = query(
        "repo.diff",
        vec![("task", s("a")), ("context", Value::int(0u64))],
    )
    .unwrap();
    assert!(matches!(q, RepoQuery::Diff { context: 0, .. }));
    for cmd in [
        "repo.tree",
        "repo.file",
        "repo.blob",
        "repo.markdown",
        "repo.diff",
    ] {
        let parsed = parse_command(cmd, &Value::object([("task", s("a"))])).unwrap();
        assert!(is_read_only(&parsed), "{cmd} changes nothing");
    }
}

#[test]
fn clients_cannot_name_refs_or_hostile_paths() {
    for commit in [
        "main",
        "HEAD",
        "refs/heads/x",
        "HEAD~1",
        "head^",
        "abc",
        "--output=/tmp/x",
        "",
        "ABCDEF0123456789ABCDEF0123456789ABCDEF01",
        &"g".repeat(40),
        &"a".repeat(41),
        "@{u}",
    ] {
        let r = query("repo.tree", vec![("task", s("a")), ("commit", s(commit))]);
        assert_eq!(r.unwrap_err().code, ErrorCode::Invalid, "{commit:?}");
    }
    for path in [
        "../x",
        "/etc/passwd",
        "a\\b",
        "a\0b",
        ".git/config",
        "a//b",
        "x/",
        "a\nb",
    ] {
        let r = query("repo.file", vec![("task", s("a")), ("path", s(path))]);
        assert_eq!(r.unwrap_err().code, ErrorCode::Invalid, "{path:?}");
    }
    // wrong types and missing fields
    assert!(query("repo.tree", vec![]).is_err());
    assert!(query("repo.tree", vec![("task", Value::int(1u64))]).is_err());
    assert!(query(
        "repo.file",
        vec![("task", s("a")), ("path", Value::int(1u64))]
    )
    .is_err());
    assert!(query("repo.diff", vec![("task", s("a")), ("context", s("3"))]).is_err());
    assert!(query("repo.diff", vec![("task", s("a")), ("base", s("main"))]).is_err());
    // the commands are not offered under other names
    assert!(parse_command("repo.write", &Value::object([("task", s("a"))])).is_err());
    assert!(parse_command("repo.target", &Value::object([("task", s("a"))])).is_err());
}

// ---- the orchestrator resolves a task -------------------------------------------------------------

fn setup() -> (Harness, Sha, Sha) {
    let mut h = Harness::new();
    h.call(
        "run.create",
        Value::object([("objective", s("o")), ("repo", s("/repo"))]),
    )
    .unwrap();
    h.call(
        "task.add",
        Value::object([
            ("run", s("r1")),
            ("id", s("a")),
            ("title", s("task a")),
            ("spec", s("do it")),
        ]),
    )
    .unwrap();
    h.call("run.start", Value::object([("run", s("r1"))]))
        .unwrap();
    let st = h.call("status", Value::Null).unwrap().data().clone();
    let run = &st.get("runs").and_then(Value::as_array).unwrap()[0];
    let base = Sha::parse(run.get("base_sha").and_then(Value::as_str).unwrap()).unwrap();
    // the worker finishes: its work is committed and recorded as the task's head
    let session = h.world.session_for("a").expect("a live session");
    let spec = h.world.session_spec(session).unwrap();
    h.world.write_result(
        &spec.cwd,
        ContractRead::Valid(WorkerResult {
            status: ResultStatus::Done,
            summary: "done".to_string(),
        }),
    );
    h.world.mark_dirty(&spec.cwd);
    h.world.exit(session, 0);
    h.orch.tick();
    let st = h.call("status", Value::Null).unwrap().data().clone();
    let task = &st.get("runs").and_then(Value::as_array).unwrap()[0]
        .get("tasks")
        .and_then(Value::as_array)
        .unwrap()[0];
    let head = Sha::parse(
        task.get("head")
            .and_then(Value::as_str)
            .expect("a recorded head"),
    )
    .unwrap();
    (h, base, head)
}

fn request(h: &mut Harness, cmd: &str, pairs: Vec<(&str, Value)>) -> Result<RepoRequest> {
    let command = parse_command(cmd, &Value::object(pairs))?;
    match h.orch.handle(command)? {
        Reply::Repo(r) => Ok(*r),
        other => panic!("not a repository request: {other:?}"),
    }
}

#[test]
fn the_orchestrator_names_the_repository_and_the_recorded_commits_of_a_task() {
    let (mut h, base, head) = setup();
    for reference in ["r1/a", "a"] {
        let r = request(&mut h, "repo.tree", vec![("task", s(reference))]).unwrap();
        assert_eq!(
            r.target,
            RepoTarget {
                task: "r1/a".to_string(),
                repo: "/repo".to_string(),
                base: base.clone(),
                head: Some(head.clone()),
                merged: None,
            }
        );
        assert!(matches!(r.query, RepoQuery::Tree { .. }));
    }
    assert_eq!(
        request(&mut h, "repo.tree", vec![("task", s("nope"))])
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert_eq!(
        request(&mut h, "repo.tree", vec![("task", s("r9/a"))])
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    // answering a request changes nothing: the event log did not grow
    let before = h.world.event_kinds().len();
    request(&mut h, "repo.diff", vec![("task", s("a"))]).unwrap();
    assert_eq!(h.world.event_kinds().len(), before);
}

#[test]
fn a_task_without_a_recorded_commit_has_no_head_but_has_a_base() {
    let mut h = Harness::new();
    h.call(
        "run.create",
        Value::object([("objective", s("o")), ("repo", s("/repo"))]),
    )
    .unwrap();
    h.call(
        "task.add",
        Value::object([
            ("run", s("r1")),
            ("id", s("a")),
            ("title", s("t")),
            ("spec", s("s")),
        ]),
    )
    .unwrap();
    let r = request(&mut h, "repo.tree", vec![("task", s("a"))]).unwrap();
    assert_eq!(r.target.head, None);
    assert_eq!(
        r.target.resolve(&CommitSel::Head).unwrap_err().code,
        ErrorCode::NotFound
    );
    assert!(r.target.resolve(&CommitSel::Base).is_ok());
}

#[test]
fn only_recorded_commits_can_be_named() {
    let target = RepoTarget {
        task: "r1/a".to_string(),
        repo: "/repo".to_string(),
        base: sha('1'),
        head: Some(sha('2')),
        merged: Some(sha('3')),
    };
    for good in ['1', '2', '3'] {
        assert_eq!(
            target.resolve(&CommitSel::Sha(sha(good))).unwrap(),
            sha(good)
        );
    }
    assert_eq!(
        target.resolve(&CommitSel::Sha(sha('4'))).unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(target.resolve(&CommitSel::Head).unwrap(), sha('2'));
    assert_eq!(target.resolve(&CommitSel::Base).unwrap(), sha('1'));
    assert_eq!(CommitSel::parse("head").unwrap().text(), "head");
    assert_eq!(
        CommitSel::parse(&"a".repeat(40)).unwrap().text(),
        "a".repeat(40)
    );
}

// ---- the browser -----------------------------------------------------------------------------------

/// A reader that counts its calls and remembers which repositories it was asked about.
struct Spy {
    inner: MemoryRepo,
    calls: AtomicUsize,
    repos: Mutex<Vec<String>>,
}

impl Spy {
    fn note(&self, repo: &str) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.repos.lock().unwrap().push(repo.to_string());
    }
}

impl RepoReader for Spy {
    fn list_dir(&self, repo: &str, c: &Sha, d: &RepoPath, l: usize) -> Result<DirListing> {
        self.note(repo);
        self.inner.list_dir(repo, c, d, l)
    }
    fn read_entry(&self, repo: &str, c: &Sha, p: &RepoPath, m: usize) -> Result<Option<EntryRead>> {
        self.note(repo);
        self.inner.read_entry(repo, c, p, m)
    }
    fn diff(&self, repo: &str, b: &Sha, h: &Sha, r: &DiffRequest) -> Result<DiffSet> {
        self.note(repo);
        self.inner.diff(repo, b, h, r)
    }
}

fn png_sized(w: u32, h: u32) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&[8, 2, 0, 0, 0, 0, 0, 0, 0]);
    v
}

fn png() -> Vec<u8> {
    png_sized(1, 1)
}

fn browser() -> (Arc<Spy>, RepoBrowser, RepoTarget) {
    let (base, head) = (sha('1'), sha('2'));
    let mut mem = MemoryRepo::new();
    mem.commit(
        &base,
        vec![
            file("README.md", "# hi\n"),
            file("src/a.rs", "one\ntwo\nthree\n"),
        ],
    );
    mem.commit(
        &head,
        vec![
            file("README.md", "# hi\n"),
            file("src/a.rs", "one\nTWO\nthree\nfour\n"),
            ("pic.png".to_string(), Item::File(png())),
            ("notes.txt".to_string(), Item::File(b"a\0b".to_vec())),
            ("link".to_string(), Item::Symlink("../secret".to_string())),
            (
                "evil\u{1b}[31m\nname.txt".to_string(),
                Item::File(b"x\x1b[2Jy\n".to_vec()),
            ),
            (
                "big.bin".to_string(),
                Item::File(vec![b'a'; 5 * 1024 * 1024]),
            ),
            (
                "huge.png".to_string(),
                Item::File(png_sized(40_000, 40_000)),
            ),
            ("wide.png".to_string(), Item::File(png_sized(9000, 10))),
            ("big.png".to_string(), {
                let mut v = png();
                v.resize(4 * 1024 * 1024 + 10, 0);
                Item::File(v)
            }),
            ("mod".to_string(), Item::Submodule("c".repeat(40))),
        ],
    );
    let spy = Arc::new(Spy {
        inner: mem,
        calls: AtomicUsize::new(0),
        repos: Mutex::new(Vec::new()),
    });
    let b = RepoBrowser::new(spy.clone());
    let target = RepoTarget {
        task: "r1/a".to_string(),
        repo: "/repo".to_string(),
        base,
        head: Some(head),
        merged: None,
    };
    (spy, b, target)
}

fn ask(b: &RepoBrowser, t: &RepoTarget, cmd: &str, pairs: Vec<(&str, Value)>) -> Result<RepoReply> {
    let mut all = vec![("task", s("r1/a"))];
    all.extend(pairs);
    b.run(t, &query(cmd, all)?)
}

fn doc(r: Result<RepoReply>) -> Value {
    match r.unwrap() {
        RepoReply::Doc(v) => v,
        RepoReply::Image(_) => panic!("an image"),
    }
}

fn arr<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k).and_then(Value::as_array).unwrap()
}

fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap()
}

#[test]
fn the_tree_lists_the_head_commit_by_default_sorted_and_safe() {
    let (spy, b, t) = browser();
    let d = doc(ask(&b, &t, "repo.tree", vec![]));
    assert_eq!(text(&d, "schema"), "fleet.repo.tree/1");
    assert_eq!(text(&d, "task"), "r1/a");
    assert_eq!(text(&d, "at"), "head");
    assert_eq!(text(&d, "commit"), "2".repeat(40));
    assert_eq!(text(&d, "path"), "");
    assert_eq!(d.get("truncated").and_then(Value::as_bool), Some(false));
    let names: Vec<&str> = arr(&d, "entries").iter().map(|e| text(e, "name")).collect();
    assert_eq!(names[0], "src", "directories first");
    assert!(names.contains(&"README.md") && names.contains(&"pic.png"));
    // a name with control characters is shown escaped and cannot be requested back
    let evil = arr(&d, "entries")
        .iter()
        .find(|e| text(e, "name").contains("name.txt"))
        .unwrap();
    assert!(text(evil, "name").contains("\\u{1b}") && text(evil, "name").contains("\\u{a}"));
    assert!(evil.get("path").unwrap().is_null());
    let src = arr(&d, "entries")
        .iter()
        .find(|e| text(e, "name") == "src")
        .unwrap();
    assert_eq!(text(src, "kind"), "dir");
    assert_eq!(text(src, "path"), "src");
    assert!(src.get("size").unwrap().is_null());
    // the base commit
    let d = doc(ask(&b, &t, "repo.tree", vec![("commit", s("base"))]));
    assert_eq!(text(&d, "at"), "base");
    assert_eq!(arr(&d, "entries").len(), 2);
    // a subdirectory
    let d = doc(ask(&b, &t, "repo.tree", vec![("path", s("src"))]));
    assert_eq!(text(&arr(&d, "entries")[0], "path"), "src/a.rs");
    // only the real repository was read
    assert!(spy.repos.lock().unwrap().iter().all(|r| r == "/repo"));
    // errors
    let missing = ask(&b, &t, "repo.tree", vec![("path", s("nope"))]).unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound);
}

#[test]
fn a_file_is_text_binary_a_link_a_submodule_or_a_placeholder() {
    let (_, b, t) = browser();
    let d = doc(ask(&b, &t, "repo.file", vec![("path", s("src/a.rs"))]));
    assert_eq!(text(&d, "schema"), "fleet.repo.file/1");
    assert_eq!(text(&d, "view"), "text");
    assert_eq!(text(&d, "class"), "text");
    let lines: Vec<&str> = arr(&d, "lines")
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert_eq!(lines, ["one", "TWO", "three", "four"]);
    assert_eq!(d.get("line_count").and_then(Value::as_u64), Some(4));
    assert_eq!(d.get("markdown").and_then(Value::as_bool), Some(false));
    assert_eq!(d.get("size").and_then(Value::as_u64), Some(19));
    let md = doc(ask(&b, &t, "repo.file", vec![("path", s("README.md"))]));
    assert_eq!(md.get("markdown").and_then(Value::as_bool), Some(true));
    // the same file at the base commit
    let old = doc(ask(
        &b,
        &t,
        "repo.file",
        vec![("path", s("src/a.rs")), ("commit", s("base"))],
    ));
    assert_eq!(arr(&old, "lines")[1].as_str(), Some("two"));
    let bin = doc(ask(&b, &t, "repo.file", vec![("path", s("notes.txt"))]));
    assert_eq!(
        (text(&bin, "view"), text(&bin, "class")),
        ("binary", "binary")
    );
    assert!(arr(&bin, "lines").is_empty());
    let img = doc(ask(&b, &t, "repo.file", vec![("path", s("pic.png"))]));
    assert_eq!((text(&img, "view"), text(&img, "image")), ("image", "png"));
    let link = doc(ask(&b, &t, "repo.file", vec![("path", s("link"))]));
    assert_eq!(text(&link, "view"), "symlink");
    assert_eq!(text(&link, "target"), "../secret");
    assert!(
        link.get("target_path").unwrap().is_null(),
        "a target outside the repository"
    );
    let module = doc(ask(&b, &t, "repo.file", vec![("path", s("mod"))]));
    assert_eq!(text(&module, "view"), "submodule");
    assert_eq!(text(&module, "submodule_commit"), "c".repeat(40));
    let big = doc(ask(&b, &t, "repo.file", vec![("path", s("big.bin"))]));
    assert_eq!(text(&big, "view"), "too_large");
    assert_eq!(
        big.get("size").and_then(Value::as_u64),
        Some(5 * 1024 * 1024)
    );
    assert_eq!(big.get("limit").and_then(Value::as_u64), Some(1024 * 1024));
    // a name with control characters cannot even be asked for
    let e = ask(
        &b,
        &t,
        "repo.file",
        vec![("path", s("evil\u{1b}[31m\nname.txt"))],
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
}

#[test]
fn files_that_do_not_exist_or_are_directories_are_errors() {
    let (_, b, t) = browser();
    assert_eq!(
        ask(&b, &t, "repo.file", vec![("path", s("nope.txt"))])
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert_eq!(
        ask(&b, &t, "repo.file", vec![("path", s("src"))])
            .unwrap_err()
            .code,
        ErrorCode::Invalid
    );
    assert_eq!(
        ask(&b, &t, "repo.file", vec![]).unwrap_err().code,
        ErrorCode::Invalid,
        "the root is a directory"
    );
    // a path below a link is not followed
    assert_eq!(
        ask(&b, &t, "repo.file", vec![("path", s("link/passwd"))])
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    // a commit the daemon did not record
    assert_eq!(
        ask(
            &b,
            &t,
            "repo.file",
            vec![("path", s("README.md")), ("commit", s(&"9".repeat(40)))]
        )
        .unwrap_err()
        .code,
        ErrorCode::NotFound
    );
    // no head yet
    let mut fresh = t.clone();
    fresh.head = None;
    assert_eq!(
        ask(&b, &fresh, "repo.file", vec![("path", s("README.md"))])
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
}

#[test]
fn the_blob_api_serves_images_and_nothing_else() {
    let (_, b, t) = browser();
    match ask(&b, &t, "repo.blob", vec![("path", s("pic.png"))]).unwrap() {
        RepoReply::Image(i) => {
            assert_eq!(i.kind.content_type(), "image/png");
            assert_eq!(i.bytes, png());
        }
        other => panic!("{other:?}"),
    }
    for (path, code) in [
        ("README.md", ErrorCode::Invalid),
        ("notes.txt", ErrorCode::Invalid),
        ("link", ErrorCode::Invalid),
        ("mod", ErrorCode::Invalid),
        ("src", ErrorCode::Invalid),
        ("big.png", ErrorCode::Limit),
        ("huge.png", ErrorCode::Limit),
        ("wide.png", ErrorCode::Limit),
        ("big.bin", ErrorCode::Limit),
        ("nope.png", ErrorCode::NotFound),
    ] {
        let e = ask(&b, &t, "repo.blob", vec![("path", s(path))]).unwrap_err();
        assert_eq!(e.code, code, "{path}: {}", e.message);
        // the dashboard answers "not an image" with 415 by this subject, a directory with 400
        let not_image = !matches!(
            path,
            "src" | "big.png" | "huge.png" | "wide.png" | "big.bin" | "nope.png"
        );
        assert_eq!(
            e.subject.as_deref() == Some(fleet_app::browse::NOT_AN_IMAGE),
            not_image,
            "{path}"
        );
    }
}

#[test]
fn the_diff_compares_base_and_head_by_default() {
    let (_, b, t) = browser();
    let d = doc(ask(&b, &t, "repo.diff", vec![]));
    assert_eq!(text(&d, "schema"), "fleet.repo.diff/1");
    assert_eq!(text(&d, "base"), "1".repeat(40));
    assert_eq!(text(&d, "head"), "2".repeat(40));
    assert_eq!(d.get("context").and_then(Value::as_u64), Some(3));
    let files = arr(&d, "files");
    let a = files
        .iter()
        .find(|f| text(f, "path") == "src/a.rs")
        .unwrap();
    assert_eq!(text(a, "status"), "modified");
    assert_eq!(a.get("additions").and_then(Value::as_u64), Some(2));
    assert_eq!(a.get("deletions").and_then(Value::as_u64), Some(1));
    let h = &arr(a, "hunks")[0];
    assert_eq!(h.get("old_start").and_then(Value::as_u64), Some(1));
    let lines = arr(h, "lines");
    assert_eq!(text(&lines[0], "k"), "ctx");
    assert_eq!(text(&lines[1], "k"), "del");
    assert_eq!(text(&lines[1], "t"), "two");
    assert_eq!(lines[1].get("o").and_then(Value::as_u64), Some(2));
    assert!(lines[1].get("n").unwrap().is_null());
    let stats = d.get("stats").unwrap();
    assert_eq!(
        stats.get("files").and_then(Value::as_u64),
        Some(files.len() as u64)
    );
    assert_eq!(d.get("truncated").and_then(Value::as_bool), Some(false));
    // paths that cannot be requested are flagged
    let evil = files
        .iter()
        .find(|f| text(f, "path").contains("name.txt"))
        .unwrap();
    assert_eq!(evil.get("path_ok").and_then(Value::as_bool), Some(false));
    assert!(!text(evil, "path").contains('\u{1b}'));
    // explicit commits, in either order
    let back = doc(ask(
        &b,
        &t,
        "repo.diff",
        vec![("base", s("head")), ("head", s("base"))],
    ));
    let a = arr(&back, "files")
        .iter()
        .find(|f| text(f, "path") == "src/a.rs")
        .unwrap();
    assert_eq!(a.get("additions").and_then(Value::as_u64), Some(1));
    // unrecorded commits
    let e = ask(&b, &t, "repo.diff", vec![("head", s(&"9".repeat(40)))]).unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
}

#[test]
fn answers_are_cached_by_commit_and_path_and_the_cache_is_bounded() {
    let (spy, b, t) = browser();
    let before = spy.calls.load(Ordering::SeqCst);
    let first = doc(ask(&b, &t, "repo.file", vec![("path", s("src/a.rs"))]));
    let calls = spy.calls.load(Ordering::SeqCst);
    assert!(calls > before);
    let second = doc(ask(&b, &t, "repo.file", vec![("path", s("src/a.rs"))]));
    assert_eq!(first, second);
    assert_eq!(
        spy.calls.load(Ordering::SeqCst),
        calls,
        "the second answer came from the cache"
    );
    // another commit or another path is another entry
    doc(ask(
        &b,
        &t,
        "repo.file",
        vec![("path", s("src/a.rs")), ("commit", s("base"))],
    ));
    assert!(spy.calls.load(Ordering::SeqCst) > calls);
    // errors are not cached
    let n = spy.calls.load(Ordering::SeqCst);
    assert!(ask(&b, &t, "repo.file", vec![("path", s("nope"))]).is_err());
    assert!(ask(&b, &t, "repo.file", vec![("path", s("nope"))]).is_err());
    assert!(spy.calls.load(Ordering::SeqCst) >= n + 2);
    // many distinct questions never grow the cache past its bound
    for i in 0..200 {
        let q = format!("src/missing{i}");
        let _ = ask(&b, &t, "repo.tree", vec![("path", s(&q))]);
        let _ = ask(
            &b,
            &t,
            "repo.diff",
            vec![("context", Value::int(i as u64 % 26))],
        );
        let _ = ask(
            &b,
            &t,
            "repo.file",
            vec![
                ("path", s("src/a.rs")),
                ("commit", s(if i % 2 == 0 { "base" } else { "head" })),
            ],
        );
    }
    let (entries, bytes) = b.cached();
    assert!(entries <= 64, "{entries} cached documents");
    assert!(bytes <= 16 * 1024 * 1024, "{bytes} bytes cached");
    // images are not cached at all
    let n = spy.calls.load(Ordering::SeqCst);
    ask(&b, &t, "repo.blob", vec![("path", s("pic.png"))]).unwrap();
    ask(&b, &t, "repo.blob", vec![("path", s("pic.png"))]).unwrap();
    assert!(spy.calls.load(Ordering::SeqCst) >= n + 2);
}

#[test]
fn a_document_too_large_to_cache_is_still_answered() {
    // a diff of many lines is bigger than one cache entry may be
    let (base, head) = (sha('1'), sha('2'));
    let mut mem = MemoryRepo::new();
    mem.commit(&base, vec![]);
    let body: String = (0..2500)
        .map(|i| format!("{i:06} {}\n", "x".repeat(100)))
        .collect();
    let files: Vec<(String, Item)> = (0..12).map(|i| file(&format!("f{i}.txt"), &body)).collect();
    mem.commit(&head, files);
    let b = RepoBrowser::new(Arc::new(mem));
    let t = RepoTarget {
        task: "r1/a".to_string(),
        repo: "/repo".to_string(),
        base,
        head: Some(head),
        merged: None,
    };
    let d = doc(ask(&b, &t, "repo.diff", vec![]));
    assert!(approx_size(&d) > 2 * 1024 * 1024, "{}", approx_size(&d));
    assert_eq!(b.cached().0, 0);
    let again = doc(ask(&b, &t, "repo.diff", vec![]));
    assert_eq!(d, again);
}

#[test]
fn approximate_sizes_grow_with_content() {
    assert!(approx_size(&Value::Null) > 0);
    assert!(approx_size(&s(&"x".repeat(1000))) > approx_size(&s("x")));
    assert!(approx_size(&Value::array([s("a"), s("b")])) > approx_size(&Value::array([])));
    assert!(
        approx_size(&Value::object([("k", s("v"))])) > approx_size(&Value::object::<&str, _>([]))
    );
}

// ---- markdown --------------------------------------------------------------------------------------

fn md_browser(files: Vec<(String, Item)>) -> (RepoBrowser, RepoTarget) {
    let (base, head) = (sha('1'), sha('2'));
    let mut mem = MemoryRepo::new();
    mem.commit(&base, vec![]);
    mem.commit(&head, files);
    let t = RepoTarget {
        task: "r1/a".to_string(),
        repo: "/repo".to_string(),
        base,
        head: Some(head),
        merged: None,
    };
    (RepoBrowser::new(Arc::new(mem)), t)
}

fn collect_t(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Array(items) => items.iter().for_each(|i| collect_t(i, out)),
        Value::Object(m) => {
            if let Some(Value::Str(t)) = m.get("t") {
                out.push(t.clone());
            }
            m.values().for_each(|i| collect_t(i, out));
        }
        _ => {}
    }
}

#[test]
fn markdown_is_parsed_into_a_closed_tree_with_links_resolved_against_the_documents_directory() {
    let source = "# Title\n\nSee [the guide](../guide.md#top), [site](https://example.com/x), [bad](javascript:alert(1)) and ![logo](img/logo.png) and ![remote](https://tracker.example/p.png).\n\n<script>alert(1)</script>\n";
    let (b, t) = md_browser(vec![
        file("docs/sub/README.md", source),
        file("docs/guide.md", "# guide\n"),
    ]);
    let d = doc(ask(
        &b,
        &t,
        "repo.markdown",
        vec![("path", s("docs/sub/README.md"))],
    ));
    assert_eq!(text(&d, "schema"), "fleet.repo.markdown/1");
    assert_eq!(text(&d, "path"), "docs/sub/README.md");
    assert_eq!(text(&d, "commit"), "2".repeat(40));
    assert_eq!(d.get("truncated").and_then(Value::as_bool), Some(false));
    assert!(d.get("notice").unwrap().is_null());
    let blocks = arr(&d, "blocks");
    assert_eq!(text(&blocks[0], "t"), "h");
    let mut kinds = Vec::new();
    collect_t(d.get("blocks").unwrap(), &mut kinds);
    for k in &kinds {
        assert!(
            [
                "h", "p", "text", "link", "image", "code", "em", "strong", "del", "br", "quote",
                "ul", "ol", "table", "hr"
            ]
            .contains(&k.as_str()),
            "{k}"
        );
    }
    let canonical = d.canonical();
    assert!(
        canonical.contains("\"path\":\"docs/guide.md\""),
        "{canonical}"
    );
    assert!(canonical.contains("\"url\":\"https://example.com/x\""));
    assert!(canonical.contains("\"path\":\"docs/sub/img/logo.png\""));
    assert!(
        !canonical.contains("javascript:alert(1)\""),
        "the dangerous target must not survive as a link: {canonical}"
    );
    // the remote image is not loaded
    assert!(canonical.contains("remote images are not loaded"));
    // raw markup is text
    assert!(canonical.contains("<script>alert(1)</script>"));
    assert!(!kinds.iter().any(|k| k == "script"));
}

#[test]
fn only_small_markdown_text_is_rendered() {
    let big = "word ".repeat(120_000);
    let (b, t) = md_browser(vec![
        file("big.md", &big),
        file("notes.txt", "# not markdown\n"),
        ("bin.md".to_string(), Item::File(b"a\0b".to_vec())),
        (
            "lossy.md".to_string(),
            Item::File(b"caf\xe9 *ok*\n".to_vec()),
        ),
        ("dir/x.md".to_string(), Item::File(b"x".to_vec())),
    ]);
    for path in ["big.md", "notes.txt", "bin.md", "dir"] {
        let e = ask(&b, &t, "repo.markdown", vec![("path", s(path))]).unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid, "{path}: {}", e.message);
    }
    assert_eq!(
        ask(&b, &t, "repo.markdown", vec![("path", s("nope.md"))])
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    let d = doc(ask(&b, &t, "repo.markdown", vec![("path", s("lossy.md"))]));
    assert_eq!(text(&d, "notice"), "Invalid UTF-8 was replaced.");
    assert!(d.canonical().contains("caf\u{fffd}"));
}

// ---- image dimensions ------------------------------------------------------------------------------

#[test]
fn an_image_that_declares_a_huge_canvas_is_never_served_and_is_a_placeholder_in_its_file_document()
{
    let (_, b, t) = browser();
    // 40000 x 40000 pixels declared by a 33 byte file
    let e = ask(&b, &t, "repo.blob", vec![("path", s("huge.png"))]).unwrap_err();
    assert_eq!(e.code, ErrorCode::Limit, "{}", e.message);
    assert!(e.message.contains("40000 x 40000"), "{}", e.message);
    assert!(e.subject.is_none(), "it is a limit, not a media-type error");
    let wide = ask(&b, &t, "repo.blob", vec![("path", s("wide.png"))]).unwrap_err();
    assert_eq!(wide.code, ErrorCode::Limit);
    let d = doc(ask(&b, &t, "repo.file", vec![("path", s("huge.png"))]));
    assert_eq!(text(&d, "view"), "image_too_large");
    assert_eq!(text(&d, "class"), "image");
    assert_eq!(text(&d, "image"), "png");
    assert_eq!(
        (
            d.get("width").and_then(Value::as_u64),
            d.get("height").and_then(Value::as_u64)
        ),
        (Some(40_000), Some(40_000))
    );
    assert_eq!(d.get("max_side").and_then(Value::as_u64), Some(8192));
    assert_eq!(
        d.get("max_pixels").and_then(Value::as_u64),
        Some(32_000_000)
    );
    // an ordinary image says its size and is served with it
    let ok = doc(ask(&b, &t, "repo.file", vec![("path", s("pic.png"))]));
    assert_eq!(text(&ok, "view"), "image");
    assert_eq!(
        (
            ok.get("width").and_then(Value::as_u64),
            ok.get("max_side").map(Value::is_null)
        ),
        (Some(1), Some(true))
    );
    match ask(&b, &t, "repo.blob", vec![("path", s("pic.png"))]).unwrap() {
        RepoReply::Image(i) => assert_eq!((i.width, i.height), (1, 1)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_image_limits_can_be_changed_but_never_past_the_hard_caps() {
    let (spy, _, t) = browser();
    let strict = RepoBrowser::new(spy.clone()).with_image_limits(ImageLimits::new(0, 0));
    // zero is clamped up to one: a 1 x 1 image still passes, anything bigger does not
    assert!(ask(&strict, &t, "repo.blob", vec![("path", s("pic.png"))]).is_ok());
    assert_eq!(
        ask(&strict, &t, "repo.blob", vec![("path", s("wide.png"))])
            .unwrap_err()
            .code,
        ErrorCode::Limit
    );
    let tiny = RepoBrowser::new(spy.clone()).with_image_limits(ImageLimits::new(1, 1));
    assert!(ask(&tiny, &t, "repo.blob", vec![("path", s("pic.png"))]).is_ok());
    let generous = RepoBrowser::new(spy).with_image_limits(ImageLimits::new(u32::MAX, u64::MAX));
    // 40000 per side is above the hard cap of 16384 whatever is asked for
    assert_eq!(
        ask(&generous, &t, "repo.blob", vec![("path", s("huge.png"))])
            .unwrap_err()
            .code,
        ErrorCode::Limit
    );
}

#[test]
fn a_cached_document_names_the_selector_of_the_request_it_answers() {
    let (_, b, t) = browser();
    let head = "2".repeat(40);
    let first = doc(ask(
        &b,
        &t,
        "repo.file",
        vec![("path", s("src/a.rs")), ("commit", s("head"))],
    ));
    let second = doc(ask(
        &b,
        &t,
        "repo.file",
        vec![("path", s("src/a.rs")), ("commit", s(&head))],
    ));
    assert_eq!(text(&first, "at"), "head");
    assert_eq!(text(&second, "at"), head, "the same commit asked for by id");
    assert_eq!(text(&first, "commit"), text(&second, "commit"));
    let tree_head = doc(ask(&b, &t, "repo.tree", vec![("commit", s("head"))]));
    let tree_id = doc(ask(&b, &t, "repo.tree", vec![("commit", s(&head))]));
    assert_eq!(
        (text(&tree_head, "at"), text(&tree_id, "at")),
        ("head", head.as_str())
    );
    let md = doc(ask(
        &b,
        &t,
        "repo.markdown",
        vec![("path", s("README.md")), ("commit", s("head"))],
    ));
    let md_id = doc(ask(
        &b,
        &t,
        "repo.markdown",
        vec![("path", s("README.md")), ("commit", s(&head))],
    ));
    assert_eq!(
        (text(&md, "at"), text(&md_id, "at")),
        ("head", head.as_str())
    );
}
