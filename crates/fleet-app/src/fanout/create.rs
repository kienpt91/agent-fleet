//! `fleet fanout`: the member tasks, the goal that carries their contract and the group, recorded
//! as one batch of events.

use crate::api::{NewFanoutArgs, Reply};
use crate::neo::data;
use crate::orchestrator::Orchestrator;
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::{show, GoalId, TaskId, WorkerName};
use fleet_domain::run::NewTask;
use fleet_domain::value::Value;
use fleet_fanout::group::{NewGroup, MAX_MEMBERS, MIN_MEMBERS};
use neo_goals::goal::{NewGoal, TaskRef};

impl Orchestrator {
    /// Creates the group. Everything is validated first (the run, the workers, the contract, the
    /// sizes) and then recorded in one batch, so a request that fails creates nothing and a store
    /// that fails leaves nothing behind. The members are ordinary tasks: the run's `max_parallel`
    /// and the daemon's session limit decide how many of them run at once.
    pub(crate) fn fanout(&mut self, args: NewFanoutArgs) -> Result<Reply> {
        if args.spec.trim().is_empty() {
            return Err(FleetError::invalid("a fan-out needs a prompt (--spec)"));
        }
        if args.count == 0 {
            return Err(FleetError::invalid("--count must be at least 1"));
        }
        // before multiplying: a huge count must not become a huge allocation
        let size = usize::try_from(args.count)
            .ok()
            .and_then(|c| c.checked_mul(args.workers.len()))
            .unwrap_or(usize::MAX);
        if !(MIN_MEMBERS..=MAX_MEMBERS).contains(&size) {
            return Err(FleetError::invalid(format!(
                "a fan-out has {MIN_MEMBERS} to {MAX_MEMBERS} members; {} worker(s) x {} make {}",
                args.workers.len(),
                args.count,
                if size == usize::MAX {
                    "too many".to_string()
                } else {
                    size.to_string()
                }
            )));
        }
        for w in &args.workers {
            if !self.cfg.workers.contains_key(w) {
                let known: Vec<&str> = self.cfg.workers.keys().map(WorkerName::as_str).collect();
                return Err(FleetError::invalid(format!(
                    "worker {} is not defined in fleet.toml (defined: {})",
                    show(w.as_str()),
                    known.join(", ")
                )));
            }
        }
        let group = match args.id.clone() {
            Some(id) => id,
            None => self.fanout.groups.next_id(),
        };
        let mut members: Vec<(TaskId, WorkerName, String)> = Vec::with_capacity(size);
        let mut per_worker: std::collections::BTreeMap<&str, u32> =
            std::collections::BTreeMap::new();
        for worker in &args.workers {
            for _ in 0..args.count {
                let k = per_worker.entry(worker.as_str()).or_insert(0);
                *k += 1;
                let task = TaskId::parse(&format!("{group}-{}", members.len() + 1))?;
                members.push((
                    task,
                    worker.clone(),
                    format!("{} [{} #{k}]", args.title.trim(), worker),
                ));
            }
        }
        let tasks: Vec<NewTask> = members
            .iter()
            .map(|(id, worker, title)| NewTask {
                id: Some(id.clone()),
                title: title.clone(),
                spec: args.spec.clone(),
                deps: Vec::new(),
                worker: Some(worker.clone()),
                host: None,
            })
            .collect();
        let (_, mut events) = self.state.add_tasks(&args.run, tasks)?;
        let (goal, contract_id) = match &args.contract {
            None => (None, None),
            Some(doc) => {
                let attached = self.attached_contract(doc)?;
                let (goal, goal_events) = self.ledger.add_goal(NewGoal {
                    id: Some(GoalId::parse(&format!("fanout-{group}"))?),
                    parent: None,
                    title: format!("fan-out {group}: {}", args.title.trim()),
                    intent: format!(
                        "The same prompt was given to {size} agents; this contract judges each member's own commit. \
                         The goal is verified only when every member passed: fleet pick chooses one of them."
                    ),
                    tasks: members
                        .iter()
                        .map(|(t, _, _)| TaskRef::new(args.run.clone(), t.clone()))
                        .collect(),
                    contract: Some(attached.clone()),
                })?;
                events.extend(goal_events);
                (Some(goal), Some(attached.id.as_str().to_string()))
            }
        };
        let (id, created) = self.fanout.groups.create(NewGroup {
            id: Some(group),
            run: args.run.clone(),
            title: args.title,
            members: members.iter().map(|(t, _, _)| t.clone()).collect(),
            goal: goal.clone(),
            contract_id: contract_id.clone(),
        })?;
        events.push(created);
        self.commit(events)?;
        self.promote()?;
        Ok(data(
            "fleet.group.fanout/1",
            vec![
                ("group", Value::str(id.as_str())),
                ("run", Value::str(args.run.as_str())),
                (
                    "goal",
                    goal.as_ref()
                        .map_or(Value::Null, |g| Value::str(g.as_str())),
                ),
                ("contract_id", contract_id.map_or(Value::Null, Value::Str)),
                (
                    "members",
                    Value::array(members.iter().map(|(t, w, title)| {
                        Value::object([
                            ("task", Value::str(t.as_str())),
                            ("worker", Value::str(w.as_str())),
                            ("title", Value::str(title)),
                        ])
                    })),
                ),
            ],
        ))
    }
}
