//! The goal entity and the values that describe it.

use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::{EvidenceId, GoalId, RunId, TaskId};
use fleet_domain::status::GoalStatus;
use neo_verify::contract::{ContractId, VerificationContract};

/// A task of a run, named the way people write it: `run/task`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskRef {
    pub run: RunId,
    pub task: TaskId,
}

impl TaskRef {
    pub fn new(run: RunId, task: TaskId) -> TaskRef {
        TaskRef { run, task }
    }

    /// Parses `run/task`. A bare task id is not enough here: a goal must name its task exactly.
    pub fn parse(s: &str) -> Result<TaskRef> {
        let Some((run, task)) = s.split_once('/') else {
            return Err(FleetError::invalid(format!(
                "task reference {} must be written <run>/<task>",
                fleet_domain::ids::show(s)
            )));
        };
        Ok(TaskRef {
            run: RunId::parse(run)?,
            task: TaskId::parse(task)?,
        })
    }
}

impl std::fmt::Display for TaskRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.run, self.task)
    }
}

/// The acceptance criteria of a goal: one contract and the id it is known by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachedContract {
    pub id: ContractId,
    pub contract: VerificationContract,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Goal {
    pub id: GoalId,
    pub parent: Option<GoalId>,
    pub title: String,
    pub intent: String,
    pub status: GoalStatus,
    /// Why the status is what it is, as last recorded (an operator's reason or the ledger's).
    pub reason: String,
    /// Tasks that do the work of this goal.
    pub tasks: Vec<TaskRef>,
    pub children: Vec<GoalId>,
    /// 1 for a root goal.
    pub depth: usize,
    pub contract: Option<AttachedContract>,
    /// Evidence recorded for this goal, oldest first.
    pub evidence: Vec<EvidenceId>,
}

/// A goal to be added.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewGoal {
    /// Chosen by the caller, or `None` for the next free `goal-<n>`.
    pub id: Option<GoalId>,
    pub parent: Option<GoalId>,
    pub title: String,
    pub intent: String,
    pub tasks: Vec<TaskRef>,
    pub contract: Option<AttachedContract>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_refs_need_both_parts() {
        let r = TaskRef::parse("r1/t2").unwrap();
        assert_eq!(r.to_string(), "r1/t2");
        assert_eq!(TaskRef::new(r.run.clone(), r.task.clone()), r);
        for bad in ["t2", "r1/", "/t2", "r1/t2/t3", "R1/t", "r1/../x"] {
            assert!(TaskRef::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn task_ref_errors_quote_hostile_text() {
        let msg = TaskRef::parse("\u{1b}[2J").unwrap_err().message;
        assert!(!msg.contains('\u{1b}'));
    }
}
