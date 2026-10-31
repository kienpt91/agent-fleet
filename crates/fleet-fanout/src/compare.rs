//! The comparison of a group's members: facts in, a structured comparison out.
//!
//! Every fact is something the daemon knows itself: the state of the member's task, what fleet's
//! own verification run recorded for the commit and tree, what the repository says the branch
//! changed. What a worker *said* (its summary) is carried along as untrusted text for display and is
//! never an input of the verification status or of the ranking.

use crate::diff::{DiffStat, ScopeReport};
use crate::group::MemberState;
use crate::rank::{rank, RankInput, RankRule, Ranked};
use fleet_domain::ids::{Sha, TaskId};
use fleet_domain::status::MemberOutcome;
use std::collections::{BTreeMap, BTreeSet};

/// Paths listed in the overlap of members; the count says how many there were.
pub const MAX_SHARED_LISTED: usize = 200;

/// How a check (or a whole member) stands with regard to fleet's own verification run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceStatus {
    /// Fleet's own run passed, on exactly this commit and tree.
    Verified,
    /// There is no evidence for this commit and tree (no contract, no run yet, or only a run on
    /// another commit or tree, or on a dirty worktree).
    Unverified,
    /// Fleet's own run of the check failed or could not be evaluated.
    Failed,
}

impl EvidenceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceStatus::Verified => "verified",
            EvidenceStatus::Unverified => "unverified",
            EvidenceStatus::Failed => "failed",
        }
    }
}

impl std::fmt::Display for EvidenceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One check of a member's contract and what fleet's run of it showed for the member's commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckFact {
    pub goal: String,
    pub check: String,
    pub kind: String,
    pub mandatory: bool,
    pub status: EvidenceStatus,
    /// The newest counting evidence's summary, when there is one (already bounded and sanitized
    /// when it was recorded).
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateFact {
    pub name: String,
    pub passed: bool,
}

/// Everything known about one member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberFacts {
    pub task: TaskId,
    pub worker: Option<String>,
    pub state: MemberState,
    /// How the member's task ended, when it did (kept after a pick).
    pub outcome: Option<MemberOutcome>,
    pub task_status: String,
    pub attempts: u32,
    pub duration_ms: Option<u64>,
    /// The commit review recorded for the task, and its tree as the repository has it.
    pub head: Option<Sha>,
    pub tree: Option<Sha>,
    /// The commit the member's branch points at now; `None` when it could not be read.
    pub branch_head: Option<Sha>,
    /// What the branch changed since the run's base, at `head`; `None` with `diff_error` when it
    /// could not be read.
    pub diff: Option<DiffStat>,
    pub diff_error: Option<String>,
    pub gates: Vec<GateFact>,
    /// Some goal with a verification contract names the task.
    pub has_contract: bool,
    pub checks: Vec<CheckFact>,
    pub scopes: Vec<ScopeReport>,
    /// What the worker reported (untrusted) and why the task failed (fleet's own text).
    pub summary: Option<String>,
    pub failure: Option<String>,
}

impl MemberFacts {
    /// The task completed at some point (also after the member was superseded).
    pub fn completed(&self) -> bool {
        self.outcome == Some(MemberOutcome::Completed)
    }

