//! The worker contract: what a worker may tell the orchestrator through files in `.fleet/`.
//!
//! Reading the files safely is an adapter concern (regular files only, size caps, no symlinks).
//! This module validates what was read. Everything here is worker-controlled input.
//!
//! `result.json`:   `{"version":1,"status":"done"|"failed","summary":"..."}`
//! `question.json`: `{"version":1,"kind":"question"|"approval","text":"...","choices":["..."],
//!                    "action":{"command":"/absolute/path","args":["..."]}}`
//! `handoff.json`:  `{"version":1,"summary":"..."}`: a summary of the state of the work for whoever
//!                    continues it. Stored and shown as UNVERIFIED; never a source of truth.

use crate::ids::is_invisible;
use crate::limits::{MAX_ACTION_ARGS, MAX_ACTION_ARG_LEN, MAX_CHOICES, MAX_SUMMARY};
use crate::sanitize::{head_str, sanitize_text};
use crate::value::Value;

const MAX_QUESTION_TEXT: usize = 2000;
const MAX_CHOICE_LEN: usize = 200;
const MAX_COMMAND_LEN: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultStatus {
    Done,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerResult {
    pub status: ResultStatus,
    pub summary: String,
}

/// Outcome of reading a contract file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContractRead<T> {
    Absent,
    Valid(T),
    /// Present but unusable; the text says why and is safe to show.
    Invalid(String),
}

fn check_version(v: &Value) -> Result<(), String> {
    match v.get("version").and_then(Value::as_u64) {
        Some(1) => Ok(()),
        _ => Err("version must be 1".to_string()),
    }
}

fn clip_text(s: &str, max: usize) -> String {
    head_str(sanitize_text(s.as_bytes()).trim(), max).to_string()
}

impl WorkerResult {
    pub fn from_value(v: &Value) -> Result<WorkerResult, String> {
        if v.as_object().is_none() {
            return Err("not a JSON object".to_string());
        }
        check_version(v)?;
        let status = match v.get("status").and_then(Value::as_str) {
            Some("done") => ResultStatus::Done,
            Some("failed") => ResultStatus::Failed,
            _ => return Err("status must be \"done\" or \"failed\"".to_string()),
        };
        let summary = match v.get("summary") {
            None | Some(Value::Null) => String::new(),
            Some(Value::Str(s)) => clip_text(s, MAX_SUMMARY),
            Some(_) => return Err("summary must be a string".to_string()),
        };
        Ok(WorkerResult { status, summary })
    }
}

/// The summary a worker may leave for the next attempt (`.fleet/handoff.json`). Worker-written, so
/// it is only ever shown labelled UNVERIFIED and quoted; it decides nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerHandoff {
    pub summary: String,
}

