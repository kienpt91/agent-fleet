//! The commands the orchestrator accepts and the replies it gives.
//!
//! Requests arrive as neutral [`Value`] documents (the transport adapter turns JSON text into
//! them) and are parsed here, so every adapter shares one definition of what a request is.

use crate::ports::SessionId;
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::{ApprovalId, DecisionId, GoalId, RunId, TaskId, WorkerName};
use fleet_domain::limits::MAX_SEND_BYTES;
use fleet_domain::run::NewTask;
use fleet_domain::status::GoalStatus;
use fleet_domain::value::{Document, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRunArgs {
    pub id: Option<RunId>,
    pub objective: String,
    pub repo: String,
    pub base: Option<String>,
    pub max_parallel: Option<u32>,
}

/// A goal to add to the goal ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewGoalArgs {
    pub id: Option<GoalId>,
    pub parent: Option<GoalId>,
    pub title: String,
    pub intent: String,
    /// `run/task` references (or bare task ids that are unique across runs).
    pub tasks: Vec<String>,
    /// A verification contract document, validated by the application.
    pub contract: Option<Document>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Ping,
    Info,
    Status {
        run: Option<RunId>,
    },
    CreateRun(NewRunArgs),
    AddTask {
        run: RunId,
        task: NewTask,
    },
    /// Load a whole DAG, into an existing run or into a run created first.
    LoadPlan {
        run: Option<RunId>,
        new_run: Option<NewRunArgs>,
        tasks: Vec<NewTask>,
    },
    StartRun {
        run: RunId,
        max_parallel: Option<u32>,
    },
    Stop {
        task: String,
    },
    Retry {
        task: String,
    },
    Cancel {
        task: String,
    },
    Tail {
        task: String,
        since: Option<u64>,
        max: usize,
    },
    Send {
        task: String,
        text: String,
        newline: bool,
    },
    Attach {
        task: String,
    },
    GateList,
    ResolveGate {
        id: DecisionId,
        answer: String,
    },
    ApprovalList,
    Decide {
        id: ApprovalId,
        approve: bool,
        /// The digest of the action the decision covers (at least 12 hex digits). Required.
        digest: String,
    },
    Merge {
        task: String,
    },
    Cleanup {
        run: Option<RunId>,
        branches: bool,
    },
    Undo {
        task: String,
        apply: bool,
    },
    Events {
        limit: usize,
    },
    GoalAdd(NewGoalArgs),
    GoalAttach {
        goal: GoalId,
        contract: Document,
    },
    GoalSetStatus {
        goal: GoalId,
        status: GoalStatus,
        reason: String,
    },
    GoalList,
    GoalShow {
        goal: GoalId,
    },
    GoalRemaining,
    /// Runs the contract of a goal, or the contracts of every goal a task works on, in the
    /// task's worktree. `task` names the worktree for a goal that does not name exactly one task.
    Verify {
        target: String,
        task: Option<String>,
    },
    VerifyStatus {
        target: String,
        task: Option<String>,
        /// The ticket `verify` returned: report on that run.
        ticket: Option<u64>,
    },
    /// The forge integration: issues, pull requests and their links to tasks.
    Forge(crate::forge::api::ForgeCommand),
    Shutdown,
}

/// What a handled command produced.
#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    Data(Value),
    /// The transport must write `bytes` into the session's terminal, then reply with `data`.
    Input {
        session: SessionId,
        bytes: Vec<u8>,
        data: Value,
    },
    /// The transport should open an interactive stream to the session.
    Attach {
        session: SessionId,
        data: Value,
    },
}

impl Reply {
    pub fn data(&self) -> &Value {
        match self {
            Reply::Data(v) => v,
            Reply::Input { data, .. } | Reply::Attach { data, .. } => data,
        }
    }
}

struct Args<'a>(&'a Value);

