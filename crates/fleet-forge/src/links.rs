//! The forge events, folded: which task came from which issue, which pull request came from which
//! task, and which publication started but never reported back. A projection of the event log like
//! the fleet state and the goal ledger, so it survives a restart. The lists the views show keep
//! the newest [`MAX_LINKS`] entries. What decides something is never dropped with them: the issue
//! and pull request keys that guard against doing the same thing twice are small and kept whole,
//! and so is every publication that has not reported back (a restart must mark it).

use crate::limits::MAX_LINKS;
use fleet_domain::events::DomainEvent;
use fleet_domain::ids::{ApprovalId, RunId, Sha, TaskId};
use fleet_domain::value::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueLink {
    pub run: RunId,
    pub task: TaskId,
    /// `host/owner/name`.
    pub repo: String,
    pub number: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishState {
    /// Started; nothing reported back yet.
    Started,
    Opened {
        number: u64,
        url: String,
    },
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publication {
    pub approval: ApprovalId,
    pub run: RunId,
    pub task: TaskId,
    pub repo: String,
    pub base: String,
    pub branch: String,
    pub head: Sha,
    pub state: PublishState,
}

#[derive(Clone, Debug, Default)]
pub struct ForgeLedger {
    /// The repository each run first used on the forge when `[github] repo` was not set.
    repos: BTreeMap<RunId, String>,
    issues: Vec<IssueLink>,
    publications: Vec<Publication>,
    /// Every issue that was made a task, by (repository, number); not capped.
    issue_keys: BTreeMap<(String, u64), IssueLink>,
    /// Every opened pull request, by (run, task, repository, head commit): its number; not capped.
    opened: BTreeMap<(RunId, TaskId, String, String), u64>,
    /// Every publication that started and has no outcome yet; not capped.
    pending: BTreeMap<ApprovalId, Publication>,
}

impl ForgeLedger {
    pub fn new() -> ForgeLedger {
        ForgeLedger::default()
    }

    /// The repository (`host/owner/name`) recorded for a run, if any.
    pub fn recorded_repo(&self, run: &RunId) -> Option<&str> {
        self.repos.get(run).map(String::as_str)
    }

    pub fn apply(&mut self, ev: &DomainEvent) {
        match ev {
            DomainEvent::ForgeRepoRecorded { run, repo } => {
                self.repos
                    .entry(run.clone())
                    .or_insert_with(|| repo.clone());
            }
            DomainEvent::IssueLinked {
                run,
                task,
                repo,
                number,
            } => {
                let link = IssueLink {
                    run: run.clone(),
                    task: task.clone(),
                    repo: repo.clone(),
                    number: *number,
                };
                self.issue_keys
                    .insert((repo.clone(), *number), link.clone());
                self.issues.push(link);
                if self.issues.len() > MAX_LINKS {
                    self.issues.remove(0);
                }
            }
            DomainEvent::PullRequestStarted {
                id,
                run,
                task,
                repo,
                base,
                branch,
                head,
            } => {
                let publication = Publication {
                    approval: id.clone(),
                    run: run.clone(),
                    task: task.clone(),
                    repo: repo.clone(),
                    base: base.clone(),
                    branch: branch.clone(),
                    head: head.clone(),
                    state: PublishState::Started,
                };
                self.pending.insert(id.clone(), publication.clone());
                self.publications.push(publication);
                if self.publications.len() > MAX_LINKS {
                    self.publications.remove(0);
                }
            }
            DomainEvent::PullRequestOpened {
                id,
                run,
                task,
                repo,
                number,
                url,
                head,
            } => {
                self.pending.remove(id);
                self.opened.insert(
                    (
                        run.clone(),
                        task.clone(),
                        repo.clone(),
                        head.as_str().to_string(),
                    ),
                    *number,
                );
                if let Some(p) = self.publications.iter_mut().find(|p| p.approval == *id) {
                    p.state = PublishState::Opened {
                        number: *number,
                        url: url.clone(),
                    };
                }
            }
            DomainEvent::PullRequestFailed { id, reason, .. } => {
                self.pending.remove(id);
                if let Some(p) = self.publications.iter_mut().find(|p| p.approval == *id) {
                    p.state = PublishState::Failed(reason.clone());
                }
            }
            _ => {}
        }
    }

    pub fn issues(&self) -> &[IssueLink] {
        &self.issues
    }

    pub fn publications(&self) -> &[Publication] {
        &self.publications
    }

    /// The issue a task was made from.
    pub fn issue_of(&self, run: &RunId, task: &TaskId) -> Option<&IssueLink> {
        self.issues
            .iter()
            .rev()
            .find(|l| l.run == *run && l.task == *task)
    }

    /// The task already made from issue `number` of `repo` (whatever else was linked since).
    pub fn task_of_issue(&self, repo: &str, number: u64) -> Option<&IssueLink> {
        self.issue_keys.get(&(repo.to_string(), number))
    }

    /// The number of the pull request already opened for this task's commit on `repo`.
    pub fn opened_number(&self, run: &RunId, task: &TaskId, repo: &str, head: &Sha) -> Option<u64> {
        self.opened
            .get(&(
                run.clone(),
                task.clone(),
                repo.to_string(),
                head.as_str().to_string(),
            ))
            .copied()
    }

    /// Publications that started and never reported back: the daemon stopped meanwhile, and the
    /// branch may or may not have been pushed, the pull request may or may not exist.
    pub fn unfinished(&self) -> Vec<&Publication> {
        self.pending.values().collect()
    }

    /// The pull requests opened for a task, oldest first.
    pub fn pulls_of(&self, run: &RunId, task: &TaskId) -> Vec<&Publication> {
        self.publications
            .iter()
            .filter(|p| p.run == *run && p.task == *task)
            .collect()
    }

    /// The document the read-only views show.
    pub fn to_value(&self) -> Value {
        let issues = self.issues.iter().map(|l| {
            Value::object([
                ("run", Value::str(l.run.as_str())),
                ("task", Value::str(l.task.as_str())),
                ("repo", Value::str(l.repo.as_str())),
                ("number", Value::int(l.number)),
            ])
        });
        let pulls = self.publications.iter().map(|p| {
            let (state, number, url, reason) = match &p.state {
                PublishState::Started => ("started", Value::Null, Value::Null, Value::Null),
                PublishState::Opened { number, url } => (
                    "opened",
                    Value::int(*number),
                    Value::str(url.as_str()),
                    Value::Null,
                ),
                PublishState::Failed(why) => {
                    ("failed", Value::Null, Value::Null, Value::str(why.as_str()))
                }
            };
            Value::object([
                ("approval", Value::str(p.approval.as_str())),
                ("run", Value::str(p.run.as_str())),
                ("task", Value::str(p.task.as_str())),
                ("repo", Value::str(p.repo.as_str())),
                ("base", Value::str(p.base.as_str())),
                ("branch", Value::str(p.branch.as_str())),
                ("head", Value::str(p.head.as_str())),
                ("state", Value::str(state)),
                ("number", number),
                ("url", url),
                ("reason", reason),
            ])
        });
        Value::object([
            ("issues", Value::array(issues)),
            ("pulls", Value::array(pulls)),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> RunId {
        RunId::parse("r1").unwrap()
    }
    fn task(s: &str) -> TaskId {
        TaskId::parse(s).unwrap()
    }
    fn sha(c: char) -> Sha {
        Sha::parse(&c.to_string().repeat(40)).unwrap()
    }
    fn approval(n: u64) -> ApprovalId {
        ApprovalId::from_seq(n)
    }

    fn started(n: u64, t: &str) -> DomainEvent {
        DomainEvent::PullRequestStarted {
            id: approval(n),
            run: run(),
            task: task(t),
            repo: "github.com/o/r".into(),
            base: "main".into(),
            branch: format!("fleet/r1/{t}"),
            head: sha('a'),
        }
    }

    #[test]
    fn issues_and_tasks_find_each_other() {
        let mut l = ForgeLedger::new();
        l.apply(&DomainEvent::IssueLinked {
            run: run(),
            task: task("a"),
            repo: "github.com/o/r".into(),
            number: 7,
        });
        assert_eq!(l.issue_of(&run(), &task("a")).map(|i| i.number), Some(7));
        assert!(l.issue_of(&run(), &task("b")).is_none());
        assert_eq!(
            l.task_of_issue("github.com/o/r", 7).map(|i| i.task.clone()),
            Some(task("a"))
        );
        assert!(l.task_of_issue("github.com/o/other", 7).is_none());
        assert!(l.task_of_issue("github.com/o/r", 8).is_none());
    }

    #[test]
    fn a_publication_goes_from_started_to_opened_or_failed() {
        let mut l = ForgeLedger::new();
        l.apply(&started(1, "a"));
        l.apply(&started(2, "b"));
        assert_eq!(l.unfinished().len(), 2);
        l.apply(&DomainEvent::PullRequestOpened {
            id: approval(1),
            run: run(),
            task: task("a"),
            repo: "github.com/o/r".into(),
            number: 12,
            url: "https://github.com/o/r/pull/12".into(),
            head: sha('a'),
        });
        l.apply(&DomainEvent::PullRequestFailed {
            id: approval(2),
            run: run(),
            task: task("b"),
            reason: "push refused".into(),
        });
        assert!(l.unfinished().is_empty());
        let a = l.pulls_of(&run(), &task("a"));
        assert_eq!(a.len(), 1);
        assert_eq!(
            a[0].state,
            PublishState::Opened {
                number: 12,
                url: "https://github.com/o/r/pull/12".into()
            }
        );
        let b = l.pulls_of(&run(), &task("b"));
        assert_eq!(b[0].state, PublishState::Failed("push refused".into()));
        // an outcome for an unknown approval changes nothing
        l.apply(&DomainEvent::PullRequestFailed {
            id: approval(9),
            run: run(),
            task: task("z"),
            reason: "x".into(),
        });
        assert_eq!(l.publications().len(), 2);
    }

    #[test]
    fn the_projection_is_bounded_and_unrelated_events_are_ignored() {
        let mut l = ForgeLedger::new();
        for n in 0..(MAX_LINKS as u64 + 10) {
            l.apply(&DomainEvent::IssueLinked {
                run: run(),
                task: task("a"),
                repo: "h/o/r".into(),
                number: n + 1,
            });
            l.apply(&started(n + 1, "a"));
        }
        assert_eq!(l.issues().len(), MAX_LINKS);
        assert_eq!(l.publications().len(), MAX_LINKS);
        l.apply(&DomainEvent::Warning {
            message: "x".into(),
        });
        assert_eq!(l.issues().len(), MAX_LINKS);
    }

    #[test]
    fn the_document_lists_both_kinds() {
        let mut l = ForgeLedger::new();
        l.apply(&DomainEvent::IssueLinked {
            run: run(),
            task: task("a"),
            repo: "github.com/o/r".into(),
            number: 7,
        });
        l.apply(&started(1, "a"));
        let v = l.to_value();
        assert_eq!(
            v.get("issues")
                .and_then(Value::as_array)
                .map(<[Value]>::len),
            Some(1)
        );
        let pulls = v.get("pulls").and_then(Value::as_array).unwrap();
        assert_eq!(
            pulls[0].get("state").and_then(Value::as_str),
            Some("started")
        );
    }

    #[test]
    fn what_guards_against_doing_it_twice_outlives_the_capped_lists() {
        let mut l = ForgeLedger::new();
        let total = MAX_LINKS as u64 + 10;
        for n in 0..total {
            l.apply(&DomainEvent::IssueLinked {
                run: run(),
                task: task("a"),
                repo: "h/o/r".into(),
                number: n + 1,
            });
            l.apply(&started(n + 1, "a"));
        }
        // the oldest entries are gone from the lists, not from the guards
        assert!(l.issues().iter().all(|i| i.number > 10));
        assert!(l.publications().iter().all(|p| p.approval != approval(1)));
        assert!(l.task_of_issue("h/o/r", 1).is_some());
        assert_eq!(l.unfinished().len() as u64, total);
        assert!(l.unfinished().iter().any(|p| p.approval == approval(1)));
        // an outcome closes it, and an opened one is found by (task, commit)
        l.apply(&DomainEvent::PullRequestOpened {
            id: approval(1),
            run: run(),
            task: task("a"),
            repo: "github.com/o/r".into(),
            number: 55,
            url: "https://github.com/o/r/pull/55".into(),
            head: sha('a'),
        });
        assert_eq!(l.unfinished().len() as u64, total - 1);
        let at = |c: char| l.opened_number(&run(), &task("a"), "github.com/o/r", &sha(c));
        assert_eq!((at('a'), at('b')), (Some(55), None));
    }

    #[test]
    fn a_run_keeps_the_first_repository_recorded_for_it() {
        let mut l = ForgeLedger::new();
        assert_eq!(l.recorded_repo(&run()), None);
        for repo in ["h/o/r", "h/evil/r"] {
            l.apply(&DomainEvent::ForgeRepoRecorded {
                run: run(),
                repo: repo.into(),
            });
        }
        assert_eq!(l.recorded_repo(&run()), Some("h/o/r"));
    }
}
