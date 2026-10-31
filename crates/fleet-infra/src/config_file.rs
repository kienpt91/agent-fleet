//! Reading `fleet.toml` and plan files.
//!
//! Both are untrusted input in the sense that a typo must never turn into a surprising
//! behaviour: unknown keys are errors, sizes are bounded before parsing, and every problem is
//! reported as a `FleetError` that names the offending key (and, for TOML syntax errors, the
//! line) without echoing file content verbatim.

use crate::fsx::{read_regular, ReadError};
use crate::json;
use fleet_app::config::{AppConfig, GateDef, WorkerDef};
use fleet_app::ports::Identity;
use fleet_domain::env::EnvPolicy;
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::ids::{show, WorkerName};
use fleet_domain::limits::{
    DEFAULT_RING_BYTES, MAX_CONFIG_FILE, MAX_PLAN_FILE, MAX_REDACT_RULES, MAX_RING_BYTES,
    MAX_TOTAL_TASKS,
};
use fleet_domain::quality::GateFormat;
use fleet_domain::redact::{Charset, Redactor, Rule};
use fleet_domain::sanitize::{head_str, sanitize_text};
use fleet_domain::value::Value;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Everything `fleet.toml` configures.
#[derive(Clone, Debug)]
pub struct LoadedConfig {
    pub app: AppConfig,
    pub redactor: Redactor,
    /// Bytes of recent output kept in memory per session.
    pub ring_bytes: usize,
}

const MAX_TOML_MESSAGE: usize = 512;
const MAX_AUTHOR_FIELD: usize = 200;
/// Plans are shallow documents; anything deeper is not a plan.
const MAX_PLAN_DEPTH: usize = 16;

