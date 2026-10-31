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
pub mod exec;
pub mod host;
pub mod known_hosts;
pub mod trust;
