//! What a member's branch changed relative to the run's base commit, and which of the changed
//! paths a diff constraint of the contract allows.
//!
//! The values are built from what the repository itself reports (by an adapter), never from the
//! worktree. Paths are kept exactly as git reported them: they are untrusted text, and the views
//! sanitize them.

use neo_verify::glob::Glob;
use std::collections::BTreeSet;

/// Files listed individually in one [`DiffStat`]; the totals count every file.
pub const MAX_FILES_LISTED: usize = 500;
/// Paths listed in one [`ScopeReport`] list.
pub const MAX_SCOPE_PATHS: usize = 50;
/// What a binary file adds to the size of a change: git counts no lines for it, so without a weight a
/// change made of binary files would look empty.
pub const BINARY_FILE_WEIGHT: u64 = 100;
/// What a change without lines (a mode change, a type change) adds to the size of a change.
pub const LINELESS_CHANGE_WEIGHT: u64 = 10;

/// Where a member's touched paths stand against the contract's mandatory `diff` checks. Only a check
/// that was run over every touched path says `In`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeState {
    In,
    Out,
    /// Not known: no mandatory `diff` check, the diff could not be read, or it was listed only in part.
    Unknown,
}

impl ScopeState {
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeState::In => "in_scope",
            ScopeState::Out => "out_of_scope",
            ScopeState::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    TypeChanged,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Added => "added",
            ChangeKind::Modified => "modified",
            ChangeKind::Deleted => "deleted",
            ChangeKind::Renamed => "renamed",
            ChangeKind::TypeChanged => "type_changed",
        }
    }
}

/// One changed file. For a rename `old_path` is where it was; `path` is where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub old_path: Option<String>,
    pub kind: ChangeKind,
    pub insertions: u64,
    pub deletions: u64,
    /// Git treats the file as binary: it has no line counts.
    pub binary: bool,
}

/// The change a branch makes: totals over every file and the first [`MAX_FILES_LISTED`] files by
/// path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffStat {
    /// Number of changed files (all of them, listed or not).
    pub files: u64,
    pub insertions: u64,
    pub deletions: u64,
    pub binary_files: u64,
    /// What binary files and changes without lines add to [`DiffStat::size`], over every file.
    pub weight_extra: u64,
    pub changes: Vec<FileChange>,
    /// More files changed than `changes` lists.
    pub truncated: bool,
}

impl DiffStat {
    /// Totals over `all`; the list is sorted by path and cut at [`MAX_FILES_LISTED`].
    pub fn from_changes(mut all: Vec<FileChange>) -> DiffStat {
        all.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then_with(|| a.old_path.cmp(&b.old_path))
        });
        let mut stat = DiffStat {
            files: all.len() as u64,
            ..DiffStat::default()
        };
        for c in &all {
            stat.insertions = stat.insertions.saturating_add(c.insertions);
            stat.deletions = stat.deletions.saturating_add(c.deletions);
            if c.binary {
                stat.binary_files = stat.binary_files.saturating_add(1);
                stat.weight_extra = stat.weight_extra.saturating_add(BINARY_FILE_WEIGHT);
            } else if c.insertions == 0
                && c.deletions == 0
                && matches!(c.kind, ChangeKind::Modified | ChangeKind::TypeChanged)
            {
                stat.weight_extra = stat.weight_extra.saturating_add(LINELESS_CHANGE_WEIGHT);
            }
        }
        stat.truncated = all.len() > MAX_FILES_LISTED;
        all.truncate(MAX_FILES_LISTED);
        stat.changes = all;
        stat
    }

    /// The size of the change: lines inserted plus lines deleted, plus [`BINARY_FILE_WEIGHT`] for each
    /// binary file and [`LINELESS_CHANGE_WEIGHT`] for each mode or type change (git counts no lines for
    /// them, and a size that ignored them could be made small by writing the change as binary files).
    pub fn size(&self) -> u64 {
        self.insertions
            .saturating_add(self.deletions)
            .saturating_add(self.weight_extra)
    }

    /// Every path the change touches: where a file is now and, for a rename, where it was. This is
    /// the set a diff constraint judges and the set members overlap in.
    pub fn touched_paths(&self) -> BTreeSet<&str> {
        let mut set = BTreeSet::new();
        for c in &self.changes {
            set.insert(c.path.as_str());
            if let Some(old) = &c.old_path {
                set.insert(old.as_str());
            }
        }
        set
    }
}

