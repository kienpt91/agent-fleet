//! The `fleet` command line: parsing into [`Command`]s, running them against a daemon, and
//! turning every outcome into output and an exit code.
//!
//! Parsing and rendering are pure; the only things that touch the world are [`run`] (stdio,
//! the socket) and the commands that talk to the daemon, which see it through the [`Caller`]
//! trait so tests can script replies and assert the exact requests.

use crate::args::Parsed;
use crate::doctor::{self, DoctorInput};
use crate::render;
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::ids::show;
use fleet_domain::limits::{MAX_ID_LEN, MAX_SPEC};
use fleet_domain::value::Value;
use fleet_infra::client::Client;
use fleet_infra::config_file::load_plan;
use fleet_infra::daemon::{run_daemon, DaemonOptions};
use fleet_infra::eventlog::verify_file;
use fleet_infra::statedir::{resolve_state_dir, StateLayout};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Every subcommand, as typed. The help text must mention each (a test enforces it).
pub const SUBCOMMANDS: [&str; 25] = [
    "daemon",
    "run create",
    "task add",
    "plan",
    "start",
    "status",
    "attach",
    "send",
    "tail",
    "stop",
    "retry",
    "cancel",
    "gate list",
    "gate resolve",
    "approval list",
    "approve",
    "deny",
    "merge",
    "cleanup",
    "undo",
    "events",
    "shutdown",
    "tui",
    "doctor",
    "verify-log",
];

pub const HELP: &str = "\
fleet - run many coding agents in parallel, each in its own git worktree

usage: fleet [global options] <command> [options]

global options (before or after the command):
  --state-dir <dir>   state directory (default: $XDG_STATE_HOME/agent-fleet or ~/.local/state/agent-fleet)
  --json              print the daemon's reply (or the error) as one JSON document
  -h, --help          show this help
  -V, --version       show the version

daemon:
  daemon [--config <fleet.toml>]     run the daemon in the foreground
  shutdown                           ask the daemon to stop
  doctor [--config <fleet.toml>]     check the setup (exit 13 when something fails)
  verify-log                         verify the event log's hash chain (exit 11 when tampered)

runs and tasks:
  run create --objective <text> --repo <path> [--base <ref>] [--id <id>] [--max-parallel N]
  task add --run <run> --title <text> [--spec <text>|-] [--after <task>]... [--worker <name>] [--id <id>]
                                     (--spec - reads the spec from stdin, at most 16 KiB)
  plan <file.toml|file.json> [--run <run>]
                                     load a whole task graph (with --run: into an existing run)
  start --run <run> [--max-parallel N]
  status [--run <run>]               runs, tasks, gates, approvals, recent events
  stop <task>                        stop the running worker
  retry <task>                       run a failed or stopped task again
  cancel <task>                      cancel a task that has not completed
  events [--limit N]                 recent audit events

working with a task (<task> is <run>/<task> or a unique task id):
  attach <task>                      interactive terminal session of the worker
  send <task> <text...> [--no-newline]
  tail <task> [--follow] [--lines N]

decisions:
  gate list                          open questions and conflicts
  gate resolve <id> --answer <text>  answer a gate (or give the answer as the rest of the line)
  approval list                      approvals and their digests
  approve <id> [--digest <hex>]      approve a pending action
  deny <id> [--digest <hex>]         deny it

results:
  merge <task>                       merge a reviewed task into the integration branch
  cleanup [--run <run>] [--branches] remove finished worktrees (and branches)
  undo <task> [--apply]              preview or revert the last worker changes (needs agent-undo)

dashboard:
  tui                                terminal dashboard

exit codes: 0 ok, 1 internal, 2 usage, 3 daemon down, 4 not found, 5 approval required,
6 conflict, 7 invalid, 8 limit, 9 unavailable, 10 forbidden, 11 tampered, 12 timeout, 13 unhealthy
";

/// Bytes of output requested per `task.tail` call; small enough that the reply fits one socket
/// message even after JSON escaping.
const TAIL_CHUNK: u64 = 16 * 1024;
const DEFAULT_TAIL_LINES: usize = 40;
const FOLLOW_POLL: Duration = Duration::from_millis(200);

/// One request/reply exchange with the daemon.
pub trait Caller {
    fn call(&mut self, cmd: &str, args: Value) -> Result<Value>;
}

impl Caller for Client {
    fn call(&mut self, cmd: &str, args: Value) -> Result<Value> {
        Client::call(self, cmd, args)
    }
}

pub struct Context {
    pub state_dir: PathBuf,
    pub socket: PathBuf,
    pub json: bool,
    pub env: Vec<(String, String)>,
}

/// How a successful reply is shown in text mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum View {
    /// The id the user asked for, used when the reply does not name the run.
    RunCreated(Option<String>),
    TaskAdded(String),
    Plan,
    Ack(String),
    Status,
    Gates,
    Approvals,
    Events,
    Merge,
    Cleanup,
    Undo(bool),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub cmd: &'static str,
    pub args: Value,
    pub view: View,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Help,
    Version,
    Daemon {
        config: Option<PathBuf>,
    },
    Attach {
        task: String,
    },
    Tui,
    Doctor {
        config: Option<PathBuf>,
    },
    VerifyLog,
    Plan {
        file: PathBuf,
        run: Option<String>,
    },
    Tail {
        task: String,
        follow: bool,
        lines: usize,
    },
    Request(Request),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cli {
    pub state_dir: Option<PathBuf>,
    pub json: bool,
    pub command: Command,
}

// ---- parsing -------------------------------------------------------------------------------

fn obj(pairs: Vec<(&'static str, Option<Value>)>) -> Value {
    Value::object(pairs.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))))
}

fn st(s: String) -> Option<Value> {
    Some(Value::Str(s))
}

fn request(cmd: &'static str, args: Value, view: View) -> Command {
    Command::Request(Request { cmd, args, view })
}

/// An id or task reference from the user, quoted only when it contains anything unusual.
fn token(s: &str) -> String {
    let plain = !s.is_empty()
        && s.len() <= 2 * MAX_ID_LEN + 1
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'));
    if plain {
        s.to_string()
    } else {
        show(s)
    }
}

fn required(p: &mut Parsed, name: &str) -> Result<String> {
    p.opt(name)?
        .ok_or_else(|| FleetError::usage(format!("option {name} is required")))
}

fn opt_uint(p: &mut Parsed, name: &str) -> Result<Option<u64>> {
    match p.opt(name)? {
        None => Ok(None),
        Some(s) => s
            .parse::<u64>()
            .map(Some)
            .map_err(|_| FleetError::usage(format!("option {name} expects a whole number"))),
    }
}

fn max_parallel(p: &mut Parsed) -> Result<Option<Value>> {
    match opt_uint(p, "--max-parallel")? {
        None => Ok(None),
        Some(n) => {
            let n = u32::try_from(n)
                .map_err(|_| FleetError::usage("option --max-parallel is too large"))?;
            Ok(Some(Value::int(n)))
        }
    }
}

fn absolute(p: &Path, what: &str) -> Result<PathBuf> {
    if p.as_os_str().is_empty() {
        return Err(FleetError::usage(format!("{what} is empty")));
    }
    std::path::absolute(p).map_err(|e| FleetError::usage(format!("{what}: {e}")))
}

fn absolute_text(s: &str, what: &str) -> Result<String> {
    let p = absolute(Path::new(s), what)?;
    p.to_str()
        .map(str::to_string)
        .ok_or_else(|| FleetError::invalid(format!("{what} is not valid UTF-8")))
}

