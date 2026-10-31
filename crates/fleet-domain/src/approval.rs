//! Approvals: a human decision bound to the digest of one exact action.
//!
//! An approval is requested by the orchestrator (for a merge) or on behalf of a worker (a
//! `question.json` of kind `approval`); never by the worker directly. It records the action
//! (tool, command, arguments) and the task id, and a SHA-256 digest of exactly those. The human
//! approves or denies the digest; using the approval re-computes the digest of what is about to
//! run, so an approval for one action can never authorize another. Approvals expire, and each
//! can be used once.

use crate::error::{ErrorCode, FleetError, Result};
use crate::events::DomainEvent;
use crate::ids::{ApprovalId, RunId, Sha, TaskId};
use crate::limits::{MAX_ACTION_ARGS, MAX_ACTION_ARG_LEN};
use crate::status::{ApprovalStatus, Purpose};
use crate::value::Value;

/// Hashing, supplied by an adapter so the domain stays free of crypto dependencies.
pub trait Digest {
    /// Lowercase hex SHA-256 of `data`.
    fn sha256_hex(&self, data: &[u8]) -> String;
}

/// What is being approved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub tool: String,
    pub command: String,
    pub args: Vec<String>,
}

impl Action {
    /// The merge of a task branch, at an exact commit, into the run's integration branch.
    pub fn merge(integration_branch: &str, task_branch: &str, head: &Sha) -> Action {
        Action {
            tool: "git".to_string(),
            command: "merge".to_string(),
            args: vec![
                integration_branch.to_string(),
                task_branch.to_string(),
                head.to_string(),
            ],
        }
    }

    /// A worker-requested command: always labelled `command`, whatever the worker called it, and
    /// always an absolute program path (checked when the request is parsed).
    pub fn command(command: &str, args: &[String]) -> Action {
        Action {
            tool: "command".to_string(),
            command: command.to_string(),
            args: args.to_vec(),
        }
    }

    /// A request that has no executable form: the worker only wants a yes or no.
    pub fn manual(question: &str) -> Action {
        Action {
            tool: "manual".to_string(),
            command: String::new(),
            args: vec![question.to_string()],
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.args.len() > MAX_ACTION_ARGS
            || self.args.iter().any(|a| a.len() > MAX_ACTION_ARG_LEN)
        {
            return Err(FleetError::limit(
                "action has too many or too long arguments",
            ));
        }
        Ok(())
    }

    /// Digest over the action and the task it runs for. The encoding is canonical JSON with sorted
    /// keys, so no two different actions share an encoding.
    pub fn digest(&self, task: &TaskId, hasher: &dyn Digest) -> String {
        let doc = Value::object([
            ("tool", Value::str(self.tool.clone())),
            ("command", Value::str(self.command.clone())),
            (
                "args",
                Value::array(self.args.iter().cloned().map(Value::Str)),
            ),
            ("task", Value::str(task.as_str())),
        ]);
        hasher.sha256_hex(doc.canonical().as_bytes())
    }

    /// One line for the human, with every argument quoted so boundaries are visible.
    pub fn display(&self) -> String {
        let mut parts = vec![self.command.clone()];
        parts.extend(self.args.iter().cloned());
        let quoted: Vec<String> = parts.iter().map(|p| format!("{p:?}")).collect();
        format!("{}: {}", self.tool, quoted.join(" "))
    }
}

/// How far an approved external command got. Consuming the approval and starting the command are
/// one step, so `Started` means the approval is gone and the command is never started again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandProgress {
    NotStarted,
    Started,
    Finished,
}

/// When an approval is requested and how long it stays usable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub now_ms: u64,
    pub ttl_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approval {
    pub id: ApprovalId,
    pub run: RunId,
    pub task: TaskId,
    pub purpose: Purpose,
    pub action: Action,
    pub digest: String,
    pub requested_ms: u64,
    pub expires_ms: u64,
    pub status: ApprovalStatus,
    pub command: CommandProgress,
}

impl Approval {
    /// Builds the request event. The digest is computed here, never taken from the caller.
    pub fn request(
        id: ApprovalId,
        run: RunId,
        task: TaskId,
        purpose: Purpose,
        action: &Action,
        window: Window,
        hasher: &dyn Digest,
    ) -> Result<DomainEvent> {
        action.validate()?;
        Ok(DomainEvent::ApprovalRequested {
            id,
            run,
            task: task.clone(),
            purpose,
            tool: action.tool.clone(),
            command: action.command.clone(),
            args: action.args.clone(),
            digest: action.digest(&task, hasher),
            requested_ms: window.now_ms,
            expires_ms: window.now_ms.saturating_add(window.ttl_ms),
        })
    }