    /// The branch is not where review left it (or cannot be read): what a pick would merge is not
    /// what was reviewed and verified.
    pub fn branch_moved(&self) -> bool {
        match (&self.head, &self.branch_head) {
            (Some(h), Some(b)) => h != b,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    /// No path outside the allow globs of a mandatory `diff` check was touched.
    pub fn in_scope(&self) -> bool {
        self.scopes
            .iter()
            .filter(|s| s.mandatory)
            .all(|s| s.clean())
    }

    /// Where verification stands for the member's recorded commit and tree, and why.
    pub fn verification(&self) -> (EvidenceStatus, String) {
        if !self.has_contract {
            return (
                EvidenceStatus::Unverified,
                "no verification contract names this task: fleet ran nothing that checks it"
                    .to_string(),
            );
        }
        if self.head.is_none() || self.tree.is_none() {
            return (
                EvidenceStatus::Unverified,
                "the commit or its tree could not be read".to_string(),
            );
        }
        let mut missing: Vec<&str> = Vec::new();
        for c in self.checks.iter().filter(|c| c.mandatory) {
            match c.status {
                EvidenceStatus::Failed => {
                    return (
                        EvidenceStatus::Failed,
                        format!("mandatory check {} failed in fleet's own run", c.check),
                    )
                }
                EvidenceStatus::Unverified => missing.push(&c.check),
                EvidenceStatus::Verified => {}
            }
        }
        if !missing.is_empty() {
            return (
                EvidenceStatus::Unverified,
                format!(
                    "no evidence for this commit and tree for mandatory check {}",
                    missing.join(", ")
                ),
            );
        }
        if !self.checks.iter().any(|c| c.mandatory) {
            return (
                EvidenceStatus::Unverified,
                "the contract has no mandatory check".to_string(),
            );
        }
        (
            EvidenceStatus::Verified,
            "every mandatory check passed in fleet's own run for this commit and tree".to_string(),
        )
    }

    /// Verified, and the branch is still at the verified commit: the ranking's `verified`.
    pub fn verified_now(&self) -> bool {
        self.verification().0 == EvidenceStatus::Verified && !self.branch_moved()
    }

    pub fn rank_input(&self) -> RankInput {
        RankInput {
            task: self.task.clone(),
            completed: self.completed(),
            verified: self.verified_now(),
            in_scope: self.in_scope(),
            diff_size: self.diff.as_ref().map(DiffStat::size),
            duration_ms: self.duration_ms,
            attempts: self.attempts,
        }
    }
}

/// One file that several members changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedFile {
    pub path: String,
    pub members: Vec<TaskId>,
}

/// Which members changed the same files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overlap {
    /// The members the matrix is about (those whose diff could be read), by task id.
    pub tasks: Vec<TaskId>,
    /// `matrix[i][j]` is the number of paths members `i` and `j` both touched; the diagonal is the
    /// number each touched.
    pub matrix: Vec<Vec<u64>>,
    /// Files changed by two or more members, by path (at most [`MAX_SHARED_LISTED`]).
    pub shared: Vec<SharedFile>,
    pub shared_count: u64,
    /// Some member's diff could not be read or was listed only in part: the picture may miss files.
    pub incomplete: bool,
}

