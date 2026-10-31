//! What the compiler refuses to show: file names and file contents that the repository's secret
//! scanner (`scripts/secret-scan.sh`) refuses to let into a commit.
//!
//! The rules are the scanner's, written out here without a regular-expression engine (the domain
//! has none): the same prefixes, character sets and minimum lengths, matched line by line. The scanner's
//! `secret-scan: allow` marker is deliberately *not* honoured: it is a reviewed exception for the
//! commit gate, and a file that needs one is still not something to copy into an agent's context.
//! A match excludes the whole file, so a key cut in half by an excerpt boundary cannot slip through.

fn is_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

fn is_token(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn is_assign_value(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'_' | b'=' | b'-')
}

fn run(s: &[u8], from: usize, pred: fn(u8) -> bool) -> usize {
    s.get(from..)
        .map_or(0, |rest| rest.iter().take_while(|b| pred(**b)).count())
}

/// Start offsets of every occurrence of `needle` in `hay`.
fn occurrences<'a>(hay: &'a [u8], needle: &'a [u8]) -> impl Iterator<Item = usize> + 'a {
    let count = (hay.len() + 1).saturating_sub(needle.len());
    (0..count).filter(move |i| hay.get(*i..*i + needle.len()) == Some(needle))
}

fn starts_with_at(s: &[u8], at: usize, lit: &[u8]) -> bool {
    s.get(at..at + lit.len()) == Some(lit)
}

/// `prefix` followed by at least `min` characters of the class.
fn prefixed(line: &[u8], prefix: &[u8], class: fn(u8) -> bool, min: usize) -> bool {
    occurrences(line, prefix).any(|i| run(line, i + prefix.len(), class) >= min)
}

fn private_key_block(line: &[u8]) -> bool {
    occurrences(line, b"-----BEGIN ").any(|i| {
        let start = i + b"-----BEGIN ".len();
        let n = run(line, start, |b| b.is_ascii_uppercase() || b == b' ');
        let body = line.get(start..start + n).unwrap_or_default();
        body.ends_with(b"PRIVATE KEY") && starts_with_at(line, start + n, b"-----")
    })
}

fn aws_access_key_id(line: &[u8]) -> bool {
    [b"AKIA", b"ASIA"].iter().any(|prefix| {
        occurrences(line, *prefix).any(|i| {
            let before_ok = i == 0 || line.get(i - 1).is_some_and(|b| !is_alnum(*b));
            let body = i + 4;
            let upper_digit = |b: u8| b.is_ascii_uppercase() || b.is_ascii_digit();
            let n = run(line, body, upper_digit);
            let after_ok = line.get(body + 16).is_none_or(|b| !is_alnum(*b));
            before_ok && n >= 16 && after_ok
        })
    })
}

fn github_token(line: &[u8]) -> bool {
    [b"ghp_", b"gho_", b"ghu_", b"ghs_", b"ghr_"]
        .iter()
        .any(|p| prefixed(line, *p, is_alnum, 30))
}

fn stripe_key(line: &[u8]) -> bool {
    [b"sk_live_", b"sk_test_", b"rk_live_", b"rk_test_"]
        .iter()
        .any(|p| prefixed(line, *p, is_alnum, 16))
}

fn slack_token(line: &[u8]) -> bool {
    [b'a', b'b', b'p', b'r', b's'].iter().any(|kind| {
        let prefix = [b'x', b'o', b'x', *kind, b'-'];
        prefixed(line, &prefix, is_token, 10)
    })
}

fn jwt(line: &[u8]) -> bool {
    occurrences(line, b"eyJ").any(|i| {
        let first = i + 3;
        let n1 = run(line, first, is_token);
        if n1 < 15 || line.get(first + n1) != Some(&b'.') {
            return false;
        }
        let second = first + n1 + 1;
        if !starts_with_at(line, second, b"eyJ") {
            return false;
        }
        let n2 = run(line, second + 3, is_token);
        if n2 < 15 || line.get(second + 3 + n2) != Some(&b'.') {
            return false;
        }
        run(line, second + 3 + n2 + 1, is_token) >= 10
    })
}

