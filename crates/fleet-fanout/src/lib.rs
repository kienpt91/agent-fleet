//! Fan-out of agent-fleet: one prompt given to several agents, their results compared, one picked.
//!
//! * [`group`]: the [`group::Groups`] aggregate, a projection of the event log (`group_created`,
//!   `member_finished`, `winner_picked`, `group_closed`) with its state machine.
//! * [`diff`]: what a member's branch changed (files, lines), as read from the repository, and
//!   which of those paths a diff constraint allows.
//! * [`rank`]: the advisory ranking rule: an ordered list of named criteria over facts the daemon
//!   knows, nothing hidden, never a decision.
//! * [`compare`]: the facts of every member, how its verification stands, and which files several
//!   members changed.
//!
//! Like `fleet-domain` the crate is pure: std only (plus the two domain crates below it), no I/O,
//! no clock, no randomness. Text that came from a worker or from a repository (summaries, paths) is
//! kept as it was read; the views that show it sanitize it.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

pub mod compare;
pub mod diff;
pub mod group;
pub mod rank;