fn no_args(p: Parsed) -> Result<()> {
    match p.finish()?.first() {
        None => Ok(()),
        Some(extra) => Err(FleetError::usage(format!(
            "unexpected argument {}",
            show(extra)
        ))),
    }
}

fn one_arg(p: Parsed, what: &str) -> Result<String> {
    let mut v = p.finish()?;
    match v.len() {
        0 => Err(FleetError::usage(format!("missing {what}"))),
        1 => v
            .pop()
            .ok_or_else(|| FleetError::internal("argument list changed")),
        _ => Err(FleetError::usage(format!(
            "unexpected argument {} after {what}",
            show(v.get(1).map_or("", String::as_str))
        ))),
    }
}

/// The spec given on the command line, or stdin (bounded) for `-`.
fn read_spec(value: String, stdin: &mut dyn Read) -> Result<String> {
    if value != "-" {
        return Ok(value);
    }
    let mut buf = Vec::new();
    Read::take(&mut *stdin, MAX_SPEC as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| FleetError::io("reading the spec from stdin", &e))?;
    if buf.len() > MAX_SPEC {
        return Err(FleetError::limit(format!(
            "the spec on stdin is longer than {MAX_SPEC} bytes"
        )));
    }
    String::from_utf8(buf).map_err(|_| FleetError::invalid("the spec on stdin is not valid UTF-8"))
}

/// Parses a command line (without the program name). `stdin` is only read for `--spec -`.
pub fn parse_cli(argv: &[String], stdin: &mut dyn Read) -> Result<Cli> {
    let mut p = Parsed::new(argv)?;
    let help = p.flag("-h") | p.flag("--help");
    let version = p.flag("-V") | p.flag("--version");
    let json = p.flag("--json");
    let state_dir = p.opt("--state-dir")?.map(PathBuf::from);
    let done = |command| {
        Ok(Cli {
            state_dir: state_dir.clone(),
            json,
            command,
        })
    };
    if help {
        return done(Command::Help);
    }
    if version {
        return done(Command::Version);
    }
    let Some(word) = p.take_word() else {
        p.finish()?;
        return Err(FleetError::usage("missing command"));
    };
    let command = parse_command(&word, p, stdin)?;
    done(command)
}

fn parse_command(word: &str, mut p: Parsed, stdin: &mut dyn Read) -> Result<Command> {
    Ok(match word {
        "daemon" => {
            let config = p.opt("--config")?;
            no_args(p)?;
            Command::Daemon {
                config: config
                    .map(|c| absolute(Path::new(&c), "--config"))
                    .transpose()?,
            }
        }
        "run" => {
            let sub = p.take_word();
            match sub.as_deref() {
                Some("create") => {
                    let objective = required(&mut p, "--objective")?;
                    let repo = absolute_text(&required(&mut p, "--repo")?, "--repo")?;
                    let base = p.opt("--base")?;
                    let id = p.opt("--id")?;
                    let parallel = max_parallel(&mut p)?;
                    no_args(p)?;
                    let view = View::RunCreated(id.clone());
                    request(
                        "run.create",
                        obj(vec![
                            ("objective", st(objective)),
                            ("repo", st(repo)),
                            ("base", base.and_then(st)),
                            ("id", id.and_then(st)),
                            ("max_parallel", parallel),
                        ]),
                        view,
                    )
                }
                other => return Err(subcommand_error("run", other, "create")),
            }
        }
        "task" => {
            let sub = p.take_word();
            match sub.as_deref() {
                Some("add") => {
                    let run = required(&mut p, "--run")?;
                    let title = required(&mut p, "--title")?;
                    let spec = p.opt("--spec")?.map(|s| read_spec(s, stdin)).transpose()?;
                    let after = p.multi("--after")?;
                    let worker = p.opt("--worker")?;
                    let id = p.opt("--id")?;
                    no_args(p)?;
                    let view = View::TaskAdded(run.clone());
                    request(
                        "task.add",
                        obj(vec![
                            ("run", st(run)),
                            ("title", st(title)),
                            ("spec", spec.and_then(st)),
                            (
                                "after",
                                Some(Value::array(after.into_iter().map(Value::Str))),
                            ),
                            ("worker", worker.and_then(st)),
                            ("id", id.and_then(st)),
                        ]),
                        view,
                    )
                }
                other => return Err(subcommand_error("task", other, "add")),
            }
        }
        "plan" => {
            let run = p.opt("--run")?;
            let file = one_arg(p, "plan file")?;
            Command::Plan {
                file: PathBuf::from(file),
                run,
            }
        }
        "start" => {
            let run = required(&mut p, "--run")?;
            let parallel = max_parallel(&mut p)?;
            no_args(p)?;
            let view = View::Ack(format!("run {} started", token(&run)));
            request(
                "run.start",
                obj(vec![("run", st(run)), ("max_parallel", parallel)]),
                view,
            )
        }
        "status" => {
            let run = p.opt("--run")?;
            no_args(p)?;
            request("status", obj(vec![("run", run.and_then(st))]), View::Status)
        }
        "attach" => Command::Attach {
            task: one_arg(p, "<task>")?,
        },
        "send" => {
            let newline = !p.flag("--no-newline");
            let mut words = p.finish()?.into_iter();
            let task = words
                .next()
                .ok_or_else(|| FleetError::usage("missing <task>"))?;
            let text: Vec<String> = words.collect();
            if text.is_empty() {
                return Err(FleetError::usage("missing text to send"));
            }
            let view = View::Ack(format!("sent to {}", token(&task)));
            request(
                "task.send",
                obj(vec![
                    ("task", st(task)),
                    ("text", st(text.join(" "))),
                    ("newline", Some(Value::Bool(newline))),
                ]),
                view,
            )
        }
        "tail" => {
            let follow = p.flag("--follow") | p.flag("-f");
            let lines = match opt_uint(&mut p, "--lines")? {
                None => DEFAULT_TAIL_LINES,
                Some(0) => return Err(FleetError::usage("option --lines must be at least 1")),
                Some(n) => usize::try_from(n).unwrap_or(usize::MAX),
            };
            Command::Tail {
                task: one_arg(p, "<task>")?,
                follow,
                lines,
            }
        }
        "stop" | "retry" | "cancel" => {
            let task = one_arg(p, "<task>")?;
            let (cmd, verb) = match word {
                "stop" => ("task.stop", "stop requested for"),
                "retry" => ("task.retry", "retry requested for"),
                _ => ("task.cancel", "cancel requested for"),
            };
            let view = View::Ack(format!("{verb} {}", token(&task)));
            request(cmd, obj(vec![("task", st(task))]), view)
        }
        "gate" => {
            let sub = p.take_word();
            match sub.as_deref() {
                Some("list") => {
                    no_args(p)?;
                    request("gate.list", obj(vec![]), View::Gates)
                }
                Some("resolve") => {
                    let answer = p.opt("--answer")?;
                    let mut words = p.finish()?.into_iter();
                    let id = words
                        .next()
                        .ok_or_else(|| FleetError::usage("missing gate id"))?;
                    let rest: Vec<String> = words.collect();
                    let answer = match (answer, rest.is_empty()) {
                        (Some(a), true) => a,
                        (None, false) => rest.join(" "),
                        (Some(_), false) => {
                            return Err(FleetError::usage(
                                "give the answer with --answer or as plain words, not both",
                            ))
                        }
                        (None, true) => return Err(FleetError::usage("missing answer")),
                    };
                    let view = View::Ack(format!("gate {} resolved", token(&id)));
                    request(
                        "gate.resolve",
                        obj(vec![("id", st(id)), ("answer", st(answer))]),
                        view,
                    )
                }
                other => return Err(subcommand_error("gate", other, "list or resolve")),
            }
        }
        "approval" => {
            let sub = p.take_word();
            match sub.as_deref() {
                Some("list") => {
                    no_args(p)?;
                    request("approval.list", obj(vec![]), View::Approvals)
                }
                other => return Err(subcommand_error("approval", other, "list")),
            }
        }
        "approve" | "deny" => {
            let approve = word == "approve";
            let digest = p.opt("--digest")?;
            let id = one_arg(p, "approval id")?;
            let view = View::Ack(format!(
                "approval {} {}",
                token(&id),
                if approve { "approved" } else { "denied" }
            ));
            request(
                "approval.decide",
                obj(vec![
                    ("id", st(id)),
                    ("approve", Some(Value::Bool(approve))),
                    ("digest", digest.and_then(st)),
                ]),
                view,
            )
        }
        "merge" => request(
            "merge",
            obj(vec![("task", st(one_arg(p, "<task>")?))]),
            View::Merge,
        ),
        "cleanup" => {
            let branches = p.flag("--branches");
            let run = p.opt("--run")?;
            no_args(p)?;
            request(
                "cleanup",
                obj(vec![
                    ("run", run.and_then(st)),
                    ("branches", Some(Value::Bool(branches))),
                ]),
                View::Cleanup,
            )
        }
        "undo" => {
            let apply = p.flag("--apply");
            request(
                "undo",
                obj(vec![
                    ("task", st(one_arg(p, "<task>")?)),
                    ("apply", Some(Value::Bool(apply))),
                ]),
                View::Undo(apply),
            )
        }
        "events" => {
            let limit = opt_uint(&mut p, "--limit")?;
            no_args(p)?;
            request(
                "events",
                obj(vec![("limit", limit.map(Value::int))]),
                View::Events,
            )
        }
        "shutdown" => {
            no_args(p)?;
            request(
                "shutdown",
                obj(vec![]),
                View::Ack("daemon is shutting down".into()),
            )
        }
        "tui" => {
            no_args(p)?;
            Command::Tui
        }
        "doctor" => {
            let config = p.opt("--config")?;
            no_args(p)?;
            Command::Doctor {
                config: config
                    .map(|c| absolute(Path::new(&c), "--config"))
                    .transpose()?,
            }
        }
        "verify-log" => {
            no_args(p)?;
            Command::VerifyLog
        }
        other => {
            return Err(FleetError::usage(format!(
                "unknown command {}",
                show(other)
            )))
        }
    })
}