fn url_with_password(line: &[u8]) -> bool {
    let user_ok = |b: u8| !(b == b'/' || is_space(b) || b == b':' || b == b'@');
    let pass_ok = |b: u8| !(b == b'/' || is_space(b) || b == b'@');
    let host_ok = |b: u8| !(b == b'/' || is_space(b));
    occurrences(line, b"://").any(|i| {
        let scheme_has_letter = line
            .get(..i)
            .unwrap_or_default()
            .iter()
            .rev()
            .take_while(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(**b, b'+' | b'.' | b'-')
            })
            .any(|b| b.is_ascii_lowercase());
        if !scheme_has_letter {
            return false;
        }
        let user = i + 3;
        let nu = run(line, user, user_ok);
        if nu == 0 || line.get(user + nu) != Some(&b':') {
            return false;
        }
        let pass = user + nu + 1;
        let np = run(line, pass, pass_ok);
        if np < 6 || line.get(pass + np) != Some(&b'@') {
            return false;
        }
        run(line, pass + np + 1, host_ok) >= 1
    })
}

/// The names of the scanner's `quoted-secret-assignment` rule, each as the possible lengths of
/// the text it matches at a position.
fn assignment_name_lengths(line: &[u8], at: usize) -> Vec<usize> {
    let mut out = Vec::new();
    for lit in [&b"secret"[..], b"token", b"passwd", b"password"] {
        if starts_with_at(line, at, lit) {
            out.push(lit.len());
        }
    }
    for (head, tail) in [(&b"api"[..], &b"key"[..]), (b"private", b"key")] {
        if starts_with_at(line, at, head) {
            let after = at + head.len();
            if starts_with_at(line, after, tail) {
                out.push(head.len() + tail.len());
            }
            if matches!(line.get(after), Some(b'_') | Some(b'-'))
                && starts_with_at(line, after + 1, tail)
            {
                out.push(head.len() + 1 + tail.len());
            }
        }
    }
    out
}

fn quoted_secret_assignment(line: &[u8]) -> bool {
    let quote = |b: u8| b == b'"' || b == b'\'';
    (0..line.len()).any(|at| {
        assignment_name_lengths(line, at).into_iter().any(|len| {
            let mut p = at + len;
            if line.get(p).is_some_and(|b| quote(*b)) {
                p += 1;
            }
            p += run(line, p, is_space);
            if !matches!(line.get(p), Some(b':') | Some(b'=')) {
                return false;
            }
            p += 1;
            p += run(line, p, is_space);
            if !line.get(p).is_some_and(|b| quote(*b)) {
                return false;
            }
            p += 1;
            let n = run(line, p, is_assign_value);
            n >= 24 && line.get(p + n).is_some_and(|b| quote(*b))
        })
    })
}

fn raw_private_hex(line: &[u8]) -> bool {
    occurrences(line, b"0x").any(|i| {
        let before_ok = i == 0 || line.get(i - 1).is_some_and(|b| !is_hex(*b));
        let body = i + 2;
        let n = run(line, body, is_hex);
        // exactly 64 digits: the run is maximal, so a longer one is not a match
        before_ok && n == 64
    })
}

/// Rule name and matcher, in the scanner's order.
type Rule = (&'static str, fn(&[u8]) -> bool);

const RULES: [Rule; 14] = [
    ("private-key-block", private_key_block),
    ("aws-access-key-id", aws_access_key_id),
    ("github-token", github_token),
    ("github-fine-grained-token", |l| {
        prefixed(l, b"github_pat_", is_word, 30)
    }),
    ("stripe-key", stripe_key),
    ("stripe-webhook-secret", |l| {
        prefixed(l, b"whsec_", is_alnum, 16)
    }),
    ("slack-token", slack_token),
    ("google-api-key", |l| prefixed(l, b"AIza", is_token, 35)),
    ("npm-token", |l| prefixed(l, b"npm_", is_alnum, 36)),
    ("sk-style-key", |l| prefixed(l, b"sk-", is_token, 32)),
    ("jwt", jwt),
    ("url-with-password", url_with_password),
    ("quoted-secret-assignment", quoted_secret_assignment),
    ("raw-private-hex", raw_private_hex),
];

/// The name of the first rule that matches a line of text.
pub fn scan_line(line: &str) -> Option<&'static str> {
    let bytes = line.as_bytes();
    RULES
        .iter()
        .find(|(_, matcher)| matcher(bytes))
        .map(|(name, _)| *name)
}

/// The name of the first rule that matches any line of `text`.
pub fn scan_text(text: &str) -> Option<&'static str> {
    text.lines().find_map(scan_line)
}

/// Like [`scan_text`] for bytes that may not be UTF-8: invalid sequences are replaced, which
/// cannot hide an ASCII key shape.
pub fn scan_bytes(bytes: &[u8]) -> Option<&'static str> {
    scan_text(&String::from_utf8_lossy(bytes))
}

