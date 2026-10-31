//! Bounded capture of worker output.
//!
//! Raw pty bytes go in; sanitized, redacted text lines come out, kept in a ring buffer with a hard
//! byte cap. Memory per session is therefore constant no matter how much a worker prints.
//! Offsets are absolute (they only ever grow) so a follower can ask "everything since offset N"
//! and learn when output was lost because it fell out of the ring.

use crate::limits::MAX_PARTIAL_LINE;
use crate::redact::{Redactor, StreamRedactor};
use crate::sanitize::{head_str, Mode, Sanitizer};
use std::collections::VecDeque;
use std::sync::Arc;

/// Fixed-capacity byte ring with absolute offsets.
#[derive(Debug)]
pub struct ByteRing {
    buf: VecDeque<u8>,
    cap: usize,
    /// Offset one past the last byte ever pushed.
    end: u64,
}

impl ByteRing {
    pub fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        ByteRing {
            buf: VecDeque::with_capacity(cap),
            cap,
            end: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Offset of the oldest retained byte.
    pub fn start_offset(&self) -> u64 {
        self.end.saturating_sub(self.buf.len() as u64)
    }

    pub fn end_offset(&self) -> u64 {
        self.end
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.end = self.end.saturating_add(bytes.len() as u64);
        if bytes.len() >= self.cap {
            self.buf.clear();
            let keep = bytes.get(bytes.len() - self.cap..).unwrap_or(bytes);
            self.buf.extend(keep.iter().copied());
            return;
        }
        let overflow = (self.buf.len() + bytes.len()).saturating_sub(self.cap);
        if overflow > 0 {
            self.buf.drain(..overflow);
        }
        self.buf.extend(bytes.iter().copied());
    }

    /// Copies up to `max` bytes starting at `offset` (clamped to the oldest retained byte).
    /// Returns the offset actually served from and the bytes.
    pub fn read_from(&self, offset: u64, max: usize) -> (u64, Vec<u8>) {
        let start = self.start_offset();
        let from = offset.max(start).min(self.end);
        let skip = usize::try_from(from - start).unwrap_or(usize::MAX);
        let bytes: Vec<u8> = self.buf.iter().skip(skip).take(max).copied().collect();
        (from, bytes)
    }
}

/// What a follower receives from [`OutputBuffer::read_since`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Offset the text starts at.
    pub from: u64,
    /// Offset to pass next time.
    pub next: u64,
    pub text: String,
    /// Output between the requested offset and `from` was overwritten before it was read.
    pub lost: bool,
}

#[derive(Debug)]
pub struct OutputBuffer {
    sanitizer: Sanitizer,
    redactor: StreamRedactor,
    partial: String,
    scratch: String,
    ring: ByteRing,
    raw_bytes: u64,
}

impl OutputBuffer {
    pub fn new(cap: usize, redactor: Arc<Redactor>) -> Self {
        OutputBuffer {
            sanitizer: Sanitizer::new(Mode::Text),
            redactor: StreamRedactor::new(redactor),
            partial: String::new(),
            scratch: String::new(),
            ring: ByteRing::new(cap),
            raw_bytes: 0,
        }
    }

    /// Feeds raw terminal bytes.
    pub fn push(&mut self, raw: &[u8]) {
        self.raw_bytes = self.raw_bytes.saturating_add(raw.len() as u64);
        self.scratch.clear();
        self.sanitizer.feed(raw, &mut self.scratch);
        let text = std::mem::take(&mut self.scratch);
        let mut rest = text.as_str();
        while let Some(nl) = rest.find('\n') {
            let (line, tail) = rest.split_at(nl);
            self.partial.push_str(line);
            self.emit_partial_as_line();
            rest = tail.strip_prefix('\n').unwrap_or(tail);
        }
        self.partial.push_str(rest);
        self.cut_long_partial();
        self.scratch = text;
    }

    /// Ends the stream: a pending unterminated line is stored as a line.
    pub fn finish(&mut self) {
        self.scratch.clear();
        self.sanitizer.finish(&mut self.scratch);
        let tail = std::mem::take(&mut self.scratch);
        self.partial.push_str(&tail);
        if !self.partial.is_empty() {
            self.emit_partial_as_line();
        }
    }

    fn emit_partial_as_line(&mut self) {
        let line = std::mem::take(&mut self.partial);
        if let Some(clean) = self.redactor.line(&line) {
            self.ring.push(clean.as_bytes());
            self.ring.push(b"\n");
        }
    }

