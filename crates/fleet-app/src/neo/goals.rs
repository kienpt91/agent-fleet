//! Goal ledger commands: add, attach criteria, change status, and the read views.

use super::{data, views};
use crate::api::{NewGoalArgs, Reply};
use crate::orchestrator::Orchestrator;
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::GoalId;
use fleet_domain::redact::Redactor;
use fleet_domain::status::GoalStatus;
use fleet_domain::value::{Document, Value};
use neo_goals::goal::{AttachedContract, NewGoal, TaskRef};
use neo_verify::contract::VerificationContract;

impl Orchestrator {
    /// Validates a contract document and gives it its id (the SHA-256 of its canonical text).
    fn attached_contract(&self, doc: &Document) -> Result<AttachedContract> {
        let contract = VerificationContract::from_value(&doc.0)
            .map_err(|e| FleetError::invalid(format!("invalid verification contract: {e}")))?;
        // The contract text is stored in the audit log and shown by `fleet goals show`: a secret
        // in it would be kept forever. Anything the redaction rules would change (key shapes,
        // tokens, the values of secret-looking variables of the daemon's environment) is refused.
        let mut redactor = Redactor::builtin();
        redactor.add_env_values(self.cfg.parent_env.iter().cloned());
        let text = contract.canonical();
        if text.lines().any(|l| redactor.redact(l) != l) {
            return Err(FleetError::invalid(
                "the verification contract contains something that looks like a secret (a key, a token or the value of a secret variable); contracts are stored in the audit log and shown to everyone who can run `fleet goals show`, so pass secrets through the environment of the command instead",
            ));
        }
        Ok(AttachedContract {
            id: contract.id(self.ports.digest.as_ref()),
            contract,
        })
    }

    pub(crate) fn goal_add(&mut self, args: NewGoalArgs) -> Result<Reply> {
        let mut tasks = Vec::with_capacity(args.tasks.len());
        for reference in &args.tasks {
            let (run, task) = self.state.find_task(reference)?;
            tasks.push(TaskRef::new(run, task));
        }
        let contract = args
            .contract
            .as_ref()
            .map(|d| self.attached_contract(d))
            .transpose()?;
        let (id, events) = self.ledger.add_goal(NewGoal {
            id: args.id,
            parent: args.parent,
            title: args.title,
            intent: args.intent,
            tasks,
            contract,
        })?;
        self.commit(events)?;
        let goal = self.ledger.goal(&id)?;
        Ok(data(
            "fleet.goals.add/1",
            vec![
                ("id", Value::str(id.as_str())),
                ("status", Value::str(goal.status.as_str())),
                (
                    "contract_id",
                    goal.contract
                        .as_ref()
                        .map_or(Value::Null, |c| Value::str(c.id.as_str())),
                ),
            ],
        ))
    }

    pub(crate) fn goal_attach(&mut self, goal: &GoalId, doc: &Document) -> Result<Reply> {
        let contract = self.attached_contract(doc)?;
        let events = self.ledger.attach_criteria(goal, &contract)?;
        self.commit(events)?;
        Ok(data(
            "fleet.goals.attach/1",
            vec![
                ("id", Value::str(goal.as_str())),
                ("contract_id", Value::str(contract.id.as_str())),
            ],
        ))
    }

    pub(crate) fn goal_set_status(
        &mut self,
        goal: &GoalId,
        status: GoalStatus,
        reason: &str,
    ) -> Result<Reply> {
        let events = self.ledger.set_status(goal, status, reason)?;
        self.commit(events)?;
        let now = self.ledger.goal(goal)?.status;
        Ok(data(
            "fleet.goals.status/1",
            vec![
                ("id", Value::str(goal.as_str())),
                ("status", Value::str(now.as_str())),
            ],
        ))
    }

    pub(crate) fn goal_list(&self) -> Reply {
        Reply::Data(views::goal_list(&self.state, &self.ledger))
    }

    pub(crate) fn goal_show(&self, goal: &GoalId) -> Result<Reply> {
        let g = self.ledger.goal(goal)?;
        Ok(Reply::Data(views::goal_show(&self.state, &self.ledger, g)))
    }

    pub(crate) fn goal_remaining(&self) -> Reply {
        Reply::Data(views::remaining(&self.state, &self.ledger))
    }
}
