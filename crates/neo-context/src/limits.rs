//! Hard caps of the context compiler. Every input is bounded before it is looked at, and every
//! layer has a token budget; nothing here depends on the model or on the machine.

/// Version of the rendered form and of the manifest. A change of either is a new version: packs of
/// different versions are never compared byte for byte.
pub const PACK_VERSION: u32 = 1;

pub const MAX_PROFILE_NAME: usize = 40;
pub const MIN_MAX_TOKENS: u32 = 8_192;
pub const DEFAULT_MAX_TOKENS: u32 = 32_000;
pub const MAX_MAX_TOKENS: u32 = 200_000;
pub const DEFAULT_CHARS_PER_TOKEN: u32 = 4;
pub const MIN_CHARS_PER_TOKEN: u32 = 1;
pub const MAX_CHARS_PER_TOKEN: u32 = 16;
/// The safety factor of the estimator in percent: 125 means "count a quarter more tokens than
/// bytes / chars-per-token says".
pub const DEFAULT_SAFETY_PERCENT: u32 = 125;
pub const MIN_SAFETY_PERCENT: u32 = 100;
pub const MAX_SAFETY_PERCENT: u32 = 400;

/// Per-layer hard budgets as a percentage of the profile's `max_tokens`, in layer order. They add
/// up to more than 100: a layer may use its whole budget, the total never exceeds the profile,
/// and the shrink order (7, 6, 5, then 2) decides who gives way.
pub const LAYER_PERCENT: [u32; 8] = [12, 15, 35, 50, 15, 35, 15, 5];

// ---- bounds on what the callers hand in -----------------------------------------------------

pub const MAX_ID: usize = 64;
pub const MAX_TITLE: usize = 200;
pub const MAX_OBJECTIVE: usize = 2 * 1024;
pub const MAX_SPEC: usize = 16 * 1024;
pub const MAX_CONSTRAINTS: usize = 32;
pub const MAX_CONSTRAINT: usize = 400;
pub const MAX_GLOSSARY: usize = 64;
pub const MAX_GLOSSARY_TERM: usize = 60;
pub const MAX_GLOSSARY_MEANING: usize = 400;

pub const MAX_GOALS: usize = 16;
pub const MAX_INTENT: usize = 600;
pub const MAX_TASKS_LISTED: usize = 40;
pub const MAX_CHECKS: usize = 32;
pub const MAX_CHECK_TEXT: usize = 400;

pub const MAX_FACTS: usize = 64;
pub const MAX_FACT_ID: usize = 40;
pub const MAX_FACT_CLAIM: usize = 600;
pub const MAX_FACT_SOURCE: usize = 300;

pub const MAX_EXCERPTS: usize = 12;
/// Bytes of one file shown in the working set.
pub const MAX_EXCERPT_BYTES: usize = 4 * 1024;
/// Bytes of one file the compiler scans for secrets (the adapter reads at most this much).
pub const MAX_SCAN_BYTES: usize = 64 * 1024;
pub const MAX_PATH: usize = 300;
pub const MAX_STAT_PATHS: usize = 40;

pub const MAX_EVENTS: usize = 40;
pub const MAX_EVENT_LINE: usize = 240;
pub const MAX_UNVERIFIED_ITEMS: usize = 24;
pub const MAX_UNVERIFIED_ITEM: usize = 2 * 1024;
pub const MAX_SUMMARY: usize = 2 * 1024;
pub const MAX_EVIDENCE: usize = 8;
pub const MAX_EVIDENCE_OUTPUT: usize = 1024;
pub const MAX_POINTERS: usize = 32;
/// Names of dropped items listed in the marker that replaces them.
pub const MAX_MARKED_DROPS: usize = 6;
/// Notes of earlier attempts shown in layer 7.
pub const MAX_NOTES: usize = 12;
