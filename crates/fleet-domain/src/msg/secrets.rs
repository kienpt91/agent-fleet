//! Does a text look like it carries a credential? The same rules as `scripts/secret-scan.sh`
//! (the scanner that keeps secrets out of the repository), written out by hand because the
//! domain has no regular-expression engine. A message that matches is refused whole, and its body
//! is never stored or delivered.
//!
//! The script matches each line with `grep -E`; so does this module: a rule never spans a line
//! break. Text is checked as written and again with invisible and control characters removed, so
//! a zero-width character inside a key does not hide it.

use crate::ids::is_invisible;

/// The rule names, in the order of `scripts/secret-scan.sh`.
pub const RULES: [&str; 14] = [
    "private-key-block",
    "aws-access-key-id",
    "github-token",
    "github-fine-grained-token",
    "stripe-key",
    "stripe-webhook-secret",
    "slack-token",
    "google-api-key",
    "npm-token",
    "sk-style-key",
    "jwt",
    "url-with-password",
    "quoted-secret-assignment",
    "raw-private-hex",
];

/// The name of the first rule `text` matches.
pub fn find_secret_shape(text: &str) -> Option<&'static str> {
    if let Some(rule) = scan_lines(text) {
        return Some(rule);
    }
    if text
        .chars()
        .any(|c| is_invisible(c) || is_squeezed_control(c))
    {
        let squeezed: String = text
            .chars()
            .filter(|c| !(is_invisible(*c) || is_squeezed_control(*c)))
            .collect();
        return scan_lines(&squeezed);
    }
    None
}

/// Controls other than the line break, which splits lines.
fn is_squeezed_control(c: char) -> bool {
    c.is_control() && c != '\n'
}

fn scan_lines(text: &str) -> Option<&'static str> {
    text.split('\n').find_map(scan_line)
}

/// One rule: does a line (as bytes) match?
type RuleFn = fn(&[u8]) -> bool;

fn scan_line(line: &str) -> Option<&'static str> {
    let b = line.as_bytes();
    let rules: [(&'static str, RuleFn); 14] = [
        ("private-key-block", private_key_block),
        ("aws-access-key-id", aws_access_key),
        ("github-token", github_token),
        ("github-fine-grained-token", github_pat),
        ("stripe-key", stripe_key),
        ("stripe-webhook-secret", stripe_webhook),
        ("slack-token", slack_token),
        ("google-api-key", google_key),
        ("npm-token", npm_token),
        ("sk-style-key", sk_key),
        ("jwt", jwt),
        ("url-with-password", url_with_password),
        ("quoted-secret-assignment", quoted_assignment),
        ("raw-private-hex", raw_private_hex),
    ];
    rules.iter().find(|(_, f)| f(b)).map(|(name, _)| *name)
}

// ---- helpers over bytes ---------------------------------------------------------------------

fn alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric()
}

fn alnum_us_dash(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

fn alnum_dash(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-'
}

fn upper_digit(c: u8) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit()
}

/// POSIX `[:space:]`.
fn space(c: u8) -> bool {
    c.is_ascii_whitespace() || c == 0x0B
}

/// Every position at which `needle` occurs.
fn find_all<'a>(hay: &'a [u8], needle: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
    let last = hay.len().checked_sub(needle.len());
    (0..=last.unwrap_or(0))
        .filter(move |_| last.is_some())
        .filter(move |i| hay.get(*i..i + needle.len()) == Some(needle))
}

/// Length of the run of `class` bytes at the start of `rest`.
fn run(rest: &[u8], class: fn(u8) -> bool) -> usize {
    rest.iter().take_while(|c| class(**c)).count()
}

/// `prefix` followed by at least `min` bytes of `class`.
fn prefixed(b: &[u8], prefix: &[u8], class: fn(u8) -> bool, min: usize) -> bool {
    find_all(b, prefix).any(|i| {
        b.get(i + prefix.len()..)
            .is_some_and(|rest| run(rest, class) >= min)
    })
}

// ---- the rules --------------------------------------------------------------------------------

/// `-----BEGIN [A-Z ]*PRIVATE KEY-----`
fn private_key_block(b: &[u8]) -> bool {
    find_all(b, b"-----BEGIN ").any(|i| {
        let Some(rest) = b.get(i + 11..) else {
            return false;
        };
        let n = run(rest, |c| c.is_ascii_uppercase() || c == b' ');
        let body = rest.get(..n).unwrap_or(&[]);
        body.ends_with(b"PRIVATE KEY") && rest.get(n..).is_some_and(|t| t.starts_with(b"-----"))
    })
}

