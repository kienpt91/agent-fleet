//! Verification: running the contracts of goals independently of the worker and recording what
//! came out as evidence.
//!
//! The rule this module enforces: a task that a goal with a contract works on completes only
//! after fleet's own run of every such contract passed every mandatory check. During review the
//! contracts run first, on a clean export of the commit the review recorded (never in the live
//! worktree: the adapter exports it); a pass lets the review go on to the quality gates and then
//! complete; a failure is a failed gate, with the failing checks quoted in the next attempt's
//! brief. What the worker reported plays no part in the decision, and evidence counts only for
//! the commit that was exported.

use super::data;
use crate::api::Reply;
use crate::orchestrator::{Key, Orchestrator};
use crate::ports::{Job, VerifiedContract, VerifyDone, VerifyJob, VerifyOrigin, VerifyTarget};
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::ids::{show, GoalId, RunId, TaskId};
use fleet_domain::limits::MAX_CONCURRENT_JOBS;
use fleet_domain::quality::GateOutcome;
use fleet_domain::sanitize::flatten_line;
use fleet_domain::status::{TaskStatus, Verdict};
use fleet_domain::value::Value;
use neo_goals::goal::TaskRef;
use neo_goals::ledger::TaskContract;
use neo_verify::evidence::EvidenceScope;
use neo_verify::limits::MAX_CONCURRENT_VERIFICATIONS;

/// A verification job in flight.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Verifying {
    /// Numbers this run for the person who started it (`fleet verify` asks for its own result).
    pub ticket: u64,
}

/// What became of one run, kept (for the last few runs) so that the person who started it hears
/// about *their* run, not about the ledger's state from an earlier one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerifyOutcome {
    pub key: Key,
    pub ticket: u64,
    /// The evidence of every contract of the run was recorded.
    pub recorded: bool,
    /// Every mandatory check passed, on the commit the review recorded, with no uncommitted
    /// changes in the worktree, and the evidence was recorded.
    pub passed: bool,
    /// Why the run does not count, when it does not.
    pub note: String,
}

/// Outcomes kept.
pub(crate) const OUTCOMES_KEPT: usize = 64;

impl Orchestrator {
    /// The contracts that stand between `run/task` and completion.
    pub(crate) fn verification_targets(&self, run: &RunId, task: &TaskId) -> Vec<VerifyTarget> {
        self.ledger
            .governing(run, task)
            .into_iter()
            .filter_map(|g| {
                g.contract.as_ref().map(|c| VerifyTarget {
                    goal: g.id.clone(),
                    contract_id: c.id.clone(),
                    contract: c.contract.clone(),
                })
            })
            .collect()
    }

    /// Starts a verification job for a task and returns its ticket. At most one runs per task and
    /// at most `MAX_CONCURRENT_VERIFICATIONS` at a time: a hung command cannot take every job
    /// slot of the daemon.
    pub(crate) fn start_verification(
        &mut self,
        key: &Key,
        attempt: u32,
        origin: VerifyOrigin,
        worktree: &str,
        targets: Vec<VerifyTarget>,
    ) -> Result<u64> {
        if self.verifying.contains_key(key) {
            return Err(FleetError::conflict(format!(
                "{}/{} is being verified already",
                key.0, key.1
            )));
        }
        if self.verifying.len() >= MAX_CONCURRENT_VERIFICATIONS
            || self.ports.jobs.running() >= MAX_CONCURRENT_JOBS
        {
            return Err(FleetError::limit(
                "too many verifications or jobs are running; try again later",
            ));
        }
        let base = self.state.run(&key.0)?.base_sha.clone();
        let state = self.ports.vcs.worktree_state(worktree, &base)?;
        if origin == VerifyOrigin::Review {
            // What is verified is what will be merged: the head the review recorded is the one
            // the worktree has now, even if the worker committed in it meanwhile.
            let recorded = self
                .state
                .run(&key.0)
                .ok()
                .and_then(|r| r.task(&key.1))
                .and_then(|t| t.head.clone());
            if recorded.as_ref() != Some(&state.head) {
                let ev = self
                    .state
                    .run(&key.0)
                    .and_then(|r| r.record_commit(&key.1, state.head.clone()))?;
                self.commit(vec![ev])?;
            }
        }
        // The operator's allowlist, nothing else: the adapter gives the commands a home, a
        // temporary directory and a working directory of their own.
        let env = self.cfg.env.build(&self.cfg.parent_env, &[], &[], &[]);
        self.ports.jobs.start(Job::Verify(Box::new(VerifyJob {
            run: key.0.clone(),
            task: key.1.clone(),
            attempt,
            origin,
            worktree: worktree.to_string(),
            state,
            env,
            targets,
        })))?;
        self.verify_seq = self.verify_seq.saturating_add(1);
        let ticket = self.verify_seq;
        self.verifying.insert(key.clone(), Verifying { ticket });
        Ok(ticket)
    }

