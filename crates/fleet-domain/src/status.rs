//! Closed vocabularies of the domain (statuses, kinds). Each has a stable lowercase name used in
//! events, JSON output and the dashboard.

use crate::value::{Field, Value};

macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $text),+
                }
            }

            pub fn parse(s: &str) -> Option<$name> {
                match s {
                    $($text => Some($name::$variant),)+
                    _ => None,
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Field for $name {
            fn to_value(&self) -> Value {
                Value::Str(self.as_str().to_string())
            }
            fn from_value(v: &Value) -> Option<Self> {
                v.as_str().and_then($name::parse)
            }
        }
    };
}

string_enum!(
    /// Lifecycle of a task.
    TaskStatus {
        /// Waiting for dependencies.
        Pending => "pending",
        /// Dependencies are done; waiting for a free slot.
        Ready => "ready",
        /// A worker session is active.
        Running => "running",
        /// The worker finished; quality gates are being checked.
        Review => "review",
        Completed => "completed",
        Failed => "failed",
        /// Waiting for a human (question, approval), a conflict resolution or a failed dependency.
        Blocked => "blocked",
    }
);

string_enum!(
    /// Why a task is blocked.
    BlockKind {
        Question => "question",
        Approval => "approval",
        Conflict => "conflict",
        Dependency => "dependency",
    }
);

string_enum!(
    /// What a decision gate asks of the human.
    DecisionKind {
        Question => "question",
        Conflict => "conflict",
        /// A message channel was muted automatically (a loop, a storm); a person decides whether
        /// it may speak again. Blocks no task.
        Channel => "channel",
    }
);

string_enum!(
    /// What an approval is for.
    Purpose {
        Merge => "merge",
        ExternalCommand => "external-command",
        Manual => "manual",
        /// Delivery of one message on a channel that requires approval. Blocks no task.
        Message => "message",
    }
);

string_enum!(
    /// How a worker session ended.
    EndKind {
        Exited => "exited",
        Signaled => "signaled",
        Timeout => "timeout",
        IdleTimeout => "idle_timeout",
        Stopped => "stopped",
        Orphaned => "orphaned",
        KillFailed => "kill_failed",
        SpawnFailed => "spawn_failed",
    }
);

string_enum!(
    /// State of an approval.
    ApprovalStatus {
        Pending => "pending",
        Approved => "approved",
        Denied => "denied",
        Expired => "expired",
        Consumed => "consumed",
    }
);

string_enum!(
    /// State of a goal in the goal ledger. `verified` is reached only through recorded evidence.
    GoalStatus {
        Open => "open",
        InProgress => "in_progress",
        Blocked => "blocked",
        /// The goal's contract passed (and every child is verified); set by the ledger, never by a person.
        Verified => "verified",
        /// Dropped from the scope; counts as neither remaining nor verified.
        Abandoned => "abandoned",
    }
);

string_enum!(
    /// What one verification check came to.
    Verdict {
        Passed => "passed",
        /// The check ran and its result was wrong.
        Failed => "failed",
        /// The check could not be evaluated (timeout, cannot start, unreadable file).
        Error => "error",
    }
);

string_enum!(
    /// How the members of a message channel reach each other.
    ChannelMode {
        /// A message without `to` goes to every other member.
        Broadcast => "broadcast",
        /// Every message names exactly one recipient.
        Direct => "direct",
    }
);

string_enum!(
    /// Who may send on a message channel.
    ChannelDirection {
        Both => "both",
        /// Members send to the channel's lead only.
        ToLead => "to_lead",
    }
);

string_enum!(
    /// What a message says it is. A label only: no kind carries any authority.
    MessageKind {
        Info => "info",
        Question => "question",
        Result => "result",
        Request => "request",
    }
);

string_enum!(
    /// Why a message was refused. Recorded in the log; the body of a refused message never is.
    BlockReason {
        /// The file or its fields did not pass validation.
        Invalid => "invalid",
        UnknownChannel => "unknown-channel",
        NotMember => "not-a-member",
        BadRecipient => "bad-recipient",
        /// The channel's direction does not let this sender send.
        Direction => "direction",
        TooLarge => "too-large",
        /// The text matches the shape of a key, token or password.
        SecretShape => "secret-shape",
        Duplicate => "duplicate",
        TaskMuted => "task-muted",
        ChannelMuted => "channel-muted",
        /// Too many messages in the last minute.
        Rate => "rate-limit",
        /// The sender's or the channel's total was used up.
        Quota => "quota",
        FanOut => "fan-out",
        ReplyDepth => "reply-depth",
        PairBudget => "pair-budget",
        GlobalCap => "global-cap",
        UnknownReply => "unknown-reply",
        /// A person denied the approval the channel requires.
        Denied => "denied",
    }
);

