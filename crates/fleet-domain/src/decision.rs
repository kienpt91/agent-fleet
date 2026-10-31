//! Decision gates: a question that blocks a task until a human answers it.
//!
//! Workers raise them (a blocking question in `question.json`) and so does the merge step when a
//! branch conflicts. Only a human resolves one, with a text answer that is fed back into the
//! task's next brief.

use crate::error::{FleetError, Result};
use crate::events::DomainEvent;
use crate::ids::{validate_text, DecisionId, RunId, TaskId};
use crate::limits::{MAX_ANSWER, MAX_CHOICES};
use crate::status::DecisionKind;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub id: DecisionId,
    pub run: RunId,
    pub task: TaskId,
    pub kind: DecisionKind,
    pub text: String,
    pub choices: Vec<String>,
    pub answer: Option<String>,
}

impl Decision {
    pub fn open(
        id: DecisionId,
        run: RunId,
        task: TaskId,
        kind: DecisionKind,
        text: &str,
        choices: Vec<String>,
    ) -> Result<DomainEvent> {
        if text.trim().is_empty() {
            return Err(FleetError::invalid("a decision gate needs a question"));
        }
        if choices.len() > MAX_CHOICES {
            return Err(FleetError::limit(format!(
                "more than {MAX_CHOICES} choices"
            )));
        }
        Ok(DomainEvent::DecisionOpened {
            id,
            run,
            task,
            kind,
            text: text.to_string(),
            choices,
        })
    }

    pub fn from_event(ev: &DomainEvent) -> Option<Decision> {
        match ev {
            DomainEvent::DecisionOpened {
                id,
                run,
                task,
                kind,
                text,
                choices,
            } => Some(Decision {
                id: id.clone(),
                run: run.clone(),
                task: task.clone(),
                kind: *kind,
                text: text.clone(),
                choices: choices.clone(),
                answer: None,
            }),
            _ => None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.answer.is_none()
    }

    /// Validates a human's answer. When the gate offers choices the answer must be one of them.
    pub fn resolve(&self, answer: &str) -> Result<DomainEvent> {
        if !self.is_open() {
            return Err(FleetError::conflict(format!(
                "gate {} is already resolved",
                self.id
            )));
        }
        let answer = answer.trim();
        if answer.is_empty() {
            return Err(FleetError::invalid("the answer is empty"));
        }
        validate_text("answer", answer, MAX_ANSWER)?;
        if !self.choices.is_empty() && !self.choices.iter().any(|c| c == answer) {
            return Err(FleetError::invalid(format!(
                "answer must be one of: {}",
                self.choices.join(", ")
            )));
        }
        Ok(DomainEvent::DecisionResolved {
            id: self.id.clone(),
            answer: answer.to_string(),
        })
    }

    pub fn apply(&mut self, ev: &DomainEvent) {
        if let DomainEvent::DecisionResolved { id, answer } = ev {
            if *id == self.id {
                self.answer = Some(answer.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    fn gate(choices: &[&str]) -> Decision {
        let ev = Decision::open(
            DecisionId::from_seq(1),
            RunId::parse("r1").unwrap(),
            TaskId::parse("t1").unwrap(),
            DecisionKind::Question,
            "Which database?",
            choices.iter().map(|c| c.to_string()).collect(),
        )
        .unwrap();
        Decision::from_event(&ev).unwrap()
    }

    #[test]
    fn opening_requires_a_question() {
        let r = Decision::open(
            DecisionId::from_seq(1),
            RunId::parse("r1").unwrap(),
            TaskId::parse("t1").unwrap(),
            DecisionKind::Question,
            "  ",
            vec![],
        );
        assert_eq!(r.unwrap_err().code, ErrorCode::Invalid);
    }

    #[test]
    fn opening_caps_choices() {
        let r = Decision::open(
            DecisionId::from_seq(1),
            RunId::parse("r1").unwrap(),
            TaskId::parse("t1").unwrap(),
            DecisionKind::Question,
            "q",
            vec!["c".to_string(); MAX_CHOICES + 1],
        );
        assert_eq!(r.unwrap_err().code, ErrorCode::Limit);
    }

    #[test]
    fn resolving_records_the_answer_once() {
        let mut g = gate(&[]);
        assert!(g.is_open());
        let ev = g.resolve("  use sqlite  ").unwrap();
        assert!(
            matches!(&ev, DomainEvent::DecisionResolved { answer, .. } if answer == "use sqlite")
        );
        g.apply(&ev);
        assert!(!g.is_open());
        assert_eq!(g.answer.as_deref(), Some("use sqlite"));
        assert_eq!(g.resolve("again").unwrap_err().code, ErrorCode::Conflict);
    }

    #[test]
    fn answers_are_validated() {
        let g = gate(&[]);
        assert!(g.resolve("").is_err());
        assert!(g.resolve("   ").is_err());
        assert!(g.resolve("a\u{1b}[2J").is_err());
        assert!(g.resolve(&"x".repeat(MAX_ANSWER + 1)).is_err());
        assert!(g.resolve("multi\nline\nanswer").is_ok());
    }

    #[test]
    fn choices_restrict_the_answer() {
        let g = gate(&["sqlite", "postgres"]);
        assert!(g.resolve("sqlite").is_ok());
        let err = g.resolve("mysql").unwrap_err();
        assert!(err.message.contains("sqlite, postgres"));
    }

    #[test]
    fn apply_ignores_other_gates() {
        let mut g = gate(&[]);
        g.apply(&DomainEvent::DecisionResolved {
            id: DecisionId::from_seq(2),
            answer: "x".into(),
        });
        assert!(g.is_open());
        assert!(Decision::from_event(&DomainEvent::Warning {
            message: String::new()
        })
        .is_none());
    }
}
