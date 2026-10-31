//! The forge commands of the wire protocol, parsed into values. Kept apart from `crate::api` so
//! that file only gains one variant ([`Command::Forge`](crate::api::Command::Forge)).

use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::{RunId, TaskId, WorkerName};
use fleet_domain::value::Value;
use fleet_forge::limits::{DEFAULT_PAGE_ITEMS, MAX_NUMBER, MAX_PAGE, MAX_PAGE_ITEMS};
use fleet_forge::model::StateFilter;

/// `issue.list` and `pr.list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListArgs {
    /// The run whose repository is meant, when `[github] repo` is not set.
    pub run: Option<RunId>,
    pub state: StateFilter,
    pub page: u64,
    pub limit: usize,
}

/// `issue.to-task`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueToTaskArgs {
    pub run: Option<RunId>,
    pub number: u64,
    pub id: Option<TaskId>,
    pub after: Vec<TaskId>,
    pub worker: Option<WorkerName>,
}

/// `pr.create`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrCreateArgs {
    pub task: String,
    pub title: Option<String>,
    pub body: Option<String>,
    pub base: Option<String>,
    pub draft: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForgeCommand {
    Status,
    /// What the last list and show commands fetched, read from memory.
    Snapshot,
    /// Tasks and the issues they came from, pull requests and the tasks they came from.
    Links,
    /// The outcome of a command that returned a ticket.
    Result {
        ticket: u64,
    },
    IssueList(ListArgs),
    IssueShow {
        run: Option<RunId>,
        number: u64,
    },
    IssueToTask(IssueToTaskArgs),
    PrList(ListArgs),
    PrShow {
        run: Option<RunId>,
        number: u64,
    },
    PrStatus {
        run: Option<RunId>,
        number: u64,
    },
    PrCreate(PrCreateArgs),
}

/// Commands that change nothing and start nothing.
pub fn is_read_only(cmd: &ForgeCommand) -> bool {
    matches!(
        cmd,
        ForgeCommand::Status
            | ForgeCommand::Snapshot
            | ForgeCommand::Links
            | ForgeCommand::Result { .. }
    )
}

fn opt_str<'a>(a: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match a.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Str(s)) => Ok(Some(s)),
        Some(_) => Err(FleetError::invalid(format!("`{key}` must be a string"))),
    }
}

fn opt_u64(a: &Value, key: &str) -> Result<Option<u64>> {
    match a.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| FleetError::invalid(format!("`{key}` must be a non-negative integer"))),
    }
}

fn number(a: &Value) -> Result<u64> {
    match opt_u64(a, "number")? {
        Some(n) if (1..=MAX_NUMBER).contains(&n) => Ok(n),
        _ => Err(FleetError::invalid(
            "`number` is required and must be a positive integer",
        )),
    }
}

fn run(a: &Value) -> Result<Option<RunId>> {
    opt_str(a, "run")?.map(RunId::parse).transpose()
}

fn list(a: &Value) -> Result<ListArgs> {
    let state = match opt_str(a, "state")? {
        None => StateFilter::Open,
        Some(s) => StateFilter::parse(s)
            .ok_or_else(|| FleetError::invalid("`state` must be open, closed or all"))?,
    };
    let page = opt_u64(a, "page")?.unwrap_or(1);
    if page == 0 || page > MAX_PAGE {
        return Err(FleetError::invalid(format!(
            "`page` must be between 1 and {MAX_PAGE}"
        )));
    }
    let limit = opt_u64(a, "limit")?
        .map_or(DEFAULT_PAGE_ITEMS, |n| {
            usize::try_from(n).unwrap_or(usize::MAX)
        })
        .clamp(1, MAX_PAGE_ITEMS);
    Ok(ListArgs {
        run: run(a)?,
        state,
        page,
        limit,
    })
}

fn string_list(a: &Value, key: &str) -> Result<Vec<String>> {
    match a.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str().map(str::to_string).ok_or_else(|| {
                    FleetError::invalid(format!("`{key}` must be a list of strings"))
                })
            })
            .collect(),
        Some(_) => Err(FleetError::invalid(format!(
            "`{key}` must be a list of strings"
        ))),
    }
}

