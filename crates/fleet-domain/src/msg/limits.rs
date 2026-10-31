//! Hard caps and defaults of the message channels. Everything a worker can send is bounded by one
//! of these, so a worker cannot grow memory, the log or another worker's inbox without limit.

use crate::error::{FleetError, Result};

/// Default size of one message body (bytes), and the ceiling no channel may exceed.
pub const DEFAULT_MESSAGE_BYTES: usize = 2 * 1024;
pub const HARD_MESSAGE_BYTES: usize = 8 * 1024;
/// A channel limit below this is a typo, not a policy.
pub const MIN_MESSAGE_BYTES: usize = 64;
/// Longest subject (characters).
pub const MAX_SUBJECT: usize = 120;

pub const DEFAULT_PER_TASK: u32 = 20;
pub const HARD_PER_TASK: u32 = 200;
pub const DEFAULT_PER_MINUTE: u32 = 10;
pub const HARD_PER_MINUTE: u32 = 120;
pub const DEFAULT_TTL_SECS: u64 = 900;
pub const MIN_TTL_SECS: u64 = 10;
pub const MAX_TTL_SECS: u64 = 86_400;

/// Members listed in one channel.
pub const MAX_MEMBERS: usize = 32;
pub const MAX_CHANNELS_PER_RUN: usize = 16;
pub const MAX_CHANNELS_TOTAL: usize = 64;

/// The largest `.fleet/outbox/<n>.json` the daemon reads (the body alone is capped lower; this
/// leaves room for the JSON around it and for escapes).
pub const MAX_OUTBOX_FILE: u64 = 32 * 1024;
/// Outbox files looked at per task per tick, and names listed when the directory is read.
pub const MAX_OUTBOX_PER_SCAN: usize = 8;
pub const MAX_OUTBOX_LISTED: usize = 64;
/// Messages kept in one worker's inbox.
pub const MAX_INBOX_ENTRIES: usize = 256;
/// Refused messages a sender may produce per minute before it is muted as a flood.
pub const MAX_BLOCKED_PER_MINUTE: usize = 20;
/// Messages (and refusals) kept in memory; refusals are dropped first.
pub const MESSAGES_KEPT: usize = 4096;
/// Digests remembered for duplicate suppression.
pub const MAX_DIGESTS_KEPT: usize = 4096;
/// Longest channel name list or member list entry text.
pub const MAX_MEMBER_TEXT: usize = 48;
/// Deliveries and expiries decided per tick.
pub const MAX_DUE_PER_TICK: usize = 32;

/// Daemon-wide limits and switches (`[messages]` in `fleet.toml`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessagingLimits {
    /// Accepted messages the daemon keeps in total (the log is append-only and bounded too).
    pub max_total_messages: usize,
    /// Recipients of one message.
    pub max_fan_out: usize,
    /// Longest reply chain (`in_reply_to`, or a message that answers the one before it).
    pub max_reply_depth: u32,
    /// Messages two tasks may exchange on one channel before it is muted.
    pub max_pair_exchanges: u32,
    /// Messages one task may send per minute over all its channels.
    pub max_per_task_per_minute: u32,
    /// Type a one-line notice into a worker's terminal when a message arrives. Off by default.
    pub notify_input: bool,
}

pub const HARD_TOTAL_MESSAGES: usize = 2000;
pub const HARD_FAN_OUT: usize = 32;
pub const HARD_REPLY_DEPTH: u32 = 32;
pub const HARD_PAIR_EXCHANGES: u32 = 200;

impl Default for MessagingLimits {
    fn default() -> Self {
        MessagingLimits {
            max_total_messages: 500,
            max_fan_out: 8,
            max_reply_depth: 6,
            max_pair_exchanges: 12,
            max_per_task_per_minute: 30,
            notify_input: false,
        }
    }
}

impl MessagingLimits {
    pub fn validate(&self) -> Result<()> {
        let bad = |what: &str, hard: u64| {
            Err(FleetError::invalid(format!(
                "[messages] {what} must be between 1 and {hard}"
            )))
        };
        if self.max_total_messages == 0 || self.max_total_messages > HARD_TOTAL_MESSAGES {
            return bad("max_total_messages", HARD_TOTAL_MESSAGES as u64);
        }
        if self.max_fan_out == 0 || self.max_fan_out > HARD_FAN_OUT {
            return bad("max_fan_out", HARD_FAN_OUT as u64);
        }
        if self.max_reply_depth == 0 || self.max_reply_depth > HARD_REPLY_DEPTH {
            return bad("max_reply_depth", u64::from(HARD_REPLY_DEPTH));
        }
        if self.max_pair_exchanges == 0 || self.max_pair_exchanges > HARD_PAIR_EXCHANGES {
            return bad("max_pair_exchanges", u64::from(HARD_PAIR_EXCHANGES));
        }
        if self.max_per_task_per_minute == 0 || self.max_per_task_per_minute > HARD_PER_MINUTE {
            return bad(
                "max_messages_per_task_per_minute",
                u64::from(HARD_PER_MINUTE),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_valid_and_inside_the_hard_caps() {
        let d = MessagingLimits::default();
        assert!(d.validate().is_ok());
        assert!(!d.notify_input, "typing into terminals is off by default");
        assert_eq!(DEFAULT_MESSAGE_BYTES, 2048);
        assert_eq!(HARD_MESSAGE_BYTES, 8192);
        assert_eq!(d.max_reply_depth, 6);
    }

    #[test]
    fn every_limit_is_checked_at_both_ends() {
        let d = MessagingLimits::default();
        let cases: Vec<MessagingLimits> = vec![
            MessagingLimits {
                max_total_messages: 0,
                ..d
            },
            MessagingLimits {
                max_total_messages: HARD_TOTAL_MESSAGES + 1,
                ..d
            },
            MessagingLimits {
                max_fan_out: 0,
                ..d
            },
            MessagingLimits {
                max_fan_out: HARD_FAN_OUT + 1,
                ..d
            },
            MessagingLimits {
                max_reply_depth: 0,
                ..d
            },
            MessagingLimits {
                max_reply_depth: HARD_REPLY_DEPTH + 1,
                ..d
            },
            MessagingLimits {
                max_pair_exchanges: 0,
                ..d
            },
            MessagingLimits {
                max_pair_exchanges: HARD_PAIR_EXCHANGES + 1,
                ..d
            },
            MessagingLimits {
                max_per_task_per_minute: 0,
                ..d
            },
            MessagingLimits {
                max_per_task_per_minute: HARD_PER_MINUTE + 1,
                ..d
            },
        ];
        for c in cases {
            assert!(c.validate().is_err(), "{c:?}");
        }
    }
}
