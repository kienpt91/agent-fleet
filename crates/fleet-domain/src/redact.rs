//! Secret redaction for stored worker output.
//!
//! No regex engine: a rule is a literal prefix followed by a run of characters from a small class
//! (`alnum`, `hex`, `base64`, `token`) of at least `min_len` characters. That covers the usual key
//! shapes (`ghp_...`, `AKIA...`, `sk-...`) without a dependency. Exact literals (for example the
//! values of secret-looking variables in the daemon's own environment) are replaced as well, and
//! PEM private-key blocks are suppressed line by line.

use crate::error::{FleetError, Result};
use crate::limits::{MAX_REDACT_LITERALS, MAX_REDACT_RULES};
use std::borrow::Cow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charset {
    Alnum,
    Hex,
    Base64,
    /// Letters, digits and `-_.~+/=`.
    Token,
}

impl Charset {
    pub fn parse(s: &str) -> Option<Charset> {
        match s {
            "alnum" => Some(Charset::Alnum),
            "hex" => Some(Charset::Hex),
            "base64" => Some(Charset::Base64),
            "token" => Some(Charset::Token),
            _ => None,
        }
    }

    fn contains(self, c: char) -> bool {
        match self {
            Charset::Alnum => c.is_ascii_alphanumeric(),
            Charset::Hex => c.is_ascii_hexdigit(),
            Charset::Base64 => {
                c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '-' | '_')
            }
            Charset::Token => {
                c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '+' | '/' | '=')
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    pub prefix: String,
    pub charset: Charset,
    pub min_len: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Redactor {
    literals: Vec<String>,
    rules: Vec<Rule>,
}

/// Names of environment variables whose values are treated as secrets.
const SECRET_NAME_PARTS: [&str; 7] = [
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "apikey",
    "api_key",
];

impl Redactor {
    /// Built-in rules for common key shapes.
    pub fn builtin() -> Redactor {
        let r = |name: &str, prefix: &str, charset: Charset, min_len: usize| Rule {
            name: name.to_string(),
            prefix: prefix.to_string(),
            charset,
            min_len,
        };
        Redactor {
            literals: Vec::new(),
            rules: vec![
                r("aws-access-key", "AKIA", Charset::Alnum, 16),
                r("aws-session-key", "ASIA", Charset::Alnum, 16),
                r("github-token", "ghp_", Charset::Alnum, 30),
                r("github-token", "gho_", Charset::Alnum, 30),
                r("github-token", "ghu_", Charset::Alnum, 30),
                r("github-token", "ghs_", Charset::Alnum, 30),
                r("github-token", "ghr_", Charset::Alnum, 30),
                r("github-pat", "github_pat_", Charset::Token, 20),
                r("slack-token", "xoxb-", Charset::Token, 10),
                r("slack-token", "xoxp-", Charset::Token, 10),
                r("slack-token", "xoxa-", Charset::Token, 10),
                r("slack-token", "xoxs-", Charset::Token, 10),
                r("api-key", "sk-", Charset::Token, 20),
                r("google-api-key", "AIza", Charset::Token, 35),
                r("bearer-token", "Bearer ", Charset::Token, 16),
                r("stripe-key", "sk_live_", Charset::Alnum, 16),
                r("stripe-key", "sk_test_", Charset::Alnum, 16),
                r("stripe-key", "rk_live_", Charset::Alnum, 16),
                r("stripe-key", "rk_test_", Charset::Alnum, 16),
                r("stripe-webhook", "whsec_", Charset::Alnum, 16),
                r("npm-token", "npm_", Charset::Alnum, 36),
                // a JSON web token: the token charset includes the dots, so all three parts go
                r("jwt", "eyJ", Charset::Token, 15),
            ],
        }
    }

    /// Adds configured literals and rules.
    pub fn extend(&mut self, literals: &[String], rules: Vec<Rule>) -> Result<()> {
        if self.literals.len() + literals.len() > MAX_REDACT_LITERALS {
            return Err(FleetError::limit(format!(
                "more than {MAX_REDACT_LITERALS} redaction literals"
            )));
        }
        // the built-in rules count too: room for them and for every rule an operator may add
        if self.rules.len() + rules.len() > MAX_REDACT_RULES + 32 {
            return Err(FleetError::limit(format!(
                "more than {MAX_REDACT_RULES} redaction rules"
            )));
        }
        for l in literals {
            if l.len() < 6 {
                return Err(FleetError::invalid(
                    "redaction literals must be at least 6 bytes (shorter values would mangle output)",
                ));
            }
            if l.contains('\n') {
                return Err(FleetError::invalid(
                    "redaction literals must be single-line",
                ));
            }
            self.literals.push(l.clone());
        }
        for rule in rules {
            if rule.prefix.is_empty()
                || rule.prefix.len() > 64
                || rule.min_len == 0
                || rule.min_len > 4096
            {
                return Err(FleetError::invalid(format!(
                    "redaction rule {:?} needs a prefix (1-64 bytes) and min_len 1-4096",
                    rule.name
                )));
            }
            self.rules.push(rule);
        }
        Ok(())
    }

    /// Adds the values of secret-looking variables (`*TOKEN*`, `*SECRET*`, ...) of the given
    /// environment so that a worker that finds one and prints it does not store it.
    pub fn add_env_values<I: IntoIterator<Item = (String, String)>>(&mut self, env: I) {
        for (name, value) in env {
            if self.literals.len() >= MAX_REDACT_LITERALS {
                break;
            }
            let lower = name.to_ascii_lowercase();
            let secret_name = SECRET_NAME_PARTS.iter().any(|p| lower.contains(p))
                || lower.ends_with("_key")
                || lower.ends_with("_pass");
            if secret_name
                && value.len() >= 8
                && !value.contains('\n')
                && !self.literals.contains(&value)
            {
                self.literals.push(value);
            }
        }
    }

    pub fn literal_count(&self) -> usize {
        self.literals.len()
    }

    /// Redacts one line (without its newline).
    pub fn redact<'a>(&self, line: &'a str) -> Cow<'a, str> {
        let mut cur: Cow<'a, str> = Cow::Borrowed(line);
        for lit in &self.literals {
            if cur.contains(lit.as_str()) {
                cur = Cow::Owned(cur.replace(lit.as_str(), "[REDACTED]"));
            }
        }
        for rule in &self.rules {
            if cur.contains(rule.prefix.as_str()) {
                cur = Cow::Owned(apply_rule(&cur, rule));
            }
        }
        cur
    }
}

