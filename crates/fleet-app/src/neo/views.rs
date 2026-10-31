//! Read models of the goal ledger: the documents `goal.list`, `goal.show`, `goal.remaining` and
//! `verify.status` return. Their shapes are versioned (`fleet.goals.*/1`, `fleet.verify/1`).
//!
//! Titles, intents and evidence text can originate from an operator's file or from a command's
//! output, so consumers must treat every string as untrusted (the CLI escapes it); the sizes are
//! bounded here.

use fleet_domain::fleet::FleetState;
use fleet_domain::ids::GoalId;
use fleet_domain::sanitize::flatten_line;
use fleet_domain::status::{GoalStatus, TaskStatus};
use fleet_domain::value::Value;
use neo_goals::goal::{Goal, TaskRef};
use neo_goals::ledger::{Ledger, TaskContract};
use neo_verify::contract::{Check, CheckKind, FileAssertion};
use neo_verify::evidence::Evidence;
use neo_verify::glob::Glob;
use std::collections::BTreeSet;

/// Pieces of evidence listed in `goal.show` (newest first).
const HISTORY_SHOWN: usize = 20;
const DETAIL_WIDTH: usize = 200;

fn opt_str(s: Option<&str>) -> Value {
    s.map_or(Value::Null, Value::str)
}

/// How a task stands with regard to the contracts that guard it:
/// `none` (no goal with a contract names it), `verified` (every contract passed by fleet's own
/// run), `failed`, `unverified` (the worker finished, or the task completed, and fleet has no
/// passing evidence) or `pending` (the worker is not done yet).
pub fn task_verification(state: &FleetState, ledger: &Ledger, task: &TaskRef) -> &'static str {
    match ledger.task_contract(&task.run, &task.task) {
        TaskContract::NotGoverned => "none",
        TaskContract::Passed => "verified",
        TaskContract::Failed => "failed",
        TaskContract::Pending => {
            let status = state
                .run(&task.run)
                .ok()
                .and_then(|r| r.task(&task.task))
                .map(|t| t.status);
            match status {
                Some(TaskStatus::Review | TaskStatus::Completed) => "unverified",
                _ => "pending",
            }
        }
    }
}

fn task_status(state: &FleetState, task: &TaskRef) -> &'static str {
    state
        .run(&task.run)
        .ok()
        .and_then(|r| r.task(&task.task))
        .map_or("unknown", |t| t.status.as_str())
}

fn task_value(state: &FleetState, ledger: &Ledger, task: &TaskRef) -> Value {
    Value::object([
        ("task", Value::str(task.to_string())),
        ("status", Value::str(task_status(state, task))),
        (
            "verification",
            Value::str(task_verification(state, ledger, task)),
        ),
    ])
}

fn contract_state_name(ledger: &Ledger, goal: &Goal) -> &'static str {
    ledger
        .contract_state(&goal.id)
        .map_or("none", |s| s.as_str())
}

/// One goal as a row of a list.
pub fn goal_summary(state: &FleetState, ledger: &Ledger, g: &Goal) -> Value {
    Value::object([
        ("id", Value::str(g.id.as_str())),
        ("parent", opt_str(g.parent.as_ref().map(GoalId::as_str))),
        ("title", Value::str(&g.title)),
        ("status", Value::str(g.status.as_str())),
        ("reason", Value::str(&g.reason)),
        ("depth", Value::int(g.depth as u64)),
        ("children", Value::int(g.children.len() as u64)),
        ("contract", Value::str(contract_state_name(ledger, g))),
        (
            "contract_id",
            opt_str(g.contract.as_ref().map(|c| c.id.as_str())),
        ),
        ("evidence", Value::int(g.evidence.len() as u64)),
        (
            "tasks",
            Value::array(g.tasks.iter().map(|t| task_value(state, ledger, t))),
        ),
    ])
}

fn count(ledger: &Ledger, status: GoalStatus) -> u64 {
    ledger.goals().filter(|g| g.status == status).count() as u64
}

pub fn goal_list(state: &FleetState, ledger: &Ledger) -> Value {
    Value::object([
        ("schema", Value::str("fleet.goals.list/1")),
        (
            "goals",
            Value::array(ledger.goals().map(|g| goal_summary(state, ledger, g))),
        ),
        ("total", Value::int(ledger.len() as u64)),
        ("verified", Value::int(count(ledger, GoalStatus::Verified))),
        (
            "abandoned",
            Value::int(count(ledger, GoalStatus::Abandoned)),
        ),
    ])
}

