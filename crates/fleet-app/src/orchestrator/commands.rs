//! Command handling: the use cases an operator can invoke.

use super::{Key, Orchestrator};
use crate::api::{Command, NewRunArgs, Reply};
use crate::ports::{IntegrationMerge, MergeOutcome};
use crate::views::{self, StatusContext};
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::events::DomainEvent;
use fleet_domain::fleet::{MergeGate, NewRun};
use fleet_domain::ids::{RunId, TaskId};
use fleet_domain::run::NewTask;
use fleet_domain::sanitize::sanitize_text;
use fleet_domain::status::{EndKind, TaskStatus};
use fleet_domain::value::Value;
use std::collections::BTreeSet;

fn data(schema: &str, pairs: Vec<(&str, Value)>) -> Reply {
    let mut map: Vec<(String, Value)> = vec![("schema".to_string(), Value::str(schema))];
    map.extend(pairs.into_iter().map(|(k, v)| (k.to_string(), v)));
    Reply::Data(Value::object(map))
}

fn qualified(run: &RunId, task: &TaskId) -> String {
    format!("{run}/{task}")
}

impl Orchestrator {
    /// Handles one command. Read-only commands never change state; the others record events.
    pub fn handle(&mut self, cmd: Command) -> Result<Reply> {
        // Expired approvals must not be decided or used: settle time-based state first.
        let _ = self.expire_approvals_now();
        match cmd {
            Command::Ping => Ok(data("fleet.ping/1", vec![("pong", Value::Bool(true))])),
            Command::Info => Ok(self.info()),
            Command::Status { run } => Ok(Reply::Data(self.status(run.as_ref()))),
            Command::CreateRun(args) => self.create_run(args),
            Command::AddTask { run, task } => self.add_tasks(&run, vec![task], "fleet.task.add/1"),
            Command::LoadPlan {
                run,
                new_run,
                tasks,
            } => self.load_plan(run, new_run, tasks),
            Command::StartRun { run, max_parallel } => self.start_run(&run, max_parallel),
            Command::Stop { task } => self.stop(&task),
            Command::Retry { task } => self.retry(&task),
            Command::Cancel { task } => self.cancel(&task),
            Command::Tail { task, since, max } => self.tail(&task, since, max),
            Command::Send {
                task,
                text,
                newline,
            } => self.send(&task, &text, newline),
            Command::Attach { task } => self.attach(&task),
            Command::GateList => Ok(self.gate_list()),
            Command::ResolveGate { id, answer } => {
                let events = self.state.resolve_decision(&id, &answer)?;
                let task = self
                    .state
                    .decision(&id)
                    .map(|d| qualified(&d.run, &d.task))?;
                self.commit(events)?;
                self.tick();
                Ok(data(
                    "fleet.gate.resolve/1",
                    vec![("id", Value::str(id.as_str())), ("task", Value::str(task))],
                ))
            }
            Command::ApprovalList => Ok(self.approval_list()),
            Command::Decide {
                id,
                approve,
                digest,
            } => {
                let now = self.now();
                let events = self.state.decide_approval(
                    &id,
                    approve,
                    digest.as_deref(),
                    now,
                    self.ports.digest.as_ref(),
                )?;
                let digest = self.state.approval(&id).map(|a| a.digest.clone())?;
                self.commit(events)?;
                self.tick();
                Ok(data(
                    "fleet.approval.decide/1",
                    vec![
                        ("id", Value::str(id.as_str())),
                        (
                            "decision",
                            Value::str(if approve { "approved" } else { "denied" }),
                        ),
                        ("digest", Value::str(digest)),
                    ],
                ))
            }
            Command::Merge { task } => self.merge(&task),
            Command::Cleanup { run, branches } => self.cleanup(run.as_ref(), branches),
            Command::Undo { task, apply } => self.undo(&task, apply),
            Command::Events { limit } => Ok(data(
                "fleet.events/1",
                vec![("events", views::recent_value(&self.recent, limit))],
            )),
            Command::Shutdown => {
                self.shutdown_requested = true;
                self.begin_shutdown();
                Ok(data(
                    "fleet.shutdown/1",
                    vec![("stopping", Value::Bool(true))],
                ))
            }
        }
    }

