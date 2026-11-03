//! Application layer: the use cases of the fleet orchestrator.
//!
//! Depends on the domain crates (`fleet-domain`, `fleet-fanout`, `fleet-forge`, `neo-goals`, `neo-verify`, `neo-context`) only. Everything that
//! touches the outside world (files, git, ptys, processes, the clock) is a trait in [`ports`];
//! adapters live in `fleet-infra`.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

pub mod api;
pub mod browse;
pub mod config;
mod fanout;
pub mod forge;
pub mod hosts;
pub mod msg;
pub mod neo;
pub mod orchestrator;
pub mod ports;
pub mod remote;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod views;