string_enum!(
    /// Why a message was not delivered to one recipient.
    ExpireReason {
        Ttl => "ttl",
        RecipientEnded => "recipient-ended",
        ApprovalExpired => "approval-expired",
        InboxFull => "inbox-full",
    }
);

string_enum!(
    /// Why a channel or a task was muted.
    MuteReason {
        /// `fleet msg mute`.
        Operator => "operator",
        ReplyDepth => "reply-depth",
        PairBudget => "pair-budget",
        /// A task kept sending messages that were refused.
        Flood => "flood",
        /// A task used up the total number of refusals it may cause (until a person looks).
        RefusalBudget => "refusal-budget",
        /// Fewer than two members can still take part, or the lead is gone.
        EndpointEnded => "endpoint-ended",
    }
);

string_enum!(
    /// Where a message stands, as shown to people.
    MessageStatus {
        AwaitingApproval => "awaiting-approval",
        /// Accepted; waiting for a recipient that is running.
        Queued => "queued",
        Delivered => "delivered",
        Expired => "expired",
        Blocked => "blocked",
    }
);

impl TaskStatus {
    /// Statuses a task never leaves without an operator or a dependency change.
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskStatus::Completed | TaskStatus::Failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for s in TaskStatus::ALL {
            assert_eq!(TaskStatus::parse(s.as_str()), Some(*s));
            assert_eq!(TaskStatus::from_value(&s.to_value()), Some(*s));
        }
        for s in EndKind::ALL {
            assert_eq!(EndKind::parse(s.as_str()), Some(*s));
        }
        for s in ApprovalStatus::ALL {
            assert_eq!(s.to_string(), s.as_str());
        }
    }

    #[test]
    fn goal_statuses_and_verdicts_round_trip() {
        for s in GoalStatus::ALL {
            assert_eq!(GoalStatus::parse(s.as_str()), Some(*s));
            assert_eq!(GoalStatus::from_value(&s.to_value()), Some(*s));
        }
        for v in Verdict::ALL {
            assert_eq!(Verdict::parse(v.as_str()), Some(*v));
        }
        assert_eq!(GoalStatus::parse("done"), None);
        assert_eq!(Verdict::parse("ok"), None);
    }

    #[test]
    fn unknown_names_are_rejected() {
        assert_eq!(TaskStatus::parse("done"), None);
        assert_eq!(Purpose::parse("Merge"), None);
        assert_eq!(BlockKind::from_value(&Value::int(1)), None);
    }

    #[test]
    fn only_completed_and_failed_are_terminal() {
        let terminal: Vec<_> = TaskStatus::ALL.iter().filter(|s| s.is_terminal()).collect();
        assert_eq!(terminal, vec![&TaskStatus::Completed, &TaskStatus::Failed]);
    }

    #[test]
    fn messaging_vocabularies_round_trip() {
        for s in ChannelMode::ALL {
            assert_eq!(ChannelMode::parse(s.as_str()), Some(*s));
        }
        for s in ChannelDirection::ALL {
            assert_eq!(ChannelDirection::from_value(&s.to_value()), Some(*s));
        }
        for s in MessageKind::ALL {
            assert_eq!(MessageKind::parse(s.as_str()), Some(*s));
        }
        for s in BlockReason::ALL {
            assert_eq!(BlockReason::parse(s.as_str()), Some(*s));
        }
        for s in ExpireReason::ALL {
            assert_eq!(ExpireReason::parse(s.as_str()), Some(*s));
        }
        for s in MuteReason::ALL {
            assert_eq!(MuteReason::parse(s.as_str()), Some(*s));
        }
        for s in MessageStatus::ALL {
            assert_eq!(s.to_string(), s.as_str());
        }
        assert_eq!(DecisionKind::parse("channel"), Some(DecisionKind::Channel));
        assert_eq!(Purpose::parse("message"), Some(Purpose::Message));
        // no kind looks like an approval or a gate answer: the vocabulary is closed
        for bad in [
            "approval",
            "approve",
            "gate",
            "gate_answer",
            "command",
            "exec",
        ] {
            assert_eq!(MessageKind::parse(bad), None, "{bad}");
        }
    }
}