    fn expire_approvals_now(&mut self) -> Result<()> {
        let events = self.state.expire_approvals(self.now());
        self.commit(events)
    }

    fn resolve(&self, reference: &str) -> Result<Key> {
        self.state.find_task(reference)
    }

    fn info(&self) -> Reply {
        let runs = self.state.runs().count();
        data(
            "fleet.info/1",
            vec![
                ("version", Value::str(&self.version)),
                ("max_sessions", Value::int(self.cfg.max_sessions as u64)),
                ("sessions", Value::int(self.live.len() as u64)),
                ("undo_available", Value::Bool(self.ports.undo.available())),
                ("runs", Value::int(runs as u64)),
                ("tasks", Value::int(self.state.total_tasks() as u64)),
                (
                    "merge_needs_approval",
                    Value::Bool(self.cfg.merge_needs_approval),
                ),
                ("external_commands", Value::Bool(self.cfg.external_commands)),
                ("gates", Value::int(self.cfg.gates.len() as u64)),
                (
                    "workers",
                    Value::array(self.cfg.workers.keys().map(|w| Value::str(w.as_str()))),
                ),
            ],
        )
    }

    fn status(&self, only: Option<&RunId>) -> Value {
        let live: BTreeSet<Key> = self.live.keys().cloned().collect();
        let now = self.now();
        views::status_value(
            &self.state,
            only,
            &StatusContext {
                now_ms: now,
                live: &live,
                recent: &self.recent,
                pid: self.pid,
                version: &self.version,
                uptime_ms: now.saturating_sub(self.started_ms),
                max_sessions: self.cfg.max_sessions,
                undo_available: self.ports.undo.available(),
            },
        )
    }

    fn gate_list(&self) -> Reply {
        data(
            "fleet.gate.list/1",
            vec![(
                "gates",
                Value::array(self.state.decisions().map(views::gate_value)),
            )],
        )
    }

    fn approval_list(&self) -> Reply {
        let now = self.now();
        data(
            "fleet.approval.list/1",
            vec![(
                "approvals",
                Value::array(
                    self.state
                        .approvals()
                        .map(|a| views::approval_value(a, now)),
                ),
            )],
        )
    }

    fn create_run_event(&mut self, args: NewRunArgs) -> Result<(RunId, DomainEvent)> {
        let base_ref = args.base.unwrap_or_else(|| "HEAD".to_string());
        let base = self.ports.vcs.resolve_base(&args.repo, &base_ref)?;
        let max_parallel = args
            .max_parallel
            .unwrap_or_else(|| u32::try_from(self.cfg.max_sessions.min(4)).unwrap_or(1));
        self.state.create_run(NewRun {
            id: args.id,
            objective: args.objective,
            repo: args.repo,
            base_ref: base.ref_name,
            base_sha: base.sha,
            max_parallel,
        })
    }

    fn create_run(&mut self, args: NewRunArgs) -> Result<Reply> {
        let (id, event) = self.create_run_event(args)?;
        self.commit(vec![event])?;
        Ok(data(
            "fleet.run.create/1",
            vec![("run", Value::str(id.as_str()))],
        ))
    }

    fn add_tasks(&mut self, run: &RunId, tasks: Vec<NewTask>, schema: &str) -> Result<Reply> {
        let (ids, events) = self.state.add_tasks(run, tasks)?;
        self.commit(events)?;
        self.promote()?;
        Ok(data(
            schema,
            vec![
                ("run", Value::str(run.as_str())),
                (
                    "tasks",
                    Value::array(ids.iter().map(|t| Value::str(t.as_str()))),
                ),
            ],
        ))
    }

