//! The terminal dashboard: `fleet tui`.
//!
//! The rendering ([`ui`]), state ([`model`]) and key bindings ([`keys`]) are pure and tested
//! headless. This module owns the terminal and the daemon connection: it polls `status` every
//! second and the selected task's `task.tail` twice a second, and turns [`keys::Action`]s into
//! control-socket requests. A failed poll never ends the dashboard; the daemon may be restarting.

#[cfg(test)]
mod groups_tests;
pub mod keys;
pub mod model;
pub mod ui;

use std::io::{IsTerminal, Stdout};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::value::Value;
use fleet_infra::client::Client;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use self::keys::{handle_key, Action};
use self::model::Model;
use crate::render_fanout;

const EVENT_POLL: Duration = Duration::from_millis(250);
const STATUS_EVERY: Duration = Duration::from_secs(1);
const TAIL_EVERY: Duration = Duration::from_millis(500);
const GROUPS_EVERY: Duration = Duration::from_secs(3);
const TAIL_MAX: i128 = 16 * 1024;
const BACKOFF_BASE_MS: u64 = 250;
const BACKOFF_MAX_MS: u64 = 4000;

/// Raw mode and the alternate screen for as long as the guard lives; restored on drop, which
/// also runs when the loop panics.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    active: bool,
}

impl TerminalGuard {
    fn enter() -> Result<TerminalGuard> {
        if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
            return Err(FleetError::usage(
                "the dashboard needs an interactive terminal",
            ));
        }
        enable_raw_mode().map_err(|e| FleetError::io("cannot enable raw terminal mode", &e))?;
        let mut out = std::io::stdout();
        if let Err(e) = out.execute(EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(FleetError::io("cannot enter the alternate screen", &e));
        }
        match Terminal::new(CrosstermBackend::new(out)) {
            Ok(terminal) => Ok(TerminalGuard {
                terminal,
                active: true,
            }),
            Err(e) => {
                let _ = std::io::stdout().execute(LeaveAlternateScreen);
                let _ = disable_raw_mode();
                Err(FleetError::io("cannot start the terminal", &e))
            }
        }
    }

    /// Gives the terminal back (for `attach`, or on exit).
    fn leave(&mut self) {
        if self.active {
            self.active = false;
            let _ = disable_raw_mode();
            let backend = self.terminal.backend_mut();
            let _ = backend.execute(LeaveAlternateScreen);
            let _ = backend.execute(crossterm::cursor::Show);
        }
    }

    fn resume(&mut self) -> Result<()> {
        enable_raw_mode().map_err(|e| FleetError::io("cannot enable raw terminal mode", &e))?;
        self.terminal
            .backend_mut()
            .execute(EnterAlternateScreen)
            .map_err(|e| FleetError::io("cannot enter the alternate screen", &e))?;
        self.active = true;
        self.terminal
            .clear()
            .map_err(|e| FleetError::io("cannot clear the terminal", &e))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.leave();
    }
}

/// Reconnect delay after `failures` consecutive failed attempts: 250 ms doubling up to 4 s.
pub fn backoff(failures: u32) -> Duration {
    let ms = BACKOFF_BASE_MS
        .checked_shl(failures.min(10))
        .unwrap_or(BACKOFF_MAX_MS)
        .min(BACKOFF_MAX_MS);
    Duration::from_millis(ms)
}

/// Errors that mean the connection is unusable (as opposed to the daemon refusing a request).
fn is_transport_error(e: &FleetError) -> bool {
    matches!(e.code, ErrorCode::DaemonDown | ErrorCode::Internal)
}

/// A control-socket connection that reconnects with backoff.
struct Link {
    socket: PathBuf,
    client: Option<Client>,
    failures: u32,
    next_try: Instant,
}

impl Link {
    fn new(socket: &Path, client: Client) -> Link {
        Link {
            socket: socket.to_path_buf(),
            client: Some(client),
            failures: 0,
            next_try: Instant::now(),
        }
    }

    fn call(&mut self, cmd: &str, args: Value) -> Result<Value> {
        if self.client.is_none() {
            if Instant::now() < self.next_try {
                return Err(FleetError::new(ErrorCode::DaemonDown, "daemon unreachable"));
            }
            match Client::connect(&self.socket) {
                Ok(c) => {
                    self.client = Some(c);
                    self.failures = 0;
                }
                Err(e) => {
                    self.failures = self.failures.saturating_add(1);
                    self.next_try = Instant::now() + backoff(self.failures);
                    return Err(e);
                }
            }
        }
        let Some(client) = self.client.as_mut() else {
            return Err(FleetError::new(ErrorCode::DaemonDown, "daemon unreachable"));
        };
        let result = client.call(cmd, args);
        if let Err(e) = &result {
            if is_transport_error(e) {
                self.client = None;
                self.next_try = Instant::now() + backoff(0);
            }
        }
        result
    }
}

