//! Scheduling: starting workers for ready tasks, reviewing finished ones and running jobs.

use super::{Key, Live, Orchestrator};
use crate::ports::{GateSpec, Job, JobDone, MergeOutcome, SessionSpec, VerifyOrigin};
use fleet_domain::brief::{build_brief, expand_placeholders, BriefInput};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::events::DomainEvent;
use fleet_domain::ids::{RunId, TaskId};
use fleet_domain::limits::{MAX_BRIEF_ARG_BYTES, MAX_CONCURRENT_JOBS, MAX_NOTE};
use fleet_domain::run::WorkerEnd;
use fleet_domain::sanitize::{head_str, sanitize_text};
use fleet_domain::status::{EndKind, TaskStatus};
use fleet_domain::supervision::SupervisionPolicy;

impl Orchestrator {
    /// Starts workers for ready tasks while capacity allows.
    pub(crate) fn schedule(&mut self) -> Result<()> {
        if self.shutting_down {
            return Ok(());
        }
        let run_ids: Vec<RunId> = self.state.runs().map(|r| r.id.clone()).collect();
        for run_id in run_ids {
            let free = self.cfg.max_sessions.saturating_sub(self.live.len());
            let Ok(run) = self.state.run(&run_id) else {
                continue;
            };
            let ready = run.dispatchable(free);
            for task in ready {
                if self.live.len() >= self.cfg.max_sessions {
                    return Ok(());
                }
                self.dispatch(&run_id, &task);
            }
        }
        Ok(())
    }

    /// Starts one worker. Failures that happen before the process exists are recorded as a failed
    /// attempt so they use the retry budget instead of looping.
    pub(crate) fn dispatch(&mut self, run_id: &RunId, task_id: &TaskId) {
        let key: Key = (run_id.clone(), task_id.clone());
        let Ok(run) = self.state.run(run_id) else {
            return;
        };
        let Some(task) = run.task(task_id).cloned() else {
            return;
        };
        let repo = run.repo.clone();
        let base = run.base_sha.clone();
        let objective = run.objective.clone();
        let dependencies: Vec<(TaskId, String, Option<String>)> = task
            .deps
            .iter()
            .filter_map(|d| run.task(d))
            .map(|t| (t.id.clone(), t.title.clone(), t.summary.clone()))
            .collect();
        let dep_branches: Vec<String> = task
            .deps
            .iter()
            .filter_map(|d| run.task(d))
            .filter(|t| t.status == TaskStatus::Completed)
            .map(|t| t.branch.clone())
            .collect();
        let attempt = task.attempt.saturating_add(1);

        let worker = match self.cfg.worker_for(task.worker.as_ref()) {
            Ok(w) => w.clone(),
            Err(e) => {
                self.fail_task(&key, e.message);
                return;
            }
        };
        let worktree =
            match self
                .ports
                .vcs
                .prepare_worktree(&repo, run_id, task_id, &task.branch, &base)
            {
                Ok(p) => p,
                Err(e) => {
                    self.start_failed(
                        &key,
                        attempt,
                        "",
                        &format!("could not prepare the worktree: {}", e.message),
                    );
                    return;
                }
            };
        if task.attempt == 0 && !dep_branches.is_empty() {
            match self
                .ports
                .vcs
                .merge_into_worktree(&worktree, &dep_branches, &self.cfg.author)
            {
                Ok(MergeOutcome::Merged { .. }) => {}
                Ok(MergeOutcome::Conflict { files }) => {
                    match self.state.dependency_conflicted(run_id, task_id, files) {
                        Ok(events) => {
                            if let Err(e) = self.commit(events) {
                                self.warn(e.message);
                            }
                        }
                        Err(e) => self.warn(e.message),
                    }
                    return;
                }
                Err(e) => {
                    self.start_failed(
                        &key,
                        attempt,
                        &worktree,
                        &format!(
                            "could not bring dependency work into the branch: {}",
                            e.message
                        ),
                    );
                    return;
                }
            }
        }
        let brief = build_brief(&BriefInput {
            run: run_id,
            objective: &objective,
            task: &task,
            dependencies,
            attempt,
        });
        if let Err(e) = self.ports.contract.prepare(&worktree, &brief) {
            self.start_failed(
                &key,
                attempt,
                &worktree,
                &format!("could not prepare .fleet: {}", e.message),
            );
            return;
        }
        if self.ports.undo.available() {
            self.checkpoint(&key, &worktree, "before");
        }
        let brief_file = format!("{worktree}/.fleet/brief.md");
        let brief_arg = head_str(&brief, MAX_BRIEF_ARG_BYTES).to_string();
        let vars = [
            ("brief", brief_arg.as_str()),
            ("task", task_id.as_str()),
            ("worktree", worktree.as_str()),
            ("run", run_id.as_str()),
            ("brief_file", brief_file.as_str()),
        ];
        let args: Result<Vec<String>> = worker
            .args
            .iter()
            .map(|a| expand_placeholders(a, &vars))
            .collect();
        let args = match args {
            Ok(a) => a,
            Err(e) => {
                self.start_failed(&key, attempt, &worktree, &e.message);
                return;
            }
        };
        let env = self.cfg.env.build(
            &self.cfg.parent_env,
            &worker.env_pass,
            &worker.env_set,
            &fleet_env(run_id, task_id, &worktree),
        );
        let spec = SessionSpec {
            run: run_id.clone(),
            task: task_id.clone(),
            attempt,
            program: worker.command.clone(),
            args,
            cwd: worktree.clone(),
            env,
        };
        let spawned = match self.ports.sessions.spawn(&spec) {
            Ok(s) => s,
            Err(e) => {
                self.start_failed(
                    &key,
                    attempt,
                    &worktree,
                    &format!("could not start the worker: {}", e.message),
                );
                return;
            }
        };
        let begin = self.state.run(run_id).and_then(|r| {
            r.begin(
                task_id,
                &worktree,
                u64::from(spawned.identity.pid),
                &spawned.identity.start_token,
            )
        });
        let recorded = begin.and_then(|ev| self.commit(vec![ev]));
        if let Err(e) = recorded {
            // The session exists but the start could not be recorded: do not leave it running
            // unsupervised.
            let _ = self
                .ports
                .sessions
                .signal(spawned.id, crate::ports::Signal::Kill);
            let _ = self.ports.sessions.release(spawned.id);
            self.warn(format!(
                "{run_id}/{task_id}: could not record the start: {}",
                e.message
            ));
            return;
        }
        self.live.insert(
            key,
            Live {
                session: spawned.id,
                attempt,
                worktree,
                supervision: SupervisionPolicy {
                    timeout_ms: worker.timeout_ms,
                    idle_timeout_ms: worker.idle_timeout_ms,
                },
                kill: None,
                question_seen_ms: None,
            },
        );
    }

