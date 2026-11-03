//! Executing one [`ForgeRequest`] against the ports. Shared by the threaded runner of the
//! adapters and the in-memory runner of the tests, so what a request means is defined once.

use super::ports::{BranchPublisher, CodeHost, ForgeAnswer, ForgeRequest, IssueTracker};
use fleet_domain::error::Result;

/// Runs `req` to completion on the calling thread.
///
/// A publication pushes first and creates the pull request only when the push succeeded: a
/// failed push never leaves a pull request behind, and a failed creation leaves only a pushed
/// branch (which the error says).
pub fn run(
    req: &ForgeRequest,
    tracker: &dyn IssueTracker,
    host: &dyn CodeHost,
    publisher: &dyn BranchPublisher,
) -> Result<ForgeAnswer> {
    Ok(match req {
        ForgeRequest::ListIssues { repo, query } => {
            ForgeAnswer::Issues(tracker.list_issues(repo, query)?)
        }
        ForgeRequest::ShowIssue { repo, number } => {
            ForgeAnswer::Issue(tracker.get_issue(repo, *number)?)
        }
        ForgeRequest::ListPulls { repo, query } => {
            ForgeAnswer::Pulls(host.list_pulls(repo, query)?)
        }
        ForgeRequest::ShowPull { repo, number } => ForgeAnswer::Pull(host.get_pull(repo, *number)?),
        ForgeRequest::PullStatus { repo, number } => {
            ForgeAnswer::Status(host.pull_status(repo, *number)?)
        }
        ForgeRequest::Preflight { repo } => {
            host.check_access(repo)?;
            ForgeAnswer::Ready
        }
        ForgeRequest::Publish(p) => {
            let (push, draft) = (&p.push, &p.draft);
            publisher.push(push)?;
            let created = host.create_pull(draft).map_err(|mut e| {
                e.message = format!(
                    "the branch {} was pushed to {} but the pull request was not created: {}",
                    push.branch, push.remote, e.message
                );
                e
            })?;
            ForgeAnswer::Published(created)
        }
    })
}
