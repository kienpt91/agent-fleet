//! Validation of every name that ends up in a path, a branch name, a ref or a command line.

use crate::error::{FleetError, Result};
use crate::limits::MAX_ID_LEN;

/// Ids that would collide with names fleet creates itself.
const RESERVED: [&str; 3] = ["integration", "head", "fleet"];

/// Run, task and worker ids: `[a-z0-9][a-z0-9_-]{0,39}`. No dots, slashes, spaces or uppercase,
/// so they are safe as a path component and as a ref component without further quoting.
pub fn validate_id(kind: &str, id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(FleetError::invalid(format!("{kind} id is empty")));
    }
    if id.len() > MAX_ID_LEN {
        return Err(FleetError::invalid(format!(
            "{kind} id is longer than {MAX_ID_LEN} characters"
        )));
    }
    let mut chars = id.chars();
    let first_ok = matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit());
    if !first_ok || !id.chars().all(is_id_char) {
        return Err(FleetError::invalid(format!(
            "{kind} id {} must match [a-z0-9][a-z0-9_-]*",
            show(id)
        )));
    }
    if RESERVED.contains(&id) {
        return Err(FleetError::invalid(format!(
            "{kind} id {id:?} is reserved"
        )));
    }
    Ok(())
}

fn is_id_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'
}

/// Worker names follow the id rules but may be `default`-style words; reserved words are allowed.
pub fn validate_worker_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_ID_LEN {
        return Err(FleetError::invalid("worker name has a bad length"));
    }
    let first_ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !first_ok || !name.chars().all(is_id_char) {
        return Err(FleetError::invalid(format!(
            "worker name {} must match [a-z0-9][a-z0-9_-]*",
            show(name)
        )));
    }
    Ok(())
}

/// A git ref name or branch name coming from outside (for example `--base`): rejects what git
/// itself rejects plus anything that could be read as an option or smuggle control characters.
pub fn validate_ref_name(name: &str) -> Result<()> {
    let bad = |why: &str| {
        Err(FleetError::invalid(format!(
            "ref name {} is not allowed: {why}",
            show(name)
        )))
    };
    if name.is_empty() {
        return bad("empty");
    }
    if name.len() > 200 {
        return bad("too long");
    }
    if name.starts_with('-') {
        return bad("starts with '-'");
    }
    if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
        return bad("empty path component");
    }
    if name.contains("..") {
        return bad("contains '..'");
    }
    if name.contains("@{") || name == "@" {
        return bad("contains '@{'");
    }
    if name.ends_with('.') || name.ends_with(".lock") {
        return bad("ends with '.' or '.lock'");
    }
    for c in name.chars() {
        if c.is_control() || c == ' ' || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\') {
            return bad("contains a forbidden character");
        }
        if !c.is_ascii() {
            return bad("contains a non-ASCII character");
        }
    }
    for part in name.split('/') {
        if part.starts_with('.') || part.ends_with(".lock") {
            return bad("a path component starts with '.' or ends with '.lock'");
        }
    }
    Ok(())
}

/// Names of the branches fleet creates.
pub fn task_branch(run: &str, task: &str) -> String {
    format!("fleet/{run}/{task}")
}

pub fn integration_branch(run: &str) -> String {
    format!("fleet/{run}/integration")
}

/// Single-line free text (titles): no control characters, bounded.
pub fn validate_line(kind: &str, text: &str, max: usize) -> Result<()> {
    if text.trim().is_empty() {
        return Err(FleetError::invalid(format!("{kind} is empty")));
    }
    if text.len() > max {
        return Err(FleetError::invalid(format!(
            "{kind} is longer than {max} bytes"
        )));
    }
    if text.chars().any(|c| c.is_control() || is_invisible(c)) {
        return Err(FleetError::invalid(format!(
            "{kind} contains control or invisible characters"
        )));
    }
    Ok(())
}

/// Multi-line free text (specs, answers): newlines and tabs allowed, other control characters not.
pub fn validate_text(kind: &str, text: &str, max: usize) -> Result<()> {
    if text.len() > max {
        return Err(FleetError::invalid(format!(
            "{kind} is longer than {max} bytes"
        )));
    }
    if text
        .chars()
        .any(|c| (c.is_control() && c != '\n' && c != '\t') || is_invisible(c))
    {
        return Err(FleetError::invalid(format!(
            "{kind} contains control or invisible characters"
        )));
    }
    Ok(())
}

/// Characters that change how text is displayed without being visible themselves.
pub fn is_invisible(c: char) -> bool {
    matches!(c as u32,
        0x061C | 0x200B | 0x200E | 0x200F | 0x2028 | 0x2029 | 0x202A..=0x202E
        | 0x2060..=0x2064 | 0x2066..=0x2069 | 0xFEFF | 0xFFF9..=0xFFFB
        | 0xE0000..=0xE007F)
}

/// Shows a possibly hostile name in an error message without letting it break the terminal.
pub fn show(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(64) + 2);
    out.push('"');
    for c in s.chars().take(64) {
        if c.is_control() || is_invisible(c) {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else if c == '"' || c == '\\' {
            out.push('\\');
            out.push(c);
        } else {
            out.push(c);
        }
    }
    if s.chars().count() > 64 {
        out.push('…');
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_accept_plain_names() {
        for ok in ["a", "t1", "build-api", "x_y", "0day", "r12"] {
            assert!(validate_id("task", ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn ids_reject_attacks() {
        let long = "a".repeat(41);
        for bad in [
            "",
            "-rf",
            "_hidden",
            "A",
            "a b",
            "a/b",
            "a.b",
            "..",
            "a\n",
            "a\0",
            "é",
            "integration",
            "head",
            long.as_str(),
        ] {
            assert!(validate_id("task", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ref_names_reject_git_and_option_tricks() {
        for bad in [
            "",
            "-x",
            "--upload-pack=evil",
            "a..b",
            "a b",
            "a\tb",
            "a~1",
            "a^",
            "a:b",
            "a?",
            "a*",
            "a[",
            "a\\b",
            "a@{u}",
            "refs//x",
            "/x",
            "x/",
            "x.lock",
            "x.",
            ".hidden",
            "a/.b",
            "ü",
            "a\u{202e}b",
        ] {
            assert!(validate_ref_name(bad).is_err(), "{bad:?}");
        }
        for ok in ["main", "feature/x-1", "release_2.0", "fleet/r1/t1"] {
            assert!(validate_ref_name(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn show_escapes_controls() {
        assert_eq!(show("a\u{1b}[31m"), "\"a\\u{1b}[31m\"");
    }

    #[test]
    fn text_rules() {
        assert!(validate_line("title", "hello", 10).is_ok());
        assert!(validate_line("title", "hello\nworld", 20).is_err());
        assert!(validate_line("title", "   ", 20).is_err());
        assert!(validate_text("spec", "a\nb\tc", 10).is_ok());
        assert!(validate_text("spec", "a\u{1b}b", 10).is_err());
        assert!(validate_text("spec", "a\u{202e}b", 10).is_err());
        assert!(validate_text("spec", "abcdef", 3).is_err());
    }
}
