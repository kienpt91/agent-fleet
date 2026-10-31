//! The `Run` aggregate: one objective, its task DAG and the lifecycle of every task in it.
//!
//! Methods named after a command (`add_tasks`, `begin`, `finish`, ...) validate the invariants
//! and return the events that record the outcome without changing anything. `apply` folds an
//! event into the state; replaying a run's events in order rebuilds it exactly.

use crate::contract::{ResultStatus, WorkerQuestion, WorkerResult};
use crate::dag::{self, Node};
use crate::error::{FleetError, Result};
use crate::events::DomainEvent;
use crate::ids::{
    integration_branch, task_branch, validate_line, validate_text, RunId, Sha, TaskId, WorkerName,
};
use crate::limits::{
    MAX_NOTE, MAX_NOTES_PER_TASK, MAX_QUESTIONS_PER_TASK, MAX_SESSIONS_HARD, MAX_SPEC,
    MAX_TASKS_PER_RUN, MAX_TITLE,
};
use crate::quality::{gate_feedback, GateOutcome};
use crate::sanitize::{head_str, tail_str};
use crate::status::{BlockKind, EndKind, TaskStatus};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    /// Decision gate id, approval id or dependency task id, depending on `kind`.
    pub subject: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRecord {
    pub attempt: u32,
    pub pid: u64,
    pub start_token: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateRecord {
    pub name: String,
    pub passed: bool,
    pub summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub spec: String,
    pub deps: Vec<TaskId>,
    pub worker: Option<WorkerName>,
    pub status: TaskStatus,
    pub block: Option<Block>,
    /// Number of dispatches so far.
    pub attempt: u32,
    /// Automatic retries consumed.
    pub retries_used: u32,
    /// Questions and approvals raised; bounded so a worker cannot loop forever.
    pub questions: u32,
    /// Text appended to the brief of the next attempt (failures, answers, approvals).
    pub notes: Vec<String>,
    pub summary: Option<String>,
    pub failure: Option<String>,
    pub branch: String,
    pub worktree: Option<String>,
    pub head: Option<Sha>,
    pub merged: Option<Sha>,
    pub session: Option<SessionRecord>,
    pub gates: Vec<GateRecord>,
    pub conflict_files: Vec<String>,
}

/// A task to be added to a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewTask {
    pub id: Option<TaskId>,
    pub title: String,
    pub spec: String,
    pub deps: Vec<TaskId>,
    pub worker: Option<WorkerName>,
}

/// What the supervisor observed when a worker session ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerEnd {
    pub attempt: u32,
    pub outcome: EndKind,
    pub exit_code: Option<i64>,
    pub output_tail: String,
    pub stripped: u64,
    pub result: Option<WorkerResult>,
    pub question: Option<WorkerQuestion>,
    /// A contract file that was present but unusable.
    pub contract_warning: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_retries: u32,
}

#[derive(Clone, Debug)]
pub struct Run {
    pub id: RunId,
    pub objective: String,
    pub repo: String,
    pub base_ref: String,
    pub base_sha: Sha,
    pub max_parallel: u32,
    pub started: bool,
    pub integration_ready: bool,
    tasks: Vec<Task>,
}

impl Run {
    pub fn from_created(ev: &DomainEvent) -> Option<Run> {
        match ev {
            DomainEvent::RunCreated {
                run,
                objective,
                repo,
                base_ref,
                base_sha,
                max_parallel,
            } => Some(Run {
                id: run.clone(),
                objective: objective.clone(),
                repo: repo.clone(),
                base_ref: base_ref.clone(),
                base_sha: base_sha.clone(),
                max_parallel: *max_parallel,
                started: false,
                integration_ready: false,
                tasks: Vec::new(),
            }),
            _ => None,
        }
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    pub fn task(&self, id: &TaskId) -> Option<&Task> {
        self.tasks.iter().find(|t| &t.id == id)
    }

    fn task_or_err(&self, id: &TaskId) -> Result<&Task> {
        self.task(id).ok_or_else(|| {
            FleetError::not_found(format!("task {id} does not exist in run {}", self.id))
        })
    }

    fn task_index(&self, id: &TaskId) -> Option<usize> {
        self.tasks.iter().position(|t| &t.id == id)
    }

    pub fn integration_branch(&self) -> String {
        integration_branch(self.id.as_str())
    }

    /// True when every task has reached a final state.
    pub fn is_finished(&self) -> bool {
        !self.tasks.is_empty() && self.tasks.iter().all(|t| t.status.is_terminal())
    }

    pub fn running_count(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Running)
            .count()
    }

    // ---- commands -------------------------------------------------------------------------

    /// Adds tasks, all or none. Ids that are not given are generated (`t1`, `t2`, ...).
    pub fn add_tasks(&self, new: Vec<NewTask>) -> Result<(Vec<TaskId>, Vec<DomainEvent>)> {
        if new.is_empty() {
            return Err(FleetError::invalid("no tasks to add"));
        }
        if self.tasks.len() + new.len() > MAX_TASKS_PER_RUN {
            return Err(FleetError::limit(format!(
                "a run holds at most {MAX_TASKS_PER_RUN} tasks"
            )));
        }
        let mut taken: BTreeSet<TaskId> = self.tasks.iter().map(|t| t.id.clone()).collect();
        let mut seq = u64::try_from(self.tasks.len()).unwrap_or(0);
        let mut filled: Vec<NewTask> = Vec::with_capacity(new.len());
        for mut n in new {
            validate_line("task title", &n.title, MAX_TITLE)?;
            validate_text("task spec", &n.spec, MAX_SPEC)?;
            if n.id.is_none() {
                loop {
                    seq += 1;
                    let candidate = TaskId::from_seq(seq);
                    if !taken.contains(&candidate) {
                        taken.insert(candidate.clone());
                        n.id = Some(candidate);
                        break;
                    }
                }
            }
            filled.push(n);
        }
        let existing: BTreeSet<TaskId> = self.tasks.iter().map(|t| t.id.clone()).collect();
        let nodes: Vec<Node> = filled
            .iter()
            .filter_map(|n| {
                n.id.clone().map(|id| Node {
                    id,
                    deps: n.deps.clone(),
                })
            })
            .collect();
        let order = dag::order_new_tasks(&existing, &nodes)?;
        let mut ids = Vec::new();
        let mut events = Vec::new();
        for i in order {
            let Some(n) = filled.get(i) else { continue };
            let Some(id) = n.id.clone() else { continue };
            ids.push(id.clone());
            events.push(DomainEvent::TaskAdded {
                run: self.id.clone(),
                task: id,
                title: n.title.trim().to_string(),
                spec: n.spec.clone(),
                deps: n.deps.clone(),
                worker: n.worker.clone(),
            });
        }
        Ok((ids, events))
    }