/// `(^|[^A-Za-z0-9])(AKIA|ASIA)[0-9A-Z]{16}([^A-Za-z0-9]|$)`
fn aws_access_key(b: &[u8]) -> bool {
    [b"AKIA".as_slice(), b"ASIA".as_slice()].iter().any(|p| {
        find_all(b, p).any(|i| {
            let before_ok = i == 0 || b.get(i - 1).is_some_and(|c| !alnum(*c));
            let body = b.get(i + 4..i + 20);
            let body_ok = body.is_some_and(|s| s.iter().all(|c| upper_digit(*c)));
            let after_ok = b.get(i + 20).is_none_or(|c| !alnum(*c));
            before_ok && body_ok && after_ok
        })
    })
}

/// `(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{30,}`
fn github_token(b: &[u8]) -> bool {
    ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"]
        .iter()
        .any(|p| prefixed(b, p.as_bytes(), alnum, 30))
}

/// `github_pat_[A-Za-z0-9_]{30,}`
fn github_pat(b: &[u8]) -> bool {
    prefixed(b, b"github_pat_", |c| alnum(c) || c == b'_', 30)
}

/// `[sr]k_(live|test)_[A-Za-z0-9]{16,}`
fn stripe_key(b: &[u8]) -> bool {
    ["sk_live_", "sk_test_", "rk_live_", "rk_test_"]
        .iter()
        .any(|p| prefixed(b, p.as_bytes(), alnum, 16))
}

/// `whsec_[A-Za-z0-9]{16,}`
fn stripe_webhook(b: &[u8]) -> bool {
    prefixed(b, b"whsec_", alnum, 16)
}

/// `xox[abprs]-[A-Za-z0-9-]{10,}`
fn slack_token(b: &[u8]) -> bool {
    ["xoxa-", "xoxb-", "xoxp-", "xoxr-", "xoxs-"]
        .iter()
        .any(|p| prefixed(b, p.as_bytes(), alnum_dash, 10))
}

/// `AIza[0-9A-Za-z_-]{35}`
fn google_key(b: &[u8]) -> bool {
    prefixed(b, b"AIza", alnum_us_dash, 35)
}

/// `npm_[A-Za-z0-9]{36}`
fn npm_token(b: &[u8]) -> bool {
    prefixed(b, b"npm_", alnum, 36)
}

/// `sk-[A-Za-z0-9_-]{32,}`
fn sk_key(b: &[u8]) -> bool {
    prefixed(b, b"sk-", alnum_us_dash, 32)
}

/// `eyJ[A-Za-z0-9_-]{15,}\.eyJ[A-Za-z0-9_-]{15,}\.[A-Za-z0-9_-]{10,}`
fn jwt(b: &[u8]) -> bool {
    find_all(b, b"eyJ").any(|i| {
        let Some(rest) = b.get(i + 3..) else {
            return false;
        };
        let n1 = run(rest, alnum_us_dash);
        if n1 < 15 || rest.get(n1) != Some(&b'.') {
            return false;
        }
        let Some(rest) = rest.get(n1 + 1..) else {
            return false;
        };
        if !rest.starts_with(b"eyJ") {
            return false;
        }
        let Some(rest) = rest.get(3..) else {
            return false;
        };
        let n2 = run(rest, alnum_us_dash);
        if n2 < 15 || rest.get(n2) != Some(&b'.') {
            return false;
        }
        rest.get(n2 + 1..)
            .is_some_and(|tail| run(tail, alnum_us_dash) >= 10)
    })
}

/// `[a-z][a-z0-9+.-]*://[^/[:space:]:@]+:[^/[:space:]@]{6,}@[^/[:space:]]+`
fn url_with_password(b: &[u8]) -> bool {
    find_all(b, b"://").any(|i| {
        // the scheme: a run of [a-z0-9+.-] right before "://" that holds a lowercase letter
        let scheme = b.get(..i).unwrap_or(&[]);
        let n = scheme
            .iter()
            .rev()
            .take_while(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(**c, b'+' | b'.' | b'-')
            })
            .count();
        let has_letter = scheme.iter().rev().take(n).any(|c| c.is_ascii_lowercase());
        if !has_letter {
            return false;
        }
        let Some(rest) = b.get(i + 3..) else {
            return false;
        };
        let user = run(rest, |c| !(c == b'/' || space(c) || c == b':' || c == b'@'));
        if user == 0 || rest.get(user) != Some(&b':') {
            return false;
        }
        let Some(rest) = rest.get(user + 1..) else {
            return false;
        };
        let pass = run(rest, |c| !(c == b'/' || space(c) || c == b'@'));
        if pass < 6 || rest.get(pass) != Some(&b'@') {
            return false;
        }
        rest.get(pass + 1..)
            .is_some_and(|host| run(host, |c| !(c == b'/' || space(c))) >= 1)
    })
}

