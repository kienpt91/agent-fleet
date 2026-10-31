//! `Vcs::diff_summary` against real temporary git repositories: what a member's branch changed,
//! as the repository reports it (files, lines, renames, binary files), and the overlap of members
//! that follows from it. The adapter runs the hardened git; these tests build their fixtures with
//! plain git.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_app::ports::Vcs;
use fleet_domain::env::EnvPolicy;
use fleet_domain::ids::{Sha, TaskId};
use fleet_fanout::compare::overlap;
use fleet_fanout::diff::{ChangeKind, DiffStat};
use fleet_infra::git::GitVcs;
use fleet_infra::testutil::{git, git_available, make_repo, TempDir};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

macro_rules! need_git {
    () => {
        if !git_available() {
            return;
        }
    };
}

struct Fx {
    tmp: TempDir,
    repo: PathBuf,
    vcs: GitVcs,
    base: Sha,
}

impl Fx {
    fn new() -> Fx {
        let tmp = TempDir::new("git-diff");
        let repo = make_repo(&tmp);
        let vcs = GitVcs::new(
            vec![(
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            )],
            EnvPolicy::default_policy(),
        );
        let base = Sha::parse(&git(&repo, &["rev-parse", "HEAD"])).unwrap();
        Fx {
            tmp,
            repo,
            vcs,
            base,
        }
    }

    fn repo_s(&self) -> &str {
        self.repo.to_str().unwrap()
    }

    /// A branch from the base commit with the changes `edit` makes, committed. Returns its head.
    fn branch(&self, name: &str, edit: impl FnOnce(&Path)) -> Sha {
        git(
            &self.repo,
            &["checkout", "-q", "-b", name, self.base.as_str()],
        );
        edit(&self.repo);
        git(&self.repo, &["add", "-A"]);
        git(&self.repo, &["commit", "-q", "--allow-empty", "-m", name]);
        let head = Sha::parse(&git(&self.repo, &["rev-parse", "HEAD"])).unwrap();
        git(&self.repo, &["checkout", "-q", "main"]);
        head
    }

    fn diff(&self, head: &Sha) -> DiffStat {
        self.vcs
            .diff_summary(self.repo_s(), &self.base, head)
            .unwrap()
    }
}

fn paths(stat: &DiffStat) -> Vec<&str> {
    stat.changes.iter().map(|c| c.path.as_str()).collect()
}

#[test]
fn an_empty_branch_changes_nothing() {
    need_git!();
    let fx = Fx::new();
    let stat = fx.diff(&fx.base);
    assert_eq!(stat, DiffStat::default());
    let head = fx.branch("empty", |_| {});
    assert_eq!(fx.diff(&head), DiffStat::default());
}

#[test]
fn files_and_lines_are_counted_against_the_base() {
    need_git!();
    let fx = Fx::new();
    let head = fx.branch("m1", |repo| {
        // src/lib.txt was "one\ntwo\nthree\n": one line replaced, two added
        fs::write(repo.join("src/lib.txt"), "one\nTWO\nthree\nfour\nfive\n").unwrap();
        fs::write(repo.join("new.txt"), "a\nb\nc\n").unwrap();
        fs::remove_file(repo.join("README.md")).unwrap();
    });
    let stat = fx.diff(&head);
    assert_eq!(stat.files, 3);
    assert_eq!(paths(&stat), ["README.md", "new.txt", "src/lib.txt"]);
    let by = |p: &str| stat.changes.iter().find(|c| c.path == p).unwrap();
    assert_eq!(by("README.md").kind, ChangeKind::Deleted);
    assert_eq!(
        (by("README.md").insertions, by("README.md").deletions),
        (0, 1)
    );
    assert_eq!(by("new.txt").kind, ChangeKind::Added);
    assert_eq!((by("new.txt").insertions, by("new.txt").deletions), (3, 0));
    assert_eq!(by("src/lib.txt").kind, ChangeKind::Modified);
    assert_eq!(
        (by("src/lib.txt").insertions, by("src/lib.txt").deletions),
        (3, 1)
    );
    assert_eq!((stat.insertions, stat.deletions), (6, 2));
    assert_eq!(stat.size(), 8);
    assert!(!stat.truncated);
}