// ---- schema -----------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct FileConfig {
    daemon: DaemonSection,
    env: EnvSection,
    redact: RedactSection,
    workers: BTreeMap<String, WorkerSection>,
    gates: Vec<GateSection>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct DaemonSection {
    max_sessions: Option<u32>,
    default_worker: Option<String>,
    max_retries: Option<u32>,
    kill_grace_secs: Option<u64>,
    approval_ttl_secs: Option<u64>,
    sensitive: Option<Vec<String>>,
    external_timeout_secs: Option<u64>,
    question_grace_secs: Option<u64>,
    author_name: Option<String>,
    author_email: Option<String>,
    ring_bytes: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct EnvSection {
    allow: Option<Vec<String>>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct RedactSection {
    literals: Vec<String>,
    env_values: Option<bool>,
    rules: Vec<RuleSection>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleSection {
    name: String,
    prefix: String,
    charset: String,
    min_len: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerSection {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env_pass: Vec<String>,
    #[serde(default)]
    env_set: BTreeMap<String, String>,
    timeout_secs: Option<u64>,
    idle_timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GateSection {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env_pass: Vec<String>,
    #[serde(default)]
    env_set: BTreeMap<String, String>,
    format: Option<String>,
    timeout_secs: Option<u64>,
}

// ---- helpers ------------------------------------------------------------------------------------

fn invalid(message: impl Into<String>) -> FleetError {
    FleetError::invalid(message)
}

fn secs_to_ms(what: &str, secs: u64) -> Result<u64> {
    secs.checked_mul(1000)
        .ok_or_else(|| invalid(format!("{what} is too large")))
}

fn shown_path(path: &Path) -> String {
    sanitize_text(path.display().to_string().as_bytes())
}

fn read_error(what: &str, path: &Path, e: &ReadError) -> FleetError {
    let code = match e {
        ReadError::Missing => ErrorCode::NotFound,
        ReadError::TooLarge { .. } => ErrorCode::Limit,
        _ => ErrorCode::Invalid,
    };
    FleetError::new(code, format!("{what} {} {e}", shown_path(path)))
}

/// A TOML error as one safe line: the parser's message (never the document) and the line.
fn toml_error(e: &toml::de::Error, text: &str) -> FleetError {
    let message = sanitize_text(head_str(e.message(), MAX_TOML_MESSAGE).as_bytes());
    let line = e
        .span()
        .and_then(|span| text.get(..span.start))
        .map(|before| before.matches('\n').count().saturating_add(1));
    match line {
        Some(line) => invalid(format!("{message} (line {line})")),
        None => invalid(message),
    }
}

fn check_identity_text(what: &str, value: &str) -> Result<()> {
    if value.trim().is_empty()
        || value.len() > MAX_AUTHOR_FIELD
        || value
            .chars()
            .any(|c| c.is_control() || c == '<' || c == '>')
    {
        return Err(invalid(format!(
            "{what} must be 1-{MAX_AUTHOR_FIELD} characters without control characters or angle brackets"
        )));
    }
    Ok(())
}

fn env_pairs(set: BTreeMap<String, String>) -> Vec<(String, String)> {
    set.into_iter().collect()
}

// ---- fleet.toml ---------------------------------------------------------------------------------

/// Reads and validates `fleet.toml`.
pub fn load_config(path: &Path, parent_env: Vec<(String, String)>) -> Result<LoadedConfig> {
    let bytes = read_regular(path, MAX_CONFIG_FILE)
        .map_err(|e| read_error("the configuration file", path, &e))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| invalid(format!("{} is not valid UTF-8", shown_path(path))))?;
    parse_config(text, parent_env).map_err(|mut e| {
        e.message = format!("{}: {}", shown_path(path), e.message);
        e
    })
}

/// The configuration used when no file exists: no workers, default limits, builtin redaction.
pub fn default_config(parent_env: Vec<(String, String)>) -> LoadedConfig {
    let mut redactor = Redactor::builtin();
    redactor.add_env_values(parent_env.iter().cloned());
    LoadedConfig {
        app: AppConfig::new(parent_env),
        redactor,
        ring_bytes: DEFAULT_RING_BYTES,
    }
}

fn parse_config(text: &str, parent_env: Vec<(String, String)>) -> Result<LoadedConfig> {
    let file: FileConfig = toml::from_str(text).map_err(|e| toml_error(&e, text))?;
    build_config(file, parent_env)
}

fn build_config(file: FileConfig, parent_env: Vec<(String, String)>) -> Result<LoadedConfig> {
    let mut app = AppConfig::new(parent_env.clone());

    let d = file.daemon;
    if let Some(n) = d.max_sessions {
        app.max_sessions = usize::try_from(n).map_err(|_| invalid("max_sessions is too large"))?;
    }
    if let Some(name) = &d.default_worker {
        app.default_worker = Some(
            WorkerName::parse(name)
                .map_err(|e| invalid(format!("default_worker: {}", e.message)))?,
        );
    }
    if let Some(n) = d.max_retries {
        app.max_retries = n;
    }
    if let Some(s) = d.kill_grace_secs {
        app.kill_grace_ms = secs_to_ms("kill_grace_secs", s)?;
    }
    if let Some(s) = d.approval_ttl_secs {
        app.approval_ttl_ms = secs_to_ms("approval_ttl_secs", s)?;
    }
    if let Some(s) = d.external_timeout_secs {
        app.external_timeout_ms = secs_to_ms("external_timeout_secs", s)?;
    }
    if let Some(s) = d.question_grace_secs {
        app.question_grace_ms = secs_to_ms("question_grace_secs", s)?;
    }
    if let Some(sensitive) = &d.sensitive {
        for item in sensitive {
            if item != "merge" && item != "external-command" {
                return Err(invalid(format!(
                    "sensitive: {} is not a known action (use \"merge\" or \"external-command\")",
                    show(item)
                )));
            }
        }
        app.merge_needs_approval = sensitive.iter().any(|s| s == "merge");
        app.external_commands = sensitive.iter().any(|s| s == "external-command");
    }
    let name = d.author_name.unwrap_or_else(|| app.author.name.clone());
    let email = d.author_email.unwrap_or_else(|| app.author.email.clone());
    check_identity_text("author_name", &name)?;
    check_identity_text("author_email", &email)?;
    app.author = Identity { name, email };

    let ring_bytes = match d.ring_bytes {
        None => DEFAULT_RING_BYTES,
        Some(n) => {
            let n = usize::try_from(n).unwrap_or(usize::MAX);
            if n == 0 || n > MAX_RING_BYTES {
                return Err(FleetError::limit(format!(
                    "ring_bytes must be between 1 and {MAX_RING_BYTES}"
                )));
            }
            n
        }
    };

    if let Some(allow) = file.env.allow {
        app.env = EnvPolicy::new(allow);
    }

    for (key, w) in file.workers {
        let name = WorkerName::parse(&key)
            .map_err(|e| invalid(format!("worker {}: {}", show(&key), e.message)))?;
        let timeout_ms = w
            .timeout_secs
            .map(|s| secs_to_ms("timeout_secs", s))
            .transpose()?;
        let idle_timeout_ms = w
            .idle_timeout_secs
            .map(|s| secs_to_ms("idle_timeout_secs", s))
            .transpose()?;
        app.workers.insert(
            name.clone(),
            WorkerDef {
                name,
                command: w.command,
                args: w.args,
                env_pass: w.env_pass,
                env_set: env_pairs(w.env_set),
                timeout_ms,
                idle_timeout_ms,
            },
        );
    }

    for g in file.gates {
        let format = match &g.format {
            None => GateFormat::Exit,
            Some(f) => GateFormat::parse(f).ok_or_else(|| {
                invalid(format!(
                    "gate {}: format {} is not known (use \"exit\" or \"refcheck-json\")",
                    show(&g.name),
                    show(f)
                ))
            })?,
        };
        let timeout_ms = secs_to_ms("timeout_secs", g.timeout_secs.unwrap_or(600))?;
        app.gates.push(GateDef {
            name: g.name,
            command: g.command,
            args: g.args,
            env_pass: g.env_pass,
            env_set: env_pairs(g.env_set),
            format,
            timeout_ms,
        });
    }

    app.validate()?;

    if file.redact.rules.len() > MAX_REDACT_RULES {
        return Err(FleetError::limit(format!(
            "more than {MAX_REDACT_RULES} redaction rules"
        )));
    }
    let rules = file
        .redact
        .rules
        .into_iter()
        .map(|r| {
            let charset = Charset::parse(&r.charset).ok_or_else(|| {
                invalid(format!(
                    "redact rule {}: charset {} is not known (use alnum, hex, base64 or token)",
                    show(&r.name),
                    show(&r.charset)
                ))
            })?;
            Ok(Rule {
                name: r.name,
                prefix: r.prefix,
                charset,
                min_len: usize::try_from(r.min_len).unwrap_or(usize::MAX),
            })
        })
        .collect::<Result<Vec<Rule>>>()?;
    let mut redactor = Redactor::builtin();
    redactor.extend(&file.redact.literals, rules)?;
    if file.redact.env_values.unwrap_or(true) {
        redactor.add_env_values(parent_env);
    }

    Ok(LoadedConfig {
        app,
        redactor,
        ring_bytes,
    })
}

// ---- plans ----------------------------------------------------------------------------------------

/// Reads a plan file (`.toml` or `.json`) and returns the arguments for `plan.load`:
/// `{"new_run": {...}?, "tasks": [...]}`. Dependencies between tasks are not checked here; the
/// domain rejects cycles and unknown ids when the plan is loaded.
pub fn load_plan(path: &Path) -> Result<Value> {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let is_toml = match extension.as_deref() {
        Some("toml") => true,
        Some("json") => false,
        _ => {
            return Err(invalid(format!(
                "{}: a plan file must end in .toml or .json",
                shown_path(path)
            )))
        }
    };
    let bytes =
        read_regular(path, MAX_PLAN_FILE).map_err(|e| read_error("the plan file", path, &e))?;
    let doc = if is_toml {
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| invalid(format!("{} is not valid UTF-8", shown_path(path))))?;
        let table: toml::Table = toml::from_str(text).map_err(|e| {
            let mut err = toml_error(&e, text);
            err.message = format!("{}: {}", shown_path(path), err.message);
            err
        })?;
        toml_to_value(&toml::Value::Table(table), 0)
    } else {
        json::parse_value(&bytes, usize::try_from(MAX_PLAN_FILE).unwrap_or(usize::MAX))
            .map_err(|e| invalid(format!("{}: {e}", shown_path(path))))
    };
    doc.and_then(|d| plan_args(&d)).map_err(|mut e| {
        if !e.message.starts_with(&shown_path(path)) {
            e.message = format!("{}: {}", shown_path(path), e.message);
        }
        e
    })
}

/// Largest accepted contract file (the contract itself is capped lower by the domain).
pub const MAX_CONTRACT_FILE_BYTES: u64 = 64 * 1024;

/// Reads a verification contract file (`.json`) and returns its document for `goal.add` and
/// `goal.attach`. Only the size, the file type and the JSON syntax are checked here; the schema
/// is validated strictly by the application layer when the goal is added.
pub fn load_contract(path: &Path) -> Result<Value> {
    let is_json = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("json"));
    if !is_json {
        return Err(invalid(format!(
            "{}: a contract file must end in .json",
            shown_path(path)
        )));
    }
    let bytes = read_regular(path, MAX_CONTRACT_FILE_BYTES)
        .map_err(|e| read_error("the contract file", path, &e))?;
    let doc = json::parse_value(
        &bytes,
        usize::try_from(MAX_CONTRACT_FILE_BYTES).unwrap_or(usize::MAX),
    )
    .map_err(|e| invalid(format!("{}: {e}", shown_path(path))))?;
    if doc.as_object().is_none() {
        return Err(invalid(format!(
            "{}: a contract must be a JSON object",
            shown_path(path)
        )));
    }
    Ok(doc)
}

fn toml_to_value(v: &toml::Value, depth: usize) -> Result<Value> {
    if depth > MAX_PLAN_DEPTH {
        return Err(invalid("the plan is nested too deeply"));
    }
    Ok(match v {
        toml::Value::String(s) => Value::Str(s.clone()),
        toml::Value::Integer(i) => Value::Int(i128::from(*i)),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Float(_) => {
            return Err(invalid("plans cannot contain floating point numbers"))
        }
        toml::Value::Datetime(_) => return Err(invalid("plans cannot contain dates or times")),
        toml::Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|i| toml_to_value(i, depth + 1))
                .collect::<Result<Vec<_>>>()?,
        ),
        toml::Value::Table(table) => {
            let mut map = BTreeMap::new();
            for (k, item) in table {
                map.insert(k.clone(), toml_to_value(item, depth + 1)?);
            }
            Value::Object(map)
        }
    })
}

