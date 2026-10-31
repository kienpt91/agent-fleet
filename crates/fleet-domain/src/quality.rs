//! Quality gates: how the result of a configured command decides whether a task may complete.
//!
//! Running the command is an adapter concern. This module judges what came back: how the process
//! ended ([`ExitKind`]) and, for the `refcheck-json` format, what its JSON said.

use crate::limits::GATE_FEEDBACK_BYTES;
use crate::sanitize::{sanitize_text, tail_str};
use crate::value::Value;

const MAX_FINDINGS_SHOWN: usize = 20;

/// How a gate command ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExitKind {
    Code(i32),
    /// Terminated by a signal (including our own kill).
    Signal(i32),
    TimedOut,
    /// Printed more than the capture limit and was killed.
    OutputLimit,
    SpawnFailed(String),
}

/// Parsed form of a gate's output format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateFormat {
    /// Pass when the exit code is 0.
    Exit,
    /// Pass when stdout is a `refcheck` JSON document without errors.
    RefcheckJson,
}

impl GateFormat {
    pub fn parse(s: &str) -> Option<GateFormat> {
        match s {
            "exit" => Some(GateFormat::Exit),
            "refcheck-json" => Some(GateFormat::RefcheckJson),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            GateFormat::Exit => "exit",
            GateFormat::RefcheckJson => "refcheck-json",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateOutcome {
    pub name: String,
    pub passed: bool,
    pub summary: String,
    /// Sanitized tail of the output, bounded.
    pub output: String,
}

fn failed_unless_clean_exit(name: &str, exit: &ExitKind, output: &str) -> Option<GateOutcome> {
    let summary = match exit {
        ExitKind::Code(_) => return None,
        ExitKind::Signal(n) => format!("killed by signal {n}"),
        ExitKind::TimedOut => "timed out".to_string(),
        ExitKind::OutputLimit => "printed more output than allowed and was stopped".to_string(),
        ExitKind::SpawnFailed(why) => format!("could not start: {}", sanitize_text(why.as_bytes())),
    };
    Some(GateOutcome {
        name: name.to_string(),
        passed: false,
        summary,
        output: clean_tail(output),
    })
}

fn clean_tail(output: &str) -> String {
    tail_str(&sanitize_text(output.as_bytes()), GATE_FEEDBACK_BYTES).to_string()
}

/// Judges a gate whose verdict is its exit code.
pub fn evaluate_exit(name: &str, exit: &ExitKind, output: &str) -> GateOutcome {
    if let Some(bad) = failed_unless_clean_exit(name, exit, output) {
        return bad;
    }
    let code = match exit {
        ExitKind::Code(c) => *c,
        _ => -1,
    };
    GateOutcome {
        name: name.to_string(),
        passed: code == 0,
        summary: if code == 0 {
            "exit code 0".to_string()
        } else {
            format!("exit code {code}")
        },
        output: clean_tail(output),
    }
}

/// Judges a gate in `refcheck-json` format. `parsed` is the result of parsing stdout as one JSON
/// document; the contract is `{"version":1,"summary":{"errors":N},"findings":[...]}`.
///
/// The gate fails when: the process was killed, timed out or could not start; stdout is not JSON;
/// the document has the wrong shape or version; `summary.errors` is not a non-negative integer;
/// errors are reported (also when the findings contain more errors than the summary claims); the
/// exit code is neither 0 nor 1; or the exit code contradicts the report.
pub fn evaluate_refcheck(
    name: &str,
    parsed: Result<&Value, &str>,
    exit: &ExitKind,
    raw_output: &str,
) -> GateOutcome {
    if let Some(bad) = failed_unless_clean_exit(name, exit, raw_output) {
        return bad;
    }
    let code = match exit {
        ExitKind::Code(c) => *c,
        _ => -1,
    };
    let fail = |summary: String, output: String| GateOutcome {
        name: name.to_string(),
        passed: false,
        summary,
        output,
    };
    let doc = match parsed {
        Ok(v) => v,
        Err(why) => {
            return fail(
                format!(
                    "output is not valid JSON ({})",
                    sanitize_text(why.as_bytes())
                ),
                clean_tail(raw_output),
            )
        }
    };
    if doc.as_object().is_none() {
        return fail("JSON is not an object".into(), clean_tail(raw_output));
    }
    if doc.get("version").and_then(Value::as_u64) != Some(1) {
        return fail(
            "JSON does not match the refcheck contract: version must be 1".into(),
            clean_tail(raw_output),
        );
    }
    let Some(summary_errors) = doc
        .get("summary")
        .and_then(|s| s.get("errors"))
        .and_then(Value::as_u64)
    else {
        return fail(
            "JSON does not match the refcheck contract: summary.errors must be a non-negative integer".into(),
            clean_tail(raw_output),
        );
    };
    let Some(findings) = doc.get("findings").and_then(Value::as_array) else {
        return fail(
            "JSON does not match the refcheck contract: findings must be an array".into(),
            clean_tail(raw_output),
        );
    };
    let reported = findings
        .iter()
        .filter(|f| f.get("severity").and_then(Value::as_str) == Some("error"))
        .count() as u64;
    let errors = summary_errors.max(reported);
    let mut lines: Vec<String> = findings
        .iter()
        .take(MAX_FINDINGS_SHOWN)
        .map(describe_finding)
        .collect();
    if findings.len() > MAX_FINDINGS_SHOWN {
        lines.push(format!(
            "... and {} more findings",
            findings.len() - MAX_FINDINGS_SHOWN
        ));
    }
    let output = tail_str(&lines.join("\n"), GATE_FEEDBACK_BYTES).to_string();

    let mut summary = format!("{errors} error(s)");
    if reported > summary_errors {
        summary.push_str(" (summary.errors is lower than the findings)");
    }
    if !(0..=1).contains(&code) {
        return fail(format!("{summary}; exit code {code}"), output);
    }
    if errors > 0 {
        return fail(summary, output);
    }
    if code != 0 {
        return fail(
            format!("{summary}; exit code {code} contradicts the report"),
            output,
        );
    }
    GateOutcome {
        name: name.to_string(),
        passed: true,
        summary,
        output,
    }
}

fn describe_finding(f: &Value) -> String {
    let text = |key: &str| {
        f.get(key)
            .and_then(Value::as_str)
            .map(|s| sanitize_text(s.as_bytes()))
            .unwrap_or_else(|| "?".to_string())
    };
    let num = |key: &str| {
        f.get(key)
            .and_then(Value::as_u64)
            .map_or_else(|| "?".to_string(), |n| n.to_string())
    };
    format!(
        "{}:{}:{} {} {}: {}",
        text("file"),
        num("line"),
        num("column"),
        text("severity"),
        text("rule"),
        text("message")
    )
}

/// Text appended to a task's brief when its gates failed.
pub fn gate_feedback(outcomes: &[GateOutcome]) -> String {
    let mut parts = Vec::new();
    for o in outcomes.iter().filter(|o| !o.passed) {
        let mut part = format!("Quality gate '{}' failed: {}", o.name, o.summary);
        if !o.output.trim().is_empty() {
            part.push('\n');
            part.push_str(&o.output);
        }
        parts.push(part);
    }
    tail_str(&parts.join("\n\n"), GATE_FEEDBACK_BYTES * 2).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(errors: Value, findings: Value) -> Value {
        Value::object([
            ("version", Value::int(1)),
            ("summary", Value::object([("errors", errors)])),
            ("findings", findings),
        ])
    }

    fn finding(sev: &str) -> Value {
        Value::object([
            ("file", Value::str("a.ts")),
            ("line", Value::int(3)),
            ("column", Value::int(1)),
            ("severity", Value::str(sev)),
            ("rule", Value::str("missing-export")),
            ("message", Value::str("nope")),
        ])
    }

    fn refcheck(v: &Value, code: i32) -> GateOutcome {
        evaluate_refcheck("refcheck", Ok(v), &ExitKind::Code(code), "raw")
    }

    #[test]
    fn exit_gate_passes_only_on_zero() {
        assert!(evaluate_exit("t", &ExitKind::Code(0), "ok").passed);
        let f = evaluate_exit("t", &ExitKind::Code(2), "boom");
        assert!(!f.passed);
        assert_eq!(f.summary, "exit code 2");
        assert_eq!(f.output, "boom");
    }

    #[test]
    fn abnormal_endings_fail_both_formats() {
        for exit in [
            ExitKind::Signal(9),
            ExitKind::TimedOut,
            ExitKind::OutputLimit,
            ExitKind::SpawnFailed("no such file".into()),
        ] {
            assert!(!evaluate_exit("t", &exit, "").passed, "{exit:?}");
            let v = doc(Value::int(0), Value::array([]));
            assert!(
                !evaluate_refcheck("t", Ok(&v), &exit, "").passed,
                "{exit:?}"
            );
        }
    }

    #[test]
    fn clean_refcheck_report_passes() {
        let v = doc(Value::int(0), Value::array([finding("warning")]));
        let o = refcheck(&v, 0);
        assert!(o.passed, "{}", o.summary);
        assert!(o.output.contains("a.ts:3:1 warning missing-export: nope"));
    }

    #[test]
    fn errors_fail() {
        let v = doc(
            Value::int(2),
            Value::array([finding("error"), finding("error")]),
        );
        let o = refcheck(&v, 1);
        assert!(!o.passed);
        assert!(o.summary.starts_with("2 error(s)"));
    }

    #[test]
    fn summary_that_understates_findings_is_not_trusted() {
        let v = doc(Value::int(0), Value::array([finding("error")]));
        let o = refcheck(&v, 0);
        assert!(!o.passed);
        assert!(o.summary.contains("lower than the findings"));
    }

    #[test]
    fn unparseable_output_fails() {
        let o = evaluate_refcheck(
            "r",
            Err("expected value at line 1"),
            &ExitKind::Code(0),
            "oops",
        );
        assert!(!o.passed);
        assert!(o.summary.contains("not valid JSON"));
        assert_eq!(o.output, "oops");
    }

    #[test]
    fn wrong_shapes_fail() {
        let cases = [
            Value::array([]),
            Value::Null,
            Value::object([("version", Value::int(1))]),
            Value::object([
                ("version", Value::int(2)),
                ("summary", Value::object([("errors", Value::int(0))])),
                ("findings", Value::array([])),
            ]),
            Value::object([
                ("summary", Value::object([("errors", Value::int(0))])),
                ("findings", Value::array([])),
            ]),
            doc(Value::str("0"), Value::array([])),
            doc(Value::Float(0.0), Value::array([])),
            doc(Value::int(-1), Value::array([])),
            doc(Value::Bool(false), Value::array([])),
            doc(Value::Null, Value::array([])),
            doc(Value::int(0), Value::Null),
            doc(Value::int(0), Value::Object(Default::default())),
            Value::object([
                ("version", Value::str("1")),
                ("summary", Value::object([("errors", Value::int(0))])),
                ("findings", Value::array([])),
            ]),
        ];
        for (i, v) in cases.iter().enumerate() {
            let o = refcheck(v, 0);
            assert!(!o.passed, "case {i} passed: {v:?}");
        }
    }

    #[test]
    fn exit_codes_must_match_the_report() {
        let clean = doc(Value::int(0), Value::array([]));
        assert!(!refcheck(&clean, 1).passed);
        assert!(!refcheck(&clean, 2).passed);
        assert!(!refcheck(&clean, 127).passed);
        assert!(!refcheck(&clean, -1).passed);
        let dirty = doc(Value::int(1), Value::array([finding("error")]));
        assert!(!refcheck(&dirty, 0).passed);
        assert!(!refcheck(&dirty, 2).passed);
    }

    #[test]
    fn huge_error_counts_do_not_overflow() {
        let v = doc(Value::Int(i128::from(u64::MAX)), Value::array([]));
        assert!(!refcheck(&v, 1).passed);
    }

    #[test]
    fn many_findings_are_truncated_and_sanitized() {
        let mut list: Vec<Value> = (0..50).map(|_| finding("warning")).collect();
        list.push(Value::object([
            ("file", Value::str("\u{1b}[31mevil")),
            ("message", Value::str("x\u{202e}y")),
            ("severity", Value::str("warning")),
        ]));
        let o = refcheck(&doc(Value::int(0), Value::array(list)), 0);
        assert!(o.passed);
        assert!(o.output.contains("and 31 more findings"));
        let hostile = refcheck(
            &doc(
                Value::int(0),
                Value::array([Value::object([("file", Value::str("\u{1b}[31mevil"))])]),
            ),
            0,
        );
        assert!(!hostile.output.contains('\u{1b}'));
    }

    #[test]
    fn non_object_findings_entries_do_not_panic() {
        let v = doc(
            Value::int(0),
            Value::array([Value::int(1), Value::Null, Value::str("x")]),
        );
        assert!(refcheck(&v, 0).passed);
    }

    #[test]
    fn feedback_lists_only_failed_gates_and_is_bounded() {
        let outcomes = vec![
            GateOutcome {
                name: "lint".into(),
                passed: true,
                summary: "ok".into(),
                output: String::new(),
            },
            GateOutcome {
                name: "tests".into(),
                passed: false,
                summary: "exit code 1".into(),
                output: "x".repeat(100_000),
            },
        ];
        let text = gate_feedback(&outcomes);
        assert!(text.len() <= GATE_FEEDBACK_BYTES * 2);
        assert!(!text.contains("lint"));
        assert!(gate_feedback(&outcomes[..1]).is_empty());
    }

    #[test]
    fn format_names() {
        assert_eq!(GateFormat::parse("exit"), Some(GateFormat::Exit));
        assert_eq!(
            GateFormat::parse("refcheck-json"),
            Some(GateFormat::RefcheckJson)
        );
        assert_eq!(GateFormat::parse("json"), None);
        assert_eq!(GateFormat::RefcheckJson.as_str(), "refcheck-json");
    }
}