    /// One goal of a result as gate outcomes, mandatory checks only (optional checks are
    /// recorded as evidence but never judge the task).
    fn outcomes_of(&self, result: &VerifiedContract) -> Vec<GateOutcome> {
        let Some(contract) = self
            .ledger
            .find(&result.goal)
            .and_then(|g| g.contract.as_ref())
        else {
            return Vec::new();
        };
        result
            .reports
            .iter()
            .filter(|r| {
                contract
                    .contract
                    .check(&r.check)
                    .is_some_and(|c| c.mandatory)
            })
            .map(|r| GateOutcome {
                name: format!("verify {}/{}", result.goal, r.check),
                passed: r.verdict == Verdict::Passed,
                summary: r.summary.clone(),
                output: r.output.clone(),
            })
            .collect()
    }

    fn remember_outcome(&mut self, outcome: VerifyOutcome) {
        self.verify_outcomes.push_back(outcome);
        while self.verify_outcomes.len() > OUTCOMES_KEPT {
            self.verify_outcomes.pop_front();
        }
    }

    /// Records the results of a verification job as evidence and, for a review, decides whether
    /// the task may go on.
    pub(crate) fn handle_verify_done(&mut self, done: VerifyDone) {
        let key: Key = (done.run.clone(), done.task.clone());
        let ticket = self.verifying.remove(&key).map_or(0, |v| v.ticket);
        let mut recorded = true;
        let mut cannot_record: Option<String> = None;
        for result in &done.results {
            let scope = EvidenceScope {
                goal: result.goal.clone(),
                run: done.run.clone(),
                task: done.task.clone(),
                contract: result.contract_id.clone(),
                commit: done.commit.clone(),
                tree: done.tree.clone(),
                dirty: done.dirty,
            };
            let now = self.now();
            let outcome = self
                .ledger
                .record_evidence(&scope, result.reports.clone(), now)
                .and_then(|events| self.commit(events));
            if let Err(e) = outcome {
                recorded = false;
                if e.code == ErrorCode::Limit {
                    cannot_record = Some(e.message.clone());
                }
                self.warn(format!(
                    "{}/{}: verification of goal {} not recorded: {}",
                    key.0, key.1, result.goal, e.message
                ));
            }
        }
        // Does this run count? Every mandatory check of every contract passed, on the commit the
        // review recorded for the task, and nothing was uncommitted behind it.
        let head = self.ledger.head_of(&key.0, &key.1).cloned();
        let on_the_reviewed_commit = head.as_ref() == Some(&done.commit);
        let all_passed = done.results.iter().all(|r| {
            let mandatory = self
                .ledger
                .find(&r.goal)
                .and_then(|g| g.contract.as_ref())
                .map(|c| {
                    c.contract
                        .mandatory_checks()
                        .map(|m| m.id.clone())
                        .collect::<Vec<_>>()
                });
            mandatory.is_some_and(|ids| {
                ids.iter().all(|id| {
                    r.reports
                        .iter()
                        .any(|rep| &rep.check == id && rep.verdict == Verdict::Passed)
                })
            })
        });
        let note = if !recorded {
            "the result could not be recorded".to_string()
        } else if done.dirty {
            "the worktree has uncommitted changes: the checks ran on the last commit, and this evidence does not count".to_string()
        } else if !on_the_reviewed_commit {
            "the commit that was checked is not the one the last review recorded for the task: this evidence does not count".to_string()
        } else if !all_passed {
            "a mandatory check did not pass".to_string()
        } else {
            String::new()
        };
        self.remember_outcome(VerifyOutcome {
            key: key.clone(),
            ticket,
            recorded,
            passed: note.is_empty(),
            note: flatten_line(&note, 240),
        });
        if done.origin != VerifyOrigin::Review {
            return;
        }
        let in_review = self
            .state
            .run(&key.0)
            .ok()
            .and_then(|r| r.task(&key.1))
            .is_some_and(|t| t.status == TaskStatus::Review && t.attempt == done.attempt);
        if !in_review {
            return;
        }
        let rkey = (key.0.clone(), key.1.clone(), done.attempt);
        if let Some(why) = cannot_record {
            self.committed.remove(&rkey);
            self.fail_task(&key, format!("the verification cannot be recorded: {why}"));
            return;
        }
        let mut outcomes: Vec<GateOutcome> = done
            .results
            .iter()
            .flat_map(|r| self.outcomes_of(r))
            .collect();
        if done.dirty {
            // The work was committed for review; changes that appeared since are a process of
            // the worker still writing, and what is merged is not what was checked.
            outcomes.push(GateOutcome {
                name: "verify worktree".to_string(),
                passed: false,
                summary: "the worktree changed after its work was committed".to_string(),
                output: "Files were changed or added after the work was committed for review. Make sure no process of yours is still running when you finish.".to_string(),
            });
        }
        // The pass must be this run's: every contract that guards the task now (not only those
        // that did when the job started) must be among the results, so that evidence of an earlier
        // run of another contract, or of the same one on another commit, never stands in for it.
        let covered = self.verification_targets(&key.0, &key.1).iter().all(|t| {
            done.results
                .iter()
                .any(|r| r.goal == t.goal && r.contract_id == t.contract_id)
        });
        if recorded
            && covered
            && !done.dirty
            && self.ledger.task_contract(&key.0, &key.1) == TaskContract::Passed
        {
            self.verified.insert(rkey);
            return;
        }
        if outcomes.iter().all(|o| o.passed) {
            // Nothing failed, yet the contracts do not all stand passed: a contract changed or a
            // goal was added while the checks ran. Run them again; never complete on this.
            return;
        }
        self.committed.remove(&rkey);
        self.apply_gate_results(&key, done.attempt, &outcomes);
    }