impl<'a> Args<'a> {
    fn req_str(&self, key: &str) -> Result<&'a str> {
        self.0
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| FleetError::invalid(format!("`{key}` is required and must be a string")))
    }

    fn opt_str(&self, key: &str) -> Result<Option<&'a str>> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Str(s)) => Ok(Some(s)),
            Some(_) => Err(FleetError::invalid(format!("`{key}` must be a string"))),
        }
    }

    fn opt_u64(&self, key: &str) -> Result<Option<u64>> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v.as_u64().map(Some).ok_or_else(|| {
                FleetError::invalid(format!("`{key}` must be a non-negative integer"))
            }),
        }
    }

    fn opt_u32(&self, key: &str) -> Result<Option<u32>> {
        match self.opt_u64(key)? {
            None => Ok(None),
            Some(n) => u32::try_from(n)
                .map(Some)
                .map_err(|_| FleetError::invalid(format!("`{key}` is too large"))),
        }
    }

    fn opt_bool(&self, key: &str) -> Result<Option<bool>> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(FleetError::invalid(format!(
                "`{key}` must be true or false"
            ))),
        }
    }

    fn str_list(&self, key: &str) -> Result<Vec<&'a str>> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str().ok_or_else(|| {
                        FleetError::invalid(format!("`{key}` must be a list of strings"))
                    })
                })
                .collect(),
            Some(_) => Err(FleetError::invalid(format!(
                "`{key}` must be a list of strings"
            ))),
        }
    }
}

fn parse_new_task(v: &Value) -> Result<NewTask> {
    if v.as_object().is_none() {
        return Err(FleetError::invalid("a task must be an object"));
    }
    let a = Args(v);
    Ok(NewTask {
        id: a.opt_str("id")?.map(TaskId::parse).transpose()?,
        title: a.req_str("title")?.to_string(),
        spec: a.opt_str("spec")?.unwrap_or("").to_string(),
        deps: a
            .str_list("after")?
            .into_iter()
            .map(TaskId::parse)
            .collect::<Result<Vec<_>>>()?,
        worker: a.opt_str("worker")?.map(WorkerName::parse).transpose()?,
    })
}

fn parse_document(v: &Value, key: &str) -> Result<Document> {
    match v.get(key) {
        Some(doc) if doc.as_object().is_some() => Ok(Document(doc.clone())),
        _ => Err(FleetError::invalid(format!("`{key}` must be an object"))),
    }
}

fn parse_goal_id(a: &Args<'_>, key: &str) -> Result<GoalId> {
    GoalId::parse(a.req_str(key)?)
}

fn parse_new_goal(v: &Value) -> Result<NewGoalArgs> {
    let a = Args(v);
    Ok(NewGoalArgs {
        id: a.opt_str("id")?.map(GoalId::parse).transpose()?,
        parent: a.opt_str("parent")?.map(GoalId::parse).transpose()?,
        title: a.req_str("title")?.to_string(),
        intent: a.opt_str("intent")?.unwrap_or("").to_string(),
        tasks: a
            .str_list("tasks")?
            .into_iter()
            .map(str::to_string)
            .collect(),
        contract: match v.get("contract") {
            None | Some(Value::Null) => None,
            Some(_) => Some(parse_document(v, "contract")?),
        },
    })
}

fn parse_new_run(v: &Value) -> Result<NewRunArgs> {
    if v.as_object().is_none() {
        return Err(FleetError::invalid("a run must be an object"));
    }
    let a = Args(v);
    Ok(NewRunArgs {
        id: a.opt_str("id")?.map(RunId::parse).transpose()?,
        objective: a.req_str("objective")?.to_string(),
        repo: a.req_str("repo")?.to_string(),
        base: a.opt_str("base")?.map(str::to_string),
        max_parallel: a.opt_u32("max_parallel")?,
    })
}