/// Parses a forge command; `None` when `name` is not one.
pub fn parse(name: &str, a: &Value) -> Result<Option<ForgeCommand>> {
    Ok(Some(match name {
        "forge.status" => ForgeCommand::Status,
        "forge.snapshot" => ForgeCommand::Snapshot,
        "forge.links" => ForgeCommand::Links,
        "forge.result" => ForgeCommand::Result {
            ticket: opt_u64(a, "ticket")?
                .ok_or_else(|| FleetError::invalid("`ticket` is required"))?,
        },
        "issue.list" => ForgeCommand::IssueList(list(a)?),
        "issue.show" => ForgeCommand::IssueShow {
            run: run(a)?,
            number: number(a)?,
        },
        "issue.to-task" => ForgeCommand::IssueToTask(IssueToTaskArgs {
            run: run(a)?,
            number: number(a)?,
            id: opt_str(a, "id")?.map(TaskId::parse).transpose()?,
            after: string_list(a, "after")?
                .iter()
                .map(|s| TaskId::parse(s))
                .collect::<Result<Vec<_>>>()?,
            worker: opt_str(a, "worker")?.map(WorkerName::parse).transpose()?,
        }),
        "pr.list" => ForgeCommand::PrList(list(a)?),
        "pr.show" => ForgeCommand::PrShow {
            run: run(a)?,
            number: number(a)?,
        },
        "pr.status" => ForgeCommand::PrStatus {
            run: run(a)?,
            number: number(a)?,
        },
        "pr.create" => ForgeCommand::PrCreate(PrCreateArgs {
            task: opt_str(a, "task")?
                .ok_or_else(|| FleetError::invalid("`task` is required and must be a string"))?
                .to_string(),
            title: opt_str(a, "title")?.map(str::to_string),
            body: opt_str(a, "body")?.map(str::to_string),
            base: opt_str(a, "base")?.map(str::to_string),
            draft: match a.get("draft") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err(FleetError::invalid("`draft` must be true or false")),
            },
        }),
        _ => return Ok(None),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(pairs: Vec<(&str, Value)>) -> Value {
        Value::object(pairs)
    }

    fn p(name: &str, a: Value) -> Result<Option<ForgeCommand>> {
        parse(name, &a)
    }

    #[test]
    fn every_forge_command_parses_and_others_are_not_claimed() {
        assert_eq!(
            p("forge.status", Value::Null).unwrap(),
            Some(ForgeCommand::Status)
        );
        assert_eq!(
            p("forge.snapshot", Value::Null).unwrap(),
            Some(ForgeCommand::Snapshot)
        );
        assert_eq!(
            p("forge.links", Value::Null).unwrap(),
            Some(ForgeCommand::Links)
        );
        assert_eq!(
            p("forge.result", obj(vec![("ticket", Value::int(3))])).unwrap(),
            Some(ForgeCommand::Result { ticket: 3 })
        );
        assert_eq!(
            p(
                "issue.show",
                obj(vec![("number", Value::int(7)), ("run", Value::str("r1"))])
            )
            .unwrap(),
            Some(ForgeCommand::IssueShow {
                run: Some(RunId::parse("r1").unwrap()),
                number: 7
            })
        );
        assert_eq!(p("status", Value::Null).unwrap(), None);
        assert_eq!(p("merge", Value::Null).unwrap(), None);
        assert_eq!(p("pr", Value::Null).unwrap(), None);
    }

    #[test]
    fn lists_have_bounded_pages_and_a_closed_state_vocabulary() {
        let Some(ForgeCommand::IssueList(l)) = p("issue.list", Value::Null).unwrap() else {
            panic!()
        };
        assert_eq!(
            (l.state, l.page, l.limit, l.run),
            (StateFilter::Open, 1, DEFAULT_PAGE_ITEMS, None)
        );
        let Some(ForgeCommand::PrList(l)) = p(
            "pr.list",
            obj(vec![
                ("state", Value::str("all")),
                ("page", Value::int(3)),
                ("limit", Value::int(10_000)),
            ]),
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(
            (l.state, l.page, l.limit),
            (StateFilter::All, 3, MAX_PAGE_ITEMS)
        );
        for bad in [
            obj(vec![("state", Value::str("merged"))]),
            obj(vec![("page", Value::int(0))]),
            obj(vec![("page", Value::int(MAX_PAGE + 1))]),
            obj(vec![("page", Value::str("2"))]),
            obj(vec![("limit", Value::int(-1))]),
            obj(vec![("run", Value::str("bad run"))]),
        ] {
            assert!(p("issue.list", bad.clone()).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn numbers_must_be_positive_integers() {
        for bad in [
            Value::Null,
            Value::int(0),
            Value::int(-3),
            Value::str("7"),
            Value::Float(1.5),
            Value::int(MAX_NUMBER + 1),
        ] {
            assert!(
                p("issue.show", obj(vec![("number", bad.clone())])).is_err(),
                "{bad:?}"
            );
        }
        assert!(p("pr.status", obj(vec![])).is_err());
    }

    #[test]
    fn to_task_and_create_take_their_options() {
        let Some(ForgeCommand::IssueToTask(a)) = p(
            "issue.to-task",
            obj(vec![
                ("number", Value::int(7)),
                ("id", Value::str("fix")),
                ("after", Value::array([Value::str("a"), Value::str("b")])),
                ("worker", Value::str("agent")),
            ]),
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(
            (a.number, a.id.map(|i| i.to_string()), a.after.len()),
            (7, Some("fix".into()), 2)
        );
        assert!(p(
            "issue.to-task",
            obj(vec![("number", Value::int(7)), ("after", Value::str("a"))])
        )
        .is_err());
        let Some(ForgeCommand::PrCreate(c)) = p(
            "pr.create",
            obj(vec![
                ("task", Value::str("r1/a")),
                ("title", Value::str("T")),
                ("draft", Value::Bool(true)),
            ]),
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(
            (c.task.as_str(), c.title.as_deref(), c.body, c.base, c.draft),
            ("r1/a", Some("T"), None, None, true)
        );
        assert!(p("pr.create", obj(vec![])).is_err());
        assert!(p(
            "pr.create",
            obj(vec![
                ("task", Value::str("a")),
                ("draft", Value::str("yes"))
            ])
        )
        .is_err());
        assert!(p(
            "pr.create",
            obj(vec![("task", Value::str("a")), ("title", Value::int(1))])
        )
        .is_err());
    }

    #[test]
    fn only_the_status_commands_are_read_only() {
        for c in [
            ForgeCommand::Status,
            ForgeCommand::Snapshot,
            ForgeCommand::Links,
            ForgeCommand::Result { ticket: 1 },
        ] {
            assert!(is_read_only(&c), "{c:?}");
        }
        for name in [
            "issue.list",
            "issue.show",
            "issue.to-task",
            "pr.list",
            "pr.show",
            "pr.status",
            "pr.create",
        ] {
            let a = obj(vec![("number", Value::int(1)), ("task", Value::str("a"))]);
            let c = p(name, a).unwrap().unwrap();
            assert!(!is_read_only(&c), "{name}");
        }
    }
}
