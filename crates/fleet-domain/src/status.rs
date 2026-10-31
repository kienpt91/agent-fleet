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
    }
);

string_enum!(
    /// What an approval is for.
    Purpose {
        Merge => "merge",
        ExternalCommand => "external-command",
        Manual => "manual",
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
}
