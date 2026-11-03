//! Human output. Pure functions from daemon replies (neutral [`Value`]s) to text.
//!
//! Everything in a reply may originate from a worker (titles, summaries, gate text, output), so
//! every string passes through [`clean`] or [`multiline`] before it is printed. Output is plain
//! text: no colors, no cursor control.

use fleet_domain::sanitize::sanitize_text;
use fleet_domain::value::Value;
use std::fmt::Write as _;

/// Shown when the daemon dropped output the reader had not yet seen.
pub const LOST_NOTICE: &str = "[fleet: some output was dropped before this point]";

const DIGEST_PREFIX: usize = 12;
const RECENT_SHOWN: usize = 10;

/// Single-line, terminal-safe text.
pub fn clean(s: &str) -> String {
    sanitize_text(s.as_bytes())
        .chars()
        .map(|c| {
            if matches!(c, '\n' | '\r' | '\t') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Terminal-safe text that keeps line breaks.
pub fn multiline(s: &str) -> String {
    sanitize_text(s.as_bytes())
}

/// Cleans and cuts to at most `width` characters, ending in `…` when something was removed.
pub fn truncate(s: &str, width: usize) -> String {
    let s = clean(s);
    if s.chars().count() <= width {
        return s;
    }
    let keep = width.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    if width > 0 {
        out.push('…');
    }
    out
}

pub(crate) fn text(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::Str(s)) => clean(s),
        Some(Value::Int(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

pub(crate) fn text_or_dash(v: &Value, key: &str) -> String {
    let t = text(v, key);
    if t.is_empty() {
        "-".to_string()
    } else {
        t
    }
}

/// The worker of a task, followed by `host:<name>` when it runs on a remote host.
fn worker_cell(t: &Value) -> String {
    let worker = text_or_dash(t, "worker");
    match text(t, "host") {
        h if h.is_empty() => worker,
        h => format!("{worker} host:{h}"),
    }
}

pub(crate) fn num(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

pub(crate) fn items<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).unwrap_or(&[])
}

pub(crate) fn prefix(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// A left-aligned table. Each cell is cleaned and cut to its column's cap; columns are as wide
/// as their widest (cut) cell, measured in characters. Trailing spaces are not emitted.
pub fn table(header: &[&str], caps: &[usize], rows: &[Vec<String>], indent: &str) -> String {
    let cols = header.len();
    let cut = |row: &[String]| -> Vec<String> {
        (0..cols)
            .map(|i| {
                let cell = row.get(i).map_or("", String::as_str);
                match caps.get(i) {
                    Some(cap) => truncate(cell, *cap),
                    None => clean(cell),
                }
            })
            .collect()
    };
    let head: Vec<String> = header.iter().map(|h| h.to_string()).collect();
    let mut all: Vec<Vec<String>> = vec![head];
    all.extend(rows.iter().map(|r| cut(r)));
    let widths: Vec<usize> = (0..cols)
        .map(|i| {
            all.iter()
                .map(|r| r.get(i).map_or(0, |c| c.chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in &all {
        let mut line = String::from(indent);
        for (i, cell) in row.iter().enumerate() {
            line.push_str(cell);
            if i + 1 < cols {
                let w = widths.get(i).copied().unwrap_or(0);
                let pad = w.saturating_sub(cell.chars().count());
                line.extend(std::iter::repeat_n(' ', pad + 2));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// `1m05s`-style duration.
pub fn duration_text(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else if s < 86_400 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3600)
    }
}

fn task_note(t: &Value) -> String {
    let mut note = String::new();
    if t.get("live").and_then(Value::as_bool) == Some(true) {
        note.push_str("(live) ");
    }
    if let Some(block) = t.get("block").filter(|b| !b.is_null()) {
        let kind = text(block, "kind");
        let subject = text(block, "subject");
        let detail = text(block, "detail");
        let _ = write!(note, "blocked: {kind}");
        if !subject.is_empty() {
            let _ = write!(note, " {subject}");
        }
        if !detail.is_empty() {
            let _ = write!(note, " - {detail}");
        }
    } else if let Some(f) = t.get("failure").and_then(Value::as_str) {
        let _ = write!(note, "failed: {}", clean(f));
    } else if let Some(s) = t.get("summary").and_then(Value::as_str) {
        note.push_str(&clean(s));
    }
    let failed_gates: Vec<String> = items(t, "gates")
        .iter()
        .filter(|g| g.get("passed").and_then(Value::as_bool) == Some(false))
        .map(|g| text(g, "name"))
        .collect();
    if !failed_gates.is_empty() {
        let _ = write!(note, " [gate failed: {}]", failed_gates.join(", "));
    }
    note.trim().to_string()
}

fn deps_text(t: &Value) -> String {
    let deps: Vec<String> = items(t, "deps")
        .iter()
        .filter_map(Value::as_str)
        .map(clean)
        .collect();
    if deps.is_empty() {
        "-".to_string()
    } else {
        deps.join(",")
    }
}

fn render_run(run: &Value, out: &mut String) {
    let _ = writeln!(
        out,
        "run {} [{}] max-parallel {}",
        text_or_dash(run, "id"),
        text_or_dash(run, "status"),
        num(run, "max_parallel")
    );
    let _ = writeln!(
        out,
        "  objective  {}",
        truncate(&text(run, "objective"), 100)
    );
    let sha = prefix(&text(run, "base_sha"), 8);
    let _ = writeln!(
        out,
        "  repo       {} (base {} @ {})",
        truncate(&text(run, "repo"), 80),
        text_or_dash(run, "base_ref"),
        if sha.is_empty() { "-" } else { &sha }
    );
    let _ = writeln!(
        out,
        "  branch     {}",
        text_or_dash(run, "integration_branch")
    );
    if let Some(c) = run.get("counts") {
        let parts: Vec<String> = [
            "pending",
            "ready",
            "running",
            "review",
            "completed",
            "failed",
            "blocked",
        ]
        .iter()
        .map(|k| format!("{k} {}", num(c, k)))
        .collect();
        let _ = writeln!(out, "  tasks      {}", parts.join(", "));
    }
    let rows: Vec<Vec<String>> = items(run, "tasks")
        .iter()
        .map(|t| {
            vec![
                text_or_dash(t, "id"),
                text_or_dash(t, "status"),
                deps_text(t),
                num(t, "attempt").to_string(),
                worker_cell(t),
                task_note(t),
            ]
        })
        .collect();
    if rows.is_empty() {
        out.push_str("  (no tasks)\n");
    } else {
        out.push_str(&table(
            &["ID", "STATUS", "DEPS", "TRY", "WORKER", "NOTE"],
            &[20, 12, 24, 4, 28, 60],
            &rows,
            "  ",
        ));
    }
}

fn render_gate_lines(gates: &[Value], only_open: bool, out: &mut String) {
    for g in gates {
        let open = g.get("open").and_then(Value::as_bool).unwrap_or(false);
        if only_open && !open {
            continue;
        }
        let _ = writeln!(
            out,
            "gate {} [{}] {}/{}: {}",
            text_or_dash(g, "id"),
            text_or_dash(g, "kind"),
            text_or_dash(g, "run"),
            text_or_dash(g, "task"),
            truncate(&text(g, "text"), 200)
        );
        let choices: Vec<String> = items(g, "choices")
            .iter()
            .filter_map(Value::as_str)
            .map(|c| truncate(c, 60))
            .collect();
        if open && !choices.is_empty() {
            let _ = writeln!(out, "  choices: {}", choices.join(" | "));
        }
        if !open {
            let _ = writeln!(out, "  answered: {}", truncate(&text(g, "answer"), 200));
        }
    }
}

/// What an approval for a pull request covers, one argument per line and none of them cut short:
/// the person who approves it reads the whole title and body, and the flag that makes it a draft
/// comes before the long fields so that nothing can push it out of sight. The body is quoted line
/// by line, so nothing in it can pass for a line fleet wrote. Returns false when `a` is not such
/// an approval.
fn render_pull_request_action(a: &Value, out: &mut String) -> bool {
    if text(a, "purpose") != "pull-request" {
        return false;
    }
    let Some(args) = a.get("args").and_then(Value::as_array) else {
        return false;
    };
    let arg = |i: usize| match args.get(i) {
        Some(Value::Str(s)) => Some(s.as_str()),
        _ => None,
    };
    let (Some(repo), Some(base), Some(branch), Some(sha), Some(title), Some(body), Some(flag)) =
        (arg(0), arg(1), arg(2), arg(3), arg(4), arg(5), arg(6))
    else {
        return false;
    };
    let _ = writeln!(out, "  repository:   {}", clean(repo));
    let _ = writeln!(out, "  base:         {}", clean(base));
    let _ = writeln!(out, "  branch:       {}", clean(branch));
    let _ = writeln!(out, "  commit:       {}", clean(sha));
    let _ = writeln!(out, "  pull request: {}", clean(flag));
    let _ = writeln!(out, "  title:        {}", clean(title));
    let quoted = fleet_domain::sanitize::quote_lines(&sanitize_text(body.as_bytes()));
    if quoted.is_empty() {
        let _ = writeln!(out, "  body:         (empty)");
    } else {
        let _ = writeln!(out, "  body:");
        for line in quoted.lines() {
            let _ = writeln!(out, "    {line}");
        }
    }
    true
}

fn render_approval_lines(approvals: &[Value], only_pending: bool, out: &mut String) {
    for a in approvals {
        let status = text(a, "status");
        if only_pending && status != "pending" {
            continue;
        }
        let _ = writeln!(
            out,
            "approval {} [{}] {}/{} {} digest {}",
            text_or_dash(a, "id"),
            text_or_dash(a, "purpose"),
            text_or_dash(a, "run"),
            text_or_dash(a, "task"),
            if status.is_empty() { "-" } else { &status },
            prefix(&text_or_dash(a, "digest"), DIGEST_PREFIX)
        );
        if !render_pull_request_action(a, out) {
            let display = text(a, "display");
            if !display.is_empty() {
                let _ = writeln!(out, "  {}", truncate(&display, 200));
            }
        }
        if status == "pending" {
            let _ = writeln!(
                out,
                "  decide: fleet approve {0} | fleet deny {0}",
                text(a, "id")
            );
        }
    }
}

fn render_recent(recent: &[Value], out: &mut String) {
    if recent.is_empty() {
        return;
    }
    out.push_str("recent\n");
    let skip = recent.len().saturating_sub(RECENT_SHOWN);
    for e in recent.iter().skip(skip) {
        let _ = writeln!(
            out,
            "  #{} {}",
            num(e, "seq"),
            truncate(&text(e, "line"), 200)
        );
    }
}

/// `fleet status`.
pub fn render_status(v: &Value) -> String {
    let mut out = String::new();
    if let Some(d) = v.get("daemon") {
        let rss = d
            .get("rss_kb")
            .and_then(Value::as_u64)
            .map_or_else(|| "-".to_string(), |k| format!("{k} kB"));
        let _ = writeln!(
            out,
            "daemon pid {} | v{} | up {} | sessions {}/{} | undo {} | rss {}",
            num(d, "pid"),
            text_or_dash(d, "version"),
            duration_text(num(d, "uptime_ms")),
            num(d, "sessions"),
            num(d, "max_sessions"),
            text_or_dash(d, "undo"),
            rss
        );
    }
    let runs = items(v, "runs");
    if runs.is_empty() {
        out.push_str("no runs yet\n");
    }
    for run in runs {
        out.push('\n');
        render_run(run, &mut out);
    }
    let mut extra = String::new();
    render_gate_lines(items(v, "gates"), true, &mut extra);
    render_approval_lines(items(v, "approvals"), true, &mut extra);
    if !extra.is_empty() {
        out.push_str("\nwaiting for you\n");
        out.push_str(&extra);
    }
    let mut recent = String::new();
    render_recent(items(v, "recent"), &mut recent);
    if !recent.is_empty() {
        out.push('\n');
        out.push_str(&recent);
    }
    out
}

/// `fleet gate list`.
pub fn render_gates(v: &Value) -> String {
    let gates = items(v, "gates");
    if gates.is_empty() {
        return "no decision gates\n".to_string();
    }
    let mut out = String::new();
    render_gate_lines(gates, false, &mut out);
    out
}

/// `fleet approval list`.
pub fn render_approvals(v: &Value) -> String {
    let approvals = items(v, "approvals");
    if approvals.is_empty() {
        return "no approvals\n".to_string();
    }
    let mut out = String::new();
    render_approval_lines(approvals, false, &mut out);
    out
}

/// The text of one `task.tail` reply, sanitized again (the daemon already did it once).
pub fn render_tail(v: &Value) -> String {
    multiline(v.get("text").and_then(Value::as_str).unwrap_or(""))
}

/// `fleet events`.
pub fn render_events(v: &Value) -> String {
    let events = if v.get("events").is_some() {
        items(v, "events")
    } else {
        items(v, "recent")
    };
    if events.is_empty() {
        return "no events\n".to_string();
    }
    let mut out = String::new();
    for e in events {
        let _ = writeln!(
            out,
            "#{} {}",
            num(e, "seq"),
            truncate(&text(e, "line"), 300)
        );
    }
    out
}

/// A free-text note some replies carry (merge message, undo report).
fn note(v: &Value) -> Option<String> {
    ["report", "message", "text", "output", "note"]
        .iter()
        .find_map(|k| v.get(k).and_then(Value::as_str))
        .map(multiline)
        .filter(|s| !s.trim().is_empty())
}

fn with_note(mut line: String, v: &Value) -> String {
    line.push('\n');
    if let Some(n) = note(v) {
        line.push_str(n.trim_end());
        line.push('\n');
    }
    line
}

/// `fleet run create`.
pub fn render_run_created(v: &Value, requested_id: Option<&str>) -> String {
    let id = ["run", "id"]
        .iter()
        .map(|k| text(v, k))
        .find(|s| !s.is_empty())
        .or_else(|| requested_id.map(clean))
        .unwrap_or_default();
    if id.is_empty() {
        "run created\n".to_string()
    } else {
        format!("run {id} created\n")
    }
}

/// `fleet task add`.
pub fn render_task_added(v: &Value, run: &str) -> String {
    let id = ["task", "id"]
        .iter()
        .map(|k| text(v, k))
        .find(|s| !s.is_empty());
    let line = match id {
        Some(id) => format!("task {id} added to run {}", clean(run)),
        None => format!("task added to run {}", clean(run)),
    };
    with_note(line, v)
}

/// `fleet plan`.
pub fn render_plan(v: &Value) -> String {
    let run = ["run", "id"]
        .iter()
        .map(|k| text(v, k))
        .find(|s| !s.is_empty());
    let count = match v.get("tasks") {
        Some(Value::Array(a)) => Some(a.len() as u64),
        Some(t) => t.as_u64(),
        None => None,
    };
    let mut line = String::from("plan loaded");
    if let Some(r) = run {
        let _ = write!(line, " into run {r}");
    }
    if let Some(n) = count {
        let _ = write!(line, " ({n} tasks)");
    }
    if let Some(Value::Array(channels)) = v.get("channels") {
        if !channels.is_empty() {
            let _ = write!(line, " and {} message channel(s)", channels.len());
        }
    }
    with_note(line, v)
}

/// `fleet merge`.
pub fn render_merge(v: &Value) -> String {
    let task = text(v, "task");
    let sha = prefix(&text(v, "merge_sha"), 12);
    let mut line = String::from("merged");
    if !task.is_empty() {
        let _ = write!(line, " {task}");
    }
    let from = text(v, "branch");
    if !from.is_empty() {
        let _ = write!(line, " ({from})");
    }
    let into = text(v, "integration_branch");
    if !into.is_empty() {
        let _ = write!(line, " into {into}");
    }
    if !sha.is_empty() {
        let _ = write!(line, " at {sha}");
    }
    with_note(line, v)
}

/// `fleet cleanup`.
pub fn render_cleanup(v: &Value) -> String {
    let mut line = String::from("cleanup finished");
    for key in ["removed", "branches_deleted"] {
        let n = match v.get(key) {
            Some(Value::Array(a)) => Some(a.len() as u64),
            Some(other) => other.as_u64(),
            None => None,
        };
        if let Some(n) = n {
            let _ = write!(line, ", {key} {n}");
        }
    }
    with_note(line, v)
}

/// `fleet undo`: the tool's report is the content.
pub fn render_undo(v: &Value, apply: bool) -> String {
    let head = if apply {
        "undo applied"
    } else {
        "undo preview (use --apply to revert)"
    };
    with_note(head.to_string(), v)
}

/// Simple acknowledgements (`stop`, `retry`, `send`, ...): the label plus any note.
pub fn render_ack(label: &str, v: &Value) -> String {
    with_note(clean(label), v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Value {
        Value::str(x)
    }

    fn task(id: &str, status: &str, deps: &[&str]) -> Value {
        Value::object([
            ("id", s(id)),
            ("title", s("a title")),
            ("status", s(status)),
            ("deps", Value::array(deps.iter().map(|d| s(d)))),
            ("worker", Value::Null),
            ("attempt", Value::int(1u64)),
            ("block", Value::Null),
            ("failure", Value::Null),
            ("summary", Value::Null),
            ("gates", Value::array([])),
            ("live", Value::Bool(false)),
        ])
    }

    fn status_fixture() -> Value {
        let mut t2 = task("t2", "running", &["t1"]);
        if let Value::Object(m) = &mut t2 {
            m.insert("worker".into(), s("coder"));
            m.insert("live".into(), Value::Bool(true));
        }
        let mut t3 = task("t3", "blocked", &["t1", "t2"]);
        if let Value::Object(m) = &mut t3 {
            m.insert(
                "block".into(),
                Value::object([
                    ("kind", s("approval")),
                    ("subject", s("a1")),
                    ("detail", s("merge needs a decision")),
                ]),
            );
        }
        let mut t1 = task("t1", "completed", &[]);
        if let Value::Object(m) = &mut t1 {
            m.insert("summary".into(), s("added the parser"));
        }
        Value::object([
            ("schema", s("fleet.status/1")),
            (
                "daemon",
                Value::object([
                    ("pid", Value::int(4242u64)),
                    ("version", s("0.1.0")),
                    ("uptime_ms", Value::int(65_000u64)),
                    ("sessions", Value::int(1u64)),
                    ("max_sessions", Value::int(4u64)),
                    ("undo", s("unavailable")),
                    ("rss_kb", Value::Null),
                ]),
            ),
            (
                "runs",
                Value::array([Value::object([
                    ("id", s("r1")),
                    ("objective", s("Ship the parser")),
                    ("repo", s("/work/repo")),
                    ("base_ref", s("main")),
                    ("base_sha", s("0123456789abcdef0123")),
                    ("status", s("active")),
                    ("max_parallel", Value::int(2u64)),
                    ("integration_branch", s("fleet/r1/integration")),
                    (
                        "counts",
                        Value::object([
                            ("pending", Value::int(0u64)),
                            ("ready", Value::int(0u64)),
                            ("running", Value::int(1u64)),
                            ("review", Value::int(0u64)),
                            ("completed", Value::int(1u64)),
                            ("failed", Value::int(0u64)),
                            ("blocked", Value::int(1u64)),
                        ]),
                    ),
                    ("tasks", Value::array([t1, t2, t3])),
                ])]),
            ),
            (
                "gates",
                Value::array([Value::object([
                    ("id", s("g1")),
                    ("run", s("r1")),
                    ("task", s("t2")),
                    ("kind", s("question")),
                    ("text", s("Use sqlite or files?")),
                    ("choices", Value::array([s("sqlite"), s("files")])),
                    ("answer", Value::Null),
                    ("open", Value::Bool(true)),
                ])]),
            ),
            (
                "approvals",
                Value::array([
                    Value::object([
                        ("id", s("a1")),
                        ("run", s("r1")),
                        ("task", s("t3")),
                        ("purpose", s("merge")),
                        ("status", s("pending")),
                        ("display", s("merge fleet/r1/t3 into integration")),
                        ("digest", s("aabbccddeeff00112233445566778899")),
                    ]),
                    Value::object([
                        ("id", s("a0")),
                        ("status", s("consumed")),
                        ("digest", s("ffff")),
                    ]),
                ]),
            ),
            (
                "recent",
                Value::array([Value::object([
                    ("seq", Value::int(7u64)),
                    ("ts_ms", Value::int(1u64)),
                    ("line", s("t2 started")),
                ])]),
            ),
        ])
    }

    #[test]
    fn a_remote_host_is_shown_after_the_worker() {
        let mut t = task("t1", "running", &[]);
        if let Value::Object(m) = &mut t {
            m.insert("worker".into(), s("coder"));
            m.insert("host".into(), s("build1"));
        }
        assert_eq!(worker_cell(&t), "coder host:build1");
        assert_eq!(worker_cell(&task("t2", "running", &[])), "-");
    }

    #[test]
    fn status_golden() {
        let want = "\
daemon pid 4242 | v0.1.0 | up 1m05s | sessions 1/4 | undo unavailable | rss -

run r1 [active] max-parallel 2
  objective  Ship the parser
  repo       /work/repo (base main @ 01234567)
  branch     fleet/r1/integration
  tasks      pending 0, ready 0, running 1, review 0, completed 1, failed 0, blocked 1
  ID  STATUS     DEPS   TRY  WORKER  NOTE
  t1  completed  -      1    -       added the parser
  t2  running    t1     1    coder   (live)
  t3  blocked    t1,t2  1    -       blocked: approval a1 - merge needs a decision

waiting for you
gate g1 [question] r1/t2: Use sqlite or files?
  choices: sqlite | files
approval a1 [merge] r1/t3 pending digest aabbccddeeff
  merge fleet/r1/t3 into integration
  decide: fleet approve a1 | fleet deny a1

recent
  #7 t2 started
";
        assert_eq!(render_status(&status_fixture()), want);
    }

    #[test]
    fn status_without_runs() {
        let v = Value::object([("runs", Value::array([]))]);
        assert_eq!(render_status(&v), "no runs yet\n");
    }

    #[test]
    fn status_survives_missing_and_mistyped_fields() {
        let v = Value::object([
            (
                "runs",
                Value::array([
                    Value::Int(3),
                    Value::Null,
                    Value::object([("tasks", s("x"))]),
                ]),
            ),
            ("gates", Value::Bool(true)),
        ]);
        let out = render_status(&v);
        assert!(out.contains("run - [-]"));
        assert!(out.contains("(no tasks)"));
    }

    #[test]
    fn hostile_reply_text_is_sanitized_everywhere() {
        let evil = "x\u{1b}]52;c;ZXZpbA==\u{7}\u{1b}[2Jy\u{202e}z\nnext\u{0}";
        let mut t = task("t1", "failed", &[]);
        if let Value::Object(m) = &mut t {
            m.insert("failure".into(), s(evil));
            m.insert("id".into(), s(evil));
            m.insert("worker".into(), s(evil));
        }
        let v = Value::object([
            (
                "runs",
                Value::array([Value::object([
                    ("id", s(evil)),
                    ("objective", s(evil)),
                    ("repo", s(evil)),
                    ("tasks", Value::array([t])),
                ])]),
            ),
            (
                "gates",
                Value::array([Value::object([
                    ("id", s(evil)),
                    ("text", s(evil)),
                    ("choices", Value::array([s(evil)])),
                    ("open", Value::Bool(true)),
                ])]),
            ),
            (
                "approvals",
                Value::array([Value::object([
                    ("id", s(evil)),
                    ("status", s("pending")),
                    ("display", s(evil)),
                    ("digest", s(evil)),
                ])]),
            ),
            ("recent", Value::array([Value::object([("line", s(evil))])])),
        ]);
        for out in [
            render_status(&v),
            render_gates(&v),
            render_approvals(&v),
            render_events(&v),
            render_tail(&Value::object([("text", s(evil))])),
            render_merge(&Value::object([("message", s(evil)), ("task", s(evil))])),
            render_undo(&Value::object([("report", s(evil))]), false),
            render_run_created(&Value::object([("run", s(evil))]), None),
            render_ack(evil, &Value::Null),
        ] {
            assert!(
                !out.chars().any(|c| c.is_control() && c != '\n'),
                "control character leaked: {out:?}"
            );
            assert!(!out.contains('\u{202e}'), "bidi override leaked");
        }
    }

    #[test]
    fn a_pull_request_approval_shows_every_argument_whole_and_the_flag_before_the_long_fields() {
        let body = format!(
            "first\n{}\n## not a heading of fleet\nlast line",
            "b".repeat(3000)
        );
        let title = "t".repeat(250);
        let a = Value::object([
            ("id", s("a1")),
            ("purpose", s("pull-request")),
            ("run", s("r1")),
            ("task", s("t1")),
            ("status", s("pending")),
            ("digest", s(&"d".repeat(64))),
            (
                "args",
                Value::array([
                    s("github.com/o/r"),
                    s("main"),
                    s("fleet/r1/t1"),
                    s(&"c".repeat(40)),
                    s(&title),
                    s(&body),
                    s("draft"),
                ]),
            ),
            ("display", s("forge: whatever")),
        ]);
        let out = render_approvals(&Value::object([("approvals", Value::array([a]))]));
        for want in [
            "repository:   github.com/o/r",
            "base:         main",
            "branch:       fleet/r1/t1",
            &format!("commit:       {}", "c".repeat(40)),
            "pull request: draft",
            &format!("title:        {title}"),
            &"b".repeat(3000),
            "last line",
        ] {
            assert!(out.contains(want), "{want}\n{out}");
        }
        assert!(!out.contains('…'), "{out}");
        assert!(
            out.find("pull request: draft") < out.find("title:"),
            "{out}"
        );
        assert!(out.find("pull request: draft") < out.find("body:"), "{out}");
        // every line of the body is quoted: none starts like a heading of fleet's own
        assert!(out.contains("    > ## not a heading of fleet"), "{out}");
        assert!(!out.lines().any(|l| l.starts_with("##")), "{out}");
    }

    #[test]
    fn truncate_by_characters_with_ellipsis() {
        assert_eq!(truncate("abcdef", 6), "abcdef");
        assert_eq!(truncate("abcdefg", 6), "abcde…");
        assert_eq!(truncate("ééééééé", 4), "ééé…");
        assert_eq!(truncate("a\nb", 10), "a b");
        assert_eq!(truncate("abc", 0), "");
    }

    #[test]
    fn table_aligns_by_character_count() {
        let rows = vec![
            vec!["é".to_string(), "x".to_string()],
            vec!["abc".to_string(), "longer cell".to_string()],
        ];
        let t = table(&["A", "B"], &[10, 6], &rows, "");
        assert_eq!(t, "A    B\né    x\nabc  longe…\n");
    }

    #[test]
    fn durations() {
        assert_eq!(duration_text(0), "0s");
        assert_eq!(duration_text(59_999), "59s");
        assert_eq!(duration_text(3_661_000), "1h01m");
        assert_eq!(duration_text(90_000_000), "1d01h");
    }

    #[test]
    fn gates_list_shows_answered_and_open() {
        let v = Value::object([(
            "gates",
            Value::array([
                Value::object([
                    ("id", s("g1")),
                    ("kind", s("conflict")),
                    ("run", s("r1")),
                    ("task", s("t1")),
                    ("text", s("which side?")),
                    ("choices", Value::array([s("ours"), s("theirs")])),
                    ("open", Value::Bool(true)),
                ]),
                Value::object([
                    ("id", s("g2")),
                    ("text", s("q")),
                    ("answer", s("yes")),
                    ("open", Value::Bool(false)),
                ]),
            ]),
        )]);
        let out = render_gates(&v);
        assert!(out.contains("gate g1 [conflict] r1/t1: which side?"));
        assert!(out.contains("choices: ours | theirs"));
        assert!(out.contains("answered: yes"));
        assert_eq!(
            render_gates(&Value::Object(Default::default())),
            "no decision gates\n"
        );
    }

    #[test]
    fn approvals_list_shows_digest_prefix_only() {
        let v = status_fixture();
        let out = render_approvals(&v);
        assert!(out.contains("digest aabbccddeeff\n"), "{out}");
        assert!(!out.contains("aabbccddeeff0011"));
        assert!(out.contains("a0"));
        assert_eq!(render_approvals(&Value::Null), "no approvals\n");
    }

    #[test]
    fn events_fall_back_to_recent() {
        let e = Value::object([(
            "events",
            Value::array([Value::object([
                ("seq", Value::int(3u64)),
                ("line", s("hi")),
            ])]),
        )]);
        assert_eq!(render_events(&e), "#3 hi\n");
        assert_eq!(render_events(&status_fixture()), "#7 t2 started\n");
        assert_eq!(render_events(&Value::Null), "no events\n");
    }

    #[test]
    fn recent_shows_only_the_newest_ten() {
        let recent: Vec<Value> = (0..25u64)
            .map(|i| Value::object([("seq", Value::int(i)), ("line", s("e"))]))
            .collect();
        let out = render_status(&Value::object([("recent", Value::array(recent))]));
        assert!(out.contains("#24 e"));
        assert!(out.contains("#15 e"));
        assert!(!out.contains("#14 e"));
    }

    #[test]
    fn small_renderers() {
        let created = Value::object([("run", s("r9"))]);
        assert_eq!(render_run_created(&created, None), "run r9 created\n");
        assert_eq!(
            render_run_created(&Value::Null, Some("mine")),
            "run mine created\n"
        );
        assert_eq!(render_run_created(&Value::Null, None), "run created\n");
        let plan = Value::object([("run", s("r1")), ("tasks", Value::array([s("a"), s("b")]))]);
        assert_eq!(render_plan(&plan), "plan loaded into run r1 (2 tasks)\n");
        let with_channels = Value::object([
            ("run", s("r1")),
            ("tasks", Value::array([s("a"), s("b")])),
            ("channels", Value::array([s("talk")])),
        ]);
        assert_eq!(
            render_plan(&with_channels),
            "plan loaded into run r1 (2 tasks) and 1 message channel(s)\n"
        );
        let merge = Value::object([
            ("task", s("r1/t1")),
            ("merge_sha", s("0123456789abcdef0123456789")),
            ("branch", s("fleet/r1/t1")),
            ("integration_branch", s("fleet/r1/integration")),
            ("message", s("fast-forward\nclean")),
        ]);
        assert_eq!(
            render_merge(&merge),
            "merged r1/t1 (fleet/r1/t1) into fleet/r1/integration at 0123456789ab\nfast-forward\nclean\n"
        );
        let cleanup = Value::object([
            ("removed", Value::array([s("w1")])),
            ("branches_deleted", Value::array([s("b1"), s("b2")])),
        ]);
        assert_eq!(
            render_cleanup(&cleanup),
            "cleanup finished, removed 1, branches_deleted 2\n"
        );
        assert!(render_undo(&Value::object([("report", s("1 file"))]), true).ends_with("1 file\n"));
        assert_eq!(render_ack("stopped", &Value::Null), "stopped\n");
    }

    #[test]
    fn failed_gates_are_listed_in_the_note() {
        let mut t = task("t1", "review", &[]);
        if let Value::Object(m) = &mut t {
            m.insert(
                "gates".into(),
                Value::array([
                    Value::object([("name", s("fmt")), ("passed", Value::Bool(true))]),
                    Value::object([("name", s("tests")), ("passed", Value::Bool(false))]),
                ]),
            );
        }
        assert_eq!(task_note(&t), "[gate failed: tests]");
    }
}