    /// Starts scheduling. `max_parallel` replaces the current limit when given.
    pub fn start(&self, max_parallel: Option<u32>) -> Result<DomainEvent> {
        let n = max_parallel.unwrap_or(self.max_parallel);
        validate_parallel(n)?;
        if self.tasks.is_empty() {
            return Err(FleetError::conflict(format!(
                "run {} has no tasks to start",
                self.id
            )));
        }
        Ok(DomainEvent::RunStarted {
            run: self.id.clone(),
            max_parallel: n,
        })
    }

    /// Moves tasks whose situation changed: pending tasks with all dependencies completed become
    /// ready; tasks that depend on a failed task become blocked; blocked tasks whose dependency
    /// recovered become pending again. Runs to a fixed point, so a chain settles in one call.
    pub fn promote(&self) -> Vec<DomainEvent> {
        let mut state: BTreeMap<&TaskId, (TaskStatus, Option<BlockKind>)> = self
            .tasks
            .iter()
            .map(|t| (&t.id, (t.status, t.block.as_ref().map(|b| b.kind))))
            .collect();
        let mut events = Vec::new();
        for _ in 0..=self.tasks.len() {
            let mut changed = false;
            for t in &self.tasks {
                let Some(&(status, block)) = state.get(&t.id) else {
                    continue;
                };
                let bad_dep = t.deps.iter().find(|d| {
                    matches!(
                        state.get(d),
                        Some((TaskStatus::Failed, _))
                            | Some((TaskStatus::Blocked, Some(BlockKind::Dependency)))
                    )
                });
                let all_done = t
                    .deps
                    .iter()
                    .all(|d| matches!(state.get(d), Some((TaskStatus::Completed, _))));
                match (status, block) {
                    (TaskStatus::Pending, _) => {
                        if let Some(dep) = bad_dep {
                            events.push(DomainEvent::TaskBlocked {
                                run: self.id.clone(),
                                task: t.id.clone(),
                                kind: BlockKind::Dependency,
                                subject: dep.to_string(),
                                detail: format!("dependency {dep} did not complete"),
                            });
                            state.insert(&t.id, (TaskStatus::Blocked, Some(BlockKind::Dependency)));
                            changed = true;
                        } else if all_done {
                            events.push(DomainEvent::TaskReady {
                                run: self.id.clone(),
                                task: t.id.clone(),
                            });
                            state.insert(&t.id, (TaskStatus::Ready, None));
                            changed = true;
                        }
                    }
                    (TaskStatus::Blocked, Some(BlockKind::Dependency)) if bad_dep.is_none() => {
                        events.push(DomainEvent::TaskUnblocked {
                            run: self.id.clone(),
                            task: t.id.clone(),
                        });
                        state.insert(&t.id, (TaskStatus::Pending, None));
                        changed = true;
                    }
                    _ => {}
                }
            }
            if !changed {
                break;
            }
        }
        events
    }