#[test]
fn the_numbers_are_about_the_commit_not_the_branch_name() {
    need_git!();
    let fx = Fx::new();
    let head = fx.branch("gone", |repo| fs::write(repo.join("x.txt"), "x\n").unwrap());
    // the branch is deleted (as `fleet cleanup --branches` would): the commit is still there
    git(&fx.repo, &["branch", "-D", "gone"]);
    assert_eq!(paths(&fx.diff(&head)), ["x.txt"]);
}

#[test]
fn disjoint_members_share_no_file_and_overlapping_members_share_exactly_theirs() {
    need_git!();
    let fx = Fx::new();
    let a = fx.branch("a", |repo| {
        fs::write(repo.join("src/lib.txt"), "one\ntwo\nthree\nA\n").unwrap();
        fs::write(repo.join("only-a.txt"), "a\n").unwrap();
    });
    let b = fx.branch("b", |repo| {
        fs::write(repo.join("src/lib.txt"), "B\none\ntwo\nthree\n").unwrap();
        fs::write(repo.join("only-b.txt"), "b\n").unwrap();
    });
    let c = fx.branch("c", |repo| {
        fs::write(repo.join("only-c.txt"), "c\n").unwrap()
    });
    let (da, db, dc) = (fx.diff(&a), fx.diff(&b), fx.diff(&c));
    let (ta, tb, tc) = (
        TaskId::parse("a").unwrap(),
        TaskId::parse("b").unwrap(),
        TaskId::parse("c").unwrap(),
    );
    let o = overlap(&[(&ta, &da), (&tb, &db), (&tc, &dc)], false);
    assert_eq!(o.shared_count, 1);
    assert_eq!(o.shared[0].path, "src/lib.txt");
    assert_eq!(o.shared[0].members, [ta.clone(), tb.clone()]);
    assert_eq!(o.matrix, vec![vec![2, 1, 0], vec![1, 2, 0], vec![0, 0, 1]]);
    assert!(!o.incomplete);

    let only_disjoint = overlap(&[(&ta, &da), (&tc, &dc)], false);
    assert!(only_disjoint.shared.is_empty());
}

#[test]
fn binary_files_are_listed_without_line_counts() {
    need_git!();
    let fx = Fx::new();
    let head = fx.branch("bin", |repo| {
        fs::write(
            repo.join("logo.png"),
            [0x89, b'P', b'N', b'G', 0, 0, 1, 2, 3, 0],
        )
        .unwrap();
        fs::write(repo.join("notes.txt"), "a\nb\n").unwrap();
    });
    let stat = fx.diff(&head);
    assert_eq!(stat.files, 2);
    assert_eq!(stat.binary_files, 1);
    let png = stat.changes.iter().find(|c| c.path == "logo.png").unwrap();
    assert!(png.binary);
    assert_eq!((png.insertions, png.deletions), (0, 0));
    assert_eq!(stat.size(), 2, "only the text file adds lines");
}

#[test]
fn a_rename_is_one_change_that_touches_two_paths() {
    need_git!();
    let fx = Fx::new();
    let plain = fx.branch("plain", |repo| {
        fs::rename(repo.join("src/lib.txt"), repo.join("src/renamed.txt")).unwrap();
    });
    let stat = fx.diff(&plain);
    assert_eq!(stat.files, 1);
    let c = &stat.changes[0];
    assert_eq!(c.kind, ChangeKind::Renamed);
    assert_eq!(c.path, "src/renamed.txt");
    assert_eq!(c.old_path.as_deref(), Some("src/lib.txt"));
    assert_eq!((c.insertions, c.deletions), (0, 0));
    let touched: Vec<&str> = stat.touched_paths().into_iter().collect();
    assert_eq!(touched, ["src/lib.txt", "src/renamed.txt"]);

    // a rename with an edit is still a rename, with the counts of the edit
    let edited = fx.branch("edited", |repo| {
        fs::remove_file(repo.join("src/lib.txt")).unwrap();
        fs::write(repo.join("src/moved.txt"), "one\ntwo\nthree\nfour\n").unwrap();
    });
    let stat = fx.diff(&edited);
    assert_eq!(stat.files, 1, "{stat:?}");
    let c = &stat.changes[0];
    assert_eq!(c.kind, ChangeKind::Renamed);
    assert_eq!((c.insertions, c.deletions), (1, 0));

    // a member that edited the old path overlaps with the one that renamed it
    let editor = fx.branch("editor", |repo| {
        fs::write(repo.join("src/lib.txt"), "one\ntwo\nthree\nmore\n").unwrap();
    });
    let (dp, de) = (fx.diff(&plain), fx.diff(&editor));
    let (tp, te) = (
        TaskId::parse("plain").unwrap(),
        TaskId::parse("editor").unwrap(),
    );
    let o = overlap(&[(&tp, &dp), (&te, &de)], false);
    assert_eq!(o.shared_count, 1);
    assert_eq!(o.shared[0].path, "src/lib.txt");
}

