//! Streaming sanitizer for pty output.
//!
//! Worker output is untrusted: it can contain escape sequences that rewrite the screen, set the
//! terminal title, write to the clipboard (OSC 52), make the terminal answer back into the input
//! (DSR, DA, window reports), and Unicode controls that reorder or hide text. Two modes:
//!
//! * [`Mode::Text`] produces plain text for the dashboard, the tail command, the on-disk logs and
//!   the audit log. Every escape sequence is removed; other control characters and invisible
//!   Unicode are replaced by a visible `\xNN` / `\u{...}` token.
//! * [`Mode::Passthrough`] is used by `attach`. The user wants the agent's full-screen UI, so
//!   colors, cursor movement and a short list of harmless private modes pass; OSC, DCS, APC, PM,
//!   SOS, terminal queries and everything not on the allowlist are dropped.
//!
//! Both modes are byte-stream state machines: a sequence split across two reads is handled the
//! same as one that arrives whole, and no sequence can grow past [`MAX_SEQUENCE`] bytes.

use crate::ids::is_invisible;
use std::fmt::Write as _;

/// Longest CSI or string sequence tracked before it is abandoned.
pub const MAX_SEQUENCE: usize = 4096;
const MAX_CSI: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Text,
    Passthrough,
}

#[derive(Debug)]
enum State {
    Ground,
    Esc,
    /// ESC followed by intermediate bytes (charset designation and friends).
    EscInter(Vec<u8>),
    Csi(Vec<u8>),
    /// OSC, DCS, APC, PM or SOS payload being swallowed.
    Str {
        len: usize,
        esc: bool,
        c2: bool,
    },
}

#[derive(Debug)]
pub struct Sanitizer {
    mode: Mode,
    state: State,
    utf8: [u8; 4],
    utf8_have: usize,
    utf8_need: usize,
    after_cr: bool,
    /// Escape sequences removed or filtered so far.
    pub stripped: u64,
}

impl Sanitizer {
    pub fn new(mode: Mode) -> Self {
        Sanitizer {
            mode,
            state: State::Ground,
            utf8: [0; 4],
            utf8_have: 0,
            utf8_need: 0,
            after_cr: false,
            stripped: 0,
        }
    }

    /// Feeds raw bytes, appending sanitized text to `out`.
    pub fn feed(&mut self, bytes: &[u8], out: &mut String) {
        for &b in bytes {
            self.step(b, out);
        }
    }

    /// Ends the stream: a half-received multibyte character becomes U+FFFD, a half-received escape
    /// sequence is dropped.
    pub fn finish(&mut self, out: &mut String) {
        if self.utf8_need > 0 {
            self.utf8_need = 0;
            self.utf8_have = 0;
            out.push('\u{fffd}');
        }
        if !matches!(self.state, State::Ground) {
            self.stripped = self.stripped.saturating_add(1);
            self.state = State::Ground;
        }
    }