/// Parses a request: its command name and its arguments object.
pub fn parse_command(name: &str, args: &Value) -> Result<Command> {
    let empty = Value::Object(Default::default());
    let args = if args.is_null() { &empty } else { args };
    if args.as_object().is_none() {
        return Err(FleetError::invalid("`args` must be an object"));
    }
    let a = Args(args);
    let task = || a.req_str("task").map(str::to_string);
    Ok(match name {
        "ping" => Command::Ping,
        "info" => Command::Info,
        "status" => Command::Status {
            run: a.opt_str("run")?.map(RunId::parse).transpose()?,
        },
        "run.create" => Command::CreateRun(parse_new_run(args)?),
        "task.add" => Command::AddTask {
            run: RunId::parse(a.req_str("run")?)?,
            task: parse_new_task(args)?,
        },
        "plan.load" => {
            let tasks = match args.get("tasks") {
                Some(Value::Array(items)) => items
                    .iter()
                    .map(parse_new_task)
                    .collect::<Result<Vec<_>>>()?,
                _ => return Err(FleetError::invalid("`tasks` must be a list")),
            };
            Command::LoadPlan {
                run: a.opt_str("run")?.map(RunId::parse).transpose()?,
                new_run: args
                    .get("new_run")
                    .filter(|v| !v.is_null())
                    .map(parse_new_run)
                    .transpose()?,
                tasks,
            }
        }
        "run.start" => Command::StartRun {
            run: RunId::parse(a.req_str("run")?)?,
            max_parallel: a.opt_u32("max_parallel")?,
        },
        "task.stop" => Command::Stop { task: task()? },
        "task.retry" => Command::Retry { task: task()? },
        "task.cancel" => Command::Cancel { task: task()? },
        "task.tail" => Command::Tail {
            task: task()?,
            since: a.opt_u64("since")?,
            max: a
                .opt_u64("max")?
                .map_or(16 * 1024, |n| usize::try_from(n).unwrap_or(usize::MAX))
                .clamp(1, 256 * 1024),
        },
        "task.send" => {
            let text = a.req_str("text")?.to_string();
            if text.len() > MAX_SEND_BYTES {
                return Err(FleetError::limit(format!(
                    "input is longer than {MAX_SEND_BYTES} bytes"
                )));
            }
            if text.contains('\0') {
                return Err(FleetError::invalid("input contains a NUL byte"));
            }
            Command::Send {
                task: task()?,
                text,
                newline: a.opt_bool("newline")?.unwrap_or(true),
            }
        }
        "task.attach" => Command::Attach { task: task()? },
        "gate.list" => Command::GateList,
        "gate.resolve" => Command::ResolveGate {
            id: DecisionId::parse(a.req_str("id")?)?,
            answer: a.req_str("answer")?.to_string(),
        },
        "approval.list" => Command::ApprovalList,
        "approval.decide" => Command::Decide {
            id: ApprovalId::parse(a.req_str("id")?)?,
            approve: a
                .opt_bool("approve")?
                .ok_or_else(|| FleetError::invalid("`approve` is required"))?,
            digest: a.req_str("digest")?.to_string(),
        },
        "merge" => Command::Merge { task: task()? },
        "cleanup" => Command::Cleanup {
            run: a.opt_str("run")?.map(RunId::parse).transpose()?,
            branches: a.opt_bool("branches")?.unwrap_or(false),
        },
        "undo" => Command::Undo {
            task: task()?,
            apply: a.opt_bool("apply")?.unwrap_or(false),
        },
        "events" => Command::Events {
            limit: a
                .opt_u64("limit")?
                .map_or(50, |n| usize::try_from(n).unwrap_or(usize::MAX))
                .clamp(1, 500),
        },
        "goal.add" => Command::GoalAdd(parse_new_goal(args)?),
        "goal.attach" => Command::GoalAttach {
            goal: parse_goal_id(&a, "goal")?,
            contract: parse_document(args, "contract")?,
        },
        "goal.status" => Command::GoalSetStatus {
            goal: parse_goal_id(&a, "goal")?,
            status: GoalStatus::parse(a.req_str("status")?).ok_or_else(|| {
                FleetError::invalid(
                    "`status` must be open, in_progress, blocked, verified or abandoned",
                )
            })?,
            reason: a.opt_str("reason")?.unwrap_or("").to_string(),
        },
        "goal.list" => Command::GoalList,
        "goal.show" => Command::GoalShow {
            goal: parse_goal_id(&a, "goal")?,
        },
        "goal.remaining" => Command::GoalRemaining,
        "verify" => Command::Verify {
            target: a.req_str("target")?.to_string(),
            task: a.opt_str("task")?.map(str::to_string),
        },
        "verify.status" => Command::VerifyStatus {
            target: a.req_str("target")?.to_string(),
            task: a.opt_str("task")?.map(str::to_string),
            ticket: a.opt_u64("ticket")?,
        },
        "shutdown" => Command::Shutdown,
        other => match crate::forge::api::parse(other, args)? {
            Some(forge) => Command::Forge(forge),
            None => {
                return Err(FleetError::invalid(format!(
                    "unknown command {}",
                    fleet_domain::ids::show(other)
                )))
            }
        },
    })
}

