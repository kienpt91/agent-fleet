//! A small path glob matcher for diff constraints, written so that no input can make it slow.
//!
//! There is no regular-expression engine and no backtracking over alternatives: a pattern is split
//! into path segments, matching walks the path once keeping the set of pattern segments that are
//! still possible (a segment-level automaton), and one segment is matched against one path segment
//! with the greedy single-restart wildcard algorithm. The cost is bounded by
//! `segments(pattern) * length(path) * length(pattern segment)`; pattern and path lengths are
//! capped, so a hostile pattern or path cannot take more than a few milliseconds.
//!
//! Syntax (always relative to the worktree, `/` separates segments, case-sensitive):
//! - `*` matches any run of characters inside one segment (also none), `?` exactly one character;
//! - `**` as a whole segment matches any number of segments, including none;
//! - everything else is literal. Classes (`[a-z]`), braces, escapes, negation, a leading `/`, a
//!   trailing `/`, empty segments and `.` or `..` segments are rejected when the pattern is built,
//!   so a pattern never means something other than what it says.

use crate::limits::{MAX_GLOB_LEN, MAX_MATCH_PATH};
use fleet_domain::ids::{is_invisible, show};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Lit(char),
    One,
    Many,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
    /// `**`
    AnyDirs,
    Pattern(Vec<Token>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Glob {
    pattern: String,
    segments: Vec<Segment>,
}

impl Glob {
    pub fn new(pattern: &str) -> Result<Glob, String> {
        let bad = |why: &str| format!("glob {}: {why}", show(pattern));
        if pattern.is_empty() {
            return Err("a glob is empty".to_string());
        }
        if pattern.len() > MAX_GLOB_LEN {
            return Err(bad(&format!("longer than {MAX_GLOB_LEN} bytes")));
        }
        if pattern.chars().any(|c| c.is_control() || is_invisible(c)) {
            return Err(bad("contains control or invisible characters"));
        }
        if pattern.starts_with('/') {
            return Err(bad(
                "must be relative to the worktree, without a leading `/`",
            ));
        }
        if pattern.ends_with('/') {
            return Err(bad(
                "must not end with `/`; use `dir/**` for everything below dir",
            ));
        }
        if let Some(c) = pattern
            .chars()
            .find(|c| matches!(c, '[' | ']' | '{' | '}' | '\\'))
        {
            return Err(bad(&format!(
                "`{c}` is not supported (only `*`, `?` and `**` are special)"
            )));
        }
        let mut segments = Vec::new();
        for raw in pattern.split('/') {
            if raw.is_empty() {
                return Err(bad("has an empty path segment"));
            }
            if raw == "." || raw == ".." {
                return Err(bad("must not contain `.` or `..` segments"));
            }
            if raw == "**" {
                segments.push(Segment::AnyDirs);
                continue;
            }
            if raw.contains("**") {
                return Err(bad("`**` must be a whole path segment"));
            }
            let tokens = raw
                .chars()
                .map(|c| match c {
                    '*' => Token::Many,
                    '?' => Token::One,
                    other => Token::Lit(other),
                })
                .collect();
            segments.push(Segment::Pattern(tokens));
        }
        Ok(Glob {
            pattern: pattern.to_string(),
            segments,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.pattern
    }

    /// True when `path` (relative, `/`-separated, as git prints it) matches the whole pattern.
    pub fn matches(&self, path: &str) -> bool {
        if path.is_empty() || path.len() > MAX_MATCH_PATH {
            return false;
        }
        let n = self.segments.len();
        let mut live = vec![false; n.saturating_add(1)];
        if let Some(first) = live.first_mut() {
            *first = true;
        }
        self.close(&mut live);
        for part in path.split('/') {
            if part.is_empty() {
                return false;
            }
            let text: Vec<char> = part.chars().collect();
            let mut next = vec![false; n.saturating_add(1)];
            for (i, seg) in self.segments.iter().enumerate() {
                if !live.get(i).copied().unwrap_or(false) {
                    continue;
                }
                match seg {
                    Segment::AnyDirs => {
                        if let Some(slot) = next.get_mut(i) {
                            *slot = true;
                        }
                    }
                    Segment::Pattern(tokens) => {
                        if segment_matches(tokens, &text) {
                            if let Some(slot) = next.get_mut(i.saturating_add(1)) {
                                *slot = true;
                            }
                        }
                    }
                }
            }
            self.close(&mut next);
            live = next;
            if !live.iter().any(|b| *b) {
                return false;
            }
        }
        live.last().copied().unwrap_or(false)
    }

    /// A `**` segment may also match nothing: whenever it is reachable, so is the segment after it.
    fn close(&self, live: &mut [bool]) {
        for (i, seg) in self.segments.iter().enumerate() {
            if matches!(seg, Segment::AnyDirs) && live.get(i).copied().unwrap_or(false) {
                if let Some(slot) = live.get_mut(i.saturating_add(1)) {
                    *slot = true;
                }
            }
        }
    }
}

/// Greedy wildcard match of one path segment: on a mismatch it returns to the most recent `*` and
/// lets it swallow one more character. One restart point, never a stack of them.
fn segment_matches(tokens: &[Token], text: &[char]) -> bool {
    let mut t = 0usize;
    let mut p = 0usize;
    let mut restart: Option<(usize, usize)> = None;
    while t < text.len() {
        let here = text.get(t).copied();
        match tokens.get(p) {
            Some(Token::Many) => {
                p += 1;
                restart = Some((p, t));
                continue;
            }
            Some(Token::One) => {
                t += 1;
                p += 1;
                continue;
            }
            Some(Token::Lit(c)) if Some(*c) == here => {
                t += 1;
                p += 1;
                continue;
            }
            _ => {}
        }
        match restart {
            Some((rp, rt)) => {
                p = rp;
                t = rt + 1;
                restart = Some((rp, rt + 1));
            }
            None => return false,
        }
    }
    while matches!(tokens.get(p), Some(Token::Many)) {
        p += 1;
    }
    p == tokens.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(p: &str) -> Glob {
        Glob::new(p).unwrap()
    }

    #[test]
    fn literal_and_wildcards() {
        assert!(g("README.md").matches("README.md"));
        assert!(!g("README.md").matches("readme.md"));
        assert!(!g("README.md").matches("docs/README.md"));
        assert!(g("src/*.rs").matches("src/main.rs"));
        assert!(!g("src/*.rs").matches("src/a/main.rs"));
        assert!(g("a?c").matches("abc"));
        assert!(!g("a?c").matches("ac"));
        assert!(g("*").matches("anything"));
        assert!(!g("*").matches("a/b"));
        assert!(g("*.t*t").matches("x.txt"));
        assert!(g("a*b*c").matches("aXXbYYc"));
        assert!(!g("a*b*c").matches("aXXbYY"));
        assert!(g(".*").matches(".gitignore"));
    }

    #[test]
    fn double_star_spans_directories() {
        assert!(g("src/**").matches("src/a.rs"));
        assert!(g("src/**").matches("src/a/b/c.rs"));
        assert!(!g("src/**").matches("tests/a.rs"));
        assert!(g("**/*.md").matches("a.md"));
        assert!(g("**/*.md").matches("docs/deep/a.md"));
        assert!(!g("**/*.md").matches("docs/a.txt"));
        assert!(g("a/**/z").matches("a/z"));
        assert!(g("a/**/z").matches("a/b/c/z"));
        assert!(!g("a/**/z").matches("a/b/c/y"));
        assert!(g("**").matches("x/y"));
    }

    #[test]
    fn paths_that_are_not_clean_never_match() {
        for p in ["", "/a", "a//b", "a/", &"a/".repeat(3000)] {
            assert!(!g("**").matches(p), "{p:?}");
        }
    }

    #[test]
    fn unicode_is_matched_by_character() {
        assert!(g("é?.txt").matches("éa.txt"));
        assert!(g("*.日本").matches("x.日本"));
        assert!(!g("?").matches("日本"));
        assert!(g("?").matches("日"));
    }

    #[test]
    fn bad_patterns_are_rejected() {
        let long = "a".repeat(MAX_GLOB_LEN + 1);
        for bad in [
            "",
            "/abs",
            "dir/",
            "a//b",
            "a/./b",
            "../x",
            "a/..",
            "a**",
            "**a",
            "a/***/b",
            "[ab]",
            "{a,b}",
            "a\\b",
            "a\nb",
            "a\u{202e}b",
            long.as_str(),
        ] {
            assert!(Glob::new(bad).is_err(), "{bad:?}");
        }
        assert!(Glob::new(&"a".repeat(MAX_GLOB_LEN)).is_ok());
    }

    #[test]
    fn error_messages_quote_hostile_patterns() {
        let msg = Glob::new("\u{1b}[31m/x").unwrap_err();
        assert!(!msg.contains('\u{1b}'));
        assert_eq!(Glob::new("a/b").unwrap().as_str(), "a/b");
    }

    #[test]
    fn many_stars_do_not_blow_up() {
        let pattern = "*a".repeat(100);
        let glob = g(&pattern);
        let path = "a".repeat(199) + "b";
        let started = std::time::Instant::now();
        assert!(!glob.matches(&path));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        let nested = "a*".repeat(60);
        let g2 = g(&format!("{nested}/**/{nested}"));
        let deep = format!("{}/{}", "a".repeat(120), "a".repeat(120));
        assert!(g2.matches(&deep));
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, bound: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % bound
        }
    }

    /// Exponential but obviously correct reference.
    fn reference_segment(p: &[Token], t: &[char]) -> bool {
        match p.split_first() {
            None => t.is_empty(),
            Some((Token::Many, rest)) => {
                (0..=t.len()).any(|skip| reference_segment(rest, &t[skip..]))
            }
            Some((Token::One, rest)) => !t.is_empty() && reference_segment(rest, &t[1..]),
            Some((Token::Lit(c), rest)) => t.first() == Some(c) && reference_segment(rest, &t[1..]),
        }
    }

    fn reference(segs: &[Segment], path: &[&str]) -> bool {
        match segs.split_first() {
            None => path.is_empty(),
            Some((Segment::AnyDirs, rest)) => {
                (0..=path.len()).any(|skip| reference(rest, &path[skip..]))
            }
            Some((Segment::Pattern(tokens), rest)) => match path.split_first() {
                None => false,
                Some((first, tail)) => {
                    let chars: Vec<char> = first.chars().collect();
                    reference_segment(tokens, &chars) && reference(rest, tail)
                }
            },
        }
    }

    #[test]
    fn agrees_with_the_reference_matcher_on_random_small_inputs() {
        let mut rng = Lcg(7);
        let pattern_atoms = ["a", "b", "*", "?", "ab", "a*", "*b", "**"];
        let path_atoms = ["a", "b", "ab", "ba", "aab", "abb", "c"];
        let mut checked = 0;
        for _ in 0..4000 {
            let parts: Vec<&str> = (0..1 + rng.next(4))
                .map(|_| pattern_atoms[rng.next(pattern_atoms.len())])
                .collect();
            let Ok(glob) = Glob::new(&parts.join("/")) else {
                continue;
            };
            let path: Vec<&str> = (0..1 + rng.next(4))
                .map(|_| path_atoms[rng.next(path_atoms.len())])
                .collect();
            let want = reference(&glob.segments, &path);
            assert_eq!(
                glob.matches(&path.join("/")),
                want,
                "{} vs {}",
                glob.as_str(),
                path.join("/")
            );
            checked += 1;
        }
        assert!(checked > 1000, "{checked}");
    }
}