    fn load_plan(
        &mut self,
        run: Option<RunId>,
        new_run: Option<NewRunArgs>,
        tasks: Vec<NewTask>,
    ) -> Result<Reply> {
        let run_id = match (run, new_run) {
            (Some(_), Some(_)) => {
                return Err(FleetError::invalid(
                    "a plan names an existing run or describes a new one, not both",
                ))
            }
            (None, None) => {
                return Err(FleetError::invalid(
                    "a plan needs a run: pass --run or add a [run] section",
                ))
            }
            (Some(r), None) => {
                // Validate the tasks against the run before changing anything.
                self.state.add_tasks(&r, tasks.clone())?;
                r
            }
            (None, Some(args)) => {
                let (id, event) = self.create_run_event(args)?;
                // The tasks must be valid for the run that is about to exist; check with a
                // scratch copy of the state so an invalid plan creates nothing.
                let mut scratch = fleet_domain::fleet::FleetState::new();
                scratch.apply(&event);
                scratch.add_tasks(&id, tasks.clone())?;
                self.commit(vec![event])?;
                id
            }
        };
        self.add_tasks(&run_id, tasks, "fleet.plan/1")
    }

    fn start_run(&mut self, run: &RunId, max_parallel: Option<u32>) -> Result<Reply> {
        let event = self.state.start_run(run, max_parallel)?;
        self.commit(vec![event])?;
        self.promote()?;
        self.tick();
        let r = self.state.run(run)?;
        Ok(data(
            "fleet.run.start/1",
            vec![
                ("run", Value::str(run.as_str())),
                ("max_parallel", Value::int(r.max_parallel)),
                ("started", Value::Bool(r.started)),
            ],
        ))
    }

    fn stop(&mut self, reference: &str) -> Result<Reply> {
        let key = self.resolve(reference)?;
        let status = self.task_status(&key);
        if self.begin_stop(&key, EndKind::Stopped) {
            return Ok(data(
                "fleet.task.stop/1",
                vec![
                    ("task", Value::str(qualified(&key.0, &key.1))),
                    ("status", Value::str("stopping")),
                ],
            ));
        }
        match status {
            Some(TaskStatus::Pending | TaskStatus::Ready | TaskStatus::Blocked) => {
                self.cancel(reference)
            }
            Some(other) => Err(FleetError::conflict(format!(
                "task {} is {other}; only running tasks can be stopped",
                qualified(&key.0, &key.1)
            ))),
            None => Err(FleetError::not_found(format!(
                "task {reference} does not exist"
            ))),
        }
    }

    fn retry(&mut self, reference: &str) -> Result<Reply> {
        let key = self.resolve(reference)?;
        let event = self.state.run(&key.0)?.operator_retry(&key.1)?;
        self.commit(vec![event])?;
        self.promote()?;
        self.tick();
        Ok(data(
            "fleet.task.retry/1",
            vec![
                ("task", Value::str(qualified(&key.0, &key.1))),
                ("status", Value::str("ready")),
            ],
        ))
    }

    fn cancel(&mut self, reference: &str) -> Result<Reply> {
        let key = self.resolve(reference)?;
        let event = self.state.run(&key.0)?.cancel(&key.1)?;
        self.commit(vec![event])?;
        self.promote()?;
        Ok(data(
            "fleet.task.cancel/1",
            vec![
                ("task", Value::str(qualified(&key.0, &key.1))),
                ("status", Value::str("failed")),
            ],
        ))
    }

    fn tail(&self, reference: &str, since: Option<u64>, max: usize) -> Result<Reply> {
        let key = self.resolve(reference)?;
        let name = qualified(&key.0, &key.1);
        if let Some(live) = self.live.get(&key) {
            let (text, next, lost) = match since {
                Some(offset) => match self.ports.sessions.read_since(live.session, offset, max) {
                    Some(c) => (c.text, c.next, c.lost),
                    None => (String::new(), offset, false),
                },
                None => {
                    let text = self
                        .ports
                        .sessions
                        .snapshot(live.session, max)
                        .unwrap_or_default();
                    let next = self
                        .ports
                        .sessions
                        .read_since(live.session, u64::MAX, 0)
                        .map_or(0, |c| c.next);
                    (text, next, false)
                }
            };
            return Ok(data(
                "fleet.tail/1",
                vec![
                    ("task", Value::str(name)),
                    ("live", Value::Bool(true)),
                    ("text", Value::str(text)),
                    ("next", Value::int(next)),
                    ("lost", Value::Bool(lost)),
                ],
            ));
        }
        let text = self
            .ports
            .logs
            .tail(&key.0, &key.1, max)
            .unwrap_or_default();
        Ok(data(
            "fleet.tail/1",
            vec![
                ("task", Value::str(name)),
                ("live", Value::Bool(false)),
                ("text", Value::str(text)),
                ("next", Value::int(0u64)),
                ("lost", Value::Bool(false)),
            ],
        ))
    }

