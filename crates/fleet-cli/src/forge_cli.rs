//! The forge commands of the command line: `fleet github status`, `fleet issue ...` and
//! `fleet pr ...`. They talk to the daemon, which runs `gh` (the CLI never does): a command that
//! needs GitHub answers with a ticket, and this module asks for the outcome until it is there.
//!
//! Every text in a reply was written by someone on GitHub (or, for the pull request, by an
//! operator) and passes through the terminal-safe helpers of [`crate::render`] before it is
//! printed. Bodies are printed quoted, under a line that says they are untrusted.

use crate::args::Parsed;
use crate::commands::Caller;
use crate::render::{clean, items, multiline, num, table, text, text_or_dash, truncate};
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::ids::show;
use fleet_domain::value::Value;
use fleet_forge::limits::MAX_PR_BODY;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

/// How often the CLI asks whether a request finished.
pub const POLL: Duration = Duration::from_millis(100);
/// How long it waits by default: a `gh` call may take 30 s, a push two minutes.
pub const DEFAULT_TIMEOUT_SECS: u64 = 240;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForgeView {
    Status,
    IssueList,
    IssueShow,
    ToTask,
    PrList,
    PrShow,
    PrStatus,
    PrCreate,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ForgeCmd {
    pub cmd: &'static str,
    pub args: Value,
    pub view: ForgeView,
    pub timeout_secs: u64,
}

fn obj(pairs: Vec<(&'static str, Option<Value>)>) -> Value {
    Value::object(pairs.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))))
}

fn st(s: String) -> Option<Value> {
    Some(Value::Str(s))
}

fn number(p: Parsed, what: &str) -> Result<(u64, Vec<String>)> {
    let mut words = p.finish()?.into_iter();
    let n = words
        .next()
        .ok_or_else(|| FleetError::usage(format!("missing {what}")))?;
    let n = n
        .trim_start_matches('#')
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            FleetError::usage(format!(
                "{what} must be a positive number, not {}",
                show(&n)
            ))
        })?;
    Ok((n, words.collect()))
}

fn uint(p: &mut Parsed, name: &str) -> Result<Option<u64>> {
    p.opt(name)?
        .map(|v| {
            v.parse::<u64>()
                .map_err(|_| FleetError::usage(format!("option {name} needs a whole number")))
        })
        .transpose()
}

fn timeout(p: &mut Parsed) -> Result<u64> {
    match uint(p, "--timeout")? {
        Some(0) => Err(FleetError::usage("option --timeout must be at least 1")),
        Some(n) => Ok(n),
        None => Ok(DEFAULT_TIMEOUT_SECS),
    }
}

/// The body given on the command line, or on stdin (bounded) for `-`.
fn read_body(value: String, stdin: &mut dyn Read) -> Result<String> {
    if value != "-" {
        return Ok(value);
    }
    let mut buf = Vec::new();
    Read::take(&mut *stdin, MAX_PR_BODY as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| FleetError::io("reading the body from stdin", &e))?;
    if buf.len() > MAX_PR_BODY {
        return Err(FleetError::limit(format!(
            "the body on stdin is longer than {MAX_PR_BODY} bytes"
        )));
    }
    String::from_utf8(buf).map_err(|_| FleetError::invalid("the body on stdin is not valid UTF-8"))
}

fn no_more(rest: Vec<String>) -> Result<()> {
    match rest.first() {
        None => Ok(()),
        Some(extra) => Err(FleetError::usage(format!(
            "unexpected argument {}",
            show(extra)
        ))),
    }
}

fn list(mut p: Parsed, cmd: &'static str, view: ForgeView) -> Result<ForgeCmd> {
    let run = p.opt("--run")?;
    let state = p.opt("--state")?;
    let page = uint(&mut p, "--page")?;
    let limit = uint(&mut p, "--limit")?;
    let timeout_secs = timeout(&mut p)?;
    no_more(p.finish()?)?;
    Ok(ForgeCmd {
        cmd,
        args: obj(vec![
            ("run", run.and_then(st)),
            ("state", state.and_then(st)),
            ("page", page.map(Value::int)),
            ("limit", limit.map(Value::int)),
        ]),
        view,
        timeout_secs,
    })
}