fn check_keys(what: &str, obj: &BTreeMap<String, Value>, allowed: &[&str]) -> Result<()> {
    match obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(key) => Err(invalid(format!("{what}: unknown key {}", show(key)))),
        None => Ok(()),
    }
}

fn check_string(
    what: &str,
    obj: &BTreeMap<String, Value>,
    key: &str,
    required: bool,
) -> Result<()> {
    match obj.get(key) {
        Some(Value::Str(_)) => Ok(()),
        Some(_) => Err(invalid(format!("{what}: {key} must be a string"))),
        None if required => Err(invalid(format!("{what}: {key} is required"))),
        None => Ok(()),
    }
}

fn check_run(run: &Value) -> Result<()> {
    let obj = run
        .as_object()
        .ok_or_else(|| invalid("run must be a table"))?;
    check_keys(
        "run",
        obj,
        &["objective", "repo", "base", "id", "max_parallel"],
    )?;
    for key in ["objective", "repo", "base", "id"] {
        check_string("run", obj, key, false)?;
    }
    match obj.get("max_parallel") {
        None | Some(Value::Int(_)) => Ok(()),
        Some(_) => Err(invalid("run: max_parallel must be an integer")),
    }
}

fn check_task(index: usize, task: &Value) -> Result<()> {
    let what = format!("task #{}", index.saturating_add(1));
    let obj = task
        .as_object()
        .ok_or_else(|| invalid(format!("{what} must be a table")))?;
    check_keys(&what, obj, &["id", "title", "spec", "after", "worker", "host"])?;
    check_string(&what, obj, "title", true)?;
    for key in ["id", "spec", "worker", "host"] {
        check_string(&what, obj, key, false)?;
    }
    match obj.get("after") {
        None => Ok(()),
        Some(Value::Array(items)) if items.iter().all(|i| matches!(i, Value::Str(_))) => Ok(()),
        Some(_) => Err(invalid(format!("{what}: after must be a list of task ids"))),
    }
}

