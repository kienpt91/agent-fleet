//! A diff between two commits as a person reviews it: files with a status and counts, hunks of
//! numbered lines, notices where something is not shown.
//!
//! The text of a diff comes from the repository, so everything is bounded ([`DiffBudget`]) and
//! decoded leniently. `git` prints the diff in a fixed format; this module reads that format
//! (a unified patch, one file at a time) and never executes or follows anything in it. A file whose
//! patch is cut by a limit says so; the totals of a file come from `git diff --numstat`, not
//! from the lines that were kept.

use super::limits::{
    MAX_DIFF_FILES, MAX_DIFF_HUNKS_PER_FILE, MAX_DIFF_LINES_PER_FILE, MAX_DIFF_LINES_TOTAL,
    MAX_DIFF_TEXT_BYTES, MAX_LINE_CHARS, MAX_PATH_BYTES,
};
use super::path::RepoPath;
use super::tree::{display_text, EntryKind};
use crate::ids::Sha;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    ModeChanged,
    TypeChanged,
}

impl FileStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FileStatus::Added => "added",
            FileStatus::Modified => "modified",
            FileStatus::Deleted => "deleted",
            FileStatus::Renamed => "renamed",
            FileStatus::Copied => "copied",
            FileStatus::ModeChanged => "mode_changed",
            FileStatus::TypeChanged => "type_changed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Add,
    Del,
}

impl LineKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LineKind::Context => "ctx",
            LineKind::Add => "add",
            LineKind::Del => "del",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// Line number in the old file (not for added lines).
    pub old: Option<u32>,
    /// Line number in the new file (not for removed lines).
    pub new: Option<u32>,
    /// The text without its line ending, clipped when very long.
    pub text: String,
    /// The line ended with CRLF.
    pub cr: bool,
    /// The file ends here without a final line ending.
    pub no_newline: bool,
    /// Changed ranges inside the line (UTF-16 code unit offsets), when it was compared with its
    /// counterpart.
    pub marks: Vec<(u32, u32)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    /// The text git prints after the second `@@` (usually the enclosing function).
    pub section: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffFile {
    /// The new path (or the only one), safe to show.
    pub path: String,
    /// The old path of a rename or copy.
    pub old_path: Option<String>,
    /// The new path can be requested back from the file viewer.
    pub path_ok: bool,
    pub old_path_ok: bool,
    pub status: FileStatus,
    /// Similarity of a rename or copy, in percent.
    pub similarity: Option<u8>,
    pub old_mode: Option<u32>,
    pub new_mode: Option<u32>,
    pub binary: bool,
    /// A submodule pointer changed.
    pub submodule: bool,
    pub additions: u32,
    pub deletions: u32,
    pub hunks: Vec<Hunk>,
    /// Some hunks or lines are not here.
    pub truncated: bool,
    pub note: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiffStats {
    pub files: usize,
    pub additions: u64,
    pub deletions: u64,
    pub binary: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffSet {
    pub base: Sha,
    pub head: Sha,
    pub context: u32,
    pub files: Vec<DiffFile>,
    /// Files were left out.
    pub truncated: bool,
    pub notice: Option<String>,
}

impl DiffSet {
    pub fn stats(&self) -> DiffStats {
        let mut s = DiffStats {
            files: self.files.len(),
            ..DiffStats::default()
        };
        for f in &self.files {
            s.additions += u64::from(f.additions);
            s.deletions += u64::from(f.deletions);
            if f.binary {
                s.binary += 1;
            }
        }
        s
    }
}

// ---- budget ----------------------------------------------------------------------------------

/// What may still be shown of a diff. One budget is spent across all files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiffBudget {
    pub lines_left: usize,
    pub text_left: usize,
    pub hunks_per_file: usize,
    pub lines_per_file: usize,
}

impl Default for DiffBudget {
    fn default() -> DiffBudget {
        DiffBudget {
            lines_left: MAX_DIFF_LINES_TOTAL,
            text_left: MAX_DIFF_TEXT_BYTES,
            hunks_per_file: MAX_DIFF_HUNKS_PER_FILE,
            lines_per_file: MAX_DIFF_LINES_PER_FILE,
        }
    }
}

impl DiffBudget {
    pub fn exhausted(&self) -> bool {
        self.lines_left == 0 || self.text_left == 0
    }
}

// ---- what git says about the changed files ---------------------------------------------------

/// One entry of `git diff --raw -z`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    /// The status letter: `A M D R C T U` (anything else counts as modified).
    pub status: u8,
    pub old_mode: u32,
    pub new_mode: u32,
    pub old_path: Vec<u8>,
    pub new_path: Vec<u8>,
    pub similarity: Option<u8>,
}