fn by_number(mut p: Parsed, cmd: &'static str, view: ForgeView) -> Result<ForgeCmd> {
    let run = p.opt("--run")?;
    let timeout_secs = timeout(&mut p)?;
    let (n, rest) = number(p, "<number>")?;
    no_more(rest)?;
    Ok(ForgeCmd {
        cmd,
        args: obj(vec![
            ("run", run.and_then(st)),
            ("number", Some(Value::int(n))),
        ]),
        view,
        timeout_secs,
    })
}

/// Parses `github ...`, `issue ...` and `pr ...`.
pub fn parse(word: &str, mut p: Parsed, stdin: &mut dyn Read) -> Result<ForgeCmd> {
    let sub = p.take_word();
    let unknown = |what: &str, got: Option<&str>, expected: &str| {
        Err(match got {
            None => FleetError::usage(format!("`fleet {what}` needs a subcommand: {expected}")),
            Some(g) => FleetError::usage(format!(
                "unknown {what} subcommand {}: use {expected}",
                show(g)
            )),
        })
    };
    match (word, sub.as_deref()) {
        ("github", Some("status")) => {
            no_more(p.finish()?)?;
            Ok(ForgeCmd {
                cmd: "forge.status",
                args: obj(vec![]),
                view: ForgeView::Status,
                timeout_secs: DEFAULT_TIMEOUT_SECS,
            })
        }
        ("github", other) => unknown("github", other, "status"),
        ("issue", Some("list")) => list(p, "issue.list", ForgeView::IssueList),
        ("issue", Some("show")) => by_number(p, "issue.show", ForgeView::IssueShow),
        ("issue", Some("to-task")) => {
            let run = p.opt("--run")?;
            let id = p.opt("--id")?;
            let after = p.multi("--after")?;
            let worker = p.opt("--worker")?;
            let timeout_secs = timeout(&mut p)?;
            let (n, rest) = number(p, "<number>")?;
            no_more(rest)?;
            Ok(ForgeCmd {
                cmd: "issue.to-task",
                args: obj(vec![
                    ("run", run.and_then(st)),
                    ("number", Some(Value::int(n))),
                    ("id", id.and_then(st)),
                    (
                        "after",
                        Some(Value::array(after.into_iter().map(Value::Str))),
                    ),
                    ("worker", worker.and_then(st)),
                ]),
                view: ForgeView::ToTask,
                timeout_secs,
            })
        }
        ("issue", other) => unknown("issue", other, "list, show or to-task"),
        ("pr", Some("list")) => list(p, "pr.list", ForgeView::PrList),
        ("pr", Some("show")) => by_number(p, "pr.show", ForgeView::PrShow),
        ("pr", Some("status")) => by_number(p, "pr.status", ForgeView::PrStatus),
        ("pr", Some("create")) => {
            let task = p
                .opt("--task")?
                .ok_or_else(|| FleetError::usage("--task is required"))?;
            let title = p.opt("--title")?;
            let body = p.opt("--body")?.map(|b| read_body(b, stdin)).transpose()?;
            let base = p.opt("--base")?;
            let draft = p.flag("--draft");
            let timeout_secs = timeout(&mut p)?;
            no_more(p.finish()?)?;
            Ok(ForgeCmd {
                cmd: "pr.create",
                args: obj(vec![
                    ("task", st(task)),
                    ("title", title.and_then(st)),
                    ("body", body.and_then(st)),
                    ("base", base.and_then(st)),
                    ("draft", draft.then_some(Value::Bool(true))),
                ]),
                view: ForgeView::PrCreate,
                timeout_secs,
            })
        }
        ("pr", other) => unknown("pr", other, "list, show, status or create"),
        _ => Err(FleetError::usage(format!("unknown command {}", show(word)))),
    }
}