fn subcommand_error(group: &str, got: Option<&str>, expected: &str) -> FleetError {
    match got {
        None => FleetError::usage(format!("{group} needs a subcommand: {expected}")),
        Some(s) => FleetError::usage(format!(
            "unknown {group} subcommand {} (expected {expected})",
            show(s)
        )),
    }
}

/// Builds the `plan.load` request from a loaded plan document.
pub fn plan_request(plan: Value, run: Option<String>) -> Result<Request> {
    let Value::Object(mut map) = plan else {
        return Err(FleetError::invalid("the plan must be an object"));
    };
    if let Some(run) = run {
        if map.get("new_run").is_some_and(|v| !v.is_null()) {
            return Err(FleetError::usage(
                "the plan has a [new_run] section; remove it to load into --run",
            ));
        }
        map.remove("new_run");
        map.insert("run".into(), Value::Str(run));
    }
    Ok(Request {
        cmd: "plan.load",
        args: Value::Object(map),
        view: View::Plan,
    })
}

// ---- running -------------------------------------------------------------------------------

fn write_out(out: &mut dyn Write, text: &str) -> Result<()> {
    out.write_all(text.as_bytes())
        .map_err(|e| FleetError::io("writing output", &e))
}

fn json_line(v: &Value) -> String {
    let mut s = v.canonical();
    s.push('\n');
    s
}

fn render_view(view: &View, data: &Value) -> String {
    match view {
        View::RunCreated(id) => render::render_run_created(data, id.as_deref()),
        View::TaskAdded(run) => render::render_task_added(data, run),
        View::Plan => render::render_plan(data),
        View::Ack(label) => render::render_ack(label, data),
        View::Status => render::render_status(data),
        View::Gates => render::render_gates(data),
        View::Approvals => render::render_approvals(data),
        View::Events => render::render_events(data),
        View::Merge => render::render_merge(data),
        View::Cleanup => render::render_cleanup(data),
        View::Undo(apply) => render::render_undo(data, *apply),
    }
}

/// Sends one request and prints the reply: the data document in JSON mode, the human view
/// otherwise.
pub fn execute_request(
    req: Request,
    caller: &mut dyn Caller,
    json: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let data = caller.call(req.cmd, req.args)?;
    let text = if json {
        json_line(&data)
    } else {
        render_view(&req.view, &data)
    };
    write_out(out, &text)
}

/// The last `n` lines of `text`.
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let skip = lines.len().saturating_sub(n);
    lines.iter().skip(skip).copied().collect()
}

/// `fleet tail`. Without `follow`: one reply. With it: poll with the daemon's `next` offset
/// every `poll` until the session is no longer live. In JSON mode every reply is printed as its
/// own line.
pub fn execute_tail(
    task: &str,
    follow: bool,
    lines: usize,
    caller: &mut dyn Caller,
    json: bool,
    out: &mut dyn Write,
    poll: Duration,
) -> Result<()> {
    let mut since: Option<u64> = None;
    let mut first = true;
    let mut ends_with_newline = true;
    loop {
        let args = obj(vec![
            ("task", st(task.to_string())),
            ("max", Some(Value::int(TAIL_CHUNK))),
            ("since", since.map(Value::int)),
        ]);
        let data = caller.call("task.tail", args)?;
        if json {
            write_out(out, &json_line(&data))?;
        } else {
            if data.get("lost").and_then(Value::as_bool) == Some(true) {
                if !ends_with_newline {
                    write_out(out, "\n")?;
                    ends_with_newline = true;
                }
                write_out(out, &format!("{}\n", render::LOST_NOTICE))?;
            }
            let mut text = render::render_tail(&data);
            if first {
                text = last_lines(&text, lines);
            }
            if !text.is_empty() {
                ends_with_newline = text.ends_with('\n');
                write_out(out, &text)?;
            }
        }
        out.flush()
            .map_err(|e| FleetError::io("writing output", &e))?;
        if !follow {
            break;
        }
        if data.get("live").and_then(Value::as_bool) != Some(true) {
            break;
        }
        let next = data
            .get("next")
            .and_then(Value::as_u64)
            .ok_or_else(|| FleetError::internal("the daemon's tail reply has no `next` offset"))?;
        since = Some(next);
        first = false;
        if !poll.is_zero() {
            std::thread::sleep(poll);
        }
    }
    if !json && !ends_with_newline {
        write_out(out, "\n")?;
    }
    Ok(())
}

fn error_value(e: &FleetError) -> Value {
    Value::object([
        ("schema", Value::str("fleet.error/1")),
        (
            "error",
            Value::object([
                ("code", Value::str(e.code.as_str())),
                ("message", Value::str(e.message.clone())),
                ("subject", e.subject.clone().map_or(Value::Null, Value::Str)),
            ]),
        ),
    ])
}