    // ---- fleet verify ------------------------------------------------------------------------

    /// Resolves a `verify` target to the task whose worktree is used and the goals whose
    /// contracts run. `run/task` is always a task; a bare word is a goal when the ledger has one
    /// of that name, else a task.
    fn verify_subject(&self, target: &str, task: Option<&str>) -> Result<(Key, Vec<GoalId>)> {
        let goal = if target.contains('/') {
            None
        } else {
            GoalId::parse(target)
                .ok()
                .filter(|g| self.ledger.find(g).is_some())
        };
        match goal {
            Some(id) => {
                let g = self.ledger.goal(&id)?;
                if g.contract.is_none() {
                    return Err(FleetError::conflict(format!(
                        "goal {} has no verification contract",
                        show(id.as_str())
                    )));
                }
                let chosen: Option<TaskRef> = match task {
                    Some(t) => {
                        let (run, task) = self.state.find_task(t)?;
                        Some(TaskRef::new(run, task))
                    }
                    None => match g.tasks.as_slice() {
                        [only] => Some(only.clone()),
                        _ => None,
                    },
                };
                let chosen = chosen.ok_or_else(|| {
                    FleetError::invalid(format!(
                        "goal {} does not name exactly one task; choose the worktree with --task <run>/<task>",
                        show(id.as_str())
                    ))
                })?;
                if !g.tasks.is_empty() && !g.tasks.contains(&chosen) {
                    return Err(FleetError::invalid(format!(
                        "goal {} does not name task {chosen}; its evidence would not count for it",
                        show(id.as_str())
                    )));
                }
                Ok(((chosen.run, chosen.task), vec![id]))
            }
            None => {
                let (run, task_id) = self.state.find_task(target)?;
                let goals: Vec<GoalId> = self
                    .ledger
                    .governing(&run, &task_id)
                    .into_iter()
                    .map(|g| g.id.clone())
                    .collect();
                if goals.is_empty() {
                    return Err(FleetError::not_found(format!(
                        "no goal with a verification contract names task {run}/{task_id}"
                    )));
                }
                Ok(((run, task_id), goals))
            }
        }
    }

