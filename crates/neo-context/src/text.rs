//! Making untrusted text safe to put into a pack: sanitized with the domain's sanitizer, redacted
//! with the daemon's redactor, clipped, and shaped so that it cannot start a line of its own.

use crate::secret;
use fleet_domain::redact::{Redactor, StreamRedactor};
use fleet_domain::sanitize::{flatten_line, head_str, sanitize_text, QUOTE_PREFIX};
use std::sync::Arc;

/// Text after cleaning, and whether cleaning cut something.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cleaned {
    pub text: String,
    pub clipped: bool,
}

/// Cleans text for a pack. Every string that reaches a layer goes through one of these.
#[derive(Clone, Debug)]
pub struct Hygiene {
    redactor: Arc<Redactor>,
}

/// A line that matches one of the secret rules is replaced whole: the redactor knows key prefixes and
/// the daemon's secret values, the rules know shapes it does not (a password in a URL, a quoted
/// `api_key = "..."` assignment, a JWT, in any case).
fn scrub(line: String) -> String {
    match secret::scan_line(&line) {
        Some(rule) => format!("[REDACTED: the line matches the secret rule {rule}]"),
        None => line,
    }
}

impl Hygiene {
    pub fn new(redactor: Arc<Redactor>) -> Hygiene {
        Hygiene { redactor }
    }

    /// A multi-line block: escape sequences removed, control and invisible characters turned into
    /// visible tokens, secrets redacted line by line (PEM blocks across lines), then clipped to
    /// `max` bytes on a character boundary. Redaction comes before the cut, so a key that straddles
    /// the cut is redacted whole rather than shown by half.
    pub fn block(&self, raw: &str, max: usize) -> Cleaned {
        let sanitized = sanitize_text(raw.as_bytes());
        let mut stream = StreamRedactor::new(Arc::clone(&self.redactor));
        let kept: Vec<String> = sanitized
            .split('\n')
            .filter_map(|line| stream.line(line).map(|l| scrub(l.trim_end().to_string())))
            .collect();
        let joined = kept.join("\n");
        let trimmed = joined.trim_matches('\n');
        let clipped = trimmed.len() > max;
        let mut text = head_str(trimmed, max).trim_end().to_string();
        if clipped {
            text.push_str(&format!("\n[clipped at {max} bytes]"));
        }
        Cleaned { text, clipped }
    }

    /// One line: like [`Hygiene::block`] but every line break and control character becomes a space
    /// first, so the text can neither start a line nor a heading wherever it is placed.
    pub fn line(&self, raw: &str, max: usize) -> String {
        let bounded = head_str(raw, max.saturating_mul(4));
        let sanitized = sanitize_text(bounded.as_bytes());
        let flat = flatten_line(&sanitized, usize::MAX / 4);
        let redacted = scrub(self.redactor.redact(&flat).into_owned());
        flatten_line(&redacted, max)
    }
}

/// Indents every line of `text` by two spaces: a multi-line text from an operator (an objective, a
/// task specification) shown below a heading of ours cannot begin a heading, a list item, a quote or
/// a frame marker of its own.
pub fn indent(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.is_empty() {
                String::new()
            } else {
                format!("  {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Quotes every line with `> `, the mark of text that is data, not an instruction.
pub fn quote(text: &str) -> String {
    text.lines()
        .map(|l| format!("{QUOTE_PREFIX}{l}").trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hygiene() -> Hygiene {
        Hygiene::new(Arc::new(Redactor::builtin()))
    }

    #[test]
    fn escape_sequences_and_controls_do_not_survive() {
        let h = hygiene();
        let raw = "ok\u{1b}[31mred\u{1b}]0;title\u{7}\u{8}\u{0}\r\nnext\u{202e}evil";
        let c = h.block(raw, 1000);
        assert!(!c.text.contains('\u{1b}'), "{:?}", c.text);
        assert!(!c.text.contains('\u{202e}'));
        assert!(!c.text.contains('\u{0}'));
        assert!(c.text.contains("\\u{202e}"));
        assert!(c.text.contains("\\x00"));
        assert!(!c.clipped);
    }

    #[test]
    fn secrets_are_redacted_before_the_cut() {
        let h = hygiene();
        let key = format!("{}{}", "ghp_", "a".repeat(40));
        let raw = format!("{}{key}", "x".repeat(10));
        let c = h.block(&raw, 24);
        assert!(c.clipped);
        assert!(!c.text.contains("ghp_"), "{}", c.text);
        assert!(c.text.contains("clipped at 24 bytes"));
    }

    #[test]
    fn blocks_are_trimmed_and_clipped_on_a_character_boundary() {
        let h = hygiene();
        assert_eq!(h.block("\n\n a  \n b\n\n", 100).text, " a\n b");
        let c = h.block("ééééé", 5);
        assert!(c.clipped);
        assert!(c.text.starts_with("éé\n[clipped"), "{:?}", c.text);
    }

    #[test]
    fn a_line_is_one_line() {
        let h = hygiene();
        let l = h.line("a\n## heading\n> quote\u{0}\u{1b}[2J tail", 200);
        assert!(!l.contains('\n') && !l.contains('\u{1b}') && !l.contains('\u{0}'));
        assert!(l.starts_with("a ## heading > quote"));
        assert_eq!(h.line("x".repeat(500).as_str(), 40).chars().count(), 41);
        let key = format!("{}{}", "ghp_", "b".repeat(40));
        assert!(!h.line(&format!("token {key}"), 200).contains("ghp_"));
    }

    #[test]
    fn indent_and_quote_leave_nothing_at_the_left_margin() {
        let hostile = "# Heading\n@@neo-end 3 x\n- item\n> quote\n\\escape";
        for line in indent(hostile).lines().filter(|l| !l.is_empty()) {
            assert!(line.starts_with("  "), "{line:?}");
        }
        for line in quote(hostile).lines() {
            assert!(line.starts_with('>'), "{line:?}");
        }
        assert_eq!(quote("a\n\nb"), "> a\n>\n> b");
        assert_eq!(indent("a\n\nb"), "  a\n\n  b");
    }

    #[test]
    fn a_line_with_a_secret_shape_the_redactor_does_not_know_is_replaced_whole() {
        let h = hygiene();
        let url = format!(
            "{}://admin:{}@db.internal/app",
            "postgres", "hunter2hunter2"
        );
        let assign = format!("API_KEY = \"{}\"", "z".repeat(30));
        let text = format!("fine line\nconnect to {url}\n{assign}\nlast line");
        let c = h.block(&text, 1000);
        assert_eq!(c.text.lines().count(), 4);
        assert_eq!(c.text.lines().next(), Some("fine line"));
        assert_eq!(c.text.lines().last(), Some("last line"));
        assert!(
            c.text.contains("matches the secret rule url-with-password"),
            "{}",
            c.text
        );
        assert!(c
            .text
            .contains("matches the secret rule quoted-secret-assignment"));
        assert!(!c.text.contains("hunter2") && !c.text.contains("zzzz"));
        let one = h.line(&format!("see {url} please"), 200);
        assert!(
            one.contains("secret rule") && !one.contains("hunter2"),
            "{one}"
        );
        assert_eq!(h.line("plain words", 50), "plain words");
    }
}