/// Checks the shape of a parsed plan and builds the `plan.load` arguments.
fn plan_args(doc: &Value) -> Result<Value> {
    let top = doc
        .as_object()
        .ok_or_else(|| invalid("a plan must be a table"))?;
    check_keys("plan", top, &["run", "tasks"])?;
    let tasks = match top.get("tasks") {
        Some(Value::Array(tasks)) => tasks,
        Some(_) => return Err(invalid("tasks must be a list of tables")),
        None => return Err(invalid("a plan needs a [[tasks]] list")),
    };
    if tasks.len() > MAX_TOTAL_TASKS {
        return Err(FleetError::limit(format!(
            "a plan can have at most {MAX_TOTAL_TASKS} tasks"
        )));
    }
    for (i, task) in tasks.iter().enumerate() {
        check_task(i, task)?;
    }
    let mut out = BTreeMap::new();
    if let Some(run) = top.get("run") {
        check_run(run)?;
        out.insert("new_run".to_string(), run.clone());
    }
    out.insert("tasks".to_string(), Value::Array(tasks.clone()));
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn contract_files_are_read_with_the_same_care_as_plans() {
        let dir = TempDir::new("contract-file");
        let good = dir.write("c.json", "{\"version\": 1, \"checks\": []}");
        let doc = load_contract(&good).unwrap();
        assert_eq!(doc.get("version").and_then(Value::as_u64), Some(1));
        let uppercase = dir.write("C.JSON", "{}");
        assert!(load_contract(&uppercase).is_ok());
        for (name, content) in [
            ("a.toml", "x = 1"),
            ("noext", "{}"),
            ("array.json", "[]"),
            ("broken.json", "{\"a\": "),
            ("text.json", "\"x\""),
        ] {
            let p = dir.write(name, content);
            let err = load_contract(&p).unwrap_err();
            assert_eq!(err.code, ErrorCode::Invalid, "{name}");
        }
        let big = dir.write(
            "big.json",
            &format!(
                "{{\"pad\": \"{}\"}}",
                "x".repeat(MAX_CONTRACT_FILE_BYTES as usize)
            ),
        );
        assert!(load_contract(&big).is_err());
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&good, &link).unwrap();
        assert!(load_contract(&link).is_err(), "a symlink is refused");
        let missing = load_contract(&dir.join("nope.json")).unwrap_err();
        assert!(!missing.message.contains('\u{1b}'));
    }

    fn env(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn parse(text: &str) -> Result<LoadedConfig> {
        parse_config(text, Vec::new())
    }

    const FULL: &str = r#"
[daemon]
max_sessions = 4
default_worker = "alpha"
max_retries = 3
kill_grace_secs = 7
approval_ttl_secs = 120
sensitive = ["merge"]
external_timeout_secs = 30
question_grace_secs = 1
author_name = "Fleet Bot"
author_email = "bot@example.invalid"
ring_bytes = 65536

[env]
allow = ["PATH", "HOME", "MY_AGENT_HOME"]

[redact]
literals = ["internal-hostname-01"]
env_values = false

[[redact.rules]]
name = "internal-key"
prefix = "zz_"
charset = "alnum"
min_len = 16

[workers.alpha]
command = "agent-one"
args = ["--task", "{task}"]
env_pass = ["AGENT_HOME"]
timeout_secs = 3600
idle_timeout_secs = 300

[workers.alpha.env_set]
MODE = "batch"

[workers.beta]
command = "agent-two"

[[gates]]
name = "tests"
command = "make"
args = ["test"]
env_set = { CI = "1" }
timeout_secs = 90

[[gates]]
name = "refs"
command = "refcheck"
format = "refcheck-json"
"#;

    #[test]
    fn full_example_is_read_completely() {
        let loaded = parse(FULL).unwrap();
        let app = &loaded.app;
        assert_eq!(app.max_sessions, 4);
        assert_eq!(
            app.default_worker.as_ref().map(|w| w.as_str()),
            Some("alpha")
        );
        assert_eq!(app.max_retries, 3);
        assert_eq!(app.kill_grace_ms, 7_000);
        assert_eq!(app.approval_ttl_ms, 120_000);
        assert!(app.merge_needs_approval);
        assert!(!app.external_commands);
        assert_eq!(app.external_timeout_ms, 30_000);
        assert_eq!(app.question_grace_ms, 1_000);
        assert_eq!(
            app.author,
            Identity {
                name: "Fleet Bot".into(),
                email: "bot@example.invalid".into()
            }
        );
        assert_eq!(loaded.ring_bytes, 65_536);
        assert_eq!(app.env.allowed(), ["PATH", "HOME", "MY_AGENT_HOME"]);

        assert_eq!(app.workers.len(), 2);
        let alpha = app.worker_for(None).unwrap();
        assert_eq!(alpha.command, "agent-one");
        assert_eq!(alpha.args, ["--task", "{task}"]);
        assert_eq!(alpha.env_pass, ["AGENT_HOME"]);
        assert_eq!(alpha.env_set, [("MODE".to_string(), "batch".to_string())]);
        assert_eq!(alpha.timeout_ms, Some(3_600_000));
        assert_eq!(alpha.idle_timeout_ms, Some(300_000));
        let beta = app
            .worker_for(Some(&WorkerName::parse("beta").unwrap()))
            .unwrap();
        assert_eq!(beta.timeout_ms, None);

        assert_eq!(app.gates.len(), 2);
        assert_eq!(app.gates[0].name, "tests");
        assert_eq!(app.gates[0].format, GateFormat::Exit);
        assert_eq!(app.gates[0].timeout_ms, 90_000);
        assert_eq!(app.gates[0].env_set, [("CI".to_string(), "1".to_string())]);
        assert_eq!(app.gates[1].format, GateFormat::RefcheckJson);
        assert_eq!(app.gates[1].timeout_ms, 600_000);
    }

    #[test]
    fn an_empty_file_gives_the_defaults() {
        let loaded = parse("").unwrap();
        let base = AppConfig::new(Vec::new());
        assert_eq!(loaded.app.max_sessions, base.max_sessions);
        assert_eq!(loaded.app.max_retries, 2);
        assert_eq!(loaded.app.kill_grace_ms, 5_000);
        assert_eq!(loaded.app.approval_ttl_ms, 900_000);
        assert!(loaded.app.merge_needs_approval);
        assert!(loaded.app.external_commands);
        assert_eq!(loaded.app.external_timeout_ms, 600_000);
        assert_eq!(loaded.app.question_grace_ms, 2_000);
        assert_eq!(loaded.app.author, base.author);
        assert_eq!(loaded.app.env.allowed(), base.env.allowed());
        assert!(loaded.app.workers.is_empty() && loaded.app.gates.is_empty());
        assert_eq!(loaded.ring_bytes, DEFAULT_RING_BYTES);

        let dflt = default_config(Vec::new());
        assert_eq!(dflt.ring_bytes, DEFAULT_RING_BYTES);
        assert_eq!(dflt.app.max_sessions, 8);
        assert!(dflt.app.workers.is_empty());
        let key = format!("ghp_{}", "a1B2".repeat(10));
        assert!(!dflt.redactor.redact(&format!("token {key}")).contains(&key));
    }

    #[test]
    fn sensitive_list_controls_both_switches() {
        let both = parse("[daemon]\nsensitive = [\"merge\", \"external-command\"]").unwrap();
        assert!(both.app.merge_needs_approval && both.app.external_commands);
        let only_ext = parse("[daemon]\nsensitive = [\"external-command\"]").unwrap();
        assert!(!only_ext.app.merge_needs_approval && only_ext.app.external_commands);
        let none = parse("[daemon]\nsensitive = []").unwrap();
        assert!(!none.app.merge_needs_approval && !none.app.external_commands);
    }

    #[test]
    fn unknown_keys_are_errors_everywhere() {
        for text in [
            "surprise = 1",
            "[daemon]\nmax_session = 4",
            "[env]\nallowed = [\"PATH\"]",
            "[redact]\nliteral = []",
            "[[redact.rules]]\nname = \"n\"\nprefix = \"p_\"\ncharset = \"hex\"\nmin_len = 8\nextra = 1",
            "[workers.a]\ncommand = \"x\"\ncomand = \"y\"",
            "[[gates]]\nname = \"g\"\ncommand = \"x\"\ntimeout = 5",
            "[unknown_section]\nx = 1",
        ] {
            let err = parse(text).err().unwrap_or_else(|| panic!("accepted: {text}"));
            assert_eq!(err.code, ErrorCode::Invalid, "{text}");
        }
        let err = parse("[daemon]\nmax_session = 4").err().unwrap();
        assert!(err.message.contains("max_session"), "{}", err.message);
        assert!(err.message.contains("line 2"), "{}", err.message);
    }

    #[test]
    fn wrong_types_and_syntax_errors_are_invalid_with_a_line() {
        for text in [
            "[daemon]\nmax_sessions = \"four\"",
            "[daemon]\nmax_sessions = -1",
            "[daemon]\nring_bytes = 1.5",
            "[daemon\nmax_sessions = 4",
            "[daemon]\nmax_sessions = 4\nmax_sessions = 5",
            "[[gates]]\nname = \"g\"",
        ] {
            let err = parse(text)
                .err()
                .unwrap_or_else(|| panic!("accepted: {text}"));
            assert_eq!(err.code, ErrorCode::Invalid, "{text}");
        }
        let err = parse("# comment\n\n[daemon\nx = 1").err().unwrap();
        assert!(err.message.contains("line"), "{}", err.message);
    }

    #[test]
    fn bad_charset_is_rejected() {
        let text =
            "[[redact.rules]]\nname = \"n\"\nprefix = \"zz_\"\ncharset = \"emoji\"\nmin_len = 8";
        let err = parse(text).err().unwrap();
        assert_eq!(err.code, ErrorCode::Invalid);
        assert!(err.message.contains("charset"), "{}", err.message);
        // an unusable rule is rejected by the redactor, not silently dropped
        let text = "[[redact.rules]]\nname = \"n\"\nprefix = \"\"\ncharset = \"hex\"\nmin_len = 8";
        assert_eq!(parse(text).err().unwrap().code, ErrorCode::Invalid);
        let text =
            "[[redact.rules]]\nname = \"n\"\nprefix = \"zz_\"\ncharset = \"hex\"\nmin_len = 0";
        assert_eq!(parse(text).err().unwrap().code, ErrorCode::Invalid);
    }

    #[test]
    fn forbidden_sensitive_values_are_rejected() {
        for text in [
            "[daemon]\nsensitive = [\"merge\", \"delete-everything\"]",
            "[daemon]\nsensitive = [\"Merge\"]",
            "[daemon]\nsensitive = [\"\"]",
        ] {
            let err = parse(text).err().unwrap();
            assert_eq!(err.code, ErrorCode::Invalid, "{text}");
        }
    }

    #[test]
    fn domain_validation_errors_surface() {
        for text in [
            "[daemon]\nmax_sessions = 0",
            "[daemon]\nmax_sessions = 1000",
            "[daemon]\nmax_retries = 11",
            "[daemon]\napproval_ttl_secs = 0",
            "[daemon]\ndefault_worker = \"ghost\"",
            "[env]\nallow = [\"BAD-NAME\"]",
            "[workers.a]\ncommand = \"\"",
            "[workers.a]\ncommand = \"x\"\nargs = [\"{no_such_placeholder}\"]",
            "[workers.a]\ncommand = \"x\"\ntimeout_secs = 0",
            "[workers.\"bad name\"]\ncommand = \"x\"",
            "[[gates]]\nname = \"g\"\ncommand = \"x\"\nformat = \"yaml\"",
            "[[gates]]\nname = \"g\"\ncommand = \"x\"\n[[gates]]\nname = \"g\"\ncommand = \"y\"",
            "[[gates]]\nname = \"has space\"\ncommand = \"x\"",
            "[[gates]]\nname = \"g\"\ncommand = \"x\"\ntimeout_secs = 0",
            "[daemon]\nauthor_name = \"a<b\"",
            "[daemon]\nauthor_email = \"\"",
            "[daemon]\nauthor_name = \"line\\nbreak\"",
        ] {
            let err = parse(text)
                .err()
                .unwrap_or_else(|| panic!("accepted: {text}"));
            assert_eq!(err.code, ErrorCode::Invalid, "{text}: {}", err.message);
        }
    }

    #[test]
    fn ring_bytes_and_overflowing_seconds_are_bounded() {
        let err = parse(&format!("[daemon]\nring_bytes = {}", MAX_RING_BYTES + 1))
            .err()
            .unwrap();
        assert_eq!(err.code, ErrorCode::Limit);
        assert_eq!(
            parse("[daemon]\nring_bytes = 0").err().unwrap().code,
            ErrorCode::Limit
        );
        assert_eq!(
            parse(&format!("[daemon]\nring_bytes = {MAX_RING_BYTES}"))
                .unwrap()
                .ring_bytes,
            MAX_RING_BYTES
        );
        // i64::MAX seconds does not fit in milliseconds
        let err = parse("[daemon]\nkill_grace_secs = 9223372036854775807")
            .err()
            .unwrap();
        assert_eq!(err.code, ErrorCode::Invalid);
        assert!(err.message.contains("too large"));
        let err = parse("[workers.a]\ncommand = \"x\"\nidle_timeout_secs = 9223372036854775807")
            .err()
            .unwrap();
        assert_eq!(err.code, ErrorCode::Invalid);
    }

    #[test]
    fn too_many_workers_gates_or_rules_are_limits_or_invalid() {
        let mut text = String::new();
        for i in 0..40 {
            text.push_str(&format!("[workers.w{i}]\ncommand = \"x\"\n"));
        }
        assert!(parse(&text).is_err());
        let mut text = String::new();
        for i in 0..=MAX_REDACT_RULES {
            text.push_str(&format!(
                "[[redact.rules]]\nname = \"r{i}\"\nprefix = \"p{i}_\"\ncharset = \"hex\"\nmin_len = 8\n"
            ));
        }
        assert_eq!(parse(&text).err().unwrap().code, ErrorCode::Limit);
    }

    #[test]
    fn env_allowlist_reaches_the_child_environment_policy() {
        let loaded = parse("[env]\nallow = [\"PATH\", \"MY_VAR\", \"FLEET_SOCKET\"]").unwrap();
        let parent = env(&[
            ("PATH", "/bin"),
            ("MY_VAR", "1"),
            ("HOME", "/home/u"),
            ("FLEET_SOCKET", "/run/s.sock"),
            ("SOME_TOKEN", "should-not-pass"),
        ]);
        let child = loaded.app.env.build(&parent, &[], &[], &[]);
        assert_eq!(child, env(&[("MY_VAR", "1"), ("PATH", "/bin")]));
        // the default allowlist applies when the table is absent
        let dflt = parse("").unwrap();
        let child = dflt.app.env.build(&parent, &[], &[], &[]);
        assert!(child.iter().any(|(k, _)| k == "HOME"));
        assert!(!child
            .iter()
            .any(|(k, _)| k == "MY_VAR" || k == "SOME_TOKEN"));
    }

    #[test]
    fn configured_redaction_actually_redacts() {
        let text = r#"
[redact]
literals = ["internal-hostname-01"]
[[redact.rules]]
name = "internal-key"
prefix = "zz_"
charset = "alnum"
min_len = 16
"#;
        let secret_env = "env-secret-value-1234";
        let loaded = parse_config(
            text,
            env(&[("DEPLOY_TOKEN", secret_env), ("EDITOR", "vim-nonsecret")]),
        )
        .unwrap();
        let r = &loaded.redactor;

        let custom = format!("zz_{}", "A1b2".repeat(8));
        let line = format!("key={custom} end");
        let out = r.redact(&line);
        assert!(!out.contains(&custom), "{out}");
        assert!(
            out.contains("[REDACTED:internal-key]") && out.ends_with(" end"),
            "{out}"
        );

        let out = r.redact("connect to internal-hostname-01 now");
        assert!(!out.contains("internal-hostname-01"), "{out}");

        // builtin rules stay active next to the configured ones
        let builtin = format!("ghp_{}", "a1B2".repeat(10));
        assert!(!r.redact(&format!("x {builtin}")).contains(&builtin));

        // secret-looking variables of the daemon's environment are redacted by value
        assert!(!r.redact(&format!("echo {secret_env}")).contains(secret_env));
        // other variables are not
        assert_eq!(r.redact("run vim-nonsecret"), "run vim-nonsecret");
        // too short for the rule: left alone
        assert_eq!(r.redact("zz_short"), "zz_short");
    }

    #[test]
    fn env_value_redaction_can_be_switched_off() {
        let loaded = parse_config(
            "[redact]\nenv_values = false",
            env(&[("DEPLOY_TOKEN", "env-secret-value-1234")]),
        )
        .unwrap();
        assert_eq!(loaded.redactor.literal_count(), 0);
        let on = parse_config("", env(&[("DEPLOY_TOKEN", "env-secret-value-1234")])).unwrap();
        assert_eq!(on.redactor.literal_count(), 1);
    }

    #[test]
    fn bad_redaction_literals_are_rejected() {
        assert_eq!(
            parse("[redact]\nliterals = [\"abc\"]").err().unwrap().code,
            ErrorCode::Invalid
        );
        assert_eq!(
            parse("[redact]\nliterals = [\"multi\\nline-value\"]")
                .err()
                .unwrap()
                .code,
            ErrorCode::Invalid
        );
    }

    #[test]
    fn load_config_reads_files_with_bounds() {
        let dir = TempDir::new("cf");
        let path = dir.write("fleet.toml", FULL);
        let loaded = load_config(&path, Vec::new()).unwrap();
        assert_eq!(loaded.app.max_sessions, 4);

        // oversize: refused before parsing
        let big = dir.write(
            "big.toml",
            &format!("# {}\n", "x".repeat(MAX_CONFIG_FILE as usize)),
        );
        let err = load_config(&big, Vec::new()).err().unwrap();
        assert_eq!(err.code, ErrorCode::Limit);

        // missing file
        let err = load_config(&dir.join("nope.toml"), Vec::new())
            .err()
            .unwrap();
        assert_eq!(err.code, ErrorCode::NotFound);

        // a symlink is not followed
        let link = dir.join("link.toml");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert_eq!(
            load_config(&link, Vec::new()).err().unwrap().code,
            ErrorCode::Invalid
        );

        // a directory is not a config file
        assert_eq!(
            load_config(dir.path(), Vec::new()).err().unwrap().code,
            ErrorCode::Invalid
        );

        // not UTF-8
        let bin = dir.join("bin.toml");
        std::fs::write(&bin, [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(
            load_config(&bin, Vec::new()).err().unwrap().code,
            ErrorCode::Invalid
        );

        // errors name the file
        let bad = dir.write("bad.toml", "[daemon]\nnope = 1\n");
        let err = load_config(&bad, Vec::new()).err().unwrap();
        assert!(err.message.contains("bad.toml"), "{}", err.message);
    }

    #[test]
    fn error_text_cannot_carry_terminal_escapes() {
        // The key name is attacker-chosen text; the message must not pass an escape through.
        let err = parse("[daemon]\n\"bad\\u001b[31mkey\" = 1").err().unwrap();
        assert!(!err.message.contains('\u{1b}'), "{:?}", err.message);
    }

    // ---- plans ----------------------------------------------------------------------------

    const PLAN_TOML: &str = r#"
[run]
objective = "Ship the feature"
repo = "/work/repo"
base = "main"
max_parallel = 2

[[tasks]]
id = "a"
title = "First"
spec = "Do the first thing."
worker = "alpha"

[[tasks]]
id = "b"
title = "Second"
after = ["a"]
"#;

    const PLAN_JSON: &str = r#"{
  "run": {"objective": "Ship the feature", "repo": "/work/repo", "base": "main", "max_parallel": 2},
  "tasks": [
    {"id": "a", "title": "First", "spec": "Do the first thing.", "worker": "alpha"},
    {"id": "b", "title": "Second", "after": ["a"]}
  ]
}"#;

    fn plan(dir: &TempDir, name: &str, text: &str) -> Result<Value> {
        let path = dir.write(name, text);
        load_plan(&path)
    }

    #[test]
    fn plan_toml_and_json_are_equivalent() {
        let dir = TempDir::new("cf");
        let from_toml = plan(&dir, "p.toml", PLAN_TOML).unwrap();
        let from_json = plan(&dir, "p.json", PLAN_JSON).unwrap();
        assert_eq!(from_toml, from_json);
        let run = from_toml.get("new_run").unwrap();
        assert_eq!(
            run.get("objective").and_then(Value::as_str),
            Some("Ship the feature")
        );
        assert_eq!(run.get("max_parallel").and_then(Value::as_u64), Some(2));
        let tasks = from_toml.get("tasks").and_then(Value::as_array).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(
            tasks[1]
                .get("after")
                .and_then(Value::as_array)
                .map(<[Value]>::len),
            Some(1)
        );
        assert!(
            from_toml.get("run").is_none(),
            "the run table is renamed to new_run"
        );
    }

    #[test]
    fn a_task_host_is_carried_through_the_plan() {
        let dir = TempDir::new("cf");
        let v = plan(
            &dir,
            "h.toml",
            "[[tasks]]\ntitle = \"A\"\nhost = \"build1\"\n",
        )
        .unwrap();
        let tasks = v.get("tasks").and_then(Value::as_array).unwrap();
        assert_eq!(tasks[0].get("host").and_then(Value::as_str), Some("build1"));
        assert!(plan(&dir, "h2.toml", "[[tasks]]\ntitle = \"A\"\nhost = 3\n").is_err());
    }

    #[test]
    fn plan_without_a_run_table_has_no_new_run() {
        let dir = TempDir::new("cf");
        let v = plan(&dir, "p.toml", "[[tasks]]\ntitle = \"Only\"\n").unwrap();
        assert!(v.get("new_run").is_none());
        assert_eq!(
            v.get("tasks").and_then(Value::as_array).map(<[Value]>::len),
            Some(1)
        );
    }

    #[test]
    fn a_dependency_cycle_is_not_this_modules_business() {
        let dir = TempDir::new("cf");
        let text = "[[tasks]]\nid = \"a\"\ntitle = \"A\"\nafter = [\"b\"]\n[[tasks]]\nid = \"b\"\ntitle = \"B\"\nafter = [\"a\"]\n";
        let v = plan(&dir, "cycle.toml", text).unwrap();
        assert_eq!(
            v.get("tasks").and_then(Value::as_array).map(<[Value]>::len),
            Some(2)
        );
        // unknown dependency ids pass through too: the domain checks them against the plan
        let v = plan(
            &dir,
            "dangling.toml",
            "[[tasks]]\ntitle = \"A\"\nafter = [\"zzz\"]\n",
        );
        assert!(v.is_ok());
    }

    #[test]
    fn bad_plan_shapes_are_invalid() {
        let dir = TempDir::new("cf");
        let cases = [
            ("a.toml", "title = \"x\""),                                  // no tasks
            ("b.toml", "tasks = []\nextra = 1"),                          // unknown top-level key
            ("c.toml", "tasks = \"x\""),                                  // tasks not a list
            ("d.toml", "tasks = [1]"),                                    // task not a table
            ("e.toml", "[[tasks]]\nid = \"a\""),                          // no title
            ("f.toml", "[[tasks]]\ntitle = 5"),                           // title not a string
            ("g.toml", "[[tasks]]\ntitle = \"a\"\nafter = \"b\""),        // after not a list
            ("h.toml", "[[tasks]]\ntitle = \"a\"\nafter = [1]"),          // after item not a string
            ("i.toml", "[[tasks]]\ntitle = \"a\"\nprio = 1"),             // unknown task key
            ("j.toml", "run = 5\n[[tasks]]\ntitle = \"a\""),              // run not a table
            ("k.toml", "[run]\nowner = \"x\"\n[[tasks]]\ntitle = \"a\""), // unknown run key
            (
                "l.toml",
                "[run]\nmax_parallel = \"2\"\n[[tasks]]\ntitle = \"a\"",
            ),
            ("m.toml", "[[tasks]]\ntitle = \"a\"\nspec = 1.5"), // float
            ("n.toml", "[[tasks]]\ntitle = \"a\"\nspec = 1979-05-27"), // date
            ("o.toml", "[[tasks]\ntitle = \"a\""),              // syntax
            ("p.json", "[]"),                                   // not an object
            ("q.json", "{\"tasks\": [{\"title\": null}]}"),
            (
                "r.json",
                "{\"tasks\": [{\"title\": \"a\"}], \"run\": {\"max_parallel\": 1.5}}",
            ),
            ("s.json", "{\"tasks\": [{\"title\": \"a\"}]} trailing"),
            ("t.json", "not json"),
        ];
        for (name, text) in cases {
            let err = plan(&dir, name, text)
                .err()
                .unwrap_or_else(|| panic!("accepted {name}"));
            assert_eq!(err.code, ErrorCode::Invalid, "{name}: {}", err.message);
            assert!(err.message.contains(name), "{name}: {}", err.message);
        }
    }

    #[test]
    fn plan_file_rules() {
        let dir = TempDir::new("cf");
        // unknown extensions are refused before the file is read
        let path = dir.write("plan.yaml", "tasks: []");
        let err = load_plan(&path).err().unwrap();
        assert_eq!(err.code, ErrorCode::Invalid);
        assert!(err.message.contains(".toml or .json"));
        let no_ext = dir.write("plan", "{}");
        assert_eq!(load_plan(&no_ext).err().unwrap().code, ErrorCode::Invalid);

        // missing
        assert_eq!(
            load_plan(&dir.join("none.toml")).err().unwrap().code,
            ErrorCode::NotFound
        );

        // oversize
        let big = dir.write("big.json", &" ".repeat(MAX_PLAN_FILE as usize + 1));
        assert_eq!(load_plan(&big).err().unwrap().code, ErrorCode::Limit);

        // symlinks are not followed
        let real = dir.write("real.toml", PLAN_TOML);
        let link = dir.join("link.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(load_plan(&link).err().unwrap().code, ErrorCode::Invalid);

        // too many tasks
        let mut text = String::new();
        for _ in 0..=MAX_TOTAL_TASKS {
            text.push_str("[[tasks]]\ntitle = \"t\"\n");
        }
        assert_eq!(
            plan(&dir, "many.toml", &text).err().unwrap().code,
            ErrorCode::Limit
        );

        // deeply nested values are refused instead of recursing without bound
        let nested = format!(
            "tasks = []\n[run]\nx = {}1{}",
            "[".repeat(40),
            "]".repeat(40)
        );
        assert!(plan(&dir, "deep.toml", &nested).is_err());
    }
}