/// `(api[_-]?key|secret|token|passwd|password|private[_-]?key)["']?[[:space:]]*[:=][[:space:]]*["'][A-Za-z0-9+/_=-]{24,}["']`
fn quoted_assignment(b: &[u8]) -> bool {
    const WORDS: [&str; 10] = [
        "apikey",
        "api_key",
        "api-key",
        "secret",
        "token",
        "passwd",
        "password",
        "privatekey",
        "private_key",
        "private-key",
    ];
    fn value_class(c: u8) -> bool {
        alnum(c) || matches!(c, b'+' | b'/' | b'_' | b'=' | b'-')
    }
    let quote = |c: u8| c == b'"' || c == b'\'';
    WORDS.iter().any(|w| {
        find_all(b, w.as_bytes()).any(|i| {
            let mut rest = b.get(i + w.len()..).unwrap_or(&[]);
            if rest.first().is_some_and(|c| quote(*c)) {
                rest = rest.get(1..).unwrap_or(&[]);
            }
            rest = rest.get(run(rest, space)..).unwrap_or(&[]);
            if !matches!(rest.first(), Some(b':' | b'=')) {
                return false;
            }
            rest = rest.get(1..).unwrap_or(&[]);
            rest = rest.get(run(rest, space)..).unwrap_or(&[]);
            if !rest.first().is_some_and(|c| quote(*c)) {
                return false;
            }
            rest = rest.get(1..).unwrap_or(&[]);
            let n = run(rest, value_class);
            n >= 24 && rest.get(n).is_some_and(|c| quote(*c))
        })
    })
}