    fn step(&mut self, b: u8, out: &mut String) {
        match &mut self.state {
            State::Ground => self.ground(b, out),
            State::Esc => self.after_esc(b, out),
            State::EscInter(buf) => {
                if (0x20..=0x2f).contains(&b) {
                    if buf.len() >= 4 {
                        self.abort_sequence();
                    } else {
                        buf.push(b);
                    }
                } else if (0x30..=0x7e).contains(&b) {
                    let inter = std::mem::take(buf);
                    self.state = State::Ground;
                    self.finish_esc_inter(&inter, b, out);
                } else {
                    self.abort_sequence();
                    self.step(b, out);
                }
            }
            State::Csi(buf) => {
                if b == 0x1b {
                    self.abort_sequence();
                    self.step(b, out);
                } else if (0x30..=0x3f).contains(&b) || (0x20..=0x2f).contains(&b) {
                    if buf.len() >= MAX_CSI {
                        self.abort_sequence();
                    } else {
                        buf.push(b);
                    }
                } else if (0x40..=0x7e).contains(&b) {
                    let body = std::mem::take(buf);
                    self.state = State::Ground;
                    self.finish_csi(&body, b, out);
                } else {
                    self.abort_sequence();
                    self.step(b, out);
                }
            }
            State::Str { len, esc, c2 } => {
                *len += 1;
                let terminated = if *esc {
                    if b == b'\\' {
                        true
                    } else {
                        // ESC inside a string starts a new escape sequence.
                        self.state = State::Esc;
                        self.stripped = self.stripped.saturating_add(1);
                        self.step(b, out);
                        return;
                    }
                } else if b == 0x07 {
                    true
                } else if b == 0x18 || b == 0x1a {
                    // CAN and SUB cancel the string.
                    true
                } else if *c2 && b == 0x9c {
                    true
                } else {
                    *esc = b == 0x1b;
                    *c2 = b == 0xc2;
                    if *len > MAX_SEQUENCE {
                        self.abort_sequence();
                    }
                    return;
                };
                if terminated {
                    self.stripped = self.stripped.saturating_add(1);
                    self.state = State::Ground;
                }
            }
        }
    }

    fn abort_sequence(&mut self) {
        self.stripped = self.stripped.saturating_add(1);
        self.state = State::Ground;
    }

    fn after_esc(&mut self, b: u8, out: &mut String) {
        match b {
            b'[' => self.state = State::Csi(Vec::new()),
            b']' | b'P' | b'_' | b'^' | b'X' => {
                self.state = State::Str {
                    len: 0,
                    esc: false,
                    c2: false,
                }
            }
            0x20..=0x2f => self.state = State::EscInter(vec![b]),
            0x1b => {} // ESC ESC: stay in Esc
            0x30..=0x7e => {
                self.state = State::Ground;
                self.finish_esc_inter(&[], b, out);
            }
            _ => {
                self.abort_sequence();
                self.step(b, out);
            }
        }
    }

    fn finish_esc_inter(&mut self, inter: &[u8], fin: u8, out: &mut String) {
        let allowed = match self.mode {
            Mode::Text => false,
            Mode::Passthrough => match (inter, fin) {
                // save/restore cursor, reverse index, keypad modes
                ([], b'7' | b'8' | b'M' | b'=' | b'>') => true,
                // ASCII and DEC line-drawing character sets
                ([b'(' | b')'], b'B' | b'0') => true,
                _ => false,
            },
        };
        if allowed {
            out.push('\u{1b}');
            for &i in inter {
                out.push(i as char);
            }
            out.push(fin as char);
        } else {
            self.stripped = self.stripped.saturating_add(1);
        }
    }

    fn finish_csi(&mut self, body: &[u8], fin: u8, out: &mut String) {
        if self.mode == Mode::Passthrough && csi_allowed(body, fin) {
            out.push('\u{1b}');
            out.push('[');
            for &c in body {
                out.push(c as char);
            }
            out.push(fin as char);
        } else {
            self.stripped = self.stripped.saturating_add(1);
        }
    }

    fn ground(&mut self, b: u8, out: &mut String) {
        if self.utf8_need > 0 {
            if b & 0xc0 == 0x80 {
                if let Some(slot) = self.utf8.get_mut(self.utf8_have) {
                    *slot = b;
                }
                self.utf8_have += 1;
                self.utf8_need -= 1;
                if self.utf8_need == 0 {
                    let have = self.utf8_have;
                    self.utf8_have = 0;
                    match self
                        .utf8
                        .get(..have)
                        .and_then(|bytes| std::str::from_utf8(bytes).ok())
                        .and_then(|s| s.chars().next())
                    {
                        Some(c) => self.emit_char(c, out),
                        None => out.push('\u{fffd}'),
                    }
                }
                return;
            }
            // Not a continuation byte: the pending character was truncated.
            self.utf8_need = 0;
            self.utf8_have = 0;
            out.push('\u{fffd}');
        }
        match b {
            0x1b => {
                self.after_cr = false;
                self.state = State::Esc;
            }
            0x00..=0x7f => self.emit_char(b as char, out),
            0xc2..=0xdf => self.start_utf8(b, 1),
            0xe0..=0xef => self.start_utf8(b, 2),
            0xf0..=0xf4 => self.start_utf8(b, 3),
            _ => out.push('\u{fffd}'),
        }
    }

