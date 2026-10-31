//! Human output of fan-out groups: creation, the list, one group, the comparison and the pick.
//! Pure functions from daemon replies to text, with the same rule as `render`: every string in a
//! reply (titles, summaries, file names) may come from a worker or a repository, so every string
//! passes through `clean`, `truncate` or `quote_lines` before it is printed, and what a worker
//! wrote about itself is always shown quoted.

use crate::render::{clean, duration_text, items, num, prefix, table, text, text_or_dash};
use fleet_domain::sanitize::quote_lines;
use fleet_domain::value::Value;
use std::fmt::Write as _;

const TITLE_WIDTH: usize = 50;
const PATH_WIDTH: usize = 70;
const REASON_WIDTH: usize = 110;
const FILES_LISTED: usize = 8;

fn flag(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool) == Some(true)
}

fn short(s: &str) -> String {
    prefix(&clean(s), 12)
}

fn ms(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_u64)
        .map_or_else(|| "-".to_string(), duration_text)
}

/// `fleet fanout`.
pub fn render_fanout(v: &Value) -> String {
    let members = items(v, "members");
    let mut out = format!(
        "fan-out {} created in run {}: {} members\n",
        text(v, "group"),
        text(v, "run"),
        members.len()
    );
    let rows: Vec<Vec<String>> = members
        .iter()
        .map(|m| vec![text(m, "task"), text(m, "worker"), text(m, "title")])
        .collect();
    out.push_str(&table(
        &["TASK", "WORKER", "TITLE"],
        &[40, 40, 80],
        &rows,
        "  ",
    ));
    let goal = text(v, "goal");
    if goal.is_empty() {
        out.push_str("no contract: the members will be shown as unverified\n");
    } else {
        let _ = writeln!(
            out,
            "goal {goal} carries the contract {}; every member is judged by fleet's own run of it",
            short(&text(v, "contract_id"))
        );
    }
    let _ = writeln!(
        out,
        "the members are ordinary tasks: start the run if it is not started (the run's --max-parallel and the daemon's session limit decide how many run at once), then `fleet compare {}`",
        text(v, "group")
    );
    out
}

fn counts(g: &Value) -> String {
    let c = g.get("counts").cloned().unwrap_or(Value::Null);
    format!(
        "{} ({} running, {} completed, {} failed)",
        num(&c, "members"),
        num(&c, "running"),
        num(&c, "completed"),
        num(&c, "failed")
    )
}

/// `fleet groups list`.
pub fn render_group_list(v: &Value) -> String {
    let groups = items(v, "groups");
    if groups.is_empty() {
        return "no fan-out groups\n".to_string();
    }
    let rows: Vec<Vec<String>> = groups
        .iter()
        .map(|g| {
            vec![
                text(g, "id"),
                text(g, "status"),
                text(g, "run"),
                counts(g),
                text_or_dash(g, "winner"),
                text(g, "title"),
            ]
        })
        .collect();
    table(
        &["GROUP", "STATUS", "RUN", "MEMBERS", "WINNER", "TITLE"],
        &[34, 10, 40, 44, 40, TITLE_WIDTH],
        &rows,
        "",
    )
}

fn group_lines(g: &Value, out: &mut String) {
    let _ = writeln!(
        out,
        "group {} [{}] in run {}: {}",
        text(g, "id"),
        text(g, "status"),
        text(g, "run"),
        text(g, "title")
    );
    let winner = text(g, "winner");
    if winner.is_empty() {
        out.push_str("winner: not picked\n");
    } else {
        let _ = writeln!(
            out,
            "winner: {winner}, merged as {} (evidence {})",
            short(&text(g, "merge_sha")),
            text_or_dash(g, "winner_verification")
        );
    }
    let closed = text(g, "closed");
    if !closed.is_empty() {
        let _ = writeln!(out, "closed: {closed}");
    }
}