/// One entry of `git diff --numstat -z`: `None` counts mean a binary file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NumStat {
    pub additions: Option<u32>,
    pub deletions: Option<u32>,
}

/// How a file's patch starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchShape {
    Plain,
    New,
    Deleted,
    Rename,
    Copy,
}

/// The part of a patch that was kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePatch {
    pub shape: PatchShape,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
    pub truncated: bool,
    /// Additions and deletions among the kept lines.
    pub kept_additions: u32,
    pub kept_deletions: u32,
}

/// Cuts the output of `git diff -p` into one chunk per file (each starts with `diff --git `).
pub fn split_patch(patch: &[u8]) -> Vec<&[u8]> {
    let mut chunks = Vec::new();
    let mut start: Option<usize> = None;
    let mut at = 0usize;
    while at < patch.len() {
        let rest = patch.get(at..).unwrap_or(&[]);
        if rest.starts_with(b"diff --git ") {
            if let Some(s) = start {
                chunks.push(patch.get(s..at).unwrap_or(&[]));
            }
            start = Some(at);
        }
        match rest.iter().position(|b| *b == b'\n') {
            Some(i) => at += i + 1,
            None => break,
        }
    }
    if let Some(s) = start {
        chunks.push(patch.get(s..).unwrap_or(&[]));
    }
    chunks
}

fn parse_u32(s: &[u8]) -> Option<u32> {
    if s.is_empty() || s.len() > 9 || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(s).ok()?.parse().ok()
}

/// `-12,3` or `-12` into (12, 3) / (12, 1).
fn range(s: &[u8], sign: u8) -> Option<(u32, u32)> {
    let s = s.strip_prefix(&[sign])?;
    match s.iter().position(|b| *b == b',') {
        Some(i) => Some((parse_u32(s.get(..i)?)?, parse_u32(s.get(i + 1..)?)?)),
        None => Some((parse_u32(s)?, 1)),
    }
}

/// `@@ -a,b +c,d @@ section` into the four numbers and the section.
fn hunk_header(line: &[u8]) -> Option<(u32, u32, u32, u32, &[u8])> {
    let rest = line.strip_prefix(b"@@ ")?;
    let end = rest.windows(3).position(|w| w == b" @@")?;
    let numbers = rest.get(..end)?;
    let section = rest.get(end + 3..).unwrap_or(&[]);
    let section = section.strip_prefix(b" ").unwrap_or(section);
    let sp = numbers.iter().position(|b| *b == b' ')?;
    let (os, oc) = range(numbers.get(..sp)?, b'-')?;
    let (ns, nc) = range(numbers.get(sp + 1..)?, b'+')?;
    Some((os, oc, ns, nc, section))
}

fn clip_line(text: &str) -> String {
    match text.char_indices().nth(MAX_LINE_CHARS) {
        None => text.to_string(),
        Some((at, _)) => {
            let rest = text.get(at..).map_or(0, |r| r.chars().count());
            format!(
                "{} \u{2026}[{rest} more characters]",
                text.get(..at).unwrap_or(text)
            )
        }
    }
}