    fn cut_long_partial(&mut self) {
        while self.partial.len() >= MAX_PARTIAL_LINE {
            let limit = head_str(&self.partial, MAX_PARTIAL_LINE).len();
            let window = head_str(&self.partial, limit);
            // Cut after whitespace when possible so a secret is not split in half.
            let cut = match window.rfind(char::is_whitespace) {
                Some(pos) if pos >= limit / 2 => pos + 1,
                _ => limit,
            };
            let rest = self.partial.split_off(cut);
            self.emit_partial_as_line();
            self.partial = rest;
        }
    }

    /// Bytes of raw terminal output received so far (before sanitizing).
    pub fn raw_bytes(&self) -> u64 {
        self.raw_bytes
    }

    /// Escape sequences removed so far.
    pub fn stripped_sequences(&self) -> u64 {
        self.sanitizer.stripped
    }

    pub fn retained_bytes(&self) -> usize {
        self.ring.len()
    }

    pub fn capacity(&self) -> usize {
        self.ring.capacity()
    }

    pub fn end_offset(&self) -> u64 {
        self.ring.end_offset()
    }

    /// Complete lines stored after `offset`, at most `max` bytes.
    pub fn read_since(&self, offset: u64, max: usize) -> Chunk {
        let (from, bytes) = self.ring.read_from(offset, max);
        let text = clean_utf8(&bytes);
        let next = from.saturating_add(bytes.len() as u64);
        Chunk {
            from,
            next,
            // `from > offset` can only happen when the ring overwrote unread data.
            lost: from > offset,
            text,
        }
    }

    /// The last `max` bytes of output including the unterminated current line (unredacted
    /// partial lines are redacted on the fly).
    pub fn snapshot(&self, max: usize) -> String {
        let start = self.ring.end_offset().saturating_sub(max as u64);
        let (_, bytes) = self.ring.read_from(start, max);
        let mut text = clean_utf8(&bytes);
        if !self.partial.is_empty() {
            let shown = self.redactor.peek(&self.partial);
            text.push_str(&shown);
        }
        crate::sanitize::tail_str(&text, max).to_string()
    }
}