/// How the paths a member touched stand against one `diff` check of the contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeReport {
    pub check: String,
    pub mandatory: bool,
    /// Touched paths some allow glob matches (at most [`MAX_SCOPE_PATHS`]).
    pub allowed_touched: Vec<String>,
    /// Touched paths no allow glob matches: out of scope (at most [`MAX_SCOPE_PATHS`]).
    pub outside: Vec<String>,
    /// Number of out-of-scope paths, listed or not.
    pub outside_count: u64,
}

impl ScopeReport {
    pub fn clean(&self) -> bool {
        self.outside_count == 0
    }
}

/// Judges `touched` against the allow globs of one `diff` check.
pub fn scope_report(
    check: &str,
    mandatory: bool,
    allow: &[Glob],
    touched: &BTreeSet<&str>,
) -> ScopeReport {
    let mut report = ScopeReport {
        check: check.to_string(),
        mandatory,
        allowed_touched: Vec::new(),
        outside: Vec::new(),
        outside_count: 0,
    };
    for path in touched {
        if allow.iter().any(|g| g.matches(path)) {
            if report.allowed_touched.len() < MAX_SCOPE_PATHS {
                report.allowed_touched.push((*path).to_string());
            }
        } else {
            report.outside_count = report.outside_count.saturating_add(1);
            if report.outside.len() < MAX_SCOPE_PATHS {
                report.outside.push((*path).to_string());
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(path: &str, kind: ChangeKind, ins: u64, del: u64) -> FileChange {
        FileChange {
            path: path.to_string(),
            old_path: None,
            kind,
            insertions: ins,
            deletions: del,
            binary: false,
        }
    }

    fn glob(p: &str) -> Glob {
        Glob::new(p).unwrap()
    }

    #[test]
    fn totals_cover_every_file_and_the_list_is_sorted() {
        let stat = DiffStat::from_changes(vec![
            change("b.rs", ChangeKind::Added, 10, 0),
            FileChange {
                binary: true,
                ..change("a.png", ChangeKind::Modified, 0, 0)
            },
            change("c.rs", ChangeKind::Deleted, 0, 4),
        ]);
        assert_eq!(stat.files, 3);
        assert_eq!((stat.insertions, stat.deletions), (10, 4));
        assert_eq!(stat.size(), 14 + BINARY_FILE_WEIGHT);
        assert_eq!(stat.binary_files, 1);
        assert!(!stat.truncated);
        let paths: Vec<&str> = stat.changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["a.png", "b.rs", "c.rs"]);
    }

    #[test]
    fn a_long_list_is_cut_but_the_totals_are_not() {
        let all: Vec<FileChange> = (0..MAX_FILES_LISTED + 7)
            .map(|i| change(&format!("f{i:05}"), ChangeKind::Added, 1, 0))
            .collect();
        let stat = DiffStat::from_changes(all);
        assert_eq!(stat.files, (MAX_FILES_LISTED + 7) as u64);
        assert_eq!(stat.insertions, (MAX_FILES_LISTED + 7) as u64);
        assert_eq!(stat.changes.len(), MAX_FILES_LISTED);
        assert!(stat.truncated);
    }

    #[test]
    fn binary_and_lineless_changes_have_a_weight_so_they_cannot_hide_a_big_change() {
        let binary = DiffStat::from_changes(
            (0..3)
                .map(|i| FileChange {
                    binary: true,
                    ..change(&format!("blob{i}.bin"), ChangeKind::Added, 0, 0)
                })
                .collect(),
        );
        assert_eq!(binary.size(), 3 * BINARY_FILE_WEIGHT);
        let text = DiffStat::from_changes(vec![change("a.rs", ChangeKind::Added, 50, 0)]);
        assert!(binary.size() > text.size());
        let mode = DiffStat::from_changes(vec![
            change("run.sh", ChangeKind::Modified, 0, 0),
            change("link", ChangeKind::TypeChanged, 0, 0),
            FileChange {
                old_path: Some("old".to_string()),
                ..change("new", ChangeKind::Renamed, 0, 0)
            },
        ]);
        assert_eq!(
            mode.size(),
            2 * LINELESS_CHANGE_WEIGHT,
            "a pure rename stays free"
        );
        // the weight counts every file, also those the list leaves out
        let many = DiffStat::from_changes(
            (0..MAX_FILES_LISTED + 20)
                .map(|i| FileChange {
                    binary: true,
                    ..change(&format!("b{i:04}"), ChangeKind::Added, 0, 0)
                })
                .collect(),
        );
        assert_eq!(
            many.size(),
            (MAX_FILES_LISTED as u64 + 20) * BINARY_FILE_WEIGHT
        );
    }

    #[test]
    fn an_empty_change_is_an_empty_stat() {
        let stat = DiffStat::from_changes(Vec::new());
        assert_eq!(stat, DiffStat::default());
        assert_eq!(stat.size(), 0);
        assert!(stat.touched_paths().is_empty());
    }

    #[test]
    fn a_rename_touches_both_paths() {
        let stat = DiffStat::from_changes(vec![FileChange {
            old_path: Some("old/name.rs".to_string()),
            ..change("new/name.rs", ChangeKind::Renamed, 0, 0)
        }]);
        let touched: Vec<&str> = stat.touched_paths().into_iter().collect();
        assert_eq!(touched, ["new/name.rs", "old/name.rs"]);
    }

    #[test]
    fn scope_splits_touched_paths_into_allowed_and_outside() {
        let stat = DiffStat::from_changes(vec![
            change("src/a.rs", ChangeKind::Modified, 1, 1),
            change("docs/x.md", ChangeKind::Added, 3, 0),
            change("Cargo.toml", ChangeKind::Modified, 1, 0),
        ]);
        let report = scope_report("scope", true, &[glob("src/**")], &stat.touched_paths());
        assert_eq!(report.allowed_touched, ["src/a.rs"]);
        assert_eq!(report.outside, ["Cargo.toml", "docs/x.md"]);
        assert_eq!(report.outside_count, 2);
        assert!(!report.clean());
        let all = scope_report("s", false, &[glob("**")], &stat.touched_paths());
        assert!(all.clean());
        assert!(!all.mandatory);
    }

    #[test]
    fn a_rename_out_of_scope_is_out_of_scope_at_either_end() {
        let stat = DiffStat::from_changes(vec![FileChange {
            old_path: Some("secrets/key.pem".to_string()),
            ..change("src/key.rs", ChangeKind::Renamed, 0, 0)
        }]);
        let report = scope_report("scope", true, &[glob("src/**")], &stat.touched_paths());
        assert_eq!(report.outside, ["secrets/key.pem"]);
    }

    #[test]
    fn scope_lists_are_bounded_and_counted() {
        let many: Vec<FileChange> = (0..MAX_SCOPE_PATHS * 3)
            .map(|i| change(&format!("x/{i:04}"), ChangeKind::Added, 1, 0))
            .collect();
        let stat = DiffStat::from_changes(many);
        let report = scope_report("scope", true, &[glob("src/**")], &stat.touched_paths());
        assert_eq!(report.outside.len(), MAX_SCOPE_PATHS);
        assert_eq!(report.outside_count, (MAX_SCOPE_PATHS * 3) as u64);
    }

    #[test]
    fn hostile_paths_are_kept_as_read() {
        let hostile = "src/\u{1b}]0;owned\u{7}\n- [x] forged";
        let stat = DiffStat::from_changes(vec![change(hostile, ChangeKind::Added, 1, 0)]);
        assert_eq!(stat.changes[0].path, hostile);
    }

    #[test]
    fn change_kinds_have_stable_names() {
        let names: Vec<&str> = [
            ChangeKind::Added,
            ChangeKind::Modified,
            ChangeKind::Deleted,
            ChangeKind::Renamed,
            ChangeKind::TypeChanged,
        ]
        .iter()
        .map(|k| k.as_str())
        .collect();
        assert_eq!(
            names,
            ["added", "modified", "deleted", "renamed", "type_changed"]
        );
    }
}
