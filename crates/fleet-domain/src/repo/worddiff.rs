//! Intra-line ("word") differences between a removed line and the added line that replaces it.
//!
//! Lines are cut into tokens (a run of letters and digits, a run of white space, or one other
//! character) and compared with Myers' O(ND) algorithm, with every size bounded so that no input,
//! however hostile, makes the work grow faster than linearly in the line length: at most
//! [`MAX_TOKENS`] tokens per line, at most [`MAX_EDITS`] differing tokens (beyond that the lines are
//! "too different" and are not marked) and a work budget per diff ([`WordBudget`]) that is spent
//! across all pairs. Ranges are given in UTF-16 code units, the unit of JavaScript string indices.

/// Tokens per line that are compared.
pub const MAX_TOKENS: usize = 1024;
/// Most differing tokens (insertions plus deletions) the search looks for.
pub const MAX_EDITS: usize = 64;
/// Longest line (in bytes) that is compared at all.
pub const MAX_LINE_BYTES: usize = 8192;
/// Work units (roughly: token comparisons) one diff may spend on word marks.
pub const WORK_PER_DIFF: u64 = 6_000_000;

/// What is left to spend on word marks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WordBudget {
    pub work_left: u64,
}

impl Default for WordBudget {
    fn default() -> WordBudget {
        WordBudget {
            work_left: WORK_PER_DIFF,
        }
    }
}

/// A token: its text and where it starts, in UTF-16 units.
struct Token<'a> {
    text: &'a str,
    start: u32,
    len: u32,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Class {
    Word,
    Space,
    Other,
}

fn class(c: char) -> Class {
    if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Other
    }
}

/// The tokens of `s`, or `None` when there are more than [`MAX_TOKENS`].
fn tokens(s: &str) -> Option<Vec<Token<'_>>> {
    let mut out: Vec<Token<'_>> = Vec::new();
    let mut units = 0u32;
    let mut run: Option<(usize, u32, Class)> = None;
    fn push<'a>(
        s: &'a str,
        from: usize,
        to: usize,
        start: u32,
        end: u32,
        out: &mut Vec<Token<'a>>,
    ) {
        out.push(Token {
            text: s.get(from..to).unwrap_or(""),
            start,
            len: end - start,
        });
    }
    for (at, c) in s.char_indices() {
        let k = class(c);
        match run {
            Some((_, _, rk)) if rk == k && k != Class::Other => {}
            Some((from, start, _)) => {
                push(s, from, at, start, units, &mut out);
                run = Some((at, units, k));
            }
            None => run = Some((at, units, k)),
        }
        units += u32::try_from(c.len_utf16()).unwrap_or(1);
        if out.len() > MAX_TOKENS {
            return None;
        }
    }
    if let Some((from, start, _)) = run {
        push(s, from, s.len(), start, units, &mut out);
    }
    (out.len() <= MAX_TOKENS).then_some(out)
}

/// Longest-common-subsequence matches of `a` and `b` as index pairs, or `None` when the sequences
/// differ in more than [`MAX_EDITS`] places or the budget runs out.
fn myers(a: &[&str], b: &[&str], budget: &mut u64) -> Option<Vec<(usize, usize)>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let dmax = isize::try_from(MAX_EDITS).unwrap_or(64).min(n + m);
    let width = usize::try_from(2 * dmax + 3).unwrap_or(0);
    let at = |k: isize| usize::try_from(k + dmax + 1).unwrap_or(0);
    let mut v: Vec<isize> = vec![0; width];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let get = |v: &Vec<isize>, k: isize| v.get(at(k)).copied().unwrap_or(0);
    let mut found = None;
    'search: for d in 0..=dmax {
        trace.push(v.clone());
        let mut k = -d;
        while k <= d {
            let mut x = if k == -d || (k != d && get(&v, k - 1) < get(&v, k + 1)) {
                get(&v, k + 1)
            } else {
                get(&v, k - 1) + 1
            };
            let mut y = x - k;
            while x < n && y < m {
                let (Some(p), Some(q)) = (
                    a.get(usize::try_from(x).unwrap_or(usize::MAX)),
                    b.get(usize::try_from(y).unwrap_or(usize::MAX)),
                ) else {
                    break;
                };
                if p != q {
                    break;
                }
                x += 1;
                y += 1;
            }
            if let Some(slot) = v.get_mut(at(k)) {
                *slot = x;
            }
            let cost = u64::try_from(d + 1).unwrap_or(1) + u64::try_from(x.max(0)).unwrap_or(0) / 8;
            *budget = budget.checked_sub(cost.max(1))?;
            if x >= n && y >= m {
                found = Some(d);
                break 'search;
            }
            k += 2;
        }
    }
    let mut d = found?;
    let (mut x, mut y) = (n, m);
    let mut matches: Vec<(usize, usize)> = Vec::new();
    while d > 0 {
        let prev = trace.get(usize::try_from(d).unwrap_or(0))?;
        let k = x - y;
        let pk = if k == -d || (k != d && get(prev, k - 1) < get(prev, k + 1)) {
            k + 1
        } else {
            k - 1
        };
        let px = get(prev, pk);
        let py = px - pk;
        while x > px && y > py && x > 0 && y > 0 {
            x -= 1;
            y -= 1;
            matches.push((usize::try_from(x).ok()?, usize::try_from(y).ok()?));
        }
        x = px;
        y = py;
        d -= 1;
    }
    while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        matches.push((usize::try_from(x).ok()?, usize::try_from(y).ok()?));
    }
    matches.reverse();
    Some(matches)
}

