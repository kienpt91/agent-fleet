//! Read models: the documents `status`, `gate.list` and `approval.list` return.
//!
//! The shapes are a public, versioned interface (`fleet.status/1` and friends); field names are
//! stable. Everything here only reads domain state.

use fleet_domain::approval::Approval;
use fleet_domain::decision::Decision;
use fleet_domain::fleet::FleetState;
use fleet_domain::ids::{RunId, TaskId};
use fleet_domain::run::{Run, Task};
use fleet_domain::status::TaskStatus;
use fleet_domain::value::Value;
use std::collections::{BTreeSet, VecDeque};

/// One line of the activity feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recent {
    pub seq: u64,
    pub ts_ms: u64,
    pub line: String,
}

fn opt_str(s: Option<&str>) -> Value {
    s.map_or(Value::Null, Value::str)
}

pub fn run_phase(run: &Run) -> &'static str {
    if run.is_finished() {
        "finished"
    } else if run.started {
        "active"
    } else {
        "created"
    }
}

fn task_value(t: &Task, live: bool) -> Value {
    Value::object([
        ("id", Value::str(t.id.as_str())),
        ("title", Value::str(&t.title)),
        ("status", Value::str(t.status.as_str())),
        (
            "deps",
            Value::array(t.deps.iter().map(|d| Value::str(d.as_str()))),
        ),
        ("worker", opt_str(t.worker.as_ref().map(|w| w.as_str()))),
        ("host", opt_str(t.host.as_ref().map(|h| h.as_str()))),
        ("attempt", Value::int(t.attempt)),
        ("retries_used", Value::int(t.retries_used)),
        ("branch", Value::str(&t.branch)),
        ("worktree", opt_str(t.worktree.as_deref())),
        ("head", opt_str(t.head.as_ref().map(|h| h.as_str()))),
        ("merged", opt_str(t.merged.as_ref().map(|h| h.as_str()))),
        ("summary", opt_str(t.summary.as_deref())),
        ("failure", opt_str(t.failure.as_deref())),
        (
            "block",
            match &t.block {
                None => Value::Null,
                Some(b) => Value::object([
                    ("kind", Value::str(b.kind.as_str())),
                    ("subject", Value::str(&b.subject)),
                    ("detail", Value::str(&b.detail)),
                ]),
            },
        ),
        (
            "gates",
            Value::array(t.gates.iter().map(|g| {
                Value::object([
                    ("name", Value::str(&g.name)),
                    ("passed", Value::Bool(g.passed)),
                    ("summary", Value::str(&g.summary)),
                ])
            })),
        ),
        ("live", Value::Bool(live)),
    ])
}

fn run_value(run: &Run, live: &BTreeSet<(RunId, TaskId)>) -> Value {
    let count =
        |s: TaskStatus| Value::int(run.tasks().iter().filter(|t| t.status == s).count() as u64);
    Value::object([
        ("id", Value::str(run.id.as_str())),
        ("objective", Value::str(&run.objective)),
        ("repo", Value::str(&run.repo)),
        ("base_ref", Value::str(&run.base_ref)),
        ("base_sha", Value::str(run.base_sha.as_str())),
        ("status", Value::str(run_phase(run))),
        ("max_parallel", Value::int(run.max_parallel)),
        ("context", Value::str(run.context.as_str())),
        ("integration_branch", Value::str(run.integration_branch())),
        (
            "counts",
            Value::object(TaskStatus::ALL.iter().map(|s| (s.as_str(), count(*s)))),
        ),
        (
            "tasks",
            Value::array(
                run.tasks()
                    .iter()
                    .map(|t| task_value(t, live.contains(&(run.id.clone(), t.id.clone())))),
            ),
        ),
    ])
}

pub fn gate_value(d: &Decision) -> Value {
    Value::object([
        ("id", Value::str(d.id.as_str())),
        ("run", Value::str(d.run.as_str())),
        ("task", Value::str(d.task.as_str())),
        ("kind", Value::str(d.kind.as_str())),
        ("text", Value::str(&d.text)),
        ("choices", Value::array(d.choices.iter().map(Value::str))),
        ("answer", opt_str(d.answer.as_deref())),
        ("open", Value::Bool(d.is_open())),
    ])
}

