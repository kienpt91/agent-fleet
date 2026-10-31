//! An in-memory forge for tests of the application layer: a tracker, a code host and a branch
//! publisher over one shared [`ForgeWorld`], and a [`ForgeJobs`] that executes requests with them
//! (at once, or when the test releases them). Tests script behaviour on the world (make a call
//! fail, hold requests, plant an issue) and read back what was asked.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use crate::forge::exec;
use crate::forge::ports::{
    BranchPublisher, CodeHost, ForgeAnswer, ForgeJobs, ForgeRequest, ForgeStatus, IssueTracker,
    ListQuery, PushRequest,
};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::Sha;
use fleet_domain::value::Value;
use fleet_forge::model::{
    CheckState, Checks, CreatedPull, Issue, IssueState, IssueSummary, Page, Pull, PullState,
    PullStatus, PullSummary, Review, StateFilter,
};
use fleet_forge::publish::PullRequestDraft;
use fleet_forge::repo::RepoRef;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Default)]
struct Inner {
    issues: Vec<Issue>,
    pulls: Vec<Pull>,
    statuses: BTreeMap<u64, (Checks, Vec<Review>)>,
    remote_urls: BTreeMap<String, String>,
    ready: bool,
    detail: String,
    fail_next: Option<FleetError>,
    fail_access: Option<FleetError>,
    fail_push: Option<FleetError>,
    fail_create: Option<FleetError>,
    fail_start: Option<FleetError>,
    hold: bool,
    calls: Vec<String>,
    pushes: Vec<PushRequest>,
    created: Vec<PullRequestDraft>,
    next_pull: u64,
}

/// Shared handle to the state behind the fake forge.
#[derive(Clone, Default)]
pub struct ForgeWorld(Arc<Mutex<Inner>>);

