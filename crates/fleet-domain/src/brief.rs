//! The brief a worker receives, and the substitution of placeholders in worker arguments.

use crate::error::{FleetError, Result};
use crate::ids::{RunId, TaskId};
use crate::limits::MAX_BRIEF_FILE_BYTES;
use crate::run::Task;
use crate::sanitize::head_str;

/// What a brief is built from.
pub struct BriefInput<'a> {
    pub run: &'a RunId,
    pub objective: &'a str,
    pub task: &'a Task,
    /// Completed dependencies: id, title and the worker's own summary when it gave one.
    pub dependencies: Vec<(TaskId, String, Option<String>)>,
    pub attempt: u32,
}

/// Plain-text brief written to `.fleet/brief.md` and offered to the worker command.
pub fn build_brief(input: &BriefInput<'_>) -> String {
    let t = input.task;
    let mut out = String::new();
    out.push_str(&format!("# Task {}: {}\n\n", t.id, t.title));
    out.push_str(&format!(
        "Run {} objective: {}\n\n",
        input.run, input.objective
    ));
    if input.attempt > 1 {
        out.push_str(&format!("This is attempt {}.\n\n", input.attempt));
    }
    out.push_str("## What to do\n\n");
    if t.spec.trim().is_empty() {
        out.push_str("(no further specification; use the title)\n\n");
    } else {
        out.push_str(t.spec.trim());
        out.push_str("\n\n");
    }
    if !input.dependencies.is_empty() {
        out.push_str("## Work this task builds on (already in your branch)\n\n");
        for (id, title, summary) in &input.dependencies {
            match summary {
                Some(s) => out.push_str(&format!("- {id} ({title}): {s}\n")),
                None => out.push_str(&format!("- {id} ({title})\n")),
            }
        }
        out.push('\n');
    }
    if !t.notes.is_empty() {
        out.push_str("## Earlier attempts and decisions\n\n");
        for note in &t.notes {
            out.push_str(note.trim());
            out.push_str("\n\n");
        }
    }
    out.push_str(CONTRACT);
    head_str(&out, MAX_BRIEF_FILE_BYTES).to_string()
}

const CONTRACT: &str = "## How to report back\n\n\
Work only inside the current directory; it is your own git worktree. Commit your work (or leave it \
uncommitted: it will be committed for you). Then, before you exit:\n\n\
- Write `.fleet/result.json`: `{\"version\":1,\"status\":\"done\",\"summary\":\"what you did\"}` \
(use `\"failed\"` if you could not do it).\n\
- To ask a human a question, write `.fleet/question.json`: \
`{\"version\":1,\"kind\":\"question\",\"text\":\"...\",\"choices\":[\"optional\",\"answers\"]}` and exit. \
You will be started again with the answer in this brief.\n\
- To ask permission for a command you must not run yourself, write `.fleet/question.json`: \
`{\"version\":1,\"kind\":\"approval\",\"text\":\"why\",\"action\":{\"command\":\"...\",\"args\":[\"...\"]}}` \
and exit. If a human approves it, fleet runs exactly that command and tells you the result.\n";

/// Placeholders a worker or gate argument may use.
pub const PLACEHOLDERS: [&str; 5] = ["brief", "task", "worktree", "run", "brief_file"];

/// Checks that a template only uses known placeholders and balanced braces.
pub fn validate_template(template: &str) -> Result<()> {
    expand_with(template, &mut |name| {
        if PLACEHOLDERS.contains(&name) {
            Ok(String::new())
        } else {
            Err(FleetError::invalid(format!(
                "unknown placeholder {{{name}}}"
            )))
        }
    })
    .map(|_| ())
}

/// Replaces `{name}` with its value. `{{` and `}}` are literal braces. An unknown name or an
/// unbalanced brace is an error. Values are inserted as-is (no shell is involved anywhere).
pub fn expand_placeholders(template: &str, vars: &[(&str, &str)]) -> Result<String> {
    expand_with(template, &mut |name| {
        vars.iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| (*v).to_string())
            .ok_or_else(|| FleetError::invalid(format!("unknown placeholder {{{name}}}")))
    })
}