/// Drops a leading partial character and a trailing incomplete one left by cutting at byte offsets.
fn clean_utf8(bytes: &[u8]) -> String {
    let start = bytes
        .iter()
        .take(4)
        .position(|b| b & 0xc0 != 0x80)
        .unwrap_or(bytes.len().min(4));
    let body = bytes.get(start..).unwrap_or(&[]);
    match std::str::from_utf8(body) {
        Ok(s) => s.to_string(),
        Err(e) => {
            let valid = body.get(..e.valid_up_to()).unwrap_or(&[]);
            String::from_utf8_lossy(valid).into_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(cap: usize) -> OutputBuffer {
        OutputBuffer::new(cap, Arc::new(Redactor::builtin()))
    }

    #[test]
    fn ring_keeps_only_the_last_cap_bytes() {
        let mut r = ByteRing::new(8);
        r.push(b"abcdef");
        r.push(b"ghij");
        assert_eq!(r.len(), 8);
        assert_eq!(r.start_offset(), 2);
        assert_eq!(r.end_offset(), 10);
        assert_eq!(r.read_from(0, 100), (2, b"cdefghij".to_vec()));
        assert_eq!(r.read_from(4, 3), (4, b"efg".to_vec()));
        assert_eq!(r.read_from(99, 3), (10, vec![]));
    }

    #[test]
    fn ring_handles_a_chunk_larger_than_its_capacity() {
        let mut r = ByteRing::new(4);
        r.push(b"0123456789");
        assert_eq!(r.read_from(0, 10), (6, b"6789".to_vec()));
        assert_eq!(r.end_offset(), 10);
        assert_eq!(r.len(), 4);
    }

    #[test]
    fn memory_stays_bounded_under_flood() {
        let mut b = buf(1024);
        let chunk = vec![b'x'; 8192];
        for _ in 0..2000 {
            b.push(&chunk);
            b.push(b"\n");
        }
        assert!(b.retained_bytes() <= 1024);
        assert!(b.raw_bytes() > 16_000_000);
    }

    #[test]
    fn lines_are_sanitized_and_stored() {
        let mut b = buf(4096);
        b.push(b"\x1b[31mred\x1b[0m line\r\nsecond\n");
        let c = b.read_since(0, 4096);
        assert_eq!(c.text, "red line\nsecond\n");
        assert!(!c.lost);
        assert_eq!(c.next, c.text.len() as u64);
    }

    #[test]
    fn secrets_are_redacted_before_storage() {
        let mut b = buf(4096);
        let tok = format!("ghp_{}", "Ab1Cd2Ef3G".repeat(4));
        b.push(format!("token is {tok}\n").as_bytes());
        let text = b.read_since(0, 4096).text;
        assert!(text.contains("[REDACTED:github-token]"));
        assert!(!text.contains("ghp_"));
    }

    #[test]
    fn secret_split_across_chunks_is_still_redacted() {
        let mut b = buf(4096);
        let tok = format!("ghp_{}", "Ab1Cd2Ef3G".repeat(4));
        let (a, c) = tok.split_at(10);
        b.push(format!("x {a}").as_bytes());
        b.push(format!("{c} y\n").as_bytes());
        let text = b.read_since(0, 4096).text;
        assert!(!text.contains("ghp_"), "{text}");
    }

    #[test]
    fn private_key_block_is_suppressed() {
        let mut b = buf(4096);
        b.push(b"-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY-----\nok\n");
        let text = b.read_since(0, 4096).text;
        assert_eq!(text, "[REDACTED:private-key]\nok\n");
    }

    #[test]
    fn overlong_line_is_flushed_in_pieces() {
        let mut b = buf(64 * 1024);
        let line = "word ".repeat(MAX_PARTIAL_LINE);
        b.push(line.as_bytes());
        assert!(b.retained_bytes() > 0);
        assert!(b.snapshot(100_000).len() >= MAX_PARTIAL_LINE);
    }

    #[test]
    fn overlong_line_without_spaces_is_cut_at_the_cap() {
        let mut b = buf(64 * 1024);
        b.push(&vec![b'a'; MAX_PARTIAL_LINE * 3 + 5]);
        let c = b.read_since(0, 1 << 20);
        assert_eq!(c.text.matches('\n').count(), 3);
    }

    #[test]
    fn follower_learns_about_lost_output() {
        let mut b = buf(16);
        b.push(b"0123456789\n");
        let first = b.read_since(0, 100);
        assert!(!first.lost);
        for _ in 0..5 {
            b.push(b"abcdefghi\n");
        }
        let second = b.read_since(first.next, 100);
        assert!(second.lost);
        assert!(second.from > first.next);
        assert!(second.text.len() <= 16);
    }

    #[test]
    fn follower_reads_incrementally() {
        let mut b = buf(4096);
        b.push(b"one\n");
        let a = b.read_since(0, 100);
        b.push(b"two\n");
        let c = b.read_since(a.next, 100);
        assert_eq!(a.text, "one\n");
        assert_eq!(c.text, "two\n");
    }

    #[test]
    fn snapshot_includes_unterminated_line() {
        let mut b = buf(4096);
        b.push(b"done\nContinue? [y/N] ");
        assert_eq!(b.snapshot(4096), "done\nContinue? [y/N] ");
    }

    #[test]
    fn snapshot_redacts_partial_lines() {
        let mut b = buf(4096);
        let tok = format!("ghp_{}", "Ab1Cd2Ef3G".repeat(4));
        b.push(format!("key={tok}").as_bytes());
        assert!(!b.snapshot(4096).contains("ghp_"));
    }

    #[test]
    fn finish_stores_the_last_unterminated_line() {
        let mut b = buf(4096);
        b.push(b"no newline at end");
        b.finish();
        assert_eq!(b.read_since(0, 4096).text, "no newline at end\n");
    }

    #[test]
    fn cut_inside_a_multibyte_character_yields_valid_text() {
        let mut b = buf(5);
        b.push("日本語\n".as_bytes());
        let c = b.read_since(0, 100);
        assert!(std::str::from_utf8(c.text.as_bytes()).is_ok());
        let s = b.snapshot(100);
        assert!(s.len() <= 5);
    }

    #[test]
    fn hostile_output_never_reaches_the_ring_raw() {
        let mut b = buf(4096);
        b.push(b"a\x1b]52;c;AAAA\x07b\x1b[2Jc\xe2\x80\xaed\x00\n");
        let text = b.read_since(0, 4096).text;
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\u{202e}'));
        assert!(text.starts_with("abc"));
        assert!(b.stripped_sequences() >= 2);
    }
}
