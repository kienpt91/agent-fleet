//! Human output of the goal ledger and of `fleet verify`. Pure functions from daemon replies to
//! text, with the same rule as `render`: every string in a reply may come from an operator's file
//! or from the output of a command a worker influenced, so every string passes through `clean`,
//! `multiline` or `quote` before it is printed.

use crate::render::{clean, items, num, prefix, table, text, text_or_dash, truncate};
use fleet_domain::sanitize::quote_lines;
use fleet_domain::value::Value;
use std::fmt::Write as _;

const TITLE_WIDTH: usize = 60;
const TASKS_WIDTH: usize = 48;
const DETAIL_WIDTH: usize = 100;
const OUTPUT_LINES: usize = 12;

/// Output of a check, quoted line by line under `indent` so that nothing in it can pose as a
/// line of fleet's own text; the last lines are kept when it is long.
fn quoted_output(output: &str, indent: &str) -> String {
    if output.trim().is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = output.lines().collect();
    let skip = lines.len().saturating_sub(OUTPUT_LINES);
    let kept = lines.get(skip..).unwrap_or(&[]).join("\n");
    let mut out = String::new();
    if skip > 0 {
        let _ = writeln!(out, "{indent}> ... ({skip} earlier lines not shown)");
    }
    for line in quote_lines(&kept).lines() {
        let _ = writeln!(out, "{indent}{line}");
    }
    out
}

fn short_id(id: &str) -> String {
    prefix(&clean(id), 12)
}