    fn start_utf8(&mut self, b: u8, need: usize) {
        self.utf8[0] = b;
        self.utf8_have = 1;
        self.utf8_need = need;
    }

    fn emit_char(&mut self, c: char, out: &mut String) {
        let was_cr = std::mem::replace(&mut self.after_cr, false);
        match self.mode {
            Mode::Text => match c {
                '\n' => {
                    if !was_cr {
                        out.push('\n');
                    }
                }
                // A bare carriage return would let later text overwrite earlier text on a real
                // terminal; as a line break nothing the worker printed can be hidden.
                '\r' => {
                    out.push('\n');
                    self.after_cr = true;
                }
                '\t' => out.push('\t'),
                c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                    let _ = write!(out, "\\x{:02x}", c as u32);
                }
                c if (0x80..=0x9f).contains(&(c as u32)) || is_invisible(c) => {
                    let _ = write!(out, "\\u{{{:x}}}", c as u32);
                }
                c => out.push(c),
            },
            Mode::Passthrough => match c {
                '\n' | '\r' | '\t' | '\u{8}' => out.push(c),
                c if (c as u32) < 0x20
                    || (0x7f..=0x9f).contains(&(c as u32))
                    || is_invisible(c) => {}
                c => out.push(c),
            },
        }
    }
}

/// Allowlist for CSI sequences in passthrough mode.
fn csi_allowed(body: &[u8], fin: u8) -> bool {
    let (private, rest) = match body.split_first() {
        Some((&p, tail)) if matches!(p, b'<' | b'=' | b'>' | b'?') => (Some(p), tail),
        _ => (None, body),
    };
    let split = rest
        .iter()
        .position(|b| (0x20..=0x2f).contains(b))
        .unwrap_or(rest.len());
    let (params, inter) = rest.split_at(split);
    if !params
        .iter()
        .all(|b| b.is_ascii_digit() || *b == b';' || *b == b':')
    {
        return false;
    }
    match (private, inter, fin) {
        (None, [], b'm') => true,
        (
            None,
            [],
            b'A'..=b'H'
            | b'f'
            | b'J'
            | b'K'
            | b'L'
            | b'M'
            | b'P'
            | b'S'
            | b'T'
            | b'X'
            | b'@'
            | b'd'
            | b'r',
        ) => true,
        (None, [], b's' | b'u') => params.is_empty(),
        // cursor style (DECSCUSR)
        (None, [b' '], b'q') => true,
        (Some(b'?'), [], b'h' | b'l') => {
            !params.is_empty()
                && params.split(|b| *b == b';').all(|p| {
                    std::str::from_utf8(p)
                        .ok()
                        .and_then(|s| s.parse::<u32>().ok())
                        .is_some_and(|n| {
                            matches!(
                                n,
                                1 | 7
                                    | 12
                                    | 25
                                    | 47
                                    | 1000
                                    | 1002
                                    | 1003
                                    | 1004
                                    | 1006
                                    | 1047
                                    | 1048
                                    | 1049
                                    | 2004
                                    | 2026
                            )
                        })
                })
        }
        _ => false,
    }
}

/// One-shot sanitization of a complete byte string in text mode.
pub fn sanitize_text(bytes: &[u8]) -> String {
    let mut s = Sanitizer::new(Mode::Text);
    let mut out = String::with_capacity(bytes.len());
    s.feed(bytes, &mut out);
    s.finish(&mut out);
    out
}