/// Prints an error the way the mode requires and returns the exit code.
pub fn report_error(e: &FleetError, json: bool, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if json {
        let _ = out.write_all(json_line(&error_value(e)).as_bytes());
    } else {
        let _ = writeln!(err, "fleet: {}", render::clean(&e.message));
        match e.code {
            ErrorCode::Usage => {
                let _ = writeln!(err, "try: fleet --help");
            }
            ErrorCode::ApprovalRequired => {
                if let Some(subject) = &e.subject {
                    let _ = writeln!(err, "run: fleet approve {}", token(&render::clean(subject)));
                }
            }
            ErrorCode::DaemonDown => {
                let _ = writeln!(err, "start it with: fleet daemon");
            }
            _ => {}
        }
    }
    e.code.exit_code()
}

fn env_value(env: &[(String, String)], name: &str) -> Option<String> {
    env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
}

fn connect(ctx: &Context) -> Result<Client> {
    Client::connect(&ctx.socket)
}

fn reject_json(ctx: &Context, what: &str) -> Result<()> {
    if ctx.json {
        return Err(FleetError::usage(format!(
            "--json is not supported by `{what}`"
        )));
    }
    Ok(())
}

fn run_verify_log(layout: &StateLayout, json: bool, out: &mut dyn Write) -> Result<()> {
    if std::fs::symlink_metadata(&layout.events).is_err() {
        return Err(FleetError::not_found(format!(
            "there is no event log at {}",
            layout.events.display()
        )));
    }
    let report = verify_file(&layout.events)?;
    let text = if json {
        json_line(&Value::object([
            ("schema", Value::str("fleet.verify-log/1")),
            ("records", Value::int(report.records)),
            ("head_hash", Value::str(report.head_hash)),
        ]))
    } else {
        format!(
            "event log ok: {} records, head {}\n",
            report.records, report.head_hash
        )
    };
    write_out(out, &text)
}

fn run_doctor(
    ctx: &Context,
    layout: StateLayout,
    config: Option<PathBuf>,
    out: &mut dyn Write,
) -> Result<i32> {
    let checks = doctor::run_checks(&DoctorInput {
        layout,
        config,
        env: ctx.env.clone(),
    });
    let text = if ctx.json {
        json_line(&doctor::to_value(&checks))
    } else {
        doctor::render_text(&checks)
    };
    write_out(out, &text)?;
    Ok(if doctor::is_healthy(&checks) {
        0
    } else {
        ErrorCode::Unhealthy.exit_code()
    })
}

fn execute(command: Command, ctx: &Context, out: &mut dyn Write) -> Result<i32> {
    let layout = StateLayout::new(ctx.state_dir.clone());
    match command {
        Command::Help => write_out(out, HELP)?,
        Command::Version => write_out(out, &format!("fleet {}\n", env!("CARGO_PKG_VERSION")))?,
        Command::Daemon { config } => {
            reject_json(ctx, "daemon")?;
            run_daemon(DaemonOptions {
                state_dir: ctx.state_dir.clone(),
                config_path: config,
            })?;
        }
        Command::Attach { task } => {
            reject_json(ctx, "attach")?;
            return crate::attach::run_attach(&ctx.socket, &task);
        }
        Command::Tui => {
            reject_json(ctx, "tui")?;
            crate::tui::run_tui(&ctx.socket)?;
        }
        Command::Doctor { config } => return run_doctor(ctx, layout, config, out),
        Command::VerifyLog => run_verify_log(&layout, ctx.json, out)?,
        Command::Plan { file, run } => {
            let plan = load_plan(&file)?;
            let req = plan_request(plan, run)?;
            execute_request(req, &mut connect(ctx)?, ctx.json, out)?;
        }
        Command::Tail {
            task,
            follow,
            lines,
        } => execute_tail(
            &task,
            follow,
            lines,
            &mut connect(ctx)?,
            ctx.json,
            out,
            FOLLOW_POLL,
        )?,
        Command::Request(req) => execute_request(req, &mut connect(ctx)?, ctx.json, out)?,
    }
    Ok(0)
}

/// Runs the command line against explicit streams; returns the exit code. Never panics.
pub fn run_with(
    argv: Vec<String>,
    env: Vec<(String, String)>,
    stdin: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    // Known before parsing so that even a usage error honors --json.
    let json_requested = argv
        .iter()
        .take_while(|a| *a != "--")
        .any(|a| a == "--json");
    let result = (|| -> Result<i32> {
        let cli = parse_cli(&argv, stdin)?;
        let state_dir = match cli.command {
            Command::Help | Command::Version => PathBuf::new(),
            _ => resolve_state_dir(
                cli.state_dir.as_deref(),
                env_value(&env, "XDG_STATE_HOME"),
                env_value(&env, "HOME"),
            )?,
        };
        let layout = StateLayout::new(state_dir.clone());
        let ctx = Context {
            state_dir,
            socket: layout.socket,
            json: cli.json,
            env,
        };
        execute(cli.command, &ctx, out)
    })();
    match result {
        Ok(code) => {
            let _ = out.flush();
            code
        }
        Err(e) => report_error(&e, json_requested, out, err),
    }
}