fn check_detail(c: &Check) -> String {
    let text = match &c.kind {
        CheckKind::Command {
            program,
            args,
            expect_exit,
            ..
        } => format!("{program} {} (expect exit {expect_exit})", args.join(" ")),
        CheckKind::RefcheckJson { program, args, .. } => {
            format!("{program} {} (refcheck-json)", args.join(" "))
        }
        CheckKind::File { path, assertion } => match assertion {
            FileAssertion::Exists => format!("{path} exists"),
            FileAssertion::Sha256(d) => format!("{path} has sha256 {d}"),
            FileAssertion::Contains(t) => format!("{path} contains {t:?}"),
        },
        CheckKind::DiffOnly { allow, deny } => {
            let list = |globs: &[Glob]| {
                globs
                    .iter()
                    .map(|g| g.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            match (allow.is_empty(), deny.is_empty()) {
                (_, true) => format!("changes only {}", list(allow)),
                (true, false) => format!("changes nothing matching {}", list(deny)),
                (false, false) => {
                    format!("changes only {}, never {}", list(allow), list(deny))
                }
            }
        }
    };
    flatten_line(&text, DETAIL_WIDTH)
}

/// One piece of evidence. `counts` says whether it counts toward its contract right now: it
/// does not when the worktree had uncommitted changes behind it or when its commit is not the one
/// the task's last review recorded.
fn evidence_value(e: &Evidence, counts: bool) -> Value {
    Value::object([
        ("id", Value::str(e.id().as_str())),
        ("check", Value::str(e.check().as_str())),
        ("verdict", Value::str(e.verdict().as_str())),
        ("exit_code", e.exit_code().map_or(Value::Null, Value::int)),
        ("summary", Value::str(e.summary())),
        ("output", Value::str(e.output())),
        ("commit", Value::str(e.commit().as_str())),
        ("tree", Value::str(e.tree().as_str())),
        ("dirty", Value::Bool(e.dirty())),
        ("counts", Value::Bool(counts)),
        ("task", Value::str(e.task().as_str())),
        ("at_ms", Value::int(e.at_ms())),
    ])
}

/// The newest evidence of every check of the goal's contract (`null` for a check that has not
/// been run), in contract order.
fn checks_value(ledger: &Ledger, g: &Goal) -> Value {
    let Some(attached) = &g.contract else {
        return Value::array([]);
    };
    let evidence = ledger.evidence_of(&g.id);
    Value::array(attached.contract.checks().iter().map(|c| {
        let newest = evidence
            .iter()
            .filter(|e| e.contract() == &attached.id && e.check() == &c.id)
            .max_by_key(|e| e.id().seq());
        Value::object([
            ("check", Value::str(c.id.as_str())),
            ("kind", Value::str(c.kind.name())),
            ("mandatory", Value::Bool(c.mandatory)),
            ("detail", Value::str(check_detail(c))),
            (
                "latest",
                newest.map_or(Value::Null, |e| evidence_value(e, ledger.counts(e))),
            ),
        ])
    }))
}

pub fn goal_show(state: &FleetState, ledger: &Ledger, g: &Goal) -> Value {
    let history: Vec<Value> = ledger
        .evidence_of(&g.id)
        .iter()
        .rev()
        .take(HISTORY_SHOWN)
        .map(|e| evidence_value(e, ledger.counts(e)))
        .collect();
    let Value::Object(mut map) = goal_summary(state, ledger, g) else {
        return Value::Null;
    };
    map.insert("schema".into(), Value::str("fleet.goals.show/1"));
    map.insert("intent".into(), Value::str(&g.intent));
    map.insert(
        "child_ids".into(),
        Value::array(g.children.iter().map(|c| Value::str(c.as_str()))),
    );
    map.insert("checks".into(), checks_value(ledger, g));
    map.insert("history".into(), Value::array(history));
    Value::Object(map)
}

/// What is left: open leaves, blockers and the tasks that can start next.
pub fn remaining(state: &FleetState, ledger: &Ledger) -> Value {
    let rem = ledger.remaining();
    let mut blockers: Vec<Value> = rem
        .blocked
        .iter()
        .map(|g| {
            Value::object([
                ("kind", Value::str("goal")),
                ("goal", Value::str(g.id.as_str())),
                ("title", Value::str(&g.title)),
                ("detail", Value::str(&g.reason)),
            ])
        })
        .collect();
    let mut next_ready: Vec<Value> = Vec::new();
    let mut seen_ready: BTreeSet<TaskRef> = BTreeSet::new();
    let mut seen_blocked: BTreeSet<TaskRef> = BTreeSet::new();
    let unfinished = ledger.goals().filter(|g| {
        matches!(
            g.status,
            GoalStatus::Open | GoalStatus::InProgress | GoalStatus::Blocked
        )
    });
    for g in unfinished {
        for t in &g.tasks {
            let Some(task) = state.run(&t.run).ok().and_then(|r| r.task(&t.task)) else {
                continue;
            };
            match task.status {
                TaskStatus::Ready if seen_ready.insert(t.clone()) => {
                    next_ready.push(Value::object([
                        ("task", Value::str(t.to_string())),
                        ("goal", Value::str(g.id.as_str())),
                        ("title", Value::str(&task.title)),
                    ]))
                }
                TaskStatus::Blocked | TaskStatus::Failed if seen_blocked.insert(t.clone()) => {
                    let detail = match (&task.block, &task.failure) {
                        (Some(b), _) => format!("{}: {}", b.kind, b.detail),
                        (None, Some(f)) => f.clone(),
                        (None, None) => String::new(),
                    };
                    blockers.push(Value::object([
                        ("kind", Value::str("task")),
                        ("task", Value::str(t.to_string())),
                        ("goal", Value::str(g.id.as_str())),
                        ("status", Value::str(task.status.as_str())),
                        ("detail", Value::str(flatten_line(&detail, DETAIL_WIDTH))),
                    ]));
                }
                _ => {}
            }
        }
    }
    Value::object([
        ("schema", Value::str("fleet.goals.remaining/1")),
        ("total", Value::int(ledger.len() as u64)),
        ("verified", Value::int(count(ledger, GoalStatus::Verified))),
        (
            "abandoned",
            Value::int(count(ledger, GoalStatus::Abandoned)),
        ),
        (
            "open_leaves",
            Value::array(
                rem.open_leaves
                    .iter()
                    .map(|g| goal_summary(state, ledger, g)),
            ),
        ),
        ("blockers", Value::array(blockers)),
        ("next_ready", Value::array(next_ready)),
    ])
}

/// The state of a `fleet verify`: whether the contracts are still running and where each of the
/// goals' contracts stands, with the newest evidence per check.
/// What `verify_status` shows.
pub(crate) struct VerifyView<'a> {
    pub(crate) target: &'a str,
    pub(crate) task: Option<&'a TaskRef>,
    pub(crate) running: bool,
    /// The run the person asked about, when they named one.
    pub(crate) ticket: Option<u64>,
    /// How that run ended, once it has.
    pub(crate) outcome: Option<&'a crate::neo::VerifyOutcome>,
    pub(crate) goals: &'a [GoalId],
}

pub(crate) fn verify_status(state: &FleetState, ledger: &Ledger, v: VerifyView<'_>) -> Value {
    let rows: Vec<Value> = v
        .goals
        .iter()
        .filter_map(|id| ledger.find(id))
        .map(|g| {
            Value::object([
                ("goal", Value::str(g.id.as_str())),
                ("title", Value::str(&g.title)),
                ("status", Value::str(g.status.as_str())),
                ("contract", Value::str(contract_state_name(ledger, g))),
                ("checks", checks_value(ledger, g)),
            ])
        })
        .collect();
    // With a ticket, `passed` is the verdict on that run; without one, the ledger's standing.
    let passed = match (v.ticket, v.outcome) {
        (Some(_), Some(o)) => o.passed,
        (Some(_), None) => false,
        (None, _) => {
            !v.goals.is_empty()
                && v.goals.iter().all(|id| {
                    ledger.contract_state(id) == Some(neo_verify::evidence::ContractState::Passed)
                })
        }
    };
    Value::object([
        ("schema", Value::str("fleet.verify/1")),
        ("target", Value::str(v.target)),
        ("task", opt_str(v.task.map(|t| t.to_string()).as_deref())),
        (
            "verification",
            opt_str(v.task.map(|t| task_verification(state, ledger, t))),
        ),
        ("ticket", v.ticket.map_or(Value::Null, Value::int)),
        ("running", Value::Bool(v.running)),
        ("passed", Value::Bool(passed)),
        (
            "recorded",
            v.outcome.map_or(Value::Null, |o| Value::Bool(o.recorded)),
        ),
        (
            "note",
            v.outcome
                .filter(|o| !o.note.is_empty())
                .map_or(Value::Null, |o| Value::str(&o.note)),
        ),
        ("goals", Value::array(rows)),
    ])
}