    pub fn from_event(ev: &DomainEvent) -> Option<Approval> {
        match ev {
            DomainEvent::ApprovalRequested {
                id,
                run,
                task,
                purpose,
                tool,
                command,
                args,
                digest,
                requested_ms,
                expires_ms,
            } => Some(Approval {
                id: id.clone(),
                run: run.clone(),
                task: task.clone(),
                purpose: *purpose,
                action: Action {
                    tool: tool.clone(),
                    command: command.clone(),
                    args: args.clone(),
                },
                digest: digest.clone(),
                requested_ms: *requested_ms,
                expires_ms: *expires_ms,
                status: ApprovalStatus::Pending,
                command: CommandProgress::NotStarted,
            }),
            _ => None,
        }
    }

    /// Pending and approved approvals lapse when their time is up.
    pub fn status_at(&self, now_ms: u64) -> ApprovalStatus {
        match self.status {
            ApprovalStatus::Pending | ApprovalStatus::Approved if now_ms >= self.expires_ms => {
                ApprovalStatus::Expired
            }
            s => s,
        }
    }

    /// The event that records a lapse, when one is due.
    pub fn expiry_event(&self, now_ms: u64) -> Option<DomainEvent> {
        (self.status_at(now_ms) == ApprovalStatus::Expired
            && self.status != ApprovalStatus::Expired)
            .then(|| DomainEvent::ApprovalExpired {
                id: self.id.clone(),
            })
    }

    fn stored_digest_is_consistent(&self, hasher: &dyn Digest) -> bool {
        self.action.digest(&self.task, hasher) == self.digest
    }

    /// A human decision. `seen_digest` is what the human saw and decides on: it is required and
    /// must match (at least 12 hex digits of the digest), so a decision always names the exact
    /// action it covers.
    pub fn decide(
        &self,
        approve: bool,
        seen_digest: &str,
        now_ms: u64,
        hasher: &dyn Digest,
    ) -> Result<DomainEvent> {
        match self.status_at(now_ms) {
            ApprovalStatus::Pending => {}
            other => {
                return Err(FleetError::conflict(format!(
                    "approval {} is {other}, not pending",
                    self.id
                )))
            }
        }
        if seen_digest.len() < 12 || !self.digest.starts_with(seen_digest) {
            return Err(FleetError::invalid(format!(
                "digest does not match approval {}: a decision names the digest of the action it covers (at least 12 hex digits)",
                self.id
            )));
        }
        if !self.stored_digest_is_consistent(hasher) {
            return Err(FleetError::new(
                ErrorCode::Tampered,
                format!("approval {} does not match its own digest", self.id),
            ));
        }
        Ok(DomainEvent::ApprovalDecided {
            id: self.id.clone(),
            approved: approve,
        })
    }

    /// Uses an approved approval for `action`. Fails unless the approval is approved, unexpired,
    /// unused, and the digest of `action` is the approved one.
    pub fn consume(
        &self,
        action: &Action,
        now_ms: u64,
        hasher: &dyn Digest,
    ) -> Result<DomainEvent> {
        match self.status_at(now_ms) {
            ApprovalStatus::Approved => {}
            ApprovalStatus::Pending => {
                return Err(FleetError::new(
                    ErrorCode::ApprovalRequired,
                    format!("approval {} is still pending", self.id),
                )
                .with_subject(self.id.as_str()))
            }
            other => {
                return Err(FleetError::conflict(format!(
                    "approval {} is {other} and cannot be used",
                    self.id
                )))
            }
        }
        if action.digest(&self.task, hasher) != self.digest
            || !self.stored_digest_is_consistent(hasher)
        {
            return Err(FleetError::conflict(format!(
                "this action differs from the one approval {} was given for",
                self.id
            )));
        }
        Ok(DomainEvent::ApprovalConsumed {
            id: self.id.clone(),
        })
    }

    /// Starts an approved external command: consumes the approval (it must be approved, unexpired
    /// and match `action`) and records that the command was started. Both events are recorded
    /// before the command is launched, so no restart or delay can make it run twice.
    pub fn start_command(
        &self,
        action: &Action,
        now_ms: u64,
        hasher: &dyn Digest,
    ) -> Result<Vec<DomainEvent>> {
        let consumed = self.consume(action, now_ms, hasher)?;
        Ok(vec![
            consumed,
            DomainEvent::ExternalCommandStarted {
                id: self.id.clone(),
            },
        ])
    }

