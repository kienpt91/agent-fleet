//! Goal ledger of NEO: what the objective is, what is left of it, and what proves it done.
//!
//! The [`ledger::Ledger`] is an aggregate rebuilt from domain events (`GoalAdded`,
//! `GoalStatusChanged`, `CriteriaAttached`, `EvidenceRecorded`). Its command methods validate and
//! return events without changing anything; `apply` folds an event in. Status `verified` is
//! reached only through recorded evidence (and verified children): no command sets it.
//! Like `fleet-domain` the crate is pure: std only, no I/O, no clock.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

pub mod goal;
pub mod ledger;
pub mod limits;