    fn send(&mut self, reference: &str, text: &str, newline: bool) -> Result<Reply> {
        let key = self.resolve(reference)?;
        let live = self.live.get(&key).ok_or_else(|| {
            FleetError::conflict(format!(
                "task {} has no running session",
                qualified(&key.0, &key.1)
            ))
        })?;
        let mut bytes = text.as_bytes().to_vec();
        if newline {
            bytes.push(b'\r');
        }
        let count = bytes.len() as u64;
        Ok(Reply::Input {
            session: live.session,
            bytes,
            data: Value::object([
                ("schema", Value::str("fleet.send/1")),
                ("task", Value::str(qualified(&key.0, &key.1))),
                ("bytes", Value::int(count)),
            ]),
        })
    }

    fn attach(&mut self, reference: &str) -> Result<Reply> {
        let key = self.resolve(reference)?;
        let live = self.live.get(&key).ok_or_else(|| {
            FleetError::conflict(format!(
                "task {} has no running session",
                qualified(&key.0, &key.1)
            ))
        })?;
        Ok(Reply::Attach {
            session: live.session,
            data: Value::object([
                ("schema", Value::str("fleet.attach/1")),
                ("task", Value::str(qualified(&key.0, &key.1))),
            ]),
        })
    }

    fn merge(&mut self, reference: &str) -> Result<Reply> {
        let key = self.resolve(reference)?;
        let (repo, branch, integration, base, title) = {
            let run = self.state.run(&key.0)?;
            let task = run.check_mergeable(&key.1)?;
            (
                run.repo.clone(),
                task.branch.clone(),
                run.integration_branch(),
                run.base_sha.clone(),
                task.title.clone(),
            )
        };
        let head = self.ports.vcs.branch_head(&repo, &branch)?;
        let now = self.now();
        let gate = self.state.request_merge(
            &key.0,
            &key.1,
            &head,
            &self.policy,
            now,
            self.ports.digest.as_ref(),
        )?;
        match gate {
            MergeGate::Pending { approval, events } => {
                self.commit(events)?;
                let digest = self.state.approval(&approval).map(|a| a.digest.clone())?;
                let prefix = digest.get(..12).unwrap_or(&digest).to_string();
                Err(FleetError::new(
                    ErrorCode::ApprovalRequired,
                    format!(
                        "merging {} needs approval {approval} (digest {prefix}); approve it with `fleet approve {approval}` and run the merge again",
                        qualified(&key.0, &key.1)
                    ),
                )
                .with_subject(approval.as_str()))
            }
            MergeGate::Proceed(events) => {
                self.commit(events)?;
                let result = self.ports.vcs.merge_into_integration(&IntegrationMerge {
                    repo,
                    run: key.0.clone(),
                    task: key.1.clone(),
                    base,
                    integration_branch: integration.clone(),
                    task_branch: branch.clone(),
                    message: format!("fleet: merge {} into {integration}: {title}", key.1),
                    identity: self.cfg.author.clone(),
                })?;
                match result.outcome {
                    MergeOutcome::Merged { sha } => {
                        let events = self.state.merge_succeeded(
                            &key.0,
                            &key.1,
                            sha.clone(),
                            result.created,
                        )?;
                        self.commit(events)?;
                        Ok(data(
                            "fleet.merge/1",
                            vec![
                                ("task", Value::str(qualified(&key.0, &key.1))),
                                ("branch", Value::str(branch)),
                                ("integration_branch", Value::str(integration)),
                                ("merge_sha", Value::str(sha.as_str())),
                            ],
                        ))
                    }
                    MergeOutcome::Conflict { files } => {
                        let events = self.state.merge_conflicted(
                            &key.0,
                            &key.1,
                            files.clone(),
                            result.created,
                        )?;
                        let gate = events.iter().find_map(|e| match e {
                            DomainEvent::DecisionOpened { id, .. } => Some(id.to_string()),
                            _ => None,
                        });
                        self.commit(events)?;
                        let listed = sanitize_text(files.join(", ").as_bytes());
                        let mut err = FleetError::conflict(format!(
                            "merging {} into {integration} conflicted in: {listed}. The merge was aborted and the integration branch is unchanged; the task is blocked behind a decision gate",
                            qualified(&key.0, &key.1)
                        ));
                        if let Some(g) = gate {
                            err = err.with_subject(g);
                        }
                        Err(err)
                    }
                }
            }
        }
    }

