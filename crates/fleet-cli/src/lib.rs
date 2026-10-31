//! The `fleet` command line, terminal dashboard and interactive attach.
//!
//! This is the outermost layer: it parses arguments, talks to the daemon over its control socket
//! through `fleet-infra`, and renders results. It holds no orchestration logic.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

pub mod args;
pub mod attach;
pub mod commands;
pub mod render;
pub mod render_msg;
pub mod render_neo;
pub mod tui;

pub use commands::run;
