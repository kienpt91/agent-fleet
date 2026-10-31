//! The read-only repository workbench: what a repository looks like at a recorded commit.
//!
//! Everything here is about the real repository at commits the daemon recorded (ADR 16), never
//! about a worktree. Names, contents and diffs come from agents and are untrusted: they are
//! validated, bounded and escaped here, and shown as text by every front end.

pub mod limits;
pub mod markdown;
pub mod path;

pub use path::RepoPath;