    pub(crate) fn verify_command(&mut self, target: &str, task: Option<&str>) -> Result<Reply> {
        let (key, goals) = self.verify_subject(target, task)?;
        let task_state = self
            .state
            .run(&key.0)?
            .task(&key.1)
            .ok_or_else(|| {
                FleetError::not_found(format!("task {}/{} does not exist", key.0, key.1))
            })?
            .clone();
        if task_state.status == TaskStatus::Running {
            return Err(FleetError::conflict(format!(
                "{}/{} is still running; verify it after the worker finished",
                key.0, key.1
            )));
        }
        let Some(worktree) = task_state.worktree.clone() else {
            return Err(FleetError::conflict(format!(
                "{}/{} has no worktree to verify",
                key.0, key.1
            )));
        };
        let targets: Vec<VerifyTarget> = goals
            .iter()
            .filter_map(|id| self.ledger.find(id))
            .filter_map(|g| {
                g.contract.as_ref().map(|c| VerifyTarget {
                    goal: g.id.clone(),
                    contract_id: c.id.clone(),
                    contract: c.contract.clone(),
                })
            })
            .collect();
        let ticket = self.start_verification(
            &key,
            task_state.attempt,
            VerifyOrigin::Manual,
            &worktree,
            targets,
        )?;
        Ok(data(
            "fleet.verify.started/1",
            vec![
                ("ticket", Value::int(ticket)),
                ("target", Value::str(target)),
                ("task", Value::str(format!("{}/{}", key.0, key.1))),
                (
                    "goals",
                    Value::array(goals.iter().map(|g| Value::str(g.as_str()))),
                ),
            ],
        ))
    }

    /// Where a verification stands. With the `ticket` of the run a person started it reports on
    /// that run (running, or how it ended); without one it reports the ledger's state of the
    /// contracts.
    pub(crate) fn verify_status(
        &self,
        target: &str,
        task: Option<&str>,
        ticket: Option<u64>,
    ) -> Result<Reply> {
        let (key, goals) = self.verify_subject(target, task)?;
        let in_flight = self.verifying.get(&key).copied();
        let running = match ticket {
            Some(t) => in_flight.is_some_and(|v| v.ticket == t),
            None => in_flight.is_some(),
        };
        let outcome = ticket.and_then(|t| {
            self.verify_outcomes
                .iter()
                .find(|o| o.ticket == t && o.key == key)
        });
        if let Some(t) = ticket {
            if !running && outcome.is_none() {
                return Err(FleetError::conflict(format!(
                    "the result of verification {t} is not known (the daemon restarted while it ran, or it was started by someone else)"
                )));
            }
        }
        let task_ref = TaskRef::new(key.0, key.1);
        Ok(Reply::Data(super::views::verify_status(
            &self.state,
            &self.ledger,
            super::views::VerifyView {
                target,
                task: Some(&task_ref),
                running,
                ticket,
                outcome,
                goals: &goals,
            },
        )))
    }
}