    fn cleanup(&mut self, only: Option<&RunId>, branches: bool) -> Result<Reply> {
        struct Item {
            key: Key,
            repo: String,
            worktree: String,
            branch: String,
            integration: String,
            merged_into_integration: bool,
        }
        let items: Vec<Item> = self
            .state
            .runs()
            .filter(|r| only.is_none_or(|o| &r.id == o))
            .flat_map(|r| {
                r.tasks()
                    .iter()
                    .filter(|t| {
                        (t.status == TaskStatus::Completed && t.merged.is_some())
                            || t.status == TaskStatus::Failed
                    })
                    .map(|t| Item {
                        key: (r.id.clone(), t.id.clone()),
                        repo: r.repo.clone(),
                        worktree: t.worktree.clone().unwrap_or_default(),
                        branch: t.branch.clone(),
                        integration: r.integration_branch(),
                        merged_into_integration: t.merged.is_some(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut removed = Vec::new();
        let mut deleted = Vec::new();
        for item in items {
            if self.live.contains_key(&item.key) {
                continue;
            }
            let name = qualified(&item.key.0, &item.key.1);
            if !item.worktree.is_empty() {
                match self.ports.vcs.remove_worktree(
                    &item.repo,
                    &item.worktree,
                    &item.branch,
                    false,
                ) {
                    Ok(()) => {
                        self.commit(vec![DomainEvent::WorktreeRemoved {
                            run: item.key.0.clone(),
                            task: item.key.1.clone(),
                        }])?;
                        self.ports.logs.remove(&item.key.0, &item.key.1);
                        removed.push(Value::str(&name));
                    }
                    Err(e) => self.warn(format!("{name}: cleanup failed: {}", e.message)),
                }
            }
            // Only branches whose work is already in the integration branch may be deleted.
            if branches && item.merged_into_integration {
                let merged = self
                    .ports
                    .vcs
                    .is_merged(&item.repo, &item.branch, &item.integration)
                    .unwrap_or(false);
                if merged {
                    match self
                        .ports
                        .vcs
                        .remove_worktree(&item.repo, "", &item.branch, true)
                    {
                        Ok(()) => deleted.push(Value::str(&item.branch)),
                        Err(e) => self.warn(format!(
                            "{name}: could not delete the branch: {}",
                            e.message
                        )),
                    }
                }
            }
        }
        Ok(data(
            "fleet.cleanup/1",
            vec![
                ("removed", Value::array(removed)),
                ("branches_deleted", Value::array(deleted)),
            ],
        ))
    }

    fn undo(&mut self, reference: &str, apply: bool) -> Result<Reply> {
        let key = self.resolve(reference)?;
        if !self.ports.undo.available() {
            return Err(FleetError::unavailable(
                "undo is not available: the optional `agent-undo` tool was not found on PATH",
            ));
        }
        if self.live.contains_key(&key) {
            return Err(FleetError::conflict(format!(
                "task {} is running; stop it before undoing its changes",
                qualified(&key.0, &key.1)
            )));
        }
        let worktree = self
            .state
            .run(&key.0)?
            .task(&key.1)
            .and_then(|t| t.worktree.clone())
            .ok_or_else(|| FleetError::conflict("the task has no worktree"))?;
        let report = self.ports.undo.undo(&worktree, apply)?;
        Ok(data(
            "fleet.undo/1",
            vec![
                ("task", Value::str(qualified(&key.0, &key.1))),
                ("applied", Value::Bool(apply)),
                ("report", Value::str(sanitize_text(report.as_bytes()))),
            ],
        ))
    }
}
