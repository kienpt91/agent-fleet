//! A safe subset of Markdown, parsed into a closed tree.
//!
//! A document in a repository is hostile text. This module never produces HTML and never
//! interprets any: `<b>`, `<script>` and `<img onerror=…>` are literal text. The result is a
//! closed tree ([`Block`], [`Inline`]) with no raw HTML node and no "unknown" node, which
//! [`MdDoc::to_value`] turns into the JSON document the workbench page builds DOM nodes from.
//!
//! What is supported: ATX and setext headings, paragraphs, emphasis, strong, strikethrough, code
//! spans, fenced and indented code blocks, bullet, ordered and task lists, block quotes, GFM pipe
//! tables, thematic breaks, hard breaks, backslash escapes, inline links, angle autolinks and
//! images. There are no reference-style links (`[a][b]` is text) and no entities (`&amp;` is
//! text).
//!
//! Links are policy, not syntax: a target with a scheme is a link only for `http` and `https`
//! (and only when it is a clean URL); every other scheme, a protocol-relative `//host`, a
//! `#fragment`, and anything that percent-decodes to a scheme is *not a link* (its text is shown
//! as plain content). A relative target is resolved against the document's directory with
//! [`RepoPath::resolve_link`] and becomes a link to a repository path, or plain content when it
//! does not resolve. Images are never loaded from anywhere but the repository: only a relative
//! target that resolves to a path carries a `path`; every other image carries a note.
//!
//! Everything is bounded and linear: the source is cut at [`MAX_MARKDOWN_BYTES`], blocks and
//! inlines nest at most 16 deep (deeper content is flattened to plain text), a document has at
//! most [`MAX_NODES`] nodes (then it stops and says it was truncated), a paragraph tracks at most
//! 4000 delimiters and brackets (the rest are literal text), a table has at most 32 columns and
//! 500 rows, and every forward scan of the inline parser draws on a budget proportional to the
//! size of the paragraph.

use super::limits::MAX_MARKDOWN_BYTES;
use super::path::RepoPath;
use crate::ids::is_invisible;
use crate::value::Value;
use std::collections::HashMap;

/// Most nodes (blocks, list items, table cells, inlines) of one document.
pub const MAX_NODES: usize = 20_000;
/// Deepest nesting of containers (quotes and list items), and of inline wrappers.
pub const MAX_DEPTH: usize = 16;
/// Most delimiter runs and brackets one paragraph tracks; later ones are literal text.
const MAX_DELIMS: usize = 4_000;
/// Most emphasis wrappers one paragraph builds.
const MAX_WRAPS: usize = 2_000;
const MAX_TABLE_COLS: usize = 32;
const MAX_TABLE_ROWS: usize = 500;
const MAX_URL_BYTES: usize = 2048;
const MAX_DEST_SCAN: usize = 4096;
const MAX_TITLE_BYTES: usize = 1024;
const MAX_ALT_CHARS: usize = 200;
const MAX_SRC_CHARS: usize = 200;
const MAX_LANG_CHARS: usize = 32;

const NOTE_REMOTE: &str = "remote images are not loaded";
const NOTE_EMBEDDED: &str = "images embedded in the document are not loaded";
const NOTE_UNUSABLE: &str = "image target not usable";

/// A parsed document.
#[derive(Clone, Debug, PartialEq)]
pub struct MdDoc {
    pub blocks: Vec<Block>,
    /// The source was cut, or a cap (nodes, table rows or columns) left something out.
    pub truncated: bool,
}

/// Column alignment of a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    None,
    Left,
    Center,
    Right,
}

/// One item of a list. `checked` is `Some` for a task item (`[ ]`, `[x]`).
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub checked: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Heading {
        level: u8,
        content: Vec<Inline>,
    },
    Paragraph(Vec<Inline>),
    Code {
        lang: Option<String>,
        text: String,
    },
    Quote(Vec<Block>),
    BulletList(Vec<Item>),
    OrderedList {
        start: u32,
        items: Vec<Item>,
    },
    Table {
        align: Vec<Align>,
        head: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
    Rule,
}

/// Where a link points: a clean http(s) URL, or a path inside the repository.
#[derive(Clone, Debug, PartialEq)]
pub enum LinkTarget {
    External(String),
    Repo(RepoPath),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Inline {
    Text(String),
    Emphasis(Vec<Inline>),
    Strong(Vec<Inline>),
    Strike(Vec<Inline>),
    Code(String),
    Break,
    Link {
        content: Vec<Inline>,
        target: LinkTarget,
        title: Option<String>,
    },
    Image {
        alt: String,
        path: Option<RepoPath>,
        src: String,
        note: Option<&'static str>,
    },
}

/// Parses `source`; `dir` is the directory of the document, the base of relative links.
pub fn parse(source: &str, dir: &RepoPath) -> MdDoc {
    let mut cut = source.len().min(MAX_MARKDOWN_BYTES);
    let truncated = source.len() > MAX_MARKDOWN_BYTES;
    while !source.is_char_boundary(cut) {
        cut -= 1;
    }
    let src = source.get(..cut).unwrap_or("");
    let lines: Vec<Line> = src
        .split('\n')
        .map(|l| Line::new(l.strip_suffix('\r').unwrap_or(l)))
        .collect();
    let mut p = Parser {
        dir,
        budget: MAX_NODES,
        truncated,
        stop: false,
    };
    let blocks = p.blocks(&lines, 0);
    MdDoc {
        blocks,
        truncated: p.truncated,
    }
}

impl MdDoc {
    /// The `blocks` array of the `fleet.repo.markdown/1` document (and nothing else).
    pub fn to_value(&self) -> Value {
        Value::array(self.blocks.iter().map(Block::to_value))
    }
}

fn inlines_value(v: &[Inline]) -> Value {
    Value::array(v.iter().map(Inline::to_value))
}

fn opt_str(s: &Option<String>) -> Value {
    match s {
        Some(s) => Value::str(s.as_str()),
        None => Value::Null,
    }
}

impl Align {
    fn to_value(self) -> Value {
        match self {
            Align::None => Value::Null,
            Align::Left => Value::str("left"),
            Align::Center => Value::str("center"),
            Align::Right => Value::str("right"),
        }
    }
}

impl Item {
    fn to_value(&self) -> Value {
        Value::object([
            (
                "checked",
                match self.checked {
                    Some(b) => Value::Bool(b),
                    None => Value::Null,
                },
            ),
            ("c", Value::array(self.blocks.iter().map(Block::to_value))),
        ])
    }
}

impl Block {
    fn to_value(&self) -> Value {
        match self {
            Block::Heading { level, content } => Value::object([
                ("t", Value::str("h")),
                ("level", Value::int(*level)),
                ("c", inlines_value(content)),
            ]),
            Block::Paragraph(c) => Value::object([("t", Value::str("p")), ("c", inlines_value(c))]),
            Block::Code { lang, text } => Value::object([
                ("t", Value::str("code")),
                ("lang", opt_str(lang)),
                ("text", Value::str(text.as_str())),
            ]),
            Block::Quote(c) => Value::object([
                ("t", Value::str("quote")),
                ("c", Value::array(c.iter().map(Block::to_value))),
            ]),
            Block::BulletList(items) => Value::object([
                ("t", Value::str("ul")),
                ("c", Value::array(items.iter().map(Item::to_value))),
            ]),
            Block::OrderedList { start, items } => Value::object([
                ("t", Value::str("ol")),
                ("start", Value::int(*start)),
                ("c", Value::array(items.iter().map(Item::to_value))),
            ]),
            Block::Table { align, head, rows } => Value::object([
                ("t", Value::str("table")),
                ("align", Value::array(align.iter().map(|a| a.to_value()))),
                ("head", Value::array(head.iter().map(|c| inlines_value(c)))),
                (
                    "rows",
                    Value::array(
                        rows.iter()
                            .map(|r| Value::array(r.iter().map(|c| inlines_value(c)))),
                    ),
                ),
            ]),
            Block::Rule => Value::object([("t", Value::str("hr"))]),
        }
    }
}

impl Inline {
    fn to_value(&self) -> Value {
        match self {
            Inline::Text(s) => {
                Value::object([("t", Value::str("text")), ("s", Value::str(s.as_str()))])
            }
            Inline::Emphasis(c) => {
                Value::object([("t", Value::str("em")), ("c", inlines_value(c))])
            }
            Inline::Strong(c) => {
                Value::object([("t", Value::str("strong")), ("c", inlines_value(c))])
            }
            Inline::Strike(c) => Value::object([("t", Value::str("del")), ("c", inlines_value(c))]),
            Inline::Code(s) => {
                Value::object([("t", Value::str("code")), ("s", Value::str(s.as_str()))])
            }
            Inline::Break => Value::object([("t", Value::str("br"))]),
            Inline::Link {
                content,
                target,
                title,
            } => {
                let (kind, url, path) = match target {
                    LinkTarget::External(u) => ("external", Value::str(u.as_str()), Value::Null),
                    LinkTarget::Repo(p) => ("repo", Value::Null, Value::str(p.as_str())),
                };
                Value::object([
                    ("t", Value::str("link")),
                    ("c", inlines_value(content)),
                    ("kind", Value::str(kind)),
                    ("url", url),
                    ("path", path),
                    ("title", opt_str(title)),
                ])
            }
            Inline::Image {
                alt,
                path,
                src,
                note,
            } => Value::object([
                ("t", Value::str("image")),
                ("alt", Value::str(alt.as_str())),
                (
                    "path",
                    match path {
                        Some(p) => Value::str(p.as_str()),
                        None => Value::Null,
                    },
                ),
                ("src", Value::str(src.as_str())),
                (
                    "note",
                    match note {
                        Some(n) => Value::str(*n),
                        None => Value::Null,
                    },
                ),
            ]),
        }
    }

    /// Nodes in this subtree, itself included.
    fn count(&self) -> usize {
        1 + match self {
            Inline::Emphasis(c) | Inline::Strong(c) | Inline::Strike(c) => {
                c.iter().map(Inline::count).sum()
            }
            Inline::Link { content, .. } => content.iter().map(Inline::count).sum(),
            _ => 0,
        }
    }
}

/// The plain text of an inline (breaks are spaces, images are their alt text), stopping once
/// `out` holds `max` bytes.
fn plain_into(inl: &Inline, out: &mut String, max: usize) {
    if out.len() >= max {
        return;
    }
    match inl {
        Inline::Text(s) | Inline::Code(s) => out.push_str(s),
        Inline::Break => out.push(' '),
        Inline::Emphasis(c) | Inline::Strong(c) | Inline::Strike(c) => {
            for k in c {
                plain_into(k, out, max);
            }
        }
        Inline::Link { content, .. } => {
            for k in content {
                plain_into(k, out, max);
            }
        }
        Inline::Image { alt, .. } => out.push_str(alt),
    }
}

// ───────────────────────────── small helpers ─────────────────────────────

/// `b[i]`, or 0 past the end (a NUL byte never matches anything the parser looks for).
fn at(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

/// `s[a..b]`, or "" when it is not a valid range on char boundaries.
fn sl(s: &str, a: usize, b: usize) -> &str {
    s.get(a..b).unwrap_or("")
}

fn trim_ws(s: &str) -> &str {
    s.trim_matches([' ', '\t'])
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while matches!(at(b, i), b' ' | b'\t' | b'\n') {
        i += 1;
    }
    i
}

fn clip_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn is_ws_char(c: char) -> bool {
    c.is_whitespace()
}

fn is_punct_char(c: char) -> bool {
    !c.is_alphanumeric() && !c.is_whitespace() && !c.is_control()
}

// ───────────────────────────── lines ─────────────────────────────

/// One line as seen from inside some containers: the text left after the container prefixes
/// were consumed. `col` is the column of the first byte of `s` in the original line (it matters
/// for tabs) and `pad` is how many columns of a partly consumed tab still count as spaces in
/// front of `s`.
#[derive(Clone, Copy)]
struct Line<'a> {
    s: &'a str,
    col: usize,
    pad: usize,
}

impl<'a> Line<'a> {
    fn new(s: &'a str) -> Self {
        Line { s, col: 0, pad: 0 }
    }

    fn empty() -> Self {
        Line::new("")
    }

    /// Columns of leading white space (the `pad` included) and how many bytes of `s` they use.
    fn leading(&self) -> (usize, usize) {
        let mut width = self.pad;
        let mut bytes = 0;
        let mut col = self.col;
        for b in self.s.bytes() {
            match b {
                b' ' => {
                    width += 1;
                    col += 1;
                }
                b'\t' => {
                    let k = 4 - col % 4;
                    width += k;
                    col += k;
                }
                _ => break,
            }
            bytes += 1;
        }
        (width, bytes)
    }

    fn is_blank(&self) -> bool {
        self.s.bytes().all(|b| b == b' ' || b == b'\t')
    }

    /// The indentation in columns and the line without it.
    fn rest(&self) -> (usize, Line<'a>) {
        let (width, bytes) = self.leading();
        (
            width,
            Line {
                s: self.s.get(bytes..).unwrap_or(""),
                col: self.col + (width - self.pad),
                pad: 0,
            },
        )
    }

    /// Removes up to `n` columns of leading white space (a tab that is only partly covered
    /// leaves the remainder as padding).
    fn skip_cols(self, mut n: usize) -> Line<'a> {
        let mut pad = self.pad;
        let mut col = self.col;
        let mut s = self.s;
        let take = n.min(pad);
        pad -= take;
        n -= take;
        while n > 0 {
            match s.as_bytes().first() {
                Some(b' ') => {
                    s = s.get(1..).unwrap_or("");
                    col += 1;
                    n -= 1;
                }
                Some(b'\t') => {
                    let k = 4 - col % 4;
                    s = s.get(1..).unwrap_or("");
                    col += k;
                    if k > n {
                        pad += k - n;
                        n = 0;
                    } else {
                        n -= k;
                    }
                }
                _ => break,
            }
        }
        Line { s, col, pad }
    }

    /// Removes `k` bytes of ASCII, non-white-space text (a marker) at the start of the line.
    fn skip_bytes(self, k: usize) -> Line<'a> {
        Line {
            s: self.s.get(k..).unwrap_or(""),
            col: self.col + k,
            pad: 0,
        }
    }

    /// The text of the line, with the padding of a partly consumed tab as spaces.
    fn text(&self) -> String {
        let mut t = " ".repeat(self.pad);
        t.push_str(self.s);
        t
    }
}

struct Fence<'a> {
    ch: u8,
    len: usize,
    indent: usize,
    info: &'a str,
}

fn fence_open(line: Line<'_>) -> Option<Fence<'_>> {
    let (indent, r) = line.rest();
    if indent >= 4 {
        return None;
    }
    let b = r.s.as_bytes();
    let ch = *b.first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let len = b.iter().take_while(|&&c| c == ch).count();
    if len < 3 {
        return None;
    }
    let info = trim_ws(r.s.get(len..)?);
    if ch == b'`' && info.contains('`') {
        return None;
    }
    Some(Fence {
        ch,
        len,
        indent,
        info,
    })
}

fn fence_close(line: Line<'_>, ch: u8, len: usize) -> bool {
    let (indent, r) = line.rest();
    if indent >= 4 {
        return false;
    }
    let run = r.s.bytes().take_while(|&c| c == ch).count();
    run >= len && r.s.get(run..).is_some_and(|t| trim_ws(t).is_empty())
}

fn lang_of(info: &str) -> Option<String> {
    let n = info.chars().count();
    let ok = (1..=MAX_LANG_CHARS).contains(&n)
        && info
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '.' | '#' | '-'));
    ok.then(|| info.to_string())
}

