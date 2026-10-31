//! `fleet pick`: the one way a group's result reaches the integration branch.
//!
//! Picking is a merge, and it is held to what a merge is held to: the approval policy applies and
//! the approval is bound to the digest of the exact action. The action of a pick is the merge of
//! the member's branch at the commit review recorded, tagged with the group and with how the
//! member's evidence stands, so an approval given for a verified pick does not cover an
//! unverified one, and neither covers a plain `fleet merge` of the same branch. Before anything is
//! asked or done the daemon checks, from its own facts, that the member is the one that was
//! reviewed (the branch still points at the reviewed commit) and what its evidence is.

use super::{MergeVariant, Worktrees};
use crate::api::Reply;
use crate::neo::data;
use crate::orchestrator::{Key, Orchestrator};
use fleet_domain::approval::Action;
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::events::DomainEvent;
use fleet_domain::ids::{show, ApprovalId, GroupId, Sha, TaskId};
use fleet_domain::sanitize::flatten_line;
use fleet_domain::status::{CloseReason, GoalStatus};
use fleet_domain::value::Value;
use fleet_fanout::compare::EvidenceStatus;
use std::collections::BTreeSet;

fn short(sha: &Sha) -> &str {
    sha.as_str().get(..12).unwrap_or(sha.as_str())
}

impl Orchestrator {
    pub(crate) fn group_pick(
        &mut self,
        group: &GroupId,
        reference: &str,
        allow_unverified: bool,
        remove_others: bool,
    ) -> Result<Reply> {
        let key: Key = self.state.find_task(reference)?;
        let g = self.fanout.groups.get(group)?.clone();
        if key.0 != g.run || g.member(&key.1).is_none() {
            return Err(FleetError::not_found(format!(
                "task {} is not a member of group {}",
                show(reference),
                show(group.as_str())
            )));
        }
        let (repo, base, branch, reviewed, merged) = {
            let run = self.state.run(&key.0)?;
            let task = run
                .task(&key.1)
                .ok_or_else(|| FleetError::not_found(format!("task {reference} does not exist")))?;
            (
                run.repo.clone(),
                run.base_sha.clone(),
                task.branch.clone(),
                task.head.clone(),
                task.merged.clone(),
            )
        };

        // A merge that happened before the pick could be recorded (the daemon stopped in between):
        // finish the pick on that merge, nothing is merged twice.
        if let (Some(sha), true) = (merged.clone(), g.winner.is_none()) {
            g.check_pick(&key.1)?;
            let word = self.pick_verification(&g, &key, &repo, &base)?.0;
            return self.record_pick(group, &key, sha, word, remove_others, &branch);
        }

        g.check_pick(&key.1)?;
        let Some(reviewed) = reviewed else {
            return Err(FleetError::conflict(format!(
                "{reference} has no recorded commit: its work was not reviewed"
            )));
        };
        let current = self.ports.vcs.branch_head(&repo, &branch)?;
        if current != reviewed {
            return Err(FleetError::conflict(format!(
                "{} points at {} now, but review and verification saw {}: its branch moved after the evidence was taken, so it cannot be picked; look at the new commits and verify them again",
                show(&branch),
                short(&current),
                short(&reviewed)
            )));
        }
        let (word, why) = self.pick_verification(&g, &key, &repo, &base)?;
        let status = match word.as_str() {
            "verified" => EvidenceStatus::Verified,
            "failed" => EvidenceStatus::Failed,
            _ => EvidenceStatus::Unverified,
        };
        match status {
            EvidenceStatus::Failed => {
                return Err(FleetError::conflict(format!(
                    "{reference} cannot be picked: {why}"
                )))
            }
            EvidenceStatus::Unverified if !allow_unverified => {
                return Err(FleetError::conflict(format!(
                    "{reference} has unverified evidence ({why}); pick it only with --allow-unverified, which the approval will state"
                )))
            }
            _ => {}
        }
        let evidence_line = match status {
            EvidenceStatus::Verified => format!("evidence: verified ({})", flatten_line(&why, 200)),
            _ => format!(
                "evidence: UNVERIFIED, picked with --allow-unverified ({})",
                flatten_line(&why, 200)
            ),
        };
        let group_id = g.id.to_string();
        let action = move |integration: &str, branch: &str, head: &Sha| Action {
            tool: "git".to_string(),
            command: "merge".to_string(),
            args: vec![
                integration.to_string(),
                branch.to_string(),
                head.to_string(),
                format!("fan-out pick of group {group_id}"),
                evidence_line.clone(),
            ],
        };
        let variant = MergeVariant {
            expected_head: &reviewed,
            action: &action,
        };
        let merged = match self.merge_task(&key, Some(&variant)) {
            Ok(m) => m,
            Err(e) if e.code == ErrorCode::ApprovalRequired => {
                let subject = e.subject.clone().unwrap_or_default();
                let digest = ApprovalId::parse(&subject)
                    .ok()
                    .and_then(|id| self.state.approval(&id).ok().map(|a| a.digest.clone()))
                    .unwrap_or_default();
                let prefix = digest.get(..12).unwrap_or(&digest);
                return Err(FleetError::new(
                    ErrorCode::ApprovalRequired,
                    format!(
                        "picking {reference} for group {} needs approval {subject} (digest {prefix}; the evidence is {}); approve it with `fleet approve {subject}` and run `fleet pick {} {reference}` again",
                        g.id,
                        if status == EvidenceStatus::Verified {
                            "verified"
                        } else {
                            "UNVERIFIED"
                        },
                        g.id
                    ),
                )
                .with_subject(subject));
            }
            Err(e) => return Err(e),
        };
        self.record_pick(group, &key, merged.sha, word, remove_others, &branch)
    }