/// Runs a forge command: one request, then (for the ones that need GitHub) asks for the outcome of
/// the ticket every `poll` for at most `timeout_secs`. An approval that is needed first ends the
/// command with the daemon's error (exit 5), like `fleet merge`.
pub fn execute(
    cmd: &ForgeCmd,
    caller: &mut dyn Caller,
    json: bool,
    out: &mut dyn Write,
    poll: Duration,
) -> Result<()> {
    let first = caller.call(cmd.cmd, cmd.args.clone())?;
    let doc = if cmd.view == ForgeView::Status {
        first
    } else {
        let ticket = first
            .get("ticket")
            .and_then(Value::as_u64)
            .ok_or_else(|| FleetError::internal("the daemon's reply has no ticket"))?;
        let deadline = Instant::now().checked_add(Duration::from_secs(cmd.timeout_secs));
        loop {
            let r = caller.call(
                "forge.result",
                obj(vec![("ticket", Some(Value::int(ticket)))]),
            )?;
            if r.get("state").and_then(Value::as_str) == Some("done") {
                break r;
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(FleetError::new(
                    ErrorCode::Timeout,
                    format!(
                        "GitHub did not answer within {} seconds; the request keeps running in the daemon",
                        cmd.timeout_secs
                    ),
                ));
            }
            if !poll.is_zero() {
                std::thread::sleep(poll);
            }
        }
    };
    let mut text = if json {
        let mut s = doc.canonical();
        s.push('\n');
        s
    } else {
        let data = doc.get("data").unwrap_or(&doc);
        match cmd.view {
            ForgeView::Status => render_status(&doc),
            ForgeView::IssueList => render_issue_list(data),
            ForgeView::IssueShow => render_issue_show(data),
            ForgeView::ToTask => render_to_task(data),
            ForgeView::PrList => render_pull_list(data),
            ForgeView::PrShow => render_pull_show(data),
            ForgeView::PrStatus => render_pull_status(data),
            ForgeView::PrCreate => render_created(data),
        }
    };
    if !text.ends_with('\n') {
        text.push('\n');
    }
    out.write_all(text.as_bytes())
        .map_err(|e| FleetError::io("writing output", &e))
}

// ---- rendering -------------------------------------------------------------------------------

fn join(v: &Value, key: &str) -> String {
    let all: Vec<String> = items(v, key)
        .iter()
        .filter_map(|l| l.as_str().map(clean))
        .collect();
    if all.is_empty() {
        "-".to_string()
    } else {
        all.join(", ")
    }
}

fn quoted(body: &str) -> String {
    let body = multiline(body);
    if body.trim().is_empty() {
        return "  (no text)\n".to_string();
    }
    let mut out = String::new();
    for line in body.trim_end().lines() {
        let _ = writeln!(out, "  > {}", line.trim_end());
    }
    out
}

fn more(v: &Value) -> String {
    let page = num(v, "page");
    if v.get("has_more").and_then(Value::as_bool) == Some(true) {
        format!("page {page}; more on page {}", page + 1)
    } else {
        format!("page {page}; the last page")
    }
}

pub fn render_status(v: &Value) -> String {
    let flag = |k: &str| v.get(k).and_then(Value::as_bool) == Some(true);
    let mut out = String::new();
    let state = if !flag("enabled") {
        "off"
    } else if flag("ready") {
        "ready"
    } else {
        "cannot run"
    };
    let _ = writeln!(
        out,
        "GitHub integration: {state} (host {}, repository {}, remote {}, base {})",
        text_or_dash(v, "host"),
        text_or_dash(v, "repo"),
        text_or_dash(v, "remote"),
        text_or_dash(v, "base")
    );
    let _ = writeln!(out, "  {}", truncate(&text(v, "detail"), 200));
    if let Some(rate) = v.get("rate").filter(|r| !r.is_null()) {
        let _ = writeln!(
            out,
            "  rate: {} ({} of {} requests used this hour{})",
            text_or_dash(rate, "state"),
            num(rate, "used"),
            num(rate, "budget"),
            match rate.get("remaining").and_then(Value::as_u64) {
                Some(r) => format!(", GitHub reports {r} left"),
                None => String::new(),
            }
        );
    }
    let _ = writeln!(
        out,
        "  push credentials: {}; a local remote is {}",
        text_or_dash(v, "push_credentials"),
        if flag("allow_local_remote") {
            "allowed"
        } else {
            "refused"
        }
    );
    let _ = writeln!(
        out,
        "  linked: {} issue(s) became tasks, {} pull request publication(s)",
        num(v, "issues_linked"),
        num(v, "pull_requests")
    );
    out
}