/// `# heading`: level and content (closing hashes removed).
fn atx(line: Line<'_>) -> Option<(u8, &str)> {
    let (indent, r) = line.rest();
    if indent >= 4 {
        return None;
    }
    let hashes = r.s.bytes().take(7).take_while(|&c| c == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let after = r.s.get(hashes..)?;
    if !after.is_empty() && !after.starts_with([' ', '\t']) {
        return None;
    }
    let content = trim_ws(after);
    let stripped = content.trim_end_matches('#');
    let content = if stripped.len() == content.len() {
        content
    } else if stripped.is_empty() {
        ""
    } else if stripped.ends_with([' ', '\t']) {
        trim_ws(stripped)
    } else {
        content
    };
    Some((hashes as u8, content))
}

fn is_hr(line: Line<'_>) -> bool {
    let (indent, r) = line.rest();
    if indent >= 4 {
        return false;
    }
    let Some(&c) = r.s.as_bytes().first() else {
        return false;
    };
    if !matches!(c, b'-' | b'*' | b'_') {
        return false;
    }
    let mut n = 0usize;
    for b in r.s.bytes() {
        if b == c {
            n += 1;
        } else if b != b' ' && b != b'\t' {
            return false;
        }
    }
    n >= 3
}

/// `===` (1) or `---` (2) under a paragraph.
fn setext_level(line: Line<'_>) -> Option<u8> {
    let (indent, r) = line.rest();
    if indent >= 4 {
        return None;
    }
    let s = r.s.trim_end_matches([' ', '\t']);
    let c = *s.as_bytes().first()?;
    if !s.bytes().all(|b| b == c) {
        return None;
    }
    match c {
        b'=' => Some(1),
        b'-' => Some(2),
        _ => None,
    }
}

struct Marker<'a> {
    indent: usize,
    /// The bullet character, or 0 for an ordered marker.
    bullet: u8,
    /// `.` or `)` for an ordered marker, else 0.
    delim: u8,
    start: u32,
    len: usize,
    after: Line<'a>,
    empty: bool,
}