    /// Records an attempt that never got a process: a dispatch followed by a failed end.
    fn start_failed(&mut self, key: &Key, attempt: u32, worktree: &str, message: &str) {
        let begin = self
            .state
            .run(&key.0)
            .and_then(|r| r.begin(&key.1, worktree, 0, ""));
        let begun = begin.and_then(|ev| self.commit(vec![ev]));
        if let Err(e) = begun {
            self.warn(format!("{}/{}: {}", key.0, key.1, e.message));
            return;
        }
        let end = WorkerEnd {
            attempt,
            outcome: EndKind::SpawnFailed,
            exit_code: None,
            output_tail: head_str(&sanitize_text(message.as_bytes()), MAX_NOTE).to_string(),
            stripped: 0,
            result: None,
            question: None,
            contract_warning: None,
        };
        self.settle(key, end);
    }

    pub(crate) fn checkpoint(&mut self, key: &Key, worktree: &str, label: &str) {
        let full = format!("fleet {label} {}/{}", key.0, key.1);
        match self.ports.undo.checkpoint(worktree, &full) {
            Ok(info) => {
                let ev = DomainEvent::CheckpointRecorded {
                    run: key.0.clone(),
                    task: key.1.clone(),
                    label: label.to_string(),
                    info: head_str(&sanitize_text(info.as_bytes()), 400).to_string(),
                };
                if let Err(e) = self.commit(vec![ev]) {
                    self.warn(e.message);
                }
            }
            Err(e) => self.warn(format!(
                "{}/{}: undo checkpoint failed: {}",
                key.0, key.1, e.message
            )),
        }
    }