    /// Ready tasks to dispatch now, oldest first, within the run's parallel limit and the
    /// caller's remaining global capacity. Nothing is dispatched before `start`.
    pub fn dispatchable(&self, global_free: usize) -> Vec<TaskId> {
        if !self.started {
            return Vec::new();
        }
        let limit = usize::try_from(self.max_parallel).unwrap_or(usize::MAX);
        let free = limit.saturating_sub(self.running_count()).min(global_free);
        self.tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Ready)
            .take(free)
            .map(|t| t.id.clone())
            .collect()
    }

    /// Records that a worker session was started for a ready task.
    pub fn begin(
        &self,
        task: &TaskId,
        worktree: &str,
        pid: u64,
        start_token: &str,
    ) -> Result<DomainEvent> {
        let t = self.task_or_err(task)?;
        if !self.started {
            return Err(FleetError::conflict(format!(
                "run {} is not started",
                self.id
            )));
        }
        if t.status != TaskStatus::Ready {
            return Err(FleetError::conflict(format!(
                "task {task} is {}, not ready",
                t.status
            )));
        }
        Ok(DomainEvent::TaskDispatched {
            run: self.id.clone(),
            task: task.clone(),
            attempt: t.attempt.saturating_add(1),
            branch: t.branch.clone(),
            worktree: worktree.to_string(),
            pid,
            start_token: start_token.to_string(),
        })
    }

    /// Decides what a finished worker session means, for every outcome except a question or
    /// approval request (which needs ids from the registry; see `FleetState::worker_ended`).
    pub fn finish(
        &self,
        task: &TaskId,
        end: &WorkerEnd,
        policy: RetryPolicy,
    ) -> Result<Vec<DomainEvent>> {
        let t = self.task_or_err(task)?;
        if t.status != TaskStatus::Running || t.attempt != end.attempt {
            return Err(FleetError::conflict(format!(
                "task {task} is not running attempt {}",
                end.attempt
            )));
        }
        let mut events = self.session_events(task, end);
        match classify(end) {
            Verdict::Stopped => events.push(DomainEvent::TaskFailed {
                run: self.id.clone(),
                task: task.clone(),
                reason: "stopped by an operator".to_string(),
            }),
            Verdict::Success(summary) => events.push(DomainEvent::TaskReviewing {
                run: self.id.clone(),
                task: task.clone(),
                summary,
            }),
            Verdict::Failure { reason, counts } => {
                events.extend(self.retry_or_fail(t, &reason, &end.output_tail, counts, policy));
            }
        }
        Ok(events)
    }

    /// The `SessionEnded` event and any contract warning.
    pub(crate) fn session_events(&self, task: &TaskId, end: &WorkerEnd) -> Vec<DomainEvent> {
        let mut events = vec![DomainEvent::SessionEnded {
            run: self.id.clone(),
            task: task.clone(),
            attempt: end.attempt,
            outcome: end.outcome,
            exit_code: end.exit_code,
            output_tail: head_str(&end.output_tail, crate::limits::EVENT_OUTPUT_TAIL * 4)
                .to_string(),
            stripped: end.stripped,
        }];
        if let Some(w) = &end.contract_warning {
            events.push(DomainEvent::Warning {
                message: format!("{}/{task}: {w}", self.id),
            });
        }
        events
    }

    fn retry_or_fail(
        &self,
        t: &Task,
        reason: &str,
        output_tail: &str,
        counts: bool,
        policy: RetryPolicy,
    ) -> Vec<DomainEvent> {
        let may_retry = !counts || t.retries_used < policy.max_retries;
        if !may_retry {
            return vec![DomainEvent::TaskFailed {
                run: self.id.clone(),
                task: t.id.clone(),
                reason: reason.to_string(),
            }];
        }
        let used = if counts {
            t.retries_used.saturating_add(1)
        } else {
            t.retries_used
        };
        let mut note = format!("Attempt {} failed: {reason}.", t.attempt);
        if !output_tail.trim().is_empty() {
            note.push_str("\nLast output:\n");
            note.push_str(tail_str(output_tail.trim_end(), 1500));
        }
        vec![
            DomainEvent::RetryQueued {
                run: self.id.clone(),
                task: t.id.clone(),
                retries_used: used,
                reason: reason.to_string(),
            },
            self.note_event(&t.id, &note),
        ]
    }

    pub(crate) fn note_event(&self, task: &TaskId, text: &str) -> DomainEvent {
        DomainEvent::NoteAdded {
            run: self.id.clone(),
            task: task.clone(),
            note: head_str(text, MAX_NOTE).to_string(),
        }
    }

    /// The worker's work was committed.
    pub fn record_commit(&self, task: &TaskId, head: Sha) -> Result<DomainEvent> {
        let t = self.task_or_err(task)?;
        if t.status != TaskStatus::Review {
            return Err(FleetError::conflict(format!(
                "task {task} is {}, not in review",
                t.status
            )));
        }
        Ok(DomainEvent::TaskCommitted {
            run: self.id.clone(),
            task: task.clone(),
            head,
        })
    }

    /// Applies the results of the quality gates. Results for an attempt that is no longer in
    /// review (stale after a restart or a retry) are ignored and produce no events.
    pub fn gates_checked(
        &self,
        task: &TaskId,
        attempt: u32,
        outcomes: &[GateOutcome],
        policy: RetryPolicy,
    ) -> Result<Vec<DomainEvent>> {
        let t = self.task_or_err(task)?;
        if t.status != TaskStatus::Review || t.attempt != attempt {
            return Ok(Vec::new());
        }
        let mut events: Vec<DomainEvent> = outcomes
            .iter()
            .map(|o| DomainEvent::GateChecked {
                run: self.id.clone(),
                task: task.clone(),
                attempt,
                gate: o.name.clone(),
                passed: o.passed,
                summary: o.summary.clone(),
                output_tail: head_str(&o.output, crate::limits::EVENT_OUTPUT_TAIL).to_string(),
            })
            .collect();
        if outcomes.iter().all(|o| o.passed) {
            events.push(DomainEvent::TaskCompleted {
                run: self.id.clone(),
                task: task.clone(),
            });
        } else {
            let feedback = gate_feedback(outcomes);
            let reason = format!(
                "quality gate failed: {}",
                outcomes
                    .iter()
                    .filter(|o| !o.passed)
                    .map(|o| o.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let mut retry = self.retry_or_fail(t, &reason, "", true, policy);
            if let Some(DomainEvent::NoteAdded { note, .. }) = retry.get_mut(1) {
                *note = head_str(&format!("{note}\n{feedback}"), MAX_NOTE).to_string();
            }
            events.extend(retry);
        }
        Ok(events)
    }

    /// An operator resets a failed task for a fresh set of attempts.
    pub fn operator_retry(&self, task: &TaskId) -> Result<DomainEvent> {
        let t = self.task_or_err(task)?;
        if t.status != TaskStatus::Failed {
            return Err(FleetError::conflict(format!(
                "task {task} is {}; only failed tasks can be retried",
                t.status
            )));
        }
        Ok(DomainEvent::RetryReset {
            run: self.id.clone(),
            task: task.clone(),
        })
    }

    /// Cancels a task that is not running (a running task is stopped through its session).
    pub fn cancel(&self, task: &TaskId) -> Result<DomainEvent> {
        let t = self.task_or_err(task)?;
        match t.status {
            TaskStatus::Pending | TaskStatus::Ready | TaskStatus::Blocked => {
                Ok(DomainEvent::TaskFailed {
                    run: self.id.clone(),
                    task: task.clone(),
                    reason: "cancelled by an operator".to_string(),
                })
            }
            other => Err(FleetError::conflict(format!(
                "task {task} is {other} and cannot be cancelled"
            ))),
        }
    }

    /// Puts a blocked task back in line, with a note for its next attempt.
    pub fn unblock(&self, task: &TaskId, note: &str) -> Result<Vec<DomainEvent>> {
        let t = self.task_or_err(task)?;
        if t.status != TaskStatus::Blocked {
            return Err(FleetError::conflict(format!(
                "task {task} is {}, not blocked",
                t.status
            )));
        }
        let mut events = Vec::new();
        if !note.trim().is_empty() {
            events.push(self.note_event(task, note));
        }
        events.push(DomainEvent::TaskUnblocked {
            run: self.id.clone(),
            task: task.clone(),
        });
        Ok(events)
    }

    /// Checks that a task may be merged into the integration branch.
    pub fn check_mergeable(&self, task: &TaskId) -> Result<&Task> {
        let t = self.task_or_err(task)?;
        if t.status != TaskStatus::Completed {
            return Err(FleetError::conflict(format!(
                "task {task} is {}; only completed tasks can be merged",
                t.status
            )));
        }
        if t.merged.is_some() {
            return Err(FleetError::conflict(format!(
                "task {task} is already merged"
            )));
        }
        Ok(t)
    }

    // ---- state ----------------------------------------------------------------------------

    pub fn apply(&mut self, ev: &DomainEvent) {
        match ev {
            DomainEvent::RunStarted { run, max_parallel } if *run == self.id => {
                self.started = true;
                self.max_parallel = *max_parallel;
            }
            DomainEvent::IntegrationCreated { run } if *run == self.id => {
                self.integration_ready = true
            }
            DomainEvent::TaskAdded {
                run,
                task,
                title,
                spec,
                deps,
                worker,
            } if *run == self.id => {
                self.tasks.push(Task {
                    id: task.clone(),
                    title: title.clone(),
                    spec: spec.clone(),
                    deps: deps.clone(),
                    worker: worker.clone(),
                    status: TaskStatus::Pending,
                    block: None,
                    attempt: 0,
                    retries_used: 0,
                    questions: 0,
                    notes: Vec::new(),
                    summary: None,
                    failure: None,
                    branch: task_branch(self.id.as_str(), task.as_str()),
                    worktree: None,
                    head: None,
                    merged: None,
                    session: None,
                    gates: Vec::new(),
                    conflict_files: Vec::new(),
                });
            }
            _ => self.apply_task_event(ev),
        }
    }

    fn apply_task_event(&mut self, ev: &DomainEvent) {
        let Some(run) = ev.run() else { return };
        if *run != self.id {
            return;
        }
        let task_id = match ev {
            DomainEvent::TaskReady { task, .. }
            | DomainEvent::TaskDispatched { task, .. }
            | DomainEvent::SessionEnded { task, .. }
            | DomainEvent::TaskCommitted { task, .. }
            | DomainEvent::TaskReviewing { task, .. }
            | DomainEvent::GateChecked { task, .. }
            | DomainEvent::TaskCompleted { task, .. }
            | DomainEvent::TaskFailed { task, .. }
            | DomainEvent::RetryQueued { task, .. }
            | DomainEvent::RetryReset { task, .. }
            | DomainEvent::NoteAdded { task, .. }
            | DomainEvent::TaskBlocked { task, .. }
            | DomainEvent::TaskUnblocked { task, .. }
            | DomainEvent::TaskMerged { task, .. }
            | DomainEvent::MergeConflicted { task, .. }
            | DomainEvent::WorktreeRemoved { task, .. } => task,
            _ => return,
        };
        let Some(idx) = self.task_index(task_id) else {
            return;
        };
        let Some(t) = self.tasks.get_mut(idx) else {
            return;
        };
        match ev {
            DomainEvent::TaskReady { .. } => t.status = TaskStatus::Ready,
            DomainEvent::TaskDispatched {
                attempt,
                worktree,
                pid,
                start_token,
                ..
            } => {
                t.status = TaskStatus::Running;
                t.attempt = *attempt;
                t.worktree = Some(worktree.clone()).filter(|w| !w.is_empty());
                t.block = None;
                t.gates.clear();
                t.session = Some(SessionRecord {
                    attempt: *attempt,
                    pid: *pid,
                    start_token: start_token.clone(),
                });
            }
            DomainEvent::SessionEnded { .. } => t.session = None,
            DomainEvent::TaskCommitted { head, .. } => t.head = Some(head.clone()),
            DomainEvent::TaskReviewing { summary, .. } => {
                t.status = TaskStatus::Review;
                t.summary = Some(summary.clone()).filter(|s| !s.is_empty());
            }
            DomainEvent::GateChecked {
                gate,
                passed,
                summary,
                ..
            } => t.gates.push(GateRecord {
                name: gate.clone(),
                passed: *passed,
                summary: summary.clone(),
            }),
            DomainEvent::TaskCompleted { .. } => t.status = TaskStatus::Completed,
            DomainEvent::TaskFailed { reason, .. } => {
                t.status = TaskStatus::Failed;
                t.failure = Some(reason.clone());
                t.session = None;
            }
            DomainEvent::RetryQueued { retries_used, .. } => {
                t.status = TaskStatus::Ready;
                t.retries_used = *retries_used;
                t.session = None;
            }
            DomainEvent::RetryReset { .. } => {
                t.status = TaskStatus::Ready;
                t.retries_used = 0;
                t.questions = 0;
                t.failure = None;
            }
            DomainEvent::NoteAdded { note, .. } => {
                t.notes.push(note.clone());
                while t.notes.len() > MAX_NOTES_PER_TASK {
                    t.notes.remove(0);
                }
            }
            DomainEvent::TaskBlocked {
                kind,
                subject,
                detail,
                ..
            } => {
                t.status = TaskStatus::Blocked;
                if matches!(kind, BlockKind::Question | BlockKind::Approval) {
                    t.questions = t.questions.saturating_add(1);
                }
                t.block = Some(Block {
                    kind: *kind,
                    subject: subject.clone(),
                    detail: detail.clone(),
                });
            }
            DomainEvent::TaskUnblocked { .. } => {
                let dependency = t
                    .block
                    .as_ref()
                    .is_some_and(|b| b.kind == BlockKind::Dependency);
                t.status = if dependency {
                    TaskStatus::Pending
                } else {
                    TaskStatus::Ready
                };
                t.block = None;
                t.conflict_files.clear();
            }
            DomainEvent::TaskMerged { merge_sha, .. } => t.merged = Some(merge_sha.clone()),
            DomainEvent::MergeConflicted { files, .. } => t.conflict_files = files.clone(),
            DomainEvent::WorktreeRemoved { .. } => t.worktree = None,
            _ => {}
        }
    }

    /// True when raising one more question would exceed the per-task allowance.
    pub fn question_limit_reached(&self, task: &TaskId) -> bool {
        self.task(task)
            .is_some_and(|t| t.questions >= MAX_QUESTIONS_PER_TASK)
    }
}

pub fn validate_parallel(n: u32) -> Result<()> {
    let max = u32::try_from(MAX_SESSIONS_HARD).unwrap_or(u32::MAX);
    if n == 0 || n > max {
        return Err(FleetError::invalid(format!(
            "max parallel must be between 1 and {max}"
        )));
    }
    Ok(())
}

enum Verdict {
    Stopped,
    Success(String),
    Failure { reason: String, counts: bool },
}

fn classify(end: &WorkerEnd) -> Verdict {
    let reported = end.result.as_ref();
    let reported_failure = reported.filter(|r| r.status == ResultStatus::Failed);
    let summary = || {
        reported
            .map(|r| r.summary.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_default()
    };
    let with_summary = |base: String| match reported_failure {
        Some(r) if !r.summary.is_empty() => format!("{base}; worker reported: {}", r.summary),
        _ => base,
    };
    match end.outcome {
        EndKind::Stopped => Verdict::Stopped,
        EndKind::Exited => match end.exit_code {
            Some(0) => match reported_failure {
                Some(r) => Verdict::Failure {
                    reason: if r.summary.is_empty() {
                        "worker reported failure".to_string()
                    } else {
                        format!("worker reported failure: {}", r.summary)
                    },
                    counts: true,
                },
                None => Verdict::Success(summary()),
            },
            Some(code) => Verdict::Failure {
                reason: with_summary(format!("worker exited with code {code}")),
                counts: true,
            },
            None => Verdict::Failure {
                reason: with_summary("worker exit status is unknown".to_string()),
                counts: true,
            },
        },
        EndKind::Orphaned => {
            if reported.is_some_and(|r| r.status == ResultStatus::Done) {
                Verdict::Success(summary())
            } else {
                Verdict::Failure {
                    reason: "the session was interrupted by a daemon restart".to_string(),
                    counts: false,
                }
            }
        }
        EndKind::Signaled => Verdict::Failure {
            reason: with_summary("worker was killed by a signal".to_string()),
            counts: true,
        },
        EndKind::Timeout => Verdict::Failure {
            reason: "worker exceeded its time limit".to_string(),
            counts: true,
        },
        EndKind::IdleTimeout => Verdict::Failure {
            reason: "worker was silent for longer than its idle limit".to_string(),
            counts: true,
        },
        EndKind::KillFailed => Verdict::Failure {
            reason: "worker did not stop when asked to".to_string(),
            counts: true,
        },
        EndKind::SpawnFailed => Verdict::Failure {
            reason: "worker could not be started".to_string(),
            counts: true,
        },
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub fn run_id() -> RunId {
        RunId::parse("r1").unwrap()
    }

    pub fn tid(s: &str) -> TaskId {
        TaskId::parse(s).unwrap()
    }

    pub fn sha(c: char) -> Sha {
        Sha::parse(&c.to_string().repeat(40)).unwrap()
    }

    pub fn new_run(max_parallel: u32) -> Run {
        Run::from_created(&DomainEvent::RunCreated {
            run: run_id(),
            objective: "ship it".into(),
            repo: "/repo".into(),
            base_ref: "main".into(),
            base_sha: sha('a'),
            max_parallel,
        })
        .unwrap()
    }

    pub fn nt(id: &str, deps: &[&str]) -> NewTask {
        NewTask {
            id: Some(tid(id)),
            title: format!("task {id}"),
            spec: "do it".into(),
            deps: deps.iter().map(|d| tid(d)).collect(),
            worker: None,
        }
    }

    pub fn apply_all(run: &mut Run, events: &[DomainEvent]) {
        for e in events {
            run.apply(e);
        }
    }

    pub fn with_tasks(max_parallel: u32, tasks: Vec<NewTask>) -> Run {
        let mut run = new_run(max_parallel);
        let (_, events) = run.add_tasks(tasks).unwrap();
        apply_all(&mut run, &events);
        let promoted = run.promote();
        apply_all(&mut run, &promoted);
        run
    }

    pub fn started(max_parallel: u32, tasks: Vec<NewTask>) -> Run {
        let mut run = with_tasks(max_parallel, tasks);
        let ev = run.start(None).unwrap();
        run.apply(&ev);
        run
    }

    pub fn dispatch(run: &mut Run, id: &str) {
        let ev = run.begin(&tid(id), "/wt", 100, "tok").unwrap();
        run.apply(&ev);
    }

    pub fn end(attempt: u32, outcome: EndKind, code: Option<i64>) -> WorkerEnd {
        WorkerEnd {
            attempt,
            outcome,
            exit_code: code,
            output_tail: "last words".into(),
            stripped: 0,
            result: None,
            question: None,
            contract_warning: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::error::ErrorCode;

    const POLICY: RetryPolicy = RetryPolicy { max_retries: 2 };

    fn kinds(events: &[DomainEvent]) -> Vec<&'static str> {
        events.iter().map(DomainEvent::kind).collect()
    }

    // ---- add_tasks ----

    #[test]
    fn add_tasks_emits_one_event_per_task_in_dependency_order() {
        let run = new_run(2);
        let (ids, events) = run.add_tasks(vec![nt("b", &["a"]), nt("a", &[])]).unwrap();
        assert_eq!(ids, vec![tid("a"), tid("b")]);
        assert_eq!(kinds(&events), vec!["task_added", "task_added"]);
    }

    #[test]
    fn add_tasks_generates_ids_that_skip_taken_ones() {
        let mut run = new_run(2);
        let (_, ev) = run.add_tasks(vec![nt("t2", &[])]).unwrap();
        apply_all(&mut run, &ev);
        let (ids, _) = run
            .add_tasks(vec![
                NewTask {
                    id: None,
                    title: "x".into(),
                    spec: String::new(),
                    deps: vec![],
                    worker: None,
                },
                NewTask {
                    id: None,
                    title: "y".into(),
                    spec: String::new(),
                    deps: vec![],
                    worker: None,
                },
            ])
            .unwrap();
        assert_eq!(ids, vec![tid("t3"), tid("t4")]);
    }

    #[test]
    fn add_tasks_rejects_invalid_input_without_events() {
        let mut run = new_run(2);
        let (_, ev) = run.add_tasks(vec![nt("a", &[])]).unwrap();
        apply_all(&mut run, &ev);
        let cases: Vec<(Vec<NewTask>, ErrorCode)> = vec![
            (vec![], ErrorCode::Invalid),
            (vec![nt("a", &[])], ErrorCode::Invalid),
            (vec![nt("b", &["ghost"])], ErrorCode::Invalid),
            (vec![nt("b", &["c"]), nt("c", &["b"])], ErrorCode::Invalid),
            (
                vec![NewTask {
                    title: "  ".into(),
                    ..nt("b", &[])
                }],
                ErrorCode::Invalid,
            ),
            (
                vec![NewTask {
                    title: "a\nb".into(),
                    ..nt("b", &[])
                }],
                ErrorCode::Invalid,
            ),
            (
                vec![NewTask {
                    spec: "x".repeat(MAX_SPEC + 1),
                    ..nt("b", &[])
                }],
                ErrorCode::Invalid,
            ),
        ];
        for (tasks, code) in cases {
            let err = run.add_tasks(tasks.clone()).unwrap_err();
            assert_eq!(err.code, code, "{tasks:?}");
        }
        assert_eq!(run.tasks().len(), 1);
    }

    #[test]
    fn add_tasks_enforces_the_per_run_limit() {
        let run = new_run(2);
        let many: Vec<NewTask> = (0..=MAX_TASKS_PER_RUN)
            .map(|i| nt(&format!("x{i}"), &[]))
            .collect();
        assert_eq!(run.add_tasks(many).unwrap_err().code, ErrorCode::Limit);
    }

    // ---- start ----

    #[test]
    fn start_requires_tasks_and_a_sane_limit() {
        let empty = new_run(2);
        assert_eq!(empty.start(None).unwrap_err().code, ErrorCode::Conflict);
        let run = with_tasks(2, vec![nt("a", &[])]);
        assert_eq!(run.start(Some(0)).unwrap_err().code, ErrorCode::Invalid);
        assert!(run.start(Some(65)).is_err());
        let ev = run.start(Some(3)).unwrap();
        assert_eq!(
            ev,
            DomainEvent::RunStarted {
                run: run_id(),
                max_parallel: 3
            }
        );
    }

    #[test]
    fn starting_sets_the_flag_and_limit() {
        let mut run = with_tasks(2, vec![nt("a", &[])]);
        assert!(!run.started);
        let ev = run.start(Some(4)).unwrap();
        run.apply(&ev);
        assert!(run.started);
        assert_eq!(run.max_parallel, 4);
    }

    // ---- promote ----

    #[test]
    fn promote_makes_dependency_free_tasks_ready_exactly_once() {
        let mut run = new_run(2);
        let (_, ev) = run.add_tasks(vec![nt("a", &[]), nt("b", &["a"])]).unwrap();
        apply_all(&mut run, &ev);
        let first = run.promote();
        assert_eq!(
            first,
            vec![DomainEvent::TaskReady {
                run: run_id(),
                task: tid("a")
            }]
        );
        apply_all(&mut run, &first);
        assert!(run.promote().is_empty());
        assert_eq!(run.task(&tid("b")).unwrap().status, TaskStatus::Pending);
    }

    #[test]
    fn completing_a_dependency_readies_its_dependents() {
        let mut run = started(2, vec![nt("a", &[]), nt("b", &["a"])]);
        dispatch(&mut run, "a");
        let ev = run
            .finish(&tid("a"), &end(1, EndKind::Exited, Some(0)), POLICY)
            .unwrap();
        apply_all(&mut run, &ev);
        apply_all(
            &mut run,
            &[DomainEvent::TaskCompleted {
                run: run_id(),
                task: tid("a"),
            }],
        );
        let promoted = run.promote();
        assert_eq!(kinds(&promoted), vec!["task_ready"]);
    }

    #[test]
    fn failed_dependency_blocks_dependents_transitively_and_recovery_unblocks() {
        let mut run = started(2, vec![nt("a", &[]), nt("b", &["a"]), nt("c", &["b"])]);
        let fail = DomainEvent::TaskFailed {
            run: run_id(),
            task: tid("a"),
            reason: "x".into(),
        };
        run.apply(&fail);
        let events = run.promote();
        assert_eq!(kinds(&events), vec!["task_blocked", "task_blocked"]);
        apply_all(&mut run, &events);
        assert_eq!(
            run.task(&tid("b")).unwrap().block.as_ref().unwrap().kind,
            BlockKind::Dependency
        );
        assert_eq!(run.task(&tid("c")).unwrap().status, TaskStatus::Blocked);

        let reset = run.operator_retry(&tid("a")).unwrap();
        run.apply(&reset);
        let events = run.promote();
        apply_all(&mut run, &events);
        assert_eq!(run.task(&tid("b")).unwrap().status, TaskStatus::Pending);
        assert_eq!(run.task(&tid("c")).unwrap().status, TaskStatus::Pending);
        assert_eq!(run.task(&tid("a")).unwrap().status, TaskStatus::Ready);
    }

    // ---- dispatch ----

    #[test]
    fn nothing_is_dispatchable_before_start() {
        let run = with_tasks(2, vec![nt("a", &[])]);
        assert!(run.dispatchable(10).is_empty());
    }

    #[test]
    fn dispatchable_respects_parallel_limit_and_global_capacity() {
        let mut run = started(2, vec![nt("a", &[]), nt("b", &[]), nt("c", &[])]);
        assert_eq!(run.dispatchable(10), vec![tid("a"), tid("b")]);
        assert_eq!(run.dispatchable(1), vec![tid("a")]);
        assert!(run.dispatchable(0).is_empty());
        dispatch(&mut run, "a");
        assert_eq!(run.dispatchable(10), vec![tid("b")]);
        dispatch(&mut run, "b");
        assert!(run.dispatchable(10).is_empty());
    }

    #[test]
    fn begin_requires_a_started_run_and_a_ready_task() {
        let run = with_tasks(2, vec![nt("a", &[])]);
        assert_eq!(
            run.begin(&tid("a"), "/wt", 1, "t").unwrap_err().code,
            ErrorCode::Conflict
        );
        let mut run = started(2, vec![nt("a", &[]), nt("b", &["a"])]);
        assert_eq!(
            run.begin(&tid("b"), "/wt", 1, "t").unwrap_err().code,
            ErrorCode::Conflict
        );
        assert_eq!(
            run.begin(&tid("zzz"), "/wt", 1, "t").unwrap_err().code,
            ErrorCode::NotFound
        );
        dispatch(&mut run, "a");
        assert!(run.begin(&tid("a"), "/wt", 1, "t").is_err());
    }

    #[test]
    fn dispatching_records_the_session_and_attempt() {
        let mut run = started(1, vec![nt("a", &[])]);
        dispatch(&mut run, "a");
        let t = run.task(&tid("a")).unwrap();
        assert_eq!(t.status, TaskStatus::Running);
        assert_eq!(t.attempt, 1);
        assert_eq!(t.worktree.as_deref(), Some("/wt"));
        assert_eq!(t.session.as_ref().unwrap().pid, 100);
        assert_eq!(t.branch, "fleet/r1/a");
    }

    // ---- finish ----

    #[test]
    fn clean_exit_goes_to_review() {
        let mut run = started(1, vec![nt("a", &[])]);
        dispatch(&mut run, "a");
        let mut e = end(1, EndKind::Exited, Some(0));
        e.result = Some(WorkerResult {
            status: ResultStatus::Done,
            summary: "added x".into(),
        });
        let events = run.finish(&tid("a"), &e, POLICY).unwrap();
        assert_eq!(kinds(&events), vec!["session_ended", "task_reviewing"]);
        apply_all(&mut run, &events);
        let t = run.task(&tid("a")).unwrap();
        assert_eq!(t.status, TaskStatus::Review);
        assert_eq!(t.summary.as_deref(), Some("added x"));
        assert!(t.session.is_none());
    }

    #[test]
    fn failing_worker_is_retried_with_a_note_until_the_budget_is_used() {
        let mut run = started(1, vec![nt("a", &[])]);
        for attempt in 1..=2u32 {
            dispatch(&mut run, "a");
            let events = run
                .finish(&tid("a"), &end(attempt, EndKind::Exited, Some(3)), POLICY)
                .unwrap();
            assert_eq!(
                kinds(&events),
                vec!["session_ended", "retry_queued", "note_added"]
            );
            apply_all(&mut run, &events);
            let t = run.task(&tid("a")).unwrap();
            assert_eq!(t.status, TaskStatus::Ready);
            assert_eq!(t.retries_used, attempt);
            assert!(t.notes.last().unwrap().contains("exited with code 3"));
            assert!(t.notes.last().unwrap().contains("last words"));
        }
        dispatch(&mut run, "a");
        let events = run
            .finish(&tid("a"), &end(3, EndKind::Exited, Some(3)), POLICY)
            .unwrap();
        assert_eq!(kinds(&events), vec!["session_ended", "task_failed"]);
        apply_all(&mut run, &events);
        assert_eq!(run.task(&tid("a")).unwrap().status, TaskStatus::Failed);
    }

    #[test]
    fn zero_retries_fails_immediately() {
        let mut run = started(1, vec![nt("a", &[])]);
        dispatch(&mut run, "a");
        let events = run
            .finish(
                &tid("a"),
                &end(1, EndKind::Exited, Some(1)),
                RetryPolicy { max_retries: 0 },
            )
            .unwrap();
        assert_eq!(kinds(&events), vec!["session_ended", "task_failed"]);
    }

    #[test]
    fn every_abnormal_end_is_a_failure() {
        for outcome in [
            EndKind::Signaled,
            EndKind::Timeout,
            EndKind::IdleTimeout,
            EndKind::KillFailed,
            EndKind::SpawnFailed,
        ] {
            let mut run = started(1, vec![nt("a", &[])]);
            dispatch(&mut run, "a");
            let events = run
                .finish(&tid("a"), &end(1, outcome, None), POLICY)
                .unwrap();
            assert_eq!(
                kinds(&events),
                vec!["session_ended", "retry_queued", "note_added"],
                "{outcome}"
            );
        }
    }

    #[test]
    fn nonzero_exit_or_unknown_status_fails_even_with_a_done_result() {
        let mut run = started(1, vec![nt("a", &[])]);
        dispatch(&mut run, "a");
        let mut e = end(1, EndKind::Exited, Some(2));
        e.result = Some(WorkerResult {
            status: ResultStatus::Done,
            summary: String::new(),
        });
        let events = run.finish(&tid("a"), &e, POLICY).unwrap();
        assert!(kinds(&events).contains(&"retry_queued"));
        let mut e = end(1, EndKind::Exited, None);
        e.result = None;
        let events = run.finish(&tid("a"), &e, POLICY).unwrap();
        assert!(kinds(&events).contains(&"retry_queued"));
    }

    #[test]
    fn worker_reporting_failure_with_exit_zero_is_a_failure() {
        let mut run = started(1, vec![nt("a", &[])]);
        dispatch(&mut run, "a");
        let mut e = end(1, EndKind::Exited, Some(0));
        e.result = Some(WorkerResult {
            status: ResultStatus::Failed,
            summary: "tests do not compile".into(),
        });
        let events = run.finish(&tid("a"), &e, POLICY).unwrap();
        assert!(
            matches!(&events[1], DomainEvent::RetryQueued { reason, .. } if reason.contains("tests do not compile"))
        );
    }

    #[test]
    fn stopped_by_operator_fails_without_retry() {
        let mut run = started(1, vec![nt("a", &[])]);
        dispatch(&mut run, "a");
        let events = run
            .finish(&tid("a"), &end(1, EndKind::Stopped, None), POLICY)
            .unwrap();
        assert_eq!(kinds(&events), vec!["session_ended", "task_failed"]);
        assert!(
            matches!(&events[1], DomainEvent::TaskFailed { reason, .. } if reason.contains("operator"))
        );
    }

    #[test]
    fn orphaned_session_is_requeued_without_using_a_retry() {
        let mut run = started(1, vec![nt("a", &[])]);
        dispatch(&mut run, "a");
        let events = run
            .finish(&tid("a"), &end(1, EndKind::Orphaned, None), POLICY)
            .unwrap();
        apply_all(&mut run, &events);
        let t = run.task(&tid("a")).unwrap();
        assert_eq!(t.status, TaskStatus::Ready);
        assert_eq!(t.retries_used, 0);
    }

    #[test]
    fn orphaned_session_with_a_done_result_goes_to_review() {
        let mut run = started(1, vec![nt("a", &[])]);
        dispatch(&mut run, "a");
        let mut e = end(1, EndKind::Orphaned, None);
        e.result = Some(WorkerResult {
            status: ResultStatus::Done,
            summary: "done".into(),
        });
        let events = run.finish(&tid("a"), &e, POLICY).unwrap();
        assert_eq!(kinds(&events), vec!["session_ended", "task_reviewing"]);
    }

    #[test]
    fn contract_warning_becomes_a_warning_event() {
        let mut run = started(1, vec![nt("a", &[])]);
        dispatch(&mut run, "a");
        let mut e = end(1, EndKind::Exited, Some(0));
        e.contract_warning = Some("result.json: is a symlink".into());
        let events = run.finish(&tid("a"), &e, POLICY).unwrap();
        assert_eq!(
            kinds(&events),
            vec!["session_ended", "warning", "task_reviewing"]
        );
    }

    #[test]
    fn finish_rejects_wrong_state_or_stale_attempt() {
        let mut run = started(1, vec![nt("a", &[])]);
        assert_eq!(
            run.finish(&tid("a"), &end(1, EndKind::Exited, Some(0)), POLICY)
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        dispatch(&mut run, "a");
        assert!(run
            .finish(&tid("a"), &end(7, EndKind::Exited, Some(0)), POLICY)
            .is_err());
        assert_eq!(
            run.finish(&tid("nope"), &end(1, EndKind::Exited, Some(0)), POLICY)
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
    }

    // ---- commit and gates ----

    fn in_review() -> Run {
        let mut run = started(1, vec![nt("a", &[])]);
        dispatch(&mut run, "a");
        let events = run
            .finish(&tid("a"), &end(1, EndKind::Exited, Some(0)), POLICY)
            .unwrap();
        apply_all(&mut run, &events);
        run
    }

    fn gate(name: &str, passed: bool) -> GateOutcome {
        GateOutcome {
            name: name.into(),
            passed,
            summary: if passed {
                "ok".into()
            } else {
                "exit code 1".into()
            },
            output: if passed {
                String::new()
            } else {
                "FAILED test_x".into()
            },
        }
    }

    #[test]
    fn commit_is_recorded_only_in_review() {
        let run = in_review();
        let ev = run.record_commit(&tid("a"), sha('c')).unwrap();
        assert!(matches!(ev, DomainEvent::TaskCommitted { .. }));
        let fresh = started(1, vec![nt("a", &[])]);
        assert_eq!(
            fresh.record_commit(&tid("a"), sha('c')).unwrap_err().code,
            ErrorCode::Conflict
        );
        let mut run = in_review();
        run.apply(&ev);
        assert_eq!(run.task(&tid("a")).unwrap().head, Some(sha('c')));
    }

    #[test]
    fn passing_gates_complete_the_task() {
        let mut run = in_review();
        let events = run
            .gates_checked(
                &tid("a"),
                1,
                &[gate("lint", true), gate("tests", true)],
                POLICY,
            )
            .unwrap();
        assert_eq!(
            kinds(&events),
            vec!["gate_checked", "gate_checked", "task_completed"]
        );
        apply_all(&mut run, &events);
        let t = run.task(&tid("a")).unwrap();
        assert_eq!(t.status, TaskStatus::Completed);
        assert_eq!(t.gates.len(), 2);
    }

    #[test]
    fn no_gates_means_completion() {
        let run = in_review();
        let events = run.gates_checked(&tid("a"), 1, &[], POLICY).unwrap();
        assert_eq!(kinds(&events), vec!["task_completed"]);
    }

    #[test]
    fn failing_gate_retries_with_the_failure_text_in_the_note() {
        let mut run = in_review();
        let events = run
            .gates_checked(
                &tid("a"),
                1,
                &[gate("lint", true), gate("tests", false)],
                POLICY,
            )
            .unwrap();
        assert_eq!(
            kinds(&events),
            vec!["gate_checked", "gate_checked", "retry_queued", "note_added"]
        );
        apply_all(&mut run, &events);
        let t = run.task(&tid("a")).unwrap();
        assert_eq!(t.status, TaskStatus::Ready);
        assert_eq!(t.retries_used, 1);
        let note = t.notes.last().unwrap();
        assert!(
            note.contains("tests") && note.contains("FAILED test_x"),
            "{note}"
        );
    }

    #[test]
    fn failing_gate_fails_the_task_when_retries_are_exhausted() {
        let run = in_review();
        let events = run
            .gates_checked(
                &tid("a"),
                1,
                &[gate("tests", false)],
                RetryPolicy { max_retries: 0 },
            )
            .unwrap();
        assert_eq!(kinds(&events), vec!["gate_checked", "task_failed"]);
    }

    #[test]
    fn stale_gate_results_are_ignored() {
        let run = in_review();
        assert!(run
            .gates_checked(&tid("a"), 9, &[gate("t", false)], POLICY)
            .unwrap()
            .is_empty());
        let mut done = in_review();
        let ev = done.gates_checked(&tid("a"), 1, &[], POLICY).unwrap();
        apply_all(&mut done, &ev);
        assert!(done
            .gates_checked(&tid("a"), 1, &[gate("t", false)], POLICY)
            .unwrap()
            .is_empty());
        assert_eq!(
            run.gates_checked(&tid("zzz"), 1, &[], POLICY)
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
    }

    // ---- operator actions ----

    #[test]
    fn operator_retry_only_applies_to_failed_tasks() {
        let mut run = started(1, vec![nt("a", &[])]);
        assert_eq!(
            run.operator_retry(&tid("a")).unwrap_err().code,
            ErrorCode::Conflict
        );
        run.apply(&DomainEvent::TaskFailed {
            run: run_id(),
            task: tid("a"),
            reason: "x".into(),
        });
        let ev = run.operator_retry(&tid("a")).unwrap();
        run.apply(&ev);
        let t = run.task(&tid("a")).unwrap();
        assert_eq!(
            (t.status, t.retries_used, t.failure.clone()),
            (TaskStatus::Ready, 0, None)
        );
    }

    #[test]
    fn cancel_applies_to_waiting_tasks_only() {
        let mut run = started(1, vec![nt("a", &[]), nt("b", &["a"])]);
        assert!(run.cancel(&tid("b")).is_ok());
        assert!(run.cancel(&tid("a")).is_ok());
        dispatch(&mut run, "a");
        assert_eq!(run.cancel(&tid("a")).unwrap_err().code, ErrorCode::Conflict);
    }

    #[test]
    fn unblock_requires_a_blocked_task_and_adds_the_note_first() {
        let mut run = started(1, vec![nt("a", &[])]);
        assert!(run.unblock(&tid("a"), "x").is_err());
        run.apply(&DomainEvent::TaskBlocked {
            run: run_id(),
            task: tid("a"),
            kind: BlockKind::Question,
            subject: "g1".into(),
            detail: String::new(),
        });
        let events = run.unblock(&tid("a"), "answer: sqlite").unwrap();
        assert_eq!(kinds(&events), vec!["note_added", "task_unblocked"]);
        apply_all(&mut run, &events);
        let t = run.task(&tid("a")).unwrap();
        assert_eq!(t.status, TaskStatus::Ready);
        assert_eq!(t.notes, vec!["answer: sqlite"]);
        assert_eq!(t.questions, 1);
        assert_eq!(
            run.unblock(&tid("a"), "").unwrap_err().code,
            ErrorCode::Conflict
        );
    }

    #[test]
    fn notes_are_bounded() {
        let mut run = started(1, vec![nt("a", &[])]);
        for i in 0..(MAX_NOTES_PER_TASK + 5) {
            run.apply(&DomainEvent::NoteAdded {
                run: run_id(),
                task: tid("a"),
                note: format!("n{i}"),
            });
        }
        let t = run.task(&tid("a")).unwrap();
        assert_eq!(t.notes.len(), MAX_NOTES_PER_TASK);
        assert_eq!(
            t.notes.last().unwrap(),
            &format!("n{}", MAX_NOTES_PER_TASK + 4)
        );
    }

    #[test]
    fn question_allowance_is_tracked() {
        let mut run = started(1, vec![nt("a", &[])]);
        assert!(!run.question_limit_reached(&tid("a")));
        for _ in 0..MAX_QUESTIONS_PER_TASK {
            run.apply(&DomainEvent::TaskBlocked {
                run: run_id(),
                task: tid("a"),
                kind: BlockKind::Question,
                subject: "g1".into(),
                detail: String::new(),
            });
        }
        assert!(run.question_limit_reached(&tid("a")));
    }

    // ---- merge ----

    #[test]
    fn only_completed_unmerged_tasks_are_mergeable() {
        let mut run = in_review();
        assert!(run.check_mergeable(&tid("a")).is_err());
        let events = run.gates_checked(&tid("a"), 1, &[], POLICY).unwrap();
        apply_all(&mut run, &events);
        assert!(run.check_mergeable(&tid("a")).is_ok());
        run.apply(&DomainEvent::TaskMerged {
            run: run_id(),
            task: tid("a"),
            merge_sha: sha('d'),
        });
        assert_eq!(
            run.check_mergeable(&tid("a")).unwrap_err().code,
            ErrorCode::Conflict
        );
        assert_eq!(
            run.check_mergeable(&tid("zzz")).unwrap_err().code,
            ErrorCode::NotFound
        );
    }

    #[test]
    fn finished_means_every_task_is_completed_or_failed() {
        let mut run = started(1, vec![nt("a", &[]), nt("b", &[])]);
        assert!(!run.is_finished());
        run.apply(&DomainEvent::TaskCompleted {
            run: run_id(),
            task: tid("a"),
        });
        assert!(!run.is_finished());
        run.apply(&DomainEvent::TaskFailed {
            run: run_id(),
            task: tid("b"),
            reason: "x".into(),
        });
        assert!(run.is_finished());
        assert!(!new_run(1).is_finished());
    }

    #[test]
    fn events_for_other_runs_are_ignored() {
        let mut run = started(1, vec![nt("a", &[])]);
        let other = RunId::parse("r9").unwrap();
        run.apply(&DomainEvent::TaskFailed {
            run: other.clone(),
            task: tid("a"),
            reason: "x".into(),
        });
        run.apply(&DomainEvent::RunStarted {
            run: other,
            max_parallel: 9,
        });
        assert_eq!(run.task(&tid("a")).unwrap().status, TaskStatus::Ready);
        assert_eq!(run.max_parallel, 1);
    }

    #[test]
    fn integration_branch_name() {
        assert_eq!(new_run(1).integration_branch(), "fleet/r1/integration");
        let mut run = new_run(1);
        run.apply(&DomainEvent::IntegrationCreated { run: run_id() });
        assert!(run.integration_ready);
    }
}
