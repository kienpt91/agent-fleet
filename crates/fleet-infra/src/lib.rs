//! Infrastructure layer: adapters that implement the ports of `fleet-app` with real files, git,
//! processes, ptys and sockets. Depends inward on `fleet-app` and `fleet-domain`.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

pub mod json;
pub mod digest;
pub mod clock;
pub mod fsx;
pub mod statedir;
pub mod procid;
pub mod eventlog;
pub mod logs;
pub mod contract_fs;
pub mod git;
pub mod jobs;
pub mod undo;
pub mod protocol;
pub mod server;
pub mod client;
pub mod config_file;
pub mod memory;
pub mod pty;

#[doc(hidden)]
pub mod testutil;