/// Commands that change nothing and are always safe to run.
pub fn is_read_only(cmd: &Command) -> bool {
    if let Command::Forge(f) = cmd {
        return crate::forge::api::is_read_only(f);
    }
    matches!(
        cmd,
        Command::Ping
            | Command::Info
            | Command::Status { .. }
            | Command::Tail { .. }
            | Command::GateList
            | Command::ApprovalList
            | Command::Events { .. }
            | Command::GoalList
            | Command::GoalShow { .. }
            | Command::GoalRemaining
            | Command::VerifyStatus { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_domain::error::ErrorCode;

    fn obj(pairs: Vec<(&str, Value)>) -> Value {
        Value::object(pairs)
    }

    fn parse(name: &str, pairs: Vec<(&str, Value)>) -> Result<Command> {
        parse_command(name, &obj(pairs))
    }

    #[test]
    fn simple_commands_parse() {
        assert_eq!(parse("ping", vec![]).unwrap(), Command::Ping);
        assert_eq!(parse_command("ping", &Value::Null).unwrap(), Command::Ping);
        assert_eq!(parse("gate.list", vec![]).unwrap(), Command::GateList);
        assert_eq!(parse("shutdown", vec![]).unwrap(), Command::Shutdown);
        assert_eq!(
            parse("status", vec![("run", Value::str("r1"))]).unwrap(),
            Command::Status {
                run: Some(RunId::parse("r1").unwrap())
            }
        );
    }

    #[test]
    fn task_add_parses_dependencies_and_worker() {
        let cmd = parse(
            "task.add",
            vec![
                ("run", Value::str("r1")),
                ("title", Value::str("t")),
                ("spec", Value::str("s")),
                ("after", Value::array([Value::str("a"), Value::str("b")])),
                ("worker", Value::str("agent")),
            ],
        )
        .unwrap();
        let Command::AddTask { run, task } = cmd else {
            panic!("wrong command")
        };
        assert_eq!(run.as_str(), "r1");
        assert_eq!(task.deps.len(), 2);
        assert_eq!(task.worker.unwrap().as_str(), "agent");
        assert!(task.id.is_none());
    }

    #[test]
    fn plan_load_parses_tasks_and_optional_run() {
        let cmd = parse(
            "plan.load",
            vec![
                (
                    "new_run",
                    obj(vec![
                        ("objective", Value::str("o")),
                        ("repo", Value::str("/r")),
                    ]),
                ),
                (
                    "tasks",
                    Value::array([obj(vec![
                        ("id", Value::str("a")),
                        ("title", Value::str("A")),
                    ])]),
                ),
            ],
        )
        .unwrap();
        let Command::LoadPlan {
            run,
            new_run,
            tasks,
        } = cmd
        else {
            panic!("wrong command")
        };
        assert!(run.is_none());
        assert_eq!(new_run.unwrap().repo, "/r");
        assert_eq!(tasks.len(), 1);
        assert!(parse("plan.load", vec![]).is_err());
        assert!(parse("plan.load", vec![("tasks", Value::str("x"))]).is_err());
    }

    #[test]
    fn invalid_requests_are_rejected_with_invalid() {
        let cases: Vec<(&str, Vec<(&str, Value)>)> = vec![
            ("nope", vec![]),
            ("task.stop", vec![]),
            ("task.stop", vec![("task", Value::int(1))]),
            ("run.start", vec![("run", Value::str("../x"))]),
            (
                "gate.resolve",
                vec![("id", Value::str("a1")), ("answer", Value::str("x"))],
            ),
            ("approval.decide", vec![("id", Value::str("a1"))]),
            (
                "approval.decide",
                vec![("id", Value::str("a1")), ("approve", Value::Bool(true))],
            ),
            (
                "approval.decide",
                vec![
                    ("id", Value::str("a1")),
                    ("approve", Value::Bool(true)),
                    ("digest", Value::Null),
                ],
            ),
            (
                "approval.decide",
                vec![("id", Value::str("a1")), ("approve", Value::str("yes"))],
            ),
            (
                "task.tail",
                vec![("task", Value::str("t")), ("since", Value::int(-1))],
            ),
            ("run.create", vec![("objective", Value::str("o"))]),
            (
                "task.add",
                vec![
                    ("run", Value::str("r1")),
                    ("title", Value::str("t")),
                    ("after", Value::str("a")),
                ],
            ),
            (
                "task.send",
                vec![("task", Value::str("t")), ("text", Value::str("a\0b"))],
            ),
            (
                "run.start",
                vec![
                    ("run", Value::str("r1")),
                    ("max_parallel", Value::int(u64::MAX)),
                ],
            ),
        ];
        for (name, pairs) in cases {
            let err = parse(name, pairs.clone()).unwrap_err();
            assert!(
                matches!(err.code, ErrorCode::Invalid | ErrorCode::Limit),
                "{name} {pairs:?}"
            );
        }
        assert!(parse_command("ping", &Value::str("x")).is_err());
    }

    #[test]
    fn send_defaults_to_a_newline_and_is_bounded() {
        let Command::Send { newline, .. } = parse(
            "task.send",
            vec![("task", Value::str("t")), ("text", Value::str("y"))],
        )
        .unwrap() else {
            panic!("wrong command")
        };
        assert!(newline);
        let big = Value::str("x".repeat(MAX_SEND_BYTES + 1));
        assert_eq!(
            parse("task.send", vec![("task", Value::str("t")), ("text", big)])
                .unwrap_err()
                .code,
            ErrorCode::Limit
        );
    }

    #[test]
    fn tail_and_events_are_clamped() {
        let Command::Tail { max, .. } = parse(
            "task.tail",
            vec![("task", Value::str("t")), ("max", Value::int(u64::MAX))],
        )
        .unwrap() else {
            panic!("wrong command")
        };
        assert_eq!(max, 256 * 1024);
        let Command::Events { limit } = parse("events", vec![("limit", Value::int(0))]).unwrap()
        else {
            panic!("wrong command")
        };
        assert_eq!(limit, 1);
    }

    #[test]
    fn read_only_classification() {
        assert!(is_read_only(&Command::Ping));
        assert!(is_read_only(&Command::Status { run: None }));
        assert!(!is_read_only(&Command::Shutdown));
        assert!(!is_read_only(&Command::Merge { task: "t".into() }));
        assert!(!is_read_only(&Command::ResolveGate {
            id: DecisionId::from_seq(1),
            answer: "x".into()
        }));
    }

    #[test]
    fn goal_commands_parse() {
        let contract = obj(vec![("version", Value::int(1))]);
        let Command::GoalAdd(args) = parse(
            "goal.add",
            vec![
                ("id", Value::str("ship")),
                ("parent", Value::str("root")),
                ("title", Value::str("Ship")),
                ("intent", Value::str("why")),
                ("tasks", Value::array([Value::str("r1/a")])),
                ("contract", contract.clone()),
            ],
        )
        .unwrap() else {
            panic!("wrong command")
        };
        assert_eq!(args.id.unwrap().as_str(), "ship");
        assert_eq!(args.parent.unwrap().as_str(), "root");
        assert_eq!((args.title.as_str(), args.intent.as_str()), ("Ship", "why"));
        assert_eq!(args.tasks, vec!["r1/a".to_string()]);
        assert_eq!(args.contract, Some(Document(contract.clone())));
        let Command::GoalAdd(bare) = parse("goal.add", vec![("title", Value::str("T"))]).unwrap()
        else {
            panic!("wrong command")
        };
        assert_eq!((bare.id, bare.parent, bare.contract), (None, None, None));
        assert!(bare.tasks.is_empty() && bare.intent.is_empty());
        assert_eq!(
            parse(
                "goal.attach",
                vec![("goal", Value::str("g")), ("contract", contract.clone())]
            )
            .unwrap(),
            Command::GoalAttach {
                goal: GoalId::parse("g").unwrap(),
                contract: Document(contract)
            }
        );
        assert_eq!(
            parse(
                "goal.status",
                vec![
                    ("goal", Value::str("g")),
                    ("status", Value::str("blocked")),
                    ("reason", Value::str("why"))
                ]
            )
            .unwrap(),
            Command::GoalSetStatus {
                goal: GoalId::parse("g").unwrap(),
                status: GoalStatus::Blocked,
                reason: "why".into()
            }
        );
        assert_eq!(parse("goal.list", vec![]).unwrap(), Command::GoalList);
        assert_eq!(
            parse("goal.remaining", vec![]).unwrap(),
            Command::GoalRemaining
        );
        assert_eq!(
            parse("goal.show", vec![("goal", Value::str("g"))]).unwrap(),
            Command::GoalShow {
                goal: GoalId::parse("g").unwrap()
            }
        );
    }

    #[test]
    fn verify_commands_parse() {
        assert_eq!(
            parse("verify", vec![("target", Value::str("r1/a"))]).unwrap(),
            Command::Verify {
                target: "r1/a".into(),
                task: None
            }
        );
        assert_eq!(
            parse(
                "verify.status",
                vec![("target", Value::str("g")), ("task", Value::str("r1/a"))]
            )
            .unwrap(),
            Command::VerifyStatus {
                target: "g".into(),
                task: Some("r1/a".into()),
                ticket: None
            }
        );
    }

    #[test]
    fn invalid_goal_requests_are_rejected() {
        let cases: Vec<(&str, Vec<(&str, Value)>)> = vec![
            ("goal.add", vec![]),
            (
                "goal.add",
                vec![("title", Value::str("t")), ("id", Value::str("Bad Id"))],
            ),
            (
                "goal.add",
                vec![("title", Value::str("t")), ("tasks", Value::str("r1/a"))],
            ),
            (
                "goal.add",
                vec![("title", Value::str("t")), ("contract", Value::str("x"))],
            ),
            ("goal.attach", vec![("goal", Value::str("g"))]),
            (
                "goal.attach",
                vec![("goal", Value::str("g")), ("contract", Value::int(1))],
            ),
            (
                "goal.status",
                vec![("goal", Value::str("g")), ("status", Value::str("done"))],
            ),
            ("goal.status", vec![("goal", Value::str("g"))]),
            ("goal.show", vec![]),
            ("goal.show", vec![("goal", Value::str("../x"))]),
            ("verify", vec![]),
            ("verify", vec![("target", Value::int(3))]),
            ("verify.status", vec![]),
        ];
        for (name, pairs) in cases {
            let err = parse(name, pairs.clone()).unwrap_err();
            assert_eq!(err.code, ErrorCode::Invalid, "{name} {pairs:?}");
        }
    }

    #[test]
    fn goal_reads_are_read_only_and_writes_are_not() {
        for cmd in [
            Command::GoalList,
            Command::GoalRemaining,
            Command::GoalShow {
                goal: GoalId::parse("g").unwrap(),
            },
            Command::VerifyStatus {
                target: "g".into(),
                task: None,
                ticket: None,
            },
        ] {
            assert!(is_read_only(&cmd), "{cmd:?}");
        }
        for cmd in [
            Command::Verify {
                target: "g".into(),
                task: None,
            },
            Command::GoalSetStatus {
                goal: GoalId::parse("g").unwrap(),
                status: GoalStatus::Open,
                reason: String::new(),
            },
        ] {
            assert!(!is_read_only(&cmd), "{cmd:?}");
        }
    }

    #[test]
    fn reply_exposes_its_data() {
        let d = Value::str("x");
        assert_eq!(Reply::Data(d.clone()).data(), &d);
        assert_eq!(
            Reply::Attach {
                session: SessionId(1),
                data: d.clone()
            }
            .data(),
            &d
        );
    }
}
