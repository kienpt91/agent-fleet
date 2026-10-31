//! Hard caps of the goal ledger.

pub const MAX_GOALS: usize = 512;
/// Levels of the goal tree; a root goal is at depth 1.
pub const MAX_DEPTH: usize = 8;
pub const MAX_CHILDREN: usize = 64;
pub const MAX_TASKS_PER_GOAL: usize = 16;
pub const MAX_INTENT: usize = 4 * 1024;
pub const MAX_REASON: usize = 400;
/// Pieces of evidence kept in the ledger; a verification that would exceed it is refused.
pub const MAX_EVIDENCE: usize = 4096;
