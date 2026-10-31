//! The verification contract: the checks that decide whether a goal's work counts as done.
//!
//! ```json
//! {"version": 1, "checks": [
//!   {"id": "tests",  "kind": "command", "program": "/usr/bin/make", "args": ["test"], "expect_exit": 0},
//!   {"id": "refs",   "kind": "refcheck-json", "program": "/usr/local/bin/refcheck", "args": ["--json", "."]},
//!   {"id": "readme", "kind": "file", "path": "README.md", "assert": "contains", "value": "## Usage"},
//!   {"id": "scope",  "kind": "diff", "allow": ["src/**", "tests/**"]}
//! ]}
//! ```
//!
//! Every field is validated strictly: unknown keys, wrong types, out-of-range numbers, duplicate
//! check ids and oversized lists are errors, never silently ignored. Optional fields have the
//! defaults written out in [`VerificationContract::to_value`], so two spellings of the same
//! contract have the same canonical text and therefore the same id.

use crate::evidence::{ContractState, Evidence};
use crate::glob::Glob;
use crate::limits::{
    DEFAULT_TIMEOUT_MS, MAX_ARGS, MAX_ARG_LEN, MAX_CHECKS, MAX_CHECK_ID, MAX_CONTRACT_BYTES,
    MAX_GLOBS, MAX_NEEDLE, MAX_PATH_LEN, MAX_PROGRAM_LEN, MAX_TIMEOUT_MS, MIN_TIMEOUT_MS,
};
use fleet_domain::approval::Digest;
use fleet_domain::ids::{is_invisible, show};
use fleet_domain::value::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const CONTRACT_VERSION: u64 = 1;

/// Name of a check inside one contract: `[a-z0-9][a-z0-9_-]{0,39}`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CheckId(String);

