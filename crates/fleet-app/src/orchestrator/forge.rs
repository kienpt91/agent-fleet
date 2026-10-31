//! The forge use cases: listing and showing issues and pull requests, making a task of an issue,
//! and opening a pull request for a task after a human approved exactly that pull request.
//!
//! Forge calls never run on the orchestrator's thread. A command that needs one hands it to the
//! [`ForgeJobs`] port and answers with a *ticket*; `tick` collects what finished and records its
//! events, and `forge.result` reports the outcome of a ticket. Everything that decides something
//! (the approval, the digest, the task that is created) happens here, on the orchestrator's
//! thread, in the order the events are recorded.
//!
//! Publishing is two steps so that an approval is never spent on a call that cannot work: a
//! *preflight* (one cheap read proving the host answers and the account sees the repository) runs
//! first, and only when it passed is the approval consumed (and `pull_request_started` recorded,
//! before anything is pushed) and the branch pushed. Everything is computed again between the
//! two steps; a difference in the digest stops the publication.

use super::{Key, Orchestrator};
use crate::api::Reply;
use crate::forge::api::{ForgeCommand, IssueToTaskArgs, ListArgs, PrCreateArgs};
use crate::forge::ports::{
    ForgeAnswer, ForgeJobs, ForgeRequest, ListQuery, PublishRequest, PushRequest,
};
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::events::DomainEvent;
use fleet_domain::fleet::MergeGate;
use fleet_domain::ids::{validate_ref_name, ApprovalId, RunId, Sha};
use fleet_domain::run::NewTask;
use fleet_domain::sanitize::flatten_line;
use fleet_domain::status::TaskStatus;
use fleet_domain::value::Value;
use fleet_forge::links::{ForgeLedger, PublishState};
use fleet_forge::publish::{
    clean_body, clean_title, default_body, default_title, PullRequestDraft,
};
use fleet_forge::push::push_target;
use fleet_forge::repo::{parse_remote, RemoteTarget, RepoRef};
use fleet_forge::untrusted::issue_task_draft;
use neo_goals::ledger::TaskContract;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Forge requests in flight at once.
const MAX_INFLIGHT: usize = 8;
/// Outcomes kept for `forge.result`.
const MAX_SLOTS: usize = 64;
/// Fetched issues and pull requests kept for the read-only views.
const MAX_DETAILS: usize = 32;

/// What the log says about a publication whose outcome was lost with the daemon.
pub const INTERRUPTED_PUBLICATION: &str = "interrupted by a daemon restart; the branch may have been pushed and the pull request may exist (see `fleet pr list`); it was not started again";

fn data(schema: &str, pairs: Vec<(&str, Value)>) -> Reply {
    let mut map: Vec<(String, Value)> = vec![("schema".to_string(), Value::str(schema))];
    map.extend(pairs.into_iter().map(|(k, v)| (k.to_string(), v)));
    Reply::Data(Value::object(map))
}