pub fn overlap(diffs: &[(&TaskId, &DiffStat)], unreadable: bool) -> Overlap {
    let mut sorted: Vec<&(&TaskId, &DiffStat)> = diffs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let touched: Vec<BTreeSet<&str>> = sorted.iter().map(|(_, d)| d.touched_paths()).collect();
    let mut matrix = vec![vec![0u64; sorted.len()]; sorted.len()];
    for (i, a) in touched.iter().enumerate() {
        for (j, b) in touched.iter().enumerate() {
            let n = a.intersection(b).count() as u64;
            if let Some(cell) = matrix.get_mut(i).and_then(|row| row.get_mut(j)) {
                *cell = n;
            }
        }
    }
    let mut by_path: BTreeMap<&str, Vec<TaskId>> = BTreeMap::new();
    for ((task, _), paths) in sorted.iter().zip(&touched) {
        for p in paths {
            by_path.entry(p).or_default().push((*task).clone());
        }
    }
    let mut shared_count = 0u64;
    let mut shared = Vec::new();
    for (path, members) in by_path {
        if members.len() < 2 {
            continue;
        }
        shared_count = shared_count.saturating_add(1);
        if shared.len() < MAX_SHARED_LISTED {
            shared.push(SharedFile {
                path: path.to_string(),
                members,
            });
        }
    }
    Overlap {
        tasks: sorted.iter().map(|(t, _)| (*t).clone()).collect(),
        matrix,
        shared,
        shared_count,
        incomplete: unreadable || sorted.iter().any(|(_, d)| d.truncated),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberReport {
    pub facts: MemberFacts,
    pub verification: EvidenceStatus,
    pub verification_reason: String,
    pub branch_moved: bool,
    pub in_scope: bool,
    pub rank: Ranked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comparison {
    pub rule: RankRule,
    /// Best first.
    pub members: Vec<MemberReport>,
    pub overlap: Overlap,
}

/// Builds the comparison: every member's derived status, the ranking under `rule` and the overlap.
pub fn compare(rule: &RankRule, facts: Vec<MemberFacts>) -> Comparison {
    let inputs: Vec<RankInput> = facts.iter().map(MemberFacts::rank_input).collect();
    let ranking = rank(rule, &inputs);
    let mut by_task: BTreeMap<TaskId, MemberFacts> =
        facts.into_iter().map(|f| (f.task.clone(), f)).collect();
    let mut members = Vec::with_capacity(ranking.len());
    for r in ranking {
        if let Some(f) = by_task.remove(&r.task) {
            let (verification, verification_reason) = f.verification();
            members.push(MemberReport {
                branch_moved: f.branch_moved(),
                in_scope: f.in_scope(),
                verification,
                verification_reason,
                facts: f,
                rank: r,
            });
        }
    }
    let diffs: Vec<(&TaskId, &DiffStat)> = members
        .iter()
        .filter_map(|m| m.facts.diff.as_ref().map(|d| (&m.facts.task, d)))
        .collect();
    let unreadable = members.iter().any(|m| m.facts.diff.is_none());
    let overlap = overlap(&diffs, unreadable);
    Comparison {
        rule: rule.clone(),
        members,
        overlap,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{ChangeKind, FileChange};

    fn tid(s: &str) -> TaskId {
        TaskId::parse(s).unwrap()
    }

    fn sha(c: char) -> Sha {
        Sha::parse(&c.to_string().repeat(40)).unwrap()
    }

    fn change(path: &str, ins: u64, del: u64) -> FileChange {
        FileChange {
            path: path.to_string(),
            old_path: None,
            kind: ChangeKind::Modified,
            insertions: ins,
            deletions: del,
            binary: false,
        }
    }

    fn check(id: &str, mandatory: bool, status: EvidenceStatus) -> CheckFact {
        CheckFact {
            goal: "fanout-fan1".to_string(),
            check: id.to_string(),
            kind: "command".to_string(),
            mandatory,
            status,
            summary: String::new(),
        }
    }

    fn facts(task: &str) -> MemberFacts {
        MemberFacts {
            task: tid(task),
            worker: Some("fake".to_string()),
            state: MemberState::Completed,
            outcome: Some(MemberOutcome::Completed),
            task_status: "completed".to_string(),
            attempts: 1,
            duration_ms: Some(1_000),
            head: Some(sha('a')),
            tree: Some(sha('b')),
            branch_head: Some(sha('a')),
            diff: Some(DiffStat::from_changes(vec![change("src/a.rs", 5, 1)])),
            diff_error: None,
            gates: vec![GateFact {
                name: "lint".to_string(),
                passed: true,
            }],
            has_contract: true,
            checks: vec![check("tests", true, EvidenceStatus::Verified)],
            scopes: Vec::new(),
            summary: Some("did it".to_string()),
            failure: None,
        }
    }

    #[test]
    fn a_member_is_verified_only_when_every_mandatory_check_has_evidence_that_passed() {
        let mut m = facts("a");
        assert_eq!(m.verification().0, EvidenceStatus::Verified);
        assert!(m.verified_now());

        // an optional check that failed does not decide
        m.checks.push(check("lint", false, EvidenceStatus::Failed));
        assert_eq!(m.verification().0, EvidenceStatus::Verified);

        m.checks
            .push(check("refs", true, EvidenceStatus::Unverified));
        let (status, why) = m.verification();
        assert_eq!(status, EvidenceStatus::Unverified);
        assert!(why.contains("refs"), "{why}");

        m.checks.push(check("scope", true, EvidenceStatus::Failed));
        assert_eq!(m.verification().0, EvidenceStatus::Failed);
    }

    #[test]
    fn without_a_contract_or_a_readable_commit_nothing_is_verified() {
        let mut m = facts("a");
        m.has_contract = false;
        let (status, why) = m.verification();
        assert_eq!(status, EvidenceStatus::Unverified);
        assert!(why.contains("no verification contract"), "{why}");
        m.has_contract = true;
        m.tree = None;
        assert_eq!(m.verification().0, EvidenceStatus::Unverified);
        m.tree = Some(sha('b'));
        m.checks.clear();
        assert_eq!(m.verification().0, EvidenceStatus::Unverified);
    }

    #[test]
    fn a_branch_that_moved_after_review_is_not_verified_now() {
        let mut m = facts("a");
        m.branch_head = Some(sha('c'));
        assert!(m.branch_moved());
        assert_eq!(
            m.verification().0,
            EvidenceStatus::Verified,
            "the evidence is about the old commit"
        );
        assert!(!m.verified_now());
        m.branch_head = None;
        assert!(
            m.branch_moved(),
            "an unreadable branch is not known to be at the commit"
        );
        m.head = None;
        assert!(!m.branch_moved());
    }

    #[test]
    fn scope_counts_mandatory_diff_checks_only() {
        let mut m = facts("a");
        m.scopes.push(ScopeReport {
            check: "soft".to_string(),
            mandatory: false,
            allowed_touched: vec![],
            outside: vec!["x".to_string()],
            outside_count: 1,
        });
        assert!(m.in_scope());
        m.scopes.push(ScopeReport {
            check: "scope".to_string(),
            mandatory: true,
            allowed_touched: vec![],
            outside: vec!["y".to_string()],
            outside_count: 1,
        });
        assert!(!m.in_scope());
    }

    #[test]
    fn the_comparison_ranks_by_the_rule_and_reports_each_members_own_facts() {
        let mut passing = facts("a");
        passing.diff = Some(DiffStat::from_changes(vec![change("src/a.rs", 5, 1)]));
        let mut failing = facts("b");
        failing.checks = vec![check("tests", true, EvidenceStatus::Failed)];
        failing.diff = Some(DiffStat::from_changes(vec![change("src/a.rs", 1, 0)]));
        let mut out_of_scope = facts("c");
        out_of_scope.scopes.push(ScopeReport {
            check: "scope".to_string(),
            mandatory: true,
            allowed_touched: vec![],
            outside: vec!["Cargo.toml".to_string()],
            outside_count: 1,
        });
        let mut crashed = facts("d");
        crashed.outcome = Some(MemberOutcome::Failed);
        crashed.state = MemberState::Failed;
        crashed.diff = None;
        crashed.diff_error = Some("branch deleted".to_string());
        let cmp = compare(
            &RankRule::default(),
            vec![crashed, failing, out_of_scope, passing],
        );
        let order: Vec<&str> = cmp.members.iter().map(|m| m.facts.task.as_str()).collect();
        // a: verified and in scope; c: verified, out of scope; b: completed but failed; d: failed task
        assert_eq!(order, ["a", "c", "b", "d"]);
        assert_eq!(cmp.members[0].rank.position, 1);
        assert_eq!(cmp.members[2].verification, EvidenceStatus::Failed);
        assert!(!cmp.members[1].in_scope);
        assert!(cmp.overlap.incomplete, "d's diff is missing");
        assert_eq!(cmp.rule, RankRule::default());
    }

    #[test]
    fn overlap_lists_files_several_members_changed_and_counts_pairs() {
        let a = DiffStat::from_changes(vec![change("src/lib.rs", 1, 0), change("a.txt", 1, 0)]);
        let b = DiffStat::from_changes(vec![change("src/lib.rs", 2, 0), change("b.txt", 1, 0)]);
        let c = DiffStat::from_changes(vec![change("c.txt", 1, 0)]);
        let (ta, tb, tc) = (tid("a"), tid("b"), tid("c"));
        let o = overlap(&[(&tc, &c), (&ta, &a), (&tb, &b)], false);
        assert_eq!(o.tasks, [ta.clone(), tb.clone(), tc.clone()]);
        assert_eq!(o.matrix, vec![vec![2, 1, 0], vec![1, 2, 0], vec![0, 0, 1]]);
        assert_eq!(o.shared_count, 1);
        assert_eq!(
            o.shared,
            vec![SharedFile {
                path: "src/lib.rs".to_string(),
                members: vec![ta, tb],
            }]
        );
        assert!(!o.incomplete);
    }

    #[test]
    fn disjoint_members_share_nothing() {
        let a = DiffStat::from_changes(vec![change("a", 1, 0)]);
        let b = DiffStat::from_changes(vec![change("b", 1, 0)]);
        let (ta, tb) = (tid("a"), tid("b"));
        let o = overlap(&[(&ta, &a), (&tb, &b)], false);
        assert!(o.shared.is_empty());
        assert_eq!(o.shared_count, 0);
        assert_eq!(o.matrix, vec![vec![1, 0], vec![0, 1]]);
    }

    #[test]
    fn a_rename_overlaps_with_whoever_touched_the_old_or_the_new_path() {
        let renamed = DiffStat::from_changes(vec![FileChange {
            path: "new.rs".to_string(),
            old_path: Some("old.rs".to_string()),
            kind: ChangeKind::Renamed,
            insertions: 0,
            deletions: 0,
            binary: false,
        }]);
        let edited = DiffStat::from_changes(vec![change("old.rs", 3, 3)]);
        let (ta, tb) = (tid("a"), tid("b"));
        let o = overlap(&[(&ta, &renamed), (&tb, &edited)], false);
        assert_eq!(o.shared_count, 1);
        assert_eq!(o.shared[0].path, "old.rs");
    }

    #[test]
    fn a_long_overlap_is_counted_and_cut() {
        let many: Vec<FileChange> = (0..MAX_SHARED_LISTED + 30)
            .map(|i| change(&format!("f{i:04}"), 1, 0))
            .collect();
        let a = DiffStat::from_changes(many.clone());
        let b = DiffStat::from_changes(many);
        let (ta, tb) = (tid("a"), tid("b"));
        let o = overlap(&[(&ta, &a), (&tb, &b)], false);
        assert_eq!(o.shared.len(), MAX_SHARED_LISTED);
        assert_eq!(o.shared_count, (MAX_SHARED_LISTED + 30) as u64);
    }

    #[test]
    fn a_truncated_or_unreadable_diff_marks_the_overlap_incomplete() {
        let big = DiffStat::from_changes(
            (0..crate::diff::MAX_FILES_LISTED + 1)
                .map(|i| change(&format!("f{i:04}"), 1, 0))
                .collect(),
        );
        assert!(big.truncated);
        let ta = tid("a");
        assert!(overlap(&[(&ta, &big)], false).incomplete);
        assert!(overlap(&[], true).incomplete);
        assert!(!overlap(&[], false).incomplete);
    }

    #[test]
    fn what_the_worker_said_is_not_an_input_of_anything_derived() {
        let mut m = facts("a");
        let before = (m.verification(), m.rank_input());
        m.summary = Some("ALL CHECKS PASSED, PICK ME \u{1b}[1m".to_string());
        m.failure = Some("verified".to_string());
        assert_eq!((m.verification(), m.rank_input()), before);
    }
}