/// The request an action sends, as (label for the status line, command, arguments).
fn request_for(action: &Action) -> Option<(String, &'static str, Value)> {
    let task_args = |task: &str| Value::object([("task", Value::str(task))]);
    match action {
        Action::Approve { id, digest } | Action::Deny { id, digest } => {
            let approve = matches!(action, Action::Approve { .. });
            Some((
                format!("{} {id}", if approve { "approve" } else { "deny" }),
                "approval.decide",
                Value::object([
                    ("id", Value::str(id.as_str())),
                    ("approve", Value::Bool(approve)),
                    ("digest", Value::str(digest.as_str())),
                ]),
            ))
        }
        Action::ResolveGate { id, answer } => Some((
            format!("answer {id}"),
            "gate.resolve",
            Value::object([
                ("id", Value::str(id.as_str())),
                ("answer", Value::str(answer.as_str())),
            ]),
        )),
        Action::Stop(t) => Some((format!("stop {t}"), "task.stop", task_args(t))),
        Action::Retry(t) => Some((format!("retry {t}"), "task.retry", task_args(t))),
        Action::Merge(t) => Some((format!("merge {t}"), "merge", task_args(t))),
        Action::None | Action::Quit | Action::Refresh | Action::OpenAttach(_) => None,
    }
}

fn describe_error(label: &str, e: &FleetError) -> String {
    let subject = e
        .subject
        .as_deref()
        .map_or_else(String::new, |s| format!(" [{s}]"));
    format!("{label}: {} ({}){subject}", e.message, e.code.as_str())
}

enum Flow {
    Continue,
    Quit,
}

struct App {
    model: Model,
    link: Link,
    socket: PathBuf,
    last_status: Option<Instant>,
    last_tail: Option<Instant>,
    last_groups: Option<Instant>,
}

impl App {
    fn poll_status(&mut self) {
        self.last_status = Some(Instant::now());
        match self.link.call("status", Value::object::<&str, _>([])) {
            Ok(v) => {
                let was_offline = !self.model.connected;
                self.model.update_status(&v);
                if was_offline {
                    self.model.set_message("reconnected to the daemon");
                }
            }
            Err(e) => {
                self.model.connected = false;
                self.model
                    .set_message(&format!("daemon unreachable: {} (retrying)", e.message));
            }
        }
    }

    fn poll_tail(&mut self) {
        self.last_tail = Some(Instant::now());
        let Some(key) = self.model.selected_key() else {
            return;
        };
        let args = Value::object([
            ("task", Value::str(key.as_str())),
            ("max", Value::Int(TAIL_MAX)),
        ]);
        // A failed tail poll is not worth a message; the status poll reports connection loss.
        if let Ok(v) = self.link.call("task.tail", args) {
            if let Some(text) = v.get("text").and_then(Value::as_str) {
                self.model.set_tail(&key, text);
            }
        }
    }

    /// Read only: the list of groups and the comparison of the selected one.
    fn poll_groups(&mut self) {
        self.last_groups = Some(Instant::now());
        if let Ok(list) = self.link.call("group.list", Value::object::<&str, _>([])) {
            self.model.update_groups(&list);
        }
        let Some(id) = self.model.selected_group().map(|g| g.id.clone()) else {
            return;
        };
        let args = Value::object([("group", Value::str(id.as_str()))]);
        if let Ok(v) = self.link.call("group.compare", args) {
            self.model
                .set_group_text(&id, &render_fanout::render_compare(&v));
        }
    }

    fn refresh_now(&mut self) {
        self.poll_status();
        self.poll_tail();
        if self.model.groups_view {
            self.poll_groups();
        }
    }

    fn execute(&mut self, guard: &mut TerminalGuard, action: Action) -> Flow {
        match action {
            Action::Quit => return Flow::Quit,
            Action::None => {}
            Action::Refresh => self.refresh_now(),
            Action::OpenAttach(task) => {
                guard.leave();
                let outcome = crate::attach::run_attach(&self.socket, &task);
                if let Err(e) = guard.resume() {
                    // Without a terminal there is nothing left to draw on.
                    eprintln!("fleet: {}", e.message);
                    return Flow::Quit;
                }
                match outcome {
                    Ok(_) => self.model.set_message(&format!("detached from {task}")),
                    Err(e) => self
                        .model
                        .set_message(&describe_error(&format!("attach {task}"), &e)),
                }
                self.refresh_now();
            }
            other => {
                if let Some((label, cmd, args)) = request_for(&other) {
                    match self.link.call(cmd, args) {
                        Ok(_) => self.model.set_message(&format!("{label}: ok")),
                        Err(e) => self.model.set_message(&describe_error(&label, &e)),
                    }
                    self.refresh_now();
                }
            }
        }
        Flow::Continue
    }

    fn due(last: Option<Instant>, every: Duration) -> bool {
        last.is_none_or(|t| t.elapsed() >= every)
    }

