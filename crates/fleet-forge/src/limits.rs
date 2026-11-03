//! Caps on everything read from a forge. A forge is a network service whose content is written by
//! strangers: sizes are bounded before they are parsed and again after.

/// Most bytes of one `gh` answer that are parsed (headers and body together).
pub const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
/// Most bytes of the header block of an answer.
pub const MAX_HEADER_BYTES: usize = 16 * 1024;
/// Most items of one page, whatever the caller asks for.
pub const MAX_PAGE_ITEMS: usize = 50;
/// Items per page when the caller does not say.
pub const DEFAULT_PAGE_ITEMS: usize = 20;
/// Most pages a caller can walk to in one command (`--page`).
pub const MAX_PAGE: u64 = 20;
/// Longest title kept (GitHub allows 256).
pub const MAX_TITLE: usize = 256;
/// Longest body kept; the rest is cut and marked.
pub const MAX_BODY: usize = 16 * 1024;
/// Longest login, label, branch name or similar short text.
pub const MAX_SHORT: usize = 100;
/// Longest URL kept.
pub const MAX_URL: usize = 300;
/// Most labels of one item.
pub const MAX_LABELS: usize = 30;
/// Most check runs counted for one commit.
pub const MAX_CHECKS: usize = 200;
/// Most reviews read for one pull request.
pub const MAX_REVIEWS: usize = 100;
/// Longest message of an error answer that is shown.
pub const MAX_ERROR_MESSAGE: usize = 300;
/// Longest pull request body that can be approved (an approval action argument).
pub const MAX_PR_BODY: usize = 4 * 1024;
/// Most links and publications kept in the projection.
pub const MAX_LINKS: usize = 512;
/// Largest issue or pull request number accepted.
pub const MAX_NUMBER: u64 = 100_000_000;