fn qualified(key: &Key) -> String {
    format!("{}/{}", key.0, key.1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadKind {
    Issues,
    Issue(u64),
    Pulls,
    Pull(u64),
    Status(u64),
}

impl ReadKind {
    fn label(self) -> &'static str {
        match self {
            ReadKind::Issues => "issue.list",
            ReadKind::Issue(_) => "issue.show",
            ReadKind::Pulls => "pr.list",
            ReadKind::Pull(_) => "pr.show",
            ReadKind::Status(_) => "pr.status",
        }
    }
}

/// What a ticket is waiting for.
enum Pending {
    Read {
        kind: ReadKind,
        repo: RepoRef,
    },
    ToTask {
        repo: RepoRef,
        run: RunId,
        args: IssueToTaskArgs,
    },
    Preflight {
        key: Key,
        args: PrCreateArgs,
        digest: String,
    },
    Publish {
        approval: ApprovalId,
        key: Key,
        repo: RepoRef,
        head: Sha,
    },
}

enum Slot {
    Running,
    Done { kind: &'static str, data: Value },
    Failed(FleetError),
}

struct Entry {
    at_ms: u64,
    repo: String,
    value: Value,
}

impl Entry {
    fn to_value(&self, key: Option<&str>) -> Value {
        let mut pairs = vec![
            ("fetched_ms", Value::int(self.at_ms)),
            ("repo", Value::str(self.repo.as_str())),
            ("data", self.value.clone()),
        ];
        if let Some(k) = key {
            pairs.insert(0, ("key", Value::str(k)));
        }
        Value::object(pairs)
    }
}

#[derive(Default)]
struct Cache {
    issues: Option<Entry>,
    pulls: Option<Entry>,
    details: VecDeque<(String, Entry)>,
}

/// The forge state of the orchestrator.
#[derive(Default)]
pub(crate) struct ForgeRuntime {
    pub(crate) ledger: ForgeLedger,
    jobs: Option<Box<dyn ForgeJobs>>,
    seq: u64,
    pending: BTreeMap<u64, Pending>,
    slots: BTreeMap<u64, Slot>,
    cache: Cache,
}

/// Everything a publication is made of, computed from the repository and the arguments now.
struct PrPlan {
    key: Key,
    repo: RepoRef,
    draft: PullRequestDraft,
    push: PushRequest,
    digest: String,
}

impl Orchestrator {
    /// Connects the forge adapter. Without one every forge command that needs the network is
    /// refused (the views of what was fetched and of the links still work).
    pub fn attach_forge(&mut self, jobs: Box<dyn ForgeJobs>) {
        self.forge.jobs = Some(jobs);
    }

    pub(crate) fn handle_forge(&mut self, cmd: ForgeCommand) -> Result<Reply> {
        match cmd {
            ForgeCommand::Status => Ok(self.forge_status()),
            ForgeCommand::Snapshot => Ok(self.forge_snapshot()),
            ForgeCommand::Links => Ok(self.forge_links()),
            ForgeCommand::Result { ticket } => self.forge_result(ticket),
            ForgeCommand::IssueList(a) => self.forge_list(&a, true),
            ForgeCommand::PrList(a) => self.forge_list(&a, false),
            ForgeCommand::IssueShow { run, number } => {
                self.forge_show(run.as_ref(), ReadKind::Issue(number))
            }
            ForgeCommand::PrShow { run, number } => {
                self.forge_show(run.as_ref(), ReadKind::Pull(number))
            }
            ForgeCommand::PrStatus { run, number } => {
                self.forge_show(run.as_ref(), ReadKind::Status(number))
            }
            ForgeCommand::IssueToTask(args) => self.issue_to_task(args),
            ForgeCommand::PrCreate(args) => self.pr_create(args),
        }
    }

    // ---- views ------------------------------------------------------------------------------

    fn forge_status(&self) -> Reply {
        let cfg = &self.cfg.forge;
        let repos = self.repo_paths();
        let (ready, detail, rate) = match &self.forge.jobs {
            Some(j) => {
                let s = j.status(&repos);
                (s.ready, s.detail, s.rate)
            }
            None => (
                false,
                if cfg.enabled {
                    "no adapter is attached".to_string()
                } else {
                    "the integration is off (set `enabled = true` in [github])".to_string()
                },
                Value::Null,
            ),
        };
        let credentials = match &cfg.push_credentials {
            fleet_forge::config::PushCredentials::None => "none",
            fleet_forge::config::PushCredentials::Gh => "gh",
            fleet_forge::config::PushCredentials::Helper(_) => "helper",
        };
        data(
            "fleet.forge.status/1",
            vec![
                ("enabled", Value::Bool(cfg.enabled)),
                ("ready", Value::Bool(cfg.enabled && ready)),
                ("detail", Value::str(detail)),
                ("host", Value::str(cfg.host.as_str())),
                (
                    "repo",
                    cfg.repo
                        .as_ref()
                        .map_or(Value::Null, |r| Value::str(r.slug())),
                ),
                ("remote", Value::str(cfg.remote.as_str())),
                ("base", Value::str(cfg.base.as_str())),
                (
                    "allowed_hosts",
                    Value::array(cfg.allowed_hosts.iter().map(Value::str)),
                ),
                ("push_credentials", Value::str(credentials)),
                ("allow_local_remote", Value::Bool(cfg.allow_local_remote)),
                ("rate", rate),
                ("inflight", Value::int(self.forge.pending.len() as u64)),
                (
                    "issues_linked",
                    Value::int(self.forge.ledger.issues().len() as u64),
                ),
                (
                    "pull_requests",
                    Value::int(self.forge.ledger.publications().len() as u64),
                ),
            ],
        )
    }

    /// What the last commands fetched, from memory: no process is started and no request is made.
    fn forge_snapshot(&self) -> Reply {
        let c = &self.forge.cache;
        data(
            "fleet.forge.snapshot/1",
            vec![
                ("enabled", Value::Bool(self.cfg.forge.enabled)),
                (
                    "issues",
                    c.issues.as_ref().map_or(Value::Null, |e| e.to_value(None)),
                ),
                (
                    "pulls",
                    c.pulls.as_ref().map_or(Value::Null, |e| e.to_value(None)),
                ),
                (
                    "details",
                    Value::array(c.details.iter().map(|(k, e)| e.to_value(Some(k)))),
                ),
            ],
        )
    }

    fn forge_links(&self) -> Reply {
        let mut doc = self.forge.ledger.to_value();
        if let Value::Object(m) = &mut doc {
            m.insert("schema".to_string(), Value::str("fleet.forge.links/1"));
        }
        Reply::Data(doc)
    }

    fn forge_result(&self, ticket: u64) -> Result<Reply> {
        match self.forge.slots.get(&ticket) {
            None => Err(FleetError::not_found(format!(
                "no forge request has the ticket {ticket} (it is too old, or the daemon restarted)"
            ))),
            Some(Slot::Running) => Ok(data(
                "fleet.forge.result/1",
                vec![
                    ("ticket", Value::int(ticket)),
                    ("state", Value::str("pending")),
                ],
            )),
            Some(Slot::Done { kind, data: doc }) => Ok(data(
                "fleet.forge.result/1",
                vec![
                    ("ticket", Value::int(ticket)),
                    ("state", Value::str("done")),
                    ("kind", Value::str(*kind)),
                    ("data", doc.clone()),
                ],
            )),
            Some(Slot::Failed(e)) => Err(e.clone()),
        }
    }

    // ---- plumbing ---------------------------------------------------------------------------

    /// The integration is on, an adapter is attached and it can start calls. Checked before any
    /// state is touched, so a missing `gh` or a switched-off integration changes nothing.
    fn forge_ready(&self) -> Result<()> {
        if !self.cfg.forge.enabled {
            return Err(FleetError::unavailable(
                "the GitHub integration is off: set `enabled = true` in the [github] section of fleet.toml and restart the daemon",
            ));
        }
        let Some(jobs) = self.forge.jobs.as_ref() else {
            return Err(FleetError::unavailable(
                "the GitHub integration has no adapter in this daemon",
            ));
        };
        let status = jobs.status(&self.repo_paths());
        if !status.ready {
            return Err(FleetError::unavailable(format!(
                "the GitHub integration cannot run: {}. Nothing was changed.",
                status.detail
            )));
        }
        if self.forge.pending.len() >= MAX_INFLIGHT {
            return Err(FleetError::limit(format!(
                "{MAX_INFLIGHT} forge requests are already in flight; wait for one to finish"
            )));
        }
        Ok(())
    }

    fn forge_adapter(&self) -> Result<&dyn ForgeJobs> {
        self.forge.jobs.as_deref().ok_or_else(|| {
            FleetError::unavailable("the GitHub integration has no adapter in this daemon")
        })
    }

    /// The repositories of every run: `gh` must not be a program inside one of them.
    fn repo_paths(&self) -> Vec<String> {
        let all: BTreeSet<String> = self.state.runs().map(|r| r.repo.clone()).collect();
        all.into_iter().collect()
    }

    /// Hands `req` to the adapter and remembers what the answer is for.
    fn forge_start(&mut self, req: ForgeRequest, pending: Pending) -> Result<u64> {
        let ticket = self.forge.seq.saturating_add(1);
        let repos = self.repo_paths();
        let jobs = self.forge.jobs.as_mut().ok_or_else(|| {
            FleetError::unavailable("the GitHub integration has no adapter in this daemon")
        })?;
        jobs.start(ticket, req, &repos)?;
        self.forge.seq = ticket;
        self.forge.pending.insert(ticket, pending);
        self.forge.slots.insert(ticket, Slot::Running);
        self.trim_slots();
        Ok(ticket)
    }

    fn trim_slots(&mut self) {
        while self.forge.slots.len() > MAX_SLOTS {
            let oldest = self
                .forge
                .slots
                .iter()
                .find(|(_, s)| !matches!(s, Slot::Running))
                .map(|(t, _)| *t);
            match oldest {
                Some(t) => {
                    self.forge.slots.remove(&t);
                }
                None => break,
            }
        }
    }

    fn ticket_reply(ticket: u64, kind: &str) -> Reply {
        data(
            "fleet.forge.ticket/1",
            vec![("ticket", Value::int(ticket)), ("kind", Value::str(kind))],
        )
    }

    /// The repository a command is about: `[github] repo`, or the one the configured remote of
    /// the run's repository (or of every run's repository) names.
    fn forge_repo(&self, run: Option<&RunId>) -> Result<RepoRef> {
        let cfg = &self.cfg.forge;
        if let Some(r) = &cfg.repo {
            return Ok(r.clone());
        }
        let paths: BTreeSet<String> = match run {
            Some(r) => BTreeSet::from([self.state.run(r)?.repo.clone()]),
            None => self.state.runs().map(|r| r.repo.clone()).collect(),
        };
        if paths.is_empty() {
            return Err(FleetError::invalid(
                "no repository is known: set `repo = \"owner/name\"` in [github], or create a run first",
            ));
        }
        let mut found = BTreeSet::new();
        for path in &paths {
            found.insert(self.repo_of_remote(path)?);
        }
        let mut it = found.into_iter();
        match (it.next(), it.next()) {
            (Some(one), None) => Ok(one),
            _ => Err(FleetError::invalid(
                "the runs use remotes that name several repositories; set `repo` in [github] or name a run",
            )),
        }
    }

    fn repo_of_remote(&self, repo_path: &str) -> Result<RepoRef> {
        let cfg = &self.cfg.forge;
        let url = self.forge_adapter()?.remote_url(repo_path, &cfg.remote)?;
        match parse_remote(&url)? {
            RemoteTarget::Https(r) | RemoteTarget::Ssh(r) => {
                if !cfg.allowed_hosts.iter().any(|h| h == r.host()) || r.host() != cfg.host {
                    return Err(FleetError::forbidden(format!(
                        "the remote {} names the host {}, which is not the configured host {}",
                        cfg.remote,
                        r.host(),
                        cfg.host
                    )));
                }
                Ok(r)
            }
            RemoteTarget::Local(_) => Err(FleetError::invalid(format!(
                "the remote {} is a path on this machine; name the repository with `repo = \"owner/name\"` in [github]",
                cfg.remote
            ))),
        }
    }

    // ---- reads ------------------------------------------------------------------------------

    fn forge_list(&mut self, a: &ListArgs, issues: bool) -> Result<Reply> {
        self.forge_ready()?;
        let repo = self.forge_repo(a.run.as_ref())?;
        let query = ListQuery {
            state: a.state,
            page: a.page,
            per_page: a.limit,
        };
        let (req, kind) = if issues {
            (
                ForgeRequest::ListIssues {
                    repo: repo.clone(),
                    query,
                },
                ReadKind::Issues,
            )
        } else {
            (
                ForgeRequest::ListPulls {
                    repo: repo.clone(),
                    query,
                },
                ReadKind::Pulls,
            )
        };
        let ticket = self.forge_start(req, Pending::Read { kind, repo })?;
        Ok(Self::ticket_reply(ticket, kind.label()))
    }

    fn forge_show(&mut self, run: Option<&RunId>, kind: ReadKind) -> Result<Reply> {
        self.forge_ready()?;
        let repo = self.forge_repo(run)?;
        let req = match kind {
            ReadKind::Issue(number) => ForgeRequest::ShowIssue {
                repo: repo.clone(),
                number,
            },
            ReadKind::Pull(number) => ForgeRequest::ShowPull {
                repo: repo.clone(),
                number,
            },
            ReadKind::Status(number) => ForgeRequest::PullStatus {
                repo: repo.clone(),
                number,
            },
            ReadKind::Issues | ReadKind::Pulls => {
                return Err(FleetError::internal("a list is not a show"))
            }
        };
        let ticket = self.forge_start(req, Pending::Read { kind, repo })?;
        Ok(Self::ticket_reply(ticket, kind.label()))
    }

    fn cache_put(&mut self, kind: ReadKind, repo: &RepoRef, value: Value) {
        let entry = Entry {
            at_ms: self.now(),
            repo: repo.full(),
            value,
        };
        let key = match kind {
            ReadKind::Issues => {
                self.forge.cache.issues = Some(entry);
                return;
            }
            ReadKind::Pulls => {
                self.forge.cache.pulls = Some(entry);
                return;
            }
            ReadKind::Issue(n) => format!("issue #{n}"),
            ReadKind::Pull(n) => format!("pr #{n}"),
            ReadKind::Status(n) => format!("pr #{n} status"),
        };
        let details = &mut self.forge.cache.details;
        details.retain(|(k, _)| *k != key);
        details.push_back((key, entry));
        while details.len() > MAX_DETAILS {
            details.pop_front();
        }
    }

    // ---- issue to task ------------------------------------------------------------------------

    fn only_run(&self, run: Option<&RunId>) -> Result<RunId> {
        match run {
            Some(r) => {
                self.state.run(r)?;
                Ok(r.clone())
            }
            None => {
                let mut it = self.state.runs();
                match (it.next(), it.next()) {
                    (Some(r), None) => Ok(r.id.clone()),
                    (None, _) => Err(FleetError::invalid(
                        "there is no run to add the task to; create one first",
                    )),
                    _ => Err(FleetError::invalid(
                        "several runs exist: name one with --run",
                    )),
                }
            }
        }
    }

    fn issue_to_task(&mut self, args: IssueToTaskArgs) -> Result<Reply> {
        self.forge_ready()?;
        let run = self.only_run(args.run.as_ref())?;
        let repo = self.forge_repo(Some(&run))?;
        if let Some(l) = self.forge.ledger.task_of_issue(&repo.full(), args.number) {
            return Err(FleetError::conflict(format!(
                "issue #{} already became task {}/{}",
                args.number, l.run, l.task
            )));
        }
        let number = args.number;
        let req = ForgeRequest::ShowIssue {
            repo: repo.clone(),
            number,
        };
        let ticket = self.forge_start(req, Pending::ToTask { repo, run, args })?;
        Ok(Self::ticket_reply(ticket, "issue.to-task"))
    }

    fn make_task_from_issue(
        &mut self,
        repo: &RepoRef,
        run: &RunId,
        args: &IssueToTaskArgs,
        issue: &fleet_forge::model::Issue,
    ) -> Result<Value> {
        let number = issue.summary.number;
        if let Some(l) = self.forge.ledger.task_of_issue(&repo.full(), number) {
            return Err(FleetError::conflict(format!(
                "issue #{number} already became task {}/{}",
                l.run, l.task
            )));
        }
        let draft = issue_task_draft(repo, issue);
        let (ids, mut events) = self.state.add_tasks(
            run,
            vec![NewTask {
                id: args.id.clone(),
                title: draft.title.clone(),
                spec: draft.spec,
                deps: args.after.clone(),
                worker: args.worker.clone(),
            }],
        )?;
        let task = ids
            .first()
            .cloned()
            .ok_or_else(|| FleetError::internal("the task was not created"))?;
        events.push(DomainEvent::IssueLinked {
            run: run.clone(),
            task: task.clone(),
            repo: repo.full(),
            number,
        });
        self.commit(events)?;
        self.promote()?;
        Ok(Value::object([
            ("task", Value::str(format!("{run}/{task}"))),
            ("issue", Value::int(number)),
            ("repo", Value::str(repo.full())),
            ("title", Value::str(draft.title)),
        ]))
    }

    // ---- pull requests ------------------------------------------------------------------------

    /// The base branch of a pull request: what the operator said, else the branch the run started
    /// from when it was a branch, else `[github] base`.
    fn default_base(&self, run_base_ref: &str) -> String {
        if run_base_ref != "HEAD"
            && Sha::parse(run_base_ref).is_err()
            && validate_ref_name(run_base_ref).is_ok()
        {
            run_base_ref.to_string()
        } else {
            self.cfg.forge.base.clone()
        }
    }

    /// Everything a publication is made of, from the repository and the arguments as they are
    /// now. Used twice (before the preflight and after it) so that nothing can change in between
    /// without a different digest.
    fn pr_plan(&self, args: &PrCreateArgs) -> Result<PrPlan> {
        let key = self.state.find_task(&args.task)?;
        let (repo_path, branch, status, base_ref, task_title) = {
            let run = self.state.run(&key.0)?;
            let task = run
                .task(&key.1)
                .ok_or_else(|| FleetError::not_found(format!("task {} does not exist", key.1)))?;
            (
                run.repo.clone(),
                task.branch.clone(),
                task.status,
                run.base_ref.clone(),
                task.title.clone(),
            )
        };
        if status != TaskStatus::Completed {
            return Err(FleetError::conflict(format!(
                "task {} is {status}; only completed tasks can be published",
                qualified(&key)
            )));
        }
        let head = self.ports.vcs.branch_head(&repo_path, &branch)?;
        // Work that a verification contract guards leaves the machine only as the commit that was
        // proven, like a merge.
        let tree = self.ports.vcs.commit_tree(&repo_path, &head)?;
        match self.ledger.task_contract_at(&key.0, &key.1, &head, &tree) {
            TaskContract::NotGoverned | TaskContract::Passed => {}
            _ => {
                return Err(FleetError::conflict(format!(
                    "{} is guarded by a verification contract and has no passing evidence for the commit that would be published ({}); run `fleet verify {}` first",
                    qualified(&key),
                    head.as_str().get(..12).unwrap_or(head.as_str()),
                    qualified(&key)
                )))
            }
        }
        let repo = self.forge_repo(Some(&key.0))?;
        let remote_url = self
            .forge_adapter()?
            .remote_url(&repo_path, &self.cfg.forge.remote)?;
        push_target(&self.cfg.forge, &remote_url, &repo)?;
        let issue = self.forge.ledger.issue_of(&key.0, &key.1).map(|l| l.number);
        let title = match &args.title {
            Some(t) => clean_title(t),
            None => default_title(&task_title, issue),
        };
        let body = match &args.body {
            Some(b) => clean_body(b)?,
            None => default_body(&key.0, &key.1, &head, issue),
        };
        let draft = PullRequestDraft {
            repo: repo.clone(),
            base: args
                .base
                .clone()
                .unwrap_or_else(|| self.default_base(&base_ref)),
            head_branch: branch.clone(),
            head_sha: head.clone(),
            title,
            body,
            draft: args.draft,
        };
        draft.validate()?;
        let opened = self
            .forge
            .ledger
            .pulls_of(&key.0, &key.1)
            .into_iter()
            .find(|p| {
                p.head == head
                    && p.repo == repo.full()
                    && matches!(p.state, PublishState::Opened { .. })
            });
        if let Some(PublishState::Opened { number, .. }) = opened.map(|p| &p.state) {
            return Err(FleetError::conflict(format!(
                "pull request #{number} was already opened for the commit {} of {}",
                head.as_str().get(..12).unwrap_or(head.as_str()),
                qualified(&key)
            )));
        }
        let digest = draft.digest(&key.1, self.ports.digest.as_ref());
        let push = PushRequest {
            repo_path,
            remote: self.cfg.forge.remote.clone(),
            branch,
            head,
            target: repo.clone(),
        };
        Ok(PrPlan {
            key,
            repo,
            draft,
            push,
            digest,
        })
    }

    fn pr_in_flight(&self, key: &Key) -> bool {
        self.forge.pending.values().any(|p| match p {
            Pending::Preflight { key: k, .. } | Pending::Publish { key: k, .. } => k == key,
            _ => false,
        })
    }

    /// First call: asks for the approval, or (when one was given for exactly this pull request)
    /// starts the preflight.
    fn pr_create(&mut self, args: PrCreateArgs) -> Result<Reply> {
        self.forge_ready()?;
        let plan = self.pr_plan(&args)?;
        if self.pr_in_flight(&plan.key) {
            return Err(FleetError::conflict(format!(
                "a pull request for {} is already being opened",
                qualified(&plan.key)
            )));
        }
        let now = self.now();
        let action = plan.draft.action();
        let gate = self.state.request_publication(
            &plan.key.0,
            &plan.key.1,
            &action,
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
                        "opening a pull request for {} on {} needs approval {approval} (digest {prefix}); read what it covers with `fleet approvals`, approve it with `fleet approve {approval}` and run the same `fleet pr create` again",
                        qualified(&plan.key),
                        plan.repo
                    ),
                )
                .with_subject(approval.as_str()))
            }
            // Nothing is consumed yet: an approval is spent only after the preflight passed.
            MergeGate::Proceed(_) => {
                let req = ForgeRequest::Preflight {
                    repo: plan.repo.clone(),
                };
                let ticket = self.forge_start(
                    req,
                    Pending::Preflight {
                        key: plan.key,
                        args,
                        digest: plan.digest,
                    },
                )?;
                Ok(Self::ticket_reply(ticket, "pr.create"))
            }
        }
    }

    /// Second step, after the preflight passed: the approval is consumed and the publication
    /// recorded as started, both before anything is pushed; then the branch is pushed and the pull
    /// request created, off this thread.
    fn pr_start(&mut self, ticket: u64, args: &PrCreateArgs, expected: &str) -> Result<Pending> {
        let plan = self.pr_plan(args)?;
        if plan.digest != expected {
            return Err(FleetError::conflict(
                "the pull request changed while it was being prepared (the branch moved or its text differs); nothing was published. Run the command again",
            ));
        }
        let now = self.now();
        let gate = self.state.request_publication(
            &plan.key.0,
            &plan.key.1,
            &plan.draft.action(),
            &self.policy,
            now,
            self.ports.digest.as_ref(),
        )?;
        let MergeGate::Proceed(mut events) = gate else {
            return Err(FleetError::conflict(
                "the approval no longer applies (it expired or was used); nothing was published. Run the command again",
            ));
        };
        let approval = events
            .iter()
            .find_map(|e| match e {
                DomainEvent::ApprovalConsumed { id } => Some(id.clone()),
                _ => None,
            })
            .ok_or_else(|| FleetError::internal("the approval was not consumed"))?;
        events.push(DomainEvent::PullRequestStarted {
            id: approval.clone(),
            run: plan.key.0.clone(),
            task: plan.key.1.clone(),
            repo: plan.repo.full(),
            base: plan.draft.base.clone(),
            branch: plan.draft.head_branch.clone(),
            head: plan.draft.head_sha.clone(),
        });
        self.commit(events)?;
        let repos = self.repo_paths();
        let started = match self.forge.jobs.as_mut() {
            Some(jobs) => jobs.start(
                ticket,
                ForgeRequest::Publish(Box::new(PublishRequest {
                    push: plan.push.clone(),
                    draft: plan.draft.clone(),
                })),
                &repos,
            ),
            None => Err(FleetError::unavailable(
                "the GitHub integration has no adapter in this daemon",
            )),
        };
        if let Err(e) = started {
            self.record_publication_failed(&approval, &plan.key, &e.message);
            return Err(e);
        }
        Ok(Pending::Publish {
            approval,
            key: plan.key,
            repo: plan.repo,
            head: plan.draft.head_sha,
        })
    }

    fn record_publication_failed(&mut self, approval: &ApprovalId, key: &Key, why: &str) {
        let event = DomainEvent::PullRequestFailed {
            id: approval.clone(),
            run: key.0.clone(),
            task: key.1.clone(),
            reason: flatten_line(why, 300),
        };
        if let Err(e) = self.commit(vec![event]) {
            self.warn(e.message);
        }
    }

    // ---- completions ----------------------------------------------------------------------------

    /// Collects what the adapter finished and records its events (a tick step).
    pub(crate) fn collect_forge(&mut self) -> Result<()> {
        let Some(jobs) = self.forge.jobs.as_mut() else {
            return Ok(());
        };
        for (ticket, result) in jobs.poll() {
            self.finish_forge(ticket, result);
        }
        Ok(())
    }

    fn finish_forge(&mut self, ticket: u64, result: Result<ForgeAnswer>) {
        let Some(pending) = self.forge.pending.remove(&ticket) else {
            return;
        };
        let slot = match pending {
            Pending::Read { kind, repo } => match result {
                Ok(answer) => {
                    let mut value = answer.to_value();
                    if let Value::Object(m) = &mut value {
                        m.insert("repo".to_string(), Value::str(repo.full()));
                    }
                    self.cache_put(kind, &repo, value.clone());
                    Slot::Done {
                        kind: kind.label(),
                        data: value,
                    }
                }
                Err(e) => Slot::Failed(e),
            },
            Pending::ToTask { repo, run, args } => match result {
                Ok(ForgeAnswer::Issue(issue)) => {
                    match self.make_task_from_issue(&repo, &run, &args, &issue) {
                        Ok(doc) => Slot::Done {
                            kind: "issue.to-task",
                            data: doc,
                        },
                        Err(e) => Slot::Failed(e),
                    }
                }
                Ok(_) => Slot::Failed(FleetError::internal("the answer is not an issue")),
                Err(e) => Slot::Failed(e),
            },
            Pending::Preflight { args, digest, .. } => match result {
                Ok(_) => match self.pr_start(ticket, &args, &digest) {
                    Ok(next) => {
                        self.forge.pending.insert(ticket, next);
                        return;
                    }
                    Err(e) => Slot::Failed(e),
                },
                Err(e) => Slot::Failed(e),
            },
            Pending::Publish {
                approval,
                key,
                repo,
                head,
            } => match result {
                Ok(ForgeAnswer::Published(created)) => {
                    let event = DomainEvent::PullRequestOpened {
                        id: approval.clone(),
                        run: key.0.clone(),
                        task: key.1.clone(),
                        repo: repo.full(),
                        number: created.number,
                        url: created.url.clone(),
                        head: head.clone(),
                    };
                    if let Err(e) = self.commit(vec![event]) {
                        self.warn(e.message);
                    }
                    Slot::Done {
                        kind: "pr.create",
                        data: Value::object([
                            ("task", Value::str(qualified(&key))),
                            ("repo", Value::str(repo.full())),
                            ("number", Value::int(created.number)),
                            ("url", Value::str(created.url)),
                            ("head", Value::str(head.as_str())),
                            ("approval", Value::str(approval.as_str())),
                        ]),
                    }
                }
                Ok(_) => {
                    self.record_publication_failed(
                        &approval,
                        &key,
                        "the answer is not a pull request",
                    );
                    Slot::Failed(FleetError::internal("the answer is not a pull request"))
                }
                Err(e) => {
                    self.record_publication_failed(&approval, &key, &e.message);
                    Slot::Failed(e)
                }
            },
        };
        self.forge.slots.insert(ticket, slot);
    }

    /// After a restart: publications that started and never reported back are recorded as
    /// unfinished. Nothing is started again.
    pub(crate) fn reconcile_publications(&mut self) {
        let events: Vec<DomainEvent> = self
            .forge
            .ledger
            .unfinished()
            .iter()
            .map(|p| DomainEvent::PullRequestFailed {
                id: p.approval.clone(),
                run: p.run.clone(),
                task: p.task.clone(),
                reason: INTERRUPTED_PUBLICATION.to_string(),
            })
            .collect();
        if events.is_empty() {
            return;
        }
        let count = events.len();
        match self.commit(events) {
            Ok(()) => self.warn(format!(
                "{count} pull request publication(s) were cut off by a restart: {INTERRUPTED_PUBLICATION}"
            )),
            Err(e) => self.warn(e.message),
        }
    }
}