impl ForgeWorld {
    pub fn new() -> ForgeWorld {
        let w = ForgeWorld::default();
        {
            let mut g = w.lock();
            g.ready = true;
            g.detail = "fake gh".to_string();
            g.next_pull = 100;
        }
        w
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        match self.0.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    // ---- scripting ----
    pub fn add_issue(&self, number: u64, title: &str, body: &str, labels: &[&str]) {
        let summary = IssueSummary {
            number,
            title: title.to_string(),
            state: IssueState::Open,
            labels: labels.iter().map(|l| l.to_string()).collect(),
            author: "octocat".to_string(),
            comments: 2,
            updated: "2026-10-30T10:00:00Z".to_string(),
            url: format!("https://github.com/o/r/issues/{number}"),
        };
        self.lock().issues.push(Issue {
            summary,
            body: body.to_string(),
            body_cut: false,
        });
    }

    pub fn add_pull(&self, number: u64, title: &str, branch: &str, head: &Sha) {
        let summary = PullSummary {
            number,
            title: title.to_string(),
            state: PullState::Open,
            draft: false,
            labels: vec![],
            author: "octocat".to_string(),
            head_branch: branch.to_string(),
            base_branch: "main".to_string(),
            head_sha: head.clone(),
            comments: 0,
            updated: "2026-10-30T10:00:00Z".to_string(),
            url: format!("https://github.com/o/r/pull/{number}"),
        };
        self.lock().pulls.push(Pull {
            summary,
            body: String::new(),
            body_cut: false,
        });
    }

    pub fn set_status(&self, number: u64, checks: Checks, reviews: Vec<Review>) {
        self.lock().statuses.insert(number, (checks, reviews));
    }

    pub fn set_remote(&self, repo_path: &str, url: &str) {
        self.lock()
            .remote_urls
            .insert(repo_path.to_string(), url.to_string());
    }

    pub fn set_ready(&self, ready: bool, detail: &str) {
        let mut g = self.lock();
        g.ready = ready;
        g.detail = detail.to_string();
    }

    /// The next call that reaches the forge fails with `e`.
    pub fn fail_next(&self, e: FleetError) {
        self.lock().fail_next = Some(e);
    }

    pub fn fail_access(&self, e: Option<FleetError>) {
        self.lock().fail_access = e;
    }

    pub fn fail_push(&self, e: Option<FleetError>) {
        self.lock().fail_push = e;
    }

    pub fn fail_create(&self, e: Option<FleetError>) {
        self.lock().fail_create = e;
    }

    /// `start` itself fails (the adapter refuses to take the call).
    pub fn fail_start(&self, e: Option<FleetError>) {
        self.lock().fail_start = e;
    }

    /// While held, started requests wait; `release` runs them at the next poll.
    pub fn hold(&self, hold: bool) {
        self.lock().hold = hold;
    }

    // ---- reading back ----
    pub fn calls(&self) -> Vec<String> {
        self.lock().calls.clone()
    }

    pub fn pushes(&self) -> Vec<PushRequest> {
        self.lock().pushes.clone()
    }

    pub fn created(&self) -> Vec<PullRequestDraft> {
        self.lock().created.clone()
    }

    fn take_failure(&self) -> Result<()> {
        match self.lock().fail_next.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

fn matches_state(filter: StateFilter, open: bool) -> bool {
    match filter {
        StateFilter::All => true,
        StateFilter::Open => open,
        StateFilter::Closed => !open,
    }
}

fn page_of<T: Clone>(items: Vec<T>, q: &ListQuery) -> Page<T> {
    let start = usize::try_from(q.page.saturating_sub(1)).unwrap_or(0) * q.per_page;
    let slice: Vec<T> = items.iter().skip(start).take(q.per_page).cloned().collect();
    Page {
        has_more: items.len() > start + slice.len(),
        items: slice,
        page: q.page,
    }
}

impl IssueTracker for ForgeWorld {
    fn list_issues(&self, repo: &RepoRef, q: &ListQuery) -> Result<Page<IssueSummary>> {
        self.lock()
            .calls
            .push(format!("list_issues {repo} {:?}", q.state));
        self.take_failure()?;
        let items: Vec<IssueSummary> = self
            .lock()
            .issues
            .iter()
            .filter(|i| matches_state(q.state, i.summary.state == IssueState::Open))
            .map(|i| i.summary.clone())
            .collect();
        Ok(page_of(items, q))
    }

    fn get_issue(&self, repo: &RepoRef, number: u64) -> Result<Issue> {
        self.lock().calls.push(format!("get_issue {repo} {number}"));
        self.take_failure()?;
        self.lock()
            .issues
            .iter()
            .find(|i| i.summary.number == number)
            .cloned()
            .ok_or_else(|| FleetError::not_found("GitHub: not found"))
    }
}

impl CodeHost for ForgeWorld {
    fn list_pulls(&self, repo: &RepoRef, q: &ListQuery) -> Result<Page<PullSummary>> {
        self.lock()
            .calls
            .push(format!("list_pulls {repo} {:?}", q.state));
        self.take_failure()?;
        let items: Vec<PullSummary> = self
            .lock()
            .pulls
            .iter()
            .filter(|p| matches_state(q.state, p.summary.state == PullState::Open))
            .map(|p| p.summary.clone())
            .collect();
        Ok(page_of(items, q))
    }

    fn get_pull(&self, repo: &RepoRef, number: u64) -> Result<Pull> {
        self.lock().calls.push(format!("get_pull {repo} {number}"));
        self.take_failure()?;
        self.lock()
            .pulls
            .iter()
            .find(|p| p.summary.number == number)
            .cloned()
            .ok_or_else(|| FleetError::not_found("GitHub: not found"))
    }

    fn pull_status(&self, repo: &RepoRef, number: u64) -> Result<PullStatus> {
        self.lock()
            .calls
            .push(format!("pull_status {repo} {number}"));
        self.take_failure()?;
        let pull = self.get_pull(repo, number)?;
        let (checks, reviews) = self.lock().statuses.get(&number).cloned().unwrap_or((
            Checks {
                state: CheckState::None,
                total: 0,
                passed: 0,
                failed: 0,
                pending: 0,
            },
            vec![],
        ));
        Ok(PullStatus {
            pull: pull.summary,
            checks,
            reviews,
        })
    }

    fn create_pull(&self, draft: &PullRequestDraft) -> Result<CreatedPull> {
        self.lock().calls.push("create_pull".to_string());
        self.take_failure()?;
        if let Some(e) = self.lock().fail_create.clone() {
            return Err(e);
        }
        let mut g = self.lock();
        g.next_pull += 1;
        let number = g.next_pull;
        g.created.push(draft.clone());
        Ok(CreatedPull {
            number,
            url: format!("https://github.com/o/r/pull/{number}"),
        })
    }

    fn check_access(&self, repo: &RepoRef) -> Result<()> {
        self.lock().calls.push(format!("check_access {repo}"));
        self.take_failure()?;
        match self.lock().fail_access.clone() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl BranchPublisher for ForgeWorld {
    fn remote_url(&self, repo_path: &str, _remote: &str) -> Result<String> {
        self.lock()
            .remote_urls
            .get(repo_path)
            .cloned()
            .ok_or_else(|| FleetError::not_found("the remote does not exist"))
    }

    fn push(&self, req: &PushRequest) -> Result<()> {
        self.lock().calls.push(format!("push {}", req.branch));
        if let Some(e) = self.lock().fail_push.clone() {
            return Err(e);
        }
        self.lock().pushes.push(req.clone());
        Ok(())
    }
}

/// The asynchronous wrapper over a [`ForgeWorld`]: requests run when they are started (or, while
/// the world is held, at the first poll after the hold ended).
pub struct FakeForgeJobs {
    world: ForgeWorld,
    queued: Vec<(u64, ForgeRequest)>,
    done: VecDeque<(u64, Result<ForgeAnswer>)>,
}

impl FakeForgeJobs {
    pub fn new(world: ForgeWorld) -> FakeForgeJobs {
        FakeForgeJobs {
            world,
            queued: Vec::new(),
            done: VecDeque::new(),
        }
    }

    fn execute(&mut self, ticket: u64, req: &ForgeRequest) {
        let w = &self.world;
        let result = exec::run(req, w, w, w);
        self.done.push_back((ticket, result));
    }
}

impl ForgeJobs for FakeForgeJobs {
    fn status(&self) -> ForgeStatus {
        let g = self.world.lock();
        ForgeStatus {
            ready: g.ready,
            detail: g.detail.clone(),
            rate: Value::object([("state", Value::str("ok"))]),
        }
    }

    fn start(&mut self, ticket: u64, req: ForgeRequest) -> Result<()> {
        if let Some(e) = self.world.lock().fail_start.clone() {
            return Err(e);
        }
        if self.world.lock().hold {
            self.queued.push((ticket, req));
        } else {
            self.execute(ticket, &req);
        }
        Ok(())
    }

    fn poll(&mut self) -> Vec<(u64, Result<ForgeAnswer>)> {
        if !self.world.lock().hold {
            for (ticket, req) in std::mem::take(&mut self.queued) {
                self.execute(ticket, &req);
            }
        }
        self.done.drain(..).collect()
    }

    fn running(&self) -> usize {
        self.queued.len()
    }

    fn remote_url(&self, repo_path: &str, remote: &str) -> Result<String> {
        BranchPublisher::remote_url(&self.world, repo_path, remote)
    }
}
