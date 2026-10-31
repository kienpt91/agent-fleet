//! Hard caps of the read-only repository workbench. Everything a repository (and so a worker)
//! controls has one of these: a path, a directory, a file, a diff.

/// Longest repository path, in bytes.
pub const MAX_PATH_BYTES: usize = 1024;
/// Longest single path component, in bytes (what common file systems allow).
pub const MAX_COMPONENT_BYTES: usize = 255;
/// Deepest repository path, in components.
pub const MAX_PATH_DEPTH: usize = 32;

/// Entries of one directory listing; a larger directory is cut and says so.
pub const MAX_TREE_ENTRIES: usize = 2000;
/// Characters of a displayed name; a longer one is cut.
pub const MAX_NAME_CHARS: usize = 255;

/// Largest file shown as text. A larger one gets a placeholder with its size.
pub const MAX_FILE_VIEW_BYTES: usize = 1024 * 1024;
/// Most lines of one file that are sent; a longer file is cut and says so.
pub const MAX_FILE_VIEW_LINES: usize = 20_000;
/// Longest line sent, in characters; a longer one is clipped with a marker.
pub const MAX_LINE_CHARS: usize = 2000;
/// Bytes at the start of a file that decide between text and binary.
pub const SNIFF_BYTES: usize = 8000;
/// Largest image the blob API serves.
pub const MAX_IMAGE_BYTES: usize = 4 * 1024 * 1024;
/// Largest Markdown document that is rendered; a larger one is shown as text.
pub const MAX_MARKDOWN_BYTES: usize = 512 * 1024;

/// Files of one diff; more are counted and not shown.
pub const MAX_DIFF_FILES: usize = 300;
/// Hunks shown of one file.
pub const MAX_DIFF_HUNKS_PER_FILE: usize = 200;
/// Lines (context included) shown of one file.
pub const MAX_DIFF_LINES_PER_FILE: usize = 4000;
/// Lines shown of the whole diff.
pub const MAX_DIFF_LINES_TOTAL: usize = 30_000;
/// Bytes of line text shown of the whole diff.
pub const MAX_DIFF_TEXT_BYTES: usize = 3 * 1024 * 1024;
/// Most lines of context a request may ask for.
pub const MAX_DIFF_CONTEXT: u32 = 25;
/// Context lines when the request names none.
pub const DEFAULT_DIFF_CONTEXT: u32 = 3;
/// Files that rename detection compares (it is quadratic in this number).
pub const RENAME_LIMIT: u32 = 200;
