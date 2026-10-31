//! Read models of groups and comparisons.
//!
//! Everything a member can influence (titles, the summary a worker wrote, file names from its
//! branch, check ids and summaries from a contract) is flattened to one bounded line here, so no
//! view, whatever shows it, receives a control sequence, a line break or a heading from it. Ids,
//! hashes and the fixed vocabulary are fleet's own.

use fleet_domain::fleet::FleetState;
use fleet_domain::sanitize::flatten_line;
use fleet_domain::value::Value;
use fleet_fanout::compare::{Comparison, EvidenceStatus, MemberReport, Overlap};
use fleet_fanout::diff::{DiffStat, ScopeReport, ScopeState};
use fleet_fanout::group::{Group, MemberState};
use fleet_fanout::rank::{Criterion, RankRule};

/// File names listed per member in a comparison; the totals count every file.
pub(crate) const MAX_CHANGES_SHOWN: usize = 100;
const LINE: usize = 300;
const SUMMARY: usize = 500;

fn flat(s: &str) -> Value {
    Value::str(flatten_line(s, LINE))
}

fn opt_flat(s: Option<&str>) -> Value {
    s.map_or(Value::Null, flat)
}

fn opt_text(s: Option<&str>, max: usize) -> Value {
    s.map_or(Value::Null, |s| Value::str(flatten_line(s, max)))
}

fn opt_ms(n: Option<u64>) -> Value {
    n.map_or(Value::Null, Value::int)
}

fn counts(g: &Group) -> Value {
    let n = |state: MemberState| {
        Value::int(g.members.iter().filter(|m| m.state == state).count() as u64)
    };
    Value::object([
        ("members", Value::int(g.members.len() as u64)),
        ("running", n(MemberState::Running)),
        ("completed", n(MemberState::Completed)),
        ("failed", n(MemberState::Failed)),
        ("winner", n(MemberState::Winner)),
        ("superseded", n(MemberState::Superseded)),
    ])
}

fn group_head(g: &Group) -> Vec<(&'static str, Value)> {
    vec![
        ("id", Value::str(g.id.as_str())),
        ("run", Value::str(g.run.as_str())),
        ("title", flat(&g.title)),
        ("status", Value::str(g.status().as_str())),
        (
            "winner",
            g.winner
                .as_ref()
                .map_or(Value::Null, |w| Value::str(w.as_str())),
        ),
        (
            "merge_sha",
            g.merge_sha
                .as_ref()
                .map_or(Value::Null, |s| Value::str(s.as_str())),
        ),
        ("winner_verification", opt_flat(g.verification.as_deref())),
        (
            "closed",
            g.closed.map_or(Value::Null, |c| Value::str(c.as_str())),
        ),
        (
            "goal",
            g.goal
                .as_ref()
                .map_or(Value::Null, |x| Value::str(x.as_str())),
        ),
        ("contract_id", opt_flat(g.contract_id.as_deref())),
        ("created_ms", Value::int(g.created_ms)),
        ("counts", counts(g)),
    ]
}

pub(crate) fn group_list(groups: &[&Group]) -> Value {
    Value::object([
        ("schema", Value::str("fleet.group.list/1")),
        (
            "groups",
            Value::array(groups.iter().map(|g| Value::object(group_head(g)))),
        ),
    ])
}

pub(crate) fn group_show(g: &Group, state: &FleetState) -> Value {
    let mut fields = group_head(g);
    fields.push((
        "members",
        Value::array(g.members.iter().map(|m| {
            let task = state.run(&g.run).ok().and_then(|r| r.task(&m.task));
            Value::object([
                ("task", Value::str(m.task.as_str())),
                ("state", Value::str(m.state.as_str())),
                (
                    "outcome",
                    m.outcome.map_or(Value::Null, |o| Value::str(o.as_str())),
                ),
                (
                    "task_status",
                    task.map_or(Value::Null, |t| Value::str(t.status.as_str())),
                ),
                (
                    "worker",
                    task.and_then(|t| t.worker.as_ref())
                        .map_or(Value::Null, |w| Value::str(w.as_str())),
                ),
                ("title", task.map_or(Value::Null, |t| flat(&t.title))),
                (
                    "attempts",
                    task.map_or(Value::Null, |t| Value::int(t.attempt)),
                ),
                ("duration_ms", opt_ms(m.duration_ms())),
                (
                    "merged",
                    task.and_then(|t| t.merged.as_ref())
                        .map_or(Value::Null, |s| Value::str(s.as_str())),
                ),
            ])
        })),
    ));
    let mut doc = vec![("schema", Value::str("fleet.group.show/1"))];
    doc.push(("group", Value::object(fields)));
    Value::object(doc)
}

fn diff_value(diff: Option<&DiffStat>, error: Option<&str>) -> Value {
    let Some(d) = diff else {
        return Value::object([
            ("available", Value::Bool(false)),
            ("error", opt_text(error, SUMMARY)),
        ]);
    };
    Value::object([
        ("available", Value::Bool(true)),
        ("error", Value::Null),
        ("files", Value::int(d.files)),
        ("insertions", Value::int(d.insertions)),
        ("deletions", Value::int(d.deletions)),
        ("size", Value::int(d.size())),
        ("binary_files", Value::int(d.binary_files)),
        ("truncated", Value::Bool(d.truncated)),
        (
            "listed",
            Value::int(d.changes.len().min(MAX_CHANGES_SHOWN) as u64),
        ),
        (
            "changes",
            Value::array(d.changes.iter().take(MAX_CHANGES_SHOWN).map(|c| {
                Value::object([
                    ("path", flat(&c.path)),
                    ("old_path", opt_flat(c.old_path.as_deref())),
                    ("kind", Value::str(c.kind.as_str())),
                    ("insertions", Value::int(c.insertions)),
                    ("deletions", Value::int(c.deletions)),
                    ("binary", Value::Bool(c.binary)),
                ])
            })),
        ),
    ])
}