/// Reads the patch of one file (a chunk from [`split_patch`]), keeping what the budget allows.
/// A hunk is read by its line counts, so a line of the file that looks like a header cannot end it
/// early; a hunk whose counts do not match its lines ends where the next header starts.
pub fn parse_file_patch(chunk: &[u8], budget: &mut DiffBudget) -> FilePatch {
    let mut patch = FilePatch {
        shape: PatchShape::Plain,
        binary: false,
        hunks: Vec::new(),
        truncated: false,
        kept_additions: 0,
        kept_deletions: 0,
    };
    let mut lines = chunk.split(|b| *b == b'\n').peekable();
    // the header: `diff --git`, mode lines, `index`, `---`, `+++`
    while let Some(line) = lines.peek() {
        if line.starts_with(b"@@ ") {
            break;
        }
        if line.starts_with(b"Binary files ") || line.starts_with(b"GIT binary patch") {
            patch.binary = true;
        } else if line.starts_with(b"new file mode ") {
            patch.shape = PatchShape::New;
        } else if line.starts_with(b"deleted file mode ") {
            patch.shape = PatchShape::Deleted;
        } else if line.starts_with(b"rename from ") {
            patch.shape = PatchShape::Rename;
        } else if line.starts_with(b"copy from ") {
            patch.shape = PatchShape::Copy;
        }
        lines.next();
    }
    let mut file_lines = 0usize;
    'hunks: while let Some(line) = lines.next() {
        let Some((os, oc, ns, nc, section)) = hunk_header(line) else {
            continue;
        };
        if patch.hunks.len() >= budget.hunks_per_file || budget.exhausted() {
            patch.truncated = true;
            break;
        }
        let mut hunk = Hunk {
            old_start: os,
            old_count: oc,
            new_start: ns,
            new_count: nc,
            section: display_text(section, 200),
            lines: Vec::new(),
        };
        let (mut old_left, mut new_left) = (oc, nc);
        let (mut old_no, mut new_no) = (os, ns);
        while old_left > 0 || new_left > 0 || lines.peek().is_some_and(|l| l.starts_with(b"\\")) {
            let Some(raw) = lines.peek().copied() else {
                break;
            };
            if raw.starts_with(b"@@ ") && (old_left > 0 || new_left > 0) {
                // the counts promised more than the patch has: the next hunk starts here
                break;
            }
            lines.next();
            let (kind, body) = match raw.split_first() {
                Some((b' ', rest)) => (LineKind::Context, rest),
                Some((b'+', rest)) => (LineKind::Add, rest),
                Some((b'-', rest)) => (LineKind::Del, rest),
                Some((b'\\', _)) => {
                    // "\ No newline at end of file" belongs to the line before it
                    if let Some(prev) = hunk.lines.last_mut() {
                        prev.no_newline = true;
                    }
                    continue;
                }
                // git always prints a space for an empty context line; some tools drop it. The
                // empty piece after the last line ending is not a line.
                None if lines.peek().is_none() => break,
                None => (LineKind::Context, &b""[..]),
                Some(_) => break,
            };
            let counts_old = kind != LineKind::Add;
            let counts_new = kind != LineKind::Del;
            if (counts_old && old_left == 0) || (counts_new && new_left == 0) {
                break;
            }
            if budget.lines_left == 0
                || budget.text_left == 0
                || file_lines >= budget.lines_per_file
            {
                patch.truncated = true;
                if !hunk.lines.is_empty() {
                    patch.hunks.push(hunk);
                }
                break 'hunks;
            }
            let decoded = String::from_utf8_lossy(body);
            let (text, cr) = match decoded.strip_suffix('\r') {
                Some(t) => (t, true),
                None => (&*decoded, false),
            };
            let text = clip_line(text);
            budget.lines_left -= 1;
            budget.text_left = budget.text_left.saturating_sub(text.len() + 1);
            file_lines += 1;
            let line = DiffLine {
                kind,
                old: counts_old.then_some(old_no),
                new: counts_new.then_some(new_no),
                text,
                cr,
                no_newline: false,
                marks: Vec::new(),
            };
            match kind {
                LineKind::Context => {
                    old_no = old_no.saturating_add(1);
                    new_no = new_no.saturating_add(1);
                    old_left -= 1;
                    new_left -= 1;
                }
                LineKind::Add => {
                    new_no = new_no.saturating_add(1);
                    new_left -= 1;
                    patch.kept_additions = patch.kept_additions.saturating_add(1);
                }
                LineKind::Del => {
                    old_no = old_no.saturating_add(1);
                    old_left -= 1;
                    patch.kept_deletions = patch.kept_deletions.saturating_add(1);
                }
            }
            hunk.lines.push(line);
        }
        patch.hunks.push(hunk);
    }
    patch
}

