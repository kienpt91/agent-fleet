//! The fan-out group aggregate and its state machine.
//!
//! A group is created with all its members at once (sibling tasks that carry the same spec), and
//! then only moves forward:
//!
//! ```text
//!   running --(every member ended)--> finished --(pick)--> decided --(worktrees gone)--> closed
//!                                         \--(no member completed)--> closed
//! ```
//!
//! `Groups` is a projection of the event log, like the fleet state and the goal ledger: the
//! command methods validate and return events without changing anything, `apply` folds an event
//! in, and replaying the log rebuilds the same registry. Group events are consequences of task
//! events (`task_completed`, `task_failed`, `worktree_removed`): [`Groups::follow_up`] derives them,
//! so no caller has to remember to record them.

use fleet_domain::error::{FleetError, Result};
use fleet_domain::events::DomainEvent;
use fleet_domain::ids::{show, validate_line, GoalId, GroupId, RunId, Sha, TaskId};
use fleet_domain::limits::MAX_TITLE;
use fleet_domain::sanitize::flatten_line;
use fleet_domain::status::{CloseReason, MemberOutcome};
use std::collections::{BTreeMap, BTreeSet};

pub const MIN_MEMBERS: usize = 2;
/// Also the number of tasks one goal may name, so a group's contract can govern all of them.
pub const MAX_MEMBERS: usize = 16;
/// Groups kept in memory; closed ones make room for new ones, oldest first.
pub const MAX_GROUPS: usize = 64;
/// Longest accepted group title (the member titles add the worker's name).
pub const MAX_GROUP_TITLE: usize = 150;

/// Where one member stands. `Winner` and `Superseded` are set by the pick, for every member at
/// once; `outcome` keeps how a superseded member's task had ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberState {
    /// The task has not ended (waiting, running, in review or blocked).
    Running,
    Completed,
    Failed,
    Winner,
    Superseded,
}

impl MemberState {
    pub fn as_str(self) -> &'static str {
        match self {
            MemberState::Running => "running",
            MemberState::Completed => "completed",
            MemberState::Failed => "failed",
            MemberState::Winner => "winner",
            MemberState::Superseded => "superseded",
        }
    }
}

impl std::fmt::Display for MemberState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupStatus {
    /// At least one member's task has not ended.
    Running,
    /// Every member ended; a winner can be picked.
    Finished,
    /// A winner was picked and merged; the others are superseded.
    Decided,
    Closed,
}

impl GroupStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            GroupStatus::Running => "running",
            GroupStatus::Finished => "finished",
            GroupStatus::Decided => "decided",
            GroupStatus::Closed => "closed",
        }
    }
}

impl std::fmt::Display for GroupStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub task: TaskId,
    pub state: MemberState,
    /// How the task ended, kept after a pick.
    pub outcome: Option<MemberOutcome>,
    /// Daemon time of the member's first dispatch.
    pub started_ms: Option<u64>,
    /// Daemon time the task last ended.
    pub finished_ms: Option<u64>,
}