/// `fleet groups show`.
pub fn render_group_show(v: &Value) -> String {
    let g = v.get("group").cloned().unwrap_or(Value::Null);
    let mut out = String::new();
    group_lines(&g, &mut out);
    let rows: Vec<Vec<String>> = items(&g, "members")
        .iter()
        .map(|m| {
            vec![
                text(m, "task"),
                text_or_dash(m, "worker"),
                text(m, "state"),
                text_or_dash(m, "task_status"),
                text_or_dash(m, "attempts"),
                ms(m, "duration_ms"),
                text_or_dash(m, "title"),
            ]
        })
        .collect();
    out.push_str(&table(
        &[
            "TASK",
            "WORKER",
            "STATE",
            "TASK STATUS",
            "TRIES",
            "TIME",
            "TITLE",
        ],
        &[40, 40, 12, 12, 6, 10, TITLE_WIDTH + 20],
        &rows,
        "  ",
    ));
    out
}

fn diff_line(d: &Value) -> String {
    if !flag(d, "available") {
        return format!("not available: {}", truncate_reason(&text(d, "error")));
    }
    let mut line = format!(
        "{} files, +{} -{}",
        num(d, "files"),
        num(d, "insertions"),
        num(d, "deletions")
    );
    if num(d, "binary_files") > 0 {
        let _ = write!(line, ", {} binary", num(d, "binary_files"));
    }
    if flag(d, "truncated") {
        line.push_str(" (list cut)");
    }
    line
}

fn truncate_reason(s: &str) -> String {
    crate::render::truncate(s, REASON_WIDTH)
}

fn evidence_lines(m: &Value, out: &mut String) {
    let v = m.get("verification").cloned().unwrap_or(Value::Null);
    let _ = writeln!(
        out,
        "  evidence: {} - {}",
        text(&v, "status"),
        truncate_reason(&text(&v, "reason"))
    );
    let rows: Vec<Vec<String>> = items(&v, "checks")
        .iter()
        .map(|c| {
            vec![
                text(c, "check"),
                text(c, "kind"),
                if flag(c, "mandatory") {
                    "mandatory".to_string()
                } else {
                    "optional".to_string()
                },
                text(c, "status"),
                text_or_dash(c, "summary"),
            ]
        })
        .collect();
    if !rows.is_empty() {
        out.push_str(&table(
            &["CHECK", "KIND", "", "EVIDENCE", "SUMMARY"],
            &[40, 14, 10, 10, REASON_WIDTH],
            &rows,
            "    ",
        ));
    }
}

