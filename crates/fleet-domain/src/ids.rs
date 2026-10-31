//! Validation of every name that ends up in a path, a branch name, a ref or a command line.

use crate::error::{FleetError, Result};
use crate::limits::MAX_ID_LEN;
use crate::value::{Field, Value};

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
        return Err(FleetError::invalid(format!("{kind} id {id:?} is reserved")));
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
        0x00AD | 0x034F | 0x061C | 0x115F | 0x1160 | 0x17B4 | 0x17B5 | 0x180B..=0x180F
        | 0x200B | 0x200E | 0x200F | 0x2028 | 0x2029 | 0x202A..=0x202E
        | 0x2060..=0x2064 | 0x2066..=0x2069 | 0x2800 | 0x3164 | 0xFE00..=0xFE0F | 0xFEFF
        | 0xFFA0 | 0xFFF9..=0xFFFB | 0xE0000..=0xE007F | 0xE0100..=0xE01EF)
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

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident, $check:expr) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn parse(s: &str) -> Result<Self> {
                let check: fn(&str) -> Result<()> = $check;
                check(s)?;
                Ok($name(s.to_string()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Field for $name {
            fn to_value(&self) -> Value {
                Value::Str(self.0.clone())
            }
            fn from_value(v: &Value) -> Option<Self> {
                v.as_str().and_then(|s| $name::parse(s).ok())
            }
        }
    };
}

string_id!(
    /// Identifies a run. Appears in branch names and paths.
    RunId,
    |s| validate_id("run", s)
);
string_id!(
    /// Identifies a task inside a run. Appears in branch names and paths.
    TaskId,
    |s| validate_id("task", s)
);
string_id!(
    /// Name of a worker definition in the daemon configuration.
    WorkerName,
    validate_worker_name
);
string_id!(
    /// Identifies a decision gate: `g` followed by a number.
    DecisionId,
    |s| validate_counter_id("decision gate", 'g', s)
);
string_id!(
    /// Identifies an approval: `a` followed by a number.
    ApprovalId,
    |s| validate_counter_id("approval", 'a', s)
);
string_id!(
    /// Identifies a goal of the goal ledger. Same character rules as run and task ids.
    GoalId,
    |s| validate_id("goal", s)
);
string_id!(
    /// Identifies one piece of evidence: `e` followed by a number.
    EvidenceId,
    |s| validate_counter_id("evidence", 'e', s)
);
string_id!(
    /// A git object id in lowercase hex (SHA-1 or SHA-256 repositories).
    Sha,
    validate_sha
);

impl DecisionId {
    pub fn from_seq(n: u64) -> Self {
        DecisionId(format!("g{n}"))
    }
}

impl ApprovalId {
    pub fn from_seq(n: u64) -> Self {
        ApprovalId(format!("a{n}"))
    }
}

impl RunId {
    pub fn from_seq(n: u64) -> Self {
        RunId(format!("r{n}"))
    }
}

impl TaskId {
    pub fn from_seq(n: u64) -> Self {
        TaskId(format!("t{n}"))
    }
}

impl GoalId {
    /// The id the ledger gives a goal that was added without a name: `goal-<n>`.
    pub fn from_seq(n: u64) -> Self {
        GoalId(format!("goal-{n}"))
    }
}

impl EvidenceId {
    pub fn from_seq(n: u64) -> Self {
        EvidenceId(format!("e{n}"))
    }

    /// The number after the `e`; evidence is ordered by it.
    pub fn seq(&self) -> u64 {
        self.0
            .strip_prefix('e')
            .and_then(|d| d.parse().ok())
            .unwrap_or(0)
    }
}

fn validate_counter_id(kind: &str, prefix: char, s: &str) -> Result<()> {
    let ok = s.len() <= 20
        && s.strip_prefix(prefix)
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()));
    if ok {
        Ok(())
    } else {
        Err(FleetError::invalid(format!(
            "{kind} id {} must look like {prefix}1",
            show(s)
        )))
    }
}