impl Member {
    /// First dispatch to the last end, when both are known.
    pub fn duration_ms(&self) -> Option<u64> {
        match (self.started_ms, self.finished_ms) {
            (Some(s), Some(f)) => Some(f.saturating_sub(s)),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub id: GroupId,
    pub run: RunId,
    pub title: String,
    /// The goal that carries the members' shared verification contract.
    pub goal: Option<GoalId>,
    pub contract_id: Option<String>,
    pub members: Vec<Member>,
    pub winner: Option<TaskId>,
    pub merge_sha: Option<Sha>,
    /// `verified` or `unverified`: how the winner's evidence stood when it was picked.
    pub verification: Option<String>,
    pub closed: Option<CloseReason>,
    pub created_ms: u64,
}

impl Group {
    pub fn status(&self) -> GroupStatus {
        if self.closed.is_some() {
            GroupStatus::Closed
        } else if self.winner.is_some() {
            GroupStatus::Decided
        } else if self.members.iter().any(|m| m.state == MemberState::Running) {
            GroupStatus::Running
        } else {
            GroupStatus::Finished
        }
    }

    pub fn member(&self, task: &TaskId) -> Option<&Member> {
        self.members.iter().find(|m| &m.task == task)
    }

    fn member_mut(&mut self, task: &TaskId) -> Option<&mut Member> {
        self.members.iter_mut().find(|m| &m.task == task)
    }

    /// Whether `task` may be picked now, as an error that says why not.
    pub fn check_pick(&self, task: &TaskId) -> Result<()> {
        match self.status() {
            GroupStatus::Closed => {
                return Err(FleetError::conflict(format!(
                    "group {} is closed",
                    show(self.id.as_str())
                )))
            }
            GroupStatus::Decided => {
                let winner = self.winner.as_ref().map_or("?", TaskId::as_str);
                return Err(FleetError::conflict(format!(
                    "group {} has its winner already: {winner}",
                    show(self.id.as_str())
                )));
            }
            GroupStatus::Running => {
                let running = self
                    .members
                    .iter()
                    .filter(|m| m.state == MemberState::Running)
                    .map(|m| m.task.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(FleetError::conflict(format!(
                    "group {} still has members that have not ended: {running}; wait for them, or stop or cancel them",
                    show(self.id.as_str())
                )));
            }
            GroupStatus::Finished => {}
        }
        let member = self.member(task).ok_or_else(|| {
            FleetError::not_found(format!(
                "task {} is not a member of group {}",
                show(task.as_str()),
                show(self.id.as_str())
            ))
        })?;
        if member.state != MemberState::Completed {
            return Err(FleetError::conflict(format!(
                "member {task} is {}; only a member whose task completed can be picked",
                member.state
            )));
        }
        Ok(())
    }

    fn apply(&mut self, ts_ms: u64, ev: &DomainEvent) {
        match ev {
            DomainEvent::TaskDispatched { task, .. } => {
                if let Some(m) = self.member_mut(task) {
                    m.started_ms.get_or_insert(ts_ms);
                }
            }
            DomainEvent::MemberFinished { task, outcome, .. } => {
                if self.winner.is_some() {
                    return;
                }
                if let Some(m) = self.member_mut(task) {
                    if m.state == MemberState::Running {
                        m.state = match outcome {
                            MemberOutcome::Completed => MemberState::Completed,
                            MemberOutcome::Failed => MemberState::Failed,
                        };
                        m.outcome = Some(*outcome);
                        m.finished_ms = Some(ts_ms);
                    }
                }
            }
            DomainEvent::RetryReset { task, .. } => {
                if self.winner.is_some() {
                    return;
                }
                let reopened = match self.member_mut(task) {
                    Some(m) if m.state == MemberState::Failed => {
                        m.state = MemberState::Running;
                        m.outcome = None;
                        m.finished_ms = None;
                        true
                    }
                    _ => false,
                };
                // A group that was closed only because nothing completed has something to wait for
                // again.
                if reopened && self.closed == Some(CloseReason::NoCompletion) {
                    self.closed = None;
                }
            }
            DomainEvent::WinnerPicked {
                task,
                merge_sha,
                verification,
                ..
            } => {
                if self.winner.is_some() || self.closed.is_some() || self.member(task).is_none() {
                    return;
                }
                for m in &mut self.members {
                    m.state = if &m.task == task {
                        MemberState::Winner
                    } else {
                        MemberState::Superseded
                    };
                }
                self.winner = Some(task.clone());
                self.merge_sha = Some(merge_sha.clone());
                self.verification = Some(flatten_line(verification, 40));
            }
            DomainEvent::GroupClosed { reason, .. } if self.closed.is_none() => {
                self.closed = Some(*reason);
            }
            _ => {}
        }
    }
}

/// What a new group is made of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewGroup {
    pub id: Option<GroupId>,
    pub run: RunId,
    pub title: String,
    pub members: Vec<TaskId>,
    pub goal: Option<GoalId>,
    pub contract_id: Option<String>,
}

/// What the registry needs to know of the fleet to decide that a group is done with.
pub trait TaskView {
    /// The task still has a worktree (it was created and not removed).
    fn has_worktree(&self, run: &RunId, task: &TaskId) -> bool;
}

#[derive(Clone, Debug, Default)]
pub struct Groups {
    groups: BTreeMap<GroupId, Group>,
    /// Group ids in the order they were created.
    order: Vec<GroupId>,
    by_task: BTreeMap<(RunId, TaskId), GroupId>,
    /// Groups created so far, pruned ones included: generated ids never repeat.
    created: u64,
}

impl Groups {
    pub fn new() -> Groups {
        Groups::default()
    }

    pub fn len(&self) -> usize {
        self.groups.len()
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn find(&self, id: &GroupId) -> Option<&Group> {
        self.groups.get(id)
    }

    pub fn get(&self, id: &GroupId) -> Result<&Group> {
        self.groups.get(id).ok_or_else(|| {
            FleetError::not_found(format!("group {} does not exist", show(id.as_str())))
        })
    }

    /// Every group, in the order they were created.
    pub fn iter(&self) -> impl Iterator<Item = &Group> {
        self.order.iter().filter_map(|id| self.groups.get(id))
    }

    /// The group a task is a member of.
    pub fn group_of(&self, run: &RunId, task: &TaskId) -> Option<&Group> {
        self.by_task
            .get(&(run.clone(), task.clone()))
            .and_then(|g| self.groups.get(g))
    }

    /// The id a group gets when none is asked for: `fan<n>`, the first one that is free.
    pub fn next_id(&self) -> GroupId {
        let mut n = self.created;
        loop {
            n = n.saturating_add(1);
            let id = GroupId::from_seq(n);
            if !self.groups.contains_key(&id) {
                return id;
            }
        }
    }

    fn room(&self) -> bool {
        self.groups.len() < MAX_GROUPS
            || self
                .groups
                .values()
                .any(|g| g.closed == Some(CloseReason::Resolved))
    }

    // ---- commands --------------------------------------------------------------------------

    /// Validates a new group and returns the event that records it.
    pub fn create(&self, new: NewGroup) -> Result<(GroupId, DomainEvent)> {
        validate_line("group title", &new.title, MAX_GROUP_TITLE)?;
        if new.members.len() < MIN_MEMBERS || new.members.len() > MAX_MEMBERS {
            return Err(FleetError::invalid(format!(
                "a fan-out has {MIN_MEMBERS} to {MAX_MEMBERS} members, not {}",
                new.members.len()
            )));
        }
        let distinct: BTreeSet<&TaskId> = new.members.iter().collect();
        if distinct.len() != new.members.len() {
            return Err(FleetError::invalid("a group names the same task twice"));
        }
        for m in &new.members {
            if self.by_task.contains_key(&(new.run.clone(), m.clone())) {
                return Err(FleetError::conflict(format!(
                    "task {m} belongs to a group already"
                )));
            }
        }
        if !self.room() {
            return Err(FleetError::limit(format!(
                "{MAX_GROUPS} groups are open; resolve one (fleet pick, then fleet cleanup) first"
            )));
        }
        let id = match new.id {
            Some(id) if self.groups.contains_key(&id) => {
                return Err(FleetError::conflict(format!(
                    "group {} exists already",
                    show(id.as_str())
                )))
            }
            Some(id) => id,
            None => self.next_id(),
        };
        let event = DomainEvent::GroupCreated {
            group: id.clone(),
            run: new.run,
            title: new.title.trim().to_string(),
            members: new.members,
            goal: new.goal,
            contract_id: new.contract_id,
        };
        Ok((id, event))
    }

    /// The event that records the pick of `task`: every other member becomes superseded.
    pub fn pick_event(
        &self,
        group: &GroupId,
        task: &TaskId,
        merge_sha: Sha,
        verification: &str,
    ) -> Result<DomainEvent> {
        let g = self.get(group)?;
        g.check_pick(task)?;
        Ok(DomainEvent::WinnerPicked {
            group: group.clone(),
            task: task.clone(),
            merge_sha,
            verification: verification.to_string(),
            superseded: g
                .members
                .iter()
                .filter(|m| &m.task != task)
                .map(|m| m.task.clone())
                .collect(),
        })
    }

    /// The group events that follow from `events`, which were applied already: a member whose task
    /// ended, a group that has nothing left to wait for. Recording these may call for more
    /// (a member that finished may complete the group), so callers repeat until it is empty.
    pub fn follow_up(&self, events: &[DomainEvent], view: &dyn TaskView) -> Vec<DomainEvent> {
        let mut out = Vec::new();
        let mut finished: BTreeSet<(GroupId, TaskId)> = BTreeSet::new();
        let mut touched: BTreeSet<GroupId> = BTreeSet::new();
        for ev in events {
            match ev {
                DomainEvent::TaskCompleted { run, task }
                | DomainEvent::TaskFailed { run, task, .. } => {
                    let Some(g) = self.group_of(run, task) else {
                        continue;
                    };
                    let running = g
                        .member(task)
                        .is_some_and(|m| m.state == MemberState::Running);
                    if running
                        && g.winner.is_none()
                        && finished.insert((g.id.clone(), task.clone()))
                    {
                        let outcome = if matches!(ev, DomainEvent::TaskCompleted { .. }) {
                            MemberOutcome::Completed
                        } else {
                            MemberOutcome::Failed
                        };
                        out.push(DomainEvent::MemberFinished {
                            group: g.id.clone(),
                            task: task.clone(),
                            outcome,
                        });
                    }
                }
                DomainEvent::MemberFinished { group, .. }
                | DomainEvent::WinnerPicked { group, .. } => {
                    touched.insert(group.clone());
                }
                DomainEvent::WorktreeRemoved { run, task } => {
                    if let Some(g) = self.group_of(run, task) {
                        touched.insert(g.id.clone());
                    }
                }
                _ => {}
            }
        }
        for id in touched {
            let Some(g) = self.groups.get(&id) else {
                continue;
            };
            if g.closed.is_some() {
                continue;
            }
            let reason = if g.winner.is_some() {
                g.members
                    .iter()
                    .all(|m| !view.has_worktree(&g.run, &m.task))
                    .then_some(CloseReason::Resolved)
            } else {
                (g.status() == GroupStatus::Finished
                    && g.members.iter().all(|m| m.state == MemberState::Failed))
                .then_some(CloseReason::NoCompletion)
            };
            if let Some(reason) = reason {
                out.push(DomainEvent::GroupClosed { group: id, reason });
            }
        }
        out
    }

    // ---- projection ------------------------------------------------------------------------

    /// Folds one stored event in; `ts_ms` is the time it was recorded.
    pub fn apply(&mut self, ts_ms: u64, ev: &DomainEvent) {
        match ev {
            DomainEvent::GroupCreated {
                group,
                run,
                title,
                members,
                goal,
                contract_id,
            } => {
                let distinct: BTreeSet<&TaskId> = members.iter().collect();
                let fresh = !self.groups.contains_key(group)
                    && distinct.len() == members.len()
                    && (MIN_MEMBERS..=MAX_MEMBERS).contains(&members.len())
                    && members
                        .iter()
                        .all(|m| !self.by_task.contains_key(&(run.clone(), m.clone())));
                if !fresh {
                    return;
                }
                if self.groups.len() >= MAX_GROUPS && !self.prune_one() {
                    return;
                }
                self.created = self.created.saturating_add(1);
                for m in members {
                    self.by_task.insert((run.clone(), m.clone()), group.clone());
                }
                self.order.push(group.clone());
                self.groups.insert(
                    group.clone(),
                    Group {
                        id: group.clone(),
                        run: run.clone(),
                        title: flatten_line(title, MAX_TITLE),
                        goal: goal.clone(),
                        contract_id: contract_id.clone(),
                        members: members
                            .iter()
                            .map(|t| Member {
                                task: t.clone(),
                                state: MemberState::Running,
                                outcome: None,
                                started_ms: None,
                                finished_ms: None,
                            })
                            .collect(),
                        winner: None,
                        merge_sha: None,
                        verification: None,
                        closed: None,
                        created_ms: ts_ms,
                    },
                );
            }
            DomainEvent::TaskDispatched { run, task, .. }
            | DomainEvent::RetryReset { run, task } => {
                if let Some(id) = self.by_task.get(&(run.clone(), task.clone())).cloned() {
                    if let Some(g) = self.groups.get_mut(&id) {
                        g.apply(ts_ms, ev);
                    }
                }
            }
            DomainEvent::MemberFinished { group, .. }
            | DomainEvent::WinnerPicked { group, .. }
            | DomainEvent::GroupClosed { group, .. } => {
                if let Some(g) = self.groups.get_mut(group) {
                    g.apply(ts_ms, ev);
                }
            }
            _ => {}
        }
    }

    /// Forgets the oldest closed-and-resolved group. False when none can go.
    fn prune_one(&mut self) -> bool {
        let Some(victim) = self
            .order
            .iter()
            .find(|id| {
                self.groups
                    .get(*id)
                    .is_some_and(|g| g.closed == Some(CloseReason::Resolved))
            })
            .cloned()
        else {
            return false;
        };
        if let Some(g) = self.groups.remove(&victim) {
            for m in &g.members {
                self.by_task.remove(&(g.run.clone(), m.task.clone()));
            }
        }
        self.order.retain(|id| *id != victim);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> RunId {
        RunId::parse("r1").unwrap()
    }

    fn tid(s: &str) -> TaskId {
        TaskId::parse(s).unwrap()
    }

    fn gid(s: &str) -> GroupId {
        GroupId::parse(s).unwrap()
    }

    fn sha(c: char) -> Sha {
        Sha::parse(&c.to_string().repeat(40)).unwrap()
    }

    fn new_group(id: &str, members: &[&str]) -> NewGroup {
        NewGroup {
            id: Some(gid(id)),
            run: run(),
            title: "try three ways".to_string(),
            members: members.iter().map(|m| tid(m)).collect(),
            goal: None,
            contract_id: None,
        }
    }

    struct Worktrees(BTreeSet<String>);

    impl TaskView for Worktrees {
        fn has_worktree(&self, _run: &RunId, task: &TaskId) -> bool {
            self.0.contains(task.as_str())
        }
    }

    fn none() -> Worktrees {
        Worktrees(BTreeSet::new())
    }

    fn with(tasks: &[&str]) -> Worktrees {
        Worktrees(tasks.iter().map(|t| t.to_string()).collect())
    }

    fn registry(id: &str, members: &[&str]) -> Groups {
        let mut groups = Groups::new();
        let (_, ev) = groups.create(new_group(id, members)).unwrap();
        groups.apply(100, &ev);
        groups
    }

    fn completed(t: &str) -> DomainEvent {
        DomainEvent::TaskCompleted {
            run: run(),
            task: tid(t),
        }
    }

    fn failed(t: &str) -> DomainEvent {
        DomainEvent::TaskFailed {
            run: run(),
            task: tid(t),
            reason: "boom".into(),
        }
    }

    /// Applies the events that follow from `events` until nothing more follows; returns them all.
    fn settle(
        groups: &mut Groups,
        ts: u64,
        events: &[DomainEvent],
        view: &dyn TaskView,
    ) -> Vec<DomainEvent> {
        let mut all = Vec::new();
        let mut batch = events.to_vec();
        loop {
            let next = groups.follow_up(&batch, view);
            if next.is_empty() {
                return all;
            }
            for e in &next {
                groups.apply(ts, e);
            }
            all.extend(next.clone());
            batch = next;
        }
    }

    #[test]
    fn a_new_group_waits_for_its_members() {
        let g = registry("fan1", &["fan1-1", "fan1-2", "fan1-3"]);
        let group = g.get(&gid("fan1")).unwrap();
        assert_eq!(group.status(), GroupStatus::Running);
        assert_eq!(group.created_ms, 100);
        assert!(group.winner.is_none());
        assert_eq!(group.members.len(), 3);
        assert!(g.group_of(&run(), &tid("fan1-2")).is_some());
        assert!(g.group_of(&run(), &tid("other")).is_none());
    }

    #[test]
    fn the_group_finishes_when_every_member_ended() {
        let mut g = registry("fan1", &["a", "b"]);
        let ev = [completed("a")];
        let out = settle(&mut g, 200, &ev, &none());
        assert_eq!(
            out,
            vec![DomainEvent::MemberFinished {
                group: gid("fan1"),
                task: tid("a"),
                outcome: MemberOutcome::Completed
            }]
        );
        assert_eq!(g.get(&gid("fan1")).unwrap().status(), GroupStatus::Running);
        settle(&mut g, 300, &[failed("b")], &none());
        let group = g.get(&gid("fan1")).unwrap();
        assert_eq!(group.status(), GroupStatus::Finished);
        assert_eq!(group.member(&tid("b")).unwrap().state, MemberState::Failed);
        assert_eq!(group.member(&tid("a")).unwrap().finished_ms, Some(200));
    }

    #[test]
    fn a_group_in_which_nothing_completed_closes_itself() {
        let mut g = registry("fan1", &["a", "b"]);
        let out = settle(&mut g, 5, &[failed("a"), failed("b")], &none());
        assert_eq!(
            out.last(),
            Some(&DomainEvent::GroupClosed {
                group: gid("fan1"),
                reason: CloseReason::NoCompletion
            })
        );
        assert_eq!(g.get(&gid("fan1")).unwrap().status(), GroupStatus::Closed);
    }

    #[test]
    fn retrying_a_failed_member_reopens_a_group_that_closed_for_want_of_a_winner() {
        let mut g = registry("fan1", &["a", "b"]);
        settle(&mut g, 5, &[failed("a"), failed("b")], &none());
        g.apply(
            6,
            &DomainEvent::RetryReset {
                run: run(),
                task: tid("a"),
            },
        );
        let group = g.get(&gid("fan1")).unwrap();
        assert_eq!(group.status(), GroupStatus::Running);
        settle(&mut g, 9, &[completed("a")], &none());
        assert_eq!(g.get(&gid("fan1")).unwrap().status(), GroupStatus::Finished);
    }

    #[test]
    fn only_a_finished_group_can_be_picked_and_only_a_completed_member() {
        let mut g = registry("fan1", &["a", "b", "c"]);
        let early = g
            .get(&gid("fan1"))
            .unwrap()
            .check_pick(&tid("a"))
            .unwrap_err();
        assert!(
            early.message.contains("have not ended"),
            "{}",
            early.message
        );
        settle(
            &mut g,
            2,
            &[completed("a"), failed("b"), completed("c")],
            &none(),
        );
        let group = g.get(&gid("fan1")).unwrap();
        assert!(group.check_pick(&tid("a")).is_ok());
        let failed_member = group.check_pick(&tid("b")).unwrap_err();
        assert!(failed_member.message.contains("is failed"));
        let stranger = group.check_pick(&tid("zzz")).unwrap_err();
        assert!(stranger.message.contains("not a member"));
    }

    #[test]
    fn the_pick_supersedes_every_other_member_once() {
        let mut g = registry("fan1", &["a", "b", "c"]);
        settle(
            &mut g,
            2,
            &[completed("a"), failed("b"), completed("c")],
            &none(),
        );
        let ev = g
            .pick_event(&gid("fan1"), &tid("c"), sha('d'), "verified")
            .unwrap();
        assert_eq!(
            ev,
            DomainEvent::WinnerPicked {
                group: gid("fan1"),
                task: tid("c"),
                merge_sha: sha('d'),
                verification: "verified".into(),
                superseded: vec![tid("a"), tid("b")],
            }
        );
        g.apply(3, &ev);
        let group = g.get(&gid("fan1")).unwrap();
        assert_eq!(group.status(), GroupStatus::Decided);
        assert_eq!(group.winner, Some(tid("c")));
        assert_eq!(group.member(&tid("c")).unwrap().state, MemberState::Winner);
        let b = group.member(&tid("b")).unwrap();
        assert_eq!(b.state, MemberState::Superseded);
        assert_eq!(b.outcome, Some(MemberOutcome::Failed));
        // a second pick is refused, whoever it names
        for t in ["a", "c"] {
            let e = g
                .pick_event(&gid("fan1"), &tid(t), sha('e'), "verified")
                .unwrap_err();
            assert!(e.message.contains("winner already"), "{}", e.message);
        }
    }

    #[test]
    fn a_decided_group_closes_when_no_member_has_a_worktree_left() {
        let mut g = registry("fan1", &["a", "b"]);
        settle(&mut g, 2, &[completed("a"), completed("b")], &none());
        let ev = g
            .pick_event(&gid("fan1"), &tid("a"), sha('d'), "unverified")
            .unwrap();
        g.apply(3, &ev);
        let removed = |t: &str| DomainEvent::WorktreeRemoved {
            run: run(),
            task: tid(t),
        };
        // one worktree is still there: not closed yet
        let out = settle(&mut g, 4, &[ev.clone(), removed("a")], &with(&["b"]));
        assert!(out.is_empty());
        assert_eq!(g.get(&gid("fan1")).unwrap().status(), GroupStatus::Decided);
        let out = settle(&mut g, 5, &[removed("b")], &none());
        assert_eq!(
            out,
            vec![DomainEvent::GroupClosed {
                group: gid("fan1"),
                reason: CloseReason::Resolved
            }]
        );
        assert_eq!(g.get(&gid("fan1")).unwrap().status(), GroupStatus::Closed);
        // and nothing more is derived once it is closed
        assert!(settle(&mut g, 6, &[removed("b")], &none()).is_empty());
    }

    #[test]
    fn durations_run_from_the_first_dispatch_to_the_last_end() {
        let mut g = registry("fan1", &["a", "b"]);
        let dispatch = |t: &str| DomainEvent::TaskDispatched {
            run: run(),
            task: tid(t),
            attempt: 1,
            branch: format!("fleet/r1/{t}"),
            worktree: "/w".into(),
            pid: 1,
            start_token: "x".into(),
        };
        g.apply(1_000, &dispatch("a"));
        g.apply(1_500, &dispatch("a")); // a second attempt does not move the start
        settle(&mut g, 4_000, &[completed("a")], &none());
        let group = g.get(&gid("fan1")).unwrap();
        assert_eq!(group.member(&tid("a")).unwrap().duration_ms(), Some(3_000));
        assert_eq!(group.member(&tid("b")).unwrap().duration_ms(), None);
    }

    #[test]
    fn creation_is_validated() {
        let g = Groups::new();
        let bad = |mut n: NewGroup, f: &dyn Fn(&mut NewGroup)| {
            f(&mut n);
            g.create(n).unwrap_err().message
        };
        let base = || new_group("fan1", &["a", "b"]);
        assert!(g.create(base()).is_ok());
        assert!(bad(base(), &|n| n.members.truncate(1)).contains("2 to 16"));
        assert!(bad(base(), &|n| n.members =
            (0..17).map(|i| tid(&format!("m{i}"))).collect())
        .contains("2 to 16"));
        assert!(bad(base(), &|n| n.members = vec![tid("a"), tid("a")]).contains("twice"));
        assert!(bad(base(), &|n| n.title = "two\nlines".into()).contains("control"));
        assert!(bad(base(), &|n| n.title = " ".into()).contains("empty"));
        assert!(bad(base(), &|n| n.title = "x".repeat(MAX_GROUP_TITLE + 1)).contains("longer"));
    }

    #[test]
    fn ids_are_unique_and_a_task_belongs_to_one_group() {
        let g = registry("fan1", &["a", "b"]);
        let dup_id = g.create(new_group("fan1", &["c", "d"])).unwrap_err();
        assert!(dup_id.message.contains("exists already"));
        let shared = g.create(new_group("fan2", &["b", "c"])).unwrap_err();
        assert!(shared.message.contains("belongs to a group already"));
        let mut other_run = new_group("fan2", &["a", "b"]);
        other_run.run = RunId::parse("r2").unwrap();
        assert!(
            g.create(other_run).is_ok(),
            "the same task id in another run is another task"
        );
        // generated ids skip what exists and never repeat
        assert_eq!(g.next_id(), gid("fan2"));
        let mut n = new_group("fan2", &["c", "d"]);
        n.id = None;
        let (id, _) = g.create(n).unwrap();
        assert_eq!(id, gid("fan2"));
    }

    #[test]
    fn replay_rebuilds_the_same_registry_and_ignores_impossible_events() {
        // what the live daemon records: task events and the group events that follow from them
        let mut live = Groups::new();
        let mut recorded: Vec<(u64, DomainEvent)> = Vec::new();
        let (_, created) = live.create(new_group("fan1", &["a", "b"])).unwrap();
        for (ts, e) in [(1, created), (2, completed("a")), (3, completed("b"))] {
            live.apply(ts, &e);
            recorded.push((ts, e.clone()));
            for f in live.follow_up(std::slice::from_ref(&e), &none()) {
                live.apply(ts, &f);
                recorded.push((ts, f));
            }
        }
        let pick = live
            .pick_event(&gid("fan1"), &tid("a"), sha('f'), "verified")
            .unwrap();
        live.apply(9, &pick);
        recorded.push((9, pick));
        // impossible events change nothing: a pick for a stranger, a group that does not exist
        recorded.push((
            10,
            DomainEvent::WinnerPicked {
                group: gid("fan1"),
                task: tid("zzz"),
                merge_sha: sha('0'),
                verification: "x".into(),
                superseded: vec![],
            },
        ));
        recorded.push((
            11,
            DomainEvent::MemberFinished {
                group: gid("nope"),
                task: tid("a"),
                outcome: MemberOutcome::Failed,
            },
        ));
        let mut replayed = Groups::new();
        for (ts, e) in &recorded {
            replayed.apply(*ts, e);
        }
        let a = live.get(&gid("fan1")).unwrap();
        let b = replayed.get(&gid("fan1")).unwrap();
        assert_eq!(a, b);
        assert_eq!(b.winner, Some(tid("a")));
    }

    #[test]
    fn a_damaged_creation_event_is_ignored() {
        let mut g = Groups::new();
        let ev = |members: Vec<TaskId>| DomainEvent::GroupCreated {
            group: gid("fan1"),
            run: run(),
            title: "t".into(),
            members,
            goal: None,
            contract_id: None,
        };
        g.apply(1, &ev(vec![tid("a")]));
        g.apply(1, &ev(vec![tid("a"), tid("a")]));
        assert!(g.is_empty());
        g.apply(1, &ev(vec![tid("a"), tid("b")]));
        g.apply(2, &ev(vec![tid("c"), tid("d")])); // same id again
        assert_eq!(g.len(), 1);
    }

    #[test]
    fn titles_from_the_log_cannot_break_a_line() {
        let mut g = Groups::new();
        g.apply(
            1,
            &DomainEvent::GroupCreated {
                group: gid("fan1"),
                run: run(),
                title: "a\n## forged\u{1b}[31m".into(),
                members: vec![tid("a"), tid("b")],
                goal: None,
                contract_id: None,
            },
        );
        let title = &g.get(&gid("fan1")).unwrap().title;
        assert!(
            !title.contains('\n') && !title.contains('\u{1b}'),
            "{title:?}"
        );
    }

    #[test]
    fn closed_groups_make_room_and_open_ones_do_not() {
        let mut g = Groups::new();
        for i in 0..MAX_GROUPS {
            let a = format!("a{i}");
            let b = format!("b{i}");
            let (_, ev) = g
                .create(new_group(&format!("g{i}"), &[a.as_str(), b.as_str()]))
                .unwrap();
            g.apply(i as u64, &ev);
        }
        assert_eq!(g.len(), MAX_GROUPS);
        let full = g.create(new_group("extra", &["x", "y"])).unwrap_err();
        assert!(full.message.contains("groups are open"), "{}", full.message);
        // resolve the first one
        g.apply(
            1_000,
            &DomainEvent::MemberFinished {
                group: gid("g0"),
                task: tid("a0"),
                outcome: MemberOutcome::Completed,
            },
        );
        g.apply(
            1_000,
            &DomainEvent::MemberFinished {
                group: gid("g0"),
                task: tid("b0"),
                outcome: MemberOutcome::Completed,
            },
        );
        let pick = g
            .pick_event(&gid("g0"), &tid("a0"), sha('a'), "verified")
            .unwrap();
        g.apply(1_001, &pick);
        g.apply(
            1_002,
            &DomainEvent::GroupClosed {
                group: gid("g0"),
                reason: CloseReason::Resolved,
            },
        );
        let (_, ev) = g.create(new_group("extra", &["x", "y"])).unwrap();
        g.apply(1_003, &ev);
        assert_eq!(g.len(), MAX_GROUPS);
        assert!(g.find(&gid("g0")).is_none());
        assert!(g.find(&gid("extra")).is_some());
        assert!(g.group_of(&run(), &tid("a0")).is_none());
    }
}