impl CheckId {
    pub fn parse(s: &str) -> Result<CheckId, String> {
        let ok = !s.is_empty()
            && s.len() <= MAX_CHECK_ID
            && s.chars().enumerate().all(|(i, c)| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || (i > 0 && matches!(c, '_' | '-'))
            });
        if ok {
            Ok(CheckId(s.to_string()))
        } else {
            Err(format!(
                "check id {} must match [a-z0-9][a-z0-9_-]{{0,{}}}",
                show(s),
                MAX_CHECK_ID - 1
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CheckId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identifies one exact contract: the SHA-256 (64 lowercase hex digits) of its canonical text.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContractId(String);

impl ContractId {
    pub fn parse(s: &str) -> Result<ContractId, String> {
        if is_hex64(s) {
            Ok(ContractId(s.to_string()))
        } else {
            Err(format!(
                "contract id {} is not 64 lowercase hex digits",
                show(s)
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ContractId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileAssertion {
    /// A regular file is there.
    Exists,
    /// The file's SHA-256 is this (64 lowercase hex digits).
    Sha256(String),
    /// The file is UTF-8 text and contains this text.
    Contains(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckKind {
    /// Run `program` (an absolute path) in the worktree; passes when it exits with `expect_exit`.
    Command {
        program: String,
        args: Vec<String>,
        expect_exit: i32,
        timeout_ms: u64,
    },
    /// Run `program`; passes when its stdout is a `refcheck` JSON document without errors (the
    /// same judgement as the `refcheck-json` quality gate).
    RefcheckJson {
        program: String,
        args: Vec<String>,
        timeout_ms: u64,
    },
    /// An assertion about one file below the worktree root.
    File {
        path: String,
        assertion: FileAssertion,
    },
    /// Everything the work changed matches at least one of the globs.
    DiffOnly { allow: Vec<Glob> },
}

impl CheckKind {
    pub fn name(&self) -> &'static str {
        match self {
            CheckKind::Command { .. } => "command",
            CheckKind::RefcheckJson { .. } => "refcheck-json",
            CheckKind::File { .. } => "file",
            CheckKind::DiffOnly { .. } => "diff",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub id: CheckId,
    /// A mandatory check must pass for the contract to pass; an optional one is run and recorded.
    pub mandatory: bool,
    pub kind: CheckKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationContract {
    checks: Vec<Check>,
}

// ---- parsing helpers ---------------------------------------------------------------------------

fn object<'a>(
    v: &'a Value,
    label: &str,
    allowed: &[&str],
) -> Result<&'a BTreeMap<String, Value>, String> {
    let map = v
        .as_object()
        .ok_or_else(|| format!("{label} must be an object"))?;
    if let Some(unknown) = map.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(format!("{label} has an unknown field {}", show(unknown)));
    }
    Ok(map)
}

fn req_str<'a>(
    map: &'a BTreeMap<String, Value>,
    key: &str,
    label: &str,
) -> Result<&'a str, String> {
    match map.get(key) {
        Some(Value::Str(s)) => Ok(s),
        Some(_) => Err(format!("{label}: `{key}` must be a string")),
        None => Err(format!("{label}: `{key}` is required")),
    }
}

fn opt_bool(
    map: &BTreeMap<String, Value>,
    key: &str,
    label: &str,
    default: bool,
) -> Result<bool, String> {
    match map.get(key) {
        None => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("{label}: `{key}` must be true or false")),
    }
}

fn opt_int(
    map: &BTreeMap<String, Value>,
    key: &str,
    label: &str,
    range: (i128, i128),
    default: i128,
) -> Result<i128, String> {
    match map.get(key) {
        None => Ok(default),
        Some(Value::Int(n)) if (range.0..=range.1).contains(n) => Ok(*n),
        Some(Value::Int(_)) => Err(format!(
            "{label}: `{key}` must be between {} and {}",
            range.0, range.1
        )),
        Some(_) => Err(format!("{label}: `{key}` must be a whole number")),
    }
}

fn str_list<'a>(
    map: &'a BTreeMap<String, Value>,
    key: &str,
    label: &str,
    max: usize,
) -> Result<Vec<&'a str>, String> {
    match map.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            if items.len() > max {
                return Err(format!("{label}: `{key}` has more than {max} entries"));
            }
            items
                .iter()
                .map(|i| {
                    i.as_str()
                        .ok_or_else(|| format!("{label}: `{key}` must be a list of strings"))
                })
                .collect()
        }
        Some(_) => Err(format!("{label}: `{key}` must be a list of strings")),
    }
}

/// Text that is used verbatim (arguments, the needle of `contains`): length-bounded, and free of
/// control characters other than newline and tab, and of invisible characters.
fn verbatim(label: &str, s: &str, max: usize) -> Result<(), String> {
    if s.len() > max {
        return Err(format!("{label} is longer than {max} bytes"));
    }
    if s.chars()
        .any(|c| (c.is_control() && c != '\n' && c != '\t') || is_invisible(c))
    {
        return Err(format!("{label} contains control or invisible characters"));
    }
    Ok(())
}

fn program(map: &BTreeMap<String, Value>, label: &str) -> Result<String, String> {
    let p = req_str(map, "program", label)?;
    if p.is_empty() || p.len() > MAX_PROGRAM_LEN {
        return Err(format!("{label}: `program` has a bad length"));
    }
    if !p.starts_with('/') {
        return Err(format!(
            "{label}: `program` must be an absolute path; checks do not search PATH"
        ));
    }
    if p.chars().any(|c| c.is_control() || is_invisible(c)) {
        return Err(format!(
            "{label}: `program` contains control or invisible characters"
        ));
    }
    Ok(p.to_string())
}

fn args(map: &BTreeMap<String, Value>, label: &str) -> Result<Vec<String>, String> {
    str_list(map, "args", label, MAX_ARGS)?
        .into_iter()
        .map(|a| {
            verbatim(&format!("{label}: an argument"), a, MAX_ARG_LEN)?;
            Ok(a.to_string())
        })
        .collect()
}

fn timeout(map: &BTreeMap<String, Value>, label: &str) -> Result<u64, String> {
    let n = opt_int(
        map,
        "timeout_ms",
        label,
        (i128::from(MIN_TIMEOUT_MS), i128::from(MAX_TIMEOUT_MS)),
        i128::from(DEFAULT_TIMEOUT_MS),
    )?;
    u64::try_from(n).map_err(|_| format!("{label}: `timeout_ms` is out of range"))
}

/// A file path below the worktree root: relative, no `.` or `..`, no empty segment.
pub fn validate_relative_path(label: &str, path: &str) -> Result<(), String> {
    if path.is_empty() || path.len() > MAX_PATH_LEN {
        return Err(format!(
            "{label} has a bad length (1 to {MAX_PATH_LEN} bytes)"
        ));
    }
    if path
        .chars()
        .any(|c| c.is_control() || is_invisible(c) || c == '\\')
    {
        return Err(format!(
            "{label} contains control, invisible or backslash characters"
        ));
    }
    if path.starts_with('/') {
        return Err(format!("{label} must be relative to the worktree"));
    }
    if path
        .split('/')
        .any(|s| s.is_empty() || s == "." || s == "..")
    {
        return Err(format!("{label} must not have empty, `.` or `..` segments"));
    }
    Ok(())
}

impl Check {
    fn from_value(v: &Value, index: usize) -> Result<Check, String> {
        let label = format!("check {}", index + 1);
        let kind_name = v
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{label}: `kind` is required and must be a string"))?;
        let common = ["id", "kind", "mandatory"];
        let allowed: Vec<&str> = match kind_name {
            "command" => [
                &common[..],
                &["program", "args", "expect_exit", "timeout_ms"],
            ]
            .concat(),
            "refcheck-json" => [&common[..], &["program", "args", "timeout_ms"]].concat(),
            "file" => [&common[..], &["path", "assert", "value"]].concat(),
            "diff" => [&common[..], &["allow"]].concat(),
            other => {
                return Err(format!(
                    "{label}: unknown kind {} (expected command, refcheck-json, file or diff)",
                    show(other)
                ))
            }
        };
        let map = object(v, &label, &allowed)?;
        let id = CheckId::parse(req_str(map, "id", &label)?)?;
        let label = format!("check {}", show(id.as_str()));
        let mandatory = opt_bool(map, "mandatory", &label, true)?;
        let kind = match kind_name {
            "command" => {
                let expect = opt_int(map, "expect_exit", &label, (0, 255), 0)?;
                CheckKind::Command {
                    program: program(map, &label)?,
                    args: args(map, &label)?,
                    expect_exit: i32::try_from(expect)
                        .map_err(|_| format!("{label}: `expect_exit` is out of range"))?,
                    timeout_ms: timeout(map, &label)?,
                }
            }
            "refcheck-json" => CheckKind::RefcheckJson {
                program: program(map, &label)?,
                args: args(map, &label)?,
                timeout_ms: timeout(map, &label)?,
            },
            "file" => {
                let path = req_str(map, "path", &label)?;
                validate_relative_path(&format!("{label}: `path`"), path)?;
                let value = map.get("value");
                let assertion = match req_str(map, "assert", &label)? {
                    "exists" => {
                        if value.is_some() {
                            return Err(format!("{label}: `exists` takes no `value`"));
                        }
                        FileAssertion::Exists
                    }
                    "sha256" => {
                        let digest = req_str(map, "value", &label)?;
                        if !is_hex64(digest) {
                            return Err(format!(
                                "{label}: `value` must be 64 lowercase hex digits"
                            ));
                        }
                        FileAssertion::Sha256(digest.to_string())
                    }
                    "contains" => {
                        let needle = req_str(map, "value", &label)?;
                        if needle.is_empty() {
                            return Err(format!("{label}: `value` is empty"));
                        }
                        verbatim(&format!("{label}: `value`"), needle, MAX_NEEDLE)?;
                        FileAssertion::Contains(needle.to_string())
                    }
                    other => {
                        return Err(format!(
                            "{label}: unknown assertion {} (expected exists, sha256 or contains)",
                            show(other)
                        ))
                    }
                };
                CheckKind::File {
                    path: path.to_string(),
                    assertion,
                }
            }
            _ => {
                let patterns = str_list(map, "allow", &label, MAX_GLOBS)?;
                if patterns.is_empty() {
                    return Err(format!("{label}: `allow` needs at least one glob"));
                }
                CheckKind::DiffOnly {
                    allow: patterns
                        .into_iter()
                        .map(Glob::new)
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|e| format!("{label}: {e}"))?,
                }
            }
        };
        Ok(Check {
            id,
            mandatory,
            kind,
        })
    }

    fn to_value(&self) -> Value {
        let mut pairs: Vec<(&str, Value)> = vec![
            ("id", Value::str(self.id.as_str())),
            ("kind", Value::str(self.kind.name())),
            ("mandatory", Value::Bool(self.mandatory)),
        ];
        let strings = |items: &[String]| Value::array(items.iter().map(Value::str));
        match &self.kind {
            CheckKind::Command {
                program,
                args,
                expect_exit,
                timeout_ms,
            } => {
                pairs.push(("program", Value::str(program)));
                pairs.push(("args", strings(args)));
                pairs.push(("expect_exit", Value::int(*expect_exit)));
                pairs.push(("timeout_ms", Value::int(*timeout_ms)));
            }
            CheckKind::RefcheckJson {
                program,
                args,
                timeout_ms,
            } => {
                pairs.push(("program", Value::str(program)));
                pairs.push(("args", strings(args)));
                pairs.push(("timeout_ms", Value::int(*timeout_ms)));
            }
            CheckKind::File { path, assertion } => {
                pairs.push(("path", Value::str(path)));
                match assertion {
                    FileAssertion::Exists => pairs.push(("assert", Value::str("exists"))),
                    FileAssertion::Sha256(d) => {
                        pairs.push(("assert", Value::str("sha256")));
                        pairs.push(("value", Value::str(d)));
                    }
                    FileAssertion::Contains(t) => {
                        pairs.push(("assert", Value::str("contains")));
                        pairs.push(("value", Value::str(t)));
                    }
                }
            }
            CheckKind::DiffOnly { allow } => {
                pairs.push((
                    "allow",
                    Value::array(allow.iter().map(|g| Value::str(g.as_str()))),
                ));
            }
        }
        Value::object(pairs)
    }
}

impl VerificationContract {
    /// Validates a parsed JSON document.
    pub fn from_value(v: &Value) -> Result<VerificationContract, String> {
        let map = object(v, "the contract", &["version", "checks"])?;
        match map.get("version").and_then(Value::as_u64) {
            Some(CONTRACT_VERSION) => {}
            _ => {
                return Err(format!(
                    "the contract's `version` must be {CONTRACT_VERSION}"
                ))
            }
        }
        let items = match map.get("checks") {
            Some(Value::Array(items)) => items,
            _ => return Err("the contract's `checks` must be a list".to_string()),
        };
        if items.is_empty() {
            return Err("the contract has no checks".to_string());
        }
        if items.len() > MAX_CHECKS {
            return Err(format!("the contract has more than {MAX_CHECKS} checks"));
        }
        let mut checks = Vec::with_capacity(items.len());
        let mut seen = BTreeSet::new();
        for (i, item) in items.iter().enumerate() {
            let check = Check::from_value(item, i)?;
            if !seen.insert(check.id.clone()) {
                return Err(format!(
                    "check id {} is used twice",
                    show(check.id.as_str())
                ));
            }
            checks.push(check);
        }
        if !checks.iter().any(|c| c.mandatory) {
            return Err("the contract needs at least one mandatory check".to_string());
        }
        let contract = VerificationContract { checks };
        if contract.canonical().len() > MAX_CONTRACT_BYTES {
            return Err(format!(
                "the contract is larger than {MAX_CONTRACT_BYTES} bytes"
            ));
        }
        Ok(contract)
    }

    pub fn checks(&self) -> &[Check] {
        &self.checks
    }

    pub fn check(&self, id: &CheckId) -> Option<&Check> {
        self.checks.iter().find(|c| c.id == *id)
    }

    pub fn mandatory_checks(&self) -> impl Iterator<Item = &Check> {
        self.checks.iter().filter(|c| c.mandatory)
    }

    pub fn needs_diff(&self) -> bool {
        self.checks
            .iter()
            .any(|c| matches!(c.kind, CheckKind::DiffOnly { .. }))
    }

    pub fn to_value(&self) -> Value {
        Value::object([
            ("version", Value::int(CONTRACT_VERSION)),
            (
                "checks",
                Value::array(self.checks.iter().map(Check::to_value)),
            ),
        ])
    }

    /// The deterministic text the contract is stored and identified by.
    pub fn canonical(&self) -> String {
        self.to_value().canonical()
    }

    pub fn id(&self, digest: &dyn Digest) -> ContractId {
        ContractId(digest.sha256_hex(self.canonical().as_bytes()))
    }

    /// Where the contract stands given everything recorded so far. Only evidence of this exact
    /// contract (`id`) counts, and for each mandatory check only the newest piece: a later run
    /// replaces an earlier one, in either direction.
    pub fn state(&self, id: &ContractId, evidence: &[&Evidence]) -> ContractState {
        let mut missing = false;
        for check in self.mandatory_checks() {
            let newest = evidence
                .iter()
                .filter(|e| e.contract() == id && e.check() == &check.id)
                .max_by_key(|e| e.id().seq());
            match newest {
                None => missing = true,
                Some(e) if !e.passed() => return ContractState::Failed,
                Some(_) => {}
            }
        }
        if missing {
            ContractState::Pending
        } else {
            ContractState::Passed
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use fleet_domain::ids::{EvidenceId, GoalId};
    use fleet_domain::status::Verdict;

    pub(crate) fn doc() -> Value {
        Value::object([
            ("version", Value::int(1)),
            (
                "checks",
                Value::array([
                    Value::object([
                        ("id", Value::str("tests")),
                        ("kind", Value::str("command")),
                        ("program", Value::str("/usr/bin/make")),
                        ("args", Value::array([Value::str("test")])),
                    ]),
                    Value::object([
                        ("id", Value::str("refs")),
                        ("kind", Value::str("refcheck-json")),
                        ("program", Value::str("/usr/bin/refcheck")),
                        ("mandatory", Value::Bool(false)),
                    ]),
                    Value::object([
                        ("id", Value::str("readme")),
                        ("kind", Value::str("file")),
                        ("path", Value::str("README.md")),
                        ("assert", Value::str("contains")),
                        ("value", Value::str("## Usage")),
                    ]),
                    Value::object([
                        ("id", Value::str("scope")),
                        ("kind", Value::str("diff")),
                        ("allow", Value::array([Value::str("src/**")])),
                    ]),
                ]),
            ),
        ])
    }

    fn with_check(check: Value) -> Value {
        Value::object([
            ("version", Value::int(1)),
            ("checks", Value::array([check])),
        ])
    }

    fn cmd(extra: Vec<(&str, Value)>) -> Value {
        let mut pairs = vec![
            ("id", Value::str("c")),
            ("kind", Value::str("command")),
            ("program", Value::str("/bin/true")),
        ];
        pairs.extend(extra);
        with_check(Value::object(pairs))
    }

    #[test]
    fn a_full_contract_parses_and_fills_defaults() {
        let c = VerificationContract::from_value(&doc()).unwrap();
        assert_eq!(c.checks().len(), 4);
        let tests = &c.checks()[0];
        assert!(tests.mandatory);
        assert_eq!(
            tests.kind,
            CheckKind::Command {
                program: "/usr/bin/make".into(),
                args: vec!["test".into()],
                expect_exit: 0,
                timeout_ms: DEFAULT_TIMEOUT_MS
            }
        );
        assert!(!c.checks()[1].mandatory);
        assert!(c.needs_diff());
        assert_eq!(c.mandatory_checks().count(), 3);
        assert!(c.check(&CheckId::parse("scope").unwrap()).is_some());
        assert!(c.check(&CheckId::parse("nope").unwrap()).is_none());
    }

    #[test]
    fn canonical_form_is_stable_and_round_trips() {
        let c = VerificationContract::from_value(&doc()).unwrap();
        let text = c.canonical();
        assert_eq!(text, c.canonical());
        let reparsed = VerificationContract::from_value(&c.to_value()).unwrap();
        assert_eq!(reparsed, c);
        assert_eq!(reparsed.canonical(), text);
        // spelling the defaults out changes nothing
        let explicit = cmd(vec![
            ("mandatory", Value::Bool(true)),
            ("expect_exit", Value::int(0)),
            ("timeout_ms", Value::int(DEFAULT_TIMEOUT_MS)),
            ("args", Value::array([])),
        ]);
        let implicit = cmd(vec![]);
        assert_eq!(
            VerificationContract::from_value(&explicit)
                .unwrap()
                .canonical(),
            VerificationContract::from_value(&implicit)
                .unwrap()
                .canonical()
        );
    }

    struct Toy;
    impl Digest for Toy {
        fn sha256_hex(&self, data: &[u8]) -> String {
            let sum: u64 = data.iter().map(|b| u64::from(*b)).sum();
            format!("{sum:064x}")
        }
    }

    #[test]
    fn the_id_follows_the_canonical_text() {
        let a = VerificationContract::from_value(&cmd(vec![])).unwrap();
        let b =
            VerificationContract::from_value(&cmd(vec![("expect_exit", Value::int(3))])).unwrap();
        assert_eq!(a.id(&Toy), a.id(&Toy));
        assert_ne!(a.id(&Toy), b.id(&Toy));
        assert!(ContractId::parse(a.id(&Toy).as_str()).is_ok());
    }

    #[test]
    fn strict_schema_rejects_everything_unexpected() {
        let bad: Vec<(&str, Value)> = vec![
            ("not an object", Value::str("x")),
            ("no version", Value::object([("checks", Value::array([]))])),
            (
                "wrong version",
                Value::object([("version", Value::int(2)), ("checks", Value::array([]))]),
            ),
            (
                "unknown top field",
                Value::object([
                    ("version", Value::int(1)),
                    ("checks", Value::array([])),
                    ("extra", Value::Null),
                ]),
            ),
            (
                "no checks",
                Value::object([("version", Value::int(1)), ("checks", Value::array([]))]),
            ),
            (
                "checks not a list",
                Value::object([("version", Value::int(1)), ("checks", Value::str("x"))]),
            ),
            (
                "unknown field in a check",
                cmd(vec![("shell", Value::Bool(true))]),
            ),
            (
                "relative program",
                cmd(vec![("program", Value::str("make"))]),
            ),
            ("empty program", cmd(vec![("program", Value::str(""))])),
            (
                "control char program",
                cmd(vec![("program", Value::str("/bin/\u{1b}x"))]),
            ),
            ("args not a list", cmd(vec![("args", Value::str("x"))])),
            (
                "arg not a string",
                cmd(vec![("args", Value::array([Value::int(1)]))]),
            ),
            (
                "arg with escape",
                cmd(vec![("args", Value::array([Value::str("\u{1b}[2J")]))]),
            ),
            ("exit too big", cmd(vec![("expect_exit", Value::int(256))])),
            ("exit negative", cmd(vec![("expect_exit", Value::int(-1))])),
            ("exit float", cmd(vec![("expect_exit", Value::Float(1.5))])),
            (
                "timeout too small",
                cmd(vec![("timeout_ms", Value::int(1))]),
            ),
            (
                "timeout too big",
                cmd(vec![("timeout_ms", Value::int(MAX_TIMEOUT_MS + 1))]),
            ),
            (
                "mandatory not a bool",
                cmd(vec![("mandatory", Value::int(1))]),
            ),
            (
                "only optional checks",
                cmd(vec![("mandatory", Value::Bool(false))]),
            ),
            (
                "unknown kind",
                with_check(Value::object([
                    ("id", Value::str("c")),
                    ("kind", Value::str("shell")),
                ])),
            ),
            (
                "no kind",
                with_check(Value::object([("id", Value::str("c"))])),
            ),
            (
                "bad id",
                with_check(Value::object([
                    ("id", Value::str("Bad Id")),
                    ("kind", Value::str("file")),
                    ("path", Value::str("a")),
                    ("assert", Value::str("exists")),
                ])),
            ),
        ];
        for (name, v) in bad {
            assert!(VerificationContract::from_value(&v).is_err(), "{name}");
        }
    }

    fn file(pairs: Vec<(&str, Value)>) -> Value {
        let mut all = vec![("id", Value::str("f")), ("kind", Value::str("file"))];
        all.extend(pairs);
        with_check(Value::object(all))
    }

    #[test]
    fn file_checks_are_validated() {
        let ok = |p: Vec<(&str, Value)>| VerificationContract::from_value(&file(p)).is_ok();
        assert!(ok(vec![
            ("path", Value::str("a/b.txt")),
            ("assert", Value::str("exists"))
        ]));
        assert!(ok(vec![
            ("path", Value::str("a")),
            ("assert", Value::str("sha256")),
            ("value", Value::str("ab".repeat(32))),
        ]));
        for bad in [
            vec![
                ("path", Value::str("/etc/passwd")),
                ("assert", Value::str("exists")),
            ],
            vec![
                ("path", Value::str("../x")),
                ("assert", Value::str("exists")),
            ],
            vec![
                ("path", Value::str("a//b")),
                ("assert", Value::str("exists")),
            ],
            vec![
                ("path", Value::str("a\\b")),
                ("assert", Value::str("exists")),
            ],
            vec![
                ("path", Value::str("a")),
                ("assert", Value::str("exists")),
                ("value", Value::str("x")),
            ],
            vec![
                ("path", Value::str("a")),
                ("assert", Value::str("sha256")),
                ("value", Value::str("ABC")),
            ],
            vec![("path", Value::str("a")), ("assert", Value::str("sha256"))],
            vec![
                ("path", Value::str("a")),
                ("assert", Value::str("contains")),
                ("value", Value::str("")),
            ],
            vec![
                ("path", Value::str("a")),
                ("assert", Value::str("contains")),
                ("value", Value::str("\u{0}")),
            ],
            vec![
                ("path", Value::str("a")),
                ("assert", Value::str("matches")),
                ("value", Value::str("x")),
            ],
            vec![("assert", Value::str("exists"))],
            vec![
                ("path", Value::str("p".repeat(MAX_PATH_LEN + 1))),
                ("assert", Value::str("exists")),
            ],
        ] {
            assert!(!ok(bad.clone()), "{bad:?}");
        }
    }

    #[test]
    fn diff_checks_are_validated() {
        let diff = |allow: Value| {
            with_check(Value::object([
                ("id", Value::str("d")),
                ("kind", Value::str("diff")),
                ("allow", allow),
            ]))
        };
        assert!(
            VerificationContract::from_value(&diff(Value::array([Value::str("a/**")]))).is_ok()
        );
        assert!(VerificationContract::from_value(&diff(Value::array([]))).is_err());
        assert!(
            VerificationContract::from_value(&diff(Value::array([Value::str("[a]")]))).is_err()
        );
        assert!(VerificationContract::from_value(&diff(Value::Null)).is_err());
        let many = Value::array((0..=MAX_GLOBS).map(|i| Value::str(format!("d{i}/**"))));
        assert!(VerificationContract::from_value(&diff(many)).is_err());
    }

    #[test]
    fn counts_are_bounded() {
        let checks = Value::array((0..=MAX_CHECKS).map(|i| {
            Value::object([
                ("id", Value::str(format!("c{i}"))),
                ("kind", Value::str("file")),
                ("path", Value::str("a")),
                ("assert", Value::str("exists")),
            ])
        }));
        let v = Value::object([("version", Value::int(1)), ("checks", checks)]);
        assert!(VerificationContract::from_value(&v)
            .unwrap_err()
            .contains("more than"));
        let args = Value::array((0..=MAX_ARGS).map(|_| Value::str("x")));
        assert!(VerificationContract::from_value(&cmd(vec![("args", args)])).is_err());
    }

    #[test]
    fn duplicate_ids_and_oversized_contracts_are_rejected() {
        let one = |id: &str| {
            Value::object([
                ("id", Value::str(id)),
                ("kind", Value::str("file")),
                ("path", Value::str("a")),
                ("assert", Value::str("exists")),
            ])
        };
        let dup = Value::object([
            ("version", Value::int(1)),
            ("checks", Value::array([one("x"), one("x")])),
        ]);
        assert!(VerificationContract::from_value(&dup)
            .unwrap_err()
            .contains("twice"));
        // 32 checks of 4 KiB arguments exceed the document cap
        let big = Value::array((0..MAX_CHECKS).map(|i| {
            Value::object([
                ("id", Value::str(format!("c{i}"))),
                ("kind", Value::str("command")),
                ("program", Value::str("/bin/true")),
                (
                    "args",
                    Value::array((0..4).map(|_| Value::str("y".repeat(MAX_ARG_LEN)))),
                ),
            ])
        }));
        let v = Value::object([("version", Value::int(1)), ("checks", big)]);
        assert!(VerificationContract::from_value(&v)
            .unwrap_err()
            .contains("larger than"));
    }

    #[test]
    fn hostile_text_is_quoted_in_errors() {
        let bad = with_check(Value::object([
            ("id", Value::str("c")),
            ("kind", Value::str("\u{1b}[31mevil")),
        ]));
        let msg = VerificationContract::from_value(&bad).unwrap_err();
        assert!(!msg.contains('\u{1b}'), "{msg:?}");
        let msg = CheckId::parse("\u{1b}x").unwrap_err();
        assert!(!msg.contains('\u{1b}'));
    }

    #[test]
    fn ids_parse_strictly() {
        assert!(CheckId::parse("unit-tests_2").is_ok());
        for bad in [
            "",
            "-a",
            "_a",
            "A",
            "a b",
            "a.b",
            &"a".repeat(MAX_CHECK_ID + 1),
        ] {
            assert!(CheckId::parse(bad).is_err(), "{bad:?}");
        }
        assert!(ContractId::parse(&"0".repeat(64)).is_ok());
        assert!(ContractId::parse(&"0".repeat(63)).is_err());
        assert!(ContractId::parse(&"G".repeat(64)).is_err());
        assert_eq!(CheckId::parse("a").unwrap().to_string(), "a");
        assert_eq!(
            ContractId::parse(&"1".repeat(64)).unwrap().to_string(),
            "1".repeat(64)
        );
    }

    fn evidence(seq: u64, contract: &ContractId, check: &str, verdict: Verdict) -> Evidence {
        Evidence::from_parts(
            EvidenceId::from_seq(seq),
            GoalId::parse("g").unwrap(),
            None,
            None,
            contract.clone(),
            CheckId::parse(check).unwrap(),
            verdict,
            None,
            "s".into(),
            String::new(),
            None,
            seq,
        )
    }

    #[test]
    fn state_uses_the_newest_evidence_of_this_contract_only() {
        let c = VerificationContract::from_value(&doc()).unwrap();
        let id = c.id(&Toy);
        let other = ContractId::parse(&"9".repeat(64)).unwrap();
        assert_eq!(c.state(&id, &[]), ContractState::Pending);
        let all_pass: Vec<Evidence> = ["tests", "readme", "scope"]
            .iter()
            .enumerate()
            .map(|(i, name)| evidence(i as u64 + 1, &id, name, Verdict::Passed))
            .collect();
        let refs: Vec<&Evidence> = all_pass.iter().collect();
        assert_eq!(c.state(&id, &refs), ContractState::Passed);
        // evidence of another contract is ignored
        assert_eq!(c.state(&other, &refs), ContractState::Pending);
        // an optional check never matters
        let mut with_optional = all_pass.clone();
        with_optional.push(evidence(10, &id, "refs", Verdict::Failed));
        let refs: Vec<&Evidence> = with_optional.iter().collect();
        assert_eq!(c.state(&id, &refs), ContractState::Passed);
        // a newer failure replaces an older pass, a newer pass replaces an older failure
        let mut regress = all_pass.clone();
        regress.push(evidence(11, &id, "tests", Verdict::Failed));
        let refs: Vec<&Evidence> = regress.iter().collect();
        assert_eq!(c.state(&id, &refs), ContractState::Failed);
        regress.push(evidence(12, &id, "tests", Verdict::Passed));
        let refs: Vec<&Evidence> = regress.iter().collect();
        assert_eq!(c.state(&id, &refs), ContractState::Passed);
        // an error is not a pass
        let mut errored = all_pass.clone();
        errored.push(evidence(13, &id, "scope", Verdict::Error));
        let refs: Vec<&Evidence> = errored.iter().collect();
        assert_eq!(c.state(&id, &refs), ContractState::Failed);
        // a missing mandatory check keeps it pending
        let partial: Vec<&Evidence> = all_pass.iter().take(2).collect();
        assert_eq!(c.state(&id, &partial), ContractState::Pending);
    }
}
