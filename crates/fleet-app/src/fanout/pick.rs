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
use fleet_fanout::compare::{EvidenceStatus, MemberFacts};
use fleet_fanout::diff::ScopeState;
use fleet_fanout::group::Group;
use std::collections::BTreeSet;

/// The statement of the evidence and the scope that an approval of a pick carries.
fn evidence_line(f: &MemberFacts, flagged: bool) -> String {
    let (status, why) = f.verification();
    // the words an operator must not miss come first: long arguments are cut when listed
    let mut line = match (status, flagged) {
        (EvidenceStatus::Verified, false) => "evidence: verified".to_string(),
        (EvidenceStatus::Verified, true) => {
            "evidence: verified, picked with --allow-unverified".to_string()
        }
        (_, _) => "evidence: UNVERIFIED, picked with --allow-unverified".to_string(),
    };
    line.push_str(&format!(" ({})", flatten_line(&why, 160)));
    line.push_str(&match f.scope_state() {
        ScopeState::In => "; scope: inside the allowed paths".to_string(),
        ScopeState::Unknown => {
            format!(
                "; scope: UNKNOWN ({})",
                flatten_line(&f.scope_reason(), 160)
            )
        }
        ScopeState::Out => "; scope: OUTSIDE the allowed paths".to_string(),
    });
    line
}

/// The word recorded with the winner: how its evidence and its scope stood.
fn pick_word(f: &MemberFacts) -> String {
    match (f.verification().0, f.scope_state()) {
        (EvidenceStatus::Verified, ScopeState::In) => "verified",
        (EvidenceStatus::Verified, _) => "verified, scope unknown",
        _ => "unverified",
    }
    .to_string()
}

fn short(sha: &Sha) -> &str {
    sha.as_str().get(..12).unwrap_or(sha.as_str())
}

impl Orchestrator {
    /// The member of `g` whose task was merged already, with its merge commit.
    fn merged_member(&self, g: &Group) -> Option<(TaskId, Sha)> {
        let run = self.state.run(&g.run).ok()?;
        g.members.iter().find_map(|m| {
            run.task(&m.task)
                .and_then(|t| t.merged.clone())
                .map(|sha| (m.task.clone(), sha))
        })
    }

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
        let (repo, base, branch, reviewed) = {
            let run = self.state.run(&key.0)?;
            let task = run
                .task(&key.1)
                .ok_or_else(|| FleetError::not_found(format!("task {reference} does not exist")))?;
            (
                run.repo.clone(),
                run.base_sha.clone(),
                task.branch.clone(),
                task.head.clone(),
            )
        };

        // A merge that happened before its pick could be recorded (the daemon stopped in between)
        // is finished, never repeated, and no other member is merged on top of it.
        if g.winner.is_none() {
            if let Some((merged, sha)) = self.merged_member(&g) {
                if merged != key.1 {
                    return Err(FleetError::conflict(format!(
                        "member {merged} of group {} was merged already and its pick was not recorded; run `fleet pick {} {merged}` to record it: no other member is merged on top of it",
                        g.id, g.id
                    )));
                }
                g.check_pick(&key.1)?;
                let facts = self.pick_facts(&g, &key, &repo, &base)?;
                return self.record_pick(group, &key, sha, &facts, remove_others, &branch);
            }
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
        let facts = self.pick_facts(&g, &key, &repo, &base)?;
        let (status, why) = facts.verification();
        let scope = facts.scope_state();
        match status {
            EvidenceStatus::Failed => {
                return Err(FleetError::conflict(format!(
                    "{reference} cannot be picked: {why}"
                )))
            }
            EvidenceStatus::Unverified if facts.has_contract => {
                return Err(FleetError::conflict(format!(
                    "{reference} has no passing evidence for its commit ({why}); a verification contract guards it, so --allow-unverified cannot help: run `fleet verify {}/{}` first",
                    key.0, key.1
                )))
            }
            _ => {}
        }
        if scope == ScopeState::Out {
            return Err(FleetError::conflict(format!(
                "{reference} cannot be picked: {}",
                facts.scope_reason()
            )));
        }
        let needs_flag = status == EvidenceStatus::Unverified || scope == ScopeState::Unknown;
        if needs_flag && !allow_unverified {
            let what = if status == EvidenceStatus::Unverified {
                format!("unverified evidence ({why})")
            } else {
                format!("a scope that is not known ({})", facts.scope_reason())
            };
            return Err(FleetError::conflict(format!(
                "{reference} has {what}; pick it only with --allow-unverified, which the approval will state"
            )));
        }
        let evidence_line = evidence_line(&facts, needs_flag);
        let word = pick_word(&facts);
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
                        "picking {reference} for group {} needs approval {subject} (digest {prefix}; the evidence is {word}); approve it with `fleet approve {subject}` and run `fleet pick {} {reference}` again",
                        g.id, g.id
                    ),
                )
                .with_subject(subject));
            }
            Err(e) => return Err(e),
        };
        self.record_pick(group, &key, merged.sha, &facts, remove_others, &branch)
    }

    /// The facts of one member: how its evidence and its scope stand for its recorded commit and tree.
    fn pick_facts(&mut self, g: &Group, key: &Key, repo: &str, base: &Sha) -> Result<MemberFacts> {
        let Some(member) = g.member(&key.1).cloned() else {
            return Err(FleetError::not_found("not a member of the group"));
        };
        self.facts_of(g, &member, repo, base)
    }

    /// Records the winner (the merge is done), supersedes the others and, when asked, removes
    /// their worktrees and branches now. A cleanup that fails is a warning: the pick stands.
    fn record_pick(
        &mut self,
        group: &GroupId,
        key: &Key,
        merge_sha: Sha,
        facts: &MemberFacts,
        remove_others: bool,
        branch: &str,
    ) -> Result<Reply> {
        let word = pick_word(facts);
        let event = self
            .fanout
            .groups
            .pick_event(group, &key.1, merge_sha.clone(), &word)?;
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
            match self.cleanup_filtered(Some(&key.0), true, Some(&keys)) {
                Ok(Reply::Data(v)) => {
                    if let Some(list) = v.get("removed").and_then(Value::as_array) {
                        removed.extend(list.iter().cloned());
                    }
                }
                Ok(_) => {}
                Err(e) => self.warn(format!(
                    "group {group}: the pick is recorded, but removing the others failed: {}",
                    e.message
                )),
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

    /// A merge whose pick was not recorded (the daemon stopped between the two): the winner is
    /// recorded now, once, for the member that was merged.
    pub(crate) fn record_unrecorded_picks(&mut self) {
        let groups: Vec<Group> = self
            .fanout
            .groups
            .iter()
            .filter(|g| g.winner.is_none() && g.closed.is_none())
            .cloned()
            .collect();
        for g in groups {
            let Some((task, sha)) = self.merged_member(&g) else {
                continue;
            };
            if g.check_pick(&task).is_err() {
                continue;
            }
            let key: Key = (g.run.clone(), task.clone());
            let (repo, base) = match self.state.run(&g.run) {
                Ok(r) => (r.repo.clone(), r.base_sha.clone()),
                Err(_) => continue,
            };
            let word = match self.pick_facts(&g, &key, &repo, &base) {
                Ok(f) => pick_word(&f),
                Err(_) => "unverified".to_string(),
            };
            let recorded = self
                .fanout
                .groups
                .pick_event(&g.id, &task, sha, &word)
                .and_then(|e| self.commit(vec![e]));
            if let Err(e) = recorded {
                self.warn(format!("group {}: {}", g.id, e.message));
            }
        }
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