/// Every rule name, for documentation and tests.
pub fn rule_names() -> Vec<&'static str> {
    RULES.iter().map(|(n, _)| *n).collect()
}

/// True for a file name (the last path component) the scanner refuses to see in a repository.
pub fn forbidden_name(base: &str) -> bool {
    let env_like = base == ".env" || base.starts_with(".env.") || base.ends_with(".env");
    if env_like {
        let template = matches!(base, ".env.example" | ".env.sample" | ".env.template")
            || base.ends_with(".env.example");
        return !template;
    }
    const SUFFIXES: [&str; 7] = [
        ".pem",
        ".key",
        ".p12",
        ".pfx",
        ".jks",
        ".keystore",
        ".tfstate",
    ];
    if SUFFIXES.iter().any(|s| base.ends_with(s)) {
        return true;
    }
    if base.contains(".tfstate.") {
        return true;
    }
    for stem in ["id_rsa", "id_ed25519", "id_ecdsa"] {
        if base == stem || base.starts_with(&format!("{stem}.")) {
            return true;
        }
    }
    if base.starts_with("service-account") && base.ends_with(".json") {
        return true;
    }
    matches!(
        base,
        ".netrc"
            | "credentials.json"
            | "secrets.json"
            | "secrets.yml"
            | "secrets.yaml"
            | "secrets.toml"
    )
}