/// Entry point of the `fleet` binary.
pub fn run(argv: Vec<String>, env: Vec<(String, String)>) -> i32 {
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let stdin = std::io::stdin();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let mut input = stdin.lock();
    run_with(argv, env, &mut input, &mut out, &mut err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_domain::error::ALL_CODES;
    use fleet_infra::testutil::TempDir;
    use std::collections::VecDeque;
    use std::io::Cursor;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn parse(a: &[&str]) -> Result<Cli> {
        parse_cli(&argv(a), &mut Cursor::new(Vec::<u8>::new()))
    }

    fn parse_ok(a: &[&str]) -> Command {
        parse(a).unwrap().command
    }

    fn req(cmd: &'static str, args: Vec<(&'static str, Value)>, view: View) -> Command {
        Command::Request(Request {
            cmd,
            args: Value::object(args),
            view,
        })
    }

    fn s(x: &str) -> Value {
        Value::str(x)
    }

    struct Fake {
        replies: VecDeque<Result<Value>>,
        calls: Vec<(String, Value)>,
    }

    impl Fake {
        fn new(replies: Vec<Result<Value>>) -> Fake {
            Fake {
                replies: replies.into(),
                calls: Vec::new(),
            }
        }
    }

    impl Caller for Fake {
        fn call(&mut self, cmd: &str, args: Value) -> Result<Value> {
            self.calls.push((cmd.to_string(), args));
            self.replies
                .pop_front()
                .unwrap_or_else(|| Err(FleetError::internal("no scripted reply")))
        }
    }

    fn exec(command: Command, caller: &mut Fake, json: bool) -> (Result<()>, String) {
        let mut out = Vec::new();
        let r = match command {
            Command::Request(r) => execute_request(r, caller, json, &mut out),
            Command::Tail {
                task,
                follow,
                lines,
            } => execute_tail(&task, follow, lines, caller, json, &mut out, Duration::ZERO),
            _ => panic!("not a socket command"),
        };
        (r, String::from_utf8(out).unwrap())
    }

    #[test]
    fn request_construction_table() {
        let cases: Vec<(Vec<&str>, Command)> = vec![
            (
                vec![
                    "run",
                    "create",
                    "--objective",
                    "ship it",
                    "--repo",
                    "/abs/repo",
                    "--base",
                    "main",
                    "--id",
                    "r7",
                    "--max-parallel",
                    "3",
                ],
                req(
                    "run.create",
                    vec![
                        ("objective", s("ship it")),
                        ("repo", s("/abs/repo")),
                        ("base", s("main")),
                        ("id", s("r7")),
                        ("max_parallel", Value::int(3u32)),
                    ],
                    View::RunCreated(Some("r7".into())),
                ),
            ),
            (
                vec!["run", "create", "--objective=o", "--repo=/r"],
                req(
                    "run.create",
                    vec![("objective", s("o")), ("repo", s("/r"))],
                    View::RunCreated(None),
                ),
            ),
            (
                vec![
                    "task", "add", "--run", "r1", "--title", "T", "--spec", "do x", "--after", "a",
                    "--after", "b", "--worker", "coder", "--id", "t9",
                ],
                req(
                    "task.add",
                    vec![
                        ("run", s("r1")),
                        ("title", s("T")),
                        ("spec", s("do x")),
                        ("after", Value::array([s("a"), s("b")])),
                        ("worker", s("coder")),
                        ("id", s("t9")),
                    ],
                    View::TaskAdded("r1".into()),
                ),
            ),
            (
                vec!["task", "add", "--run", "r1", "--title", "T"],
                req(
                    "task.add",
                    vec![
                        ("run", s("r1")),
                        ("title", s("T")),
                        ("after", Value::array([])),
                    ],
                    View::TaskAdded("r1".into()),
                ),
            ),
            (
                vec!["start", "--run", "r1", "--max-parallel", "2"],
                req(
                    "run.start",
                    vec![("run", s("r1")), ("max_parallel", Value::int(2u32))],
                    View::Ack("run r1 started".into()),
                ),
            ),
            (vec!["status"], req("status", vec![], View::Status)),
            (
                vec!["status", "--run", "r1"],
                req("status", vec![("run", s("r1"))], View::Status),
            ),
            (
                vec!["send", "r1/t1", "hello", "there", "--no-newline"],
                req(
                    "task.send",
                    vec![
                        ("task", s("r1/t1")),
                        ("text", s("hello there")),
                        ("newline", Value::Bool(false)),
                    ],
                    View::Ack("sent to r1/t1".into()),
                ),
            ),
            (
                vec!["send", "t1", "go"],
                req(
                    "task.send",
                    vec![
                        ("task", s("t1")),
                        ("text", s("go")),
                        ("newline", Value::Bool(true)),
                    ],
                    View::Ack("sent to t1".into()),
                ),
            ),
            (
                vec!["stop", "t1"],
                req(
                    "task.stop",
                    vec![("task", s("t1"))],
                    View::Ack("stop requested for t1".into()),
                ),
            ),
            (
                vec!["retry", "r1/t1"],
                req(
                    "task.retry",
                    vec![("task", s("r1/t1"))],
                    View::Ack("retry requested for r1/t1".into()),
                ),
            ),
            (
                vec!["cancel", "t2"],
                req(
                    "task.cancel",
                    vec![("task", s("t2"))],
                    View::Ack("cancel requested for t2".into()),
                ),
            ),
            (vec!["gate", "list"], req("gate.list", vec![], View::Gates)),
            (
                vec!["gate", "resolve", "g1", "--answer", "use files"],
                req(
                    "gate.resolve",
                    vec![("id", s("g1")), ("answer", s("use files"))],
                    View::Ack("gate g1 resolved".into()),
                ),
            ),
            (
                vec!["gate", "resolve", "g1", "use", "files"],
                req(
                    "gate.resolve",
                    vec![("id", s("g1")), ("answer", s("use files"))],
                    View::Ack("gate g1 resolved".into()),
                ),
            ),
            (
                vec!["approval", "list"],
                req("approval.list", vec![], View::Approvals),
            ),
            (
                vec!["approve", "a1", "--digest", "abcd"],
                req(
                    "approval.decide",
                    vec![
                        ("id", s("a1")),
                        ("approve", Value::Bool(true)),
                        ("digest", s("abcd")),
                    ],
                    View::Ack("approval a1 approved".into()),
                ),
            ),
            (
                vec!["deny", "a1"],
                req(
                    "approval.decide",
                    vec![("id", s("a1")), ("approve", Value::Bool(false))],
                    View::Ack("approval a1 denied".into()),
                ),
            ),
            (
                vec!["merge", "r1/t1"],
                req("merge", vec![("task", s("r1/t1"))], View::Merge),
            ),
            (
                vec!["cleanup", "--run", "r1", "--branches"],
                req(
                    "cleanup",
                    vec![("run", s("r1")), ("branches", Value::Bool(true))],
                    View::Cleanup,
                ),
            ),
            (
                vec!["cleanup"],
                req(
                    "cleanup",
                    vec![("branches", Value::Bool(false))],
                    View::Cleanup,
                ),
            ),
            (
                vec!["undo", "t1", "--apply"],
                req(
                    "undo",
                    vec![("task", s("t1")), ("apply", Value::Bool(true))],
                    View::Undo(true),
                ),
            ),
            (
                vec!["undo", "t1"],
                req(
                    "undo",
                    vec![("task", s("t1")), ("apply", Value::Bool(false))],
                    View::Undo(false),
                ),
            ),
            (
                vec!["events", "--limit", "5"],
                req("events", vec![("limit", Value::int(5u64))], View::Events),
            ),
            (
                vec!["shutdown"],
                req(
                    "shutdown",
                    vec![],
                    View::Ack("daemon is shutting down".into()),
                ),
            ),
        ];
        for (args, want) in cases {
            assert_eq!(parse_ok(&args), want, "{args:?}");
        }
    }

    #[test]
    fn non_request_commands_parse() {
        assert_eq!(
            parse_ok(&["attach", "r1/t1"]),
            Command::Attach {
                task: "r1/t1".into()
            }
        );
        assert_eq!(parse_ok(&["tui"]), Command::Tui);
        assert_eq!(parse_ok(&["verify-log"]), Command::VerifyLog);
        assert_eq!(
            parse_ok(&["tail", "t1"]),
            Command::Tail {
                task: "t1".into(),
                follow: false,
                lines: DEFAULT_TAIL_LINES
            }
        );
        assert_eq!(
            parse_ok(&["tail", "t1", "--follow", "--lines", "5"]),
            Command::Tail {
                task: "t1".into(),
                follow: true,
                lines: 5
            }
        );
        assert_eq!(
            parse_ok(&["plan", "p.toml", "--run", "r1"]),
            Command::Plan {
                file: "p.toml".into(),
                run: Some("r1".into())
            }
        );
        assert_eq!(
            parse_ok(&["daemon", "--config", "/etc/fleet.toml"]),
            Command::Daemon {
                config: Some("/etc/fleet.toml".into())
            }
        );
        assert_eq!(parse_ok(&["daemon"]), Command::Daemon { config: None });
        assert_eq!(
            parse_ok(&["doctor", "--config=/c.toml"]),
            Command::Doctor {
                config: Some("/c.toml".into())
            }
        );
    }

    #[test]
    fn relative_repo_and_config_become_absolute() {
        let Command::Request(r) =
            parse_ok(&["run", "create", "--objective", "o", "--repo", "rel/repo"])
        else {
            panic!("not a request");
        };
        let repo = r.args.get("repo").and_then(Value::as_str).unwrap();
        assert!(Path::new(repo).is_absolute(), "{repo}");
        assert!(repo.ends_with("rel/repo"));
        let Command::Doctor { config } = parse_ok(&["doctor", "--config", "f.toml"]) else {
            panic!("not doctor");
        };
        assert!(config.unwrap().is_absolute());
    }

    #[test]
    fn global_options_work_before_and_after_the_command() {
        for a in [
            vec!["--json", "--state-dir", "/s", "status"],
            vec!["status", "--json", "--state-dir", "/s"],
            vec!["--state-dir=/s", "status", "--json"],
        ] {
            let cli = parse(&a).unwrap();
            assert!(cli.json, "{a:?}");
            assert_eq!(cli.state_dir, Some(PathBuf::from("/s")));
            assert!(matches!(cli.command, Command::Request(_)));
        }
        assert!(!parse(&["status"]).unwrap().json);
    }

    #[test]
    fn help_and_version_win_anywhere() {
        for a in [
            vec!["--help"],
            vec!["-h"],
            vec!["status", "-h"],
            vec!["run", "create", "--help"],
        ] {
            assert_eq!(parse_ok(&a), Command::Help, "{a:?}");
        }
        assert_eq!(parse_ok(&["--version"]), Command::Version);
        assert_eq!(parse_ok(&["-V"]), Command::Version);
        assert_eq!(
            parse_ok(&["send", "t1", "--", "-h"]),
            req(
                "task.send",
                vec![
                    ("task", s("t1")),
                    ("text", s("-h")),
                    ("newline", Value::Bool(true))
                ],
                View::Ack("sent to t1".into())
            ),
            "-h after -- is text"
        );
    }

    #[test]
    fn usage_errors_table() {
        let cases: Vec<(Vec<&str>, &str)> = vec![
            (vec![], "missing command"),
            (vec!["--json"], "missing command"),
            (vec!["--bogus"], "unknown option"),
            (vec!["frobnicate"], "unknown command"),
            (vec!["run"], "needs a subcommand"),
            (vec!["run", "make"], "unknown run subcommand"),
            (vec!["task"], "needs a subcommand"),
            (vec!["gate", "open"], "unknown gate subcommand"),
            (vec!["approval"], "needs a subcommand"),
            (
                vec!["run", "create", "--repo", "/r"],
                "--objective is required",
            ),
            (
                vec!["run", "create", "--objective", "o"],
                "--repo is required",
            ),
            (
                vec!["run", "create", "--objective", "o", "--repo", ""],
                "--repo is empty",
            ),
            (
                vec![
                    "run",
                    "create",
                    "--objective",
                    "o",
                    "--repo",
                    "/r",
                    "--max-parallel",
                    "x",
                ],
                "--max-parallel expects a whole number",
            ),
            (
                vec![
                    "run",
                    "create",
                    "--objective",
                    "o",
                    "--repo",
                    "/r",
                    "--max-parallel",
                    "99999999999",
                ],
                "too large",
            ),
            (vec!["task", "add", "--title", "t"], "--run is required"),
            (
                vec!["task", "add", "--run", "r1", "--title"],
                "--title needs a value",
            ),
            (vec!["status", "--nope"], "unknown option"),
            (vec!["status", "extra"], "unexpected argument"),
            (vec!["start"], "--run is required"),
            (vec!["attach"], "missing <task>"),
            (vec!["attach", "a", "b"], "unexpected argument"),
            (vec!["send", "t1"], "missing text"),
            (vec!["send"], "missing <task>"),
            (vec!["tail", "t1", "--lines", "0"], "at least 1"),
            (vec!["tail", "t1", "--lines", "-3"], "whole number"),
            (vec!["stop"], "missing <task>"),
            (vec!["gate", "resolve"], "missing gate id"),
            (vec!["gate", "resolve", "g1"], "missing answer"),
            (
                vec!["gate", "resolve", "g1", "--answer", "a", "b"],
                "not both",
            ),
            (vec!["approve"], "missing approval id"),
            (vec!["merge"], "missing <task>"),
            (vec!["undo", "t", "--apply=1"], "unknown option"),
            (vec!["events", "--limit", "many"], "whole number"),
            (vec!["shutdown", "now"], "unexpected argument"),
            (vec!["plan"], "missing plan file"),
            (vec!["status", "--run", "a", "--run", "b"], "more than once"),
        ];
        for (a, want) in cases {
            let e = parse(&a).unwrap_err();
            assert_eq!(e.code, ErrorCode::Usage, "{a:?}");
            assert!(e.message.contains(want), "{a:?}: {}", e.message);
        }
    }

    #[test]
    fn hostile_arguments_never_reach_error_text_raw() {
        let e = parse(&["\u{1b}[31mevil\u{7}"]).unwrap_err();
        assert!(!e.message.contains('\u{1b}') && !e.message.contains('\u{7}'));
        let e = parse(&["status", "--x\u{1b}[2J"]).unwrap_err();
        assert!(!e.message.contains('\u{1b}'));
        let long = "z".repeat(5000);
        let e = parse(&[long.as_str()]).unwrap_err();
        assert!(
            e.message.len() < 200,
            "long names are cut: {}",
            e.message.len()
        );
    }

    #[test]
    fn option_looking_values_after_terminator_are_text() {
        let c = parse_ok(&["send", "t1", "--", "--json", "--no-newline"]);
        assert_eq!(
            c,
            req(
                "task.send",
                vec![
                    ("task", s("t1")),
                    ("text", s("--json --no-newline")),
                    ("newline", Value::Bool(true))
                ],
                View::Ack("sent to t1".into())
            )
        );
        assert!(!parse(&["send", "t1", "--", "--json"]).unwrap().json);
    }

    #[test]
    fn spec_from_stdin_is_read_and_bounded() {
        let a = argv(&["task", "add", "--run", "r1", "--title", "t", "--spec", "-"]);
        let mut input = Cursor::new(b"line one\nline two".to_vec());
        let Command::Request(r) = parse_cli(&a, &mut input).unwrap().command else {
            panic!("not a request");
        };
        assert_eq!(
            r.args.get("spec").and_then(Value::as_str),
            Some("line one\nline two")
        );
        let mut exactly = Cursor::new(vec![b'x'; MAX_SPEC]);
        assert!(parse_cli(&a, &mut exactly).is_ok());
        let mut big = Cursor::new(vec![b'x'; MAX_SPEC + 1]);
        let e = parse_cli(&a, &mut big).unwrap_err();
        assert_eq!(e.code, ErrorCode::Limit);
        let mut bad = Cursor::new(vec![0xff, 0xfe]);
        assert_eq!(
            parse_cli(&a, &mut bad).unwrap_err().code,
            ErrorCode::Invalid
        );
        // stdin is untouched when the spec is given inline
        struct Panics;
        impl Read for Panics {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                panic!("stdin must not be read");
            }
        }
        let inline = argv(&[
            "task", "add", "--run", "r1", "--title", "t", "--spec", "text",
        ]);
        assert!(parse_cli(&inline, &mut Panics).is_ok());
    }

    #[test]
    fn plan_request_rules() {
        let plan = Value::object([
            ("tasks", Value::array([])),
            (
                "new_run",
                Value::object([("objective", s("o")), ("repo", s("/r"))]),
            ),
        ]);
        let r = plan_request(plan.clone(), None).unwrap();
        assert_eq!(r.cmd, "plan.load");
        assert_eq!(r.args, plan, "without --run the plan is sent unchanged");

        let e = plan_request(plan, Some("r1".into())).unwrap_err();
        assert_eq!(e.code, ErrorCode::Usage);
        assert!(e.message.contains("new_run"));

        let no_run = Value::object([("tasks", Value::array([])), ("new_run", Value::Null)]);
        let r = plan_request(no_run, Some("r1".into())).unwrap();
        assert_eq!(
            r.args,
            Value::object([("tasks", Value::array([])), ("run", s("r1"))])
        );
        assert_eq!(
            plan_request(Value::Null, None).unwrap_err().code,
            ErrorCode::Invalid
        );
    }

    #[test]
    fn execute_sends_exact_request_and_prints_view() {
        let mut f = Fake::new(vec![Ok(Value::object([("run", s("r1"))]))]);
        let c = parse_ok(&["run", "create", "--objective", "o", "--repo", "/r"]);
        let (r, out) = exec(c, &mut f, false);
        r.unwrap();
        assert_eq!(out, "run r1 created\n");
        assert_eq!(f.calls.len(), 1);
        assert_eq!(f.calls[0].0, "run.create");
        assert_eq!(
            f.calls[0].1,
            Value::object([("objective", s("o")), ("repo", s("/r"))])
        );
    }

    #[test]
    fn json_mode_prints_the_reply_untouched() {
        let reply = Value::object([
            ("schema", s("fleet.status/1")),
            ("runs", Value::array([])),
            ("weird", s("a\u{1b}b")),
        ]);
        let mut f = Fake::new(vec![Ok(reply.clone())]);
        let (r, out) = exec(parse_ok(&["status"]), &mut f, true);
        r.unwrap();
        assert_eq!(out, format!("{}\n", reply.canonical()));
        assert!(out.contains("\\u001b"), "JSON escapes, it does not strip");
        assert_eq!(out.matches('\n').count(), 1, "one document, one line");
    }

    #[test]
    fn daemon_errors_propagate_with_code_and_subject() {
        let err =
            FleetError::new(ErrorCode::ApprovalRequired, "merge needs approval").with_subject("a4");
        let mut f = Fake::new(vec![Err(err)]);
        let (r, out) = exec(parse_ok(&["merge", "t1"]), &mut f, false);
        let e = r.unwrap_err();
        assert_eq!(e.code, ErrorCode::ApprovalRequired);
        assert_eq!(e.subject.as_deref(), Some("a4"));
        assert!(out.is_empty());
    }

    #[test]
    fn hostile_reply_text_is_sanitized_in_every_view() {
        let evil = "\u{1b}]0;title\u{7}x\u{1b}[2Jy\u{202e}z";
        let reply = Value::object([
            ("message", s(evil)),
            ("report", s(evil)),
            ("run", s(evil)),
            ("task", s(evil)),
            ("tasks", Value::array([])),
            (
                "gates",
                Value::array([Value::object([
                    ("text", s(evil)),
                    ("open", Value::Bool(true)),
                ])]),
            ),
            (
                "approvals",
                Value::array([Value::object([
                    ("display", s(evil)),
                    ("status", s("pending")),
                ])]),
            ),
            ("events", Value::array([Value::object([("line", s(evil))])])),
        ]);
        for a in [
            vec!["merge", "t1"],
            vec!["undo", "t1"],
            vec!["cleanup"],
            vec!["gate", "list"],
            vec!["approval", "list"],
            vec!["events"],
            vec!["run", "create", "--objective", "o", "--repo", "/r"],
            vec!["task", "add", "--run", "r1", "--title", "t"],
            vec!["stop", "t1"],
        ] {
            let mut f = Fake::new(vec![Ok(reply.clone())]);
            let (r, out) = exec(parse_ok(&a), &mut f, false);
            r.unwrap();
            assert!(
                !out.chars().any(|c| c.is_control() && c != '\n'),
                "{a:?}: {out:?}"
            );
            assert!(!out.contains('\u{202e}'), "{a:?}");
        }
    }

    fn tail_reply(text: &str, next: u64, live: bool, lost: bool) -> Result<Value> {
        Ok(Value::object([
            ("schema", s("fleet.tail/1")),
            ("task", s("r1/t1")),
            ("live", Value::Bool(live)),
            ("text", s(text)),
            ("next", Value::int(next)),
            ("lost", Value::Bool(lost)),
        ]))
    }

    #[test]
    fn tail_without_follow_shows_the_last_lines_once() {
        let mut f = Fake::new(vec![tail_reply("a\nb\nc\nd\n", 8, true, false)]);
        let (r, out) = exec(parse_ok(&["tail", "t1", "--lines", "2"]), &mut f, false);
        r.unwrap();
        assert_eq!(out, "c\nd\n");
        assert_eq!(f.calls.len(), 1);
        assert_eq!(f.calls[0].0, "task.tail");
        assert_eq!(
            f.calls[0].1,
            Value::object([("task", s("t1")), ("max", Value::int(TAIL_CHUNK))])
        );
    }

    #[test]
    fn tail_follow_polls_with_next_until_not_live() {
        let mut f = Fake::new(vec![
            tail_reply("one\ntwo\n", 8, true, false),
            tail_reply("three", 13, true, false),
            tail_reply("four\n", 19, true, true),
            tail_reply("", 19, false, false),
        ]);
        let (r, out) = exec(parse_ok(&["tail", "t1", "-f"]), &mut f, false);
        r.unwrap();
        assert_eq!(
            out,
            format!("one\ntwo\nthree\n{}\nfour\n", render::LOST_NOTICE)
        );
        let sinces: Vec<Option<u64>> = f
            .calls
            .iter()
            .map(|(_, a)| a.get("since").and_then(Value::as_u64))
            .collect();
        assert_eq!(sinces, vec![None, Some(8), Some(13), Some(19)]);
    }

    #[test]
    fn tail_follow_terminates_the_last_line() {
        let mut f = Fake::new(vec![tail_reply("no newline", 10, false, false)]);
        let (r, out) = exec(parse_ok(&["tail", "t1", "--follow"]), &mut f, false);
        r.unwrap();
        assert_eq!(out, "no newline\n");
    }

    #[test]
    fn tail_follow_requires_a_next_offset_while_live() {
        let mut f = Fake::new(vec![Ok(Value::object([
            ("live", Value::Bool(true)),
            ("text", s("x")),
        ]))]);
        let (r, _) = exec(parse_ok(&["tail", "t1", "-f"]), &mut f, false);
        assert_eq!(r.unwrap_err().code, ErrorCode::Internal);
    }

    #[test]
    fn tail_json_prints_each_reply_as_a_line() {
        let mut f = Fake::new(vec![
            tail_reply("a", 1, true, false),
            tail_reply("b", 2, false, false),
        ]);
        let (r, out) = exec(parse_ok(&["tail", "t1", "-f"]), &mut f, true);
        r.unwrap();
        assert_eq!(out.lines().count(), 2);
        assert!(out.lines().all(|l| l.starts_with("{\"live\"")));
    }

    #[test]
    fn tail_text_is_sanitized_again() {
        let mut f = Fake::new(vec![tail_reply(
            "ok\u{1b}[31m red\u{1b}]52;c;AA\u{7}\n",
            5,
            false,
            false,
        )]);
        let (r, out) = exec(parse_ok(&["tail", "t1"]), &mut f, false);
        r.unwrap();
        assert!(!out.contains('\u{1b}') && !out.contains('\u{7}'), "{out:?}");
        assert!(out.starts_with("ok"));
    }

    #[test]
    fn last_lines_edge_cases() {
        assert_eq!(last_lines("", 3), "");
        assert_eq!(last_lines("a", 3), "a");
        assert_eq!(last_lines("a\nb\n", 1), "b\n");
        assert_eq!(last_lines("a\nb", 1), "b");
        assert_eq!(last_lines("a\n\n\n", 2), "\n\n");
    }

    fn report(e: &FleetError, json: bool) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = report_error(e, json, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn every_error_code_has_its_exit_code_in_both_modes() {
        for code in ALL_CODES {
            let e = FleetError::new(code, "something went wrong");
            let (exit, out, err) = report(&e, false);
            assert_eq!(exit, code.exit_code());
            assert!(out.is_empty());
            assert!(err.starts_with("fleet: something went wrong\n"), "{err}");

            let (exit, out, err) = report(&e.clone().with_subject("x1"), true);
            assert_eq!(exit, code.exit_code());
            assert!(err.is_empty(), "json errors go to stdout only");
            assert_eq!(
                out,
                format!(
                    "{{\"error\":{{\"code\":\"{}\",\"message\":\"something went wrong\",\"subject\":\"x1\"}},\"schema\":\"fleet.error/1\"}}\n",
                    code.as_str()
                )
            );
        }
    }

    #[test]
    fn exit_codes_are_the_documented_ones() {
        let want = [
            (ErrorCode::Internal, 1),
            (ErrorCode::Usage, 2),
            (ErrorCode::DaemonDown, 3),
            (ErrorCode::NotFound, 4),
            (ErrorCode::ApprovalRequired, 5),
            (ErrorCode::Conflict, 6),
            (ErrorCode::Invalid, 7),
            (ErrorCode::Limit, 8),
            (ErrorCode::Unavailable, 9),
            (ErrorCode::Forbidden, 10),
            (ErrorCode::Tampered, 11),
            (ErrorCode::Timeout, 12),
            (ErrorCode::Unhealthy, 13),
        ];
        for (code, n) in want {
            assert_eq!(report(&FleetError::new(code, "m"), false).0, n);
        }
    }

    #[test]
    fn json_error_without_subject_has_null() {
        let (_, out, _) = report(&FleetError::not_found("no such task"), true);
        assert!(out.contains("\"subject\":null"), "{out}");
    }

    #[test]
    fn approval_required_prints_the_next_step() {
        let e = FleetError::new(ErrorCode::ApprovalRequired, "merge needs approval a3")
            .with_subject("a3");
        let (code, _, err) = report(&e, false);
        assert_eq!(code, 5);
        assert!(err.ends_with("run: fleet approve a3\n"), "{err}");
        let none = FleetError::new(ErrorCode::ApprovalRequired, "needs approval");
        assert!(!report(&none, false).2.contains("run:"));
    }

    #[test]
    fn hostile_subject_and_message_are_neutralized_in_text_mode() {
        let e = FleetError::new(ErrorCode::ApprovalRequired, "bad\u{1b}[31m news")
            .with_subject("a1; rm -rf ~\u{1b}[2J");
        let (_, _, err) = report(&e, false);
        assert!(!err.contains('\u{1b}'), "{err:?}");
        assert!(
            err.contains("fleet approve \"a1; rm -rf ~"),
            "an odd subject is quoted, never pasted as a command: {err}"
        );
    }

    #[test]
    fn usage_and_daemon_down_hints() {
        let (code, _, err) = report(&FleetError::usage("bad flag"), false);
        assert_eq!(code, 2);
        assert_eq!(err, "fleet: bad flag\ntry: fleet --help\n");
        let (_, _, err) = report(&FleetError::new(ErrorCode::DaemonDown, "no daemon"), false);
        assert!(err.contains("fleet daemon"));
    }

    #[test]
    fn help_lists_every_subcommand_and_option() {
        for sub in SUBCOMMANDS {
            assert!(HELP.contains(sub), "help is missing `{sub}`");
        }
        for opt in [
            "--state-dir",
            "--json",
            "--help",
            "--version",
            "-h",
            "-V",
            "--config",
            "--objective",
            "--repo",
            "--base",
            "--id",
            "--max-parallel",
            "--run",
            "--title",
            "--spec",
            "--after",
            "--worker",
            "--no-newline",
            "--follow",
            "--lines",
            "--answer",
            "--digest",
            "--branches",
            "--apply",
            "--limit",
        ] {
            assert!(HELP.contains(opt), "help is missing `{opt}`");
        }
        for code in ALL_CODES {
            assert!(
                HELP.contains(&code.exit_code().to_string()),
                "exit code {} undocumented",
                code.exit_code()
            );
        }
    }

    fn run_args(a: &[&str]) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with(
            argv(a),
            vec![],
            &mut Cursor::new(Vec::<u8>::new()),
            &mut out,
            &mut err,
        );
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn help_and_version_exit_zero() {
        let (code, out, err) = run_args(&["--help"]);
        assert_eq!(code, 0);
        assert_eq!(out, HELP);
        assert!(err.is_empty());
        let (code, out, _) = run_args(&["-V"]);
        assert_eq!(code, 0);
        assert!(out.starts_with("fleet "));
    }

    #[test]
    fn usage_errors_exit_two_with_one_line_and_hint() {
        let (code, out, err) = run_args(&["nonsense"]);
        assert_eq!(code, 2);
        assert!(out.is_empty());
        assert_eq!(err.lines().count(), 2, "{err}");
        assert!(err.ends_with("try: fleet --help\n"));
    }

    #[test]
    fn usage_errors_honor_json() {
        let (code, out, err) = run_args(&["--json", "nonsense"]);
        assert_eq!(code, 2);
        assert!(err.is_empty());
        assert!(out.contains("\"code\":\"usage\""), "{out}");
    }

    #[test]
    fn json_flag_is_rejected_for_interactive_commands() {
        for a in [
            vec!["--json", "attach", "t1"],
            vec!["--json", "tui"],
            vec!["--json", "daemon"],
        ] {
            let mut full = vec!["--state-dir", "/nonexistent-fleet-cli-test"];
            full.extend(a.iter().copied());
            let (code, _, _) = run_args(&full);
            assert_eq!(code, 2, "{a:?}");
        }
    }

    #[test]
    fn talking_to_a_missing_daemon_exits_three() {
        let (code, out, err) = run_args(&["--state-dir", "/nonexistent-fleet-cli-test", "status"]);
        assert_eq!(code, 3, "{err}");
        assert!(out.is_empty());
        assert!(err.starts_with("fleet: "));
        assert!(err.contains("fleet daemon"));
        let (code, out, _) = run_args(&[
            "--state-dir",
            "/nonexistent-fleet-cli-test",
            "status",
            "--json",
        ]);
        assert_eq!(code, 3);
        assert!(out.contains("\"code\":\"daemon_down\""), "{out}");
    }

    #[test]
    fn verify_log_without_a_log_is_not_found() {
        let t = TempDir::new("cli-verify");
        let dir = t.path().to_str().unwrap().to_string();
        let (code, _, err) = run_args(&["--state-dir", &dir, "verify-log"]);
        assert_eq!(code, 4, "{err}");
        assert!(err.contains("no event log"));
    }

    #[test]
    fn state_dir_comes_from_the_environment_when_not_given() {
        let t = TempDir::new("cli-env");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let env = vec![(
            "XDG_STATE_HOME".to_string(),
            t.path().to_str().unwrap().to_string(),
        )];
        let code = run_with(
            argv(&["verify-log"]),
            env,
            &mut Cursor::new(Vec::<u8>::new()),
            &mut out,
            &mut err,
        );
        assert_eq!(code, 4);
        let err = String::from_utf8(err).unwrap();
        assert!(
            err.contains(t.path().to_str().unwrap()),
            "the log is looked for under XDG_STATE_HOME: {err}"
        );
    }

    #[test]
    fn token_quotes_only_unusual_references() {
        assert_eq!(token("r1/t-2.a_b"), "r1/t-2.a_b");
        assert_eq!(token("a b"), "\"a b\"");
        assert_eq!(token(""), "\"\"");
        assert!(token("x\u{1b}").contains("\\u{1b}"));
        assert!(token(&"a".repeat(500)).ends_with("…\""));
    }
}