fn validate_sha(s: &str) -> Result<()> {
    let ok = (s.len() == 40 || s.len() == 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if ok {
        Ok(())
    } else {
        Err(FleetError::invalid(format!(
            "object id {} is not 40 or 64 lowercase hex digits",
            show(s)
        )))
    }
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
    fn goal_and_evidence_ids() {
        assert_eq!(GoalId::from_seq(3).as_str(), "goal-3");
        assert!(GoalId::parse(GoalId::from_seq(3).as_str()).is_ok());
        assert!(GoalId::parse("Goal").is_err());
        assert!(GoalId::parse("a/b").is_err());
        let e = EvidenceId::from_seq(12);
        assert_eq!(e.as_str(), "e12");
        assert_eq!(e.seq(), 12);
        assert!(EvidenceId::parse("e12").is_ok());
        for bad in ["12", "e", "e1x", "g1", "e-1"] {
            assert!(EvidenceId::parse(bad).is_err(), "{bad}");
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
    fn invisible_characters_are_recognised() {
        let invisible = [
            0x00ad, 0x034f, 0x061c, 0x115f, 0x1160, 0x17b4, 0x17b5, 0x180b, 0x180c, 0x180d, 0x180e,
            0x180f, 0x200b, 0x200e, 0x200f, 0x2028, 0x2029, 0x202a, 0x202e, 0x2060, 0x2064, 0x2066,
            0x2069, 0x2800, 0x3164, 0xfe00, 0xfe0f, 0xfeff, 0xffa0, 0xfff9, 0xfffb, 0xe0000,
            0xe007f, 0xe0100, 0xe01ef,
        ];
        for cp in invisible {
            let c = char::from_u32(cp).unwrap();
            assert!(is_invisible(c), "U+{cp:04X} must be invisible");
            assert!(
                validate_line("title", &format!("a{c}b"), 20).is_err(),
                "U+{cp:04X}"
            );
        }
        // Neighbours and ordinary text stay visible.
        for c in [
            'a',
            ' ',
            'é',
            '\u{ac}',
            '\u{ae}',
            '\u{2801}',
            '\u{3165}',
            '\u{fe10}',
            '\u{e01f0}',
            '日',
        ] {
            assert!(!is_invisible(c), "{c:?} must stay visible");
        }
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

    #[test]
    fn typed_ids_validate_on_parse() {
        assert!(RunId::parse("r1").is_ok());
        assert!(RunId::parse("../x").is_err());
        assert!(TaskId::parse("build-api").is_ok());
        assert!(TaskId::parse("Build").is_err());
        assert!(WorkerName::parse("default").is_ok());
        assert!(WorkerName::parse("a b").is_err());
        assert_eq!(DecisionId::from_seq(3).as_str(), "g3");
        assert_eq!(ApprovalId::from_seq(12).to_string(), "a12");
        assert_eq!(RunId::from_seq(2).as_str(), "r2");
        assert_eq!(TaskId::from_seq(9).as_str(), "t9");
        assert!(DecisionId::parse("g").is_err());
        assert!(DecisionId::parse("a1").is_err());
        assert!(ApprovalId::parse("a1x").is_err());
        assert!(ApprovalId::parse("a1").is_ok());
    }

    #[test]
    fn sha_accepts_only_lowercase_hex() {
        assert!(Sha::parse(&"a".repeat(40)).is_ok());
        assert!(Sha::parse(&"0".repeat(64)).is_ok());
        assert!(Sha::parse(&"A".repeat(40)).is_err());
        assert!(Sha::parse(&"a".repeat(39)).is_err());
        assert!(Sha::parse("--upload-pack=x").is_err());
    }

    #[test]
    fn typed_ids_decode_from_values_with_validation() {
        assert_eq!(
            RunId::from_value(&Value::str("r1")).map(|r| r.to_string()),
            Some("r1".to_string())
        );
        assert_eq!(RunId::from_value(&Value::str("../r")), None);
        assert_eq!(RunId::from_value(&Value::int(1)), None);
        assert_eq!(
            TaskId::from_value(&TaskId::from_seq(4).to_value()),
            Some(TaskId::from_seq(4))
        );
    }
}