/// The last component of a `/`-separated path.
pub fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Why a file must not be shown, if it must not: its name, or something in its content.
pub fn refusal(path: &str, content: &[u8]) -> Option<String> {
    if path.split('/').any(forbidden_name) {
        return Some("forbidden file name".to_string());
    }
    scan_bytes(content).map(|rule| format!("matches secret rule {rule}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad(prefix: &str, ch: char, n: usize) -> String {
        format!("{prefix}{}", ch.to_string().repeat(n))
    }

    /// One shape per rule, built at run time so that this file holds no key-shaped text. Each
    /// pair is a positive and the same shape one character short.
    fn shapes() -> Vec<(&'static str, String, String)> {
        let pem = |label: &str| format!("{}{}{}", "-----BEGIN ", label, "PRIVATE KEY-----");
        vec![
            (
                "private-key-block",
                pem("RSA "),
                pem("RSA ").replace("PRIVATE KEY-----", "PUBLIC KEY-----"),
            ),
            (
                "aws-access-key-id",
                format!("id={}", pad("AKIA", 'A', 16)),
                format!("id={}", pad("AKIA", 'A', 15)),
            ),
            ("github-token", pad("ghp_", 'a', 30), pad("ghp_", 'a', 29)),
            (
                "github-fine-grained-token",
                pad("github_pat_", 'a', 30),
                pad("github_pat_", 'a', 29),
            ),
            (
                "stripe-key",
                pad("sk_live_", 'a', 16),
                pad("sk_live_", 'a', 15),
            ),
            (
                "stripe-webhook-secret",
                pad("whsec_", 'a', 16),
                pad("whsec_", 'a', 15),
            ),
            ("slack-token", pad("xoxb-", 'a', 10), pad("xoxb-", 'a', 9)),
            ("google-api-key", pad("AIza", 'a', 35), pad("AIza", 'a', 34)),
            ("npm-token", pad("npm_", 'a', 36), pad("npm_", 'a', 35)),
            ("sk-style-key", pad("sk-", 'a', 32), pad("sk-", 'a', 31)),
            (
                "jwt",
                format!(
                    "{}.{}.{}",
                    pad("eyJ", 'a', 15),
                    pad("eyJ", 'b', 15),
                    "c".repeat(10)
                ),
                format!(
                    "{}.{}.{}",
                    pad("eyJ", 'a', 15),
                    pad("eyJ", 'b', 15),
                    "c".repeat(9)
                ),
            ),
            (
                "url-with-password",
                format!("{}://user:{}@host.example", "postgres", "p".repeat(6)),
                format!("{}://user:{}@host.example", "postgres", "p".repeat(5)),
            ),
            (
                "quoted-secret-assignment",
                format!("{} = \"{}\"", "api_key", "a".repeat(24)),
                format!("{} = \"{}\"", "api_key", "a".repeat(23)),
            ),
            (
                "raw-private-hex",
                format!("k = {}", pad("0x", 'a', 64)),
                format!("k = {}", pad("0x", 'a', 63)),
            ),
        ]
    }

    #[test]
    fn every_rule_of_the_scanner_matches_its_shape_and_not_a_near_miss() {
        let all = shapes();
        assert_eq!(all.len(), RULES.len());
        for ((name, hit, miss), rule) in all.iter().zip(rule_names()) {
            assert_eq!(*name, rule, "rules are listed in the scanner's order");
            assert_eq!(scan_line(hit), Some(*name), "{name}: {hit}");
            assert_eq!(scan_line(miss), None, "{name}: near miss {miss}");
        }
    }

    #[test]
    fn shapes_match_in_the_middle_of_a_line_and_in_multi_line_text() {
        for (name, hit, _) in shapes() {
            let text = format!("first line\nlet x = \"{hit}\"; // trailing\nlast");
            assert_eq!(scan_text(&text), Some(name), "{name}");
            assert_eq!(scan_bytes(text.as_bytes()), Some(name));
        }
        assert_eq!(scan_text("nothing to see\nhere"), None);
        assert_eq!(scan_text(""), None);
    }

    #[test]
    fn boundaries_of_the_aws_and_hex_rules_follow_the_scanner() {
        // a longer run of the same class does not match (the regex needs a non-alphanumeric after 16)
        assert_eq!(scan_line(&pad("AKIA", 'A', 17)), None);
        assert_eq!(scan_line(&format!("x{}", pad("AKIA", 'A', 16))), None);
        assert_eq!(
            scan_line(&format!("({})", pad("AKIA", 'A', 16))),
            Some("aws-access-key-id")
        );
        assert_eq!(scan_line(&pad("0x", 'a', 65)), None);
        assert_eq!(scan_line(&format!("a{}", pad("0x", 'a', 64))), None);
        assert_eq!(
            scan_line(&format!(",{}", pad("0x", 'a', 64))),
            Some("raw-private-hex")
        );
        // the token rules accept longer runs
        assert_eq!(scan_line(&pad("ghp_", 'a', 80)), Some("github-token"));
        assert_eq!(scan_line(&pad("sk-", 'a', 80)), Some("sk-style-key"));
    }

    #[test]
    fn a_secret_assignment_needs_a_quote_and_a_long_value() {
        let long = "a".repeat(30);
        assert_eq!(
            scan_line(&format!("password: '{long}'")),
            Some("quoted-secret-assignment")
        );
        assert_eq!(
            scan_line(&format!("private-key=\"{long}\"")),
            Some("quoted-secret-assignment")
        );
        assert_eq!(
            scan_line(&format!("token = {long}")),
            None,
            "unquoted values are not flagged"
        );
        assert_eq!(scan_line("token = \"short\""), None);
        assert_eq!(
            scan_line(&format!("\"secret\" : \"{long}\"")),
            Some("quoted-secret-assignment")
        );
    }

    #[test]
    fn forbidden_names_follow_the_scanner() {
        for bad in [
            ".env",
            ".env.local",
            "prod.env",
            "server.pem",
            "a.key",
            "a.p12",
            "a.pfx",
            "a.jks",
            "a.keystore",
            "id_rsa",
            "id_rsa.pub",
            "id_ed25519",
            "id_ecdsa.bak",
            ".netrc",
            "credentials.json",
            "service-account-prod.json",
            "terraform.tfstate",
            "terraform.tfstate.backup",
            "secrets.json",
            "secrets.yml",
            "secrets.yaml",
            "secrets.toml",
        ] {
            assert!(forbidden_name(bad), "{bad}");
        }
        for fine in [
            ".env.example",
            ".env.sample",
            ".env.template",
            "local.env.example",
            "README.md",
            "main.rs",
            "keys.rs",
            "id_rsa_notes",
            "secrets.rs",
            "environment",
        ] {
            assert!(!forbidden_name(fine), "{fine}");
        }
    }

    #[test]
    fn refusal_looks_at_every_path_component_and_at_the_content() {
        assert_eq!(refusal("src/main.rs", b"fn main() {}"), None);
        assert!(refusal("config/.env", b"X=1").is_some());
        assert!(
            refusal("secrets.json/inner.txt", b"x").is_some(),
            "a directory named like a secret file"
        );
        let hit = pad("ghp_", 'a', 40);
        let why = refusal("src/a.rs", hit.as_bytes()).unwrap();
        assert!(why.contains("github-token"), "{why}");
        // invalid UTF-8 around an ASCII shape does not hide it
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend_from_slice(hit.as_bytes());
        assert!(refusal("src/a.rs", &bytes).is_some());
        assert_eq!(base_name("a/b/c.rs"), "c.rs");
        assert_eq!(base_name("c.rs"), "c.rs");
    }
}