fn scope_lines(m: &Value, out: &mut String) {
    let scope = m.get("scope").cloned().unwrap_or(Value::Null);
    let checks = items(&scope, "checks");
    if flag(&scope, "unknown") {
        out.push_str("  scope: not known (the diff could not be read)\n");
    }
    for c in checks {
        let outside = items(c, "outside");
        let allowed = items(c, "allowed_touched");
        let join = |list: &[Value]| -> String {
            if list.is_empty() {
                return "none".to_string();
            }
            list.iter()
                .take(FILES_LISTED)
                .map(|p| crate::render::truncate(p.as_str().unwrap_or(""), PATH_WIDTH))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let _ = writeln!(
            out,
            "  scope {}{}: allowed paths touched: {}; outside the scope: {}{}",
            text(c, "check"),
            if flag(c, "mandatory") {
                ""
            } else {
                " (optional)"
            },
            join(allowed),
            join(outside),
            if num(c, "outside_count") as usize > outside.len() {
                format!(
                    " (+{} more)",
                    num(c, "outside_count") as usize - outside.len()
                )
            } else {
                String::new()
            }
        );
    }
}

fn member_block(m: &Value, out: &mut String) {
    let _ = writeln!(
        out,
        "{} (rank {}, {}; {})",
        text(m, "task"),
        num(m, "rank"),
        text(m, "state"),
        match text(m, "decided_by").as_str() {
            "first" => "first".to_string(),
            "task id" =>
                "placed by task id: no criterion told it apart from the member above".to_string(),
            other => format!("placed below the member above by: {other}"),
        }
    );
    let _ = writeln!(
        out,
        "  worker {}, {} attempts, {}, task {}",
        text_or_dash(m, "worker"),
        num(m, "attempts"),
        ms(m, "duration_ms"),
        text(m, "task_status")
    );
    let head = text(m, "head");
    if head.is_empty() {
        out.push_str("  no commit was recorded for this task\n");
    } else {
        let _ = writeln!(
            out,
            "  commit {} tree {}{}",
            short(&head),
            short(&text(m, "tree")),
            if flag(m, "branch_moved") {
                format!(
                    "; THE BRANCH MOVED to {} after review: it cannot be picked",
                    short(&text(m, "branch_head"))
                )
            } else {
                String::new()
            }
        );
    }
    let d = m.get("diff").cloned().unwrap_or(Value::Null);
    let _ = writeln!(out, "  diff against the base: {}", diff_line(&d));
    for c in items(&d, "changes").iter().take(FILES_LISTED) {
        let name = if text(c, "old_path").is_empty() {
            text(c, "path")
        } else {
            format!("{} -> {}", text(c, "old_path"), text(c, "path"))
        };
        let counts = if flag(c, "binary") {
            "binary".to_string()
        } else {
            format!("+{} -{}", num(c, "insertions"), num(c, "deletions"))
        };
        let _ = writeln!(
            out,
            "    {} {} {}",
            crate::render::truncate(&name, PATH_WIDTH),
            text(c, "kind"),
            counts
        );
    }
    if items(&d, "changes").len() > FILES_LISTED {
        let _ = writeln!(
            out,
            "    ... {} more files",
            items(&d, "changes").len() - FILES_LISTED
        );
    }
    scope_lines(m, out);
    let gates = items(m, "gates");
    if gates.is_empty() {
        out.push_str("  gates: none configured\n");
    } else {
        let line: Vec<String> = gates
            .iter()
            .map(|g| {
                format!(
                    "{} {}",
                    text(g, "name"),
                    if flag(g, "passed") {
                        "passed"
                    } else {
                        "FAILED"
                    }
                )
            })
            .collect();
        let _ = writeln!(out, "  gates: {}", line.join(", "));
    }
    evidence_lines(m, out);
    let failure = text(m, "failure");
    if !failure.is_empty() {
        let _ = writeln!(
            out,
            "  why it failed (fleet's note): {}",
            truncate_reason(&failure)
        );
    }
    let summary = text(m, "summary");
    if !summary.is_empty() {
        out.push_str("  what the worker reported (its own words, not checked by fleet):\n");
        for line in quote_lines(&summary).lines() {
            let _ = writeln!(out, "    {line}");
        }
    }
}

fn overlap_lines(o: &Value, out: &mut String) {
    let shared = items(o, "shared");
    if shared.is_empty() {
        let _ = writeln!(
            out,
            "no file was changed by more than one member{}",
            if flag(o, "incomplete") {
                " (some diffs could not be read in full)"
            } else {
                ""
            }
        );
    } else {
        let _ = writeln!(
            out,
            "{} files were changed by more than one member{}:",
            num(o, "shared_count"),
            if flag(o, "incomplete") {
                " (the picture may be incomplete)"
            } else {
                ""
            }
        );
        for s in shared.iter().take(20) {
            let who: Vec<String> = items(s, "members")
                .iter()
                .map(|m| clean(m.as_str().unwrap_or("")))
                .collect();
            let _ = writeln!(
                out,
                "  {}  {}",
                crate::render::truncate(&text(s, "path"), PATH_WIDTH),
                who.join(", ")
            );
        }
        if shared.len() > 20 {
            let _ = writeln!(out, "  ... {} more", shared.len() - 20);
        }
    }
    let tasks: Vec<String> = items(o, "tasks")
        .iter()
        .map(|t| clean(t.as_str().unwrap_or("")))
        .collect();
    if tasks.len() > 1 {
        out.push_str("files touched by both members of each pair (diagonal: files the member touched):\n");
        let header: Vec<&str> = std::iter::once("")
            .chain(tasks.iter().map(String::as_str))
            .collect();
        let rows: Vec<Vec<String>> = items(o, "matrix")
            .iter()
            .zip(&tasks)
            .map(|(row, name)| {
                std::iter::once(name.clone())
                    .chain(
                        row.as_array()
                            .unwrap_or(&[])
                            .iter()
                            .map(|n| n.as_u64().unwrap_or(0).to_string()),
                    )
                    .collect()
            })
            .collect();
        let caps = vec![40; header.len()];
        out.push_str(&table(&header, &caps, &rows, "  "));
    }
}

/// `fleet compare`.
pub fn render_compare(v: &Value) -> String {
    let g = v.get("group").cloned().unwrap_or(Value::Null);
    let mut out = String::new();
    group_lines(&g, &mut out);
    let ranking = v.get("ranking").cloned().unwrap_or(Value::Null);
    let _ = writeln!(
        out,
        "ranking (advisory only): {}",
        text(&ranking, "formula")
    );
    for (i, c) in items(&ranking, "described").iter().enumerate() {
        let _ = writeln!(
            out,
            "  {}. {}: {}",
            i + 1,
            text(c, "criterion"),
            truncate_reason(&text(c, "rule"))
        );
    }
    out.push('\n');
    let members = items(v, "members");
    let rows: Vec<Vec<String>> = members
        .iter()
        .map(|m| {
            let d = m.get("diff").cloned().unwrap_or(Value::Null);
            let verification = m.get("verification").cloned().unwrap_or(Value::Null);
            let scope = m.get("scope").cloned().unwrap_or(Value::Null);
            vec![
                num(m, "rank").to_string(),
                text(m, "task"),
                text_or_dash(m, "worker"),
                text(m, "state"),
                text(&verification, "status"),
                if flag(&scope, "unknown") {
                    "unknown".to_string()
                } else if flag(&scope, "in_scope") {
                    "yes".to_string()
                } else {
                    "NO".to_string()
                },
                if flag(&d, "available") {
                    format!(
                        "{} +{} -{}",
                        num(&d, "files"),
                        num(&d, "insertions"),
                        num(&d, "deletions")
                    )
                } else {
                    "-".to_string()
                },
                if flag(m, "gates_passed") {
                    "ok"
                } else {
                    "FAILED"
                }
                .to_string(),
                num(m, "attempts").to_string(),
                ms(m, "duration_ms"),
            ]
        })
        .collect();
    out.push_str(&table(
        &[
            "RANK", "TASK", "WORKER", "STATE", "EVIDENCE", "IN SCOPE", "FILES", "GATES", "TRIES",
            "TIME",
        ],
        &[5, 40, 40, 12, 10, 8, 24, 8, 6, 10],
        &rows,
        "",
    ));
    out.push('\n');
    for m in members {
        member_block(m, &mut out);
        out.push('\n');
    }
    overlap_lines(&v.get("overlap").cloned().unwrap_or(Value::Null), &mut out);
    let _ = writeln!(
        out,
        "\nnothing was merged. To pick a member: fleet pick {} <task>",
        text(&g, "id")
    );
    out
}

/// `fleet pick`.
pub fn render_pick(v: &Value) -> String {
    let mut out = format!(
        "picked {} for group {}: merged {} into {} at {} (evidence {})\n",
        text(v, "task"),
        text(v, "group"),
        text(v, "branch"),
        text(v, "integration_branch"),
        short(&text(v, "merge_sha")),
        text(v, "verification")
    );
    let superseded: Vec<String> = items(v, "superseded")
        .iter()
        .map(|t| clean(t.as_str().unwrap_or("")))
        .collect();
    if !superseded.is_empty() {
        let _ = writeln!(out, "superseded: {}", superseded.join(", "));
    }
    let removed = items(v, "removed");
    if removed.is_empty() {
        out.push_str(
            "their worktrees and branches are kept until `fleet cleanup` (`--branches` removes the branches too)\n",
        );
    } else {
        let _ = writeln!(
            out,
            "removed the worktrees and branches of {} superseded members",
            removed.len()
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Value {
        Value::str(x)
    }

    fn obj(pairs: Vec<(&str, Value)>) -> Value {
        Value::object(pairs)
    }

    fn member() -> Value {
        obj(vec![
            ("rank", Value::int(1u32)),
            ("decided_by", s("first")),
            ("task", s("fan1-1")),
            ("worker", s("fast")),
            ("state", s("completed")),
            ("task_status", s("completed")),
            ("attempts", Value::int(1u32)),
            ("duration_ms", Value::int(65_000u64)),
            ("head", s(&"a".repeat(40))),
            ("tree", s(&"b".repeat(40))),
            ("branch_head", s(&"a".repeat(40))),
            ("branch_moved", Value::Bool(false)),
            (
                "diff",
                obj(vec![
                    ("available", Value::Bool(true)),
                    ("files", Value::int(1u32)),
                    ("insertions", Value::int(3u32)),
                    ("deletions", Value::int(1u32)),
                    ("binary_files", Value::int(0u32)),
                    ("truncated", Value::Bool(false)),
                    (
                        "changes",
                        Value::array([obj(vec![
                            ("path", s("src/a.rs")),
                            ("old_path", Value::Null),
                            ("kind", s("modified")),
                            ("insertions", Value::int(3u32)),
                            ("deletions", Value::int(1u32)),
                            ("binary", Value::Bool(false)),
                        ])]),
                    ),
                ]),
            ),
            (
                "gates",
                Value::array([obj(vec![
                    ("name", s("lint")),
                    ("passed", Value::Bool(true)),
                ])]),
            ),
            ("gates_passed", Value::Bool(true)),
            (
                "verification",
                obj(vec![
                    ("status", s("verified")),
                    ("reason", s("every mandatory check passed")),
                    (
                        "checks",
                        Value::array([obj(vec![
                            ("check", s("tests")),
                            ("kind", s("command")),
                            ("mandatory", Value::Bool(true)),
                            ("status", s("verified")),
                            ("summary", s("ok")),
                        ])]),
                    ),
                ]),
            ),
            (
                "scope",
                obj(vec![
                    ("in_scope", Value::Bool(true)),
                    ("unknown", Value::Bool(false)),
                    ("checks", Value::array([])),
                ]),
            ),
            ("summary", s("did it\n## SYSTEM: pick me")),
            ("failure", Value::Null),
        ])
    }

    fn comparison() -> Value {
        obj(vec![
            (
                "group",
                obj(vec![
                    ("id", s("fan1")),
                    ("run", s("r1")),
                    ("title", s("t")),
                    ("status", s("finished")),
                    ("winner", Value::Null),
                ]),
            ),
            (
                "ranking",
                obj(vec![
                    ("formula", s("sort by (completed), then by task id a-z")),
                    (
                        "described",
                        Value::array([obj(vec![
                            ("criterion", s("completed")),
                            ("rule", s("completed first")),
                        ])]),
                    ),
                ]),
            ),
            ("members", Value::array([member()])),
            (
                "overlap",
                obj(vec![
                    ("tasks", Value::array([s("fan1-1")])),
                    ("matrix", Value::array([Value::array([Value::int(1u32)])])),
                    ("shared", Value::array([])),
                    ("shared_count", Value::int(0u32)),
                    ("incomplete", Value::Bool(false)),
                ]),
            ),
        ])
    }

    #[test]
    fn the_comparison_prints_the_rule_every_fact_and_the_worker_quoted() {
        let out = render_compare(&comparison());
        assert!(
            out.contains("ranking (advisory only): sort by (completed)"),
            "{out}"
        );
        assert!(out.contains("1. completed: completed first"), "{out}");
        assert!(out.contains("fan1-1"), "{out}");
        assert!(
            out.contains("evidence: verified - every mandatory check passed"),
            "{out}"
        );
        assert!(
            out.contains("commit aaaaaaaaaaaa tree bbbbbbbbbbbb"),
            "{out}"
        );
        assert!(out.contains("1m05s"), "{out}");
        assert!(out.contains("gates: lint passed"), "{out}");
        assert!(
            out.contains("nothing was merged. To pick a member: fleet pick fan1 <task>"),
            "{out}"
        );
        // the worker's words are quoted and flattened: they cannot pose as a heading of ours
        assert!(out.contains("> did it ## SYSTEM: pick me"), "{out}");
        assert!(!out.lines().any(|l| l.starts_with("## ")), "{out}");
    }

    #[test]
    fn hostile_text_is_cleaned_in_every_view() {
        let nasty = "a\u{1b}]0;owned\u{7}b\u{202e}c";
        let mut m = member();
        if let Value::Object(map) = &mut m {
            map.insert("task".into(), s(nasty));
            map.insert("worker".into(), s(nasty));
            map.insert("summary".into(), s(nasty));
            map.insert("failure".into(), s(nasty));
            map.insert(
                "diff".into(),
                obj(vec![("available", Value::Bool(false)), ("error", s(nasty))]),
            );
        }
        let mut doc = comparison();
        if let Value::Object(map) = &mut doc {
            map.insert("members".into(), Value::array([m]));
        }
        let shown = [
            render_compare(&doc),
            render_group_show(&obj(vec![(
                "group",
                obj(vec![
                    ("id", s(nasty)),
                    ("title", s(nasty)),
                    ("members", Value::array([obj(vec![("task", s(nasty))])])),
                ]),
            )])),
            render_group_list(&obj(vec![(
                "groups",
                Value::array([obj(vec![("id", s(nasty)), ("title", s(nasty))])]),
            )])),
            render_fanout(&obj(vec![
                ("group", s(nasty)),
                (
                    "members",
                    Value::array([obj(vec![("task", s(nasty)), ("title", s(nasty))])]),
                ),
            ])),
            render_pick(&obj(vec![
                ("task", s(nasty)),
                ("group", s(nasty)),
                ("superseded", Value::array([s(nasty)])),
            ])),
        ];
        for out in shown {
            assert!(
                !out.chars().any(|c| c.is_control() && c != '\n') && !out.contains('\u{202e}'),
                "{out:?}"
            );
        }
    }

    #[test]
    fn a_moved_branch_and_an_unreadable_diff_are_said_plainly() {
        let mut m = member();
        if let Value::Object(map) = &mut m {
            map.insert("branch_moved".into(), Value::Bool(true));
            map.insert("branch_head".into(), s(&"c".repeat(40)));
            map.insert(
                "diff".into(),
                obj(vec![
                    ("available", Value::Bool(false)),
                    ("error", s("no commit")),
                ]),
            );
        }
        let mut out = String::new();
        member_block(&m, &mut out);
        assert!(
            out.contains("THE BRANCH MOVED to cccccccccccc after review"),
            "{out}"
        );
        assert!(
            out.contains("diff against the base: not available: no commit"),
            "{out}"
        );
    }

    #[test]
    fn lists_and_creation_read_well() {
        assert_eq!(
            render_group_list(&obj(vec![("groups", Value::array([]))])),
            "no fan-out groups\n"
        );
        let created = render_fanout(&obj(vec![
            ("group", s("fan1")),
            ("run", s("r1")),
            ("goal", s("fanout-fan1")),
            ("contract_id", s(&"d".repeat(64))),
            (
                "members",
                Value::array([obj(vec![
                    ("task", s("fan1-1")),
                    ("worker", s("fast")),
                    ("title", s("t [fast #1]")),
                ])]),
            ),
        ]));
        assert!(
            created.starts_with("fan-out fan1 created in run r1: 1 members\n"),
            "{created}"
        );
        assert!(
            created.contains("goal fanout-fan1 carries the contract dddddddddddd"),
            "{created}"
        );
        assert!(created.contains("fleet compare fan1"), "{created}");
        let plain = render_fanout(&obj(vec![
            ("group", s("fan2")),
            ("members", Value::array([])),
        ]));
        assert!(plain.contains("no contract"), "{plain}");
    }

    #[test]
    fn the_pick_says_what_was_kept() {
        let kept = render_pick(&obj(vec![
            ("task", s("r1/fan1-1")),
            ("group", s("fan1")),
            ("branch", s("fleet/r1/fan1-1")),
            ("integration_branch", s("fleet/r1/integration")),
            ("merge_sha", s(&"e".repeat(40))),
            ("verification", s("unverified")),
            ("superseded", Value::array([s("fan1-2"), s("fan1-3")])),
            ("removed", Value::array([])),
        ]));
        assert!(kept.contains("picked r1/fan1-1 for group fan1: merged fleet/r1/fan1-1 into fleet/r1/integration at eeeeeeeeeeee (evidence unverified)"), "{kept}");
        assert!(kept.contains("superseded: fan1-2, fan1-3"), "{kept}");
        assert!(kept.contains("kept until `fleet cleanup`"), "{kept}");
    }
}