    /// Takes tasks whose worker finished through commit and quality gates.
    pub(crate) fn review_scan(&mut self) -> Result<()> {
        let keys: Vec<Key> = self
            .state
            .runs()
            .flat_map(|r| {
                r.tasks()
                    .iter()
                    .filter(|t| t.status == TaskStatus::Review)
                    .map(|t| (r.id.clone(), t.id.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        for key in keys {
            self.begin_review(&key);
        }
        Ok(())
    }

    /// Commits the worker's output and starts the quality gates (or completes the task when no
    /// gates are configured). Safe to call repeatedly: each step runs once per attempt.
    pub(crate) fn begin_review(&mut self, key: &Key) {
        let Ok(run) = self.state.run(&key.0) else {
            return;
        };
        let Some(task) = run.task(&key.1).cloned() else {
            return;
        };
        if task.status != TaskStatus::Review {
            return;
        }
        let rkey = (key.0.clone(), key.1.clone(), task.attempt);
        if self.gate_jobs.contains(&rkey) {
            return;
        }
        let repo = run.repo.clone();
        let Some(worktree) = task.worktree.clone() else {
            self.fail_task(key, "the task has no worktree to review".to_string());
            return;
        };
        if !self.committed.contains(&rkey) {
            let message = clip_line(&format!("fleet: {} - {}", task.id, task.title));
            let head = match self
                .ports
                .vcs
                .commit_all(&worktree, &message, &self.cfg.author)
            {
                Ok(Some(sha)) => Ok(sha),
                Ok(None) => self.ports.vcs.branch_head(&repo, &task.branch),
                Err(e) => Err(e),
            };
            let head = match head {
                Ok(h) => h,
                Err(e) => {
                    self.fail_task(
                        key,
                        format!("could not commit the worker's output: {}", e.message),
                    );
                    return;
                }
            };
            let recorded = self
                .state
                .run(&key.0)
                .and_then(|r| r.record_commit(&key.1, head))
                .and_then(|ev| self.commit(vec![ev]));
            if let Err(e) = recorded {
                self.warn(e.message);
                return;
            }
            if self.ports.undo.available() {
                self.checkpoint(key, &worktree, "after");
            }
            self.committed.insert(rkey.clone());
        }
        // The contracts of the goals this task works on run first, in fleet's own run: nothing the
        // worker said counts. Only a pass lets the review go on to the quality gates.
        let targets = self.verification_targets(&key.0, &key.1);
        if !targets.is_empty() && !self.verified.contains(&rkey) {
            if self.verifying.contains_key(key) || self.ports.jobs.running() >= MAX_CONCURRENT_JOBS
            {
                return;
            }
            if let Err(e) =
                self.start_verification(key, task.attempt, VerifyOrigin::Review, &worktree, targets)
            {
                self.fail_task(
                    key,
                    format!("could not start the verification: {}", e.message),
                );
            }
            return;
        }
        if self.cfg.gates.is_empty() {
            self.committed.remove(&rkey);
            self.apply_gate_results(key, task.attempt, &[]);
            return;
        }
        if self.ports.jobs.running() >= MAX_CONCURRENT_JOBS {
            return;
        }
        let forced = fleet_env(&key.0, &key.1, &worktree);
        let brief_file = format!("{worktree}/.fleet/brief.md");
        let mut specs = Vec::new();
        for g in &self.cfg.gates {
            let vars = [
                ("brief", ""),
                ("task", key.1.as_str()),
                ("worktree", worktree.as_str()),
                ("run", key.0.as_str()),
                ("brief_file", brief_file.as_str()),
            ];
            let args: Result<Vec<String>> = g
                .args
                .iter()
                .map(|a| expand_placeholders(a, &vars))
                .collect();
            match args {
                Ok(args) => specs.push(GateSpec {
                    name: g.name.clone(),
                    program: g.command.clone(),
                    args,
                    env: self
                        .cfg
                        .env
                        .build(&self.cfg.parent_env, &g.env_pass, &g.env_set, &forced),
                    format: g.format,
                    timeout_ms: g.timeout_ms,
                }),
                Err(e) => {
                    self.fail_task(key, format!("gate {}: {}", g.name, e.message));
                    return;
                }
            }
        }
        let job = Job::Gates {
            run: key.0.clone(),
            task: key.1.clone(),
            attempt: task.attempt,
            worktree,
            gates: specs,
        };
        match self.ports.jobs.start(job) {
            Ok(()) => {
                self.gate_jobs.insert(rkey);
            }
            Err(e) => self.fail_task(
                key,
                format!("could not start the quality gates: {}", e.message),
            ),
        }
    }

    pub(crate) fn apply_gate_results(
        &mut self,
        key: &Key,
        attempt: u32,
        outcomes: &[fleet_domain::quality::GateOutcome],
    ) {
        // A second line of defence for the rule that a task with a contract completes only after
        // fleet's own run passed it: results that would complete such a task without that pass
        // are dropped, and the review goes through verification again.
        let rkey = (key.0.clone(), key.1.clone(), attempt);
        let unguarded =
            self.verification_targets(&key.0, &key.1).is_empty() || self.verified.contains(&rkey);
        if outcomes.iter().all(|o| o.passed) && !unguarded {
            self.committed.remove(&rkey);
            return;
        }
        self.verified.remove(&rkey);
        let events = self
            .state
            .run(&key.0)
            .and_then(|r| r.gates_checked(&key.1, attempt, outcomes, self.policy.retry));
        match events.and_then(|e| self.commit(e)) {
            Ok(()) => {}
            Err(e) => self.warn(format!("{}/{}: {}", key.0, key.1, e.message)),
        }
    }

    /// Results of background jobs.
    pub(crate) fn collect_jobs(&mut self) -> Result<()> {
        for done in self.ports.jobs.poll() {
            match done {
                JobDone::Gates {
                    run,
                    task,
                    attempt,
                    outcomes,
                } => {
                    let rkey = (run.clone(), task.clone(), attempt);
                    self.gate_jobs.remove(&rkey);
                    self.committed.remove(&rkey);
                    self.apply_gate_results(&(run, task), attempt, &outcomes);
                }
                JobDone::Verify(done) => self.handle_verify_done(*done),
                JobDone::Command {
                    approval,
                    exit_code,
                    summary,
                    output,
                } => {
                    self.command_jobs.remove(&approval);
                    let result = self
                        .state
                        .external_ran(&approval, exit_code, &summary, &output)
                        .and_then(|e| self.commit(e));
                    if let Err(e) = result {
                        self.warn(format!("approval {approval}: {}", e.message));
                    }
                }
            }
        }
        Ok(())
    }

    /// Runs commands that a human approved on behalf of a worker. The approval is consumed, and
    /// the start recorded, *before* anything is launched: a restart, a slow command or an
    /// expired window can never make the same approval run a command twice. What happens to a
    /// command that was started and never reported back is settled in `startup`.
    pub(crate) fn start_commands(&mut self) -> Result<()> {
        let runnable: Vec<(
            fleet_domain::ids::ApprovalId,
            RunId,
            TaskId,
            String,
            Vec<String>,
        )> = self
            .state
            .runnable_commands()
            .into_iter()
            .filter(|a| !self.command_jobs.contains(&a.id))
            .map(|a| {
                (
                    a.id.clone(),
                    a.run.clone(),
                    a.task.clone(),
                    a.action.command.clone(),
                    a.action.args.clone(),
                )
            })
            .collect();
        for (id, run, task, program, args) in runnable {
            if self.ports.jobs.running() >= MAX_CONCURRENT_JOBS {
                break;
            }
            let now = self.now();
            let started = self
                .state
                .start_command(&id, now, self.ports.digest.as_ref())
                .and_then(|events| self.commit(events));
            if let Err(e) = started {
                // Nothing was recorded, so nothing runs; the approval is looked at again later.
                self.warn(format!("approval {id}: {}", e.message));
                continue;
            }
            if !program.starts_with('/') {
                let _ =
                    self.finish_command(&id, None, "refused: the command is not an absolute path");
                continue;
            }
            let worktree = self
                .state
                .run(&run)
                .ok()
                .and_then(|r| r.task(&task))
                .and_then(|t| t.worktree.clone());
            let Some(worktree) = worktree else {
                let _ = self.finish_command(
                    &id,
                    None,
                    "the task has no worktree to run the command in",
                );
                continue;
            };
            let env = self.cfg.env.build(
                &self.cfg.parent_env,
                &[],
                &[],
                &fleet_env(&run, &task, &worktree),
            );
            let job = Job::Command {
                approval: id.clone(),
                worktree,
                program,
                args,
                env,
                timeout_ms: self.cfg.external_timeout_ms,
            };
            match self.ports.jobs.start(job) {
                Ok(()) => {
                    self.command_jobs.insert(id);
                }
                Err(e) => {
                    let _ =
                        self.finish_command(&id, None, &format!("could not start: {}", e.message));
                }
            }
        }
        Ok(())
    }

    fn finish_command(
        &mut self,
        id: &fleet_domain::ids::ApprovalId,
        code: Option<i64>,
        summary: &str,
    ) -> Result<()> {
        let events = self
            .state
            .external_ran(id, code, summary, "")
            .map_err(|e| FleetError::new(e.code, e.message))?;
        self.commit(events)
    }
}

/// Variables fleet itself sets for children. Never the socket, the state directory or a token:
/// `XDG_STATE_HOME` points inside the worktree, so a `fleet` run by an agent looks for the
/// daemon's socket in a directory that is not the operator's.
pub(crate) fn fleet_env(run: &RunId, task: &TaskId, worktree: &str) -> Vec<(String, String)> {
    vec![
        ("FLEET_RUN".to_string(), run.to_string()),
        ("FLEET_TASK".to_string(), task.to_string()),
        ("FLEET_WORKTREE".to_string(), worktree.to_string()),
        (
            "FLEET_BRIEF_FILE".to_string(),
            format!("{worktree}/.fleet/brief.md"),
        ),
        (
            "XDG_STATE_HOME".to_string(),
            format!("{worktree}/.fleet/xdg-state"),
        ),
    ]
}

fn clip_line(s: &str) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    head_str(&flat, 200).to_string()
}
