//! Context packs and handoffs on top of the orchestrator: collecting what a pack is compiled from,
//! compiling it when a worker starts (for runs that chose `context = "compiled"`), and making
//! handoff packets when an attempt ends, when a task goes back in line and when the daemon finds a
//! task running after a crash.
//!
//! Every input comes from fleet's own state: the fleet state, the goal ledger, the event log, the
//! configuration and the *repository* at commits fleet recorded (never the worker-writable worktree,
//! ADR 16). What a worker wrote reaches the compiler only through [`UnverifiedInput`], which the
//! compiler renders in layer 7 alone, labelled and quoted.

use super::context_state::HandoffClass;
use crate::orchestrator::{Key, Orchestrator};
use crate::ports::{StoredPack, TreeFile, TreeListing};
use fleet_domain::contract::ContractRead;
use fleet_domain::error::{FleetError, Result};
use fleet_domain::events::DomainEvent;
use fleet_domain::redact::Redactor;
use fleet_domain::status::{ContextMode, TaskStatus, Verdict};
use fleet_domain::value::Document;
use neo_context::compile::Compiler;
use neo_context::error::ContextError;
use neo_context::handoff::{HandoffPacket, HandoffParts, HandoffTrigger};
use neo_context::input::*;
use neo_context::invariants::builtin_glossary;
use neo_context::pack::Pack;
use neo_goals::goal::Goal;
use neo_verify::contract::{Check, CheckKind, FileAssertion};
use neo_verify::glob::Glob;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Files listed per commit when selecting the working set.
const MAX_TREE_FILES: usize = 20_000;
/// Globs of a `deny` list written out in a check's line of layer 4 (the rest is counted).
const DENY_SHOWN: usize = 6;
/// Files larger than this are never read for an excerpt.
const MAX_FILE_READ: u64 = 1024 * 1024;
const MAX_CHANGED: usize = 10_000;
/// Committed files of the worker's own work shown (UNVERIFIED) in layer 7.
const WORK_FILES: usize = 4;

pub(crate) fn context_error(e: ContextError) -> FleetError {
    match e {
        ContextError::LayerOverflow { .. } | ContextError::PackOverflow { .. } => {
            FleetError::limit(e.to_string())
        }
        ContextError::InvalidInput(_) | ContextError::InvalidProfile(_) => {
            FleetError::invalid(e.to_string())
        }
        ContextError::Malformed(_) => FleetError::internal(e.to_string()),
    }
}

