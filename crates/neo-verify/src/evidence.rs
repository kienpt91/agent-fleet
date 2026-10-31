//! Evidence: what fleet itself saw when it ran one check. A value object: it has no mutating
//! method, it is created once and recorded as an event, and a later run produces a new piece
//! instead of editing an old one.

use crate::contract::{CheckId, ContractId};
use crate::judge::CheckReport;
use crate::limits::{MAX_EVIDENCE_OUTPUT, MAX_EVIDENCE_SUMMARY};
use fleet_domain::events::DomainEvent;
use fleet_domain::ids::{EvidenceId, GoalId, RunId, Sha, TaskId};
use fleet_domain::sanitize::{head_str, sanitize_text, tail_str};
use fleet_domain::status::Verdict;

/// Where a piece of evidence belongs and what it was taken from: always a clean export of one
/// commit of one task's branch, never the live worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidenceScope {
    pub goal: GoalId,
    pub run: RunId,
    pub task: TaskId,
    pub contract: ContractId,
    /// The commit that was exported and checked.
    pub commit: Sha,
    /// The tree object of that commit: what exactly was on disk for the checks.
    pub tree: Sha,
    /// The task's worktree had changes that are not in `commit` when the run started. The checks
    /// ran on the commit regardless, so the evidence says nothing about the working state: it is
    /// kept and shown, and never counts toward a contract's state.
    pub dirty: bool,
}

/// Where a contract stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContractState {
    /// At least one mandatory check has not been run yet.
    Pending,
    /// Every mandatory check's newest evidence passed.
    Passed,
    /// A mandatory check's newest evidence is a failure or an error.
    Failed,
}

