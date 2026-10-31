//! The goal ledger aggregate: a forest of goals, the evidence recorded for them and the rules
//! that connect the two.
//!
//! Rules enforced here:
//! - the ledger is a tree by construction: a goal's parent must already exist when it is added and
//!   never changes, so no cycle can be formed; depth, fan-out, goal and evidence counts are bounded;
//! - `verified` is derived, never set: a goal is verified exactly when its own contract passes (or,
//!   without a contract, when it has children) and every child that is not abandoned is verified.
//!   After every command the ledger settles to that fixed point, in both directions: new evidence
//!   can verify a goal and its ancestors, and a regression, a new child or a new contract can take
//!   the verification away again;
//! - evidence is immutable and only ever added.

use crate::goal::{AttachedContract, Goal, NewGoal, TaskRef};
use crate::limits::{
    MAX_CHILDREN, MAX_DEPTH, MAX_EVIDENCE, MAX_GOALS, MAX_INTENT, MAX_REASON, MAX_TASKS_PER_GOAL,
};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::events::DomainEvent;
use fleet_domain::ids::{show, validate_line, validate_text, EvidenceId, GoalId, RunId, TaskId};
use fleet_domain::limits::MAX_TITLE;
use fleet_domain::sanitize::flatten_line;
use fleet_domain::status::GoalStatus;
use fleet_domain::value::Document;
use neo_verify::contract::{ContractId, VerificationContract};
use neo_verify::evidence::{ContractState, Evidence, EvidenceScope};
use neo_verify::judge::CheckReport;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// What is left to do, derived from the ledger alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remaining<'a> {
    /// Goals that are not done and not waiting on a child: nothing but themselves stands between
    /// them and `verified`. Blocked goals are listed under `blocked` instead.
    pub open_leaves: Vec<&'a Goal>,
    /// Goals whose status is `blocked`.
    pub blocked: Vec<&'a Goal>,
}

/// What the ledger says about the verification of one task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskContract {
    /// No goal that names the task has a contract: nothing but the quality gates stands guard.
    NotGoverned,
    /// Some contract of the task has mandatory checks without evidence, none has a failure.
    Pending,
    /// A mandatory check of one of the task's contracts failed or errored.
    Failed,
    /// Every contract of the task passed.
    Passed,
}

#[derive(Clone, Debug, Default)]
pub struct Ledger {
    goals: BTreeMap<GoalId, Goal>,
    /// Goal ids in the order they were added: a parent always comes before its children.
    order: Vec<GoalId>,
    /// Shared, so that settling on a copy of the ledger does not copy every piece of evidence.
    evidence: BTreeMap<EvidenceId, Arc<Evidence>>,
    evidence_seq: u64,
}

fn manual_reason(reason: &str, default: &str) -> Result<String> {
    if reason.trim().is_empty() {
        return Ok(default.to_string());
    }
    validate_line("reason", reason, MAX_REASON)?;
    Ok(reason.trim().to_string())
}

impl Ledger {
    pub fn new() -> Ledger {
        Ledger::default()
    }

    // ---- queries -------------------------------------------------------------------------

    pub fn len(&self) -> usize {
        self.goals.len()
    }

    pub fn is_empty(&self) -> bool {
        self.goals.is_empty()
    }

    pub fn find(&self, id: &GoalId) -> Option<&Goal> {
        self.goals.get(id)
    }

    pub fn goal(&self, id: &GoalId) -> Result<&Goal> {
        self.goals.get(id).ok_or_else(|| {
            FleetError::not_found(format!("goal {} does not exist", show(id.as_str())))
        })
    }

    /// Every goal, parents before children, in the order they were added.
    pub fn goals(&self) -> impl Iterator<Item = &Goal> {
        self.order.iter().filter_map(|id| self.goals.get(id))
    }

    pub fn roots(&self) -> impl Iterator<Item = &Goal> {
        self.goals().filter(|g| g.parent.is_none())
    }

    pub fn children_of<'a>(&'a self, goal: &'a Goal) -> impl Iterator<Item = &'a Goal> {
        goal.children.iter().filter_map(|c| self.goals.get(c))
    }

    pub fn evidence(&self, id: &EvidenceId) -> Option<&Evidence> {
        self.evidence.get(id).map(Arc::as_ref)
    }

