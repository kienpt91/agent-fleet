//! SSH remote worktrees with the system OpenSSH client.
//!
//! * [`binary`]: the `ssh` program, resolved once to an absolute path outside any repository.
//! * [`argv`]: the fixed command line of the client (no shell, no forwarding, no agent, one key,
//!   one pinned known_hosts file).
//! * [`known_hosts`]: host key pinning, written only by `fleet host trust`.
//! * [`exec`]: running the client without a terminal with bounded time, bytes and silence.

pub mod argv;
pub mod binary;
pub mod client;
pub mod doctor;
pub mod exec;
pub mod host;
pub mod known_hosts;
pub mod trust;

use fleet_domain::error::Result;
use std::path::PathBuf;

/// The `ssh` program: the configured absolute path, or the first acceptable one in an absolute
/// entry of `path_env` (see [`binary::resolve_tool`]). Resolved once; callers keep the result.
pub fn locate_ssh(configured: Option<&str>, path_env: &str) -> Result<PathBuf> {
    match configured {
        Some(path) => binary::check_configured_tool(path),
        None => binary::resolve_tool("ssh", path_env),
    }
}
