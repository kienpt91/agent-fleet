//! Fan-out use cases: one prompt given to several agents, their results compared, one picked.
//!
//! The registry of groups is a projection of the event log, like the goal ledger: `Orchestrator::commit`
//! folds every stored event into it and derives the group events that follow from task events
//! (a member that ended, a group with nothing left to wait for), and a restart rebuilds it by replay.
//! The use cases here validate a request against the domain, record events, and read the facts a
//! comparison is made of from the places they live: the task state, the evidence in the ledger,
//! and the repository (through the `Vcs` port, at the commit review recorded).
//!
//! * `create`: `fleet fanout` makes the sibling tasks, the goal that carries their shared contract
//!   and the group in one atomic batch of events, so no member exists without the others.
//! * `compare`: `fleet compare`, read only: facts in, the structured comparison out.
//! * `pick`: `fleet pick`, the only way a group's result reaches the integration branch: the normal
//!   merge approval, bound to the exact action, then the other members are superseded.
//! * `views`: every text of a member (titles, summaries, paths) is untrusted and goes out sanitized.

mod compare;
mod create;
mod pick;
mod views;

use fleet_domain::fleet::FleetState;
use fleet_domain::ids::{RunId, Sha, TaskId};
use fleet_domain::status::{MemberOutcome, TaskStatus};
use fleet_fanout::diff::DiffStat;
use fleet_fanout::group::{Groups, TaskView};
use std::collections::VecDeque;

/// Diffs kept in memory: a commit never changes, so what it changed relative to a base is cached.
const DIFF_CACHE: usize = 64;

/// The registry of groups and what is cached for it.
#[derive(Default)]
pub(crate) struct FanoutState {
    pub(crate) groups: Groups,
    diffs: VecDeque<((String, Sha, Sha), DiffStat)>,
}

impl FanoutState {
    pub(crate) fn cached_diff(&self, repo: &str, base: &Sha, head: &Sha) -> Option<DiffStat> {
        self.diffs
            .iter()
            .find(|((r, b, h), _)| r == repo && b == base && h == head)
            .map(|(_, d)| d.clone())
    }

    pub(crate) fn remember_diff(&mut self, repo: &str, base: &Sha, head: &Sha, stat: &DiffStat) {
        self.diffs
            .push_back(((repo.to_string(), base.clone(), head.clone()), stat.clone()));
        while self.diffs.len() > DIFF_CACHE {
            self.diffs.pop_front();
        }
    }
}

/// Whether tasks still have their worktrees, for deciding that a group is done with.
pub(crate) struct Worktrees<'a>(pub(crate) &'a FleetState);

impl TaskView for Worktrees<'_> {
    fn has_worktree(&self, run: &RunId, task: &TaskId) -> bool {
        self.0
            .run(run)
            .ok()
            .and_then(|r| r.task(task))
            .is_some_and(|t| t.worktree.is_some())
    }

    fn outcome(&self, run: &RunId, task: &TaskId) -> Option<MemberOutcome> {
        match self.0.run(run).ok()?.task(task)?.status {
            TaskStatus::Completed => Some(MemberOutcome::Completed),
            TaskStatus::Failed => Some(MemberOutcome::Failed),
            _ => None,
        }
    }
}

/// How a merge differs when it is the pick of a fan-out group.
pub(crate) struct MergeVariant<'a> {
    /// The commit the task's review recorded: the branch must still point at it.
    pub expected_head: &'a Sha,
    /// The exact action the approval is bound to, from (integration branch, task branch, head).
    pub action: &'a dyn Fn(&str, &str, &Sha) -> fleet_domain::approval::Action,
}

/// What a merge of a task into its run's integration branch produced.
pub(crate) struct MergedTask {
    pub branch: String,
    pub integration: String,
    pub sha: Sha,
}

#[cfg(test)]
mod tests {
    use crate::api::{is_read_only, parse_command, Command};
    use fleet_domain::error::ErrorCode;
    use fleet_domain::value::Value;