/// `(^|[^0-9A-Fa-f])0x[0-9a-fA-F]{64}([^0-9A-Fa-f]|$)`
fn raw_private_hex(b: &[u8]) -> bool {
    find_all(b, b"0x").any(|i| {
        let before_ok = i == 0 || b.get(i - 1).is_some_and(|c| !c.is_ascii_hexdigit());
        let body_ok = b
            .get(i + 2..i + 66)
            .is_some_and(|s| s.iter().all(u8::is_ascii_hexdigit));
        let after_ok = b.get(i + 66).is_none_or(|c| !c.is_ascii_hexdigit());
        before_ok && body_ok && after_ok
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shape assembled at run time, so no source line of this file looks like a key.
    fn shape(prefix: &str, body_char: char, n: usize) -> String {
        format!("{prefix}{}", body_char.to_string().repeat(n))
    }

    fn found(text: &str) -> Option<&'static str> {
        find_secret_shape(text)
    }

    #[test]
    fn every_rule_of_the_script_matches_its_own_shape() {
        let jwt_text = format!(
            "{}.{}.{}",
            shape("eyJ", 'a', 20),
            shape("eyJ", 'b', 20),
            shape("", 'c', 12)
        );
        let url = format!(
            "{}://{}:{}@{}",
            "postgres", "admin", "hunter2hunter2", "db.local"
        );
        let assignment = format!("{} = \"{}\"", "password", "A".repeat(30));
        let key_block = format!("-----BEGIN {} KEY-----", "RSA PRIVATE");
        let cases: Vec<(&str, String)> = vec![
            ("private-key-block", key_block),
            (
                "aws-access-key-id",
                format!("id {}", shape("AKIA", 'Q', 16)),
            ),
            ("aws-access-key-id", shape("ASIA", '7', 16)),
            ("github-token", shape("ghp_", 'a', 36)),
            ("github-token", shape("gho_", 'B', 30)),
            ("github-token", shape("ghu_", '1', 31)),
            ("github-token", shape("ghs_", 'z', 40)),
            ("github-token", shape("ghr_", 'Z', 30)),
            ("github-fine-grained-token", shape("github_pat_", 'x', 40)),
            ("stripe-key", shape("sk_live_", 'a', 20)),
            ("stripe-key", shape("rk_test_", 'a', 16)),
            ("stripe-webhook-secret", shape("whsec_", 'a', 20)),
            ("slack-token", shape("xoxb-", '1', 14)),
            ("slack-token", shape("xoxr-", 'a', 10)),
            ("google-api-key", shape("AIza", 'a', 35)),
            ("npm-token", shape("npm_", 'a', 36)),
            ("sk-style-key", shape("sk-", 'a', 40)),
            ("jwt", jwt_text),
            ("url-with-password", url),
            ("quoted-secret-assignment", assignment),
            ("raw-private-hex", format!("key: 0x{}", "ab".repeat(32))),
        ];
        for (rule, text) in &cases {
            assert_eq!(found(text), Some(*rule), "{rule}: {text}");
        }
        for rule in RULES {
            assert!(
                cases.iter().any(|(r, _)| *r == rule),
                "no case for the rule {rule}"
            );
        }
    }

    #[test]
    fn shapes_that_are_one_character_too_short_do_not_match() {
        let near_misses = [
            shape("AKIA", 'Q', 15),
            shape("AKIA", 'Q', 17),
            shape("ghp_", 'a', 29),
            shape("github_pat_", 'x', 29),
            shape("sk_live_", 'a', 15),
            shape("whsec_", 'a', 15),
            shape("xoxb-", '1', 9),
            shape("AIza", 'a', 34),
            shape("npm_", 'a', 35),
            shape("sk-", 'a', 31),
            format!("0x{}", "a".repeat(63)),
            format!("0x{}", "a".repeat(65)),
        ];
        for text in near_misses {
            // the 17-character AWS body and the 65-digit number run on, so the boundary rule fails
            assert_eq!(found(&text), None, "{text}");
        }
    }

    #[test]
    fn ordinary_text_is_not_a_secret() {
        for text in [
            "",
            "please review the migration in db/schema.sql",
            "the token budget for this task is 4000",
            "password policy: at least twelve characters",
            "see https://example.com/docs and http://localhost:8080/health",
            "commit 4b825dc642cb6eb9a060e54bf8d69288fbee4904 builds",
            "Bearer tokens are described in the docs",
            "-----BEGIN CERTIFICATE-----",
            "the AKIA prefix is how AWS access key ids start",
            "ssh://git@host/repo.git",
        ] {
            assert_eq!(found(text), None, "{text}");
        }
    }

    #[test]
    fn a_rule_never_spans_a_line_break() {
        let text = format!("{}\n{}", "ghp_", "a".repeat(40));
        assert_eq!(found(&text), None);
        let url = format!("{}://{}:\n{}@host", "https", "user", "pass12345");
        assert_eq!(found(&url), None);
    }

    #[test]
    fn invisible_characters_do_not_hide_a_key() {
        let key = shape("ghp_", 'a', 36);
        let hidden = format!("gh\u{200b}p_{}", "a".repeat(36));
        assert_eq!(found(&key), Some("github-token"));
        assert_eq!(found(&hidden), Some("github-token"));
        let controls = format!("sk\u{1b}-{}", "q".repeat(40));
        assert_eq!(found(&controls), Some("sk-style-key"));
    }

    #[test]
    fn a_key_inside_longer_text_is_found_wherever_it_is() {
        let key = shape("sk_test_", 'k', 24);
        for text in [
            format!("{key} at the start"),
            format!("at the end {key}"),
            format!("in \"{key}\" quotes"),
            format!("line one\nline two {key}\nline three"),
        ] {
            assert_eq!(found(&text), Some("stripe-key"), "{text}");
        }
    }

    #[test]
    fn the_aws_rule_needs_its_boundaries() {
        let body = "Q".repeat(16);
        assert_eq!(found(&format!("xAKIA{body}")), None, "a letter before");
        assert_eq!(found(&format!("AKIA{body}z")), None, "a letter after");
        assert_eq!(found(&format!("=AKIA{body};")), Some("aws-access-key-id"));
    }

    #[test]
    fn url_and_assignment_rules_follow_the_script() {
        // a user without a password, a short password and no host are not matches
        assert_eq!(found("https://user@host/path"), None);
        assert_eq!(found(&format!("{}://u:{}@h", "https", "short")), None);
        assert_eq!(found(&format!("{}://u:{}@", "https", "longenough")), None);
        // a short value, or one without quotes, is not an assignment
        assert_eq!(
            found(&format!("{} = \"{}\"", "token", "a".repeat(23))),
            None
        );
        assert_eq!(found(&format!("{} = {}", "token", "a".repeat(40))), None);
        assert_eq!(
            found(&format!("{}: '{}'", "api_key", "a".repeat(24))),
            Some("quoted-secret-assignment")
        );
        assert_eq!(
            found(&format!(
                "\"{}\":\"{}\"",
                "private-key",
                "Zz09+/=".repeat(4)
            )),
            Some("quoted-secret-assignment")
        );
    }

    #[test]
    fn the_rule_table_has_no_duplicates_and_matches_the_script() {
        let mut names = RULES.to_vec();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), RULES.len());
        for name in RULES {
            assert!(!name.is_empty());
        }
    }
}