// ---- assembling a file -----------------------------------------------------------------------

fn path_for_request(bytes: &[u8]) -> bool {
    bytes.len() <= MAX_PATH_BYTES
        && std::str::from_utf8(bytes)
            .ok()
            .is_some_and(|s| !s.is_empty() && RepoPath::parse(s).is_ok())
}

/// The status a raw letter stands for, given what the numbers say.
fn status_of(change: &FileChange, additions: u32, deletions: u32, binary: bool) -> FileStatus {
    match change.status {
        b'A' => FileStatus::Added,
        b'D' => FileStatus::Deleted,
        b'R' => FileStatus::Renamed,
        b'C' => FileStatus::Copied,
        b'T' => FileStatus::TypeChanged,
        _ if change.old_mode != change.new_mode && additions == 0 && deletions == 0 && !binary => {
            FileStatus::ModeChanged
        }
        _ => FileStatus::Modified,
    }
}

/// The entry of one changed file: what git listed, its counts and the part of its patch that was
/// kept (`None` when the patch was not read, for example because the budget was spent).
pub fn assemble_file(
    change: &FileChange,
    stat: Option<NumStat>,
    patch: Option<FilePatch>,
) -> DiffFile {
    let binary = match (&stat, &patch) {
        (Some(s), _) => s.additions.is_none() || s.deletions.is_none(),
        (None, Some(p)) => p.binary,
        (None, None) => false,
    };
    let (additions, deletions) = match (&stat, &patch) {
        (Some(s), _) => (s.additions.unwrap_or(0), s.deletions.unwrap_or(0)),
        (None, Some(p)) => (p.kept_additions, p.kept_deletions),
        (None, None) => (0, 0),
    };
    let status = status_of(change, additions, deletions, binary);
    let renamed = matches!(status, FileStatus::Renamed | FileStatus::Copied);
    let submodule = [change.old_mode, change.new_mode]
        .iter()
        .any(|m| EntryKind::from_mode(*m) == EntryKind::Submodule && *m != 0);
    let (hunks, truncated, patch_read) = match patch {
        Some(p) => (p.hunks, p.truncated, true),
        None => (Vec::new(), false, false),
    };
    // a file whose content did not change (a mode or type change, a pure rename) has no lines to
    // be missing
    let missing = !patch_read && !binary && (additions > 0 || deletions > 0);
    let note = if binary {
        Some("Binary file, not shown.".to_string())
    } else if missing {
        Some("Not shown: the diff is larger than the limits.".to_string())
    } else if truncated {
        Some("Only the first part of this file's diff is shown.".to_string())
    } else {
        None
    };
    DiffFile {
        path: display_text(&change.new_path, MAX_PATH_BYTES),
        old_path: renamed.then(|| display_text(&change.old_path, MAX_PATH_BYTES)),
        path_ok: path_for_request(&change.new_path) && status != FileStatus::Deleted,
        old_path_ok: renamed && path_for_request(&change.old_path),
        status,
        similarity: if renamed { change.similarity } else { None },
        old_mode: (change.old_mode != 0).then_some(change.old_mode),
        new_mode: (change.new_mode != 0).then_some(change.new_mode),
        binary,
        submodule,
        additions,
        deletions,
        hunks,
        truncated: truncated || missing,
        note,
    }
}