    fn args(pairs: Vec<(&str, Value)>) -> Value {
        Value::object(pairs)
    }

    fn s(x: &str) -> Value {
        Value::str(x)
    }

    fn list(items: &[&str]) -> Value {
        Value::array(items.iter().map(|i| Value::str(*i)))
    }

    #[test]
    fn the_group_commands_parse_and_only_the_views_are_read_only() {
        let fanout = parse_command(
            "group.fanout",
            &args(vec![
                ("run", s("r1")),
                ("title", s("t")),
                ("spec", s("do it")),
                ("workers", list(&["a", "a", "b"])),
                ("count", Value::int(2u32)),
            ]),
        )
        .unwrap();
        let Command::Fanout(f) = &fanout else {
            panic!("{fanout:?}")
        };
        assert_eq!(f.workers.len(), 3);
        assert_eq!(f.count, 2);
        assert!(f.id.is_none() && f.contract.is_none());
        assert!(!is_read_only(&fanout));

        let list_cmd = parse_command("group.list", &Value::Null).unwrap();
        let show = parse_command("group.show", &args(vec![("group", s("fan1"))])).unwrap();
        let compare = parse_command(
            "group.compare",
            &args(vec![
                ("group", s("fan1")),
                ("rank", list(&["diff", "time"])),
            ]),
        )
        .unwrap();
        let plain = parse_command("group.compare", &args(vec![("group", s("fan1"))])).unwrap();
        assert!(matches!(&compare, Command::GroupCompare { rank: Some(r), .. } if r.len() == 2));
        assert!(matches!(&plain, Command::GroupCompare { rank: None, .. }));
        for c in [&list_cmd, &show, &compare, &plain] {
            assert!(is_read_only(c), "{c:?}");
        }

        let pick = parse_command(
            "group.pick",
            &args(vec![
                ("group", s("fan1")),
                ("task", s("fan1-2")),
                ("allow_unverified", Value::Bool(true)),
            ]),
        )
        .unwrap();
        assert!(matches!(
            &pick,
            Command::GroupPick {
                allow_unverified: true,
                remove_others: false,
                ..
            }
        ));
        assert!(!is_read_only(&pick), "a pick merges");
    }

    #[test]
    fn malformed_group_requests_are_invalid_not_guessed() {
        let bad = |cmd: &str, a: Value| parse_command(cmd, &a).unwrap_err().code;
        assert_eq!(bad("group.show", args(vec![])), ErrorCode::Invalid);
        assert_eq!(
            bad("group.show", args(vec![("group", s("Not Valid"))])),
            ErrorCode::Invalid
        );
        assert_eq!(
            bad("group.pick", args(vec![("group", s("fan1"))])),
            ErrorCode::Invalid
        );
        assert_eq!(
            bad(
                "group.pick",
                args(vec![
                    ("group", s("fan1")),
                    ("task", s("t")),
                    ("allow_unverified", s("yes")),
                ])
            ),
            ErrorCode::Invalid
        );
        assert_eq!(
            bad(
                "group.compare",
                args(vec![("group", s("fan1")), ("rank", s("diff"))])
            ),
            ErrorCode::Invalid
        );
        let fanout = |extra: Vec<(&str, Value)>| {
            let mut pairs = vec![
                ("run", s("r1")),
                ("title", s("t")),
                ("spec", s("x")),
                ("workers", list(&["a", "b"])),
            ];
            pairs.extend(extra);
            args(pairs)
        };
        assert_eq!(
            bad("group.fanout", fanout(vec![("count", s("two"))])),
            ErrorCode::Invalid
        );
        assert_eq!(
            bad("group.fanout", fanout(vec![("workers", s("a,b"))])),
            ErrorCode::Invalid
        );
        assert_eq!(
            bad("group.fanout", fanout(vec![("contract", s("{}"))])),
            ErrorCode::Invalid
        );
        assert_eq!(
            bad("group.fanout", fanout(vec![("id", s("A/B"))])),
            ErrorCode::Invalid
        );
    }
}
