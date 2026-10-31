//! Hard caps of the verification context. A contract comes from a file an operator wrote, but it
//! is still parsed as untrusted input, and everything recorded from a check is bounded.

/// Largest accepted contract document (the same cap applies to the stored canonical form).
pub const MAX_CONTRACT_BYTES: usize = 32 * 1024;
pub const MAX_CHECKS: usize = 32;
pub const MAX_CHECK_ID: usize = 40;
pub const MAX_PROGRAM_LEN: usize = 4 * 1024;
pub const MAX_ARGS: usize = 32;
pub const MAX_ARG_LEN: usize = 4 * 1024;
pub const MAX_PATH_LEN: usize = 512;
pub const MAX_NEEDLE: usize = 1024;
pub const MAX_GLOBS: usize = 32;
/// A deny list enumerates what a task must never touch, so it may be longer than an allow list.
pub const MAX_DENY_GLOBS: usize = 128;
pub const MAX_GLOB_LEN: usize = 256;
/// Longest path the glob matcher looks at; longer paths never match.
pub const MAX_MATCH_PATH: usize = 4 * 1024;

pub const MIN_TIMEOUT_MS: u64 = 100;
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const MAX_TIMEOUT_MS: u64 = 1_800_000;
/// All checks of one verification job together may take this long.
pub const JOB_BUDGET_MS: u64 = 1_800_000;
/// Verification jobs that run at the same time (a hung command holds one of these, not every
/// job slot of the daemon).
pub const MAX_CONCURRENT_VERIFICATIONS: usize = 2;

/// Largest file a file assertion reads (to hash it or to search it).
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Paths of a diff the diff constraint examines; a larger change is an error, not a pass.
pub const MAX_CHANGED_PATHS: usize = 10_000;

/// Output kept in a piece of evidence (the tail).
pub const MAX_EVIDENCE_OUTPUT: usize = 2 * 1024;
pub const MAX_EVIDENCE_SUMMARY: usize = 400;
/// Offending paths named in the summary of a failed diff constraint.
pub const MAX_PATHS_SHOWN: usize = 10;