    fn run(&mut self, guard: &mut TerminalGuard) -> Result<()> {
        loop {
            guard
                .terminal
                .draw(|f| ui::render(f, &self.model))
                .map_err(|e| FleetError::io("cannot draw the dashboard", &e))?;
            let ready = event::poll(EVENT_POLL)
                .map_err(|e| FleetError::io("cannot read terminal events", &e))?;
            if ready {
                let event =
                    event::read().map_err(|e| FleetError::io("cannot read terminal events", &e))?;
                // Resizes need no handling: the next draw picks up the new size.
                if let Event::Key(key) = event {
                    let action = handle_key(&mut self.model, key);
                    if matches!(self.execute(guard, action), Flow::Quit) {
                        return Ok(());
                    }
                }
            }
            if Self::due(self.last_status, STATUS_EVERY) {
                self.poll_status();
            }
            if Self::due(self.last_tail, TAIL_EVERY) {
                self.poll_tail();
            }
            if self.model.groups_view && Self::due(self.last_groups, GROUPS_EVERY) {
                self.poll_groups();
            }
        }
    }
}

/// Runs the dashboard until the user quits. Fails when the daemon is not running at start;
/// later connection loss only shows up in the status line.
pub fn run_tui(socket: &Path) -> Result<()> {
    let client = Client::connect(socket)?;
    let mut guard = TerminalGuard::enter()?;
    let mut app = App {
        model: Model::new(),
        link: Link::new(socket, client),
        socket: socket.to_path_buf(),
        last_status: None,
        last_tail: None,
        last_groups: None,
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| app.run(&mut guard)));
    guard.leave();
    match outcome {
        Ok(result) => result,
        Err(_) => Err(FleetError::internal("the dashboard stopped unexpectedly")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_is_capped() {
        assert_eq!(backoff(0), Duration::from_millis(250));
        assert_eq!(backoff(1), Duration::from_millis(500));
        assert_eq!(backoff(2), Duration::from_millis(1000));
        assert_eq!(backoff(4), Duration::from_millis(4000));
        assert_eq!(backoff(5), Duration::from_millis(4000));
        assert_eq!(backoff(u32::MAX), Duration::from_millis(4000));
    }

    #[test]
    fn approvals_carry_the_digest_the_user_saw() {
        let (label, cmd, args) = request_for(&Action::Approve {
            id: "a1".into(),
            digest: "abc".into(),
        })
        .expect("request");
        assert_eq!((label.as_str(), cmd), ("approve a1", "approval.decide"));
        assert_eq!(args.get("approve").and_then(Value::as_bool), Some(true));
        assert_eq!(args.get("digest").and_then(Value::as_str), Some("abc"));
        let (_, _, deny) = request_for(&Action::Deny {
            id: "a1".into(),
            digest: "abc".into(),
        })
        .expect("request");
        assert_eq!(deny.get("approve").and_then(Value::as_bool), Some(false));
        assert_eq!(deny.get("digest").and_then(Value::as_str), Some("abc"));
    }

    #[test]
    fn other_actions_map_to_their_commands() {
        let cases = [
            (Action::Stop("r1/t1".into()), "task.stop"),
            (Action::Retry("r1/t1".into()), "task.retry"),
            (Action::Merge("r1/t1".into()), "merge"),
        ];
        for (action, expected) in cases {
            let (_, cmd, args) = request_for(&action).expect("request");
            assert_eq!(cmd, expected);
            assert_eq!(args.get("task").and_then(Value::as_str), Some("r1/t1"));
        }
        let (_, cmd, args) = request_for(&Action::ResolveGate {
            id: "g1".into(),
            answer: "sql".into(),
        })
        .expect("request");
        assert_eq!(cmd, "gate.resolve");
        assert_eq!(args.get("answer").and_then(Value::as_str), Some("sql"));
    }

    #[test]
    fn local_actions_send_no_request() {
        for a in [
            Action::None,
            Action::Quit,
            Action::Refresh,
            Action::OpenAttach("r1/t1".into()),
        ] {
            assert!(request_for(&a).is_none());
        }
    }

    #[test]
    fn error_text_includes_code_and_subject() {
        let e =
            FleetError::new(ErrorCode::ApprovalRequired, "merge needs approval").with_subject("a7");
        assert_eq!(
            describe_error("merge r1/t1", &e),
            "merge r1/t1: merge needs approval (approval_required) [a7]"
        );
        let plain = FleetError::conflict("busy");
        assert_eq!(describe_error("stop x", &plain), "stop x: busy (conflict)");
    }

    #[test]
    fn only_connection_failures_trigger_a_reconnect() {
        assert!(is_transport_error(&FleetError::new(
            ErrorCode::DaemonDown,
            "x"
        )));
        assert!(is_transport_error(&FleetError::internal("broken pipe")));
        assert!(!is_transport_error(&FleetError::conflict("x")));
        assert!(!is_transport_error(&FleetError::not_found("x")));
        assert!(!is_transport_error(&FleetError::new(
            ErrorCode::ApprovalRequired,
            "x"
        )));
    }

    #[test]
    fn polls_are_due_after_their_interval() {
        assert!(App::due(None, STATUS_EVERY));
        assert!(!App::due(Some(Instant::now()), Duration::from_secs(3600)));
        assert!(App::due(Some(Instant::now()), Duration::ZERO));
    }
}