    pub fn apply(&mut self, ev: &DomainEvent) {
        match ev {
            DomainEvent::ApprovalDecided { id, approved } if *id == self.id => {
                self.status = if *approved {
                    ApprovalStatus::Approved
                } else {
                    ApprovalStatus::Denied
                };
            }
            DomainEvent::ApprovalExpired { id } if *id == self.id => {
                self.status = ApprovalStatus::Expired
            }
            DomainEvent::ApprovalConsumed { id } if *id == self.id => {
                self.status = ApprovalStatus::Consumed
            }
            DomainEvent::ExternalCommandStarted { id }
                if *id == self.id && self.command == CommandProgress::NotStarted =>
            {
                self.command = CommandProgress::Started;
            }
            DomainEvent::ExternalCommandRan { id, .. } if *id == self.id => {
                self.command = CommandProgress::Finished;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::Digest;

    /// A deterministic stand-in digest for domain tests (not cryptographic).
    pub struct FakeDigest;

    impl Digest for FakeDigest {
        fn sha256_hex(&self, data: &[u8]) -> String {
            let mut acc: [u64; 4] = [
                0xcbf29ce484222325,
                0x100000001b3,
                0x9e3779b97f4a7c15,
                0xff51afd7ed558ccd,
            ];
            for (i, b) in data.iter().enumerate() {
                let slot = i % 4;
                acc[slot] = (acc[slot] ^ u64::from(*b))
                    .wrapping_mul(0x100000001b3)
                    .rotate_left(5);
                acc[(slot + 1) % 4] ^= acc[slot];
            }
            acc.iter().map(|a| format!("{a:016x}")).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::FakeDigest;
    use super::*;

    fn sha() -> Sha {
        Sha::parse(&"a".repeat(40)).unwrap()
    }

    fn approval(ttl: u64) -> Approval {
        let action = Action::merge("fleet/r1/integration", "fleet/r1/t1", &sha());
        let ev = Approval::request(
            ApprovalId::from_seq(1),
            RunId::parse("r1").unwrap(),
            TaskId::parse("t1").unwrap(),
            Purpose::Merge,
            &action,
            Window {
                now_ms: 1000,
                ttl_ms: ttl,
            },
            &FakeDigest,
        )
        .unwrap();
        Approval::from_event(&ev).unwrap()
    }

    fn approved(ttl: u64) -> Approval {
        let mut a = approval(ttl);
        let ev = a
            .decide(true, &a.digest.clone(), 1001, &FakeDigest)
            .unwrap();
        a.apply(&ev);
        a
    }

    #[test]
    fn digest_depends_on_every_part_of_the_action() {
        let t1 = TaskId::parse("t1").unwrap();
        let t2 = TaskId::parse("t2").unwrap();
        let base = Action {
            tool: "shell".into(),
            command: "npm".into(),
            args: vec!["publish".into()],
        };
        let d = base.digest(&t1, &FakeDigest);
        assert_eq!(d, base.digest(&t1, &FakeDigest));
        assert_ne!(d, base.digest(&t2, &FakeDigest));
        for changed in [
            Action {
                tool: "sh".into(),
                ..base.clone()
            },
            Action {
                command: "npx".into(),
                ..base.clone()
            },
            Action {
                args: vec!["publish".into(), "--tag".into()],
                ..base.clone()
            },
            Action {
                args: vec![],
                ..base.clone()
            },
        ] {
            assert_ne!(d, changed.digest(&t1, &FakeDigest), "{changed:?}");
        }
    }

    #[test]
    fn argument_boundaries_are_part_of_the_digest() {
        let t = TaskId::parse("t1").unwrap();
        let a = Action {
            tool: "shell".into(),
            command: "echo".into(),
            args: vec!["a b".into()],
        };
        let b = Action {
            tool: "shell".into(),
            command: "echo".into(),
            args: vec!["a".into(), "b".into()],
        };
        assert_ne!(a.digest(&t, &FakeDigest), b.digest(&t, &FakeDigest));
    }

    #[test]
    fn request_computes_the_digest_and_expiry() {
        let a = approval(5000);
        assert_eq!(a.status, ApprovalStatus::Pending);
        assert_eq!(a.expires_ms, 6000);
        assert_eq!(a.digest, a.action.digest(&a.task, &FakeDigest));
    }

    #[test]
    fn deciding_flips_the_status() {
        let mut a = approval(5000);
        let ev = a
            .decide(true, &a.digest.clone(), 1500, &FakeDigest)
            .unwrap();
        assert!(matches!(
            ev,
            DomainEvent::ApprovalDecided { approved: true, .. }
        ));
        a.apply(&ev);
        assert_eq!(a.status, ApprovalStatus::Approved);

        let mut d = approval(5000);
        let ev = d
            .decide(false, &d.digest.clone(), 1500, &FakeDigest)
            .unwrap();
        d.apply(&ev);
        assert_eq!(d.status, ApprovalStatus::Denied);
    }

    #[test]
    fn cannot_decide_twice_or_after_expiry() {
        let a = approved(5000);
        assert_eq!(
            a.decide(true, &a.digest.clone(), 1500, &FakeDigest)
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        let late = approval(5000);
        let err = late
            .decide(true, &late.digest.clone(), 6000, &FakeDigest)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
        assert!(err.message.contains("expired"));
    }

    #[test]
    fn digest_check_requires_a_matching_prefix() {
        let a = approval(5000);
        let full = a.digest.clone();
        assert!(a.decide(true, &full, 1500, &FakeDigest).is_ok());
        assert!(a.decide(true, &full[..12], 1500, &FakeDigest).is_ok());
        assert!(a.decide(true, &full[..8], 1500, &FakeDigest).is_err());
        assert!(a.decide(true, &"0".repeat(64), 1500, &FakeDigest).is_err());
    }

    #[test]
    fn tampered_record_cannot_be_decided_or_used() {
        let mut a = approval(5000);
        a.action.args.push("--force".into());
        assert_eq!(
            a.decide(true, &a.digest.clone(), 1500, &FakeDigest)
                .unwrap_err()
                .code,
            ErrorCode::Tampered
        );
        let mut b = approved(5000);
        b.action.command = "rm".into();
        let act = b.action.clone();
        assert!(b.consume(&act, 1500, &FakeDigest).is_err());
    }

    #[test]
    fn consume_requires_the_exact_approved_action() {
        let a = approved(5000);
        let same = a.action.clone();
        let ev = a.consume(&same, 1500, &FakeDigest).unwrap();
        assert!(matches!(ev, DomainEvent::ApprovalConsumed { .. }));

        let other_head = Action::merge(
            "fleet/r1/integration",
            "fleet/r1/t1",
            &Sha::parse(&"b".repeat(40)).unwrap(),
        );
        let err = a.consume(&other_head, 1500, &FakeDigest).unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
    }

    #[test]
    fn a_decision_must_name_the_digest_of_the_action_it_covers() {
        let a = approval(5000);
        for seen in ["", "abc", &a.digest[..11], "0".repeat(64).as_str()] {
            let err = a.decide(true, seen, 1500, &FakeDigest).unwrap_err();
            assert_eq!(err.code, ErrorCode::Invalid, "{seen:?}");
            assert!(err.message.contains("digest"), "{}", err.message);
        }
        assert!(a.decide(true, &a.digest[..12], 1500, &FakeDigest).is_ok());
    }

    #[test]
    fn consume_is_single_use() {
        let mut a = approved(5000);
        let act = a.action.clone();
        let ev = a.consume(&act, 1500, &FakeDigest).unwrap();
        a.apply(&ev);
        assert_eq!(a.status, ApprovalStatus::Consumed);
        assert!(a.consume(&act, 1500, &FakeDigest).is_err());
    }

    #[test]
    fn pending_denied_and_expired_cannot_be_consumed() {
        let pending = approval(5000);
        let act = pending.action.clone();
        let err = pending.consume(&act, 1500, &FakeDigest).unwrap_err();
        assert_eq!(err.code, ErrorCode::ApprovalRequired);
        assert_eq!(err.subject.as_deref(), Some("a1"));

        let mut denied = approval(5000);
        let ev = denied
            .decide(false, &denied.digest.clone(), 1500, &FakeDigest)
            .unwrap();
        denied.apply(&ev);
        assert!(denied.consume(&act, 1500, &FakeDigest).is_err());

        let ok = approved(5000);
        assert!(ok.consume(&act, 6001, &FakeDigest).is_err());
    }

    #[test]
    fn expiry_event_is_emitted_exactly_once() {
        let mut a = approval(5000);
        assert!(a.expiry_event(5999).is_none());
        let ev = a.expiry_event(6000).unwrap();
        assert!(matches!(ev, DomainEvent::ApprovalExpired { .. }));
        a.apply(&ev);
        assert!(a.expiry_event(9000).is_none());
        assert_eq!(a.status, ApprovalStatus::Expired);
        let mut used = approved(5000);
        let act = used.action.clone();
        let ev = used.consume(&act, 2000, &FakeDigest).unwrap();
        used.apply(&ev);
        assert!(used.expiry_event(99_999).is_none());
    }

    fn approved_command(ttl: u64) -> Approval {
        let action = Action::command("/usr/bin/make", &["deploy".to_string()]);
        let ev = Approval::request(
            ApprovalId::from_seq(2),
            RunId::parse("r1").unwrap(),
            TaskId::parse("t1").unwrap(),
            Purpose::ExternalCommand,
            &action,
            Window {
                now_ms: 1000,
                ttl_ms: ttl,
            },
            &FakeDigest,
        )
        .unwrap();
        let mut a = Approval::from_event(&ev).unwrap();
        let decided = a
            .decide(true, &a.digest.clone(), 1001, &FakeDigest)
            .unwrap();
        a.apply(&decided);
        a
    }

    #[test]
    fn worker_commands_always_carry_the_fixed_tool_label() {
        let a = Action::command("/bin/ls", &["-l".to_string()]);
        assert_eq!(a.tool, "command");
        assert_eq!(a.display(), "command: \"/bin/ls\" \"-l\"");
    }

    #[test]
    fn starting_a_command_consumes_the_approval_and_records_the_start() {
        let mut a = approved_command(5000);
        assert_eq!(a.command, CommandProgress::NotStarted);
        let act = a.action.clone();
        let events = a.start_command(&act, 1500, &FakeDigest).unwrap();
        let kinds: Vec<&str> = events.iter().map(DomainEvent::kind).collect();
        assert_eq!(kinds, vec!["approval_consumed", "external_command_started"]);
        for e in &events {
            a.apply(e);
        }
        assert_eq!(a.status, ApprovalStatus::Consumed);
        assert_eq!(a.command, CommandProgress::Started);
        a.apply(&DomainEvent::ExternalCommandRan {
            id: a.id.clone(),
            exit_code: Some(0),
            summary: String::new(),
            output_tail: String::new(),
        });
        assert_eq!(a.command, CommandProgress::Finished);
    }

    #[test]
    fn a_started_command_cannot_be_started_again_even_if_it_runs_past_expiry() {
        let mut a = approved_command(5000);
        let act = a.action.clone();
        for e in a.start_command(&act, 1500, &FakeDigest).unwrap() {
            a.apply(&e);
        }
        for now in [1500, 5999, 6000, 1_000_000] {
            assert!(a.start_command(&act, now, &FakeDigest).is_err(), "{now}");
        }
        // The run outliving the approval's window does not make the approval lapse.
        assert!(a.expiry_event(1_000_000).is_none());
    }

    #[test]
    fn commands_are_not_started_without_a_live_matching_approval() {
        let pending = {
            let action = Action::command("/usr/bin/make", &[]);
            let ev = Approval::request(
                ApprovalId::from_seq(3),
                RunId::parse("r1").unwrap(),
                TaskId::parse("t1").unwrap(),
                Purpose::ExternalCommand,
                &action,
                Window {
                    now_ms: 1000,
                    ttl_ms: 5000,
                },
                &FakeDigest,
            )
            .unwrap();
            Approval::from_event(&ev).unwrap()
        };
        let act = pending.action.clone();
        assert!(pending.start_command(&act, 1500, &FakeDigest).is_err());

        let approved = approved_command(5000);
        let act = approved.action.clone();
        assert!(approved.start_command(&act, 6000, &FakeDigest).is_err());
        let other = Action::command("/usr/bin/make", &["clean".to_string()]);
        assert_eq!(
            approved
                .start_command(&other, 1500, &FakeDigest)
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
    }

    #[test]
    fn oversized_actions_are_rejected() {
        let big = Action {
            tool: "shell".into(),
            command: "x".into(),
            args: vec!["a".into(); 65],
        };
        assert!(big.validate().is_err());
        let long = Action {
            tool: "shell".into(),
            command: "x".into(),
            args: vec!["a".repeat(5000)],
        };
        assert!(long.validate().is_err());
    }

    #[test]
    fn display_quotes_each_argument() {
        let a = Action {
            tool: "shell".into(),
            command: "echo".into(),
            args: vec!["a b".into(), "c".into()],
        };
        assert_eq!(a.display(), "shell: \"echo\" \"a b\" \"c\"");
    }

    #[test]
    fn from_event_ignores_other_events() {
        assert!(Approval::from_event(&DomainEvent::Warning {
            message: String::new()
        })
        .is_none());
    }
}
