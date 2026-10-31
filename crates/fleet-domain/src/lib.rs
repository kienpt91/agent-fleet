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

pub mod error;
pub mod ids;
pub mod limits;
pub mod redact;
pub mod sanitize;