/// Ranges (UTF-16 units) of the tokens that are not in `kept`, with touching ranges merged.
fn ranges(toks: &[Token<'_>], kept: &[bool]) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for (t, keep) in toks.iter().zip(kept) {
        if *keep {
            continue;
        }
        let end = t.start + t.len;
        match out.last_mut() {
            Some(last) if last.1 == t.start => last.1 = end,
            _ => out.push((t.start, end)),
        }
    }
    out
}

/// Changed ranges of one line, in UTF-16 units.
pub type Ranges = Vec<(u32, u32)>;

/// The changed ranges of `old` and of `new`: what is left of each line outside its ranges is the
/// same text. `None` when the lines are not compared (too long, too many tokens, too different,
/// the budget is spent, or nothing but the whole line would be marked).
pub fn changed_ranges(old: &str, new: &str, budget: &mut WordBudget) -> Option<(Ranges, Ranges)> {
    if old == new || old.len() > MAX_LINE_BYTES || new.len() > MAX_LINE_BYTES {
        return None;
    }
    let a = tokens(old)?;
    let b = tokens(new)?;
    let mut left = budget.work_left;
    // the common head and tail cost nothing but a linear scan
    let mut head = 0usize;
    while a
        .get(head)
        .zip(b.get(head))
        .is_some_and(|(p, q)| p.text == q.text)
    {
        head += 1;
    }
    let mut tail = 0usize;
    while tail + head < a.len().min(b.len())
        && a.get(a.len() - 1 - tail)
            .zip(b.get(b.len() - 1 - tail))
            .is_some_and(|(p, q)| p.text == q.text)
    {
        tail += 1;
    }
    left = left.checked_sub(u64::try_from(head + tail + a.len() + b.len()).unwrap_or(u64::MAX))?;
    let mid_a: Vec<&str> = a
        .get(head..a.len() - tail)?
        .iter()
        .map(|t| t.text)
        .collect();
    let mid_b: Vec<&str> = b
        .get(head..b.len() - tail)?
        .iter()
        .map(|t| t.text)
        .collect();
    let matches = myers(&mid_a, &mid_b, &mut left);
    budget.work_left = left;
    let matches = matches?;
    let mut keep_a = vec![false; a.len()];
    let mut keep_b = vec![false; b.len()];
    for i in 0..head {
        if let Some(k) = keep_a.get_mut(i) {
            *k = true;
        }
        if let Some(k) = keep_b.get_mut(i) {
            *k = true;
        }
    }
    for i in 0..tail {
        if let Some(k) = keep_a.get_mut(a.len() - 1 - i) {
            *k = true;
        }
        if let Some(k) = keep_b.get_mut(b.len() - 1 - i) {
            *k = true;
        }
    }
    for (i, j) in matches {
        if let Some(k) = keep_a.get_mut(head + i) {
            *k = true;
        }
        if let Some(k) = keep_b.get_mut(head + j) {
            *k = true;
        }
    }
    // nothing in common: the whole line is the change, which marks nothing useful
    if !keep_a.iter().any(|k| *k) {
        return None;
    }
    Some((ranges(&a, &keep_a), ranges(&b, &keep_b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(a: &str, b: &str) -> Option<(Ranges, Ranges)> {
        changed_ranges(a, b, &mut WordBudget::default())
    }

    /// The text of `s` outside the ranges (UTF-16 units).
    fn outside(s: &str, ranges: &[(u32, u32)]) -> String {
        let units: Vec<u16> = s.encode_utf16().collect();
        let mut out: Vec<u16> = Vec::new();
        for (i, u) in units.iter().enumerate() {
            let i = u32::try_from(i).unwrap();
            if !ranges.iter().any(|(s, e)| i >= *s && i < *e) {
                out.push(*u);
            }
        }
        String::from_utf16_lossy(&out)
    }

    #[test]
    fn a_changed_word_is_the_only_thing_marked() {
        let (o, n) = run("let x = foo(1, 2);", "let x = bar(1, 2);").unwrap();
        assert_eq!(o, vec![(8, 11)]);
        assert_eq!(n, vec![(8, 11)]);
        let (o, n) = run("a b c", "a b c d").unwrap();
        assert!(o.is_empty());
        assert_eq!(n, vec![(5, 7)]);
    }

    #[test]
    fn identical_unrelated_and_empty_lines_mark_nothing() {
        assert_eq!(run("same", "same"), None);
        assert_eq!(run("abc", "xyz"), None);
        assert_eq!(run("", "x"), None);
        assert_eq!(run("x", ""), None);
    }

    #[test]
    fn ranges_count_utf16_units() {
        // a character outside the basic plane is two units
        let (o, n) = run("\u{1f600} old tail", "\u{1f600} new tail").unwrap();
        assert_eq!(o, vec![(3, 6)]);
        assert_eq!(n, vec![(3, 6)]);
        let (o, _) = run("é x", "é y").unwrap();
        assert_eq!(o, vec![(2, 3)]);
    }

    #[test]
    fn what_is_left_outside_the_ranges_is_the_same_text() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut seed_len = 12345u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let atoms = [
            "a",
            "b",
            " ",
            "  ",
            "foo",
            "bar",
            "_",
            "(",
            ")",
            ";",
            "\u{e9}",
            "\u{1f600}",
            "12",
        ];
        for _ in 0..3000 {
            let mut pick = |len: u64| -> String {
                let mut s = String::new();
                for _ in 0..len {
                    let i = usize::try_from(next() % atoms.len() as u64).unwrap();
                    s.push_str(atoms[i]);
                }
                s
            };
            let a = pick(3 + (seed_len % 20));
            let b = pick(3 + ((seed_len / 7) % 20));
            seed_len = seed_len
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            if let Some((o, n)) = run(&a, &b) {
                assert_eq!(outside(&a, &o), outside(&b, &n), "{a:?} / {b:?}");
                for (s, e) in o.iter().chain(&n) {
                    assert!(s < e);
                }
            }
        }
    }

    #[test]
    fn work_is_bounded_whatever_the_lines_are() {
        // alternating tokens, no common tokens in the middle, the longest allowed lines
        let a: String = (0..MAX_TOKENS)
            .map(|i| if i % 2 == 0 { "x" } else { "+" })
            .collect();
        let b: String = (0..MAX_TOKENS)
            .map(|i| if i % 2 == 0 { "y" } else { "-" })
            .collect();
        let mut budget = WordBudget::default();
        let start = std::time::Instant::now();
        assert!(changed_ranges(&a, &b, &mut budget).is_none());
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        // lines past the caps are not compared at all
        let long = "word ".repeat(MAX_LINE_BYTES);
        assert_eq!(run(&long, "x"), None);
        let many: String = "a ".repeat(MAX_TOKENS);
        assert_eq!(run(&many, "b"), None);
        // the budget is shared: once spent, nothing more is marked
        let mut tiny = WordBudget { work_left: 5 };
        assert_eq!(
            changed_ranges("let x = foo;", "let x = bar;", &mut tiny),
            None
        );
        // many pairs of the largest similar lines cost a bounded total
        let base: String = (0..400).map(|i| format!("w{i} ")).collect();
        let other: String = (0..400)
            .map(|i| {
                if i % 3 == 0 {
                    format!("z{i} ")
                } else {
                    format!("w{i} ")
                }
            })
            .collect();
        let mut budget = WordBudget::default();
        let start = std::time::Instant::now();
        let mut marked = 0;
        for _ in 0..10_000 {
            if changed_ranges(&base, &other, &mut budget).is_some() {
                marked += 1;
            }
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        assert!(marked < 10_000, "the budget ran out");
        assert!(budget.work_left < WORK_PER_DIFF);
    }

    #[test]
    fn many_edits_are_too_different_to_mark() {
        let a: String = (0..200).map(|i| format!("a{i} ")).collect();
        let b: String = (0..200).map(|i| format!("b{i} ")).collect();
        assert_eq!(run(&a, &b), None);
    }
}
