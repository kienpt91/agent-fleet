//! Infrastructure layer: adapters that implement the ports of `fleet-app` with real files, git,
//! processes, ptys and sockets. Depends inward on `fleet-app` and `fleet-domain`.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

pub mod client;
pub mod clock;
pub mod config_file;
pub mod context_store;
pub mod contract_fs;
pub mod daemon;
pub mod digest;
pub mod doctor;
pub mod eventlog;
pub mod forge_gh;
pub mod forge_jobs;
pub mod fsx;
pub mod git;
mod git_diff;
pub mod jobs;
pub mod json;
pub mod knowledge;
pub mod logs;
pub mod memory;
pub mod procid;
pub mod protocol;
pub mod pty;
pub mod server;
pub mod statedir;
pub mod toolrun;
pub mod undo;
pub mod verify;
pub mod web;

#[doc(hidden)]
pub mod testutil;