/// Keeps at most `max` bytes from the end of `s`, on a character boundary.
pub fn tail_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Keeps at most `max` bytes from the start of `s`, on a character boundary.
pub fn head_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(input: &[u8]) -> String {
        sanitize_text(input)
    }

    fn pass(input: &[u8]) -> String {
        let mut s = Sanitizer::new(Mode::Passthrough);
        let mut out = String::new();
        s.feed(input, &mut out);
        s.finish(&mut out);
        out
    }

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(text(b"hello\nworld\ttab"), "hello\nworld\ttab");
        assert_eq!(text("héllo ✓ 日本".as_bytes()), "héllo ✓ 日本");
    }

    #[test]
    fn colors_and_cursor_moves_are_removed_in_text_mode() {
        assert_eq!(text(b"\x1b[31mred\x1b[0m \x1b[2J\x1b[1;1Hx"), "red x");
    }

    #[test]
    fn osc_title_and_clipboard_are_removed() {
        assert_eq!(text(b"a\x1b]0;evil title\x07b"), "ab");
        assert_eq!(text(b"a\x1b]52;c;ZXZpbA==\x1b\\b"), "ab");
        assert_eq!(text(b"a\x1b]8;;http://x\x07link\x1b]8;;\x07b"), "alinkb");
    }

    #[test]
    fn dcs_apc_pm_sos_are_removed() {
        assert_eq!(text(b"a\x1bPq#0;2;0;0;0\x1b\\b"), "ab");
        assert_eq!(text(b"a\x1b_Gi=1;AAAA\x1b\\b"), "ab");
        assert_eq!(text(b"a\x1b^private\x1b\\b"), "ab");
        assert_eq!(text(b"a\x1bXsos\x1b\\b"), "ab");
    }

    #[test]
    fn split_sequences_across_chunks() {
        let mut s = Sanitizer::new(Mode::Text);
        let mut out = String::new();
        for chunk in [&b"x\x1b"[..], b"[3", b"1mre", b"d\x1b]0;ti", b"tle\x07!"] {
            s.feed(chunk, &mut out);
        }
        s.finish(&mut out);
        assert_eq!(out, "xred!");
    }

    #[test]
    fn unterminated_osc_is_capped_and_text_resumes() {
        let mut input = b"\x1b]0;".to_vec();
        input.extend(std::iter::repeat_n(b'A', MAX_SEQUENCE * 2));
        let out = text(&input);
        assert!(out.len() < MAX_SEQUENCE * 2);
        assert!(out.starts_with('A'));
    }

    #[test]
    fn control_characters_become_visible_tokens() {
        assert_eq!(text(b"a\x07b\x00c\x7fd"), "a\\x07b\\x00c\\x7fd");
        assert_eq!(text("a\u{9b}b".as_bytes()), "a\\u{9b}b");
    }

    #[test]
    fn bidi_and_invisible_characters_are_escaped() {
        assert_eq!(text("a\u{202e}b".as_bytes()), "a\\u{202e}b");
        assert_eq!(
            text("a\u{2066}b\u{2069}".as_bytes()),
            "a\\u{2066}b\\u{2069}"
        );
        assert_eq!(text("a\u{200b}b".as_bytes()), "a\\u{200b}b");
        assert_eq!(text("a\u{feff}b".as_bytes()), "a\\u{feff}b");
        assert_eq!(text("a\u{e0041}b".as_bytes()), "a\\u{e0041}b");
    }

    #[test]
    fn carriage_return_cannot_hide_text() {
        assert_eq!(text(b"secret\rinnocent\n"), "secret\ninnocent\n");
        assert_eq!(text(b"line\r\nnext"), "line\nnext");
        assert_eq!(text(b"a\r\r\nb"), "a\n\nb");
    }

    #[test]
    fn invalid_utf8_becomes_replacement_character() {
        assert_eq!(text(b"a\xffb"), "a\u{fffd}b");
        assert_eq!(text(b"a\xe2\x82"), "a\u{fffd}");
        assert_eq!(text(b"a\xc0\xafb"), "a\u{fffd}\u{fffd}b");
        assert_eq!(text(b"\xed\xa0\x80"), "\u{fffd}");
    }

    #[test]
    fn multibyte_split_across_chunks() {
        let mut s = Sanitizer::new(Mode::Text);
        let mut out = String::new();
        let bytes = "日".as_bytes();
        s.feed(&bytes[..1], &mut out);
        s.feed(&bytes[1..], &mut out);
        assert_eq!(out, "日");
    }

    #[test]
    fn esc_followed_by_junk_is_dropped_but_text_survives() {
        assert_eq!(text(b"a\x1b\x01b"), "a\\x01b");
        assert_eq!(text(b"a\x1b(Bb"), "ab");
        assert_eq!(text(b"a\x1bcb"), "ab");
    }

    #[test]
    fn output_never_contains_escape_or_c1() {
        let mut input = Vec::new();
        for b in 0u8..=255 {
            input.extend_from_slice(&[b, 0x1b, b, b'[', b, b']', b]);
        }
        let out = text(&input);
        assert!(!out.contains('\u{1b}'));
        assert!(!out.chars().any(|c| (0x80..=0x9f).contains(&(c as u32))));
        assert!(!out
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t'));
    }

    #[test]
    fn passthrough_keeps_colors_and_cursor_motion() {
        assert_eq!(
            pass(b"\x1b[1;31mhi\x1b[0m\x1b[2;3H\x1b[K"),
            "\x1b[1;31mhi\x1b[0m\x1b[2;3H\x1b[K"
        );
        assert_eq!(pass(b"\x1b[?25l\x1b[?1049h"), "\x1b[?25l\x1b[?1049h");
    }

    #[test]
    fn passthrough_drops_queries_osc_and_window_ops() {
        assert_eq!(pass(b"a\x1b]52;c;AAAA\x07b"), "ab");
        assert_eq!(pass(b"a\x1b[6nb"), "ab");
        assert_eq!(pass(b"a\x1b[cb"), "ab");
        assert_eq!(pass(b"a\x1b[21tb"), "ab");
        assert_eq!(pass(b"a\x1b[>0cb"), "ab");
        assert_eq!(pass(b"a\x1b[?9999hb"), "ab");
        assert_eq!(pass(b"a\x1b[1;2xb"), "ab");
        assert_eq!(pass(b"a\x1b[1;2$pb"), "ab");
        assert_eq!(pass(b"a\x1bcb"), "ab");
    }

    #[test]
    fn passthrough_drops_bidi_bell_and_c1() {
        assert_eq!(pass("a\u{202e}b\u{7}c\u{9b}d".as_bytes()), "abcd");
        assert_eq!(pass(b"a\r\nb\tc\x08"), "a\r\nb\tc\x08");
    }

    #[test]
    fn passthrough_param_garbage_is_rejected() {
        assert_eq!(pass(b"a\x1b[1;$mb"), "ab");
        assert_eq!(pass(b"a\x1b[;;;;;;;;mb"), "a\x1b[;;;;;;;;mb");
    }

    #[test]
    fn stripped_counter_counts_sequences() {
        let mut s = Sanitizer::new(Mode::Text);
        let mut out = String::new();
        s.feed(b"\x1b[1m\x1b]0;x\x07\x1bc", &mut out);
        assert_eq!(s.stripped, 3);
    }

    #[test]
    fn head_and_tail_respect_char_boundaries() {
        assert_eq!(tail_str("aé", 1), "");
        assert_eq!(tail_str("aé", 2), "é");
        assert_eq!(head_str("éa", 1), "");
        assert_eq!(head_str("éa", 3), "éa");
    }
}