impl ContractState {
    pub fn as_str(self) -> &'static str {
        match self {
            ContractState::Pending => "pending",
            ContractState::Passed => "passed",
            ContractState::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evidence {
    id: EvidenceId,
    goal: GoalId,
    run: RunId,
    task: TaskId,
    contract: ContractId,
    check: CheckId,
    verdict: Verdict,
    exit_code: Option<i64>,
    summary: String,
    output: String,
    commit: Sha,
    tree: Sha,
    dirty: bool,
    at_ms: u64,
}

fn bounded_summary(s: &str) -> String {
    head_str(sanitize_text(s.as_bytes()).trim(), MAX_EVIDENCE_SUMMARY).to_string()
}

fn bounded_output(s: &str) -> String {
    tail_str(&sanitize_text(s.as_bytes()), MAX_EVIDENCE_OUTPUT).to_string()
}

impl Evidence {
    /// Builds evidence from every field. Text is sanitized and bounded here, whatever the caller
    /// did before, so no piece of evidence can carry control characters or more than
    /// [`MAX_EVIDENCE_OUTPUT`] bytes of output.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        id: EvidenceId,
        goal: GoalId,
        run: RunId,
        task: TaskId,
        contract: ContractId,
        check: CheckId,
        verdict: Verdict,
        exit_code: Option<i64>,
        summary: String,
        output: String,
        commit: Sha,
        tree: Sha,
        dirty: bool,
        at_ms: u64,
    ) -> Evidence {
        Evidence {
            id,
            goal,
            run,
            task,
            contract,
            check,
            verdict,
            exit_code,
            summary: bounded_summary(&summary),
            output: bounded_output(&output),
            commit,
            tree,
            dirty,
            at_ms,
        }
    }

    /// Evidence for the result of one check; `at_ms` comes from the caller's clock.
    pub fn record(
        id: EvidenceId,
        scope: &EvidenceScope,
        report: CheckReport,
        at_ms: u64,
    ) -> Evidence {
        Evidence::from_parts(
            id,
            scope.goal.clone(),
            scope.run.clone(),
            scope.task.clone(),
            scope.contract.clone(),
            report.check,
            report.verdict,
            report.exit_code,
            report.summary,
            report.output,
            scope.commit.clone(),
            scope.tree.clone(),
            scope.dirty,
            at_ms,
        )
    }

    pub fn id(&self) -> &EvidenceId {
        &self.id
    }
    pub fn goal(&self) -> &GoalId {
        &self.goal
    }
    pub fn run(&self) -> &RunId {
        &self.run
    }
    pub fn task(&self) -> &TaskId {
        &self.task
    }
    pub fn contract(&self) -> &ContractId {
        &self.contract
    }
    pub fn check(&self) -> &CheckId {
        &self.check
    }
    pub fn verdict(&self) -> Verdict {
        self.verdict
    }
    pub fn passed(&self) -> bool {
        self.verdict == Verdict::Passed
    }
    pub fn exit_code(&self) -> Option<i64> {
        self.exit_code
    }
    pub fn summary(&self) -> &str {
        &self.summary
    }
    pub fn output(&self) -> &str {
        &self.output
    }
    pub fn commit(&self) -> &Sha {
        &self.commit
    }
    pub fn tree(&self) -> &Sha {
        &self.tree
    }
    /// True when the worktree had uncommitted changes while this was taken.
    pub fn dirty(&self) -> bool {
        self.dirty
    }
    /// True when this piece may count toward a contract's state: its checks ran on a clean
    /// export of a commit and nothing about the working state is hidden behind it.
    pub fn counts(&self) -> bool {
        !self.dirty
    }
    pub fn at_ms(&self) -> u64 {
        self.at_ms
    }

    pub fn to_event(&self) -> DomainEvent {
        DomainEvent::EvidenceRecorded {
            id: self.id.clone(),
            goal: self.goal.clone(),
            run: self.run.clone(),
            task: self.task.clone(),
            contract_id: self.contract.to_string(),
            check: self.check.to_string(),
            verdict: self.verdict,
            exit_code: self.exit_code,
            summary: self.summary.clone(),
            output: self.output.clone(),
            commit: self.commit.clone(),
            tree: self.tree.clone(),
            dirty: self.dirty,
            at_ms: self.at_ms,
        }
    }

    /// The evidence an `EvidenceRecorded` event records; `None` for any other event or for one
    /// whose contract or check id is malformed.
    pub fn from_event(event: &DomainEvent) -> Option<Evidence> {
        let DomainEvent::EvidenceRecorded {
            id,
            goal,
            run,
            task,
            contract_id,
            check,
            verdict,
            exit_code,
            summary,
            output,
            commit,
            tree,
            dirty,
            at_ms,
        } = event
        else {
            return None;
        };
        Some(Evidence::from_parts(
            id.clone(),
            goal.clone(),
            run.clone(),
            task.clone(),
            ContractId::parse(contract_id).ok()?,
            CheckId::parse(check).ok()?,
            *verdict,
            *exit_code,
            summary.clone(),
            output.clone(),
            commit.clone(),
            tree.clone(),
            *dirty,
            *at_ms,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> EvidenceScope {
        EvidenceScope {
            goal: GoalId::parse("g").unwrap(),
            run: RunId::parse("r1").unwrap(),
            task: TaskId::parse("t1").unwrap(),
            contract: ContractId::parse(&"a".repeat(64)).unwrap(),
            commit: Sha::parse(&"b".repeat(40)).unwrap(),
            tree: Sha::parse(&"c".repeat(40)).unwrap(),
            dirty: false,
        }
    }

    fn report(output: &str) -> CheckReport {
        CheckReport {
            check: CheckId::parse("tests").unwrap(),
            verdict: Verdict::Failed,
            exit_code: Some(2),
            summary: "exit code 2, expected 0".into(),
            output: output.into(),
        }
    }

    #[test]
    fn record_keeps_scope_report_and_clock() {
        let e = Evidence::record(EvidenceId::from_seq(4), &scope(), report("boom"), 1234);
        assert_eq!(e.id().as_str(), "e4");
        assert_eq!(e.goal().as_str(), "g");
        assert_eq!(e.run().as_str(), "r1");
        assert_eq!(e.task().as_str(), "t1");
        assert_eq!(e.contract().as_str(), "a".repeat(64));
        assert_eq!(e.check().as_str(), "tests");
        assert_eq!(e.verdict(), Verdict::Failed);
        assert!(!e.passed());
        assert_eq!(e.exit_code(), Some(2));
        assert_eq!(e.summary(), "exit code 2, expected 0");
        assert_eq!(e.output(), "boom");
        assert_eq!(e.commit().as_str(), "b".repeat(40));
        assert_eq!(e.tree().as_str(), "c".repeat(40));
        assert!(!e.dirty() && e.counts());
        assert_eq!(e.at_ms(), 1234);
    }

    #[test]
    fn output_is_sanitized_and_bounded_to_its_tail() {
        let big = format!("{}END\u{1b}[31m", "x".repeat(MAX_EVIDENCE_OUTPUT * 3));
        let e = Evidence::record(EvidenceId::from_seq(1), &scope(), report(&big), 1);
        assert!(e.output().len() <= MAX_EVIDENCE_OUTPUT);
        assert!(e.output().ends_with("END"), "{:?}", e.output());
        assert!(!e.output().contains('\u{1b}'));
        let long_summary = CheckReport {
            summary: "s".repeat(5000),
            ..report("")
        };
        let e = Evidence::record(EvidenceId::from_seq(1), &scope(), long_summary, 1);
        assert!(e.summary().len() <= MAX_EVIDENCE_SUMMARY);
    }

    #[test]
    fn events_round_trip_and_other_events_are_not_evidence() {
        let e = Evidence::record(EvidenceId::from_seq(9), &scope(), report("o"), 7);
        let event = e.to_event();
        assert_eq!(Evidence::from_event(&event), Some(e));
        assert_eq!(
            Evidence::from_event(&DomainEvent::Warning {
                message: "w".into()
            }),
            None
        );
        let DomainEvent::EvidenceRecorded { .. } = &event else {
            panic!("wrong event")
        };
        // a damaged contract or check id in the log is not evidence
        for tamper in 0..2 {
            let mut damaged = event.clone();
            if let DomainEvent::EvidenceRecorded {
                contract_id, check, ..
            } = &mut damaged
            {
                if tamper == 0 {
                    *contract_id = "short".into();
                } else {
                    *check = "Not Valid".into();
                }
            }
            assert_eq!(Evidence::from_event(&damaged), None);
        }
    }

    #[test]
    fn contract_state_names() {
        assert_eq!(ContractState::Pending.as_str(), "pending");
        assert_eq!(ContractState::Passed.as_str(), "passed");
        assert_eq!(ContractState::Failed.as_str(), "failed");
    }
}
