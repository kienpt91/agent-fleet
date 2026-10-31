//! Application layer: the use cases of the fleet orchestrator.
//!
//! Depends on `fleet-domain` only. Everything that touches the outside world (files, git, ptys,
//! processes, the clock) is a trait in [`ports`]; adapters live in `fleet-infra`.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
pub mod api;
pub mod config;
pub mod orchestrator;
pub mod ports;
pub mod views;