fn scope_value(s: &ScopeReport) -> Value {
    Value::object([
        ("check", flat(&s.check)),
        ("mandatory", Value::Bool(s.mandatory)),
        (
            "allowed_touched",
            Value::array(s.allowed_touched.iter().map(|p| flat(p))),
        ),
        ("outside", Value::array(s.outside.iter().map(|p| flat(p)))),
        ("outside_count", Value::int(s.outside_count)),
    ])
}

fn member_value(m: &MemberReport) -> Value {
    let f = &m.facts;
    Value::object([
        ("rank", Value::int(m.rank.position as u64)),
        ("decided_by", Value::str(m.rank.decided_by.as_str())),
        ("task", Value::str(f.task.as_str())),
        ("worker", opt_flat(f.worker.as_deref())),
        ("state", Value::str(f.state.as_str())),
        (
            "outcome",
            f.outcome.map_or(Value::Null, |o| Value::str(o.as_str())),
        ),
        ("task_status", Value::str(&f.task_status)),
        ("attempts", Value::int(f.attempts)),
        ("duration_ms", opt_ms(f.duration_ms)),
        (
            "head",
            f.head
                .as_ref()
                .map_or(Value::Null, |s| Value::str(s.as_str())),
        ),
        (
            "tree",
            f.tree
                .as_ref()
                .map_or(Value::Null, |s| Value::str(s.as_str())),
        ),
        (
            "branch_head",
            f.branch_head
                .as_ref()
                .map_or(Value::Null, |s| Value::str(s.as_str())),
        ),
        ("branch_moved", Value::Bool(m.branch_moved)),
        ("diff", diff_value(f.diff.as_ref(), f.diff_error.as_deref())),
        (
            "gates",
            Value::array(f.gates.iter().map(|g| {
                Value::object([("name", flat(&g.name)), ("passed", Value::Bool(g.passed))])
            })),
        ),
        (
            "gates_passed",
            Value::Bool(f.gates.iter().all(|g| g.passed)),
        ),
        (
            "verification",
            Value::object([
                ("status", Value::str(m.verification.as_str())),
                ("reason", flat(&m.verification_reason)),
                ("has_contract", Value::Bool(f.has_contract)),
                (
                    "checks",
                    Value::array(f.checks.iter().map(|c| {
                        Value::object([
                            ("goal", flat(&c.goal)),
                            ("check", flat(&c.check)),
                            ("kind", Value::str(&c.kind)),
                            ("mandatory", Value::Bool(c.mandatory)),
                            ("status", Value::str(c.status.as_str())),
                            ("summary", opt_text(Some(&c.summary), SUMMARY)),
                        ])
                    })),
                ),
            ]),
        ),
        (
            "scope",
            Value::object([
                ("in_scope", Value::Bool(m.scope == ScopeState::In)),
                ("state", Value::str(m.scope.as_str())),
                ("unknown", Value::Bool(m.scope == ScopeState::Unknown)),
                ("reason", flat(&f.scope_reason())),
                ("checks", Value::array(f.scopes.iter().map(scope_value))),
            ]),
        ),
        // what the worker wrote about its own work: shown, never used for anything above
        ("summary", opt_text(f.summary.as_deref(), SUMMARY)),
        ("failure", opt_text(f.failure.as_deref(), SUMMARY)),
    ])
}

fn overlap_value(o: &Overlap) -> Value {
    Value::object([
        (
            "tasks",
            Value::array(o.tasks.iter().map(|t| Value::str(t.as_str()))),
        ),
        (
            "matrix",
            Value::array(
                o.matrix
                    .iter()
                    .map(|row| Value::array(row.iter().map(|n| Value::int(*n)))),
            ),
        ),
        (
            "shared",
            Value::array(o.shared.iter().map(|s| {
                Value::object([
                    ("path", flat(&s.path)),
                    (
                        "members",
                        Value::array(s.members.iter().map(|t| Value::str(t.as_str()))),
                    ),
                ])
            })),
        ),
        ("shared_count", Value::int(o.shared_count)),
        ("incomplete", Value::Bool(o.incomplete)),
    ])
}

fn rule_value(rule: &RankRule) -> Value {
    Value::object([
        (
            "criteria",
            Value::array(rule.criteria().iter().map(|c| Value::str(c.as_str()))),
        ),
        (
            "described",
            Value::array(rule.criteria().iter().map(|c| {
                Value::object([
                    ("criterion", Value::str(c.as_str())),
                    ("rule", Value::str(c.describe())),
                ])
            })),
        ),
        ("formula", Value::str(rule.formula())),
        (
            "known",
            Value::array(Criterion::ALL.iter().map(|c| Value::str(c.as_str()))),
        ),
        ("advisory", Value::Bool(true)),
    ])
}

/// The fields of a `fleet.group.compare/1` document, after the schema.
pub(crate) fn comparison_fields(g: &Group, c: &Comparison) -> Vec<(&'static str, Value)> {
    let verified = c
        .members
        .iter()
        .filter(|m| m.verification == EvidenceStatus::Verified && !m.branch_moved)
        .count() as u64;
    vec![
        ("group", Value::object(group_head(g))),
        ("ranking", rule_value(&c.rule)),
        ("members", Value::array(c.members.iter().map(member_value))),
        ("overlap", overlap_value(&c.overlap)),
        ("verified_members", Value::int(verified)),
        (
            "note",
            Value::str(
                "everything above is computed from facts the daemon knows (task state, its own verification runs, the repository); the ranking is advisory: fleet pick is the only way a member is merged",
            ),
        ),
    ]
}
