//! Domain layer: pure rules of the fleet orchestrator.
//!
//! This crate depends on `std` only and performs no I/O: no file system, process, network,
//! clock or randomness. Time, hashing and ids reach it through arguments and the small port
//! traits declared here.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

pub mod approval;
pub mod brief;
pub mod chain;
pub mod contract;
pub mod dag;
pub mod decision;
pub mod env;
pub mod error;
pub mod events;
pub mod fleet;
pub mod ids;
pub mod limits;
pub mod msg;
pub mod output;
pub mod quality;
pub mod redact;
pub mod repo;
pub mod run;
pub mod sanitize;
pub mod status;
pub mod supervision;
pub mod value;