fn list_marker(line: Line<'_>) -> Option<Marker<'_>> {
    let (indent, r) = line.rest();
    if indent >= 4 {
        return None;
    }
    let b = r.s.as_bytes();
    let first = *b.first()?;
    let (bullet, delim, start, len) = if matches!(first, b'-' | b'*' | b'+') {
        (first, 0, 1, 1)
    } else if first.is_ascii_digit() {
        let digits = b.iter().take(10).take_while(|c| c.is_ascii_digit()).count();
        if digits > 9 {
            return None;
        }
        let d = at(b, digits);
        if d != b'.' && d != b')' {
            return None;
        }
        let n = r.s.get(..digits)?.parse::<u32>().unwrap_or(1);
        (0, d, n, digits + 1)
    } else {
        return None;
    };
    if !matches!(b.get(len), None | Some(b' ') | Some(b'\t')) {
        return None;
    }
    let after = r.skip_bytes(len);
    Some(Marker {
        indent,
        bullet,
        delim,
        start,
        len,
        empty: after.is_blank(),
        after,
    })
}

fn same_kind(a: &Marker<'_>, b: &Marker<'_>) -> bool {
    a.bullet == b.bullet && a.delim == b.delim
}

/// Whether the line starts a block of its own (so it is never a lazy continuation).
fn starts_block(line: Line<'_>) -> bool {
    let (indent, r) = line.rest();
    if indent >= 4 {
        return false;
    }
    r.s.starts_with('>')
        || fence_open(line).is_some()
        || atx(line).is_some()
        || is_hr(line)
        || list_marker(line).is_some()
}

/// Whether the line ends a paragraph it follows.
fn interrupts(line: Line<'_>) -> bool {
    let (indent, r) = line.rest();
    if indent >= 4 {
        return false;
    }
    if r.s.starts_with('>') || fence_open(line).is_some() || atx(line).is_some() || is_hr(line) {
        return true;
    }
    match list_marker(line) {
        Some(m) => !m.empty && (m.bullet != 0 || m.start == 1),
        None => false,
    }
}

/// Tracks whether the last line of a container can be continued by a line that lacks the
/// container's prefix (a lazy continuation of a paragraph).
#[derive(Default)]
struct Lazy {
    fence: Option<(u8, usize)>,
    para: bool,
}

impl Lazy {
    fn feed(&mut self, l: Line<'_>) {
        if let Some((ch, len)) = self.fence {
            if fence_close(l, ch, len) {
                self.fence = None;
            }
            self.para = false;
            return;
        }
        if let Some(f) = fence_open(l) {
            self.fence = Some((f.ch, f.len));
            self.para = false;
            return;
        }
        self.para = !l.is_blank() && atx(l).is_none() && !is_hr(l);
    }
}

// ───────────────────────────── tables ─────────────────────────────

/// The cells of a table row, at most `limit` of them (an escaped `\|` is a pipe in the cell).
fn split_cells(s: &str, limit: usize) -> Vec<String> {
    let s = trim_ws(s);
    let s = s.strip_prefix('|').unwrap_or(s);
    let mut cells: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let mut ended_with_pipe = false;
    while let Some(c) = chars.next() {
        ended_with_pipe = false;
        match c {
            '\\' => match chars.peek() {
                Some('|') => {
                    cur.push('|');
                    chars.next();
                }
                Some('\\') => {
                    cur.push_str("\\\\");
                    chars.next();
                }
                _ => cur.push('\\'),
            },
            '|' => {
                cells.push(trim_ws(&cur).to_string());
                cur.clear();
                ended_with_pipe = true;
                if cells.len() >= limit {
                    break;
                }
            }
            c => cur.push(c),
        }
    }
    if !ended_with_pipe {
        cells.push(trim_ws(&cur).to_string());
    }
    cells
}

/// The alignments of a delimiter row (`|:--|--:|`), at most `MAX_TABLE_COLS + 1` of them.
fn delim_row(line: Line<'_>) -> Option<Vec<Align>> {
    let (indent, r) = line.rest();
    if indent >= 4 {
        return None;
    }
    let s = trim_ws(r.s);
    if !matches!(s.as_bytes().first(), Some(b'|' | b'-' | b':')) || !s.contains('|') {
        return None;
    }
    let s = s.strip_prefix('|').unwrap_or(s);
    let s = s.strip_suffix('|').unwrap_or(s);
    let mut out = Vec::new();
    for cell in s.split('|') {
        let c = trim_ws(cell);
        let left = c.starts_with(':');
        let c = c.strip_prefix(':').unwrap_or(c);
        let right = c.ends_with(':');
        let c = c.strip_suffix(':').unwrap_or(c);
        if c.is_empty() || !c.bytes().all(|b| b == b'-') {
            return None;
        }
        out.push(match (left, right) {
            (true, true) => Align::Center,
            (true, false) => Align::Left,
            (false, true) => Align::Right,
            (false, false) => Align::None,
        });
        if out.len() > MAX_TABLE_COLS {
            break;
        }
    }
    Some(out)
}

/// A table header at line `i` (header row, then a delimiter row with the same cell count).
fn table_head(lines: &[Line<'_>], i: usize) -> Option<(Vec<String>, Vec<Align>)> {
    let l0 = lines.get(i)?;
    let l1 = lines.get(i + 1)?;
    let (indent, r0) = l0.rest();
    if indent >= 4 {
        return None;
    }
    let align = delim_row(*l1)?;
    let head = split_cells(r0.s, MAX_TABLE_COLS + 1);
    (head.len() == align.len()).then_some((head, align))
}

// ───────────────────────────── block parser ─────────────────────────────

struct Parser<'d> {
    dir: &'d RepoPath,
    budget: usize,
    truncated: bool,
    stop: bool,
}

impl Parser<'_> {
    /// Spends `n` nodes; when they are gone, everything stops and the document is truncated.
    fn charge(&mut self, n: usize) -> bool {
        if self.budget >= n {
            self.budget -= n;
            true
        } else {
            self.stop = true;
            self.truncated = true;
            false
        }
    }

    /// Parses the inline content of one paragraph, heading or table cell and charges its nodes
    /// (the ones that do not fit are dropped).
    fn inline(&mut self, text: &str) -> Vec<Inline> {
        let v = Inl::new(self.dir, text).run();
        let mut out = Vec::with_capacity(v.len());
        for inl in v {
            if self.charge(inl.count()) {
                out.push(inl);
            } else {
                break;
            }
        }
        out
    }

    fn blocks(&mut self, lines: &[Line<'_>], depth: usize) -> Vec<Block> {
        let mut out = Vec::new();
        if depth >= MAX_DEPTH {
            self.flatten(lines, &mut out);
            return out;
        }
        let n = lines.len();
        let mut i = 0;
        while i < n && !self.stop {
            let Some(&line) = lines.get(i) else { break };
            if line.is_blank() {
                i += 1;
                continue;
            }
            let (indent, rest) = line.rest();
            if indent >= 4 {
                i = self.indented_code(lines, i, &mut out);
            } else if let Some(f) = fence_open(line) {
                i = self.fenced(lines, i, &f, &mut out);
            } else if let Some((level, content)) = atx(line) {
                if !self.charge(1) {
                    break;
                }
                let content = self.inline(content);
                out.push(Block::Heading { level, content });
                i += 1;
            } else if is_hr(line) {
                if !self.charge(1) {
                    break;
                }
                out.push(Block::Rule);
                i += 1;
            } else if rest.s.starts_with('>') {
                i = self.quote(lines, i, depth, &mut out);
            } else if let Some(m) = list_marker(line) {
                i = self.list(lines, i, m, depth, &mut out);
            } else {
                i = self.paragraph(lines, i, &mut out);
            }
        }
        out
    }

    /// Content nested too deeply: its text, as one paragraph.
    fn flatten(&mut self, lines: &[Line<'_>], out: &mut Vec<Block>) {
        let mut s = String::new();
        for l in lines {
            let t = l.rest().1.s.trim_end_matches([' ', '\t']);
            if t.is_empty() {
                continue;
            }
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(t);
        }
        if !s.is_empty() && self.charge(2) {
            out.push(Block::Paragraph(vec![Inline::Text(s)]));
        }
    }

    fn indented_code(&mut self, lines: &[Line<'_>], i: usize, out: &mut Vec<Block>) -> usize {
        let mut j = i;
        let mut last = i;
        while let Some(l) = lines.get(j) {
            if l.is_blank() {
                j += 1;
            } else if l.leading().0 >= 4 {
                last = j;
                j += 1;
            } else {
                break;
            }
        }
        if !self.charge(1) {
            return lines.len();
        }
        let mut text = String::new();
        for (k, l) in lines.get(i..=last).unwrap_or(&[]).iter().enumerate() {
            if k > 0 {
                text.push('\n');
            }
            text.push_str(&l.skip_cols(4).text());
        }
        out.push(Block::Code { lang: None, text });
        last + 1
    }

    fn fenced(
        &mut self,
        lines: &[Line<'_>],
        i: usize,
        f: &Fence<'_>,
        out: &mut Vec<Block>,
    ) -> usize {
        if !self.charge(1) {
            return lines.len();
        }
        let mut text = String::new();
        let mut j = i + 1;
        let mut closed = false;
        while let Some(&l) = lines.get(j) {
            if fence_close(l, f.ch, f.len) {
                closed = true;
                break;
            }
            if j > i + 1 {
                text.push('\n');
            }
            let strip = f.indent.min(l.leading().0);
            text.push_str(&l.skip_cols(strip).text());
            j += 1;
        }
        out.push(Block::Code {
            lang: lang_of(f.info),
            text,
        });
        if closed {
            j + 1
        } else {
            j
        }
    }

    fn quote(&mut self, lines: &[Line<'_>], i: usize, depth: usize, out: &mut Vec<Block>) -> usize {
        if !self.charge(1) {
            return lines.len();
        }
        let mut inner: Vec<Line> = Vec::new();
        let mut lazy = Lazy::default();
        let mut j = i;
        while let Some(&l) = lines.get(j) {
            let (indent, r) = l.rest();
            if indent < 4 && r.s.starts_with('>') {
                let mut il = r.skip_bytes(1);
                if matches!(il.s.as_bytes().first(), Some(b' ' | b'\t')) {
                    il = il.skip_cols(1);
                }
                lazy.feed(il);
                inner.push(il);
            } else if lazy.para && !l.is_blank() && !starts_block(l) {
                lazy.feed(l);
                inner.push(l);
            } else {
                break;
            }
            j += 1;
        }
        let kids = self.blocks(&inner, depth + 1);
        out.push(Block::Quote(kids));
        j
    }

    fn list(
        &mut self,
        lines: &[Line<'_>],
        i: usize,
        first: Marker<'_>,
        depth: usize,
        out: &mut Vec<Block>,
    ) -> usize {
        if !self.charge(1) {
            return lines.len();
        }
        let mut items: Vec<Item> = Vec::new();
        let mut i = i;
        let mut cur = Marker { ..first };
        let (ordered, start) = (first.bullet == 0, first.start);
        loop {
            // The column the item's content starts at, and its first line.
            let m_width = cur.indent + cur.len;
            let (sp_width, _) = cur.after.leading();
            let (off, mut first_line) = if cur.empty {
                (m_width + 1, Line::empty())
            } else if sp_width >= 5 {
                (m_width + 1, cur.after.skip_cols(1))
            } else {
                (m_width + sp_width, cur.after.skip_cols(sp_width))
            };
            let mut checked = None;
            if first_line.pad == 0 {
                let b = first_line.s.as_bytes();
                if at(b, 0) == b'['
                    && matches!(at(b, 1), b' ' | b'x' | b'X')
                    && at(b, 2) == b']'
                    && matches!(at(b, 3), b' ' | b'\t')
                {
                    checked = Some(at(b, 1) != b' ');
                    first_line = first_line.skip_bytes(3);
                }
            }
            let starts_blank = first_line.is_blank();
            let mut lazy = Lazy::default();
            lazy.feed(first_line);
            let mut item_lines = vec![first_line];
            let mut j = i + 1;
            while let Some(&l) = lines.get(j) {
                if l.is_blank() {
                    if starts_blank && item_lines.len() == 1 {
                        break;
                    }
                    let mut k = j;
                    while lines.get(k).is_some_and(|x| x.is_blank()) {
                        k += 1;
                    }
                    let Some(&next) = lines.get(k) else { break };
                    if next.leading().0 < off {
                        break;
                    }
                    for _ in j..k {
                        item_lines.push(Line::empty());
                    }
                    lazy.feed(Line::empty());
                    j = k;
                } else if l.leading().0 >= off {
                    let il = l.skip_cols(off);
                    lazy.feed(il);
                    item_lines.push(il);
                    j += 1;
                } else if lazy.para && !starts_block(l) {
                    lazy.feed(l);
                    item_lines.push(l);
                    j += 1;
                } else {
                    break;
                }
            }
            if !self.charge(1) {
                return lines.len();
            }
            let blocks = self.blocks(&item_lines, depth + 1);
            items.push(Item { checked, blocks });
            i = j;
            if self.stop {
                break;
            }
            // The next item: a marker of the same kind after any blank lines.
            let mut k = i;
            while lines.get(k).is_some_and(|x| x.is_blank()) {
                k += 1;
            }
            let Some(&next) = lines.get(k) else { break };
            if is_hr(next) {
                break;
            }
            match list_marker(next) {
                Some(m) if same_kind(&m, &first) => {
                    cur = m;
                    i = k;
                }
                _ => break,
            }
        }
        out.push(if ordered {
            Block::OrderedList { start, items }
        } else {
            Block::BulletList(items)
        });
        i
    }

    fn paragraph(&mut self, lines: &[Line<'_>], i: usize, out: &mut Vec<Block>) -> usize {
        if let Some((head, align)) = table_head(lines, i) {
            return self.table(lines, i, head, align, out);
        }
        let mut j = i + 1;
        let mut setext = None;
        while let Some(&l) = lines.get(j) {
            if l.is_blank() {
                break;
            }
            if let Some(level) = setext_level(l) {
                setext = Some(level);
                break;
            }
            if interrupts(l) || table_head(lines, j).is_some() {
                break;
            }
            j += 1;
        }
        let text = para_text(lines.get(i..j).unwrap_or(&[]));
        if !self.charge(1) {
            return lines.len();
        }
        let content = self.inline(&text);
        match setext {
            Some(level) => {
                out.push(Block::Heading { level, content });
                j + 1
            }
            None => {
                out.push(Block::Paragraph(content));
                j
            }
        }
    }

    fn table(
        &mut self,
        lines: &[Line<'_>],
        i: usize,
        mut head: Vec<String>,
        mut align: Vec<Align>,
        out: &mut Vec<Block>,
    ) -> usize {
        if !self.charge(1) {
            return lines.len();
        }
        if align.len() > MAX_TABLE_COLS {
            self.truncated = true;
        }
        align.truncate(MAX_TABLE_COLS);
        head.truncate(MAX_TABLE_COLS);
        let cols = align.len();
        let mut head_cells = Vec::with_capacity(cols);
        for c in &head {
            head_cells.push(self.cell(c));
        }
        let mut rows: Vec<Vec<Vec<Inline>>> = Vec::new();
        let mut j = i + 2;
        while let Some(&l) = lines.get(j) {
            if l.is_blank() || interrupts(l) {
                break;
            }
            if rows.len() >= MAX_TABLE_ROWS {
                self.truncated = true;
            } else if !self.stop {
                let mut cells = split_cells(l.rest().1.s, cols + 1);
                cells.truncate(cols);
                cells.resize(cols, String::new());
                let mut row = Vec::with_capacity(cols);
                for c in &cells {
                    row.push(self.cell(c));
                }
                rows.push(row);
            }
            j += 1;
        }
        out.push(Block::Table {
            align,
            head: head_cells,
            rows,
        });
        j
    }

    fn cell(&mut self, text: &str) -> Vec<Inline> {
        if self.charge(1) {
            self.inline(text)
        } else {
            Vec::new()
        }
    }
}

/// The text of a paragraph: its lines, joined with `\n`, indentation removed.
fn para_text(lines: &[Line<'_>]) -> String {
    let mut out = String::new();
    let last = lines.len().saturating_sub(1);
    for (k, l) in lines.iter().enumerate() {
        if k > 0 {
            out.push('\n');
        }
        let s = l.rest().1.s;
        out.push_str(if k == last {
            s.trim_end_matches([' ', '\t'])
        } else {
            s
        });
    }
    out
}

// ───────────────────────────── link policy ─────────────────────────────

/// `scheme` of `scheme:rest` (`^[A-Za-z][A-Za-z0-9+.-]*:`).
fn scheme_of(t: &str) -> Option<&str> {
    let b = t.as_bytes();
    if !at(b, 0).is_ascii_alphabetic() {
        return None;
    }
    let mut i = 1;
    loop {
        let c = at(b, i);
        if c.is_ascii_alphanumeric() || matches!(c, b'+' | b'.' | b'-') {
            i += 1;
        } else {
            break;
        }
    }
    if at(b, i) == b':' {
        t.get(..i)
    } else {
        None
    }
}

/// Whether the target looks like it has a scheme once its percent escapes are decoded.
fn decoded_has_scheme(t: &str) -> bool {
    let b = t.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < b.len() && out.len() < 256 {
        let c = at(b, i);
        let hex = |x: u8| (x as char).to_digit(16);
        if c == b'%' {
            if let (Some(h), Some(l)) = (hex(at(b, i + 1)), hex(at(b, i + 2))) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    scheme_of(&String::from_utf8_lossy(&out)).is_some()
}

/// A clean http(s) URL: bounded, no control, white space, backslash, invisible or bidi
/// character, with `//` and a non-empty host after the scheme.
fn url_ok(u: &str) -> bool {
    if u.len() > MAX_URL_BYTES {
        return false;
    }
    if u.chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\' || is_invisible(c))
    {
        return false;
    }
    let Some(scheme) = scheme_of(u) else {
        return false;
    };
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) {
        return false;
    }
    let Some(rest) = u.get(scheme.len() + 1..).and_then(|r| r.strip_prefix("//")) else {
        return false;
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = sl(rest, 0, end);
    let host = authority.rsplit('@').next().unwrap_or("");
    !host.is_empty() && !host.starts_with(':')
}

/// What a link target is, or `None` when it is not a link.
fn classify_link(dest: &str, dir: &RepoPath) -> Option<LinkTarget> {
    let t = dest.trim();
    if t.is_empty() {
        return None;
    }
    if scheme_of(t).is_some() {
        return url_ok(t).then(|| LinkTarget::External(t.to_string()));
    }
    if t.starts_with("//") || t.starts_with('#') || decoded_has_scheme(t) {
        return None;
    }
    dir.resolve_link(t).map(LinkTarget::Repo)
}

/// Path, source text and note of an image target.
fn classify_image(dest: &str, dir: &RepoPath) -> (Option<RepoPath>, String, Option<&'static str>) {
    let t = dest.trim();
    let src = clip_chars(t, MAX_SRC_CHARS);
    if t.is_empty() {
        return (None, src, Some(NOTE_UNUSABLE));
    }
    if let Some(scheme) = scheme_of(t) {
        let note = if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
            NOTE_REMOTE
        } else if scheme.eq_ignore_ascii_case("data") {
            NOTE_EMBEDDED
        } else {
            NOTE_UNUSABLE
        };
        return (None, src, Some(note));
    }
    if t.starts_with("//") || t.starts_with('#') || decoded_has_scheme(t) {
        return (None, src, Some(NOTE_UNUSABLE));
    }
    match dir.resolve_link(t) {
        Some(p) => (Some(p), src, None),
        None => (None, src, Some(NOTE_UNUSABLE)),
    }
}

// ───────────────────────────── inline parser ─────────────────────────────

const NIL: usize = usize::MAX;

enum Kind {
    Text(String),
    /// A delimiter run that is still on the stack: character and the characters left.
    Delim(u8, usize),
    Node(Inline),
    Gone,
}

struct Slot {
    kind: Kind,
    prev: usize,
    next: usize,
    /// Nesting depth of a node (0 for text).
    depth: u8,
}

#[derive(Clone, Copy)]
struct Delim {
    node: usize,
    ch: u8,
    len: usize,
    orig: usize,
    can_open: bool,
    can_close: bool,
    prev: usize,
    next: usize,
    live: bool,
}

struct Bracket {
    node: usize,
    image: bool,
    /// The last delimiter before this bracket (emphasis inside the brackets is processed above it).
    dbot: usize,
}

struct Dest<'t> {
    target: &'t str,
    title: Option<&'t str>,
    end: usize,
}

/// The inline parser of one paragraph: a doubly linked list of tokens (so that wrapping a range
/// never shifts the rest), the delimiter stack of CommonMark's emphasis algorithm and a bracket
/// stack for links and images.
struct Inl<'d, 't> {
    dir: &'d RepoPath,
    text: &'t str,
    slots: Vec<Slot>,
    head: usize,
    tail: usize,
    delims: Vec<Delim>,
    dtail: usize,
    brackets: Vec<Bracket>,
    /// Link brackets below this stack index are inactive (a link was made after them).
    floor: usize,
    tracked: usize,
    wraps: usize,
    /// Bytes the forward scans (destinations, titles, autolinks) may still look at.
    scan: usize,
    pending: String,
    runs: Vec<(usize, usize)>,
    same: Vec<usize>,
    runs_ready: bool,
}

fn is_special(b: u8) -> bool {
    matches!(
        b,
        b'\\' | b'`' | b'*' | b'_' | b'~' | b'[' | b']' | b'!' | b'<' | b'\n'
    )
}

fn flank(c: u8, prev: Option<char>, next: Option<char>) -> (bool, bool) {
    let pw = prev.is_none_or(is_ws_char);
    let nw = next.is_none_or(is_ws_char);
    let pp = prev.is_some_and(is_punct_char);
    let np = next.is_some_and(is_punct_char);
    let left = !nw && (!np || pw || pp);
    let right = !pw && (!pp || nw || np);
    if c == b'_' {
        (left && (!right || pp), right && (!left || np))
    } else {
        (left, right)
    }
}

fn code_content(raw: &str) -> String {
    let s: String = raw
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect();
    if s.len() >= 2 && s.starts_with(' ') && s.ends_with(' ') && s.bytes().any(|b| b != b' ') {
        sl(&s, 1, s.len() - 1).to_string()
    } else {
        s
    }
}

impl<'d, 't> Inl<'d, 't> {
    fn new(dir: &'d RepoPath, text: &'t str) -> Self {
        Inl {
            dir,
            text,
            slots: Vec::new(),
            head: NIL,
            tail: NIL,
            delims: Vec::new(),
            dtail: NIL,
            brackets: Vec::new(),
            floor: 0,
            tracked: 0,
            wraps: 0,
            scan: text.len().saturating_mul(4).saturating_add(8192),
            pending: String::new(),
            runs: Vec::new(),
            same: Vec::new(),
            runs_ready: false,
        }
    }

    // ── token list ──

    fn next(&self, i: usize) -> usize {
        self.slots.get(i).map_or(NIL, |s| s.next)
    }

    fn prev(&self, i: usize) -> usize {
        self.slots.get(i).map_or(NIL, |s| s.prev)
    }

    fn set_next(&mut self, i: usize, v: usize) {
        if let Some(s) = self.slots.get_mut(i) {
            s.next = v;
        }
    }

    fn set_prev(&mut self, i: usize, v: usize) {
        if let Some(s) = self.slots.get_mut(i) {
            s.prev = v;
        }
    }

    fn push(&mut self, kind: Kind, depth: u8) -> usize {
        let idx = self.slots.len();
        self.slots.push(Slot {
            kind,
            prev: self.tail,
            next: NIL,
            depth,
        });
        if self.tail == NIL {
            self.head = idx;
        } else {
            self.set_next(self.tail, idx);
        }
        self.tail = idx;
        idx
    }

    fn insert_after(&mut self, a: usize, kind: Kind, depth: u8) -> usize {
        let nx = self.next(a);
        let idx = self.slots.len();
        self.slots.push(Slot {
            kind,
            prev: a,
            next: nx,
            depth,
        });
        self.set_next(a, idx);
        if nx == NIL {
            self.tail = idx;
        } else {
            self.set_prev(nx, idx);
        }
        idx
    }

    fn unlink(&mut self, i: usize) {
        let (p, n) = (self.prev(i), self.next(i));
        if p == NIL {
            self.head = n;
        } else {
            self.set_next(p, n);
        }
        if n == NIL {
            self.tail = p;
        } else {
            self.set_prev(n, p);
        }
        if let Some(s) = self.slots.get_mut(i) {
            s.kind = Kind::Gone;
            s.prev = NIL;
            s.next = NIL;
        }
    }

    fn flush(&mut self) {
        if !self.pending.is_empty() {
            let s = std::mem::take(&mut self.pending);
            self.push(Kind::Text(s), 0);
        }
    }

    /// Takes the tokens strictly between `a` and `b` (`NIL` is the start and the end of the
    /// list) out of the list, as inlines with adjacent text merged, and the deepest nesting among
    /// them.
    fn take_between(&mut self, a: usize, b: usize) -> (Vec<Inline>, u8) {
        let mut c = if a == NIL { self.head } else { self.next(a) };
        let mut out: Vec<Inline> = Vec::new();
        let mut depth = 0u8;
        while c != NIL && c != b {
            let nx = self.next(c);
            if let Some(s) = self.slots.get_mut(c) {
                let kind = std::mem::replace(&mut s.kind, Kind::Gone);
                let d = s.depth;
                let inl = match kind {
                    Kind::Text(t) => Some(Inline::Text(t)),
                    Kind::Delim(ch, n) => Some(Inline::Text((ch as char).to_string().repeat(n))),
                    Kind::Node(i) => {
                        depth = depth.max(d);
                        Some(i)
                    }
                    Kind::Gone => None,
                };
                if let Some(inl) = inl {
                    push_inline(&mut out, inl);
                }
            }
            c = nx;
        }
        if a == NIL {
            self.head = b;
        } else {
            self.set_next(a, b);
        }
        if b == NIL {
            self.tail = a;
        } else {
            self.set_prev(b, a);
        }
        (out, depth)
    }

    /// Children that would nest too deeply once wrapped are flattened to plain text first.
    fn fit_depth(&self, kids: Vec<Inline>, depth: u8) -> (Vec<Inline>, u8) {
        if usize::from(depth) < MAX_DEPTH {
            return (kids, depth);
        }
        let mut s = String::new();
        for k in &kids {
            plain_into(k, &mut s, usize::MAX);
        }
        if s.is_empty() {
            (Vec::new(), 0)
        } else {
            (vec![Inline::Text(s)], 0)
        }
    }

    // ── delimiter stack ──

    fn delim(&self, i: usize) -> Option<Delim> {
        self.delims.get(i).copied()
    }

    fn push_delim(&mut self, node: usize, ch: u8, len: usize, can_open: bool, can_close: bool) {
        let idx = self.delims.len();
        self.delims.push(Delim {
            node,
            ch,
            len,
            orig: len,
            can_open,
            can_close,
            prev: self.dtail,
            next: NIL,
            live: true,
        });
        if self.dtail != NIL {
            if let Some(d) = self.delims.get_mut(self.dtail) {
                d.next = idx;
            }
        }
        self.dtail = idx;
    }

    /// Takes a delimiter off the stack: its characters stay in the text as they are.
    fn remove_delim(&mut self, i: usize) {
        let Some(d) = self.delim(i) else { return };
        if !d.live {
            return;
        }
        if d.prev != NIL {
            if let Some(p) = self.delims.get_mut(d.prev) {
                p.next = d.next;
            }
        }
        if d.next == NIL {
            self.dtail = d.prev;
        } else if let Some(n) = self.delims.get_mut(d.next) {
            n.prev = d.prev;
        }
        if let Some(x) = self.delims.get_mut(i) {
            x.live = false;
        }
    }

    fn set_delim_len(&mut self, i: usize, len: usize) {
        let Some(d) = self.delims.get_mut(i) else {
            return;
        };
        d.len = len;
        let (node, ch) = (d.node, d.ch);
        if let Some(s) = self.slots.get_mut(node) {
            s.kind = Kind::Delim(ch, len);
        }
    }

    /// CommonMark's "process emphasis" over the delimiters above `bottom`, with the
    /// `openers_bottom` optimisation that keeps it linear.
    fn process_emphasis(&mut self, bottom: usize) {
        let bot: isize = if bottom == NIL { -1 } else { bottom as isize };
        let mut first = NIL;
        let mut c = self.dtail;
        while c != NIL && (c as isize) > bot {
            first = c;
            c = self.delim(c).map_or(NIL, |d| d.prev);
        }
        // Indexed by (character, closer can open, closer length mod 3).
        let mut bounds = [bot; 18];
        let mut closer = first;
        while closer != NIL {
            let Some(cd) = self.delim(closer) else { break };
            if !cd.can_close {
                closer = cd.next;
                continue;
            }
            let key = match cd.ch {
                b'*' => 0,
                b'_' => 6,
                _ => 12,
            } + usize::from(cd.can_open) * 3
                + cd.orig % 3;
            let bound = bounds.get(key).copied().unwrap_or(bot);
            let mut found = NIL;
            if self.wraps < MAX_WRAPS {
                let mut o = cd.prev;
                while o != NIL && (o as isize) > bound {
                    let Some(od) = self.delim(o) else { break };
                    if od.can_open && od.ch == cd.ch {
                        let odd = (cd.can_open || od.can_close)
                            && cd.orig % 3 != 0
                            && (od.orig + cd.orig) % 3 == 0;
                        if !odd {
                            found = o;
                            break;
                        }
                    }
                    o = od.prev;
                }
            }
            if found == NIL {
                let nb = if cd.prev == NIL {
                    bot
                } else {
                    (cd.prev as isize).max(bot)
                };
                if let Some(slot) = bounds.get_mut(key) {
                    *slot = nb;
                }
                if !cd.can_open {
                    self.remove_delim(closer);
                }
                closer = cd.next;
                continue;
            }
            let Some(od) = self.delim(found) else { break };
            let use_len = if cd.ch == b'~' || (cd.len >= 2 && od.len >= 2) {
                2
            } else {
                1
            };
            // Delimiters between the two cannot match across them any more.
            let mut d = cd.prev;
            while d != NIL && d != found {
                let p = self.delim(d).map_or(NIL, |x| x.prev);
                self.remove_delim(d);
                d = p;
            }
            let (kids, depth) = self.take_between(od.node, cd.node);
            let (kids, depth) = self.fit_depth(kids, depth);
            let inl = match (cd.ch, use_len) {
                (b'~', _) => Inline::Strike(kids),
                (_, 2) => Inline::Strong(kids),
                _ => Inline::Emphasis(kids),
            };
            self.wraps += 1;
            self.insert_after(od.node, Kind::Node(inl), depth + 1);
            self.set_delim_len(found, od.len.saturating_sub(use_len));
            self.set_delim_len(closer, cd.len.saturating_sub(use_len));
            if od.len <= use_len {
                self.remove_delim(found);
                self.unlink(od.node);
            }
            if cd.len <= use_len {
                let next = cd.next;
                self.remove_delim(closer);
                self.unlink(cd.node);
                closer = next;
            }
        }
        while self.dtail != NIL && (self.dtail as isize) > bot {
            let t = self.dtail;
            self.remove_delim(t);
        }
    }

    // ── scanning ──

    fn prepare_runs(&mut self) {
        self.runs_ready = true;
        let b = self.text.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if at(b, i) == b'`' {
                let s = i;
                while at(b, i) == b'`' {
                    i += 1;
                }
                self.runs.push((s, i - s));
            } else {
                i += 1;
            }
        }
        self.same = vec![NIL; self.runs.len()];
        let mut last: HashMap<usize, usize> = HashMap::new();
        for (idx, &(_, len)) in self.runs.iter().enumerate().rev() {
            if let Some(&nx) = last.get(&len) {
                if let Some(s) = self.same.get_mut(idx) {
                    *s = nx;
                }
            }
            last.insert(len, idx);
        }
    }

    fn run(mut self) -> Vec<Inline> {
        let text = self.text;
        let b = text.as_bytes();
        let n = b.len();
        let mut pos = 0;
        while pos < n {
            let start = pos;
            while pos < n && !is_special(at(b, pos)) {
                pos += 1;
            }
            if pos > start {
                self.pending.push_str(sl(text, start, pos));
            }
            if pos >= n {
                break;
            }
            let c = at(b, pos);
            match c {
                b'\n' => {
                    let trailing = self
                        .pending
                        .bytes()
                        .rev()
                        .take_while(|&x| x == b' ')
                        .count();
                    self.pending.truncate(self.pending.len() - trailing);
                    if trailing >= 2 {
                        self.flush();
                        self.push(Kind::Node(Inline::Break), 0);
                    } else {
                        self.pending.push(' ');
                    }
                    pos += 1;
                }
                b'\\' => {
                    let nx = at(b, pos + 1);
                    if pos + 1 >= n {
                        self.pending.push('\\');
                        pos += 1;
                    } else if nx == b'\n' {
                        let trailing = self
                            .pending
                            .bytes()
                            .rev()
                            .take_while(|&x| x == b' ')
                            .count();
                        self.pending.truncate(self.pending.len() - trailing);
                        self.flush();
                        self.push(Kind::Node(Inline::Break), 0);
                        pos += 2;
                    } else if nx == b'`' {
                        let mut e = pos + 1;
                        while at(b, e) == b'`' {
                            e += 1;
                        }
                        self.pending.push_str(sl(text, pos + 1, e));
                        pos = e;
                    } else if nx.is_ascii_punctuation() {
                        self.pending.push(nx as char);
                        pos += 2;
                    } else {
                        self.pending.push('\\');
                        pos += 1;
                    }
                }
                b'`' => pos = self.code_span(pos),
                b'*' | b'_' | b'~' => {
                    let mut e = pos;
                    while at(b, e) == c {
                        e += 1;
                    }
                    let k = e - pos;
                    let prev = text.get(..pos).and_then(|s| s.chars().next_back());
                    let next = text.get(e..).and_then(|s| s.chars().next());
                    let (can_open, can_close) = flank(c, prev, next);
                    if (c != b'~' || k == 2) && (can_open || can_close) && self.tracked < MAX_DELIMS
                    {
                        self.flush();
                        self.tracked += 1;
                        let node = self.push(Kind::Delim(c, k), 0);
                        self.push_delim(node, c, k, can_open, can_close);
                    } else {
                        self.pending.push_str(sl(text, pos, e));
                    }
                    pos = e;
                }
                b'[' => {
                    if self.tracked < MAX_DELIMS {
                        self.flush();
                        self.tracked += 1;
                        let node = self.push(Kind::Text("[".to_string()), 0);
                        self.brackets.push(Bracket {
                            node,
                            image: false,
                            dbot: self.dtail,
                        });
                    } else {
                        self.pending.push('[');
                    }
                    pos += 1;
                }
                b'!' => {
                    if at(b, pos + 1) == b'[' && self.tracked < MAX_DELIMS {
                        self.flush();
                        self.tracked += 1;
                        let node = self.push(Kind::Text("![".to_string()), 0);
                        self.brackets.push(Bracket {
                            node,
                            image: true,
                            dbot: self.dtail,
                        });
                        pos += 2;
                    } else {
                        self.pending.push('!');
                        pos += 1;
                    }
                }
                b']' => pos = self.close_bracket(pos),
                _ => match self.autolink(pos) {
                    Some(end) => pos = end,
                    None => {
                        self.pending.push('<');
                        pos += 1;
                    }
                },
            }
        }
        self.flush();
        self.process_emphasis(NIL);
        self.take_between(NIL, NIL).0
    }

    /// A backtick run at `pos`: a code span when a run of the same length follows, else text.
    fn code_span(&mut self, pos: usize) -> usize {
        if !self.runs_ready {
            self.prepare_runs();
        }
        let text = self.text;
        let r = self.runs.binary_search_by_key(&pos, |r| r.0).ok();
        let k = r.and_then(|r| self.runs.get(r)).map_or(1, |r| r.1);
        let closer = r
            .and_then(|r| self.same.get(r).copied())
            .filter(|&c| c != NIL)
            .and_then(|c| self.runs.get(c).copied());
        match closer {
            Some((cp, _)) => {
                let content = code_content(sl(text, pos + k, cp));
                self.flush();
                self.push(Kind::Node(Inline::Code(content)), 0);
                cp + k
            }
            None => {
                self.pending.push_str(sl(text, pos, pos + k));
                pos + k
            }
        }
    }

    /// `<http://…>` at `pos`: the position after it when it is a link.
    fn autolink(&mut self, pos: usize) -> Option<usize> {
        let text = self.text;
        let rest = text.get(pos + 1..)?;
        let head = rest.as_bytes().get(..6.min(rest.len()))?;
        let lower = head.to_ascii_lowercase();
        if !(lower.starts_with(b"http:") || lower.starts_with(b"https:")) || self.scan == 0 {
            return None;
        }
        let mut end = None;
        let mut scanned = 0;
        for (k, c) in rest.bytes().enumerate().take(MAX_URL_BYTES + 2) {
            scanned = k + 1;
            if c == b'>' {
                end = Some(k);
                break;
            }
            if c <= b' ' || c == b'<' || c == 0x7f {
                break;
            }
        }
        self.scan = self.scan.saturating_sub(scanned);
        let k = end?;
        let inner = rest.get(..k)?;
        if !url_ok(inner) {
            return None;
        }
        self.flush();
        let link = Inline::Link {
            content: vec![Inline::Text(inner.to_string())],
            target: LinkTarget::External(inner.to_string()),
            title: None,
        };
        self.push(Kind::Node(link), 1);
        self.floor = self.brackets.len();
        Some(pos + k + 2)
    }

    /// The `(destination "title")` that follows a `]`, `p` being the position of the `(`.
    fn parse_dest(&mut self, p: usize) -> Option<Dest<'t>> {
        if self.scan == 0 {
            return None;
        }
        let (dest, scanned) = scan_dest(self.text, p);
        self.scan = self.scan.saturating_sub(scanned.saturating_sub(p));
        dest
    }

    /// A `]` at `pos`: closes a link or an image, or is text. Returns the next position.
    fn close_bracket(&mut self, pos: usize) -> usize {
        let Some(br) = self.brackets.pop() else {
            self.pending.push(']');
            return pos + 1;
        };
        let idx = self.brackets.len();
        let inactive = !br.image && idx < self.floor;
        self.floor = self.floor.min(idx);
        if !inactive && at(self.text.as_bytes(), pos + 1) == b'(' {
            if let Some(d) = self.parse_dest(pos + 1) {
                self.flush();
                self.process_emphasis(br.dbot);
                if br.image {
                    self.make_image(br.node, d.target);
                } else {
                    self.make_link(br.node, &d);
                }
                return d.end;
            }
        }
        self.pending.push(']');
        pos + 1
    }

    fn make_link(&mut self, opener: usize, d: &Dest<'_>) {
        match classify_link(d.target, self.dir) {
            Some(target) => {
                let (kids, depth) = self.take_between(opener, NIL);
                let (kids, depth) = self.fit_depth(kids, depth);
                let title = d.title.filter(|t| !t.is_empty()).map(|t| t.to_string());
                let link = Inline::Link {
                    content: kids,
                    target,
                    title,
                };
                self.push(Kind::Node(link), depth + 1);
                self.unlink(opener);
                // A link may not contain a link: every earlier link bracket is dead now.
                self.floor = self.brackets.len();
            }
            // Not a link: what was written between the brackets is plain content.
            None => self.unlink(opener),
        }
    }

    fn make_image(&mut self, opener: usize, target: &str) {
        let (kids, _) = self.take_between(opener, NIL);
        let mut alt = String::new();
        for k in &kids {
            plain_into(k, &mut alt, MAX_ALT_CHARS * 4);
        }
        let (path, src, note) = classify_image(target, self.dir);
        let img = Inline::Image {
            alt: clip_chars(&alt, MAX_ALT_CHARS),
            path,
            src,
            note,
        };
        self.push(Kind::Node(img), 1);
        self.unlink(opener);
    }
}

fn push_inline(out: &mut Vec<Inline>, inl: Inline) {
    match inl {
        Inline::Text(t) if t.is_empty() => {}
        Inline::Text(t) => {
            if let Some(Inline::Text(last)) = out.last_mut() {
                last.push_str(&t);
            } else {
                out.push(Inline::Text(t));
            }
        }
        other => out.push(other),
    }
}

/// Scans `(destination "title")` with the `(` at `p`. Returns the result and the position the
/// scan reached (what it cost).
fn scan_dest(text: &str, p: usize) -> (Option<Dest<'_>>, usize) {
    let b = text.as_bytes();
    let n = b.len();
    let mut i = skip_ws(b, p + 1);
    let target;
    if at(b, i) == b'<' {
        let mut j = i + 1;
        loop {
            if j >= n || j - i > MAX_DEST_SCAN {
                return (None, j.min(n));
            }
            match at(b, j) {
                b'\n' | b'<' => return (None, j),
                b'\\' if at(b, j + 1).is_ascii_punctuation() => j += 2,
                b'>' => break,
                _ => j += 1,
            }
        }
        target = sl(text, i + 1, j);
        i = j + 1;
    } else {
        let mut depth = 0usize;
        let mut j = i;
        while j < n {
            let c = at(b, j);
            if c == b'\\' && at(b, j + 1).is_ascii_punctuation() {
                j += 2;
                continue;
            }
            if c <= b' ' || c == 0x7f {
                break;
            }
            if c == b'(' {
                depth += 1;
            } else if c == b')' {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            j += 1;
            if j - i > MAX_DEST_SCAN {
                return (None, j);
            }
        }
        if depth != 0 {
            return (None, j);
        }
        target = sl(text, i, j);
        i = j;
    }
    let before = i;
    i = skip_ws(b, i);
    let mut title = None;
    let q = at(b, i);
    if i > before && matches!(q, b'"' | b'\'' | b'(') {
        let close = if q == b'(' { b')' } else { q };
        let mut j = i + 1;
        loop {
            if j >= n || j - i > MAX_TITLE_BYTES {
                return (None, j.min(n));
            }
            let c = at(b, j);
            if c == b'\\' && at(b, j + 1).is_ascii_punctuation() {
                j += 2;
            } else if c == close {
                break;
            } else {
                j += 1;
            }
        }
        title = Some(sl(text, i + 1, j));
        i = skip_ws(b, j + 1);
    }
    if at(b, i) == b')' {
        (
            Some(Dest {
                target,
                title,
                end: i + 1,
            }),
            i,
        )
    } else {
        (None, i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn root() -> RepoPath {
        RepoPath::root()
    }

    fn doc(s: &str) -> MdDoc {
        parse(s, &root())
    }

    /// The canonical JSON of the blocks array.
    fn canon(s: &str) -> String {
        doc(s).to_value().canonical()
    }

    fn canon_in(dir: &str, s: &str) -> String {
        parse(s, &RepoPath::parse(dir).unwrap())
            .to_value()
            .canonical()
    }

    fn text(s: &str) -> String {
        format!(r#"{{"s":{},"t":"text"}}"#, json_str(s))
    }

    fn json_str(s: &str) -> String {
        Value::str(s).canonical()
    }

    fn para(inner: &str) -> String {
        format!(r#"{{"c":[{inner}],"t":"p"}}"#)
    }

    fn arr(items: &[String]) -> String {
        format!("[{}]", items.join(","))
    }

    // ── walking a value ──

    fn objects(v: &Value, out: &mut Vec<Value>) {
        match v {
            Value::Object(m) => {
                out.push(v.clone());
                for x in m.values() {
                    objects(x, out);
                }
            }
            Value::Array(a) => {
                for x in a {
                    objects(x, out);
                }
            }
            _ => {}
        }
    }

    fn all_objects(d: &MdDoc) -> Vec<Value> {
        let mut out = Vec::new();
        objects(&d.to_value(), &mut out);
        out
    }

    fn has_t(d: &MdDoc, t: &str) -> bool {
        all_objects(d)
            .iter()
            .any(|o| o.get("t").and_then(Value::as_str) == Some(t))
    }

    // ── headings, paragraphs, breaks ──

    #[test]
    fn atx_headings_of_every_level() {
        for level in 1..=6 {
            let src = format!("{} Title", "#".repeat(level));
            assert_eq!(
                canon(&src),
                format!(r#"[{{"c":[{}],"level":{level},"t":"h"}}]"#, text("Title"))
            );
        }
        // Seven hashes, no space and no content are not headings (but `#` alone is an empty one).
        assert_eq!(canon("####### x"), arr(&[para(&text("####### x"))]));
        assert_eq!(canon("#hashtag"), arr(&[para(&text("#hashtag"))]));
        assert_eq!(canon("#"), r#"[{"c":[],"level":1,"t":"h"}]"#);
    }

    #[test]
    fn atx_closing_hashes_are_removed() {
        assert_eq!(
            canon("## Title ##"),
            format!(r#"[{{"c":[{}],"level":2,"t":"h"}}]"#, text("Title"))
        );
        assert_eq!(
            canon("# C# #"),
            format!(r#"[{{"c":[{}],"level":1,"t":"h"}}]"#, text("C#"))
        );
        assert_eq!(
            canon("# a#"),
            format!(r#"[{{"c":[{}],"level":1,"t":"h"}}]"#, text("a#"))
        );
    }

    #[test]
    fn setext_headings() {
        assert_eq!(
            canon("Title\n====="),
            format!(r#"[{{"c":[{}],"level":1,"t":"h"}}]"#, text("Title"))
        );
        assert_eq!(
            canon("Two\nlines\n---"),
            format!(r#"[{{"c":[{}],"level":2,"t":"h"}}]"#, text("Two lines"))
        );
        // A rule after a blank line is a rule.
        assert_eq!(
            canon("a\n\n---"),
            arr(&[para(&text("a")), r#"{"t":"hr"}"#.to_string()])
        );
    }

    #[test]
    fn paragraphs_soft_and_hard_breaks() {
        assert_eq!(canon("a\nb"), arr(&[para(&text("a b"))]));
        assert_eq!(canon("a\n\nb"), arr(&[para(&text("a")), para(&text("b"))]));
        let br = r#"{"t":"br"}"#;
        assert_eq!(
            canon("a  \nb"),
            arr(&[para(&format!("{},{br},{}", text("a"), text("b")))])
        );
        assert_eq!(
            canon("a\\\nb"),
            arr(&[para(&format!("{},{br},{}", text("a"), text("b")))])
        );
        assert_eq!(canon("a \nb"), arr(&[para(&text("a b"))]));
        assert_eq!(canon(""), "[]");
        assert_eq!(canon("\n\n  \n"), "[]");
    }

    #[test]
    fn crlf_lines_are_lines() {
        assert_eq!(canon("# a\r\n\r\nb\r\n"), canon("# a\n\nb\n"));
    }

    #[test]
    fn backslash_escapes() {
        assert_eq!(canon(r"\*a\*"), arr(&[para(&text("*a*"))]));
        assert_eq!(canon(r"a\qb"), arr(&[para(&text(r"a\qb"))]));
        assert_eq!(canon("trailing\\"), arr(&[para(&text("trailing\\"))]));
        assert_eq!(canon(r"\[x\](y)"), arr(&[para(&text("[x](y)"))]));
    }

    // ── emphasis ──

    fn em(kind: &str, inner: &str) -> String {
        format!(r#"{{"c":[{inner}],"t":"{kind}"}}"#)
    }

    #[test]
    fn emphasis_strong_and_strike() {
        assert_eq!(canon("*a*"), arr(&[para(&em("em", &text("a")))]));
        assert_eq!(canon("_a_"), arr(&[para(&em("em", &text("a")))]));
        assert_eq!(canon("**a**"), arr(&[para(&em("strong", &text("a")))]));
        assert_eq!(canon("__a__"), arr(&[para(&em("strong", &text("a")))]));
        assert_eq!(canon("~~a~~"), arr(&[para(&em("del", &text("a")))]));
        assert_eq!(
            canon("***a***"),
            arr(&[para(&em("em", &em("strong", &text("a"))))])
        );
        assert_eq!(
            canon("*a **b** c*"),
            arr(&[para(&em(
                "em",
                &format!("{},{},{}", text("a "), em("strong", &text("b")), text(" c"))
            ))])
        );
    }

    #[test]
    fn emphasis_flanking_rules() {
        assert_eq!(
            canon("snake_case_name"),
            arr(&[para(&text("snake_case_name"))])
        );
        assert_eq!(canon("2 * 3 * 4"), arr(&[para(&text("2 * 3 * 4"))]));
        assert_eq!(
            canon("a*b*c"),
            arr(&[para(&format!(
                "{},{},{}",
                text("a"),
                em("em", &text("b")),
                text("c")
            ))])
        );
        assert_eq!(canon("*a"), arr(&[para(&text("*a"))]));
        assert_eq!(canon("a*"), arr(&[para(&text("a*"))]));
        assert_eq!(canon("~a~ ~~~b~~~"), arr(&[para(&text("~a~ ~~~b~~~"))]));
        assert_eq!(
            canon("**a*"),
            arr(&[para(&format!("{},{}", text("*"), em("em", &text("a"))))])
        );
    }

    #[test]
    fn code_spans() {
        let code = |s: &str| format!(r#"{{"s":{},"t":"code"}}"#, json_str(s));
        assert_eq!(canon("`a`"), arr(&[para(&code("a"))]));
        assert_eq!(canon("``a`b``"), arr(&[para(&code("a`b"))]));
        assert_eq!(canon("` a `"), arr(&[para(&code("a"))]));
        assert_eq!(canon("`  `"), arr(&[para(&code("  "))]));
        assert_eq!(canon("`*a*`"), arr(&[para(&code("*a*"))]));
        assert_eq!(canon("`a\nb`"), arr(&[para(&code("a b"))]));
        assert_eq!(canon("`a"), arr(&[para(&text("`a"))]));
        assert_eq!(canon("``a`"), arr(&[para(&text("``a`"))]));
        assert_eq!(canon(r"\`a`"), arr(&[para(&text("`a`"))]));
    }

    // ── code blocks ──

    fn code_block(lang: Option<&str>, t: &str) -> String {
        let l = lang.map_or("null".to_string(), json_str);
        format!(r#"{{"lang":{l},"t":"code","text":{}}}"#, json_str(t))
    }

    #[test]
    fn fenced_code_blocks() {
        assert_eq!(
            canon("```rust\nfn x() {}\n```"),
            arr(&[code_block(Some("rust"), "fn x() {}")])
        );
        assert_eq!(
            canon("~~~\na\n\n b\n~~~"),
            arr(&[code_block(None, "a\n\n b")])
        );
        assert_eq!(
            canon("```c++\nx\n```"),
            arr(&[code_block(Some("c++"), "x")])
        );
        assert_eq!(canon("```c#\nx\n```"), arr(&[code_block(Some("c#"), "x")]));
        // The info string must be a clean token.
        assert_eq!(
            canon("```js onclick=1\nx\n```"),
            arr(&[code_block(None, "x")])
        );
        assert_eq!(canon("```<script>\nx\n```"), arr(&[code_block(None, "x")]));
        let long = "a".repeat(33);
        assert_eq!(
            canon(&format!("```{long}\nx\n```")),
            arr(&[code_block(None, "x")])
        );
        let ok = "a".repeat(32);
        assert_eq!(
            canon(&format!("```{ok}\nx\n```")),
            arr(&[code_block(Some(&ok), "x")])
        );
        // Unclosed fences run to the end; a longer closing fence closes; a shorter one does not.
        assert_eq!(canon("```\na\n``"), arr(&[code_block(None, "a\n``")]));
        assert_eq!(
            canon("```\na\n`````\nb"),
            arr(&[code_block(None, "a"), para(&text("b"))])
        );
        assert_eq!(canon("```"), arr(&[code_block(None, "")]));
        // Markup inside is text; the fence indentation is removed from the lines.
        assert_eq!(
            canon("  ```\n  *a*\n    b\n  ```"),
            arr(&[code_block(None, "*a*\n  b")])
        );
        // A backtick fence cannot have a backtick in its info string.
        assert_eq!(canon("``` a`b"), arr(&[para(&text("``` a`b"))]));
    }

    #[test]
    fn indented_code_blocks() {
        assert_eq!(
            canon("    a\n     b\n\n    c"),
            arr(&[code_block(None, "a\n b\n\nc")])
        );
        assert_eq!(canon("\ta\n\tb"), arr(&[code_block(None, "a\nb")]));
        // A paragraph continues over an indented line.
        assert_eq!(canon("a\n    b"), arr(&[para(&text("a b"))]));
    }

    #[test]
    fn tabs_in_fenced_code_are_kept() {
        assert_eq!(
            canon("```go\nif x {\n\ty()\n}\n```"),
            arr(&[code_block(Some("go"), "if x {\n\ty()\n}")])
        );
    }

    // ── lists ──

    fn item(checked: Option<bool>, inner: &str) -> String {
        let c = match checked {
            None => "null",
            Some(true) => "true",
            Some(false) => "false",
        };
        format!(r#"{{"c":[{inner}],"checked":{c}}}"#)
    }

    fn ul(items: &[String]) -> String {
        format!(r#"{{"c":[{}],"t":"ul"}}"#, items.join(","))
    }

    fn ol(start: u32, items: &[String]) -> String {
        format!(r#"{{"c":[{}],"start":{start},"t":"ol"}}"#, items.join(","))
    }

    #[test]
    fn bullet_lists() {
        for m in ['-', '*', '+'] {
            let src = format!("{m} a\n{m} b");
            assert_eq!(
                canon(&src),
                arr(&[ul(&[
                    item(None, &para(&text("a"))),
                    item(None, &para(&text("b")))
                ])])
            );
        }
        // A different bullet starts a new list.
        assert_eq!(
            canon("- a\n* b"),
            arr(&[
                ul(&[item(None, &para(&text("a")))]),
                ul(&[item(None, &para(&text("b")))])
            ])
        );
        // Blank lines between items keep one list.
        assert_eq!(
            canon("- a\n\n- b"),
            arr(&[ul(&[
                item(None, &para(&text("a"))),
                item(None, &para(&text("b")))
            ])])
        );
        assert_eq!(canon("-a"), arr(&[para(&text("-a"))]));
    }

    #[test]
    fn ordered_lists_keep_their_start() {
        assert_eq!(
            canon("1. a\n2. b"),
            arr(&[ol(
                1,
                &[item(None, &para(&text("a"))), item(None, &para(&text("b")))]
            )])
        );
        assert_eq!(
            canon("7) a\n8) b"),
            arr(&[ol(
                7,
                &[item(None, &para(&text("a"))), item(None, &para(&text("b")))]
            )])
        );
        assert_eq!(
            canon("0. a"),
            arr(&[ol(0, &[item(None, &para(&text("a")))])])
        );
        assert_eq!(
            canon("999999999. a"),
            arr(&[ol(999_999_999, &[item(None, &para(&text("a")))])])
        );
        // Ten digits are not a marker; a different delimiter starts a new list.
        assert_eq!(canon("1234567890. a"), arr(&[para(&text("1234567890. a"))]));
        assert_eq!(
            canon("1. a\n2) b"),
            arr(&[
                ol(1, &[item(None, &para(&text("a")))]),
                ol(2, &[item(None, &para(&text("b")))])
            ])
        );
        // Only a list starting with 1 interrupts a paragraph.
        assert_eq!(canon("a\n2. b"), arr(&[para(&text("a 2. b"))]));
        assert_eq!(
            canon("a\n1. b"),
            arr(&[para(&text("a")), ol(1, &[item(None, &para(&text("b")))])])
        );
    }

    #[test]
    fn nested_lists_by_indentation() {
        assert_eq!(
            canon("- a\n  - b\n    - c\n- d"),
            arr(&[ul(&[
                item(
                    None,
                    &format!(
                        "{},{}",
                        para(&text("a")),
                        ul(&[item(
                            None,
                            &format!(
                                "{},{}",
                                para(&text("b")),
                                ul(&[item(None, &para(&text("c")))])
                            )
                        )])
                    )
                ),
                item(None, &para(&text("d")))
            ])])
        );
        // Ordered inside bullet, content offset of `1. `.
        assert_eq!(
            canon("- a\n  1. b"),
            arr(&[ul(&[item(
                None,
                &format!(
                    "{},{}",
                    para(&text("a")),
                    ol(1, &[item(None, &para(&text("b")))])
                )
            )])])
        );
        // Not indented enough: a sibling.
        assert_eq!(
            canon("1. a\n - b"),
            arr(&[
                ol(1, &[item(None, &para(&text("a")))]),
                ul(&[item(None, &para(&text("b")))])
            ])
        );
    }

    #[test]
    fn task_items() {
        assert_eq!(
            canon("- [ ] a\n- [x] b\n- [X] c\n- [y] d\n- [x]e"),
            arr(&[ul(&[
                item(Some(false), &para(&text("a"))),
                item(Some(true), &para(&text("b"))),
                item(Some(true), &para(&text("c"))),
                item(None, &para(&text("[y] d"))),
                item(None, &para(&text("[x]e"))),
            ])])
        );
    }

    #[test]
    fn list_item_lazy_continuation_and_paragraphs() {
        assert_eq!(
            canon("- a\nb"),
            arr(&[ul(&[item(None, &para(&text("a b")))])])
        );
        assert_eq!(
            canon("- a\n\n  b"),
            arr(&[ul(&[item(
                None,
                &format!("{},{}", para(&text("a")), para(&text("b")))
            )])])
        );
        assert_eq!(
            canon("- a\n\nb"),
            arr(&[ul(&[item(None, &para(&text("a")))]), para(&text("b"))])
        );
        // An empty item, and an item that starts with a blank line.
        assert_eq!(
            canon("-\n  a"),
            arr(&[ul(&[item(None, &para(&text("a")))])])
        );
        assert_eq!(
            canon("-\n\n  a"),
            arr(&[ul(&[item(None, "")]), para(&text("a"))])
        );
        // A rule is a rule, not an item.
        assert_eq!(canon("* * *"), r#"[{"t":"hr"}]"#);
        assert_eq!(
            canon("- a\n---"),
            arr(&[
                ul(&[item(None, &para(&text("a")))]),
                r#"{"t":"hr"}"#.to_string()
            ])
        );
    }

    // ── quotes, rules ──

    fn quote(inner: &str) -> String {
        format!(r#"{{"c":[{inner}],"t":"quote"}}"#)
    }

    #[test]
    fn block_quotes() {
        assert_eq!(canon("> a\n> b"), arr(&[quote(&para(&text("a b")))]));
        assert_eq!(canon(">a"), arr(&[quote(&para(&text("a")))]));
        assert_eq!(
            canon("> > a\n> b"),
            arr(&[quote(&quote(&para(&text("a b"))))])
        );
        assert_eq!(canon("> a\nb"), arr(&[quote(&para(&text("a b")))]));
        assert_eq!(
            canon("> a\n\nb"),
            arr(&[quote(&para(&text("a"))), para(&text("b"))])
        );
        assert_eq!(
            canon("> # h\n> - x"),
            arr(&[quote(&format!(
                r#"{{"c":[{}],"level":1,"t":"h"}},{}"#,
                text("h"),
                ul(&[item(None, &para(&text("x")))])
            ))])
        );
        // A lazy line does not continue a fenced block.
        assert_eq!(
            canon("> ```\n> a\nb"),
            arr(&[quote(&code_block(None, "a")), para(&text("b"))])
        );
        // Quotes in lists and lists in quotes.
        assert_eq!(
            canon("- > a"),
            arr(&[ul(&[item(None, &quote(&para(&text("a"))))])])
        );
    }

    #[test]
    fn thematic_breaks() {
        for src in ["---", "***", "___", "- - -", " * * * *", "_____"] {
            assert_eq!(canon(src), r#"[{"t":"hr"}]"#, "{src:?}");
        }
        assert_eq!(canon("--"), arr(&[para(&text("--"))]));
        assert_eq!(canon("---a"), arr(&[para(&text("---a"))]));
    }

    // ── tables ──

    fn cell(s: &str) -> String {
        format!("[{}]", text(s))
    }

    #[test]
    fn tables_with_alignment() {
        let src = "| a | b | c | d |\n|:--|:-:|--:|---|\n| 1 | 2 | 3 | 4 |\n| x |";
        let want = format!(
            r#"[{{"align":["left","center","right",null],"head":[{},{},{},{}],"rows":[[{},{},{},{}],[{},[],[],[]]],"t":"table"}}]"#,
            cell("a"),
            cell("b"),
            cell("c"),
            cell("d"),
            cell("1"),
            cell("2"),
            cell("3"),
            cell("4"),
            cell("x"),
        );
        assert_eq!(canon(src), want);
    }

    #[test]
    fn table_without_outer_pipes_extra_cells_and_escapes() {
        let src = "a | b\n--|--\n1 | 2 | 3\nx \\| y | `c`";
        let want = format!(
            r#"[{{"align":[null,null],"head":[{},{}],"rows":[[{},{}],[{},[{{"s":"c","t":"code"}}]]],"t":"table"}}]"#,
            cell("a"),
            cell("b"),
            cell("1"),
            cell("2"),
            cell("x | y"),
        );
        assert_eq!(canon(src), want);
    }

    #[test]
    fn table_needs_a_matching_delimiter_row() {
        // Different cell counts, or no pipe in the delimiter row: not a table.
        assert_eq!(
            canon("a | b\n---|---|---"),
            arr(&[para(&text("a | b ---|---|---"))])
        );
        assert_eq!(
            canon("a\n---"),
            format!(r#"[{{"c":[{}],"level":2,"t":"h"}}]"#, text("a"))
        );
        assert!(!has_t(&doc("| a |\n| b |"), "table"));
    }

    #[test]
    fn table_ends_at_a_blank_line_or_another_block() {
        let d = doc("| a |\n|---|\n| 1 |\n\npara\n");
        assert_eq!(d.blocks.len(), 2);
        let d = doc("| a |\n|---|\n| 1 |\n# h");
        assert_eq!(d.blocks.len(), 2);
        // A table can start right after a paragraph line.
        let d = doc("intro\n| a |\n|---|\n| 1 |");
        assert!(matches!(d.blocks.first(), Some(Block::Paragraph(_))));
        assert!(matches!(d.blocks.get(1), Some(Block::Table { .. })));
    }

    #[test]
    fn table_caps_columns_and_rows() {
        let cols = 40;
        let head = format!("|{}\n", "a|".repeat(cols));
        let delim = format!("|{}\n", "-|".repeat(cols));
        let row = format!("|{}\n", "b|".repeat(cols));
        let d = doc(&format!("{head}{delim}{row}"));
        assert!(d.truncated);
        match d.blocks.first() {
            Some(Block::Table { align, head, rows }) => {
                assert_eq!(align.len(), MAX_TABLE_COLS);
                assert_eq!(head.len(), MAX_TABLE_COLS);
                assert_eq!(rows.first().map(Vec::len), Some(MAX_TABLE_COLS));
            }
            other => panic!("{other:?}"),
        }
        let mut src = String::from("| a |\n|---|\n");
        for i in 0..MAX_TABLE_ROWS + 50 {
            src.push_str(&format!("| {i} |\n"));
        }
        let d = doc(&src);
        assert!(d.truncated);
        match d.blocks.first() {
            Some(Block::Table { rows, .. }) => assert_eq!(rows.len(), MAX_TABLE_ROWS),
            other => panic!("{other:?}"),
        }
        assert_eq!(d.blocks.len(), 1);
    }

    // ── links and images ──

    fn link_ext(inner: &str, url: &str, title: Option<&str>) -> String {
        format!(
            r#"{{"c":[{inner}],"kind":"external","path":null,"t":"link","title":{},"url":{}}}"#,
            title.map_or("null".to_string(), json_str),
            json_str(url)
        )
    }

    fn link_repo(inner: &str, path: &str) -> String {
        format!(
            r#"{{"c":[{inner}],"kind":"repo","path":{},"t":"link","title":null,"url":null}}"#,
            json_str(path)
        )
    }

    fn image(alt: &str, path: Option<&str>, src: &str, note: Option<&str>) -> String {
        format!(
            r#"{{"alt":{},"note":{},"path":{},"src":{},"t":"image"}}"#,
            json_str(alt),
            note.map_or("null".to_string(), json_str),
            path.map_or("null".to_string(), json_str),
            json_str(src)
        )
    }

    #[test]
    fn external_links() {
        assert_eq!(
            canon("[a](https://ok.example/x?y=1#z)"),
            arr(&[para(&link_ext(
                &text("a"),
                "https://ok.example/x?y=1#z",
                None
            ))])
        );
        assert_eq!(
            canon(r#"[a](http://ok.example "the title")"#),
            arr(&[para(&link_ext(
                &text("a"),
                "http://ok.example",
                Some("the title")
            ))])
        );
        assert_eq!(
            canon("[a](HTTPS://OK.example)"),
            arr(&[para(&link_ext(&text("a"), "HTTPS://OK.example", None))])
        );
        assert_eq!(
            canon("see <https://ok.example/p>."),
            arr(&[para(&format!(
                "{},{},{}",
                text("see "),
                link_ext(&text("https://ok.example/p"), "https://ok.example/p", None),
                text(".")
            ))])
        );
        assert_eq!(
            canon("[**b** `c`](https://ok.example/wiki_(x))"),
            arr(&[para(&link_ext(
                &format!(
                    "{},{},{}",
                    em("strong", &text("b")),
                    text(" "),
                    r#"{"s":"c","t":"code"}"#
                ),
                "https://ok.example/wiki_(x)",
                None
            ))])
        );
        assert_eq!(
            canon("[a](<https://ok.example/x>)"),
            arr(&[para(&link_ext(&text("a"), "https://ok.example/x", None))])
        );
    }

    #[test]
    fn repo_links_resolve_against_the_directory() {
        assert_eq!(
            canon_in(
                "docs/guide",
                "[a](../x.md#top) [b](sub/y.md) [c](/z.md) [d](./w%20v.md)"
            ),
            arr(&[para(&format!(
                "{},{},{},{},{},{},{}",
                link_repo(&text("a"), "docs/x.md"),
                text(" "),
                link_repo(&text("b"), "docs/guide/sub/y.md"),
                text(" "),
                link_repo(&text("c"), "z.md"),
                text(" "),
                link_repo(&text("d"), "docs/guide/w v.md")
            ))])
        );
    }

    #[test]
    fn images() {
        assert_eq!(
            canon_in("docs", "![logo](img/a.png)"),
            arr(&[para(&image(
                "logo",
                Some("docs/img/a.png"),
                "img/a.png",
                None
            ))])
        );
        assert_eq!(
            canon("![x](https://tracker.example/p.png)"),
            arr(&[para(&image(
                "x",
                None,
                "https://tracker.example/p.png",
                Some("remote images are not loaded")
            ))])
        );
        assert_eq!(
            canon("![x](data:image/png;base64,AAAA)"),
            arr(&[para(&image(
                "x",
                None,
                "data:image/png;base64,AAAA",
                Some("images embedded in the document are not loaded")
            ))])
        );
        assert_eq!(
            canon("![*e* `c`](ftp://x/y.png)"),
            arr(&[para(&image(
                "e c",
                None,
                "ftp://x/y.png",
                Some("image target not usable")
            ))])
        );
        assert_eq!(
            canon("![x](../../../../etc/passwd)"),
            arr(&[para(&image(
                "x",
                None,
                "../../../../etc/passwd",
                Some("image target not usable")
            ))])
        );
        assert_eq!(
            canon("![x]()"),
            arr(&[para(&image("x", None, "", Some("image target not usable")))])
        );
        // The alt text and the source text are clipped.
        let long = "a".repeat(300);
        let d = doc(&format!("![{long}]({long}.png)"));
        let objs = all_objects(&d);
        let img = objs
            .iter()
            .find(|o| o.get("t").and_then(Value::as_str) == Some("image"))
            .unwrap();
        assert_eq!(
            img.get("alt")
                .and_then(Value::as_str)
                .map(|s| s.chars().count()),
            Some(200)
        );
        assert_eq!(
            img.get("src")
                .and_then(Value::as_str)
                .map(|s| s.chars().count()),
            Some(200)
        );
    }

    #[test]
    fn image_inside_a_link_is_fine_but_links_do_not_nest() {
        let d = doc("[![x](a.png)](https://ok.example)");
        let objs = all_objects(&d);
        assert_eq!(
            objs.iter()
                .filter(|o| o.get("t").and_then(Value::as_str) == Some("link"))
                .count(),
            1
        );
        assert_eq!(
            objs.iter()
                .filter(|o| o.get("t").and_then(Value::as_str) == Some("image"))
                .count(),
            1
        );
        // [outer [inner](https://a.example)](https://b.example): only the inner one is a link.
        let d = doc("[outer [inner](https://a.example)](https://b.example)");
        let objs = all_objects(&d);
        let links: Vec<_> = objs
            .iter()
            .filter(|o| o.get("t").and_then(Value::as_str) == Some("link"))
            .collect();
        assert_eq!(links.len(), 1);
        assert_eq!(
            links[0].get("url").and_then(Value::as_str),
            Some("https://a.example")
        );
        let d = doc("[x <https://a.example>](https://b.example)");
        let objs = all_objects(&d);
        assert_eq!(
            objs.iter()
                .filter(|o| o.get("t").and_then(Value::as_str) == Some("link"))
                .count(),
            1
        );
    }

    #[test]
    fn no_reference_links_no_entities_no_html() {
        assert_eq!(
            canon("[a][b]\n\n[b]: https://ok.example"),
            arr(&[
                para(&text("[a][b]")),
                para(&text("[b]: https://ok.example"))
            ])
        );
        assert_eq!(canon("[a]"), arr(&[para(&text("[a]"))]));
        assert_eq!(
            canon("&amp; &lt;b&gt; &#60;"),
            arr(&[para(&text("&amp; &lt;b&gt; &#60;"))])
        );
        assert_eq!(canon("<b>x</b>"), arr(&[para(&text("<b>x</b>"))]));
        assert_eq!(canon("<!-- c -->"), arr(&[para(&text("<!-- c -->"))]));
        assert_eq!(
            canon("<div>\n*a*\n</div>"),
            arr(&[para(&format!(
                "{},{},{}",
                text("<div> "),
                em("em", &text("a")),
                text(" </div>")
            ))])
        );
    }

    // ── golden documents ──

    #[test]
    fn golden_readme() {
        let src = "# Title\n\nHello *world*, see [docs](docs/a.md).\n\n- one\n- two\n\n```sh\nmake\n```\n";
        let want = concat!(
            r#"[{"c":[{"s":"Title","t":"text"}],"level":1,"t":"h"},"#,
            r#"{"c":[{"s":"Hello ","t":"text"},{"c":[{"s":"world","t":"text"}],"t":"em"},{"s":", see ","t":"text"},"#,
            r#"{"c":[{"s":"docs","t":"text"}],"kind":"repo","path":"a/docs/a.md","t":"link","title":null,"url":null},{"s":".","t":"text"}],"t":"p"},"#,
            r#"{"c":[{"c":[{"c":[{"s":"one","t":"text"}],"t":"p"}],"checked":null},{"c":[{"c":[{"s":"two","t":"text"}],"t":"p"}],"checked":null}],"t":"ul"},"#,
            r#"{"lang":"sh","t":"code","text":"make"}]"#
        );
        assert_eq!(canon_in("a", src), want);
    }

    #[test]
    fn golden_table_and_quote() {
        let src = "> note\n\n| k | v |\n|---|--:|\n| a | 1 |\n";
        let want = concat!(
            r#"[{"c":[{"c":[{"s":"note","t":"text"}],"t":"p"}],"t":"quote"},"#,
            r#"{"align":[null,"right"],"head":[[{"s":"k","t":"text"}],[{"s":"v","t":"text"}]],"#,
            r#""rows":[[[{"s":"a","t":"text"}],[{"s":"1","t":"text"}]]],"t":"table"}]"#
        );
        assert_eq!(canon(src), want);
    }

    #[test]
    fn golden_hostile_document() {
        let src = "<script>alert(1)</script>\n\n[x](javascript:alert(1)) ![y](https://tracker.example/p.png)\n";
        let want = arr(&[
            para(&text("<script>alert(1)</script>")),
            para(&format!(
                "{},{}",
                text("x "),
                image(
                    "y",
                    None,
                    "https://tracker.example/p.png",
                    Some("remote images are not loaded")
                )
            )),
        ]);
        assert_eq!(canon(src), want);
    }

    // ── security ──

    fn links_and_images(d: &MdDoc) -> (Vec<Value>, Vec<Value>) {
        let objs = all_objects(d);
        let pick = |t: &str| {
            objs.iter()
                .filter(|o| o.get("t").and_then(Value::as_str) == Some(t))
                .cloned()
                .collect::<Vec<_>>()
        };
        (pick("link"), pick("image"))
    }

    #[test]
    fn hostile_link_targets_are_not_links() {
        let targets = [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "java\tscript:alert(1)",
            "java\nscript:alert(1)",
            "java%73cript:alert(1)",
            "%6Aavascript:alert(1)",
            "data:text/html,<script>",
            "vbscript:x",
            "file:///etc/passwd",
            "mailto:a@b.example",
            "ftp://x.example/y",
            "blob:https://x/1",
            "about:blank",
            "//evil.example/x",
            "#frag",
            "https://ok.example/a b",
            "<https://ok.example/a b>",
            "https://ok.example/\\x",
            "https://ok.example/\u{202e}x",
            "https://ok.example/\u{200b}x",
            "https://ok.example/\u{7}x",
            "https://",
            "https:///path",
            "https:example.com",
            "http:/example.com",
            "../../x",
            "../../../../etc/passwd",
            "a\\b",
            "a\0b",
            "",
            "a/../../x",
            "%2e%2e/%2e%2e/x",
            "x/.git/config",
            "a:b",
            "C:\\windows",
        ];
        for t in targets {
            for src in [
                format!("[x]({t})"),
                format!("[x](<{t}>)"),
                format!("![x]({t})"),
            ] {
                let d = doc(&src);
                let (links, images) = links_and_images(&d);
                assert!(
                    links.is_empty(),
                    "{src:?} made a link: {:?}",
                    d.to_value().canonical()
                );
                for img in images {
                    assert!(img.get("path").unwrap().is_null(), "{src:?}");
                    assert!(img.get("note").unwrap().as_str().is_some(), "{src:?}");
                }
            }
        }
    }

    #[test]
    fn rejected_link_keeps_only_its_text() {
        assert_eq!(canon("[x](javascript:alert(1))"), arr(&[para(&text("x"))]));
        assert_eq!(canon("[x](JaVaScRiPt:alert(1))"), arr(&[para(&text("x"))]));
        assert_eq!(
            canon("[*x*](data:text/html,<script>)"),
            arr(&[para(&em("em", &text("x")))])
        );
        assert_eq!(canon("[x](//evil.example/x)"), arr(&[para(&text("x"))]));
        assert_eq!(canon("[x](../../x)"), arr(&[para(&text("x"))]));
        assert_eq!(canon("a [x](#top) b"), arr(&[para(&text("a x b"))]));
    }

    #[test]
    fn malformed_destinations_stay_text() {
        // A space in a bare destination ends it; what follows is not a title: not a link.
        assert_eq!(
            canon("[x](https://ok.example/a b)"),
            arr(&[para(&text("[x](https://ok.example/a b)"))])
        );
        assert_eq!(
            canon("[x](java\tscript:alert(1))"),
            arr(&[para(&text("[x](java\tscript:alert(1))"))])
        );
        assert_eq!(canon("[x](a\0b)"), arr(&[para(&text("[x](a\0b)"))]));
        assert_eq!(canon("[x](unclosed"), arr(&[para(&text("[x](unclosed"))]));
    }

    #[test]
    fn script_and_html_are_text() {
        for src in [
            "<script>alert(1)</script>",
            "<img src=x onerror=alert(1)>",
            "<a href=\"javascript:alert(1)\">x</a>",
            "<iframe srcdoc=\"<script>\"></iframe>",
            "<svg onload=alert(1)>",
        ] {
            let d = doc(src);
            let (links, images) = links_and_images(&d);
            assert!(links.is_empty() && images.is_empty(), "{src}");
            assert_eq!(d.blocks.len(), 1);
            // The text is intact.
            let mut s = String::new();
            for o in all_objects(&d) {
                if let Some(t) = o.get("s").and_then(Value::as_str) {
                    s.push_str(t);
                }
            }
            assert_eq!(s, src.replace('\n', " "));
        }
        assert_eq!(
            canon("<javascript:alert(1)>"),
            arr(&[para(&text("<javascript:alert(1)>"))])
        );
        assert_eq!(
            canon("<mailto:a@b.example>"),
            arr(&[para(&text("<mailto:a@b.example>"))])
        );
        assert_eq!(
            canon("<https://ok.example/a b>"),
            arr(&[para(&text("<https://ok.example/a b>"))])
        );
    }

    #[test]
    fn escapes_and_bidi_are_text_never_structure() {
        let src = "\u{1b}[31mred\u{1b}[0m \u{202e}evil [a\u{202e}b](https://ok.example/) \u{7} \0 `\u{202e}`";
        let d = doc(src);
        let (links, _) = links_and_images(&d);
        assert_eq!(links.len(), 1);
        let json = d.to_value().canonical();
        assert!(json.contains("\\u001b"), "{json}");
        // No structure besides the paragraph, the link and the code span.
        for o in all_objects(&d) {
            let t = o.get("t").and_then(Value::as_str);
            assert!(matches!(t, Some("p" | "text" | "link" | "code")), "{t:?}");
        }
        let mut s = String::new();
        for o in all_objects(&d) {
            if o.get("t").and_then(Value::as_str) == Some("text") {
                s.push_str(o.get("s").and_then(Value::as_str).unwrap());
            }
        }
        assert!(s.contains('\u{1b}') && s.contains('\u{202e}') && s.contains('\0'));
        // A bidi override inside a URL is not a link.
        assert!(links_and_images(&doc("[a](https://ok.example/\u{202e})"))
            .0
            .is_empty());
        assert!(links_and_images(&doc("<https://ok.example/\u{202e}>"))
            .0
            .is_empty());
    }

    #[test]
    fn external_urls_are_clean_and_bounded() {
        let long_ok = format!("https://ok.example/{}", "a".repeat(2048 - 19));
        assert_eq!(long_ok.len(), 2048);
        assert!(!links_and_images(&doc(&format!("[x]({long_ok})")))
            .0
            .is_empty());
        let long_bad = format!("{long_ok}a");
        assert!(links_and_images(&doc(&format!("[x]({long_bad})")))
            .0
            .is_empty());
        assert!(links_and_images(&doc(&format!("<{long_bad}>")))
            .0
            .is_empty());
    }

    // ── bounds ──

    #[test]
    fn source_is_cut_on_a_char_boundary() {
        let src = format!("{}é", "a".repeat(MAX_MARKDOWN_BYTES - 1));
        let d = doc(&src);
        assert!(d.truncated);
        let mut total = 0;
        for o in all_objects(&d) {
            if let Some(s) = o.get("s").and_then(Value::as_str) {
                total += s.len();
            }
        }
        assert_eq!(total, MAX_MARKDOWN_BYTES - 1);
        assert!(!doc("small").truncated);
        let exact = "a".repeat(MAX_MARKDOWN_BYTES);
        assert!(!doc(&exact).truncated);
    }

    fn block_depth(v: &Value) -> usize {
        // Nesting of block containers (quote, list items) below this block.
        match v.get("t").and_then(Value::as_str) {
            Some("quote") => {
                1 + v
                    .get("c")
                    .and_then(Value::as_array)
                    .map_or(0, |c| c.iter().map(block_depth).max().unwrap_or(0))
            }
            Some("ul" | "ol") => {
                1 + v.get("c").and_then(Value::as_array).map_or(0, |items| {
                    items
                        .iter()
                        .map(|i| {
                            i.get("c")
                                .and_then(Value::as_array)
                                .map_or(0, |c| c.iter().map(block_depth).max().unwrap_or(0))
                        })
                        .max()
                        .unwrap_or(0)
                })
            }
            _ => 0,
        }
    }

    fn inline_depth(v: &Value) -> usize {
        match v.get("t").and_then(Value::as_str) {
            Some("em" | "strong" | "del" | "link") => {
                1 + v
                    .get("c")
                    .and_then(Value::as_array)
                    .map_or(0, |c| c.iter().map(inline_depth).max().unwrap_or(0))
            }
            _ => 0,
        }
    }

    #[test]
    fn deeply_nested_quotes_are_flattened() {
        let src = format!("{}deep", "> ".repeat(40));
        let d = doc(&src);
        let v = d.to_value();
        let depth = v.as_array().unwrap().iter().map(block_depth).max().unwrap();
        assert_eq!(depth, MAX_DEPTH);
        // The text and the remaining markers are all still there.
        let json = v.canonical();
        assert!(json.contains("deep"));
        assert!(json.contains("> > "));
    }

    #[test]
    fn deeply_nested_lists_are_flattened() {
        let mut src = String::new();
        for i in 0..40 {
            src.push_str(&" ".repeat(i * 2));
            src.push_str("- x\n");
        }
        let d = doc(&src);
        let v = d.to_value();
        let depth = v.as_array().unwrap().iter().map(block_depth).max().unwrap();
        assert!(depth <= MAX_DEPTH, "{depth}");
        assert!(depth >= MAX_DEPTH - 1, "{depth}");
    }

    #[test]
    fn deeply_nested_inlines_are_flattened() {
        // *a **b *c ... nested emphasis, 60 levels.
        let mut src = String::new();
        for i in 0..60 {
            src.push_str(if i % 2 == 0 { "*a " } else { "**b " });
        }
        for i in (0..60).rev() {
            src.push_str(if i % 2 == 0 { "a*" } else { "b**" });
            src.push(' ');
        }
        let d = doc(&src);
        let v = d.to_value();
        let depth = v
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|p| p.get("c").and_then(Value::as_array).unwrap().iter())
            .map(inline_depth)
            .max()
            .unwrap();
        assert!(depth <= MAX_DEPTH, "{depth}");
        assert!(depth >= 10, "{depth}");
        // Nested links by brackets and images cannot go deep either.
        let mut src = String::new();
        for _ in 0..40 {
            src.push_str("*[");
        }
        src.push('x');
        for _ in 0..40 {
            src.push_str("](https://ok.example/)*");
        }
        let v = doc(&src).to_value();
        let depth = v
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|p| p.get("c").and_then(Value::as_array).unwrap().iter())
            .map(inline_depth)
            .max()
            .unwrap();
        assert!(depth <= MAX_DEPTH, "{depth}");
    }

    #[test]
    fn node_cap_stops_and_truncates() {
        let src = "- a\n".repeat(30_000);
        let d = doc(&src);
        assert!(d.truncated);
        let n = all_objects(&d)
            .iter()
            .filter(|o| o.get("t").is_some())
            .count();
        assert!(n <= MAX_NODES, "{n}");
        let src = "a\n\n".repeat(30_000);
        let d = doc(&src);
        assert!(d.truncated);
        assert!(
            all_objects(&d)
                .iter()
                .filter(|o| o.get("t").is_some())
                .count()
                <= MAX_NODES
        );
        let src = "*a*\n\n".repeat(30_000);
        let d = doc(&src);
        assert!(d.truncated);
        assert!(
            all_objects(&d)
                .iter()
                .filter(|o| o.get("t").is_some())
                .count()
                <= MAX_NODES
        );
    }

    // ── structural validation shared by the property and size tests ──

    #[derive(Default)]
    struct Stats {
        nodes: usize,
        text: usize,
        max_block: usize,
        max_inline: usize,
    }

    fn keys(v: &Value) -> Vec<&str> {
        v.as_object().unwrap().keys().map(String::as_str).collect()
    }

    fn expect_keys(v: &Value, want: &[&str]) {
        let mut want: Vec<&str> = want.to_vec();
        want.sort_unstable();
        assert_eq!(keys(v), want, "{}", v.canonical());
    }

    fn t_of(v: &Value) -> &str {
        v.get("t")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("no t: {}", v.canonical()))
    }

    fn external_ok(url: &str) -> bool {
        let lower = url.to_ascii_lowercase();
        let rest = if let Some(r) = lower.strip_prefix("https:") {
            r
        } else if let Some(r) = lower.strip_prefix("http:") {
            r
        } else {
            return false;
        };
        let Some(rest) = rest.strip_prefix("//") else {
            return false;
        };
        url.len() <= 2048
            && !url
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || c == '\\' || is_invisible(c))
            && !rest.is_empty()
            && !rest.starts_with('/')
    }

    fn check_inlines(v: &Value, depth: usize, st: &mut Stats) {
        let arr = v
            .as_array()
            .unwrap_or_else(|| panic!("inlines not an array: {}", v.canonical()));
        for n in arr {
            st.nodes += 1;
            st.max_inline = st.max_inline.max(depth + 1);
            match t_of(n) {
                "text" => {
                    expect_keys(n, &["t", "s"]);
                    st.text += n.get("s").and_then(Value::as_str).unwrap().len();
                }
                "code" => {
                    expect_keys(n, &["t", "s"]);
                    st.text += n.get("s").and_then(Value::as_str).unwrap().len();
                }
                "br" => expect_keys(n, &["t"]),
                "em" | "strong" | "del" => {
                    expect_keys(n, &["t", "c"]);
                    check_inlines(n.get("c").unwrap(), depth + 1, st);
                }
                "link" => {
                    expect_keys(n, &["t", "c", "kind", "url", "path", "title"]);
                    match n.get("kind").and_then(Value::as_str) {
                        Some("external") => {
                            let url = n.get("url").and_then(Value::as_str).expect("external url");
                            assert!(n.get("path").unwrap().is_null());
                            assert!(external_ok(url), "bad external url {url:?}");
                            st.text += url.len();
                        }
                        Some("repo") => {
                            let p = n.get("path").and_then(Value::as_str).expect("repo path");
                            assert!(n.get("url").unwrap().is_null());
                            assert!(
                                RepoPath::parse(p).is_ok() && !p.is_empty(),
                                "bad path {p:?}"
                            );
                            st.text += p.len();
                        }
                        other => panic!("kind {other:?}"),
                    }
                    if let Some(t) = n.get("title").and_then(Value::as_str) {
                        st.text += t.len();
                    }
                    check_inlines(n.get("c").unwrap(), depth + 1, st);
                }
                "image" => {
                    expect_keys(n, &["t", "alt", "path", "src", "note"]);
                    let alt = n.get("alt").and_then(Value::as_str).unwrap();
                    let src = n.get("src").and_then(Value::as_str).unwrap();
                    assert!(alt.chars().count() <= 200 && src.chars().count() <= 200);
                    st.text += alt.len() + src.len();
                    match n.get("path").and_then(Value::as_str) {
                        Some(p) => {
                            assert!(
                                RepoPath::parse(p).is_ok() && !p.is_empty(),
                                "bad image path {p:?}"
                            );
                            assert!(n.get("note").unwrap().is_null());
                        }
                        None => assert!(n.get("note").and_then(Value::as_str).is_some()),
                    }
                }
                other => panic!("inline t {other:?}"),
            }
        }
    }

    fn check_blocks(v: &Value, depth: usize, st: &mut Stats) {
        let arr = v
            .as_array()
            .unwrap_or_else(|| panic!("blocks not an array: {}", v.canonical()));
        for b in arr {
            st.nodes += 1;
            st.max_block = st.max_block.max(depth + 1);
            match t_of(b) {
                "h" => {
                    expect_keys(b, &["t", "level", "c"]);
                    let l = b.get("level").and_then(Value::as_i64).unwrap();
                    assert!((1..=6).contains(&l));
                    check_inlines(b.get("c").unwrap(), 0, st);
                }
                "p" => {
                    expect_keys(b, &["t", "c"]);
                    check_inlines(b.get("c").unwrap(), 0, st);
                }
                "code" => {
                    expect_keys(b, &["t", "lang", "text"]);
                    st.text += b.get("text").and_then(Value::as_str).unwrap().len();
                    if let Some(l) = b.get("lang").and_then(Value::as_str) {
                        assert!(!l.is_empty() && l.chars().count() <= 32);
                        assert!(l
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || "_+.#-".contains(c)));
                        st.text += l.len();
                    }
                }
                "quote" => {
                    expect_keys(b, &["t", "c"]);
                    check_blocks(b.get("c").unwrap(), depth + 1, st);
                }
                t @ ("ul" | "ol") => {
                    if t == "ul" {
                        expect_keys(b, &["t", "c"]);
                    } else {
                        expect_keys(b, &["t", "c", "start"]);
                        assert!(b.get("start").and_then(Value::as_u64).unwrap() <= 999_999_999);
                    }
                    for item in b.get("c").and_then(Value::as_array).unwrap() {
                        st.nodes += 1;
                        expect_keys(item, &["checked", "c"]);
                        match item.get("checked").unwrap() {
                            Value::Null | Value::Bool(_) => {}
                            other => panic!("checked {other:?}"),
                        }
                        check_blocks(item.get("c").unwrap(), depth + 1, st);
                    }
                }
                "table" => {
                    expect_keys(b, &["t", "align", "head", "rows"]);
                    let align = b.get("align").and_then(Value::as_array).unwrap();
                    assert!(align.len() <= MAX_TABLE_COLS);
                    for a in align {
                        assert!(matches!(
                            a.as_str(),
                            None | Some("left" | "center" | "right")
                        ));
                    }
                    let head = b.get("head").and_then(Value::as_array).unwrap();
                    assert_eq!(head.len(), align.len());
                    for c in head {
                        st.nodes += 1;
                        check_inlines(c, 0, st);
                    }
                    let rows = b.get("rows").and_then(Value::as_array).unwrap();
                    assert!(rows.len() <= MAX_TABLE_ROWS);
                    for r in rows {
                        let r = r.as_array().unwrap();
                        assert_eq!(r.len(), align.len());
                        for c in r {
                            st.nodes += 1;
                            check_inlines(c, 0, st);
                        }
                    }
                }
                "hr" => expect_keys(b, &["t"]),
                other => panic!("block t {other:?}"),
            }
        }
    }

    /// Validates a whole parse of `src`; returns the statistics.
    fn check_doc(src: &str) -> (MdDoc, Stats) {
        let d = doc(src);
        let v = d.to_value();
        let mut st = Stats::default();
        check_blocks(&v, 0, &mut st);
        assert!(st.nodes <= MAX_NODES, "{} nodes", st.nodes);
        assert!(
            st.max_block <= MAX_DEPTH + 1,
            "block depth {}",
            st.max_block
        );
        assert!(
            st.max_inline <= MAX_DEPTH + 1,
            "inline depth {}",
            st.max_inline
        );
        let seen = src.len().min(MAX_MARKDOWN_BYTES);
        assert!(
            st.text <= seen * 3 + 1024,
            "text volume {} for {} bytes",
            st.text,
            seen
        );
        (d, st)
    }

    // ── property tests ──

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    const FRAGMENTS: &[&str] = &[
        "*",
        "**",
        "***",
        "_",
        "__",
        "~~",
        "~",
        "`",
        "``",
        "```",
        "~~~",
        "[",
        "]",
        "](",
        "![",
        ")",
        "(",
        "<",
        ">",
        "|",
        "---",
        "===",
        ":---:",
        "|---|---|",
        "|:-|-:|",
        "1. ",
        "2) ",
        "- ",
        "* ",
        "+ ",
        "- [ ] ",
        "- [x] ",
        "> ",
        ">",
        "# ",
        "## ",
        "#",
        "\\",
        "\\*",
        "\n",
        "\n\n",
        "\n",
        "  ",
        "    ",
        "\t",
        " ",
        "  \n",
        "0",
        "7",
        "42",
        "999999999",
        "1234567890",
        "a",
        "b",
        "word",
        "hello world",
        "x_y_z",
        "snake_case",
        "https://ok.example/a",
        "http://ok.example",
        "HTTPS://OK.EXAMPLE/",
        "javascript:alert(1)",
        "JaVaScRiPt:x",
        "java%73cript:x",
        "data:text/html,<script>",
        "//evil.example/x",
        "#frag",
        "../../x",
        "../a/b.md",
        "./c.png",
        "/abs/d.md",
        "a b",
        "%20",
        "%",
        "%zz",
        "\"t\"",
        "'t'",
        "<script>alert(1)</script>",
        "<img src=x onerror=1>",
        "<https://ok.example/p>",
        "<mailto:a@b>",
        "&amp;",
        "é",
        "日本語",
        "\u{202e}",
        "\u{200b}",
        "\u{1b}[31m",
        "\0",
        "\u{7f}",
        "😀",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "[x](",
        "[x](https://ok.example/a)",
        "![i](a.png)",
        "![i](https://t.example/p.png)",
        "[a](b \"t\")",
        "[]()",
        "<>",
        "\\\n",
    ];

    fn random_doc(rng: &mut Rng, max_frags: usize) -> String {
        let n = 1 + rng.below(max_frags);
        let mut s = String::new();
        for _ in 0..n {
            s.push_str(FRAGMENTS[rng.below(FRAGMENTS.len())]);
        }
        s
    }

    #[test]
    fn property_random_markdown_soup() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for round in 0..4000 {
            let max = if round % 10 == 0 { 600 } else { 60 };
            let src = random_doc(&mut rng, max);
            let dir = if round % 3 == 0 { "docs/guide" } else { "" };
            let d = parse(&src, &RepoPath::parse(dir).unwrap());
            let v = d.to_value();
            let mut st = Stats::default();
            check_blocks(&v, 0, &mut st);
            assert!(st.nodes <= MAX_NODES);
            assert!(st.max_block <= MAX_DEPTH + 1, "{src:?}");
            assert!(st.max_inline <= MAX_DEPTH + 1, "{src:?}");
            assert!(
                st.text <= src.len() * 3 + 1024,
                "{src:?}: {} > {}",
                st.text,
                src.len()
            );
            assert!(!d.truncated, "{src:?}");
        }
    }

    #[test]
    fn property_random_bytes_of_markdown_characters() {
        // Denser than the fragments: every character is a Markdown special.
        let specials: Vec<char> = "*_~`[]()!<>|-=#>\\\n \t:+.1x\"'&é\u{202e}"
            .chars()
            .collect();
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        for _ in 0..3000 {
            let n = 1 + rng.below(300);
            let src: String = (0..n)
                .map(|_| specials[rng.below(specials.len())])
                .collect();
            check_doc(&src);
        }
    }

    #[test]
    fn property_no_panic_on_every_prefix_of_a_rich_document() {
        let src = "# T\n\n> q *e* **s** ~~d~~ `c`\n> > n\n\n1. a\n   - [ ] b\n\n| a | b |\n|:-:|--:|\n| [l](x.md) | ![i](p.png) |\n\n```rs\nfn\n```\n\n    code\n\n---\n[x](https://ok.example) <https://ok.example>\n";
        for (i, _) in src.char_indices() {
            check_doc(&src[..i]);
        }
        check_doc(src);
    }

    // ── adversarial sizes ──

    fn timed(name: &str, src: &str) -> (MdDoc, Stats) {
        let t = Instant::now();
        let (d, st) = check_doc(src);
        let took = t.elapsed();
        assert!(took < Duration::from_secs(3), "{name}: {took:?}");
        (d, st)
    }

    fn fill(unit: &str, bytes: usize) -> String {
        unit.repeat(bytes / unit.len())
    }

    #[test]
    fn adversarial_delimiter_runs() {
        let max = MAX_MARKDOWN_BYTES;
        timed("*", &"*".repeat(500_000));
        timed("_", &"_".repeat(500_000));
        timed("~~", &fill("~~", max));
        timed("*a", &fill("*a", max));
        timed("a*", &fill("a*", max));
        timed("*a *", &fill("*a **", max));
        timed("_a_", &fill("_a_", max));
        timed("_a_ ", &fill("_a_ ", max));
        timed("*_", &fill("*_", max));
        timed("**a*", &fill("**a*", max));
        timed("~~a~~ ", &fill("~~a~~ ", max));
        timed("* a", &fill("* a", max));
        // A huge run of openers closed by a huge run.
        let src = format!("{}a{}", "*".repeat(250_000), "*".repeat(250_000));
        timed("open/close", &src);
        let src = format!("{}a{}", "_".repeat(250_000), "_".repeat(250_000));
        timed("open/close _", &src);
    }

    #[test]
    fn adversarial_brackets_and_destinations() {
        let max = MAX_MARKDOWN_BYTES;
        timed("[", &"[".repeat(500_000));
        timed("]", &"]".repeat(500_000));
        timed("](", &fill("](", max));
        timed("[](", &fill("[](", max));
        timed("[a](", &fill("[a](", max));
        timed("[a](x", &fill("[a](x", max));
        timed("[a](<", &fill("[a](<", max));
        timed("[a](x \"", &fill("[a](x \"", max));
        timed("![", &fill("![", max));
        timed("![[[", &fill("![[[", max));
        timed("![a](", &fill("![a](", max));
        timed("[![", &fill("[![", max));
        timed("[a](b)", &fill("[a](b)", max));
        timed("[*", &fill("[*", max));
        timed("*[a](b)*", &fill("*[a](b)*", max));
        timed("<https:", &fill("<https:", max));
        timed("<", &"<".repeat(500_000));
        timed("<https://a", &fill("<https://a", max));
        timed("long dest", &format!("[a](x{}", "(".repeat(500_000)));
        timed("long dest 2", &format!("[a](<{}", "x".repeat(500_000)));
        timed("long title", &format!("[a](x \"{}", "x".repeat(500_000)));
        timed(
            "nested links",
            &format!(
                "{}x{}",
                "[a](https://x.example) [".repeat(20_000),
                "](y)".repeat(20_000)
            ),
        );
    }

    #[test]
    fn adversarial_code_spans_and_backslashes() {
        let max = MAX_MARKDOWN_BYTES;
        timed("`", &"`".repeat(500_000));
        timed("` `", &fill("` ", max));
        timed("`a`", &fill("`a`", max));
        timed("`` `", &fill("`` `", max));
        timed("growing runs", &{
            let mut s = String::new();
            let mut k = 1;
            while s.len() < max {
                s.push_str(&"`".repeat(k));
                s.push('a');
                k += 1;
            }
            s
        });
        timed("growing then shrinking", &{
            let mut s = String::new();
            let mut k = 1;
            while s.len() < max / 2 {
                s.push_str(&"`".repeat(k));
                s.push(' ');
                k += 1;
            }
            while k > 0 && s.len() < max {
                s.push_str(&"`".repeat(k));
                s.push(' ');
                k -= 1;
            }
            s
        });
        timed("\\", &"\\".repeat(500_000));
        timed("\\`", &fill("\\`", max));
        timed("`\\", &fill("`\\", max));
    }

    #[test]
    fn adversarial_blocks() {
        let max = MAX_MARKDOWN_BYTES;
        timed("> x200k", &"> ".repeat(200_000));
        timed(">", &">".repeat(500_000));
        timed(">\\n", &">\n".repeat(250_000));
        timed("> a\\n", &"> a\n".repeat(100_000));
        timed(
            "> > >\\n",
            &"> > > > > > > > > > > > > > > > > > > > a\n".repeat(10_000),
        );
        timed("lazy", &format!("> a\n{}", "b\n".repeat(250_000)));
        timed("- x200k", &"- ".repeat(200_000));
        timed("- -a", &"- -a".repeat(100_000));
        timed("1. x", &"1. ".repeat(160_000));
        timed("- a\\n", &"- a\n".repeat(120_000));
        timed("*\\n", &"*\n".repeat(250_000));
        timed("-\\n", &"-\n".repeat(250_000));
        timed("blank", &"\n".repeat(max));
        timed("spaces", &" ".repeat(max));
        timed("spaces nl", &" \n".repeat(250_000));
        timed("tabs", &"\t".repeat(max));
        timed("#", &"#".repeat(500_000));
        timed("# #", &"# ".repeat(250_000));
        timed("#\\n", &"#\n".repeat(250_000));
        timed("---", &"---\n".repeat(120_000));
        timed("a\\n---", &"a\n---\n".repeat(80_000));
        timed("```", &"```".repeat(160_000));
        timed("```\\n", &"```\n".repeat(120_000));
        timed("~~~ a", &"~~~ a\n".repeat(80_000));
        timed("unclosed fence", &format!("```\n{}", "x\n".repeat(250_000)));
        timed("indented", &"    a\n".repeat(80_000));
        timed("a\\n", &"a\n".repeat(250_000));
        timed("a  \\n", &"a  \n".repeat(150_000));
        timed("a\\\\n", &"a\\\n".repeat(200_000));
        timed("nested list indentation", &{
            let mut s = String::new();
            let mut i = 0;
            while s.len() < max {
                s.push_str(&" ".repeat(i * 2));
                s.push_str("- x\n");
                i += 1;
            }
            s
        });
        timed(
            "deep indentation then marker",
            &format!("{}- x\n", " ".repeat(500_000)),
        );
        timed(
            "quote of lists of quotes",
            &"> - > - > - > - > - > - > - > - > - > - > - > - > - > - > - > - > - > - x\n"
                .repeat(7000),
        );
        timed(
            "whitespace line after whitespace line",
            &"   \n    \n\t\n".repeat(60_000),
        );
    }

    #[test]
    fn adversarial_tables() {
        let max = MAX_MARKDOWN_BYTES;
        let (d, _) = timed(
            "100k rows",
            &format!("|a|b|\n|-|-|\n{}", "|x|y|\n".repeat(80_000)),
        );
        assert!(d.truncated);
        timed("100k cells", &format!("{}\n|-|\n|a|", "|a".repeat(200_000)));
        timed(
            "100k delimiter cells",
            &format!("|a|\n{}\n|a|", "|-".repeat(200_000)),
        );
        timed("pipes", &"|".repeat(500_000));
        timed("pipes nl", &"|\n".repeat(250_000));
        timed("table header repeats", &"|a|\n|-|\n".repeat(60_000));
        timed(
            "cells with brackets",
            &format!("|a|\n|-|\n{}", "|[a](b)|\n".repeat(50_000)),
        );
        timed(
            "one wide row",
            &format!("|a|b|\n|-|-|\n{}", "x|".repeat(max / 4)),
        );
        timed("pipes and dashes", &fill("|-", max));
        timed(
            "long cell",
            &format!("|a|\n|-|\n|{}|", "*a ".repeat(100_000)),
        );
    }

    #[test]
    fn adversarial_mixed_and_random_large() {
        let mut rng = Rng(0x1234_5678_9ABC_DEF1);
        for round in 0..6 {
            let mut s = String::new();
            while s.len() < MAX_MARKDOWN_BYTES + 1000 {
                s.push_str(FRAGMENTS[rng.below(FRAGMENTS.len())]);
            }
            let (d, _) = timed(&format!("random large {round}"), &s);
            assert!(d.truncated);
        }
        // Many small paragraphs, each at the delimiter cap.
        let para = format!("{}\n\n", "*a ".repeat(2500));
        timed(
            "many paragraphs at the delimiter cap",
            &para.repeat(MAX_MARKDOWN_BYTES / para.len()),
        );
        let para = format!("{}\n\n", "[a ".repeat(2500));
        timed(
            "many paragraphs of brackets",
            &para.repeat(MAX_MARKDOWN_BYTES / para.len()),
        );
        let para = format!("{}\n\n", "[a](b ".repeat(1500));
        timed(
            "many paragraphs of broken links",
            &para.repeat(MAX_MARKDOWN_BYTES / para.len()),
        );
    }

    #[test]
    fn delimiter_cap_leaves_literal_text() {
        // Past the cap, delimiters are literal and no character is lost.
        let src = "*a ".repeat(MAX_DELIMS + 100);
        let d = doc(&src);
        let mut s = String::new();
        fn collect(v: &Value, s: &mut String) {
            match v {
                Value::Object(m) => {
                    if let Some(t) = m.get("s").and_then(Value::as_str) {
                        s.push_str(t);
                    }
                    for x in m.values() {
                        collect(x, s);
                    }
                }
                Value::Array(a) => a.iter().for_each(|x| collect(x, s)),
                _ => {}
            }
        }
        collect(&d.to_value(), &mut s);
        assert_eq!(s.trim_end(), src.trim_end());
    }

    #[test]
    fn text_keeps_every_character() {
        // Without structure, the text of a paragraph is the paragraph (breaks aside).
        let src = "a\u{202e}b\u{1b}[0m\u{7f}\0 c   d\t e";
        assert_eq!(canon(src), arr(&[para(&text(src))]));
        assert_eq!(canon("日本語 😀"), arr(&[para(&text("日本語 😀"))]));
    }

    #[test]
    fn value_numbers_are_integers() {
        let d = doc("# a\n\n3. x");
        let v = d.to_value();
        let h = v.as_array().unwrap().first().unwrap();
        assert_eq!(h.get("level"), Some(&Value::int(1)));
        let ol = v.as_array().unwrap().get(1).unwrap();
        assert_eq!(ol.get("start"), Some(&Value::int(3)));
    }

    /// Link and image targets from random pieces that are (or imitate) schemes, escapes and paths:
    /// whatever comes out is an http(s) URL that passed the policy, a path inside the repository, or
    /// nothing loadable.
    #[test]
    fn property_random_targets_never_become_dangerous_links() {
        let pieces = [
            "javascript:",
            "JaVaScRiPt:",
            "java%73cript:",
            "%6Aavascript:",
            "data:",
            "vbscript:",
            "file:",
            "http:",
            "https:",
            "HTTP://",
            "//",
            "/",
            "../",
            "./",
            "..",
            "%2e%2e/",
            "%00",
            "%0a",
            "%09",
            "\\",
            " ",
            "\t",
            "\n",
            "\u{202e}",
            "\u{200b}",
            "\u{1b}",
            "alert(1)",
            "x.png",
            "a b",
            "host.example",
            "@",
            ":",
            "#",
            "?",
            "%",
            "%zz",
            "%e9",
            "\u{e9}",
            "<",
            ">",
            "\"",
            "'",
        ];
        let dir = RepoPath::parse("docs/sub").unwrap();
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..30_000 {
            let n = 1 + next() % 6;
            let mut t = String::new();
            for _ in 0..n {
                t.push_str(pieces[usize::try_from(next() % pieces.len() as u64).unwrap()]);
            }
            if let Some(target) = classify_link(&t, &dir) {
                match target {
                    LinkTarget::External(u) => {
                        let l = u.to_ascii_lowercase();
                        assert!(
                            l.starts_with("http://") || l.starts_with("https://"),
                            "{t:?} -> {u:?}"
                        );
                        assert!(
                            u.chars()
                                .all(|c| !c.is_control() && !c.is_whitespace() && c != '\\'),
                            "{u:?}"
                        );
                    }
                    LinkTarget::Repo(p) => {
                        assert!(RepoPath::parse(p.as_str()).is_ok(), "{t:?}");
                        assert!(
                            scheme_of(t.trim()).is_none() && !decoded_has_scheme(t.trim()),
                            "{t:?}"
                        );
                    }
                }
            }
            let (path, src, note) = classify_image(&t, &dir);
            assert!(src.chars().count() <= MAX_SRC_CHARS);
            match (&path, note) {
                (Some(p), None) => assert!(RepoPath::parse(p.as_str()).is_ok(), "{t:?}"),
                (None, Some(_)) => {}
                other => panic!("an image has a path or a note: {other:?} for {t:?}"),
            }
        }
    }
}
