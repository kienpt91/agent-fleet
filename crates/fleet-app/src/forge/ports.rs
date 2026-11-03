//! The ports of the forge integration. All are synchronous and thread-safe: the adapter decides
//! what a call costs (a child process, a rate-limit refusal) and the asynchronous wrapper
//! [`ForgeJobs`] keeps that off the orchestrator's thread.

use fleet_domain::error::Result;
use fleet_domain::ids::Sha;
use fleet_domain::value::Value;
use fleet_forge::model::{
    CreatedPull, Issue, IssueSummary, Page, Pull, PullStatus, PullSummary, StateFilter,
};
use fleet_forge::publish::PullRequestDraft;
use fleet_forge::repo::RepoRef;

/// Which page of a list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListQuery {
    pub state: StateFilter,
    /// From 1.
    pub page: u64,
    pub per_page: usize,
}

/// Issues. Reading only: changing an issue is not something fleet does.
pub trait IssueTracker: Send + Sync {
    fn list_issues(&self, repo: &RepoRef, q: &ListQuery) -> Result<Page<IssueSummary>>;
    fn get_issue(&self, repo: &RepoRef, number: u64) -> Result<Issue>;
}

/// Pull requests (or whatever a host calls a reviewed change).
pub trait CodeHost: Send + Sync {
    fn list_pulls(&self, repo: &RepoRef, q: &ListQuery) -> Result<Page<PullSummary>>;
    fn get_pull(&self, repo: &RepoRef, number: u64) -> Result<Pull>;
    /// The pull request with the checks of its head commit and its standing reviews.
    fn pull_status(&self, repo: &RepoRef, number: u64) -> Result<PullStatus>;
    /// Creates the pull request for `draft`. The branch must already be on the host.
    fn create_pull(&self, draft: &PullRequestDraft) -> Result<CreatedPull>;
    /// One cheap read that proves the host can be reached, the account is signed in and the
    /// repository is visible to it. Nothing is changed.
    fn check_access(&self, repo: &RepoRef) -> Result<()>;
}

/// What to push: exactly one commit to exactly one branch name of one remote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushRequest {
    /// The local repository.
    pub repo_path: String,
    /// The name of the configured remote.
    pub remote: String,
    /// The branch on the remote (and locally): `fleet/<run>/<task>`.
    pub branch: String,
    /// The commit the local branch must still point at, and that is pushed.
    pub head: Sha,
    /// The repository the remote must name.
    pub target: RepoRef,
}

/// Pushing a branch to the remote. Never forces, never pushes tags or anything else.
pub trait BranchPublisher: Send + Sync {
    /// The URL the local repository's remote points at (read only).
    fn remote_url(&self, repo_path: &str, remote: &str) -> Result<String>;
    fn push(&self, req: &PushRequest) -> Result<()>;
}

/// One unit of forge work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForgeRequest {
    ListIssues {
        repo: RepoRef,
        query: ListQuery,
    },
    ShowIssue {
        repo: RepoRef,
        number: u64,
    },
    ListPulls {
        repo: RepoRef,
        query: ListQuery,
    },
    ShowPull {
        repo: RepoRef,
        number: u64,
    },
    PullStatus {
        repo: RepoRef,
        number: u64,
    },
    /// Proves the host answers and the account may see the repository, before anything is
    /// consumed or changed.
    Preflight {
        repo: RepoRef,
    },
    /// Push the branch of `push`, then create the pull request `draft`.
    Publish(Box<PublishRequest>),
}

/// What publishing a pull request is made of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishRequest {
    pub push: PushRequest,
    pub draft: PullRequestDraft,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForgeAnswer {
    Issues(Page<IssueSummary>),
    Issue(Issue),
    Pulls(Page<PullSummary>),
    Pull(Pull),
    Status(PullStatus),
    Published(CreatedPull),
    /// The preflight passed.
    Ready,
}

impl ForgeAnswer {
    pub fn to_value(&self) -> Value {
        match self {
            ForgeAnswer::Issues(p) => p.to_value(IssueSummary::to_value),
            ForgeAnswer::Issue(i) => i.to_value(),
            ForgeAnswer::Pulls(p) => p.to_value(PullSummary::to_value),
            ForgeAnswer::Pull(p) => p.to_value(),
            ForgeAnswer::Status(s) => s.to_value(),
            ForgeAnswer::Published(c) => Value::object([
                ("number", Value::int(c.number)),
                ("url", Value::str(c.url.as_str())),
            ]),
            ForgeAnswer::Ready => Value::object([("ready", Value::Bool(true))]),
        }
    }
}

/// What `fleet github status` shows about the adapter.
#[derive(Clone, Debug, PartialEq)]
pub struct ForgeStatus {
    /// The adapter can start calls (the program was found and trusted).
    pub ready: bool,
    /// One line: where the program is, or why there is none.
    pub detail: String,
    /// The rate gate: `ok`, `limited` or `budget` with its numbers.
    pub rate: Value,
}

/// Forge calls run off the orchestrator's thread: `start` hands one over and returns at once,
/// `poll` collects what finished. A call that cannot even start (the queue is full) is an error
/// of `start`; one that fails is an error in the answer.
pub trait ForgeJobs: Send {
    /// `repos` are the local repositories of every run: the adapter refuses to run a program that
    /// lies inside one of them.
    fn status(&self, repos: &[String]) -> ForgeStatus;
    fn start(&mut self, ticket: u64, req: ForgeRequest, repos: &[String]) -> Result<()>;
    /// Whether `start` would take a call now (the program is trusted, the rate gate is open, a
    /// slot is free), without starting anything. A publication asks before its approval is spent.
    fn can_start(&self, repos: &[String]) -> Result<()>;
    fn poll(&mut self) -> Vec<(u64, Result<ForgeAnswer>)>;
    /// Calls in flight.
    fn running(&self) -> usize;
    /// The URL the remote of a local repository points at (a read of its configuration).
    fn remote_url(&self, repo_path: &str, remote: &str) -> Result<String>;
}
