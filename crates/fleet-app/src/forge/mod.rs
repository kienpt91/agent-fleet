//! Forge integration of the application layer: the neutral ports (`IssueTracker` for issues,
//! `CodeHost` for pull requests, `BranchPublisher` for pushing a branch), the asynchronous wrapper
//! the orchestrator talks to (`ForgeJobs`), the wire commands and how a request is executed
//! against the ports.
//!
//! Nothing here knows GitHub, `gh` or git: the adapters in `fleet-infra` do. A later tracker
//! (Linear, say) implements `IssueTracker` and needs no change to the use cases in
//! `orchestrator::forge`.

pub mod api;
pub mod exec;
pub mod ports;

pub use ports::*;