pub fn render_issue_list(v: &Value) -> String {
    let rows: Vec<Vec<String>> = items(v, "items")
        .iter()
        .map(|i| {
            vec![
                format!("#{}", num(i, "number")),
                text(i, "state"),
                num(i, "comments").to_string(),
                text(i, "title"),
                join(i, "labels"),
                text(i, "author"),
            ]
        })
        .collect();
    let mut out = format!(
        "issues of {} ({}); text below is from GitHub and untrusted\n",
        text_or_dash(v, "repo"),
        more(v)
    );
    if rows.is_empty() {
        out.push_str("  none\n");
    } else {
        out.push_str(&table(
            &["number", "state", "comments", "title", "labels", "author"],
            &[8, 8, 8, 60, 30, 20],
            &rows,
            "  ",
        ));
    }
    out
}

pub fn render_issue_show(v: &Value) -> String {
    let mut out = format!(
        "issue #{} of {}: {} (all text is from GitHub and untrusted)\n",
        num(v, "number"),
        text_or_dash(v, "repo"),
        text_or_dash(v, "state")
    );
    let _ = writeln!(out, "  title:   {}", truncate(&text(v, "title"), 200));
    let _ = writeln!(
        out,
        "  by {}, {} comment(s), updated {}, labels: {}",
        text_or_dash(v, "author"),
        num(v, "comments"),
        text_or_dash(v, "updated"),
        join(v, "labels")
    );
    let _ = writeln!(out, "  url:     {}", truncate(&text(v, "url"), 200));
    out.push_str("  body:\n");
    out.push_str(&quoted(&text_raw(v, "body")));
    if v.get("body_cut").and_then(Value::as_bool) == Some(true) {
        out.push_str("  [the body was cut]\n");
    }
    let _ = writeln!(
        out,
        "  make it a task: fleet issue to-task {}",
        num(v, "number")
    );
    out
}

