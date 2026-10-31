//! Dashboard state, built from `fleet.status/1` documents.
//!
//! The daemon is trusted to be well formed but not to be sane: every accessor here tolerates
//! missing or mistyped fields (they become defaults), and every string is sanitized and
//! length-capped on the way in, so rendering code can never be handed a control character.

use fleet_domain::sanitize::{head_str, sanitize_text};
use fleet_domain::value::Value;

const MAX_FIELD: usize = 400;
const MAX_ROWS: usize = 2048;
const MAX_LIST: usize = 64;
const MAX_TAIL_BYTES: usize = 64 * 1024;
/// Order of `counts` in [`Model::visible_counts`] and the header.
pub const STATUS_NAMES: [&str; 7] = [
    "pending",
    "ready",
    "running",
    "review",
    "completed",
    "failed",
    "blocked",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Tasks,
    Gates,
    Approvals,
    Output,
}

impl Focus {
    pub fn next(self) -> Focus {
        match self {
            Focus::Tasks => Focus::Gates,
            Focus::Gates => Focus::Approvals,
            Focus::Approvals => Focus::Output,
            Focus::Output => Focus::Tasks,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Focus::Tasks => "tasks",
            Focus::Gates => "gates",
            Focus::Approvals => "approvals",
            Focus::Output => "output",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DaemonInfo {
    pub pid: u64,
    pub version: String,
    pub uptime_ms: u64,
    pub sessions: u64,
    pub max_sessions: u64,
    pub undo: String,
    pub rss_kb: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunRow {
    pub id: String,
    pub objective: String,
    pub status: String,
    pub max_parallel: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BlockInfo {
    pub kind: String,
    pub subject: String,
    pub detail: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GateResult {
    pub name: String,
    pub passed: bool,
    pub summary: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TaskRow {
    pub run: String,
    pub id: String,
    pub title: String,
    pub status: String,
    pub deps: Vec<String>,
    pub worker: Option<String>,
    pub host: Option<String>,
    pub attempt: u64,
    pub retries_used: u64,
    pub summary: Option<String>,
    pub failure: Option<String>,
    pub block: Option<BlockInfo>,
    pub gates: Vec<GateResult>,
    pub live: bool,
}

impl TaskRow {
    pub fn key(&self) -> String {
        format!("{}/{}", self.run, self.id)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DecisionGate {
    pub id: String,
    pub run: String,
    pub task: String,
    pub kind: String,
    pub text: String,
    pub choices: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ApprovalRow {
    pub id: String,
    pub run: String,
    pub task: String,
    pub purpose: String,
    pub display: String,
    pub digest: String,
    pub expires_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct EventLine {
    pub seq: u64,
    pub line: String,
}

#[derive(Debug)]
pub struct Model {
    pub daemon: DaemonInfo,
    pub now_ms: u64,
    pub runs: Vec<RunRow>,
    pub tasks: Vec<TaskRow>,
    /// Open decision gates only.
    pub gates: Vec<DecisionGate>,
    /// Pending approvals only.
    pub approvals: Vec<ApprovalRow>,
    pub events: Vec<EventLine>,
    pub selected: usize,
    pub gate_selected: usize,
    pub approval_selected: usize,
    pub focus: Focus,
    pub tail_key: Option<String>,
    pub tail: String,
    pub message: String,
    pub show_help: bool,
    /// Gate answer being typed.
    pub input: Option<String>,
    /// True once a status document has been applied.
    pub loaded: bool,
    pub connected: bool,
}

impl Default for Model {
    fn default() -> Self {
        Model {
            daemon: DaemonInfo::default(),
            now_ms: 0,
            runs: Vec::new(),
            tasks: Vec::new(),
            gates: Vec::new(),
            approvals: Vec::new(),
            events: Vec::new(),
            selected: 0,
            gate_selected: 0,
            approval_selected: 0,
            focus: Focus::Tasks,
            tail_key: None,
            tail: String::new(),
            message: String::new(),
            show_help: false,
            input: None,
            loaded: false,
            connected: true,
        }
    }
}

/// Plain single-line text: escapes removed, controls made visible, whitespace collapsed to
/// spaces and the length capped.
pub fn clean_line(s: &str) -> String {
    let text = sanitize_text(head_str(s, MAX_FIELD * 4).as_bytes());
    let flat: String = text
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    head_str(flat.trim(), MAX_FIELD).to_string()
}

fn text_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .map(clean_line)
        .unwrap_or_default()
}

fn opt_text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(clean_line)
}

fn num_of(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn list_of<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).unwrap_or(&[])
}

fn strings_of(v: &Value, key: &str) -> Vec<String> {
    list_of(v, key)
        .iter()
        .filter_map(Value::as_str)
        .take(MAX_LIST)
        .map(clean_line)
        .collect()
}

fn parse_task(run: &str, t: &Value) -> TaskRow {
    TaskRow {
        run: run.to_string(),
        id: text_of(t, "id"),
        title: text_of(t, "title"),
        status: text_of(t, "status"),
        deps: strings_of(t, "deps"),
        worker: opt_text(t, "worker"),
        host: opt_text(t, "host"),
        attempt: num_of(t, "attempt"),
        retries_used: num_of(t, "retries_used"),
        summary: opt_text(t, "summary"),
        failure: opt_text(t, "failure"),
        block: t
            .get("block")
            .filter(|b| b.as_object().is_some())
            .map(|b| BlockInfo {
                kind: text_of(b, "kind"),
                subject: text_of(b, "subject"),
                detail: text_of(b, "detail"),
            }),
        gates: list_of(t, "gates")
            .iter()
            .take(MAX_LIST)
            .map(|g| GateResult {
                name: text_of(g, "name"),
                passed: g.get("passed").and_then(Value::as_bool).unwrap_or(false),
                summary: text_of(g, "summary"),
            })
            .collect(),
        live: t.get("live").and_then(Value::as_bool).unwrap_or(false),
    }
}

fn clamp_index(index: usize, len: usize) -> usize {
    index.min(len.saturating_sub(1))
}

impl Model {
    pub fn new() -> Model {
        Model::default()
    }

    /// Replaces the daemon-derived state with `status`. Selection follows the selected task by
    /// key when it still exists and is clamped otherwise.
    pub fn update_status(&mut self, status: &Value) {
        let previous = self.selected_key();
        let previous_gate = self.gates.get(self.gate_selected).map(|g| g.id.clone());
        let previous_approval = self
            .approvals
            .get(self.approval_selected)
            .map(|a| a.id.clone());

        let d = status.get("daemon").cloned().unwrap_or(Value::Null);
        self.daemon = DaemonInfo {
            pid: num_of(&d, "pid"),
            version: text_of(&d, "version"),
            uptime_ms: num_of(&d, "uptime_ms"),
            sessions: num_of(&d, "sessions"),
            max_sessions: num_of(&d, "max_sessions"),
            undo: text_of(&d, "undo"),
            rss_kb: d.get("rss_kb").and_then(Value::as_u64),
        };
        self.now_ms = num_of(status, "now_ms");

        self.runs.clear();
        self.tasks.clear();
        for run in list_of(status, "runs").iter().take(MAX_LIST) {
            let id = text_of(run, "id");
            self.runs.push(RunRow {
                id: id.clone(),
                objective: text_of(run, "objective"),
                status: text_of(run, "status"),
                max_parallel: num_of(run, "max_parallel"),
            });
            for t in list_of(run, "tasks") {
                if self.tasks.len() >= MAX_ROWS {
                    break;
                }
                self.tasks.push(parse_task(&id, t));
            }
        }

        self.gates = list_of(status, "gates")
            .iter()
            .filter(|g| g.get("open").and_then(Value::as_bool).unwrap_or(false))
            .take(MAX_ROWS)
            .map(|g| DecisionGate {
                id: text_of(g, "id"),
                run: text_of(g, "run"),
                task: text_of(g, "task"),
                kind: text_of(g, "kind"),
                text: text_of(g, "text"),
                choices: strings_of(g, "choices"),
            })
            .collect();
        self.approvals = list_of(status, "approvals")
            .iter()
            .filter(|a| a.get("status").and_then(Value::as_str) == Some("pending"))
            .take(MAX_ROWS)
            .map(|a| ApprovalRow {
                id: text_of(a, "id"),
                run: text_of(a, "run"),
                task: text_of(a, "task"),
                purpose: text_of(a, "purpose"),
                display: text_of(a, "display"),
                digest: text_of(a, "digest"),
                expires_ms: num_of(a, "expires_ms"),
            })
            .collect();
        self.events = list_of(status, "recent")
            .iter()
            .take(MAX_ROWS)
            .map(|e| EventLine {
                seq: num_of(e, "seq"),
                line: text_of(e, "line"),
            })
            .collect();

        self.selected = previous
            .and_then(|k| self.tasks.iter().position(|t| t.key() == k))
            .unwrap_or(self.selected);
        self.selected = clamp_index(self.selected, self.tasks.len());
        self.gate_selected = previous_gate
            .and_then(|id| self.gates.iter().position(|g| g.id == id))
            .unwrap_or(self.gate_selected);
        self.gate_selected = clamp_index(self.gate_selected, self.gates.len());
        self.approval_selected = previous_approval
            .and_then(|id| self.approvals.iter().position(|a| a.id == id))
            .unwrap_or(self.approval_selected);
        self.approval_selected = clamp_index(self.approval_selected, self.approvals.len());

        if self.selected_key() != self.tail_key {
            self.tail_key = None;
            self.tail.clear();
        }
        self.loaded = true;
        self.connected = true;
    }

    pub fn selected_task(&self) -> Option<&TaskRow> {
        self.tasks.get(self.selected)
    }

    pub fn selected_key(&self) -> Option<String> {
        self.selected_task().map(TaskRow::key)
    }

    pub fn selected_gate(&self) -> Option<&DecisionGate> {
        self.gates.get(self.gate_selected)
    }

    pub fn selected_approval(&self) -> Option<&ApprovalRow> {
        self.approvals.get(self.approval_selected)
    }

    /// Moves the selection of the focused list by `delta`, clamped to the list.
    pub fn move_selection(&mut self, delta: i32) {
        let (index, len) = match self.focus {
            Focus::Tasks | Focus::Output => (&mut self.selected, self.tasks.len()),
            Focus::Gates => (&mut self.gate_selected, self.gates.len()),
            Focus::Approvals => (&mut self.approval_selected, self.approvals.len()),
        };
        let step = usize::try_from(delta.unsigned_abs()).unwrap_or(usize::MAX);
        let moved = if delta < 0 {
            index.saturating_sub(step)
        } else {
            index.saturating_add(step)
        };
        *index = clamp_index(moved, len);
    }

    /// Stores the tail text of the task `key`. Text for a task that is no longer selected is
    /// dropped (a slow poll must not show the previous task's output).
    pub fn set_tail(&mut self, key: &str, text: &str) {
        if self.selected_key().as_deref() != Some(key) {
            return;
        }
        let kept = fleet_domain::sanitize::tail_str(text, MAX_TAIL_BYTES);
        self.tail = sanitize_text(kept.as_bytes());
        self.tail_key = Some(key.to_string());
    }

    pub fn set_message(&mut self, text: &str) {
        self.message = clean_line(text);
    }

    /// Task counts by status, in [`STATUS_NAMES`] order.
    pub fn visible_counts(&self) -> [usize; 7] {
        let mut counts = [0usize; 7];
        for t in &self.tasks {
            if let Some(i) = STATUS_NAMES.iter().position(|n| *n == t.status) {
                if let Some(c) = counts.get_mut(i) {
                    *c += 1;
                }
            }
        }
        counts
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use fleet_domain::value::Value;

    pub fn task(id: &str, title: &str, status: &str, deps: &[&str]) -> Value {
        Value::object([
            ("id", Value::str(id)),
            ("title", Value::str(title)),
            ("status", Value::str(status)),
            ("deps", Value::array(deps.iter().map(|d| Value::str(*d)))),
            ("worker", Value::str("coder")),
            ("attempt", Value::int(1)),
            ("retries_used", Value::int(0)),
            ("block", Value::Null),
            ("gates", Value::array([])),
            ("live", Value::Bool(status == "running")),
        ])
    }

    pub fn status(tasks: Vec<Value>) -> Value {
        Value::object([
            ("schema", Value::str("fleet.status/1")),
            (
                "daemon",
                Value::object([
                    ("pid", Value::int(42)),
                    ("version", Value::str("0.1.0")),
                    ("uptime_ms", Value::int(5000)),
                    ("sessions", Value::int(2)),
                    ("max_sessions", Value::int(8)),
                    ("undo", Value::str("available")),
                    ("rss_kb", Value::int(20480)),
                ]),
            ),
            ("now_ms", Value::int(1_000_000)),
            (
                "runs",
                Value::array([Value::object([
                    ("id", Value::str("r1")),
                    ("objective", Value::str("ship it")),
                    ("status", Value::str("active")),
                    ("max_parallel", Value::int(2)),
                    ("tasks", Value::array(tasks)),
                ])]),
            ),
            ("gates", Value::array([])),
            ("approvals", Value::array([])),
            ("recent", Value::array([])),
        ])
    }

    pub fn approval(id: &str, digest: &str) -> Value {
        Value::object([
            ("id", Value::str(id)),
            ("run", Value::str("r1")),
            ("task", Value::str("t1")),
            ("purpose", Value::str("merge")),
            ("display", Value::str("merge r1/t1")),
            ("digest", Value::str(digest)),
            ("status", Value::str("pending")),
            ("expires_ms", Value::int(1_090_000)),
        ])
    }

    pub fn gate(id: &str, text: &str, open: bool) -> Value {
        Value::object([
            ("id", Value::str(id)),
            ("run", Value::str("r1")),
            ("task", Value::str("t1")),
            ("kind", Value::str("question")),
            ("text", Value::str(text)),
            ("choices", Value::array([])),
            ("open", Value::Bool(open)),
        ])
    }

    pub fn with(status: Value, key: &str, items: Vec<Value>) -> Value {
        let Value::Object(mut m) = status else {
            return status;
        };
        m.insert(key.to_string(), Value::Array(items));
        Value::Object(m)
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn three() -> Value {
        status(vec![
            task("t1", "one", "running", &[]),
            task("t2", "two", "pending", &["t1"]),
            task("t3", "three", "completed", &[]),
        ])
    }

    #[test]
    fn builds_rows_and_counts() {
        let mut m = Model::new();
        m.update_status(&three());
        assert_eq!(m.tasks.len(), 3);
        assert_eq!(m.selected_key().as_deref(), Some("r1/t1"));
        assert_eq!(m.visible_counts(), [1, 0, 1, 0, 1, 0, 0]);
        assert_eq!(m.daemon.max_sessions, 8);
        assert_eq!(
            m.tasks.get(1).map(|t| t.deps.clone()),
            Some(vec!["t1".to_string()])
        );
    }

    #[test]
    fn only_open_gates_and_pending_approvals_are_kept() {
        let s = with(
            three(),
            "gates",
            vec![gate("g1", "q?", true), gate("g2", "old", false)],
        );
        let mut denied = approval("a2", "x");
        if let Value::Object(m) = &mut denied {
            m.insert("status".into(), Value::str("denied"));
        }
        let s = with(s, "approvals", vec![approval("a1", "d1"), denied]);
        let mut m = Model::new();
        m.update_status(&s);
        assert_eq!(m.gates.len(), 1);
        assert_eq!(m.approvals.len(), 1);
        assert_eq!(m.approvals.first().map(|a| a.id.as_str()), Some("a1"));
    }

    #[test]
    fn selection_clamps_at_both_ends() {
        let mut m = Model::new();
        m.update_status(&three());
        m.move_selection(-1);
        assert_eq!(m.selected, 0);
        m.move_selection(1);
        m.move_selection(1);
        m.move_selection(1);
        m.move_selection(i32::MAX);
        assert_eq!(m.selected, 2);
        m.move_selection(i32::MIN);
        assert_eq!(m.selected, 0);
    }

    #[test]
    fn selection_follows_the_task_and_survives_removal() {
        let mut m = Model::new();
        m.update_status(&three());
        m.move_selection(1);
        m.move_selection(1);
        assert_eq!(m.selected_key().as_deref(), Some("r1/t3"));
        // t1 disappears: the selection stays on t3 although its index changed.
        m.update_status(&status(vec![
            task("t2", "two", "pending", &[]),
            task("t3", "three", "completed", &[]),
        ]));
        assert_eq!(m.selected_key().as_deref(), Some("r1/t3"));
        // t3 disappears too: clamped to the last remaining row.
        m.update_status(&status(vec![task("t2", "two", "pending", &[])]));
        assert_eq!(m.selected_key().as_deref(), Some("r1/t2"));
        m.update_status(&status(vec![]));
        assert!(m.selected_task().is_none());
        assert_eq!(m.selected, 0);
        m.move_selection(5);
        assert_eq!(m.selected, 0);
    }

    #[test]
    fn tail_for_another_task_is_ignored_and_cleared_on_switch() {
        let mut m = Model::new();
        m.update_status(&three());
        m.set_tail("r1/t2", "stale");
        assert!(m.tail.is_empty());
        m.set_tail("r1/t1", "fresh\n");
        assert_eq!(m.tail, "fresh\n");
        m.move_selection(1);
        m.update_status(&three());
        assert!(m.tail.is_empty());
    }

    #[test]
    fn hostile_strings_are_neutralised_at_ingestion() {
        let evil = "a\x1b]0;pwn\x07b\x1b[2Jc\u{202e}d\ne";
        let mut m = Model::new();
        m.update_status(&status(vec![task("t1", evil, "running", &[])]));
        let title = m.tasks.first().map(|t| t.title.clone()).unwrap_or_default();
        assert!(
            !title.chars().any(|c| c.is_control() || c == '\u{202e}'),
            "{title:?}"
        );
        m.set_tail("r1/t1", "x\x1b[31m\x07\u{202e}y");
        assert!(!m
            .tail
            .chars()
            .any(|c| (c.is_control() && c != '\n') || c == '\u{202e}'));
    }

    #[test]
    fn garbage_documents_never_panic() {
        let mut m = Model::new();
        for v in [
            Value::Null,
            Value::Bool(true),
            Value::int(7),
            Value::str("x"),
            Value::array([Value::Null, Value::int(1)]),
            Value::object([
                ("runs", Value::int(1)),
                ("gates", Value::str("g")),
                ("daemon", Value::array([])),
            ]),
            Value::object([(
                "runs",
                Value::array([Value::Null, Value::object([("tasks", Value::str("t"))])]),
            )]),
            Value::object([(
                "runs",
                Value::array([Value::object([(
                    "tasks",
                    Value::array([Value::object([
                        ("id", Value::int(1)),
                        ("deps", Value::str("x")),
                        ("block", Value::str("b")),
                        ("gates", Value::array([Value::Null])),
                        ("attempt", Value::str("many")),
                    ])]),
                )])]),
            )]),
        ] {
            m.update_status(&v);
            let _ = m.visible_counts();
            let _ = m.selected_key();
        }
        assert!(m.loaded);
    }

    #[test]
    fn missing_fields_become_defaults() {
        let mut m = Model::new();
        m.update_status(&Value::object([(
            "runs",
            Value::array([Value::object([
                ("id", Value::str("r9")),
                (
                    "tasks",
                    Value::array([Value::object([("id", Value::str("tx"))])]),
                ),
            ])]),
        )]));
        let t = m.selected_task().cloned().unwrap_or_default();
        assert_eq!(t.key(), "r9/tx");
        assert_eq!(t.status, "");
        assert_eq!(t.attempt, 0);
        assert!(t.block.is_none() && t.worker.is_none());
        assert_eq!(m.daemon.rss_kb, None);
    }

    #[test]
    fn row_count_is_bounded() {
        let many: Vec<Value> = (0..(MAX_ROWS + 50))
            .map(|i| task(&format!("t{i}"), "x", "pending", &[]))
            .collect();
        let mut m = Model::new();
        m.update_status(&status(many));
        assert_eq!(m.tasks.len(), MAX_ROWS);
    }

    #[test]
    fn a_task_host_is_parsed_when_present() {
        let mut t = task("t1", "x", "pending", &[]);
        if let Value::Object(mp) = &mut t {
            mp.insert("host".into(), Value::str("build1"));
        }
        let mut m = Model::new();
        m.update_status(&status(vec![t, task("t2", "x", "pending", &[])]));
        assert_eq!(m.tasks[0].host.as_deref(), Some("build1"));
        assert_eq!(m.tasks[1].host, None);
    }

    #[test]
    fn focus_cycles_through_all_panels() {
        let mut f = Focus::Tasks;
        let mut seen = Vec::new();
        for _ in 0..4 {
            f = f.next();
            seen.push(f);
        }
        assert_eq!(
            seen,
            vec![Focus::Gates, Focus::Approvals, Focus::Output, Focus::Tasks]
        );
    }
}