fn join_command(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A check as an agent reads it in layer 4: its kind, and what it does in one line.
pub(crate) fn describe_check(check: &Check) -> (String, String) {
    let what = match &check.kind {
        CheckKind::Command {
            program,
            args,
            expect_exit,
            timeout_ms,
        } => format!(
            "{} (must exit with {expect_exit}; at most {} s)",
            join_command(program, args),
            timeout_ms / 1000
        ),
        CheckKind::RefcheckJson {
            program,
            args,
            timeout_ms,
        } => format!(
            "{} (its stdout must be a refcheck JSON report without errors; at most {} s)",
            join_command(program, args),
            timeout_ms / 1000
        ),
        CheckKind::File { path, assertion } => match assertion {
            FileAssertion::Exists => format!("{path} exists"),
            FileAssertion::Sha256(hash) => format!("{path} has the sha256 {hash}"),
            FileAssertion::Contains(text) => format!("{path} contains {text:?}"),
        },
        CheckKind::DiffOnly { allow, deny } => {
            let list = |globs: &[Glob]| {
                globs
                    .iter()
                    .map(Glob::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            // a deny list can be long (the protected paths of a repository): a few globs and the
            // count keep the line within what a pack gives a check, and say that there are more
            let denied = |globs: &[Glob]| {
                let shown = globs.iter().take(DENY_SHOWN).map(Glob::as_str);
                let mut text = shown.collect::<Vec<_>>().join(", ");
                if globs.len() > DENY_SHOWN {
                    text.push_str(&format!(" and {} more", globs.len() - DENY_SHOWN));
                }
                text
            };
            match (allow.is_empty(), deny.is_empty()) {
                (_, true) => format!(
                    "every path your commits change matches one of: {}",
                    list(allow)
                ),
                (true, false) => format!(
                    "no path your commits change matches any of: {}",
                    denied(deny)
                ),
                (false, false) => format!(
                    "every path your commits change matches one of: {}; and none matches any of: {}",
                    list(allow),
                    denied(deny)
                ),
            }
        }
    };
    (check.kind.name().to_string(), what)
}

/// Goals in the order of their ids. The ledger lists them in the order they were added; a pack
/// must not depend on that, only on which goals there are.
fn by_id(mut goals: Vec<&Goal>) -> Vec<&Goal> {
    goals.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    goals
}

fn short(s: &str) -> String {
    s.get(..12).unwrap_or(s).to_string()
}

/// The working set, the paths the task's commits changed with their total, the committed files of
/// the worker's own work, and every path that was a candidate.
type RepositoryInputs = (
    WorkingSetInput,
    Vec<ChangedPath>,
    u64,
    Vec<Excerpt>,
    Vec<String>,
);

impl Orchestrator {
    pub(crate) fn redactor(&self) -> Arc<Redactor> {
        Arc::clone(&self.cfg.redactor)
    }

    /// The compiler for a task: the profile of the worker that runs it, the configured policy for
    /// text agents wrote, and the daemon's redactor.
    pub(crate) fn compiler_for(&self, key: &Key) -> Result<Compiler> {
        let task = self
            .state
            .run(&key.0)?
            .task(&key.1)
            .ok_or_else(|| FleetError::not_found(format!("no task {}/{}", key.0, key.1)))?;
        let profile_name = self
            .cfg
            .worker_for(task.worker.as_ref())
            .ok()
            .and_then(|w| w.profile.clone());
        let profile = self
            .cfg
            .context
            .profile_for(profile_name.as_deref())?
            .clone();
        Ok(Compiler::new(
            profile,
            self.redactor(),
            self.cfg.context.policy,
        ))
    }

    /// What fleet's own runs of the checks say about the task right now, one line per check of
    /// every contract that governs it.
    pub(crate) fn verification_lines(&self, key: &Key) -> Vec<VerificationLine> {
        let mut lines = Vec::new();
        for goal in by_id(self.ledger.governing(&key.0, &key.1)) {
            let Some(attached) = &goal.contract else {
                continue;
            };
            let evidence = self.ledger.evidence_of(&goal.id);
            for check in attached.contract.checks() {
                let newest = evidence.iter().rev().find(|e| {
                    e.run() == &key.0
                        && e.task() == &key.1
                        && e.check() == &check.id
                        && self.ledger.counts(e)
                });
                let (state, commit, id) = match newest {
                    None => (CheckState::NotRun, None, None),
                    Some(e) => (
                        match e.verdict() {
                            Verdict::Passed => CheckState::Passed,
                            Verdict::Failed => CheckState::Failed,
                            Verdict::Error => CheckState::Error,
                        },
                        Some(short(e.commit().as_str())),
                        Some(e.id().to_string()),
                    ),
                };
                lines.push(VerificationLine {
                    goal: goal.id.to_string(),
                    check: check.id.as_str().to_string(),
                    mandatory: check.mandatory,
                    state,
                    commit,
                    evidence: id,
                });
            }
        }
        lines
    }

    fn goal_ancestors(&self, goal: &Goal) -> Vec<(String, String)> {
        let mut chain = Vec::new();
        let mut next = goal.parent.clone();
        while let Some(id) = next {
            let Some(parent) = self.ledger.find(&id) else {
                break;
            };
            chain.push((parent.id.to_string(), parent.title.clone()));
            next = parent.parent.clone();
            if chain.len() > 16 {
                break;
            }
        }
        chain.reverse();
        chain
    }

    /// Output of fleet's own runs that did not pass, for this task, oldest first.
    fn evidence_excerpts(&self, key: &Key) -> Vec<EvidenceExcerpt> {
        let mut all: Vec<_> = self
            .ledger
            .governing(&key.0, &key.1)
            .into_iter()
            .flat_map(|g| self.ledger.evidence_of(&g.id))
            .filter(|e| e.run() == &key.0 && e.task() == &key.1 && !e.passed())
            .collect();
        all.sort_by_key(|e| (e.at_ms(), e.id().to_string()));
        let skip = all.len().saturating_sub(neo_context::limits::MAX_EVIDENCE);
        all.into_iter()
            .skip(skip)
            .map(|e| EvidenceExcerpt {
                id: e.id().to_string(),
                goal: e.goal().to_string(),
                check: e.check().as_str().to_string(),
                verdict: e.verdict().to_string(),
                summary: e.summary().to_string(),
                output: e.output().to_string(),
            })
            .collect()
    }

    fn read_excerpt(
        &self,
        repo: &str,
        file: &crate::ports::TreeFile,
        selection: Selection,
    ) -> Result<Excerpt> {
        let bytes =
            self.ports
                .vcs
                .read_blob(repo, &file.blob, neo_context::limits::MAX_SCAN_BYTES)?;
        Ok(Excerpt {
            path: file.path.clone(),
            blob: file.blob.to_string(),
            size: file.size,
            bytes,
            selection,
        })
    }

    /// The files layer 6 shows (as the repository had them at the run's base commit), the paths
    /// and committed files of the worker's own work (layer 7, UNVERIFIED), and, when asked, the
    /// files of the base commit that are in the task's scope (for scoping facts).
    ///
    /// The scope is made of operator inputs only: the base commit's tree and the paths the task's
    /// contracts allow (every file when no contract limits the paths). Which paths the worker's
    /// commits changed steers which files layer 6 shows first, and nothing else: it never decides
    /// what layers 1 to 5 hold.
    fn repository_inputs(&self, key: &Key, need_scope: bool) -> Result<RepositoryInputs> {
        let run = self.state.run(&key.0)?;
        let task = run
            .task(&key.1)
            .ok_or_else(|| FleetError::not_found(format!("no task {}/{}", key.0, key.1)))?;
        let repo = run.repo.as_str();
        let mut allowed: Vec<&Glob> = Vec::new();
        let governing = self.ledger.governing(&key.0, &key.1);
        for g in &governing {
            if let Some(c) = &g.contract {
                for check in c.contract.checks() {
                    // only `allow` scopes what is shown; a `deny` list says what may not be changed, not what may not be read
                    if let CheckKind::DiffOnly { allow, .. } = &check.kind {
                        allowed.extend(allow.iter());
                    }
                }
            }
        }
        let (changed, changed_total) = match &task.head {
            Some(head) => {
                let (paths, truncated) =
                    self.ports
                        .vcs
                        .changed_paths(repo, &run.base_sha, head, MAX_CHANGED)?;
                let total = paths.len() as u64 + u64::from(truncated);
                (paths, total)
            }
            None => (Vec::new(), 0),
        };
        let base: TreeListing = if changed.is_empty() && allowed.is_empty() && !need_scope {
            TreeListing::default()
        } else {
            self.ports
                .vcs
                .tree_files(repo, &run.base_sha, MAX_TREE_FILES)?
        };
        // looked up by path, not searched for: the paths a worker changed are many and its choice
        let by_path: BTreeMap<&str, &TreeFile> =
            base.files.iter().map(|f| (f.path.as_str(), f)).collect();
        let readable = |f: &TreeFile| f.size <= MAX_FILE_READ;
        let mut chosen: Vec<(&TreeFile, Selection)> = Vec::new();
        let mut chosen_paths: BTreeSet<&str> = BTreeSet::new();
        for path in &changed {
            if chosen.len() >= neo_context::limits::MAX_EXCERPTS {
                break;
            }
            if let Some(f) = by_path.get(path.as_str()).filter(|f| readable(f)) {
                if chosen_paths.insert(f.path.as_str()) {
                    chosen.push((f, Selection::Changed));
                }
            }
        }
        for f in base.files.iter().filter(|f| readable(f)) {
            if chosen.len() >= neo_context::limits::MAX_EXCERPTS {
                break;
            }
            if !chosen_paths.contains(f.path.as_str()) && allowed.iter().any(|g| g.matches(&f.path))
            {
                chosen_paths.insert(f.path.as_str());
                chosen.push((f, Selection::Allowed));
            }
        }
        let mut files = Vec::with_capacity(chosen.len());
        for (f, selection) in &chosen {
            files.push(self.read_excerpt(repo, f, *selection)?);
        }
        let scope: Vec<String> = if need_scope {
            base.files
                .iter()
                .filter(|f| allowed.is_empty() || allowed.iter().any(|g| g.matches(&f.path)))
                .map(|f| f.path.clone())
                .collect()
        } else {
            Vec::new()
        };

        let mut work = Vec::new();
        if let Some(head) = &task.head {
            if !changed.is_empty() {
                let at_head = self.ports.vcs.tree_files(repo, head, MAX_TREE_FILES)?;
                let head_by_path: BTreeMap<&str, &TreeFile> =
                    at_head.files.iter().map(|f| (f.path.as_str(), f)).collect();
                for path in changed.iter() {
                    if work.len() >= WORK_FILES {
                        break;
                    }
                    if let Some(f) = head_by_path.get(path.as_str()).filter(|f| readable(f)) {
                        work.push(self.read_excerpt(repo, f, Selection::Changed)?);
                    }
                }
            }
        }
        let paths = changed
            .into_iter()
            .take(neo_context::limits::MAX_STAT_PATHS)
            .map(|path| ChangedPath { path })
            .collect();
        Ok((WorkingSetInput { files }, paths, changed_total, work, scope))
    }

    /// Everything one compile of the pack of `key` needs.
    pub(crate) fn pack_input(&self, key: &Key, attempt: u32) -> Result<PackInput> {
        self.pack_input_as_of(key, attempt, None)
    }

    /// Like [`Orchestrator::pack_input`], with the event history cut before sequence number
    /// `as_of` (what a pack recorded at that point in the log could have seen).
    pub(crate) fn pack_input_as_of(
        &self,
        key: &Key,
        attempt: u32,
        as_of: Option<u64>,
    ) -> Result<PackInput> {
        let run = self.state.run(&key.0)?;
        let task = run
            .task(&key.1)
            .ok_or_else(|| FleetError::not_found(format!("no task {}/{}", key.0, key.1)))?;

        let mut glossary = builtin_glossary();
        glossary.extend(self.cfg.context.glossary.iter().cloned());
        let charter = CharterInput {
            objective: run.objective.clone(),
            constraints: self.cfg.context.constraints.clone(),
            glossary,
        };

        let line = |t: &fleet_domain::run::Task| TaskLine {
            id: t.id.to_string(),
            title: t.title.clone(),
        };
        let plan = PlanInput {
            goals: by_id(self.ledger.owners_of(&key.0, &key.1))
                .into_iter()
                .map(|g| GoalLine {
                    id: g.id.to_string(),
                    title: g.title.clone(),
                    status: g.status.to_string(),
                    intent: g.intent.clone(),
                    ancestors: self.goal_ancestors(g),
                })
                .collect(),
            done: run
                .tasks()
                .iter()
                .filter(|t| t.status == TaskStatus::Completed)
                .map(line)
                .collect(),
            remaining: run
                .tasks()
                .iter()
                .filter(|t| t.status != TaskStatus::Completed)
                .map(line)
                .collect(),
            dependencies: task
                .deps
                .iter()
                .filter_map(|d| run.task(d))
                .map(|d| DependencyLine {
                    id: d.id.to_string(),
                    title: d.title.clone(),
                    done: d.status == TaskStatus::Completed,
                })
                .collect(),
        };

        let contracts = by_id(self.ledger.governing(&key.0, &key.1))
            .into_iter()
            .filter_map(|g| {
                g.contract.as_ref().map(|c| ContractLines {
                    goal: g.id.to_string(),
                    contract: short(c.id.as_str()),
                    checks: c
                        .contract
                        .checks()
                        .iter()
                        .map(|check| {
                            let (kind, what) = describe_check(check);
                            CheckLine {
                                id: check.id.as_str().to_string(),
                                mandatory: check.mandatory,
                                kind,
                                what,
                            }
                        })
                        .collect(),
                })
            })
            .collect();
        let task_input = TaskInput {
            id: task.id.to_string(),
            title: task.title.clone(),
            spec: task.spec.clone(),
            contracts,
            verification: self.verification_lines(key),
        };

        // Layer 5 is operator-written. A knowledge file inside the run's repository (or one of its
        // worktrees, which live below it) is a file a worker can write, so it is not used.
        if let Some(file) = &self.cfg.context.knowledge {
            let repo = run.repo.trim_end_matches('/');
            if file == repo || file.starts_with(&format!("{repo}/")) {
                return Err(FleetError::invalid(format!(
                    "the knowledge file {} is inside the repository of run {}, where a worker can write; keep it outside",
                    fleet_domain::ids::show(file),
                    key.0
                )));
            }
        }
        let entries = self.ports.knowledge.entries()?;
        let need_scope = entries.iter().any(|e| !e.paths.is_empty());
        let (working_set, changed, changed_total, work, scope) =
            self.repository_inputs(key, need_scope)?;

        let globs_match = |paths: &[String]| -> bool {
            paths.iter().any(|p| match Glob::new(p) {
                Ok(g) => scope.iter().any(|c| g.matches(c)),
                Err(_) => false,
            })
        };
        let knowledge = entries
            .into_iter()
            .filter(|e| e.paths.is_empty() || globs_match(&e.paths))
            .map(|e| e.fact)
            .collect();

        // the task's own events and those of what it builds on, oldest first
        let mut events = self.context.history(key);
        for dep in &task.deps {
            events.extend(self.context.history(&(key.0.clone(), dep.clone())));
        }
        events.sort_by_key(|e| e.seq);
        events.retain(|e| as_of.is_none_or(|limit| e.seq < limit));

        let unverified = UnverifiedInput {
            notes: task
                .notes
                .iter()
                .map(|n| NoteInput { text: n.clone() })
                .collect(),
            dependency_summaries: task
                .deps
                .iter()
                .filter_map(|d| run.task(d))
                .filter_map(|d| {
                    d.summary.as_ref().map(|s| DependencySummary {
                        task: d.id.to_string(),
                        title: d.title.clone(),
                        summary: s.clone(),
                    })
                })
                .collect(),
            changed,
            changed_total,
            work,
            evidence: self.evidence_excerpts(key),
        };

        Ok(PackInput {
            subject: Subject {
                run: key.0.to_string(),
                task: key.1.to_string(),
                attempt,
                base: run.base_sha.to_string(),
                head: task.head.as_ref().map(ToString::to_string),
            },
            charter,
            plan,
            task: task_input,
            knowledge,
            working_set,
            events,
            unverified,
            handoff: self.context.handoff(key).cloned(),
        })
    }

    /// Compiles the pack of an attempt of a task from the current state.
    pub(crate) fn compile_pack(&self, key: &Key, attempt: u32, with_handoff: bool) -> Result<Pack> {
        let compiler = self.compiler_for(key)?;
        let mut input = self.pack_input(key, attempt)?;
        if !with_handoff {
            input.handoff = None;
        }
        compiler
            .compile(&input, self.ports.digest.as_ref())
            .map_err(context_error)
    }

    /// Compiles the pack again as it would have been when the pack with `digest` was recorded:
    /// from the same state, with the events the log held before that record. Whatever changed in
    /// the state since (a goal, a note, a new commit) shows as a changed layer.
    pub(crate) fn compile_pack_as_of(
        &self,
        key: &Key,
        attempt: u32,
        recorded_digest: &str,
        handoff_digest: Option<&str>,
    ) -> Result<Pack> {
        let compiler = self.compiler_for(key)?;
        let as_of = self.context.compiled_at(key, recorded_digest);
        let mut input = self.pack_input_as_of(key, attempt, as_of)?;
        if input.handoff.as_ref().map(|h| h.digest()) != handoff_digest {
            input.handoff = None;
        }
        compiler
            .compile(&input, self.ports.digest.as_ref())
            .map_err(context_error)
    }

    /// Writes a compiled pack where the worker will look, keeps it, and records that it was made.
    pub(crate) fn publish_pack(
        &mut self,
        key: &Key,
        attempt: u32,
        worktree: &str,
        pack: &Pack,
    ) -> Result<()> {
        let document = pack.document();
        self.ports
            .contract
            .prepare_context(worktree, pack.text(), &document.canonical())?;
        self.ports.context.save_pack(
            &key.0,
            &key.1,
            &StoredPack {
                digest: pack.digest().to_string(),
                text: pack.text().to_string(),
                document: document.clone(),
            },
        )?;
        let m = pack.manifest();
        self.commit(vec![DomainEvent::ContextCompiled {
            run: key.0.clone(),
            task: key.1.clone(),
            attempt,
            profile: m.profile.name.clone(),
            pack_digest: pack.digest().to_string(),
            stable_digest: m.stable_digest.clone(),
            tokens: m.total_tokens,
            manifest: Document(m.to_value()),
        }])
    }

    pub(crate) fn uses_compiled_context(&self, key: &Key) -> bool {
        self.state
            .run(&key.0)
            .is_ok_and(|r| r.context == ContextMode::Compiled)
    }

    // ---- handoffs ---------------------------------------------------------------------------

    /// Reads what a worker left in `.fleet/handoff.json` and keeps it until the handoff of the
    /// attempt is made. Whatever it says is UNVERIFIED; an unusable file is a warning, not a failure.
    pub(crate) fn stash_worker_summary(&mut self, key: &Key, worktree: &str) {
        if !self.uses_compiled_context(key) || worktree.is_empty() {
            return;
        }
        match self.ports.contract.read_handoff(worktree) {
            ContractRead::Valid(h) => {
                self.handoff_summaries.insert(key.clone(), h.summary);
            }
            ContractRead::Absent => {}
            ContractRead::Invalid(why) => {
                self.warn(format!("{}/{}: handoff.json ignored: {why}", key.0, key.1));
            }
        }
    }

    /// Makes a handoff packet for the task as it stands and records it. Returns the packet and
    /// whether it is new (the same state gives the same packet, which is not recorded twice).
    pub(crate) fn make_handoff(
        &mut self,
        key: &Key,
        trigger: HandoffTrigger,
    ) -> Result<(HandoffPacket, bool)> {
        let run = self.state.run(&key.0)?;
        if run.context != ContextMode::Compiled {
            return Err(FleetError::conflict(format!(
                "run {} does not use compiled context (create it with context = \"compiled\")",
                key.0
            )));
        }
        let task = run
            .task(&key.1)
            .ok_or_else(|| FleetError::not_found(format!("no task {}/{}", key.0, key.1)))?;
        let attempt = task.attempt;
        let compiler = self.compiler_for(key)?;
        let mut input = self.pack_input(key, attempt)?;
        input.handoff = None;
        let hasher = self.ports.digest.as_ref();
        let pack = compiler.compile(&input, hasher).map_err(context_error)?;
        let verification = self.verification_lines(key);
        let mut evidence: Vec<String> = verification
            .iter()
            .filter_map(|v| v.evidence.clone())
            .collect();
        evidence.sort();
        evidence.dedup();
        // kept until the handoff is recorded: if recording fails (or the daemon dies first), the
        // summary is still in the worker's `.fleet/handoff.json`, which start-up reads again
        let summary = self.handoff_summaries.get(key).cloned();
        let packet = HandoffPacket::new(
            HandoffParts {
                run: key.0.to_string(),
                task: key.1.to_string(),
                attempt,
                trigger,
                verification,
                evidence,
                summary,
            },
            &pack,
            compiler.hygiene(),
            hasher,
        );
        if self.context.last_handoff_digest(key) == Some(packet.digest()) {
            self.handoff_summaries.remove(key);
            return Ok((packet, false));
        }
        let document = packet.to_value();
        self.ports
            .context
            .save_handoff(&key.0, &key.1, packet.digest(), &document)?;
        self.commit(vec![DomainEvent::HandoffCreated {
            run: key.0.clone(),
            task: key.1.clone(),
            attempt,
            trigger: trigger.as_str().to_string(),
            digest: packet.digest().to_string(),
            packet: Document(document),
        }])?;
        self.handoff_summaries.remove(key);
        Ok((packet, true))
    }

    /// Before a task is briefed again, the handoff of the attempt that ended must exist: it is
    /// made now if it is missing (the daemon died after recording the end of the attempt, or the
    /// store refused it), from the summary still in the worker's `.fleet/handoff.json`. If it
    /// cannot be recorded the next attempt does not start yet, so nothing the worker left is wiped.
    pub(crate) fn ensure_restart_handoff(&mut self, key: &Key, worktree: &str) -> Result<()> {
        let attempt = self
            .state
            .run(&key.0)
            .ok()
            .and_then(|r| r.task(&key.1))
            .map_or(0, |t| t.attempt);
        if attempt == 0
            || self.context.was_made(key, attempt, HandoffClass::Restart)
            || self.context.was_made(key, attempt, HandoffClass::TaskEnd)
        {
            return Ok(());
        }
        self.stash_worker_summary(key, worktree);
        self.make_handoff(key, HandoffTrigger::Retry).map(|_| ())
    }

    /// After a restart: an attempt whose end was recorded but whose handoff was not (the daemon
    /// died between the two steps, or the store refused it) gets it now. The worker's summary is
    /// read again from `.fleet/handoff.json`, which stays in the worktree until the next attempt
    /// starts, so nothing a worker left is lost by a crash in between.
    pub(crate) fn reconcile_handoffs(&mut self) {
        let mut wanted: Vec<(Key, HandoffTrigger, String)> = Vec::new();
        for run in self
            .state
            .runs()
            .filter(|r| r.context == ContextMode::Compiled)
        {
            for t in run.tasks().iter().filter(|t| t.attempt >= 1) {
                let trigger = match t.status {
                    TaskStatus::Completed => HandoffTrigger::TaskCompleted,
                    TaskStatus::Failed => HandoffTrigger::TaskFailed,
                    TaskStatus::Ready => HandoffTrigger::Retry,
                    _ => continue,
                };
                wanted.push((
                    (run.id.clone(), t.id.clone()),
                    trigger,
                    t.worktree.clone().unwrap_or_default(),
                ));
            }
        }
        for (key, trigger, worktree) in wanted {
            let attempt = self
                .state
                .run(&key.0)
                .ok()
                .and_then(|r| r.task(&key.1))
                .map_or(0, |t| t.attempt);
            let Some(class) = HandoffClass::of(trigger) else {
                continue;
            };
            if self.context.was_made(&key, attempt, class) {
                continue;
            }
            self.stash_worker_summary(&key, &worktree);
            self.auto_handoff(&key, trigger);
        }
    }

    /// The handoff made by the flow itself (an attempt ended; a task goes back in line; the daemon
    /// found it running after a crash): at most one per attempt and class, never a failure of the
    /// flow that triggered it.
    pub(crate) fn auto_handoff(&mut self, key: &Key, trigger: HandoffTrigger) {
        if !self.uses_compiled_context(key) {
            return;
        }
        let Some(class) = HandoffClass::of(trigger) else {
            return;
        };
        let attempt = self
            .state
            .run(&key.0)
            .ok()
            .and_then(|r| r.task(&key.1))
            .map_or(0, |t| t.attempt);
        if self.context.was_made(key, attempt, class) {
            return;
        }
        if let Err(e) = self.make_handoff(key, trigger) {
            self.warn(format!(
                "{}/{}: could not make a handoff: {}",
                key.0, key.1, e.message
            ));
        }
    }

    /// Handoffs for the events a batch just recorded.
    pub(crate) fn handoffs_for(&mut self, events: &[DomainEvent]) {
        let mut wanted: Vec<(Key, HandoffTrigger)> = Vec::new();
        for event in events {
            let made = match event {
                DomainEvent::TaskCompleted { run, task } => {
                    Some((run, task, HandoffTrigger::TaskCompleted))
                }
                DomainEvent::TaskFailed { run, task, .. } => {
                    Some((run, task, HandoffTrigger::TaskFailed))
                }
                DomainEvent::RetryQueued { run, task, .. } => {
                    Some((run, task, HandoffTrigger::Retry))
                }
                _ => None,
            };
            if let Some((run, task, trigger)) = made {
                let key = (run.clone(), task.clone());
                if !wanted.contains(&(key.clone(), trigger)) {
                    wanted.push((key, trigger));
                }
            }
        }
        for (key, trigger) in wanted {
            self.auto_handoff(&key, trigger);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neo_verify::contract::CheckId;

    fn diff(allow: &[&str], deny: &[&str]) -> Check {
        let globs = |items: &[&str]| items.iter().map(|g| Glob::new(g).unwrap()).collect();
        Check {
            id: CheckId::parse("scope").unwrap(),
            mandatory: true,
            kind: CheckKind::DiffOnly {
                allow: globs(allow),
                deny: globs(deny),
            },
        }
    }

    #[test]
    fn an_allow_only_diff_check_reads_as_it_always_did() {
        let (kind, what) = describe_check(&diff(&["src/**", "*.md"], &[]));
        assert_eq!(kind, "diff");
        assert_eq!(
            what,
            "every path your commits change matches one of: src/**, *.md"
        );
    }

    #[test]
    fn a_deny_list_is_named_and_a_long_one_is_counted() {
        let (_, what) = describe_check(&diff(&[], &["Cargo.lock", "dogfood/**"]));
        assert_eq!(
            what,
            "no path your commits change matches any of: Cargo.lock, dogfood/**"
        );
        let (_, both) = describe_check(&diff(&["a/**"], &["b/**"]));
        assert_eq!(
            both,
            "every path your commits change matches one of: a/**; and none matches any of: b/**"
        );
        let many: Vec<String> = (0..80).map(|i| format!("d{i}/**")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let (_, long) = describe_check(&diff(&["x/**"], &refs));
        assert!(long.contains("d0/**, d1/**, d2/**, d3/**, d4/**, d5/** and 74 more"));
        assert!(!long.contains("d6/**"));
    }
}
