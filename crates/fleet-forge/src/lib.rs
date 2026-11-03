//! The forge model of agent-fleet: how a code host (GitHub today) and an issue tracker look to the
//! domain, with every rule that does not need I/O.
//!
//! * [`repo`] names a repository on a host and reads remote URLs (and refuses URLs that carry
//!   credentials).
//! * [`model`] and [`schema`] are the values read from a forge and the strict schemas they are read
//!   with: unknown keys, wrong types, oversized or hostile text are rejected or cleaned before any
//!   other layer sees them.
//! * [`untrusted`] turns an issue into the text of a task: quoted, labelled, never trusted.
//! * [`publish`] is the pull request a human approves: its digest covers repository, base, head
//!   branch and commit, title, body and the draft flag.
//! * [`links`] is the projection of the forge events: which task came from which issue, which
//!   pull request came from which task, and which publication never reported back.
//! * [`rate`] is the rate-limit gate: a budget, back-off with an injected jitter, a clear status.
//! * [`config`] is the validated `[github]` section.
//!
//! Like `fleet-domain` the crate is pure: std only, no I/O, no clock, no randomness.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

pub mod config;
pub mod limits;
pub mod links;
pub mod model;
pub mod publish;
pub mod push;
pub mod rate;
pub mod repo;
pub mod response;
pub mod schema;
pub mod untrusted;