#[test]
fn a_file_replaced_by_a_symlink_is_a_type_change() {
    need_git!();
    let fx = Fx::new();
    let head = fx.branch("sym", |repo| {
        fs::remove_file(repo.join("README.md")).unwrap();
        symlink("src/lib.txt", repo.join("README.md")).unwrap();
    });
    let stat = fx.diff(&head);
    assert_eq!(stat.changes[0].kind, ChangeKind::TypeChanged);
    assert_eq!(stat.changes[0].path, "README.md");
}

#[test]
fn awkward_file_names_are_reported_exactly() {
    need_git!();
    let fx = Fx::new();
    let head = fx.branch("odd", |repo| {
        fs::write(repo.join("tab\there.txt"), "x\n").unwrap();
        fs::write(repo.join("line\nbreak.txt"), "x\n").unwrap();
        fs::write(repo.join("naïve ☃.txt"), "x\n").unwrap();
        fs::write(repo.join("esc\u{1b}[31m.txt"), "x\n").unwrap();
    });
    let stat = fx.diff(&head);
    let mut got: Vec<&str> = paths(&stat);
    got.sort_unstable();
    assert_eq!(
        got,
        [
            "esc\u{1b}[31m.txt",
            "line\nbreak.txt",
            "naïve ☃.txt",
            "tab\there.txt"
        ]
    );
    assert!(stat.changes.iter().all(|c| c.insertions == 1));
}

#[test]
fn a_long_change_is_totalled_in_full_and_listed_in_part() {
    need_git!();
    let fx = Fx::new();
    let head = fx.branch("many", |repo| {
        fs::create_dir_all(repo.join("many")).unwrap();
        for i in 0..520 {
            fs::write(repo.join(format!("many/f{i:04}.txt")), "x\n").unwrap();
        }
    });
    let stat = fx.diff(&head);
    assert_eq!(stat.files, 520);
    assert_eq!(stat.insertions, 520);
    assert_eq!(stat.changes.len(), fleet_fanout::diff::MAX_FILES_LISTED);
    assert!(stat.truncated);
}

#[test]
fn a_commit_that_does_not_exist_is_an_error() {
    need_git!();
    let fx = Fx::new();
    let ghost = Sha::parse(&"1".repeat(40)).unwrap();
    assert!(fx.vcs.diff_summary(fx.repo_s(), &fx.base, &ghost).is_err());
    assert!(fx
        .vcs
        .diff_summary(
            &fx.tmp.join("not-a-repo").display().to_string(),
            &fx.base,
            &fx.base
        )
        .is_err());
}

#[test]
fn configured_diff_programs_and_text_conversion_do_not_run() {
    need_git!();
    let fx = Fx::new();
    let marker = fx.tmp.join("ran");
    let script = fx.tmp.script(
        "hook.sh",
        &format!(
            "#!/bin/sh\n: > '{}'\ncat \"$@\"\nexit 0\n",
            marker.display()
        ),
    );
    let mut config = fs::read_to_string(fx.repo.join(".git/config")).unwrap();
    config.push_str(&format!(
        "[diff]\n\texternal = {}\n[diff \"evil\"]\n\ttextconv = {}\n",
        script.display(),
        script.display()
    ));
    fs::write(fx.repo.join(".git/config"), config).unwrap();
    let head = fx.branch("attrs", |repo| {
        fs::write(repo.join(".gitattributes"), "*.txt diff=evil\n").unwrap();
        fs::write(repo.join("a.txt"), "a\n").unwrap();
    });
    let stat = fx.diff(&head);
    assert_eq!(stat.files, 2);
    assert!(!marker.exists(), "a configured diff program ran");
}