pub fn approval_value(a: &Approval, now_ms: u64) -> Value {
    Value::object([
        ("id", Value::str(a.id.as_str())),
        ("run", Value::str(a.run.as_str())),
        ("task", Value::str(a.task.as_str())),
        ("purpose", Value::str(a.purpose.as_str())),
        ("tool", Value::str(&a.action.tool)),
        ("command", Value::str(&a.action.command)),
        ("args", Value::array(a.action.args.iter().map(Value::str))),
        ("display", Value::str(a.action.display())),
        ("digest", Value::str(&a.digest)),
        ("status", Value::str(a.status_at(now_ms).as_str())),
        ("requested_ms", Value::int(a.requested_ms)),
        ("expires_ms", Value::int(a.expires_ms)),
    ])
}

pub fn recent_value(recent: &VecDeque<Recent>, limit: usize) -> Value {
    let skip = recent.len().saturating_sub(limit);
    Value::array(recent.iter().skip(skip).map(|r| {
        Value::object([
            ("seq", Value::int(r.seq)),
            ("ts_ms", Value::int(r.ts_ms)),
            ("line", Value::str(&r.line)),
        ])
    }))
}

/// Inputs of the status document that do not come from domain state.
pub struct StatusContext<'a> {
    pub now_ms: u64,
    pub live: &'a BTreeSet<(RunId, TaskId)>,
    pub recent: &'a VecDeque<Recent>,
    pub pid: u64,
    pub version: &'a str,
    pub uptime_ms: u64,
    pub max_sessions: usize,
    pub undo_available: bool,
}