fn expand_with(template: &str, lookup: &mut dyn FnMut(&str) -> Result<String>) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(ch) if ch.is_ascii_alphanumeric() || ch == '_' => name.push(ch),
                        _ => {
                            return Err(FleetError::invalid(
                                "unbalanced or malformed `{` in a template",
                            ))
                        }
                    }
                }
                out.push_str(&lookup(&name)?);
            }
            '}' => return Err(FleetError::invalid("unbalanced `}` in a template")),
            c => out.push(c),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::DomainEvent;
    use crate::run::test_support::{apply_all, nt, run_id, tid, with_tasks};

    fn task_with_notes(notes: &[&str]) -> Task {
        let mut run = with_tasks(1, vec![nt("a", &[])]);
        for n in notes {
            apply_all(
                &mut run,
                &[DomainEvent::NoteAdded {
                    run: run_id(),
                    task: tid("a"),
                    note: n.to_string(),
                }],
            );
        }
        run.task(&tid("a")).unwrap().clone()
    }

    #[test]
    fn brief_has_the_task_the_objective_and_the_contract() {
        let t = task_with_notes(&[]);
        let b = build_brief(&BriefInput {
            run: &run_id(),
            objective: "ship it",
            task: &t,
            dependencies: vec![],
            attempt: 1,
        });
        assert!(b.contains("# Task a: task a"));
        assert!(b.contains("ship it"));
        assert!(b.contains("do it"));
        assert!(b.contains(".fleet/result.json") && b.contains(".fleet/question.json"));
        assert!(!b.contains("attempt"));
    }

    #[test]
    fn brief_includes_dependencies_notes_and_attempt() {
        let t = task_with_notes(&["Attempt 1 failed: exit 2", "Question: q\nAnswer: a"]);
        let b = build_brief(&BriefInput {
            run: &run_id(),
            objective: "o",
            task: &t,
            dependencies: vec![
                (tid("base"), "the base".into(), Some("added models".into())),
                (tid("x"), "x".into(), None),
            ],
            attempt: 3,
        });
        assert!(b.contains("attempt 3"));
        assert!(b.contains("- base (the base): added models"));
        assert!(b.contains("- x (x)\n"));
        assert!(b.contains("Attempt 1 failed") && b.contains("Answer: a"));
    }

    #[test]
    fn brief_is_bounded() {
        let big = "x".repeat(10_000);
        let notes: Vec<&str> = std::iter::repeat_n(big.as_str(), 12).collect();
        let t = task_with_notes(&notes);
        let b = build_brief(&BriefInput {
            run: &run_id(),
            objective: "o",
            task: &t,
            dependencies: vec![],
            attempt: 1,
        });
        assert!(b.len() <= MAX_BRIEF_FILE_BYTES);
    }

    #[test]
    fn placeholders_expand_without_interpreting_values() {
        let vars = [
            ("task", "t1"),
            ("worktree", "/w t; rm -rf /"),
            ("brief", "$(evil) `x` {task}"),
        ];
        assert_eq!(
            expand_placeholders("--dir={worktree} --id {task}", &vars).unwrap(),
            "--dir=/w t; rm -rf / --id t1"
        );
        assert_eq!(
            expand_placeholders("{brief}", &vars).unwrap(),
            "$(evil) `x` {task}"
        );
    }

    #[test]
    fn braces_can_be_escaped() {
        assert_eq!(
            expand_placeholders("{{literal}} {task}", &[("task", "t")]).unwrap(),
            "{literal} t"
        );
        assert_eq!(expand_placeholders("a}}b", &[]).unwrap(), "a}b");
    }

    #[test]
    fn bad_templates_are_rejected() {
        for bad in [
            "{unknown}",
            "{",
            "}",
            "{task",
            "{ta sk}",
            "{}",
            "a { b",
            "{task}}x}",
        ] {
            assert!(expand_placeholders(bad, &[("task", "t")]).is_err(), "{bad}");
        }
    }

    #[test]
    fn template_validation_knows_the_placeholders() {
        assert!(
            validate_template("--task {task} --dir {worktree} {brief} {run} {brief_file}").is_ok()
        );
        assert!(validate_template("{nope}").is_err());
        assert!(validate_template("{{ok}}").is_ok());
        assert!(validate_template("no placeholders").is_ok());
    }
}