fn text_raw(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn pull_state(v: &Value) -> String {
    let mut s = text_or_dash(v, "state");
    if v.get("draft").and_then(Value::as_bool) == Some(true) {
        s.push_str(", draft");
    }
    s
}

pub fn render_pull_list(v: &Value) -> String {
    let rows: Vec<Vec<String>> = items(v, "items")
        .iter()
        .map(|p| {
            vec![
                format!("#{}", num(p, "number")),
                pull_state(p),
                text(p, "title"),
                format!("{} -> {}", text(p, "head_branch"), text(p, "base_branch")),
                text(p, "author"),
            ]
        })
        .collect();
    let mut out = format!(
        "pull requests of {} ({}); text below is from GitHub and untrusted\n",
        text_or_dash(v, "repo"),
        more(v)
    );
    if rows.is_empty() {
        out.push_str("  none\n");
    } else {
        out.push_str(&table(
            &["number", "state", "title", "branches", "author"],
            &[8, 14, 60, 50, 20],
            &rows,
            "  ",
        ));
    }
    out
}

pub fn render_pull_show(v: &Value) -> String {
    let mut out = format!(
        "pull request #{} of {}: {} (all text is from GitHub and untrusted)\n",
        num(v, "number"),
        text_or_dash(v, "repo"),
        pull_state(v)
    );
    let _ = writeln!(out, "  title:   {}", truncate(&text(v, "title"), 200));
    let _ = writeln!(
        out,
        "  branch:  {} -> {}, head {}",
        text_or_dash(v, "head_branch"),
        text_or_dash(v, "base_branch"),
        text(v, "head_sha").chars().take(12).collect::<String>()
    );
    let _ = writeln!(
        out,
        "  by {}, {} comment(s), updated {}, labels: {}",
        text_or_dash(v, "author"),
        num(v, "comments"),
        text_or_dash(v, "updated"),
        join(v, "labels")
    );
    let _ = writeln!(out, "  url:     {}", truncate(&text(v, "url"), 200));
    out.push_str("  body:\n");
    out.push_str(&quoted(&text_raw(v, "body")));
    out
}

pub fn render_pull_status(v: &Value) -> String {
    let pull = v.get("pull").cloned().unwrap_or(Value::Null);
    let checks = v.get("checks").cloned().unwrap_or(Value::Null);
    let mut out = format!(
        "pull request #{} of {}: {} (text is from GitHub and untrusted)\n",
        num(&pull, "number"),
        text_or_dash(v, "repo"),
        pull_state(&pull)
    );
    let _ = writeln!(out, "  title:   {}", truncate(&text(&pull, "title"), 200));
    let _ = writeln!(
        out,
        "  checks:  {} ({} passed, {} failed, {} pending of {})",
        text_or_dash(&checks, "state"),
        num(&checks, "passed"),
        num(&checks, "failed"),
        num(&checks, "pending"),
        num(&checks, "total")
    );
    let reviews: Vec<String> = items(v, "reviews")
        .iter()
        .map(|r| format!("{} {}", text(r, "author"), text(r, "state")))
        .collect();
    let _ = writeln!(
        out,
        "  reviews: {} ({})",
        text_or_dash(v, "decision"),
        if reviews.is_empty() {
            "none yet".to_string()
        } else {
            reviews.join(", ")
        }
    );
    out
}

pub fn render_to_task(v: &Value) -> String {
    format!(
        "task {} was created from issue #{} of {}; its text is quoted and labelled untrusted in the brief\n",
        text_or_dash(v, "task"),
        num(v, "issue"),
        text_or_dash(v, "repo")
    )
}

pub fn render_created(v: &Value) -> String {
    format!(
        "pull request #{} opened for {} on {}: {}\n",
        num(v, "number"),
        text_or_dash(v, "task"),
        text_or_dash(v, "repo"),
        text_or_dash(v, "url")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{parse_cli, Command};
    use std::collections::VecDeque;
    use std::io::Cursor;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn parse_with(a: &[&str], stdin: &str) -> Result<ForgeCmd> {
        let cli = parse_cli(&argv(a), &mut Cursor::new(stdin.as_bytes().to_vec()))?;
        match cli.command {
            Command::Forge(c) => Ok(c),
            other => panic!("not a forge command: {other:?}"),
        }
    }

    fn parse_ok(a: &[&str]) -> ForgeCmd {
        parse_with(a, "").unwrap()
    }

    fn s(x: &str) -> Value {
        Value::str(x)
    }

    #[test]
    fn every_forge_command_parses() {
        let c = parse_ok(&["github", "status"]);
        assert_eq!((c.cmd, c.view), ("forge.status", ForgeView::Status));
        let c = parse_ok(&[
            "issue", "list", "--state", "all", "--page", "2", "--limit", "5", "--run", "r1",
        ]);
        assert_eq!(c.cmd, "issue.list");
        assert_eq!(
            c.args,
            Value::object([
                ("run", s("r1")),
                ("state", s("all")),
                ("page", Value::int(2u64)),
                ("limit", Value::int(5u64))
            ])
        );
        let c = parse_ok(&["issue", "show", "#7"]);
        assert_eq!(
            (c.cmd, c.args),
            ("issue.show", Value::object([("number", Value::int(7u64))]))
        );
        let c = parse_ok(&[
            "issue", "to-task", "7", "--run", "r1", "--id", "fix", "--after", "a", "--after", "b",
            "--worker", "agent",
        ]);
        assert_eq!(c.cmd, "issue.to-task");
        assert_eq!(c.args.get("after"), Some(&Value::array([s("a"), s("b")])));
        assert_eq!(c.args.get("id"), Some(&s("fix")));
        let c = parse_ok(&["pr", "list"]);
        assert_eq!((c.cmd, c.view), ("pr.list", ForgeView::PrList));
        assert_eq!(parse_ok(&["pr", "show", "9"]).cmd, "pr.show");
        assert_eq!(
            parse_ok(&["pr", "status", "9", "--timeout", "5"]).timeout_secs,
            5
        );
        let c = parse_ok(&[
            "pr", "create", "--task", "r1/a", "--title", "T", "--body", "B", "--base", "develop",
            "--draft",
        ]);
        assert_eq!(
            c.args,
            Value::object([
                ("task", s("r1/a")),
                ("title", s("T")),
                ("body", s("B")),
                ("base", s("develop")),
                ("draft", Value::Bool(true)),
            ])
        );
        let c = parse_ok(&["pr", "create", "--task", "a"]);
        assert_eq!(
            c.args,
            Value::object([("task", s("a"))]),
            "nothing the person did not say is sent"
        );
        assert_eq!(c.timeout_secs, DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn the_body_can_come_from_stdin_within_the_limit() {
        let c = parse_with(
            &["pr", "create", "--task", "a", "--body", "-"],
            "from stdin\n",
        )
        .unwrap();
        assert_eq!(c.args.get("body"), Some(&s("from stdin\n")));
        let big = "x".repeat(MAX_PR_BODY + 1);
        let err = parse_with(&["pr", "create", "--task", "a", "--body", "-"], &big).unwrap_err();
        assert_eq!(err.code, ErrorCode::Limit);
    }

    #[test]
    fn usage_errors_name_the_problem() {
        for (a, want) in [
            (vec!["github"], "needs a subcommand"),
            (vec!["github", "login"], "unknown github subcommand"),
            (vec!["issue"], "needs a subcommand"),
            (vec!["issue", "close", "1"], "unknown issue subcommand"),
            (vec!["issue", "show"], "missing <number>"),
            (vec!["issue", "show", "abc"], "positive number"),
            (vec!["issue", "show", "0"], "positive number"),
            (vec!["issue", "show", "1", "2"], "unexpected argument"),
            (vec!["issue", "list", "7"], "unexpected argument"),
            (vec!["issue", "list", "--page", "x"], "whole number"),
            (vec!["issue", "to-task"], "missing <number>"),
            (vec!["pr", "comment", "1"], "unknown pr subcommand"),
            (vec!["pr", "create"], "--task is required"),
            (
                vec!["pr", "create", "--task", "a", "extra"],
                "unexpected argument",
            ),
            (
                vec!["pr", "create", "--task", "a", "--timeout", "0"],
                "at least 1",
            ),
            (vec!["pr", "create", "--task", "a", "--force"], "force"),
        ] {
            let e = parse_with(&a, "").unwrap_err();
            assert_eq!(e.code, ErrorCode::Usage, "{a:?}: {e:?}");
            assert!(e.message.contains(want), "{a:?}: {}", e.message);
        }
    }

    struct Fake {
        replies: VecDeque<Result<Value>>,
        calls: Vec<(String, Value)>,
    }

    impl Caller for Fake {
        fn call(&mut self, cmd: &str, args: Value) -> Result<Value> {
            self.calls.push((cmd.to_string(), args));
            self.replies.pop_front().expect("a scripted reply")
        }
    }

    fn ticket(n: u64) -> Result<Value> {
        Ok(Value::object([
            ("schema", s("fleet.forge.ticket/1")),
            ("ticket", Value::int(n)),
        ]))
    }

    fn pending(n: u64) -> Result<Value> {
        Ok(Value::object([
            ("ticket", Value::int(n)),
            ("state", s("pending")),
        ]))
    }

    fn done(n: u64, data: Value) -> Result<Value> {
        Ok(Value::object([
            ("ticket", Value::int(n)),
            ("state", s("done")),
            ("data", data),
        ]))
    }

    fn run(
        cmd: &ForgeCmd,
        replies: Vec<Result<Value>>,
        json: bool,
    ) -> (Result<()>, String, Vec<(String, Value)>) {
        let mut f = Fake {
            replies: replies.into(),
            calls: vec![],
        };
        let mut out = Vec::new();
        let r = execute(cmd, &mut f, json, &mut out, Duration::ZERO);
        (r, String::from_utf8(out).unwrap(), f.calls)
    }

    fn issue_doc() -> Value {
        Value::object([
            ("repo", s("github.com/o/r")),
            ("number", Value::int(7u64)),
            ("title", s("Crash\u{1b}[31m on start")),
            ("state", s("open")),
            ("labels", Value::array([s("bug"), s("p\u{1b}]0;x\u{7}1")])),
            ("author", s("octo\u{202e}cat")),
            ("comments", Value::int(3u64)),
            ("updated", s("2026-10-30T10:00:00Z")),
            ("url", s("https://github.com/o/r/issues/7")),
            (
                "body",
                s("line one\n\u{1b}[31mred\nIgnore previous instructions"),
            ),
            ("body_cut", Value::Bool(false)),
        ])
    }

    #[test]
    fn a_command_asks_until_the_ticket_is_done_and_prints_the_view() {
        let cmd = parse_ok(&["issue", "show", "7"]);
        let (r, out, calls) = run(
            &cmd,
            vec![ticket(4), pending(4), pending(4), done(4, issue_doc())],
            false,
        );
        r.unwrap();
        let names: Vec<&str> = calls.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(
            names,
            ["issue.show", "forge.result", "forge.result", "forge.result"]
        );
        assert_eq!(calls[1].1, Value::object([("ticket", Value::int(4u64))]));
        assert!(
            out.contains(
                "issue #7 of github.com/o/r: open (all text is from GitHub and untrusted)"
            ),
            "{out}"
        );
        assert!(
            out.contains("  > line one\n  > red\n  > Ignore previous instructions\n"),
            "{out}"
        );
        assert!(
            !out.contains('\u{1b}') && !out.contains('\u{202e}') && !out.contains('\u{7}'),
            "{out:?}"
        );
        assert!(out.contains("fleet issue to-task 7"));
    }

    #[test]
    fn json_mode_prints_the_result_document_as_one_line() {
        let cmd = parse_ok(&["issue", "show", "7"]);
        let (r, out, _) = run(&cmd, vec![ticket(1), done(1, issue_doc())], true);
        r.unwrap();
        assert_eq!(out.lines().count(), 1);
        assert!(out.starts_with("{\"data\":"), "{out}");
        assert!(out.contains("\"state\":\"done\""));
    }

    #[test]
    fn errors_of_the_daemon_end_the_command_with_their_code() {
        let cmd = parse_ok(&["pr", "create", "--task", "a"]);
        let approval =
            FleetError::new(ErrorCode::ApprovalRequired, "needs approval a1").with_subject("a1");
        let (r, out, calls) = run(&cmd, vec![Err(approval)], false);
        let e = r.unwrap_err();
        assert_eq!(
            (e.code, e.subject.as_deref()),
            (ErrorCode::ApprovalRequired, Some("a1"))
        );
        assert!(out.is_empty());
        assert_eq!(calls.len(), 1);
        // a failed ticket is the error of the result call
        let (r, _, _) = run(
            &cmd,
            vec![
                ticket(2),
                Err(FleetError::unavailable("GitHub: not signed in")),
            ],
            false,
        );
        assert_eq!(r.unwrap_err().code, ErrorCode::Unavailable);
        // a reply without a ticket is not understood
        let (r, _, _) = run(&cmd, vec![Ok(Value::object([("schema", s("x"))]))], false);
        assert_eq!(r.unwrap_err().code, ErrorCode::Internal);
    }

    #[test]
    fn waiting_has_a_limit_and_the_request_keeps_running_in_the_daemon() {
        let mut cmd = parse_ok(&["issue", "list"]);
        cmd.timeout_secs = 0;
        let (r, _, _) = run(&cmd, vec![ticket(1), pending(1)], false);
        let e = r.unwrap_err();
        assert_eq!(e.code, ErrorCode::Timeout);
        assert!(e.message.contains("keeps running in the daemon"));
    }

    #[test]
    fn status_does_not_wait_for_a_ticket() {
        let cmd = parse_ok(&["github", "status"]);
        let doc = Value::object([
            ("enabled", Value::Bool(true)),
            ("ready", Value::Bool(false)),
            ("host", s("github.com")),
            ("repo", s("o/r")),
            ("remote", s("origin")),
            ("base", s("main")),
            ("detail", s("gh was not found\u{1b}[0m")),
            (
                "rate",
                Value::object([
                    ("state", s("limited")),
                    ("used", Value::int(3u64)),
                    ("budget", Value::int(300u64)),
                    ("remaining", Value::int(0u64)),
                ]),
            ),
            ("push_credentials", s("none")),
            ("allow_local_remote", Value::Bool(false)),
            ("issues_linked", Value::int(2u64)),
            ("pull_requests", Value::int(1u64)),
        ]);
        let (r, out, calls) = run(&cmd, vec![Ok(doc)], false);
        r.unwrap();
        assert_eq!(calls.len(), 1);
        assert!(
            out.starts_with("GitHub integration: cannot run (host github.com, repository o/r"),
            "{out}"
        );
        assert!(
            out.contains("rate: limited (3 of 300 requests used this hour, GitHub reports 0 left)"),
            "{out}"
        );
        assert!(out.contains("a local remote is refused"));
        assert!(!out.contains('\u{1b}'));
        let off = render_status(&Value::object([("enabled", Value::Bool(false))]));
        assert!(off.starts_with("GitHub integration: off"), "{off}");
    }

    #[test]
    fn lists_and_pull_requests_are_tables_and_cards() {
        let page = Value::object([
            ("repo", s("github.com/o/r")),
            ("page", Value::int(2u64)),
            ("has_more", Value::Bool(true)),
            (
                "items",
                Value::array([Value::object([
                    ("number", Value::int(9u64)),
                    ("state", s("open")),
                    ("draft", Value::Bool(true)),
                    ("title", s("Fix it")),
                    ("head_branch", s("fleet/r1/a")),
                    ("base_branch", s("main")),
                    ("author", s("octocat")),
                    ("labels", Value::array([])),
                    ("comments", Value::int(0u64)),
                ])]),
            ),
        ]);
        let t = render_pull_list(&page);
        assert!(t.contains("page 2; more on page 3"), "{t}");
        assert!(
            t.contains("#9") && t.contains("open, draft") && t.contains("fleet/r1/a -> main"),
            "{t}"
        );
        let empty = render_issue_list(&Value::object([
            ("repo", s("github.com/o/r")),
            ("page", Value::int(1u64)),
            ("has_more", Value::Bool(false)),
            ("items", Value::array([])),
        ]));
        assert!(
            empty.contains("the last page") && empty.contains("none"),
            "{empty}"
        );
        let status = Value::object([
            ("repo", s("github.com/o/r")),
            (
                "pull",
                Value::object([
                    ("number", Value::int(9u64)),
                    ("state", s("open")),
                    ("title", s("Fix it")),
                ]),
            ),
            (
                "checks",
                Value::object([
                    ("state", s("failing")),
                    ("total", Value::int(3u64)),
                    ("passed", Value::int(2u64)),
                    ("failed", Value::int(1u64)),
                    ("pending", Value::int(0u64)),
                ]),
            ),
            (
                "reviews",
                Value::array([Value::object([
                    ("author", s("ann")),
                    ("state", s("changes_requested")),
                ])]),
            ),
            ("decision", s("changes_requested")),
        ]);
        let t = render_pull_status(&status);
        assert!(
            t.contains("checks:  failing (2 passed, 1 failed, 0 pending of 3)"),
            "{t}"
        );
        assert!(
            t.contains("reviews: changes_requested (ann changes_requested)"),
            "{t}"
        );
        let t = render_created(&Value::object([
            ("number", Value::int(12u64)),
            ("task", s("r1/a")),
            ("repo", s("github.com/o/r")),
            ("url", s("https://github.com/o/r/pull/12")),
        ]));
        assert_eq!(
            t,
            "pull request #12 opened for r1/a on github.com/o/r: https://github.com/o/r/pull/12\n"
        );
        let t = render_to_task(&Value::object([
            ("task", s("r1/t1")),
            ("issue", Value::int(7u64)),
            ("repo", s("github.com/o/r")),
        ]));
        assert!(
            t.contains("task r1/t1 was created from issue #7") && t.contains("untrusted"),
            "{t}"
        );
        let show = render_pull_show(&Value::object([
            ("number", Value::int(9u64)),
            ("repo", s("github.com/o/r")),
            ("state", s("merged")),
            ("title", s("T")),
            ("head_branch", s("b")),
            ("base_branch", s("main")),
            ("head_sha", s("a".repeat(40).as_str())),
            ("body", s("")),
        ]));
        assert!(
            show.contains("head aaaaaaaaaaaa") && show.contains("(no text)"),
            "{show}"
        );
    }
}