impl WorkerHandoff {
    pub fn from_value(v: &Value) -> Result<WorkerHandoff, String> {
        if v.as_object().is_none() {
            return Err("not a JSON object".to_string());
        }
        check_version(v)?;
        match v.get("summary") {
            Some(Value::Str(s)) => {
                let summary = clip_text(s, MAX_SUMMARY);
                if summary.is_empty() {
                    Err("summary is empty".to_string())
                } else {
                    Ok(WorkerHandoff { summary })
                }
            }
            _ => Err("summary must be a string".to_string()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuestionKind {
    Question,
    Approval,
}

/// An action a worker asks permission for. The command is an absolute path; the label under which
/// the human sees it is fixed by fleet (`Action::command`), never chosen by the worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionRequest {
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerQuestion {
    pub kind: QuestionKind,
    pub text: String,
    pub choices: Vec<String>,
    pub action: Option<ActionRequest>,
}

/// Exact-text fields (commands, arguments) must survive unchanged because approvals are bound to
/// them, so anything that would be altered or hidden by display is rejected instead of cleaned.
fn exact_text(label: &str, s: &str, max: usize) -> Result<String, String> {
    if s.len() > max {
        return Err(format!("{label} is longer than {max} bytes"));
    }
    if s.chars().any(|c| c.is_control() || is_invisible(c)) {
        return Err(format!("{label} contains control or invisible characters"));
    }
    Ok(s.to_string())
}

impl WorkerQuestion {
    pub fn from_value(v: &Value) -> Result<WorkerQuestion, String> {
        if v.as_object().is_none() {
            return Err("not a JSON object".to_string());
        }
        check_version(v)?;
        let kind = match v.get("kind").and_then(Value::as_str) {
            Some("question") => QuestionKind::Question,
            Some("approval") => QuestionKind::Approval,
            _ => return Err("kind must be \"question\" or \"approval\"".to_string()),
        };
        let text = match v.get("text").and_then(Value::as_str) {
            Some(t) => clip_text(t, MAX_QUESTION_TEXT),
            None => return Err("text must be a string".to_string()),
        };
        if text.is_empty() {
            return Err("text is empty".to_string());
        }
        let mut choices = Vec::new();
        match v.get("choices") {
            None | Some(Value::Null) => {}
            Some(Value::Array(items)) => {
                if items.len() > MAX_CHOICES {
                    return Err(format!("more than {MAX_CHOICES} choices"));
                }
                for item in items {
                    let Some(s) = item.as_str() else {
                        return Err("choices must be strings".to_string());
                    };
                    let c = clip_text(s, MAX_CHOICE_LEN);
                    if c.is_empty() {
                        return Err("a choice is empty".to_string());
                    }
                    choices.push(c);
                }
            }
            Some(_) => return Err("choices must be an array".to_string()),
        }
        let action = match (kind, v.get("action")) {
            (_, None | Some(Value::Null)) => None,
            (QuestionKind::Question, Some(_)) => {
                return Err("only approval requests may carry an action".to_string())
            }
            (QuestionKind::Approval, Some(a)) => Some(parse_action(a)?),
        };
        Ok(WorkerQuestion {
            kind,
            text,
            choices,
            action,
        })
    }
}

fn parse_action(a: &Value) -> Result<ActionRequest, String> {
    if a.as_object().is_none() {
        return Err("action must be an object".to_string());
    }
    let command = match a.get("command").and_then(Value::as_str) {
        Some(c) if !c.trim().is_empty() => exact_text("action.command", c, MAX_COMMAND_LEN)?,
        _ => return Err("action.command must be a non-empty string".to_string()),
    };
    if !command.starts_with('/') {
        return Err("action.command must be an absolute path (it starts with '/')".to_string());
    }
    let mut args = Vec::new();
    match a.get("args") {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) => {
            if items.len() > MAX_ACTION_ARGS {
                return Err(format!(
                    "action.args has more than {MAX_ACTION_ARGS} entries"
                ));
            }
            for item in items {
                let Some(s) = item.as_str() else {
                    return Err("action.args must be strings".to_string());
                };
                args.push(exact_text("an action argument", s, MAX_ACTION_ARG_LEN)?);
            }
        }
        Some(_) => return Err("action.args must be an array".to_string()),
    }
    Ok(ActionRequest { command, args })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(extra: Vec<(&str, Value)>) -> Value {
        let mut pairs = vec![
            ("version", Value::int(1)),
            ("kind", Value::str("question")),
            ("text", Value::str("Which database?")),
        ];
        pairs.extend(extra);
        // later keys replace earlier ones
        Value::object(pairs)
    }

    #[test]
    fn result_parses() {
        let v = Value::object([
            ("version", Value::int(1)),
            ("status", Value::str("done")),
            ("summary", Value::str("added the endpoint")),
        ]);
        assert_eq!(
            WorkerResult::from_value(&v),
            Ok(WorkerResult {
                status: ResultStatus::Done,
                summary: "added the endpoint".into()
            })
        );
        let failed = Value::object([("version", Value::int(1)), ("status", Value::str("failed"))]);
        assert_eq!(
            WorkerResult::from_value(&failed).map(|r| r.status),
            Ok(ResultStatus::Failed)
        );
    }

    #[test]
    fn result_rejects_bad_shapes() {
        for v in [
            Value::Null,
            Value::array([]),
            Value::object([("status", Value::str("done"))]),
            Value::object([("version", Value::int(2)), ("status", Value::str("done"))]),
            Value::object([("version", Value::int(1)), ("status", Value::str("ok"))]),
            Value::object([("version", Value::int(1))]),
            Value::object([
                ("version", Value::int(1)),
                ("status", Value::str("done")),
                ("summary", Value::int(3)),
            ]),
        ] {
            assert!(WorkerResult::from_value(&v).is_err(), "{v:?}");
        }
    }

    #[test]
    fn result_summary_is_sanitized_and_clipped() {
        let v = Value::object([
            ("version", Value::int(1)),
            ("status", Value::str("done")),
            (
                "summary",
                Value::str(format!("\u{1b}[31m{}\u{202e}", "x".repeat(10_000))),
            ),
        ]);
        let r = WorkerResult::from_value(&v).unwrap();
        assert!(r.summary.len() <= MAX_SUMMARY);
        assert!(!r.summary.contains('\u{1b}'));
    }

    #[test]
    fn question_parses_with_choices() {
        let v = q(vec![(
            "choices",
            Value::array([Value::str("sqlite"), Value::str("postgres")]),
        )]);
        let parsed = WorkerQuestion::from_value(&v).unwrap();
        assert_eq!(parsed.kind, QuestionKind::Question);
        assert_eq!(parsed.choices, vec!["sqlite", "postgres"]);
        assert!(parsed.action.is_none());
    }

    #[test]
    fn approval_with_action_parses() {
        let v = Value::object([
            ("version", Value::int(1)),
            ("kind", Value::str("approval")),
            ("text", Value::str("publish the package")),
            (
                "action",
                Value::object([
                    ("command", Value::str("/usr/bin/npm")),
                    ("args", Value::array([Value::str("publish")])),
                ]),
            ),
        ]);
        let parsed = WorkerQuestion::from_value(&v).unwrap();
        let action = parsed.action.unwrap();
        assert_eq!(action.command, "/usr/bin/npm");
        assert_eq!(action.args, vec!["publish"]);
    }

    #[test]
    fn approval_without_action_is_allowed() {
        let v = Value::object([
            ("version", Value::int(1)),
            ("kind", Value::str("approval")),
            ("text", Value::str("ok to delete the cache?")),
        ]);
        assert!(WorkerQuestion::from_value(&v).unwrap().action.is_none());
    }

    #[test]
    fn question_rejects_bad_input() {
        let bad: Vec<Value> = vec![
            Value::Null,
            q(vec![("version", Value::int(3))]),
            q(vec![("kind", Value::str("order"))]),
            q(vec![("text", Value::str("   "))]),
            q(vec![("text", Value::int(1))]),
            q(vec![("choices", Value::str("a"))]),
            q(vec![("choices", Value::array([Value::int(1)]))]),
            q(vec![("choices", Value::array([Value::str("")]))]),
            q(vec![(
                "choices",
                Value::array((0..9).map(|i| Value::str(format!("c{i}")))),
            )]),
            q(vec![(
                "action",
                Value::object([("command", Value::str("ls"))]),
            )]),
        ];
        for v in bad {
            assert!(WorkerQuestion::from_value(&v).is_err(), "{v:?}");
        }
    }

    fn approval_with(action: Value) -> Value {
        Value::object([
            ("version", Value::int(1)),
            ("kind", Value::str("approval")),
            ("text", Value::str("run it")),
            ("action", action),
        ])
    }

    #[test]
    fn action_is_validated_exactly() {
        let bad = [
            Value::str("/bin/ls"),
            Value::object([("command", Value::str(""))]),
            Value::object([("command", Value::int(1))]),
            Value::object([("command", Value::str("/bin/ls\n-la"))]),
            Value::object([("command", Value::str("/bin/l\u{202e}s"))]),
            Value::object([
                ("command", Value::str("/bin/ls")),
                ("args", Value::str("x")),
            ]),
            Value::object([
                ("command", Value::str("/bin/ls")),
                ("args", Value::array([Value::int(1)])),
            ]),
            Value::object([
                ("command", Value::str("/bin/ls")),
                ("args", Value::array([Value::str("a\0b")])),
            ]),
            Value::object([
                ("command", Value::str("/bin/ls")),
                (
                    "args",
                    Value::array((0..65).map(|i| Value::str(format!("a{i}")))),
                ),
            ]),
            Value::object([("command", Value::str(format!("/{}", "x".repeat(5000))))]),
            Value::object([
                ("command", Value::str("/bin/ls")),
                ("args", Value::array([Value::str("y".repeat(5000))])),
            ]),
        ];
        for action in bad {
            assert!(
                WorkerQuestion::from_value(&approval_with(action.clone())).is_err(),
                "{action:?}"
            );
        }
    }

    #[test]
    fn the_command_must_be_an_absolute_path() {
        for relative in [
            "npm",
            "ls",
            "./run.sh",
            "../x",
            "bin/tool",
            "~/bin/tool",
            " /bin/ls",
            "$HOME/x",
            "C:\\x",
        ] {
            let err = WorkerQuestion::from_value(&approval_with(Value::object([(
                "command",
                Value::str(relative),
            )])))
            .unwrap_err();
            assert!(err.contains("absolute"), "{relative:?}: {err}");
        }
        for ok in ["/bin/ls", "/usr/local/bin/npm", "/"] {
            let a = WorkerQuestion::from_value(&approval_with(Value::object([(
                "command",
                Value::str(ok),
            )])))
            .unwrap()
            .action
            .unwrap();
            assert_eq!(a.command, ok);
        }
    }

    #[test]
    fn a_worker_chosen_tool_label_is_ignored() {
        let with_tool = approval_with(Value::object([
            ("command", Value::str("/bin/ls")),
            ("tool", Value::str("Operator Approved Safe Tool")),
        ]));
        let without = approval_with(Value::object([("command", Value::str("/bin/ls"))]));
        assert_eq!(
            WorkerQuestion::from_value(&with_tool).unwrap(),
            WorkerQuestion::from_value(&without).unwrap()
        );
    }

    #[test]
    fn exact_text_is_not_rewritten() {
        let v = approval_with(Value::object([
            ("command", Value::str("/bin/echo ")),
            ("args", Value::array([Value::str("a  b")])),
        ]));
        let a = WorkerQuestion::from_value(&v).unwrap().action.unwrap();
        assert_eq!(a.command, "/bin/echo ");
        assert_eq!(a.args, vec!["a  b"]);
    }

    #[test]
    fn question_text_is_sanitized() {
        let v = q(vec![("text", Value::str("go\u{1b}]0;x\u{7}\u{202e} on?"))]);
        let t = WorkerQuestion::from_value(&v).unwrap().text;
        assert!(!t.contains('\u{1b}') && !t.contains('\u{202e}'));
        assert!(t.starts_with("go"));
    }

    #[test]
    fn handoff_summary_parses_and_is_cleaned() {
        let v = Value::object([
            ("version", Value::int(1)),
            (
                "summary",
                Value::str("did \u{1b}[31mthe\u{1b}[0m work\u{202e}"),
            ),
        ]);
        let h = WorkerHandoff::from_value(&v).unwrap();
        assert!(h.summary.starts_with("did "));
        assert!(!h.summary.contains('\u{1b}') && !h.summary.contains('\u{202e}'));
        let long = Value::object([
            ("version", Value::int(1)),
            ("summary", Value::str("x".repeat(10_000))),
        ]);
        assert!(WorkerHandoff::from_value(&long).unwrap().summary.len() <= MAX_SUMMARY);
    }

    #[test]
    fn handoff_rejects_bad_shapes() {
        let bad = [
            Value::Null,
            Value::str("x"),
            Value::object([("summary", Value::str("s"))]),
            Value::object([("version", Value::int(2)), ("summary", Value::str("s"))]),
            Value::object([("version", Value::int(1))]),
            Value::object([("version", Value::int(1)), ("summary", Value::int(3))]),
            Value::object([("version", Value::int(1)), ("summary", Value::str("  \n "))]),
        ];
        for v in bad {
            assert!(WorkerHandoff::from_value(&v).is_err(), "{v:?}");
        }
    }
}