    /// How the evidence of a member stands for its recorded commit and tree: the word recorded with
    /// the pick (`verified`, `unverified` or `failed`) and why.
    fn pick_verification(
        &mut self,
        g: &fleet_fanout::group::Group,
        key: &Key,
        repo: &str,
        base: &Sha,
    ) -> Result<(String, String)> {
        let Some(member) = g.member(&key.1).cloned() else {
            return Err(FleetError::not_found("not a member of the group"));
        };
        let facts = self.facts_of(g, &member, repo, base)?;
        let (status, why) = facts.verification();
        Ok((status.as_str().to_string(), why))
    }

    /// Records the winner (the merge is done), supersedes the others and, when asked, removes
    /// their worktrees and branches now.
    fn record_pick(
        &mut self,
        group: &GroupId,
        key: &Key,
        merge_sha: Sha,
        verification: String,
        remove_others: bool,
        branch: &str,
    ) -> Result<Reply> {
        let word = if verification == "verified" {
            "verified"
        } else {
            "unverified"
        };
        let event = self
            .fanout
            .groups
            .pick_event(group, &key.1, merge_sha.clone(), word)?;
        let superseded: Vec<TaskId> = match &event {
            DomainEvent::WinnerPicked { superseded, .. } => superseded.clone(),
            _ => Vec::new(),
        };
        self.commit(vec![event])?;
        let integration = self.state.run(&key.0)?.integration_branch();
        let mut removed = Vec::new();
        if remove_others {
            let keys: BTreeSet<Key> = superseded
                .iter()
                .map(|t| (key.0.clone(), t.clone()))
                .collect();
            if let Reply::Data(v) = self.cleanup_filtered(Some(&key.0), true, Some(&keys))? {
                if let Some(list) = v.get("removed").and_then(Value::as_array) {
                    removed.extend(list.iter().cloned());
                }
            }
        }
        Ok(data(
            "fleet.group.pick/1",
            vec![
                ("group", Value::str(group.as_str())),
                ("task", Value::str(format!("{}/{}", key.0, key.1))),
                ("branch", Value::str(branch)),
                ("integration_branch", Value::str(integration)),
                ("merge_sha", Value::str(merge_sha.as_str())),
                ("verification", Value::str(word)),
                (
                    "superseded",
                    Value::array(superseded.iter().map(|t| Value::str(t.as_str()))),
                ),
                ("removed", Value::array(removed)),
            ],
        ))
    }

    /// The group events that follow from `events` (which were applied already): a member whose
    /// task ended, a group with nothing left to wait for. When a group is closed because its
    /// winner is merged and everything is cleaned up, its goal is no longer asked to be verified by
    /// every member: it is abandoned, unless it was verified.
    pub(crate) fn group_follow_up(&self, events: &[DomainEvent]) -> Vec<DomainEvent> {
        let view = Worktrees(&self.state);
        let mut out = self.fanout.groups.follow_up(events, &view);
        self.abandon_closed_goals(&mut out);
        out
    }

    /// Whatever a restart finds unrecorded in the groups (see `Groups::reconcile`).
    pub(crate) fn group_reconcile(&self) -> Vec<DomainEvent> {
        let view = Worktrees(&self.state);
        let mut out = self.fanout.groups.reconcile(&view);
        self.abandon_closed_goals(&mut out);
        out
    }

    fn abandon_closed_goals(&self, events: &mut Vec<DomainEvent>) {
        let mut extra = Vec::new();
        for e in events.iter() {
            let DomainEvent::GroupClosed {
                group,
                reason: CloseReason::Resolved,
            } = e
            else {
                continue;
            };
            let Some(goal) = self.fanout.groups.find(group).and_then(|g| g.goal.clone()) else {
                continue;
            };
            let open = self
                .ledger
                .find(&goal)
                .is_some_and(|g| !matches!(g.status, GoalStatus::Verified | GoalStatus::Abandoned));
            if open {
                if let Ok(ev) = self.ledger.set_status(
                    &goal,
                    GoalStatus::Abandoned,
                    &format!(
                        "fan-out group {group} was resolved: the other members were superseded"
                    ),
                ) {
                    extra.extend(ev);
                }
            }
        }
        events.extend(extra);
    }
}