fn apply_rule(line: &str, rule: &Rule) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(pos) = rest.find(rule.prefix.as_str()) {
        let after = &rest[pos + rule.prefix.len()..];
        let run: usize = after
            .chars()
            .take_while(|c| rule.charset.contains(*c))
            .map(char::len_utf8)
            .sum();
        let count = after[..run].chars().count();
        out.push_str(&rest[..pos]);
        if count >= rule.min_len {
            out.push_str("[REDACTED:");
            out.push_str(&rule.name);
            out.push(']');
            rest = &after[run..];
        } else {
            out.push_str(&rest[pos..pos + rule.prefix.len()]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Stateful per-stream wrapper: tracks PEM private-key blocks spanning several lines.
#[derive(Debug)]
pub struct StreamRedactor {
    inner: std::sync::Arc<Redactor>,
    in_pem: bool,
    pem_lines: usize,
}

/// A real key block is far shorter; the cap keeps a stray BEGIN line from hiding output forever.
const MAX_PEM_LINES: usize = 200;

impl StreamRedactor {
    pub fn new(inner: std::sync::Arc<Redactor>) -> Self {
        StreamRedactor {
            inner,
            in_pem: false,
            pem_lines: 0,
        }
    }

    /// Redacts a not-yet-terminated line for display without changing the block state.
    pub fn peek<'a>(&self, line: &'a str) -> Cow<'a, str> {
        if self.in_pem || (line.contains("-----BEGIN ") && line.contains("PRIVATE KEY-----")) {
            return Cow::Borrowed("");
        }
        self.inner.redact(line)
    }

    /// Redacts one line. Returns `None` for lines that are part of a private-key block.
    pub fn line<'a>(&mut self, line: &'a str) -> Option<Cow<'a, str>> {
        let is_begin = line.contains("-----BEGIN ") && line.contains("PRIVATE KEY-----");
        let is_end = line.contains("-----END ") && line.contains("PRIVATE KEY-----");
        if self.in_pem {
            self.pem_lines += 1;
            if is_end || self.pem_lines > MAX_PEM_LINES {
                self.in_pem = false;
            }
            return None;
        }
        if is_begin {
            self.in_pem = !is_end;
            self.pem_lines = 0;
            return Some(Cow::Borrowed("[REDACTED:private-key]"));
        }
        Some(self.inner.redact(line))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn github_style_token_is_redacted() {
        let r = Redactor::builtin();
        let tok = format!("ghp_{}", "a1B2".repeat(10));
        let line = format!("export TOKEN={tok} done");
        assert_eq!(r.redact(&line), "export TOKEN=[REDACTED:github-token] done");
    }

    #[test]
    fn short_runs_are_left_alone() {
        let r = Redactor::builtin();
        assert_eq!(r.redact("ghp_short and sk-abc"), "ghp_short and sk-abc");
    }

    #[test]
    fn stripe_npm_and_jwt_shapes_are_built_in() {
        let r = Redactor::builtin();
        let stripe = format!("{}{}", "sk_live_", "a1B2".repeat(6));
        let hook = format!("{}{}", "whsec_", "a1B2".repeat(6));
        let npm = format!("{}{}", "npm_", "a1B2".repeat(10));
        let jwt = [
            "eyJhbGciOiJIUzI1NiJ9",
            "eyJzdWIiOiIxMjM0NTY3",
            "c2lnbmF0dXJlLXZhbHVl",
        ]
        .join(".");
        for (name, secret) in [
            ("stripe-key", &stripe),
            ("stripe-webhook", &hook),
            ("npm-token", &npm),
            ("jwt", &jwt),
        ] {
            let line = format!("value: {secret} end");
            let out = r.redact(&line);
            assert!(out.contains(&format!("[REDACTED:{name}]")), "{name}: {out}");
            assert!(
                !out.contains(secret.as_str()) && out.ends_with(" end"),
                "{name}: {out}"
            );
        }
        // a short word that starts like one is left alone
        assert_eq!(r.redact("eyJ is a prefix"), "eyJ is a prefix");
        assert_eq!(r.redact("npm_run_build"), "npm_run_build");
    }

    #[test]
    fn operators_keep_room_for_their_own_rules_beside_the_built_in_ones() {
        let mut r = Redactor::builtin();
        let rules: Vec<Rule> = (0..MAX_REDACT_RULES)
            .map(|i| Rule {
                name: format!("r{i}"),
                prefix: format!("zz{i}-"),
                charset: Charset::Alnum,
                min_len: 8,
            })
            .collect();
        assert!(r.extend(&[], rules).is_ok());
    }
    #[test]
    fn aws_and_bearer_shapes() {
        let r = Redactor::builtin();
        let line = format!(
            "key AKIA{} auth: Bearer {}",
            "A1B2C3D4E5F6G7H8",
            "x".repeat(20)
        );
        let out = r.redact(&line);
        assert!(out.contains("[REDACTED:aws-access-key]"));
        assert!(out.contains("[REDACTED:bearer-token]"));
        assert!(!out.contains("AKIA"));
    }

    #[test]
    fn literals_and_env_values() {
        let mut r = Redactor::builtin();
        r.add_env_values([
            ("MY_API_TOKEN".to_string(), "hunter2hunter2".to_string()),
            (
                "PATH".to_string(),
                "/usr/bin:/bin:/usr/local/bin".to_string(),
            ),
            ("SHORT_SECRET".to_string(), "abc".to_string()),
        ]);
        assert_eq!(r.literal_count(), 1);
        assert_eq!(r.redact("pw=hunter2hunter2!"), "pw=[REDACTED]!");
        assert_eq!(r.redact("path /usr/bin:/bin"), "path /usr/bin:/bin");
    }

    #[test]
    fn extend_validates() {
        let mut r = Redactor::default();
        assert!(r.extend(&["abc".to_string()], vec![]).is_err());
        assert!(r.extend(&["abcdefgh".to_string()], vec![]).is_ok());
        let bad = Rule {
            name: "x".into(),
            prefix: String::new(),
            charset: Charset::Alnum,
            min_len: 4,
        };
        assert!(r.extend(&[], vec![bad]).is_err());
    }

    #[test]
    fn pem_blocks_are_suppressed() {
        let mut s = StreamRedactor::new(Arc::new(Redactor::builtin()));
        let lines = [
            "before",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "b3BlbnNzaC1rZXktdjEAAAAA",
            "-----END OPENSSH PRIVATE KEY-----",
            "after",
        ];
        let out: Vec<_> = lines
            .iter()
            .map(|l| s.line(l).map(|c| c.into_owned()))
            .collect();
        assert_eq!(out[0].as_deref(), Some("before"));
        assert_eq!(out[1].as_deref(), Some("[REDACTED:private-key]"));
        assert_eq!(out[2], None);
        assert_eq!(out[3], None);
        assert_eq!(out[4].as_deref(), Some("after"));
    }

    #[test]
    fn unterminated_pem_block_does_not_hide_output_forever() {
        let mut s = StreamRedactor::new(Arc::new(Redactor::builtin()));
        assert!(s.line("-----BEGIN PRIVATE KEY-----").is_some());
        for _ in 0..MAX_PEM_LINES {
            assert!(s.line("x").is_none());
        }
        assert!(s.line("x").is_none());
        assert_eq!(s.line("visible again").as_deref(), Some("visible again"));
    }

    #[test]
    fn custom_rule_with_hex_charset() {
        let mut r = Redactor::default();
        r.extend(
            &[],
            vec![Rule {
                name: "internal".into(),
                prefix: "int_".into(),
                charset: Charset::Hex,
                min_len: 8,
            }],
        )
        .unwrap();
        assert_eq!(r.redact("a int_deadbeef01 b"), "a [REDACTED:internal] b");
        assert_eq!(r.redact("int_xyz"), "int_xyz");
    }
}