pub fn status_value(state: &FleetState, only: Option<&RunId>, ctx: &StatusContext<'_>) -> Value {
    let runs = state
        .runs()
        .filter(|r| only.is_none_or(|o| &r.id == o))
        .map(|r| run_value(r, ctx.live));
    let gates = state
        .decisions()
        .filter(|d| d.is_open() && only.is_none_or(|o| &d.run == o))
        .map(gate_value);
    let approvals = state
        .approvals()
        .filter(|a| only.is_none_or(|o| &a.run == o))
        .map(|a| approval_value(a, ctx.now_ms));
    Value::object([
        ("schema", Value::str("fleet.status/1")),
        (
            "daemon",
            Value::object([
                ("pid", Value::int(ctx.pid)),
                ("version", Value::str(ctx.version)),
                ("uptime_ms", Value::int(ctx.uptime_ms)),
                ("sessions", Value::int(ctx.live.len() as u64)),
                ("max_sessions", Value::int(ctx.max_sessions as u64)),
                (
                    "undo",
                    Value::str(if ctx.undo_available {
                        "available"
                    } else {
                        "unavailable"
                    }),
                ),
                ("rss_kb", Value::Null),
                ("output_bytes", Value::int(0u64)),
            ]),
        ),
        ("now_ms", Value::int(ctx.now_ms)),
        ("runs", Value::array(runs)),
        ("gates", Value::array(gates)),
        ("approvals", Value::array(approvals)),
        ("recent", recent_value(ctx.recent, 20)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_domain::events::DomainEvent;
    use fleet_domain::ids::Sha;
    use fleet_domain::run::NewTask;

    fn sha() -> Sha {
        Sha::parse(&"a".repeat(40)).unwrap()
    }

    fn state() -> FleetState {
        let mut s = FleetState::new();
        let (run, ev) = s
            .create_run(fleet_domain::fleet::NewRun {
                id: None,
                objective: "obj".into(),
                repo: "/r".into(),
                base_ref: "main".into(),
                base_sha: sha(),
                max_parallel: 2,
                context: fleet_domain::status::ContextMode::Off,
            })
            .unwrap();
        s.apply(&ev);
        let (_, ev) = s
            .add_tasks(
                &run,
                vec![NewTask {
                    id: None,
                    title: "first".into(),
                    spec: String::new(),
                    deps: vec![],
                    worker: None,
                    host: None,
                }],
            )
            .unwrap();
        for e in &ev {
            s.apply(e);
        }
        s
    }

    fn ctx<'a>(
        live: &'a BTreeSet<(RunId, TaskId)>,
        recent: &'a VecDeque<Recent>,
    ) -> StatusContext<'a> {
        StatusContext {
            now_ms: 5,
            live,
            recent,
            pid: 1,
            version: "0.1.0",
            uptime_ms: 7,
            max_sessions: 4,
            undo_available: false,
        }
    }

    #[test]
    fn status_has_the_documented_top_level_keys() {
        let s = state();
        let live = BTreeSet::new();
        let recent = VecDeque::new();
        let v = status_value(&s, None, &ctx(&live, &recent));
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "approvals",
                "daemon",
                "gates",
                "now_ms",
                "recent",
                "runs",
                "schema"
            ]
        );
        assert_eq!(
            v.get("schema").and_then(Value::as_str),
            Some("fleet.status/1")
        );
        let daemon = v.get("daemon").unwrap();
        assert_eq!(
            daemon.get("undo").and_then(Value::as_str),
            Some("unavailable")
        );
        assert!(daemon.get("rss_kb").unwrap().is_null());
    }

    #[test]
    fn run_and_task_documents_have_stable_fields() {
        let s = state();
        let live: BTreeSet<_> = [(RunId::parse("r1").unwrap(), TaskId::parse("t1").unwrap())]
            .into_iter()
            .collect();
        let recent = VecDeque::new();
        let v = status_value(&s, None, &ctx(&live, &recent));
        let run = &v.get("runs").and_then(Value::as_array).unwrap()[0];
        let run_keys: Vec<&str> = run
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            run_keys,
            vec![
                "base_ref",
                "base_sha",
                "context",
                "counts",
                "id",
                "integration_branch",
                "max_parallel",
                "objective",
                "repo",
                "status",
                "tasks"
            ]
        );
        assert_eq!(run.get("status").and_then(Value::as_str), Some("created"));
        let counts = run.get("counts").unwrap().as_object().unwrap();
        assert_eq!(counts.len(), TaskStatus::ALL.len());
        let task = &run.get("tasks").and_then(Value::as_array).unwrap()[0];
        let task_keys: Vec<&str> = task
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            task_keys,
            vec![
                "attempt",
                "block",
                "branch",
                "deps",
                "failure",
                "gates",
                "head",
                "host",
                "id",
                "live",
                "merged",
                "retries_used",
                "status",
                "summary",
                "title",
                "worker",
                "worktree"
            ]
        );
        assert_eq!(task.get("live").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn filtering_by_run_hides_other_runs_gates_and_approvals() {
        let mut s = state();
        let (_, ev) = s
            .create_run(fleet_domain::fleet::NewRun {
                id: None,
                objective: "second".into(),
                repo: "/r".into(),
                base_ref: "main".into(),
                base_sha: sha(),
                max_parallel: 1,
                context: fleet_domain::status::ContextMode::Off,
            })
            .unwrap();
        s.apply(&ev);
        let live = BTreeSet::new();
        let recent = VecDeque::new();
        let only = RunId::parse("r2").unwrap();
        let v = status_value(&s, Some(&only), &ctx(&live, &recent));
        assert_eq!(v.get("runs").and_then(Value::as_array).unwrap().len(), 1);
    }

    #[test]
    fn recent_is_limited_to_the_newest_entries() {
        let recent: VecDeque<Recent> = (1..=30)
            .map(|i| Recent {
                seq: i,
                ts_ms: i,
                line: format!("e{i}"),
            })
            .collect();
        let v = recent_value(&recent, 5);
        let items = v.as_array().unwrap();
        assert_eq!(items.len(), 5);
        assert_eq!(items[0].get("seq").and_then(Value::as_u64), Some(26));
    }

    #[test]
    fn phase_reflects_start_and_completion() {
        let mut s = state();
        let run_id = RunId::parse("r1").unwrap();
        assert_eq!(run_phase(s.run(&run_id).unwrap()), "created");
        s.apply(&DomainEvent::RunStarted {
            run: run_id.clone(),
            max_parallel: 2,
        });
        assert_eq!(run_phase(s.run(&run_id).unwrap()), "active");
        s.apply(&DomainEvent::TaskCompleted {
            run: run_id.clone(),
            task: TaskId::parse("t1").unwrap(),
        });
        assert_eq!(run_phase(s.run(&run_id).unwrap()), "finished");
    }
}