fn task_cells(goal: &Value) -> String {
    let tasks = items(goal, "tasks");
    if tasks.is_empty() {
        return "-".to_string();
    }
    tasks
        .iter()
        .map(|t| {
            let verification = text(t, "verification");
            if verification == "none" || verification.is_empty() {
                format!("{} ({})", text(t, "task"), text(t, "status"))
            } else {
                format!(
                    "{} ({}, {})",
                    text(t, "task"),
                    text(t, "status"),
                    verification
                )
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `fleet goals add`.
pub fn render_goal_added(v: &Value) -> String {
    let mut line = format!("goal {} added ({})", text(v, "id"), text(v, "status"));
    let contract = text(v, "contract_id");
    if !contract.is_empty() {
        let _ = write!(line, ", contract {}", short_id(&contract));
    }
    line.push('\n');
    line
}

/// `fleet goals attach`.
pub fn render_goal_attached(v: &Value) -> String {
    format!(
        "goal {} now has contract {}\n",
        text(v, "id"),
        short_id(&text(v, "contract_id"))
    )
}

/// `fleet goals status`.
pub fn render_goal_status(v: &Value) -> String {
    format!("goal {} is {}\n", text(v, "id"), text(v, "status"))
}

/// `fleet goals list`.
pub fn render_goal_list(v: &Value) -> String {
    let goals = items(v, "goals");
    if goals.is_empty() {
        return "no goals\n".to_string();
    }
    let mut out = format!(
        "{} goals, {} verified, {} abandoned\n",
        num(v, "total"),
        num(v, "verified"),
        num(v, "abandoned")
    );
    let rows: Vec<Vec<String>> = goals
        .iter()
        .map(|g| {
            let depth = usize::try_from(num(g, "depth"))
                .unwrap_or(1)
                .saturating_sub(1);
            vec![
                format!("{}{}", "  ".repeat(depth.min(8)), text(g, "id")),
                text(g, "status"),
                text(g, "contract"),
                task_cells(g),
                text(g, "title"),
            ]
        })
        .collect();
    out.push_str(&table(
        &["GOAL", "STATUS", "CONTRACT", "TASKS", "TITLE"],
        &[48, 12, 8, TASKS_WIDTH, TITLE_WIDTH],
        &rows,
        "",
    ));
    out
}

fn latest_cells(check: &Value) -> Vec<String> {
    let latest = check.get("latest").filter(|l| !l.is_null());
    match latest {
        None => vec!["not run".to_string(), String::new()],
        Some(e) => vec![text(e, "verdict"), text_or_dash(e, "summary")],
    }
}

fn render_checks(out: &mut String, checks: &[Value]) {
    for c in checks {
        let flag = if c.get("mandatory").and_then(Value::as_bool) == Some(false) {
            "optional"
        } else {
            "mandatory"
        };
        let cells = latest_cells(c);
        let _ = writeln!(
            out,
            "  {} [{}, {}] {}",
            text(c, "check"),
            text(c, "kind"),
            flag,
            cells.join(": ").trim_end_matches(": ")
        );
        let detail = text(c, "detail");
        if !detail.is_empty() {
            let _ = writeln!(out, "    {}", truncate(&detail, DETAIL_WIDTH));
        }
        if let Some(e) = c.get("latest").filter(|l| !l.is_null()) {
            out.push_str(&quoted_output(
                e.get("output").and_then(Value::as_str).unwrap_or(""),
                "    ",
            ));
        }
    }
}

/// `fleet goals show`.
pub fn render_goal_show(v: &Value) -> String {
    let mut out = format!("goal {} [{}]\n", text(v, "id"), text(v, "status"));
    let _ = writeln!(out, "title:    {}", text(v, "title"));
    let parent = text(v, "parent");
    if !parent.is_empty() {
        let _ = writeln!(out, "parent:   {parent}");
    }
    let reason = text(v, "reason");
    if !reason.is_empty() {
        let _ = writeln!(out, "reason:   {reason}");
    }
    let intent = v.get("intent").and_then(Value::as_str).unwrap_or("");
    if !intent.trim().is_empty() {
        out.push_str("intent:\n");
        for line in quote_lines(intent).lines() {
            let _ = writeln!(out, "  {line}");
        }
    }
    let contract_id = text(v, "contract_id");
    if contract_id.is_empty() {
        out.push_str("contract: none\n");
    } else {
        let _ = writeln!(
            out,
            "contract: {} (id {})",
            text(v, "contract"),
            short_id(&contract_id)
        );
    }
    let tasks = items(v, "tasks");
    if !tasks.is_empty() {
        out.push_str("tasks:\n");
        for t in tasks {
            let _ = writeln!(
                out,
                "  {}  {}  verification: {}",
                text(t, "task"),
                text(t, "status"),
                text(t, "verification")
            );
        }
    }
    let children: Vec<String> = items(v, "child_ids")
        .iter()
        .filter_map(Value::as_str)
        .map(clean)
        .collect();
    if !children.is_empty() {
        let _ = writeln!(out, "children: {}", children.join(", "));
    }
    let checks = items(v, "checks");
    if !checks.is_empty() {
        out.push_str("checks (newest evidence of each):\n");
        render_checks(&mut out, checks);
    }
    let history = items(v, "history");
    if !history.is_empty() {
        out.push_str("evidence history (newest first):\n");
        let rows: Vec<Vec<String>> = history
            .iter()
            .map(|e| {
                vec![
                    text(e, "id"),
                    text(e, "check"),
                    text(e, "verdict"),
                    short_id(&text(e, "commit")),
                    text(e, "task"),
                    text(e, "summary"),
                ]
            })
            .collect();
        out.push_str(&table(
            &["EVIDENCE", "CHECK", "VERDICT", "COMMIT", "TASK", "SUMMARY"],
            &[10, 24, 8, 12, 24, DETAIL_WIDTH],
            &rows,
            "  ",
        ));
    }
    out
}

/// `fleet goals remaining`.
pub fn render_goal_remaining(v: &Value) -> String {
    let leaves = items(v, "open_leaves");
    let blockers = items(v, "blockers");
    let ready = items(v, "next_ready");
    let mut out = format!(
        "{} goals, {} verified, {} abandoned\n",
        num(v, "total"),
        num(v, "verified"),
        num(v, "abandoned")
    );
    if leaves.is_empty() && blockers.is_empty() && ready.is_empty() {
        out.push_str("nothing is left to do\n");
        return out;
    }
    out.push_str("open leaves:\n");
    if leaves.is_empty() {
        out.push_str("  none\n");
    } else {
        let rows: Vec<Vec<String>> = leaves
            .iter()
            .map(|g| {
                vec![
                    text(g, "id"),
                    text(g, "status"),
                    text(g, "contract"),
                    task_cells(g),
                    text(g, "title"),
                ]
            })
            .collect();
        out.push_str(&table(
            &["GOAL", "STATUS", "CONTRACT", "TASKS", "TITLE"],
            &[40, 12, 8, TASKS_WIDTH, TITLE_WIDTH],
            &rows,
            "  ",
        ));
    }
    out.push_str("blockers:\n");
    if blockers.is_empty() {
        out.push_str("  none\n");
    }
    for b in blockers {
        match text(b, "kind").as_str() {
            "task" => {
                let _ = writeln!(
                    out,
                    "  task {} ({}) for goal {}: {}",
                    text(b, "task"),
                    text(b, "status"),
                    text(b, "goal"),
                    truncate(&text(b, "detail"), DETAIL_WIDTH)
                );
            }
            _ => {
                let _ = writeln!(
                    out,
                    "  goal {} is blocked: {}",
                    text(b, "goal"),
                    truncate(&text(b, "detail"), DETAIL_WIDTH)
                );
            }
        }
    }
    out.push_str("next ready tasks:\n");
    if ready.is_empty() {
        out.push_str("  none\n");
    }
    for r in ready {
        let _ = writeln!(
            out,
            "  {}  (goal {})  {}",
            text(r, "task"),
            text(r, "goal"),
            truncate(&text(r, "title"), TITLE_WIDTH)
        );
    }
    out
}

/// `fleet verify`: the final state of the contracts that were run.
pub fn render_verify(v: &Value) -> String {
    let passed = v.get("passed").and_then(Value::as_bool) == Some(true);
    let mut out = format!(
        "verification of {}: {}\n",
        text(v, "target"),
        if passed { "PASSED" } else { "NOT PASSED" }
    );
    let task = text(v, "task");
    if !task.is_empty() {
        let _ = writeln!(out, "task {task}: {}", text_or_dash(v, "verification"));
    }
    for g in items(v, "goals") {
        let _ = writeln!(
            out,
            "goal {} [{}] contract {}",
            text(g, "goal"),
            text(g, "status"),
            text(g, "contract")
        );
        render_checks(&mut out, items(g, "checks"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Value {
        Value::str(x)
    }

    fn goal(id: &str, depth: u64, status: &str) -> Value {
        Value::object([
            ("id", s(id)),
            ("parent", Value::Null),
            ("title", s("Title")),
            ("status", s(status)),
            ("reason", s("added")),
            ("depth", Value::int(depth)),
            ("children", Value::int(0)),
            ("contract", s("pending")),
            ("contract_id", s(&"ab".repeat(32))),
            ("evidence", Value::int(0)),
            (
                "tasks",
                Value::array([Value::object([
                    ("task", s("r1/a")),
                    ("status", s("review")),
                    ("verification", s("unverified")),
                ])]),
            ),
        ])
    }

    fn evidence(verdict: &str, output: &str) -> Value {
        Value::object([
            ("id", s("e1")),
            ("check", s("tests")),
            ("verdict", s(verdict)),
            ("exit_code", Value::int(1)),
            ("summary", s("exit code 1, expected 0")),
            ("output", s(output)),
            ("commit", s(&"c".repeat(40))),
            ("task", s("a")),
            ("at_ms", Value::int(1)),
        ])
    }

    fn check(latest: Value) -> Value {
        Value::object([
            ("check", s("tests")),
            ("kind", s("command")),
            ("mandatory", Value::Bool(true)),
            ("detail", s("/usr/bin/make test (expect exit 0)")),
            ("latest", latest),
        ])
    }

    #[test]
    fn list_shows_the_tree_and_the_verification_of_each_task() {
        let v = Value::object([
            ("total", Value::int(2)),
            ("verified", Value::int(0)),
            ("abandoned", Value::int(0)),
            (
                "goals",
                Value::array([goal("root", 1, "open"), goal("kid", 2, "in_progress")]),
            ),
        ]);
        let out = render_goal_list(&v);
        assert!(
            out.starts_with("2 goals, 0 verified, 0 abandoned\n"),
            "{out}"
        );
        assert!(out.contains("root"), "{out}");
        assert!(out.contains("  kid"), "children are indented: {out}");
        assert!(out.contains("r1/a (review, unverified)"), "{out}");
        assert_eq!(
            render_goal_list(&Value::object([("goals", Value::array([]))])),
            "no goals\n"
        );
    }

    #[test]
    fn show_quotes_intent_and_check_output() {
        let mut g = goal("g", 1, "open");
        if let Value::Object(m) = &mut g {
            m.insert("intent".into(), s("line one\n# Heading\u{1b}[2J"));
            m.insert("child_ids".into(), Value::array([s("a"), s("b")]));
            m.insert(
                "checks".into(),
                Value::array([check(evidence(
                    "failed",
                    "boom\n# not a heading\n\u{1b}]0;x\u{7}",
                ))]),
            );
            m.insert("history".into(), Value::array([evidence("failed", "boom")]));
        }
        let out = render_goal_show(&g);
        assert!(out.contains("goal g [open]"), "{out}");
        assert!(out.contains("  > line one"), "{out}");
        assert!(
            out.contains("  > # Heading"),
            "intent lines cannot become headings: {out}"
        );
        assert!(
            out.contains("      > # not a heading") || out.contains("    > # not a heading"),
            "{out}"
        );
        assert!(!out.contains('\u{1b}') && !out.contains('\u{7}'), "{out:?}");
        assert!(out.contains("children: a, b"));
        assert!(
            out.contains("tests [command, mandatory] failed: exit code 1, expected 0"),
            "{out}"
        );
        assert!(out.contains("/usr/bin/make test"));
        assert!(out.contains("EVIDENCE"), "{out}");
        assert!(
            out.contains(&"c".repeat(12)) && !out.contains(&"c".repeat(13)),
            "{out}"
        );
    }

    #[test]
    fn show_without_a_contract_or_evidence() {
        let mut g = goal("g", 1, "open");
        if let Value::Object(m) = &mut g {
            m.insert("contract_id".into(), Value::Null);
            m.insert("tasks".into(), Value::array([]));
            m.insert("checks".into(), Value::array([]));
        }
        let out = render_goal_show(&g);
        assert!(out.contains("contract: none"), "{out}");
        assert!(!out.contains("checks"), "{out}");
    }

    #[test]
    fn a_check_that_was_never_run_says_so() {
        let mut g = goal("g", 1, "open");
        if let Value::Object(m) = &mut g {
            m.insert("checks".into(), Value::array([check(Value::Null)]));
        }
        assert!(render_goal_show(&g).contains("tests [command, mandatory] not run"));
    }

    #[test]
    fn long_output_keeps_its_last_lines() {
        let output: String = (0..40).map(|i| format!("line {i}\n")).collect();
        let out = quoted_output(&output, "  ");
        assert!(out.contains("(28 earlier lines not shown)"), "{out}");
        assert!(
            out.contains("> line 39") && !out.contains("> line 5\n"),
            "{out}"
        );
        assert_eq!(quoted_output("  \n", "  "), "");
    }

    #[test]
    fn remaining_has_three_sections() {
        let v = Value::object([
            ("total", Value::int(4)),
            ("verified", Value::int(1)),
            ("abandoned", Value::int(0)),
            ("open_leaves", Value::array([goal("one", 2, "open")])),
            (
                "blockers",
                Value::array([
                    Value::object([
                        ("kind", s("goal")),
                        ("goal", s("stuck")),
                        ("detail", s("needs\u{1b}[31m a decision")),
                    ]),
                    Value::object([
                        ("kind", s("task")),
                        ("task", s("r1/x")),
                        ("status", s("failed")),
                        ("goal", s("one")),
                        ("detail", s("tests failed")),
                    ]),
                ]),
            ),
            (
                "next_ready",
                Value::array([Value::object([
                    ("task", s("r1/b")),
                    ("goal", s("two")),
                    ("title", s("Task b")),
                ])]),
            ),
        ]);
        let out = render_goal_remaining(&v);
        assert!(out.contains("open leaves:\n"), "{out}");
        assert!(out.contains("goal stuck is blocked: needs"), "{out}");
        assert!(!out.contains('\u{1b}'));
        assert!(
            out.contains("task r1/x (failed) for goal one: tests failed"),
            "{out}"
        );
        assert!(out.contains("r1/b  (goal two)  Task b"), "{out}");
        let empty = Value::object([("total", Value::int(2)), ("verified", Value::int(2))]);
        assert!(render_goal_remaining(&empty).ends_with("nothing is left to do\n"));
        let only_blockers = Value::object([
            ("total", Value::int(1)),
            (
                "blockers",
                Value::array([Value::object([
                    ("kind", s("goal")),
                    ("goal", s("g")),
                    ("detail", s("x")),
                ])]),
            ),
        ]);
        let out = render_goal_remaining(&only_blockers);
        assert!(
            out.contains("open leaves:\n  none\n") && out.contains("next ready tasks:\n  none\n"),
            "{out}"
        );
    }

    #[test]
    fn verify_reports_pass_and_fail() {
        let doc = |passed: bool, verdict: &str| {
            Value::object([
                ("target", s("r1/a")),
                ("task", s("r1/a")),
                (
                    "verification",
                    s(if passed { "verified" } else { "failed" }),
                ),
                ("running", Value::Bool(false)),
                ("passed", Value::Bool(passed)),
                (
                    "goals",
                    Value::array([Value::object([
                        ("goal", s("g")),
                        ("status", s("open")),
                        ("contract", s(if passed { "passed" } else { "failed" })),
                        (
                            "checks",
                            Value::array([check(evidence(verdict, "assertion failed"))]),
                        ),
                    ])]),
                ),
            ])
        };
        let ok = render_verify(&doc(true, "passed"));
        assert!(ok.starts_with("verification of r1/a: PASSED\n"), "{ok}");
        let bad = render_verify(&doc(false, "failed"));
        assert!(
            bad.starts_with("verification of r1/a: NOT PASSED\n"),
            "{bad}"
        );
        assert!(bad.contains("task r1/a: failed"), "{bad}");
        assert!(bad.contains("    > assertion failed"), "{bad}");
    }

    #[test]
    fn acknowledgements() {
        let added = Value::object([
            ("id", s("g")),
            ("status", s("open")),
            ("contract_id", s(&"ab".repeat(32))),
        ]);
        assert_eq!(
            render_goal_added(&added),
            format!("goal g added (open), contract {}\n", "ab".repeat(6))
        );
        let plain = Value::object([
            ("id", s("g")),
            ("status", s("open")),
            ("contract_id", Value::Null),
        ]);
        assert_eq!(render_goal_added(&plain), "goal g added (open)\n");
        assert_eq!(
            render_goal_attached(&Value::object([
                ("id", s("g")),
                ("contract_id", s(&"cd".repeat(32)))
            ])),
            format!("goal g now has contract {}\n", "cd".repeat(6))
        );
        assert_eq!(render_goal_status(&plain), "goal g is open\n");
    }
}
