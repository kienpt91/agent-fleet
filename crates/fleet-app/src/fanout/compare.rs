//! `fleet compare`: the facts of every member, gathered from the places they live.
//!
//! Nothing here comes from what a worker said. The state of the task is the daemon's; the diff is
//! what the repository reports for the commit review recorded; the verification status is read
//! from the ledger's evidence, which fleet itself recorded for exactly that commit and tree; the
//! paths a member touched are judged against the allow globs of the contract's `diff` checks.

use super::views;
use crate::api::Reply;
use crate::neo::data;
use crate::orchestrator::Orchestrator;
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::{GroupId, Sha};
use fleet_fanout::compare::{compare, CheckFact, EvidenceStatus, GateFact, MemberFacts};
use fleet_fanout::diff::{scope_report, DiffStat};
use fleet_fanout::group::{Group, Member};
use fleet_fanout::rank::RankRule;
use neo_goals::goal::TaskRef;
use neo_verify::contract::CheckKind;

impl Orchestrator {
    pub(crate) fn group_list(&self) -> Reply {
        Reply::Data(views::group_list(
            self.fanout.groups.iter().collect::<Vec<_>>().as_slice(),
        ))
    }

    pub(crate) fn group_show(&self, group: &GroupId) -> Result<Reply> {
        let g = self.fanout.groups.get(group)?;
        Ok(Reply::Data(views::group_show(g, &self.state)))
    }

    pub(crate) fn group_compare(
        &mut self,
        group: &GroupId,
        rank: Option<&[String]>,
    ) -> Result<Reply> {
        let rule = match rank {
            None => self.cfg.compare_rank.clone(),
            Some(names) => {
                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                RankRule::parse(&names)?
            }
        };
        let g = self.fanout.groups.get(group)?.clone();
        let facts = self.member_facts(&g)?;
        let comparison = compare(&rule, facts);
        Ok(data(
            "fleet.group.compare/1",
            views::comparison_fields(&g, &comparison),
        ))
    }

    /// The facts of every member of `g`, in the group's order.
    pub(crate) fn member_facts(&mut self, g: &Group) -> Result<Vec<MemberFacts>> {
        let run = self.state.run(&g.run)?;
        let repo = run.repo.clone();
        let base = run.base_sha.clone();
        let mut out = Vec::with_capacity(g.members.len());
        for m in &g.members {
            out.push(self.facts_of(g, m, &repo, &base)?);
        }
        Ok(out)
    }

    pub(super) fn facts_of(
        &mut self,
        g: &Group,
        m: &Member,
        repo: &str,
        base: &Sha,
    ) -> Result<MemberFacts> {
        let task = self
            .state
            .run(&g.run)?
            .task(&m.task)
            .cloned()
            .ok_or_else(|| FleetError::not_found(format!("task {} does not exist", m.task)))?;
        // The commit review recorded and its tree, from the repository; the branch as it is now.
        let head = task.head.clone();
        let tree = head
            .as_ref()
            .and_then(|h| self.ports.vcs.commit_tree(repo, h).ok());
        let branch_head = self.ports.vcs.branch_head(repo, &task.branch).ok();
        let (diff, diff_error): (Option<DiffStat>, Option<String>) = match &head {
            None => (
                None,
                Some("no commit of this task was recorded (its work was not reviewed)".to_string()),
            ),
            Some(h) => match self.fanout.cached_diff(repo, base, h) {
                Some(d) => (Some(d), None),
                None => match self.ports.vcs.diff_summary(repo, base, h) {
                    Ok(d) => {
                        self.fanout.remember_diff(repo, base, h, &d);
                        (Some(d), None)
                    }
                    Err(e) => (None, Some(e.message)),
                },
            },
        };
        let touched = diff.as_ref().map(DiffStat::touched_paths);

        let tref = TaskRef::new(g.run.clone(), m.task.clone());
        let goals: Vec<_> = self
            .ledger
            .goals()
            .filter(|goal| goal.contract.is_some() && goal.tasks.contains(&tref))
            .collect();
        let mut checks = Vec::new();
        let mut scopes = Vec::new();
        let mut has_scope_check = false;
        let mut scope_unknown = false;
        for goal in &goals {
            let Some(attached) = &goal.contract else {
                continue;
            };
            let evidence = self.ledger.evidence_of(&goal.id);
            for check in attached.contract.checks() {
                let newest = match (&head, &tree) {
                    (Some(h), Some(t)) => evidence
                        .iter()
                        .filter(|e| {
                            e.counts()
                                && e.contract() == &attached.id
                                && e.check() == &check.id
                                && e.run() == &g.run
                                && e.task() == &m.task
                                && e.commit() == h
                                && e.tree() == t
                        })
                        .max_by_key(|e| e.id().seq()),
                    _ => None,
                };
                checks.push(CheckFact {
                    goal: goal.id.to_string(),
                    check: check.id.to_string(),
                    kind: check.kind.name().to_string(),
                    mandatory: check.mandatory,
                    status: match newest {
                        None => EvidenceStatus::Unverified,
                        Some(e) if e.passed() => EvidenceStatus::Verified,
                        Some(_) => EvidenceStatus::Failed,
                    },
                    summary: newest.map(|e| e.summary().to_string()).unwrap_or_default(),
                });
                if let CheckKind::DiffOnly { allow } = &check.kind {
                    has_scope_check |= check.mandatory;
                    match &touched {
                        Some(paths) => {
                            scopes.push(scope_report(
                                check.id.as_str(),
                                check.mandatory,
                                allow,
                                paths,
                            ));
                            // the paths of a diff that was listed only in part were not all judged
                            scope_unknown |=
                                check.mandatory && diff.as_ref().is_some_and(|d| d.truncated);
                        }
                        None => scope_unknown |= check.mandatory,
                    }
                }
            }
        }
        Ok(MemberFacts {
            task: m.task.clone(),
            worker: task.worker.as_ref().map(|w| w.to_string()),
            state: m.state,
            outcome: m.outcome,
            task_status: task.status.to_string(),
            attempts: task.attempt,
            duration_ms: m.duration_ms(),
            head,
            tree,
            branch_head,
            diff,
            diff_error,
            gates: task
                .gates
                .iter()
                .map(|gate| GateFact {
                    name: gate.name.clone(),
                    passed: gate.passed,
                })
                .collect(),
            has_contract: !goals.is_empty(),
            checks,
            scopes,
            has_scope_check,
            scope_unknown,
            summary: task.summary.clone(),
            failure: task.failure.clone(),
        })
    }
}