/// The most files a diff shows.
pub const FILE_LIMIT: usize = MAX_DIFF_FILES;

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(patch: &str) -> Vec<&[u8]> {
        split_patch(patch.as_bytes())
    }

    fn parse(patch: &str) -> FilePatch {
        let mut b = DiffBudget::default();
        parse_file_patch(patch.as_bytes(), &mut b)
    }

    const SIMPLE: &str = "diff --git a/f.txt b/f.txt\nindex 1111111..2222222 100644\n--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,4 @@ fn main()\n one\n-two\n+TWO\n+two and a half\n three\n";

    #[test]
    fn a_simple_hunk_is_read_with_numbers() {
        let p = parse(SIMPLE);
        assert_eq!(p.shape, PatchShape::Plain);
        assert!(!p.binary && !p.truncated);
        assert_eq!(p.hunks.len(), 1);
        let h = &p.hunks[0];
        assert_eq!(
            (h.old_start, h.old_count, h.new_start, h.new_count),
            (1, 3, 1, 4)
        );
        assert_eq!(h.section, "fn main()");
        let got: Vec<(LineKind, Option<u32>, Option<u32>, &str)> = h
            .lines
            .iter()
            .map(|l| (l.kind, l.old, l.new, l.text.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (LineKind::Context, Some(1), Some(1), "one"),
                (LineKind::Del, Some(2), None, "two"),
                (LineKind::Add, None, Some(2), "TWO"),
                (LineKind::Add, None, Some(3), "two and a half"),
                (LineKind::Context, Some(3), Some(4), "three"),
            ]
        );
        assert_eq!((p.kept_additions, p.kept_deletions), (2, 1));
    }

    #[test]
    fn counts_may_be_omitted_and_the_marker_for_a_missing_newline_sticks_to_its_line() {
        let p = parse("diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n");
        let h = &p.hunks[0];
        assert_eq!((h.old_count, h.new_count), (1, 1));
        assert_eq!(h.lines.len(), 2);
        assert!(h.lines.iter().all(|l| l.no_newline));
    }

    #[test]
    fn crlf_is_flagged_and_removed_from_the_text() {
        let p = parse("diff --git a/x b/x\n@@ -1,2 +1,2 @@\n a\r\n-b\r\n+c\n");
        let l = &p.hunks[0].lines;
        assert!(l[0].cr && l[1].cr && !l[2].cr);
        assert_eq!(l[0].text, "a");
    }

    #[test]
    fn several_hunks_and_several_files() {
        let two = "diff --git a/a b/a\n@@ -1 +1 @@\n-x\n+y\n@@ -10,2 +10,2 @@\n k\n-m\n+n\n";
        let p = parse(two);
        assert_eq!(p.hunks.len(), 2);
        assert_eq!(p.hunks[1].lines[0].old, Some(10));
        let all =
            format!("{SIMPLE}diff --git a/b b/b\nnew file mode 100644\n@@ -0,0 +1 @@\n+hello\n");
        let c = chunks(&all);
        assert_eq!(c.len(), 2);
        assert_eq!(
            parse_file_patch(c[1], &mut DiffBudget::default()).shape,
            PatchShape::New
        );
        assert!(chunks("").is_empty());
        assert!(chunks("no header at all\n").is_empty());
    }

    #[test]
    fn headers_decide_the_shape_and_binary_files_have_no_hunks() {
        for (header, shape) in [
            ("new file mode 100644", PatchShape::New),
            ("deleted file mode 100644", PatchShape::Deleted),
            (
                "similarity index 90%\nrename from a\nrename to b",
                PatchShape::Rename,
            ),
            (
                "similarity index 90%\ncopy from a\ncopy to b",
                PatchShape::Copy,
            ),
            ("old mode 100644\nnew mode 100755", PatchShape::Plain),
        ] {
            let p = parse(&format!("diff --git a/a b/b\n{header}\n"));
            assert_eq!(p.shape, shape, "{header}");
            assert!(p.hunks.is_empty());
        }
        let b = parse("diff --git a/i.png b/i.png\nindex 1..2 100644\nBinary files a/i.png and b/i.png differ\n");
        assert!(b.binary && b.hunks.is_empty());
        let g = parse("diff --git a/i b/i\nGIT binary patch\nliteral 3\nabc\n\n");
        assert!(g.binary);
    }

    #[test]
    fn a_line_that_looks_like_a_header_does_not_end_the_hunk() {
        // the file's own lines: "+++ b/x", "--- a/x", "@@ -1 +1 @@", "diff --git a b" behind their markers
        let p = parse("diff --git a/x b/x\n@@ -1,3 +1,3 @@\n---- a/x\n-@@ -1 +1 @@\n++++ b/x\n+diff --git a/y b/y\n  keep\n");
        assert_eq!(p.hunks.len(), 1);
        let t: Vec<&str> = p.hunks[0].lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            t,
            [
                "--- a/x",
                "@@ -1 +1 @@",
                "+++ b/x",
                "diff --git a/y b/y",
                " keep"
            ]
        );
    }

    #[test]
    fn hostile_text_is_kept_as_text() {
        let p = parse("diff --git a/x b/x\n@@ -1 +1 @@\n-<script>alert(1)</script>\n+\u{1b}[31m\u{202e}evil\n");
        assert_eq!(p.hunks[0].lines[0].text, "<script>alert(1)</script>");
        assert_eq!(p.hunks[0].lines[1].text, "\u{1b}[31m\u{202e}evil");
        // invalid UTF-8 is replaced
        let mut raw = b"diff --git a/x b/x\n@@ -1 +1 @@\n+ok \xff\xfe\n".to_vec();
        raw.extend_from_slice(b"");
        let p = parse_file_patch(&raw, &mut DiffBudget::default());
        assert_eq!(p.hunks[0].lines[0].text, "ok \u{fffd}\u{fffd}");
    }

    #[test]
    fn long_lines_are_clipped_with_a_marker() {
        let long = "x".repeat(MAX_LINE_CHARS + 10);
        let p = parse(&format!("diff --git a/x b/x\n@@ -0,0 +1 @@\n+{long}\n"));
        let t = &p.hunks[0].lines[0].text;
        assert!(t.ends_with("[10 more characters]"), "{t}");
        assert!(t.chars().count() < MAX_LINE_CHARS + 40);
    }

    #[test]
    fn the_budget_cuts_files_hunks_and_the_whole_diff() {
        let body: String = (0..50).map(|i| format!("+line {i}\n")).collect();
        let patch = format!("diff --git a/x b/x\n@@ -0,0 +1,50 @@\n{body}");
        // per file
        let mut b = DiffBudget {
            lines_per_file: 10,
            ..DiffBudget::default()
        };
        let p = parse_file_patch(patch.as_bytes(), &mut b);
        assert!(p.truncated);
        assert_eq!(p.hunks[0].lines.len(), 10);
        assert_eq!(b.lines_left, MAX_DIFF_LINES_TOTAL - 10);
        // the whole diff
        let mut b = DiffBudget {
            lines_left: 7,
            ..DiffBudget::default()
        };
        let p = parse_file_patch(patch.as_bytes(), &mut b);
        assert!(p.truncated && p.hunks[0].lines.len() == 7 && b.lines_left == 0 && b.exhausted());
        let again = parse_file_patch(patch.as_bytes(), &mut b);
        assert!(again.truncated && again.hunks.is_empty());
        // text bytes
        let mut b = DiffBudget {
            text_left: 30,
            ..DiffBudget::default()
        };
        let p = parse_file_patch(patch.as_bytes(), &mut b);
        assert!(p.truncated && p.hunks[0].lines.len() < 50);
        // hunks
        let hunks: String = (0..10)
            .map(|i| format!("@@ -{n},1 +{n},1 @@\n-a\n+b\n", n = i * 10 + 1))
            .collect();
        let mut b = DiffBudget {
            hunks_per_file: 3,
            ..DiffBudget::default()
        };
        let p = parse_file_patch(format!("diff --git a/x b/x\n{hunks}").as_bytes(), &mut b);
        assert_eq!(p.hunks.len(), 3);
        assert!(p.truncated);
    }

    #[test]
    fn malformed_patches_never_panic_and_stay_bounded() {
        for bad in [
            "",
            "diff --git",
            "@@",
            "@@ -1 @@\n",
            "@@ -a,b +c,d @@\n x\n",
            "@@ -99999999999,1 +1 @@\n-x\n",
            "diff --git a b\n@@ -1,5 +1,5 @@\n+x\n",
            "diff --git a b\n@@ -1,1 +1,1 @@\n\\\n\\\n\\\n",
            "diff --git a b\n@@ -0,0 +0,0 @@\n+x\n-y\n z\n",
            "diff --git a b\n@@ -1,2 +1,2 @@\n?bad\n",
        ] {
            let p = parse(bad);
            assert!(p.hunks.iter().all(|h| h.lines.len() <= 10));
        }
        let mut seed = 0x1234_5678_9abc_def1u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let atoms: [&str; 12] = [
            "@@ -1,3 +1,3 @@\n",
            "@@ -5 +5,2 @@ x\n",
            "+a\n",
            "-b\n",
            " c\n",
            "\\ No newline at end of file\n",
            "diff --git a/x b/x\n",
            "Binary files a and b differ\n",
            "\n",
            "\u{ff}",
            "new file mode 100644\n",
            "\r\n",
        ];
        for _ in 0..2000 {
            let mut s = String::new();
            for _ in 0..(next() % 30) {
                s.push_str(atoms[usize::try_from(next()).unwrap_or(0) % atoms.len()]);
            }
            let mut b = DiffBudget::default();
            for c in split_patch(s.as_bytes()) {
                let p = parse_file_patch(c, &mut b);
                assert!(p.hunks.len() <= MAX_DIFF_HUNKS_PER_FILE);
                for h in &p.hunks {
                    assert!(h.lines.len() <= MAX_DIFF_LINES_PER_FILE);
                }
            }
        }
    }

    fn change(status: u8, old_mode: u32, new_mode: u32, old: &str, new: &str) -> FileChange {
        FileChange {
            status,
            old_mode,
            new_mode,
            old_path: old.as_bytes().to_vec(),
            new_path: new.as_bytes().to_vec(),
            similarity: None,
        }
    }

    fn ns(a: u32, d: u32) -> Option<NumStat> {
        Some(NumStat {
            additions: Some(a),
            deletions: Some(d),
        })
    }

    #[test]
    fn statuses_follow_git_and_the_numbers() {
        let m = assemble_file(
            &change(b'M', 0o100_644, 0o100_644, "a", "a"),
            ns(2, 1),
            Some(parse(SIMPLE)),
        );
        assert_eq!(m.status, FileStatus::Modified);
        assert_eq!((m.additions, m.deletions), (2, 1));
        assert!(m.path_ok && !m.old_path_ok && m.old_path.is_none());
        let a = assemble_file(&change(b'A', 0, 0o100_644, "a", "a"), ns(3, 0), None);
        assert_eq!(a.status, FileStatus::Added);
        assert_eq!((a.old_mode, a.new_mode), (None, Some(0o100_644)));
        let d = assemble_file(&change(b'D', 0o100_644, 0, "a", "a"), ns(0, 3), None);
        assert_eq!(d.status, FileStatus::Deleted);
        assert!(
            !d.path_ok,
            "a deleted file has nothing at its path at the head commit"
        );
        let t = assemble_file(
            &change(b'T', 0o100_644, 0o120_000, "a", "a"),
            ns(0, 0),
            None,
        );
        assert_eq!(t.status, FileStatus::TypeChanged);
        let mode_only = assemble_file(
            &change(b'M', 0o100_644, 0o100_755, "a", "a"),
            ns(0, 0),
            Some(parse(
                "diff --git a/a b/a\nold mode 100644\nnew mode 100755\n",
            )),
        );
        assert_eq!(mode_only.status, FileStatus::ModeChanged);
        assert!(mode_only.hunks.is_empty() && !mode_only.truncated && mode_only.note.is_none());
        let mut r = change(b'R', 0o100_644, 0o100_644, "old name", "new/name");
        r.similarity = Some(87);
        let r = assemble_file(&r, ns(1, 1), Some(parse(SIMPLE)));
        assert_eq!(r.status, FileStatus::Renamed);
        assert_eq!(r.old_path.as_deref(), Some("old name"));
        assert_eq!(r.similarity, Some(87));
        assert!(r.path_ok && r.old_path_ok);
        let mut c = change(b'C', 0o100_644, 0o100_644, "o", "n");
        c.similarity = Some(100);
        assert_eq!(assemble_file(&c, ns(0, 0), None).status, FileStatus::Copied);
    }

    #[test]
    fn binary_files_and_submodules_are_flagged() {
        let b = assemble_file(
            &change(b'M', 0o100_644, 0o100_644, "i.png", "i.png"),
            Some(NumStat {
                additions: None,
                deletions: None,
            }),
            None,
        );
        assert!(b.binary && b.hunks.is_empty() && !b.truncated);
        assert_eq!(b.note.as_deref(), Some("Binary file, not shown."));
        // binary known only from the patch
        let from_patch = assemble_file(
            &change(b'M', 0o100_644, 0o100_644, "i", "i"),
            None,
            Some(parse(
                "diff --git a/i b/i\nBinary files a/i and b/i differ\n",
            )),
        );
        assert!(from_patch.binary);
        let s = assemble_file(
            &change(b'M', 0o160_000, 0o160_000, "vendor/lib", "vendor/lib"),
            ns(1, 1),
            Some(parse("diff --git a/v b/v\n@@ -1 +1 @@\n-Subproject commit aaaa\n+Subproject commit bbbb\n")),
        );
        assert!(s.submodule && !s.binary);
        assert_eq!(s.hunks[0].lines.len(), 2);
    }

    #[test]
    fn a_file_whose_patch_was_not_read_says_so() {
        let f = assemble_file(
            &change(b'M', 0o100_644, 0o100_644, "a", "a"),
            ns(5, 5),
            None,
        );
        assert!(f.truncated && f.hunks.is_empty());
        assert!(f.note.as_deref().unwrap_or("").starts_with("Not shown"));
        let mut b = DiffBudget {
            lines_per_file: 2,
            ..DiffBudget::default()
        };
        let cut = parse_file_patch(SIMPLE.as_bytes(), &mut b);
        let f = assemble_file(
            &change(b'M', 0o100_644, 0o100_644, "f.txt", "f.txt"),
            ns(2, 1),
            Some(cut),
        );
        assert!(f.truncated);
        assert_eq!(
            (f.additions, f.deletions),
            (2, 1),
            "the totals come from numstat"
        );
        assert!(f.note.is_some());
    }

    #[test]
    fn hostile_paths_are_shown_safely_and_not_offered_to_the_viewer() {
        let evil = FileChange {
            status: b'M',
            old_mode: 0o100_644,
            new_mode: 0o100_644,
            old_path: b"a\x1b[31m\nb\xff".to_vec(),
            new_path: b"a\x1b[31m\nb\xff".to_vec(),
            similarity: None,
        };
        let f = assemble_file(&evil, ns(1, 0), None);
        assert!(!f.path.contains('\u{1b}') && !f.path.contains('\n'));
        assert!(f.path.contains("\\u{1b}") && f.path.contains("\\xff"));
        assert!(!f.path_ok);
        for bad in ["../x", "/abs", "a//b", ".git/config", "a\\b", ""] {
            let c = change(b'M', 0o100_644, 0o100_644, bad, bad);
            assert!(!assemble_file(&c, ns(1, 0), None).path_ok, "{bad:?}");
        }
    }

    #[test]
    fn stats_add_up() {
        let files = vec![
            assemble_file(
                &change(b'M', 0o100_644, 0o100_644, "a", "a"),
                ns(2, 1),
                None,
            ),
            assemble_file(&change(b'A', 0, 0o100_644, "b", "b"), ns(4, 0), None),
            assemble_file(
                &change(b'M', 0o100_644, 0o100_644, "c.png", "c.png"),
                Some(NumStat {
                    additions: None,
                    deletions: None,
                }),
                None,
            ),
        ];
        let set = DiffSet {
            base: Sha::parse(&"a".repeat(40)).unwrap(),
            head: Sha::parse(&"b".repeat(40)).unwrap(),
            context: 3,
            files,
            truncated: false,
            notice: None,
        };
        assert_eq!(
            set.stats(),
            DiffStats {
                files: 3,
                additions: 6,
                deletions: 1,
                binary: 1
            }
        );
        for s in [
            FileStatus::Added,
            FileStatus::Modified,
            FileStatus::Deleted,
            FileStatus::Renamed,
            FileStatus::Copied,
            FileStatus::ModeChanged,
            FileStatus::TypeChanged,
        ] {
            assert!(!s.as_str().is_empty());
        }
        assert_eq!(LineKind::Context.as_str(), "ctx");
        assert_eq!(LineKind::Add.as_str(), "add");
        assert_eq!(LineKind::Del.as_str(), "del");
        assert_eq!(FILE_LIMIT, MAX_DIFF_FILES);
    }
}
