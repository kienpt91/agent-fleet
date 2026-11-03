//! Agent-to-agent messaging, mediated by the daemon.
//!
//! Agents do not talk to each other and nothing is typed into a terminal. A worker writes a file
//! to its own `.fleet/outbox/`; the daemon reads it with the care it gives every worker file,
//! decides under the operator's policy whether it may be delivered, records the decision in the
//! audit log and writes accepted messages into the inbox of a running recipient. Everything is
//! declared by the operator (channels, members, limits), bounded, rate limited, de-duplicated,
//! checked for loops and storms, auditable and mutable by a person (`fleet msg mute`).
//!
//! The content of a message is **untrusted peer content**. It is sanitized, redacted and
//! clipped, shown with a label that says who wrote it, and never used as a contract, an approval,
//! an acceptance criterion or any other field the daemon treats as true.
//!
//! This module is pure: clocks, files and hashing reach it as arguments.
//!
//! | module | what |
//! |--------|------|
//! | [`channel`] | the channel an operator declares, membership, the roster of a run |
//! | [`message`] | the value objects: sender, outbox file, message and its state machine |
//! | [`board`] | the aggregate: submission policy, delivery planning, mutes, replay |
//! | [`loops`] | reply-depth and pair-budget counting |
//! | [`ratelimit`] | sliding windows over an injected clock |
//! | [`secrets`] | the shapes of keys and tokens that make a message refused |
//! | [`inbox`] | what is written into an inbox and what a brief says about messages |
//! | [`limits`] | every cap and default |

pub mod board;
pub mod channel;
pub mod inbox;
pub mod limits;
pub mod loops;
pub mod message;
pub mod ratelimit;
pub mod secrets;