    /// Evidence of one goal, oldest first.
    pub fn evidence_of(&self, goal: &GoalId) -> Vec<&Evidence> {
        self.goals
            .get(goal)
            .map(|g| {
                g.evidence
                    .iter()
                    .filter_map(|e| self.evidence.get(e).map(Arc::as_ref))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn evidence_count(&self) -> usize {
        self.evidence.len()
    }

    /// Where the goal's own contract stands; `None` when it has none.
    pub fn contract_state(&self, goal: &GoalId) -> Option<ContractState> {
        let g = self.goals.get(goal)?;
        let c = g.contract.as_ref()?;
        Some(c.contract.state(&c.id, &self.evidence_of(goal)))
    }

    /// Goals that have not been abandoned and name `run/task` as one of their tasks.
    pub fn owners_of(&self, run: &RunId, task: &TaskId) -> Vec<&Goal> {
        let wanted = TaskRef::new(run.clone(), task.clone());
        self.goals()
            .filter(|g| g.status != GoalStatus::Abandoned && g.tasks.contains(&wanted))
            .collect()
    }

    /// The goals whose contracts must pass before `run/task` may complete.
    pub fn governing(&self, run: &RunId, task: &TaskId) -> Vec<&Goal> {
        self.owners_of(run, task)
            .into_iter()
            .filter(|g| g.contract.is_some())
            .collect()
    }

    pub fn task_contract(&self, run: &RunId, task: &TaskId) -> TaskContract {
        let governing = self.governing(run, task);
        if governing.is_empty() {
            return TaskContract::NotGoverned;
        }
        let mut pending = false;
        for g in governing {
            match self.contract_state(&g.id) {
                Some(ContractState::Failed) => return TaskContract::Failed,
                Some(ContractState::Pending) | None => pending = true,
                Some(ContractState::Passed) => {}
            }
        }
        if pending {
            TaskContract::Pending
        } else {
            TaskContract::Passed
        }
    }

    pub fn remaining(&self) -> Remaining<'_> {
        let mut open_leaves = Vec::new();
        let mut blocked = Vec::new();
        for g in self.goals() {
            match g.status {
                GoalStatus::Blocked => blocked.push(g),
                GoalStatus::Open | GoalStatus::InProgress if self.children_done(g) => {
                    open_leaves.push(g)
                }
                _ => {}
            }
        }
        Remaining {
            open_leaves,
            blocked,
        }
    }

    /// True when no child is left that is neither verified nor abandoned.
    fn children_done(&self, g: &Goal) -> bool {
        self.children_of(g)
            .all(|c| matches!(c.status, GoalStatus::Verified | GoalStatus::Abandoned))
    }

    fn next_goal_id(&self) -> GoalId {
        let mut n = (self.goals.len() as u64).saturating_add(1);
        loop {
            let id = GoalId::from_seq(n);
            if !self.goals.contains_key(&id) {
                return id;
            }
            n = n.saturating_add(1);
        }
    }

    /// True when the goal's own criteria and all its live children are satisfied.
    fn satisfied(&self, g: &Goal) -> bool {
        let live: Vec<&Goal> = self
            .children_of(g)
            .filter(|c| c.status != GoalStatus::Abandoned)
            .collect();
        if !live.iter().all(|c| c.status == GoalStatus::Verified) {
            return false;
        }
        match &g.contract {
            Some(c) => c.contract.state(&c.id, &self.evidence_of(&g.id)) == ContractState::Passed,
            None => !live.is_empty(),
        }
    }

    // ---- commands ------------------------------------------------------------------------

    /// Adds a goal (and its contract, when it has one).
    pub fn add_goal(&self, new: NewGoal) -> Result<(GoalId, Vec<DomainEvent>)> {
        validate_line("goal title", &new.title, MAX_TITLE)?;
        validate_text("goal intent", &new.intent, MAX_INTENT)?;
        if new.tasks.len() > MAX_TASKS_PER_GOAL {
            return Err(FleetError::limit(format!(
                "a goal names at most {MAX_TASKS_PER_GOAL} tasks"
            )));
        }
        let distinct: BTreeSet<&TaskRef> = new.tasks.iter().collect();
        if distinct.len() != new.tasks.len() {
            return Err(FleetError::invalid("a goal names the same task twice"));
        }
        if self.goals.len() >= MAX_GOALS {
            return Err(FleetError::limit(format!(
                "the ledger holds {MAX_GOALS} goals already"
            )));
        }
        let id = match &new.id {
            Some(id) if self.goals.contains_key(id) => {
                return Err(FleetError::conflict(format!(
                    "goal {} exists already",
                    show(id.as_str())
                )))
            }
            Some(id) => id.clone(),
            None => self.next_goal_id(),
        };
        if let Some(parent) = &new.parent {
            let p = self.goal(parent)?;
            if p.status == GoalStatus::Abandoned {
                return Err(FleetError::conflict(format!(
                    "goal {} is abandoned; nothing can be added under it",
                    show(parent.as_str())
                )));
            }
            if p.depth >= MAX_DEPTH {
                return Err(FleetError::limit(format!(
                    "goals are nested at most {MAX_DEPTH} levels deep"
                )));
            }
            if p.children.len() >= MAX_CHILDREN {
                return Err(FleetError::limit(format!(
                    "a goal has at most {MAX_CHILDREN} children"
                )));
            }
        }
        let mut events = vec![DomainEvent::GoalAdded {
            goal: id.clone(),
            parent: new.parent.clone(),
            title: new.title.trim().to_string(),
            intent: new.intent.clone(),
            tasks: new.tasks.iter().map(TaskRef::to_string).collect(),
        }];
        if let Some(c) = &new.contract {
            events.push(criteria_event(&id, c));
        }
        Ok((id, self.settled(events)))
    }

    /// Makes `contract` the acceptance criteria of a goal, replacing an earlier one.
    pub fn attach_criteria(
        &self,
        goal: &GoalId,
        contract: &AttachedContract,
    ) -> Result<Vec<DomainEvent>> {
        let g = self.goal(goal)?;
        if g.status == GoalStatus::Abandoned {
            return Err(FleetError::conflict(format!(
                "goal {} is abandoned",
                show(goal.as_str())
            )));
        }
        if g.contract.as_ref().is_some_and(|c| c.id == contract.id) {
            return Err(FleetError::conflict(format!(
                "goal {} has this contract already",
                show(goal.as_str())
            )));
        }
        Ok(self.settled(vec![criteria_event(goal, contract)]))
    }

    /// An operator changes the status of a goal. `verified` cannot be set this way, and a verified
    /// goal can only be abandoned: verification is evidence's to give and to take away.
    pub fn set_status(
        &self,
        goal: &GoalId,
        to: GoalStatus,
        reason: &str,
    ) -> Result<Vec<DomainEvent>> {
        let g = self.goal(goal)?;
        if to == GoalStatus::Verified {
            return Err(FleetError::forbidden(
                "a goal becomes verified only through recorded evidence, never by a status change",
            ));
        }
        if g.status == to {
            return Err(FleetError::conflict(format!(
                "goal {} is {to} already",
                show(goal.as_str())
            )));
        }
        if g.status == GoalStatus::Verified && to != GoalStatus::Abandoned {
            return Err(FleetError::conflict(format!(
                "goal {} is verified; only new evidence can change that (or abandon it)",
                show(goal.as_str())
            )));
        }
        let reason = manual_reason(reason, "set by an operator")?;
        Ok(self.settled(vec![DomainEvent::GoalStatusChanged {
            goal: goal.clone(),
            from: g.status,
            to,
            reason,
        }]))
    }

    /// Records the reports of one run of a goal's contract as evidence and settles the ledger:
    /// the returned events are the evidence followed by every status change it causes.
    pub fn record_evidence(
        &self,
        scope: &EvidenceScope,
        reports: Vec<CheckReport>,
        at_ms: u64,
    ) -> Result<Vec<DomainEvent>> {
        let g = self.goal(&scope.goal)?;
        let Some(attached) = &g.contract else {
            return Err(FleetError::conflict(format!(
                "goal {} has no contract",
                show(scope.goal.as_str())
            )));
        };
        if attached.id != scope.contract {
            return Err(FleetError::conflict(format!(
                "the contract of goal {} changed while it was being checked; the result was dropped",
                show(scope.goal.as_str())
            )));
        }
        if g.status == GoalStatus::Abandoned {
            return Err(FleetError::conflict(format!(
                "goal {} is abandoned",
                show(scope.goal.as_str())
            )));
        }
        if reports.is_empty() {
            return Err(FleetError::invalid("there is no check result to record"));
        }
        if reports
            .iter()
            .any(|r| attached.contract.check(&r.check).is_none())
        {
            return Err(FleetError::invalid(
                "a check result names a check the contract does not have",
            ));
        }
        if self.evidence.len().saturating_add(reports.len()) > MAX_EVIDENCE {
            return Err(FleetError::limit(format!(
                "the ledger holds {MAX_EVIDENCE} pieces of evidence already"
            )));
        }
        let mut seq = self.evidence_seq;
        let events = reports
            .into_iter()
            .map(|report| {
                seq = seq.saturating_add(1);
                Evidence::record(EvidenceId::from_seq(seq), scope, report, at_ms).to_event()
            })
            .collect();
        Ok(self.settled(events))
    }

    // ---- settling ------------------------------------------------------------------------

    /// `events` followed by the status changes they cause.
    fn settled(&self, mut events: Vec<DomainEvent>) -> Vec<DomainEvent> {
        let mut sim = self.clone();
        for e in &events {
            sim.apply(e);
        }
        events.extend(sim.settle());
        events
    }

    /// Moves every goal to the status its evidence and children call for, children first, until
    /// nothing changes. Applies the changes it returns.
    fn settle(&mut self) -> Vec<DomainEvent> {
        let mut out = Vec::new();
        for _ in 0..MAX_DEPTH.saturating_add(2) {
            let mut changed = false;
            let ids: Vec<GoalId> = self.order.iter().rev().cloned().collect();
            for id in ids {
                let Some(g) = self.goals.get(&id) else {
                    continue;
                };
                if g.status == GoalStatus::Abandoned {
                    continue;
                }
                let satisfied = self.satisfied(g);
                let verified = g.status == GoalStatus::Verified;
                let (to, reason) = if satisfied && !verified {
                    (
                        GoalStatus::Verified,
                        if g.contract.is_some() {
                            "every mandatory check of the contract passed"
                        } else {
                            "every child is verified"
                        },
                    )
                } else if !satisfied && verified {
                    (
                        GoalStatus::InProgress,
                        "the evidence or the children no longer support verified",
                    )
                } else {
                    continue;
                };
                let event = DomainEvent::GoalStatusChanged {
                    goal: id,
                    from: g.status,
                    to,
                    reason: reason.to_string(),
                };
                self.apply(&event);
                out.push(event);
                changed = true;
            }
            if !changed {
                break;
            }
        }
        out
    }

    // ---- replay ----------------------------------------------------------------------------

    /// Folds one event into the ledger. Returns false when the event does not belong to the
    /// ledger or does not fit it (an unknown parent, a repeated id, a status change from the wrong
    /// status, a malformed contract): such an event changes nothing, so a damaged or reordered
    /// history cannot make the ledger inconsistent.
    pub fn apply(&mut self, event: &DomainEvent) -> bool {
        match event {
            DomainEvent::GoalAdded {
                goal,
                parent,
                title,
                intent,
                tasks,
            } => self.apply_added(goal, parent.as_ref(), title, intent, tasks),
            DomainEvent::GoalStatusChanged {
                goal,
                from,
                to,
                reason,
            } => match self.goals.get_mut(goal) {
                Some(g) if g.status == *from => {
                    g.status = *to;
                    g.reason = flatten_line(reason, MAX_REASON);
                    true
                }
                _ => false,
            },
            DomainEvent::CriteriaAttached {
                goal,
                contract_id,
                contract,
            } => self.apply_criteria(goal, contract_id, contract),
            DomainEvent::EvidenceRecorded { goal, .. } => {
                let Some(e) = Evidence::from_event(event) else {
                    return false;
                };
                let fits = self
                    .goals
                    .get(goal)
                    .is_some_and(|g| g.contract.as_ref().is_some_and(|c| c.id == *e.contract()));
                if !fits
                    || self.evidence.contains_key(e.id())
                    || self.evidence.len() >= MAX_EVIDENCE
                {
                    return false;
                }
                self.evidence_seq = self.evidence_seq.max(e.id().seq());
                if let Some(g) = self.goals.get_mut(goal) {
                    g.evidence.push(e.id().clone());
                }
                self.evidence.insert(e.id().clone(), Arc::new(e));
                true
            }
            _ => false,
        }
    }

    fn apply_added(
        &mut self,
        id: &GoalId,
        parent: Option<&GoalId>,
        title: &str,
        intent: &str,
        tasks: &[String],
    ) -> bool {
        if self.goals.contains_key(id) || self.goals.len() >= MAX_GOALS {
            return false;
        }
        let depth = match parent {
            None => 1,
            Some(p) => match self.goals.get(p) {
                Some(pg)
                    if pg.status != GoalStatus::Abandoned
                        && pg.depth < MAX_DEPTH
                        && pg.children.len() < MAX_CHILDREN =>
                {
                    pg.depth.saturating_add(1)
                }
                _ => return false,
            },
        };
        let Ok(tasks) = tasks
            .iter()
            .map(|t| TaskRef::parse(t))
            .collect::<Result<Vec<_>>>()
        else {
            return false;
        };
        if let Some(pg) = parent.and_then(|p| self.goals.get_mut(p)) {
            pg.children.push(id.clone());
        }
        self.goals.insert(
            id.clone(),
            Goal {
                id: id.clone(),
                parent: parent.cloned(),
                title: flatten_line(title, MAX_TITLE),
                intent: intent.to_string(),
                status: GoalStatus::Open,
                reason: "added".to_string(),
                tasks,
                children: Vec::new(),
                depth,
                contract: None,
                evidence: Vec::new(),
            },
        );
        self.order.push(id.clone());
        true
    }

    fn apply_criteria(&mut self, goal: &GoalId, contract_id: &str, document: &Document) -> bool {
        let Ok(id) = ContractId::parse(contract_id) else {
            return false;
        };
        let Ok(contract) = VerificationContract::from_value(&document.0) else {
            return false;
        };
        match self.goals.get_mut(goal) {
            Some(g) if g.status != GoalStatus::Abandoned => {
                g.contract = Some(AttachedContract { id, contract });
                true
            }
            _ => false,
        }
    }
}

fn criteria_event(goal: &GoalId, c: &AttachedContract) -> DomainEvent {
    DomainEvent::CriteriaAttached {
        goal: goal.clone(),
        contract_id: c.id.to_string(),
        contract: Document(c.contract.to_value()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_domain::approval::Digest;
    use fleet_domain::error::ErrorCode;
    use fleet_domain::status::Verdict;
    use fleet_domain::value::Value;
    use neo_verify::contract::CheckId;

    /// Not a cryptographic hash; distinct texts give distinct ids in these tests.
    struct Fnv;
    impl Digest for Fnv {
        fn sha256_hex(&self, data: &[u8]) -> String {
            let mut h: u64 = 0xcbf29ce484222325;
            for b in data {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x100000001b3);
            }
            format!("{h:016x}").repeat(4)
        }
    }

    fn gid(s: &str) -> GoalId {
        GoalId::parse(s).unwrap()
    }

    /// A contract of file-exists checks: `(id, mandatory)`.
    fn contract(checks: &[(&str, bool)]) -> AttachedContract {
        let doc = Value::object([
            ("version", Value::int(1)),
            (
                "checks",
                Value::array(checks.iter().map(|(id, mandatory)| {
                    Value::object([
                        ("id", Value::str(*id)),
                        ("kind", Value::str("file")),
                        ("mandatory", Value::Bool(*mandatory)),
                        ("path", Value::str("a.txt")),
                        ("assert", Value::str("exists")),
                    ])
                })),
            ),
        ]);
        let contract = VerificationContract::from_value(&doc).unwrap();
        AttachedContract {
            id: contract.id(&Fnv),
            contract,
        }
    }

    fn new_goal(id: &str, parent: Option<&str>, c: Option<AttachedContract>) -> NewGoal {
        NewGoal {
            id: Some(gid(id)),
            parent: parent.map(gid),
            title: format!("goal {id}"),
            intent: String::new(),
            tasks: vec![],
            contract: c,
        }
    }

    /// Runs a command and folds its events in, as the application does.
    fn commit(ledger: &mut Ledger, events: Vec<DomainEvent>) -> Vec<DomainEvent> {
        for e in &events {
            assert!(ledger.apply(e), "{e:?} was not applied");
        }
        events
    }

    fn add(ledger: &mut Ledger, g: NewGoal) -> Vec<DomainEvent> {
        let (_, events) = ledger.add_goal(g).unwrap();
        commit(ledger, events)
    }

    fn scope(goal: &str, c: &AttachedContract) -> EvidenceScope {
        EvidenceScope {
            goal: gid(goal),
            run: None,
            task: None,
            contract: c.id.clone(),
            commit: None,
        }
    }

    fn report(check: &str, verdict: Verdict) -> CheckReport {
        CheckReport {
            check: CheckId::parse(check).unwrap(),
            verdict,
            exit_code: None,
            summary: format!("{check} {verdict}"),
            output: String::new(),
        }
    }

    fn evidence(
        ledger: &mut Ledger,
        goal: &str,
        c: &AttachedContract,
        reports: Vec<(&str, Verdict)>,
    ) -> Vec<DomainEvent> {
        let reports = reports.into_iter().map(|(c, v)| report(c, v)).collect();
        let events = ledger.record_evidence(&scope(goal, c), reports, 5).unwrap();
        commit(ledger, events)
    }

    fn status(ledger: &Ledger, id: &str) -> GoalStatus {
        ledger.goal(&gid(id)).unwrap().status
    }

    fn kinds(events: &[DomainEvent]) -> Vec<&'static str> {
        events.iter().map(DomainEvent::kind).collect()
    }

    #[test]
    fn goals_get_ids_titles_and_a_tree_position() {
        let mut l = Ledger::new();
        assert!(l.is_empty());
        let (id, events) = l
            .add_goal(NewGoal {
                id: None,
                parent: None,
                title: "  Ship it ".into(),
                intent: "line one\nline two".into(),
                tasks: vec![TaskRef::parse("r1/t1").unwrap()],
                contract: None,
            })
            .unwrap();
        assert_eq!(id.as_str(), "goal-1");
        assert_eq!(kinds(&events), ["goal_added"]);
        commit(&mut l, events);
        let g = l.goal(&id).unwrap();
        assert_eq!(
            (g.title.as_str(), g.depth, g.status),
            ("Ship it", 1, GoalStatus::Open)
        );
        assert_eq!(g.tasks[0].to_string(), "r1/t1");
        add(&mut l, new_goal("child", Some("goal-1"), None));
        assert_eq!(l.goal(&gid("child")).unwrap().depth, 2);
        assert_eq!(l.goal(&gid("goal-1")).unwrap().children, vec![gid("child")]);
        assert_eq!(l.len(), 2);
        assert_eq!(l.roots().count(), 1);
        assert_eq!(l.children_of(l.goal(&gid("goal-1")).unwrap()).count(), 1);
        // the next automatic id skips ids taken by hand
        add(&mut l, new_goal("goal-3", None, None));
        let (next, _) = l
            .add_goal(NewGoal {
                id: None,
                ..new_goal("x", None, None)
            })
            .unwrap();
        assert_eq!(next.as_str(), "goal-4");
    }

    #[test]
    fn adding_a_goal_validates_everything() {
        let mut l = Ledger::new();
        add(&mut l, new_goal("root", None, None));
        let code = |g: NewGoal| l.add_goal(g).unwrap_err().code;
        assert_eq!(code(new_goal("root", None, None)), ErrorCode::Conflict);
        assert_eq!(code(new_goal("x", Some("nope"), None)), ErrorCode::NotFound);
        assert_eq!(
            code(NewGoal {
                title: "".into(),
                ..new_goal("x", None, None)
            }),
            ErrorCode::Invalid
        );
        assert_eq!(
            code(NewGoal {
                title: "a\u{1b}b".into(),
                ..new_goal("x", None, None)
            }),
            ErrorCode::Invalid
        );
        assert_eq!(
            code(NewGoal {
                intent: "x".repeat(MAX_INTENT + 1),
                ..new_goal("x", None, None)
            }),
            ErrorCode::Invalid
        );
        let t = TaskRef::parse("r1/t1").unwrap();
        assert_eq!(
            code(NewGoal {
                tasks: vec![t.clone(), t.clone()],
                ..new_goal("x", None, None)
            }),
            ErrorCode::Invalid
        );
        let many: Vec<TaskRef> = (0..=MAX_TASKS_PER_GOAL)
            .map(|i| TaskRef::parse(&format!("r1/t{i}")).unwrap())
            .collect();
        assert_eq!(
            code(NewGoal {
                tasks: many,
                ..new_goal("x", None, None)
            }),
            ErrorCode::Limit
        );
    }

    #[test]
    fn depth_fan_out_and_size_are_bounded() {
        let mut l = Ledger::new();
        add(&mut l, new_goal("d1", None, None));
        for depth in 2..=MAX_DEPTH {
            add(
                &mut l,
                new_goal(&format!("d{depth}"), Some(&format!("d{}", depth - 1)), None),
            );
        }
        let err = l
            .add_goal(new_goal("too-deep", Some(&format!("d{MAX_DEPTH}")), None))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Limit);
        for i in 0..MAX_CHILDREN - 1 {
            add(&mut l, new_goal(&format!("c{i}"), Some("d1"), None));
        }
        assert_eq!(
            l.add_goal(new_goal("one-more", Some("d1"), None))
                .unwrap_err()
                .code,
            ErrorCode::Limit
        );
        let mut big = Ledger::new();
        for i in 0..MAX_GOALS {
            add(&mut big, new_goal(&format!("g{i}"), None, None));
        }
        assert_eq!(
            big.add_goal(new_goal("overflow", None, None))
                .unwrap_err()
                .code,
            ErrorCode::Limit
        );
    }

    #[test]
    fn evidence_verifies_a_leaf_and_a_later_failure_takes_it_back() {
        let mut l = Ledger::new();
        let c = contract(&[("tests", true), ("lint", false)]);
        let events = add(&mut l, new_goal("leaf", None, Some(c.clone())));
        assert_eq!(kinds(&events), ["goal_added", "criteria_attached"]);
        assert_eq!(l.contract_state(&gid("leaf")), Some(ContractState::Pending));

        // a failure does not verify
        let ev = evidence(&mut l, "leaf", &c, vec![("tests", Verdict::Failed)]);
        assert_eq!(kinds(&ev), ["evidence_recorded"]);
        assert_eq!(status(&l, "leaf"), GoalStatus::Open);
        assert_eq!(l.contract_state(&gid("leaf")), Some(ContractState::Failed));

        // the optional check is irrelevant; the mandatory one passing verifies
        let ev = evidence(
            &mut l,
            "leaf",
            &c,
            vec![("lint", Verdict::Failed), ("tests", Verdict::Passed)],
        );
        assert_eq!(
            kinds(&ev),
            [
                "evidence_recorded",
                "evidence_recorded",
                "goal_status_changed"
            ]
        );
        assert_eq!(status(&l, "leaf"), GoalStatus::Verified);
        assert_eq!(l.evidence_of(&gid("leaf")).len(), 3);
        assert_eq!(l.evidence_count(), 3);

        // a regression
        let ev = evidence(&mut l, "leaf", &c, vec![("tests", Verdict::Error)]);
        assert_eq!(kinds(&ev), ["evidence_recorded", "goal_status_changed"]);
        assert_eq!(status(&l, "leaf"), GoalStatus::InProgress);
    }

    #[test]
    fn evidence_ids_count_up_and_evidence_is_never_edited() {
        let mut l = Ledger::new();
        let c = contract(&[("tests", true)]);
        add(&mut l, new_goal("leaf", None, Some(c.clone())));
        evidence(&mut l, "leaf", &c, vec![("tests", Verdict::Failed)]);
        let first = l.evidence(&EvidenceId::from_seq(1)).unwrap().clone();
        evidence(&mut l, "leaf", &c, vec![("tests", Verdict::Passed)]);
        let ids: Vec<String> = l
            .evidence_of(&gid("leaf"))
            .iter()
            .map(|e| e.id().to_string())
            .collect();
        assert_eq!(ids, ["e1", "e2"]);
        assert_eq!(l.evidence(&EvidenceId::from_seq(1)), Some(&first));
        // replaying the same event again changes nothing
        let again = first.to_event();
        assert!(!l.apply(&again));
        assert_eq!(l.evidence_count(), 2);
        assert!(l.evidence(&EvidenceId::from_seq(9)).is_none());
    }

    #[test]
    fn a_parent_is_verified_only_when_children_and_its_own_contract_agree() {
        let mut l = Ledger::new();
        let pc = contract(&[("integration", true)]);
        let cc = contract(&[("unit", true)]);
        add(&mut l, new_goal("parent", None, Some(pc.clone())));
        add(&mut l, new_goal("a", Some("parent"), Some(cc.clone())));
        add(&mut l, new_goal("b", Some("parent"), Some(cc.clone())));

        // the parent's contract passes first: still waiting for the children
        evidence(
            &mut l,
            "parent",
            &pc,
            vec![("integration", Verdict::Passed)],
        );
        assert_eq!(status(&l, "parent"), GoalStatus::Open);
        evidence(&mut l, "a", &cc, vec![("unit", Verdict::Passed)]);
        assert_eq!(status(&l, "a"), GoalStatus::Verified);
        assert_eq!(status(&l, "parent"), GoalStatus::Open);
        // the last child verifies the parent in the same batch of events
        let ev = evidence(&mut l, "b", &cc, vec![("unit", Verdict::Passed)]);
        assert_eq!(
            kinds(&ev),
            [
                "evidence_recorded",
                "goal_status_changed",
                "goal_status_changed"
            ]
        );
        assert_eq!(status(&l, "parent"), GoalStatus::Verified);
        // a child regressing takes the parent's verification away
        evidence(&mut l, "a", &cc, vec![("unit", Verdict::Failed)]);
        assert_eq!(status(&l, "a"), GoalStatus::InProgress);
        assert_eq!(status(&l, "parent"), GoalStatus::InProgress);
        // and the parent's own contract failing does too, with all children verified
        evidence(&mut l, "a", &cc, vec![("unit", Verdict::Passed)]);
        assert_eq!(status(&l, "parent"), GoalStatus::Verified);
        evidence(
            &mut l,
            "parent",
            &pc,
            vec![("integration", Verdict::Failed)],
        );
        assert_eq!(status(&l, "parent"), GoalStatus::InProgress);
        assert_eq!(status(&l, "a"), GoalStatus::Verified);
    }

    #[test]
    fn a_parent_without_a_contract_follows_its_children_and_a_leaf_without_one_never_verifies() {
        let mut l = Ledger::new();
        let cc = contract(&[("unit", true)]);
        add(&mut l, new_goal("parent", None, None));
        add(&mut l, new_goal("kid", Some("parent"), Some(cc.clone())));
        add(&mut l, new_goal("bare", None, None));
        evidence(&mut l, "kid", &cc, vec![("unit", Verdict::Passed)]);
        assert_eq!(status(&l, "parent"), GoalStatus::Verified);
        assert_eq!(
            status(&l, "bare"),
            GoalStatus::Open,
            "no contract, no evidence, no verification"
        );
        // a new child puts the parent back to work
        add(&mut l, new_goal("kid2", Some("parent"), Some(cc)));
        assert_eq!(status(&l, "parent"), GoalStatus::InProgress);
        // abandoning that child lets the parent be verified again
        let ev = l
            .set_status(&gid("kid2"), GoalStatus::Abandoned, "out of scope")
            .unwrap();
        assert_eq!(kinds(&ev), ["goal_status_changed", "goal_status_changed"]);
        commit(&mut l, ev);
        assert_eq!(status(&l, "parent"), GoalStatus::Verified);
        assert_eq!(l.goal(&gid("kid2")).unwrap().reason, "out of scope");
    }

    #[test]
    fn a_parent_whose_children_are_all_abandoned_is_not_verified() {
        let mut l = Ledger::new();
        add(&mut l, new_goal("parent", None, None));
        add(&mut l, new_goal("kid", Some("parent"), None));
        let ev = l
            .set_status(&gid("kid"), GoalStatus::Abandoned, "")
            .unwrap();
        commit(&mut l, ev);
        assert_eq!(status(&l, "parent"), GoalStatus::Open);
        assert_eq!(l.goal(&gid("kid")).unwrap().reason, "set by an operator");
    }

    #[test]
    fn status_changes_by_hand_follow_the_machine() {
        let mut l = Ledger::new();
        let c = contract(&[("tests", true)]);
        add(&mut l, new_goal("g", None, Some(c.clone())));
        let err = l
            .set_status(&gid("g"), GoalStatus::Verified, "trust me")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        assert_eq!(
            l.set_status(&gid("g"), GoalStatus::Open, "")
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert_eq!(
            l.set_status(&gid("nope"), GoalStatus::Blocked, "")
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
        assert_eq!(
            l.set_status(&gid("g"), GoalStatus::Blocked, "a\u{1b}b")
                .unwrap_err()
                .code,
            ErrorCode::Invalid
        );
        assert_eq!(
            l.set_status(&gid("g"), GoalStatus::Blocked, &"r".repeat(MAX_REASON + 1))
                .unwrap_err()
                .code,
            ErrorCode::Invalid
        );
        for to in [
            GoalStatus::InProgress,
            GoalStatus::Blocked,
            GoalStatus::Open,
        ] {
            let ev = l.set_status(&gid("g"), to, "because").unwrap();
            commit(&mut l, ev);
            assert_eq!(status(&l, "g"), to);
        }
        evidence(&mut l, "g", &c, vec![("tests", Verdict::Passed)]);
        assert_eq!(status(&l, "g"), GoalStatus::Verified);
        for to in [
            GoalStatus::Open,
            GoalStatus::InProgress,
            GoalStatus::Blocked,
        ] {
            assert_eq!(
                l.set_status(&gid("g"), to, "").unwrap_err().code,
                ErrorCode::Conflict,
                "{to}"
            );
        }
        let ev = l
            .set_status(&gid("g"), GoalStatus::Abandoned, "dropped")
            .unwrap();
        commit(&mut l, ev);
        // abandoned goals can come back, and are then settled against their evidence again
        let ev = l.set_status(&gid("g"), GoalStatus::Open, "").unwrap();
        assert_eq!(kinds(&ev), ["goal_status_changed", "goal_status_changed"]);
        commit(&mut l, ev);
        assert_eq!(status(&l, "g"), GoalStatus::Verified);
    }

    #[test]
    fn attaching_another_contract_discards_the_old_verification() {
        let mut l = Ledger::new();
        let c1 = contract(&[("tests", true)]);
        let c2 = contract(&[("tests", true), ("docs", true)]);
        add(&mut l, new_goal("g", None, Some(c1.clone())));
        evidence(&mut l, "g", &c1, vec![("tests", Verdict::Passed)]);
        assert_eq!(status(&l, "g"), GoalStatus::Verified);
        assert_eq!(
            l.attach_criteria(&gid("g"), &c1).unwrap_err().code,
            ErrorCode::Conflict
        );
        let ev = l.attach_criteria(&gid("g"), &c2).unwrap();
        assert_eq!(kinds(&ev), ["criteria_attached", "goal_status_changed"]);
        commit(&mut l, ev);
        assert_eq!(status(&l, "g"), GoalStatus::InProgress);
        assert_eq!(l.contract_state(&gid("g")), Some(ContractState::Pending));
        // a result of the old contract that arrives late is refused
        let err = l
            .record_evidence(&scope("g", &c1), vec![report("tests", Verdict::Passed)], 1)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
        assert_eq!(
            l.attach_criteria(&gid("nope"), &c2).unwrap_err().code,
            ErrorCode::NotFound
        );
        let ev = l.set_status(&gid("g"), GoalStatus::Abandoned, "").unwrap();
        commit(&mut l, ev);
        assert_eq!(
            l.attach_criteria(&gid("g"), &c1).unwrap_err().code,
            ErrorCode::Conflict
        );
    }

    #[test]
    fn evidence_must_fit_the_goal_and_its_contract() {
        let mut l = Ledger::new();
        let c = contract(&[("tests", true)]);
        add(&mut l, new_goal("bare", None, None));
        add(&mut l, new_goal("g", None, Some(c.clone())));
        let rec = |l: &Ledger, goal: &str, reports: Vec<CheckReport>| {
            l.record_evidence(&scope(goal, &c), reports, 1)
        };
        assert_eq!(
            rec(&l, "bare", vec![report("tests", Verdict::Passed)])
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert_eq!(
            rec(&l, "nope", vec![report("tests", Verdict::Passed)])
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
        assert_eq!(rec(&l, "g", vec![]).unwrap_err().code, ErrorCode::Invalid);
        assert_eq!(
            rec(&l, "g", vec![report("other", Verdict::Passed)])
                .unwrap_err()
                .code,
            ErrorCode::Invalid
        );
        let ev = l.set_status(&gid("g"), GoalStatus::Abandoned, "").unwrap();
        commit(&mut l, ev);
        assert_eq!(
            rec(&l, "g", vec![report("tests", Verdict::Passed)])
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
    }

    #[test]
    fn evidence_is_bounded() {
        let mut l = Ledger::new();
        let c = contract(&[("tests", true)]);
        add(&mut l, new_goal("g", None, Some(c.clone())));
        for _ in 0..MAX_EVIDENCE {
            evidence(&mut l, "g", &c, vec![("tests", Verdict::Failed)]);
        }
        let err = l
            .record_evidence(&scope("g", &c), vec![report("tests", Verdict::Passed)], 1)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Limit);
        assert_eq!(l.evidence_count(), MAX_EVIDENCE);
    }

    #[test]
    fn events_that_do_not_fit_are_ignored() {
        let mut l = Ledger::new();
        let c = contract(&[("tests", true)]);
        add(&mut l, new_goal("g", None, Some(c.clone())));
        let added = |id: &str, parent: Option<&str>, tasks: Vec<&str>| DomainEvent::GoalAdded {
            goal: gid(id),
            parent: parent.map(gid),
            title: "t".into(),
            intent: String::new(),
            tasks: tasks.into_iter().map(String::from).collect(),
        };
        assert!(!l.apply(&added("g", None, vec![])), "duplicate id");
        assert!(
            !l.apply(&added("x", Some("missing"), vec![])),
            "unknown parent"
        );
        assert!(
            !l.apply(&added("x", None, vec!["not-a-task-ref"])),
            "bad task"
        );
        assert!(
            !l.apply(&DomainEvent::Warning {
                message: "w".into()
            }),
            "foreign event"
        );
        let change =
            |goal: &str, from: GoalStatus, to: GoalStatus| DomainEvent::GoalStatusChanged {
                goal: gid(goal),
                from,
                to,
                reason: String::new(),
            };
        assert!(
            !l.apply(&change("g", GoalStatus::Blocked, GoalStatus::Open)),
            "wrong from"
        );
        assert!(
            !l.apply(&change("zz", GoalStatus::Open, GoalStatus::Blocked)),
            "unknown goal"
        );
        let criteria = |goal: &str, id: &str, doc: Value| DomainEvent::CriteriaAttached {
            goal: gid(goal),
            contract_id: id.into(),
            contract: Document(doc),
        };
        assert!(!l.apply(&criteria("g", "short", c.contract.to_value())));
        assert!(!l.apply(&criteria(
            "g",
            c.id.as_str(),
            Value::object([("version", Value::int(7))])
        )));
        assert!(!l.apply(&criteria("zz", c.id.as_str(), c.contract.to_value())));
        // evidence for an unknown goal, for another contract, or malformed
        let other = contract(&[("tests", true), ("more", false)]);
        for bad in [scope("zz", &c), scope("g", &other)] {
            let e = Evidence::record(
                EvidenceId::from_seq(50),
                &bad,
                report("tests", Verdict::Passed),
                1,
            );
            assert!(!l.apply(&e.to_event()));
        }
        assert_eq!(l.len(), 1);
        assert_eq!(l.evidence_count(), 0);
    }

    #[test]
    fn remaining_lists_open_leaves_and_blockers() {
        let mut l = Ledger::new();
        let c = contract(&[("tests", true)]);
        add(&mut l, new_goal("root", None, None));
        add(&mut l, new_goal("a", Some("root"), Some(c.clone())));
        add(&mut l, new_goal("b", Some("root"), None));
        add(&mut l, new_goal("c", Some("root"), None));
        add(&mut l, new_goal("d", Some("root"), None));
        for (id, to) in [
            ("b", GoalStatus::Blocked),
            ("c", GoalStatus::InProgress),
            ("d", GoalStatus::Abandoned),
        ] {
            let ev = l.set_status(&gid(id), to, "x").unwrap();
            commit(&mut l, ev);
        }
        let r = l.remaining();
        let names = |v: &[&Goal]| v.iter().map(|g| g.id.to_string()).collect::<Vec<_>>();
        assert_eq!(names(&r.open_leaves), ["a", "c"]);
        assert_eq!(names(&r.blocked), ["b"]);
        evidence(&mut l, "a", &c, vec![("tests", Verdict::Passed)]);
        let r = l.remaining();
        assert_eq!(names(&r.open_leaves), ["c"]);
    }

    #[test]
    fn a_parent_with_only_its_own_contract_left_is_a_leaf_of_the_remaining_work() {
        let mut l = Ledger::new();
        let pc = contract(&[("integration", true)]);
        let cc = contract(&[("unit", true)]);
        add(&mut l, new_goal("parent", None, Some(pc)));
        add(&mut l, new_goal("kid", Some("parent"), Some(cc.clone())));
        assert_eq!(
            l.remaining()
                .open_leaves
                .iter()
                .map(|g| g.id.to_string())
                .collect::<Vec<_>>(),
            ["kid"]
        );
        evidence(&mut l, "kid", &cc, vec![("unit", Verdict::Passed)]);
        assert_eq!(
            l.remaining()
                .open_leaves
                .iter()
                .map(|g| g.id.to_string())
                .collect::<Vec<_>>(),
            ["parent"]
        );
    }

    #[test]
    fn tasks_are_governed_by_the_contracts_of_their_goals() {
        let mut l = Ledger::new();
        let c = contract(&[("tests", true)]);
        let r1 = RunId::parse("r1").unwrap();
        let t1 = TaskId::parse("t1").unwrap();
        let t2 = TaskId::parse("t2").unwrap();
        assert_eq!(l.task_contract(&r1, &t1), TaskContract::NotGoverned);
        let mut with_task = new_goal("g", None, Some(c.clone()));
        with_task.tasks = vec![TaskRef::new(r1.clone(), t1.clone())];
        add(&mut l, with_task);
        let mut no_contract = new_goal("h", None, None);
        no_contract.tasks = vec![TaskRef::new(r1.clone(), t2.clone())];
        add(&mut l, no_contract);
        assert_eq!(l.owners_of(&r1, &t1).len(), 1);
        assert_eq!(l.governing(&r1, &t1).len(), 1);
        assert_eq!(l.task_contract(&r1, &t1), TaskContract::Pending);
        assert_eq!(l.owners_of(&r1, &t2).len(), 1);
        assert_eq!(
            l.task_contract(&r1, &t2),
            TaskContract::NotGoverned,
            "no contract, no extra guard"
        );
        evidence(&mut l, "g", &c, vec![("tests", Verdict::Failed)]);
        assert_eq!(l.task_contract(&r1, &t1), TaskContract::Failed);
        evidence(&mut l, "g", &c, vec![("tests", Verdict::Passed)]);
        assert_eq!(l.task_contract(&r1, &t1), TaskContract::Passed);
        let ev = l.set_status(&gid("g"), GoalStatus::Abandoned, "").unwrap();
        commit(&mut l, ev);
        assert_eq!(
            l.task_contract(&r1, &t1),
            TaskContract::NotGoverned,
            "abandoned goals govern nothing"
        );
    }

    // ---- property tests ----------------------------------------------------------------------

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, bound: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % bound.max(1)
        }
    }

    fn check_invariants(l: &Ledger) {
        let ids: Vec<&GoalId> = l.order.iter().collect();
        assert_eq!(ids.len(), l.goals.len());
        for (i, g) in l.goals().enumerate() {
            assert!(g.depth >= 1 && g.depth <= MAX_DEPTH);
            assert!(g.children.len() <= MAX_CHILDREN);
            // acyclic: the parent was added earlier, so walking up always ends
            match &g.parent {
                None => assert_eq!(g.depth, 1),
                Some(p) => {
                    let pos = ids.iter().position(|x| *x == p).expect("parent exists");
                    assert!(pos < i, "a parent is added before its children");
                    let pg = l.goal(p).unwrap();
                    assert_eq!(g.depth, pg.depth + 1);
                    assert!(pg.children.contains(&g.id));
                }
            }
            if g.status == GoalStatus::Abandoned {
                continue;
            }
            // the fixed point: verified exactly when satisfied
            assert_eq!(
                l.satisfied(g),
                g.status == GoalStatus::Verified,
                "{} {:?}",
                g.id,
                g.status
            );
            if g.status == GoalStatus::Verified {
                for c in l.children_of(g) {
                    assert!(matches!(
                        c.status,
                        GoalStatus::Verified | GoalStatus::Abandoned
                    ));
                }
                if g.contract.is_some() {
                    assert_eq!(l.contract_state(&g.id), Some(ContractState::Passed));
                }
            }
            // evidence of a goal is ordered by id
            let seqs: Vec<u64> = l.evidence_of(&g.id).iter().map(|e| e.id().seq()).collect();
            assert!(seqs.windows(2).all(|w| w[0] < w[1]));
        }
    }

    #[test]
    fn random_histories_keep_every_invariant_and_replay_exactly() {
        for seed in 0..40u64 {
            let mut rng = Lcg(seed + 1);
            let mut l = Ledger::new();
            let mut log: Vec<DomainEvent> = Vec::new();
            let mut seen_evidence: BTreeMap<EvidenceId, Evidence> = BTreeMap::new();
            let contracts = [
                contract(&[("a", true)]),
                contract(&[("a", true), ("b", true), ("opt", false)]),
                contract(&[("b", true)]),
            ];
            for step in 0..120 {
                let ids: Vec<GoalId> = l.order.clone();
                let pick = |rng: &mut Lcg| ids.get(rng.next(ids.len())).cloned();
                let result: Result<Vec<DomainEvent>> = match rng.next(10) {
                    0..=2 => {
                        let parent = if rng.next(3) == 0 {
                            None
                        } else {
                            pick(&mut rng)
                        };
                        let c = (rng.next(3) != 0).then(|| contracts[rng.next(3)].clone());
                        l.add_goal(NewGoal {
                            id: None,
                            parent,
                            title: format!("g{step}"),
                            intent: String::new(),
                            tasks: vec![],
                            contract: c,
                        })
                        .map(|(_, e)| e)
                    }
                    3..=6 => match pick(&mut rng) {
                        Some(goal) => match l.goal(&goal).unwrap().contract.clone() {
                            Some(c) => {
                                let checks: Vec<&neo_verify::contract::Check> =
                                    c.contract.checks().iter().collect();
                                let mut reports: Vec<CheckReport> = Vec::new();
                                for ch in &checks {
                                    if rng.next(4) == 0 {
                                        continue;
                                    }
                                    let v = match rng.next(10) {
                                        0..=6 => Verdict::Passed,
                                        7..=8 => Verdict::Failed,
                                        _ => Verdict::Error,
                                    };
                                    reports.push(report(ch.id.as_str(), v));
                                }
                                l.record_evidence(&scope(goal.as_str(), &c), reports, step as u64)
                            }
                            None => continue,
                        },
                        None => continue,
                    },
                    7..=8 => match pick(&mut rng) {
                        Some(goal) => {
                            let to = GoalStatus::ALL[rng.next(GoalStatus::ALL.len())];
                            let r = l.set_status(&goal, to, "");
                            if to == GoalStatus::Verified {
                                assert_eq!(r.as_ref().unwrap_err().code, ErrorCode::Forbidden);
                            }
                            r
                        }
                        None => continue,
                    },
                    _ => match pick(&mut rng) {
                        Some(goal) => l.attach_criteria(&goal, &contracts[rng.next(3)]),
                        None => continue,
                    },
                };
                if let Ok(events) = result {
                    for e in &events {
                        assert!(l.apply(e), "seed {seed} step {step}: {e:?}");
                    }
                    log.extend(events);
                }
                check_invariants(&l);
                // evidence is append-only and immutable
                for (id, old) in &seen_evidence {
                    assert_eq!(l.evidence(id), Some(old));
                }
                for g in l.goals() {
                    for e in l.evidence_of(&g.id) {
                        seen_evidence.insert(e.id().clone(), e.clone());
                    }
                }
            }
            // replaying the log from nothing gives the same ledger
            let mut again = Ledger::new();
            for e in &log {
                assert!(again.apply(e));
            }
            let snapshot =
                |l: &Ledger| (l.goals().cloned().collect::<Vec<_>>(), l.evidence_count());
            assert_eq!(snapshot(&again), snapshot(&l), "seed {seed}");
            check_invariants(&again);
        }
    }

    #[test]
    fn a_status_machine_sweep_never_reaches_verified_without_evidence() {
        // From a fresh goal with a contract, every sequence of manual status changes leaves it
        // unverified.
        let statuses = [
            GoalStatus::Open,
            GoalStatus::InProgress,
            GoalStatus::Blocked,
            GoalStatus::Abandoned,
            GoalStatus::Verified,
        ];
        let c = contract(&[("tests", true)]);
        for seed in 0..50u64 {
            let mut rng = Lcg(seed + 100);
            let mut l = Ledger::new();
            add(&mut l, new_goal("g", None, Some(c.clone())));
            for _ in 0..30 {
                let to = statuses[rng.next(statuses.len())];
                if let Ok(ev) = l.set_status(&gid("g"), to, "") {
                    commit(&mut l, ev);
                }
                assert_ne!(status(&l, "g"), GoalStatus::Verified);
            }
        }
    }
}
