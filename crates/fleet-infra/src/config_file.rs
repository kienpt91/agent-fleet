//! Reading `fleet.toml` and plan files.
//!
//! Both are untrusted input in the sense that a typo must never turn into a surprising
//! behaviour: unknown keys are errors, sizes are bounded before parsing, and every problem is
//! reported as a `FleetError` that names the offending key (and, for TOML syntax errors, the
//! line) without echoing file content verbatim.

use crate::fsx::{read_regular, ReadError};
use crate::json;
use fleet_app::config::{AppConfig, ContextConfig, GateDef, WorkerDef};
use fleet_app::ports::Identity;
use fleet_domain::env::EnvPolicy;
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::ids::{show, WorkerName};
use fleet_domain::limits::{
    DEFAULT_RING_BYTES, MAX_CONFIG_FILE, MAX_PLAN_FILE, MAX_REDACT_RULES, MAX_RING_BYTES, MAX_SPEC,
    MAX_TOTAL_TASKS,
};
use fleet_domain::quality::GateFormat;
use fleet_domain::redact::{Charset, Redactor, Rule};
use fleet_domain::repo::view::ImageLimits;
use fleet_domain::sanitize::{head_str, sanitize_text};
use fleet_domain::value::Value;
use fleet_forge::config::{ForgeConfig, PushCredentials};
use fleet_forge::repo::RepoRef;
use neo_context::estimate::Profile;
use neo_context::input::GlossaryEntry;
use neo_context::layer::UnverifiedPolicy;
use neo_context::limits::{DEFAULT_CHARS_PER_TOKEN, DEFAULT_MAX_TOKENS, DEFAULT_SAFETY_PERCENT};
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
    /// `[context]`, read by hand so that every error names the key.
    context: toml::Table,
    web: WebSection,
    gates: Vec<GateSection>,
    github: GithubSection,
    compare: CompareSection,
}

/// `[web]`. The values are read as TOML values so that every error names the key.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct WebSection {
    image_max_side: Option<toml::Value>,
    image_max_megapixels: Option<toml::Value>,
}

/// An integer in `1..=max`, or an error naming `web.<key>`.
fn web_limit(key: &str, v: &Option<toml::Value>, max: u64) -> Result<Option<u64>> {
    let Some(v) = v else { return Ok(None) };
    let n = v
        .as_integer()
        .ok_or_else(|| invalid(format!("web.{key} must be an integer")))?;
    match u64::try_from(n) {
        Ok(n) if (1..=max).contains(&n) => Ok(Some(n)),
        _ => Err(invalid(format!("web.{key} must be between 1 and {max}"))),
    }
}

/// `[compare]`: how `fleet compare` orders fan-out members unless asked otherwise.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct CompareSection {
    rank: Option<Vec<String>>,
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
    profile: Option<String>,
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

/// The forge integration. Off unless `enabled = true`; no token appears here, ever: signing in is
/// `gh`'s own business.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct GithubSection {
    enabled: Option<bool>,
    host: Option<String>,
    repo: Option<String>,
    remote: Option<String>,
    allowed_hosts: Option<Vec<String>>,
    base: Option<String>,
    rate_budget_per_hour: Option<u32>,
    rate_reserve: Option<u32>,
    rate_backoff_secs: Option<u64>,
    rate_backoff_max_secs: Option<u64>,
    gh: Option<String>,
    env_pass: Option<Vec<String>>,
    push_credentials: Option<String>,
    push_credential_helper: Option<String>,
    push_url: Option<String>,
    allow_local_remote: Option<bool>,
    timeout_secs: Option<u64>,
    push_timeout_secs: Option<u64>,
}

fn apply_github(forge: &mut ForgeConfig, g: GithubSection) -> Result<()> {
    if let Some(v) = g.enabled {
        forge.enabled = v;
    }
    if let Some(v) = g.host {
        forge.host = v.to_ascii_lowercase();
        if g.allowed_hosts.is_none() {
            forge.allowed_hosts = vec![forge.host.clone()];
        }
    }
    if let Some(v) = g.allowed_hosts {
        forge.allowed_hosts = v.iter().map(|h| h.to_ascii_lowercase()).collect();
    }
    if let Some(slug) = g.repo {
        forge.repo = Some(
            RepoRef::from_slug(&forge.host, &slug)
                .map_err(|e| invalid(format!("[github] repo: {}", e.message)))?,
        );
    }
    if let Some(v) = g.remote {
        forge.remote = v;
    }
    if let Some(v) = g.base {
        forge.base = v;
    }
    if let Some(v) = g.rate_budget_per_hour {
        forge.rate.budget_per_hour = v;
    }
    if let Some(v) = g.rate_reserve {
        forge.rate.reserve = v;
    }
    if let Some(s) = g.rate_backoff_secs {
        forge.rate.base_backoff_ms = secs_to_ms("rate_backoff_secs", s)?;
    }
    if let Some(s) = g.rate_backoff_max_secs {
        forge.rate.max_backoff_ms = secs_to_ms("rate_backoff_max_secs", s)?;
    }
    if forge.rate.base_backoff_ms == 0 || forge.rate.max_backoff_ms < forge.rate.base_backoff_ms {
        return Err(invalid(
            "[github] rate_backoff_secs must be positive and at most rate_backoff_max_secs",
        ));
    }
    forge.gh = g.gh;
    if let Some(v) = g.env_pass {
        forge.env_pass = v;
    }
    forge.push_credentials = match (g.push_credentials.as_deref(), g.push_credential_helper) {
        (None | Some("none"), None) => PushCredentials::None,
        (Some("gh"), None) => PushCredentials::Gh,
        (Some("helper"), Some(path)) => PushCredentials::Helper(path),
        (Some("helper"), None) => {
            return Err(invalid(
                "[github] push_credentials = \"helper\" needs push_credential_helper (an absolute path)",
            ))
        }
        (_, Some(_)) => {
            return Err(invalid(
                "[github] push_credential_helper is used only with push_credentials = \"helper\"",
            ))
        }
        (Some(other), None) => {
            return Err(invalid(format!(
                "[github] push_credentials {} is not known (use \"none\", \"gh\" or \"helper\")",
                show(other)
            )))
        }
    };
    forge.push_url = g.push_url;
    if let Some(v) = g.allow_local_remote {
        forge.allow_local_remote = v;
    }
    if let Some(s) = g.timeout_secs {
        forge.timeout_ms = secs_to_ms("timeout_secs", s)?;
    }
    if let Some(s) = g.push_timeout_secs {
        forge.push_timeout_ms = secs_to_ms("push_timeout_secs", s)?;
    }
    Ok(())
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

    if let Some(names) = &file.compare.rank {
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        app.compare_rank = fleet_fanout::rank::RankRule::parse(&names)
            .map_err(|e| invalid(format!("compare.rank: {}", e.message)))?;
    }

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
                profile: w.profile,
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

    app.context = build_context(file.context)?;

    let side = web_limit(
        "image_max_side",
        &file.web.image_max_side,
        u64::from(ImageLimits::HARD_MAX_SIDE),
    )?;
    let megapixels = web_limit(
        "image_max_megapixels",
        &file.web.image_max_megapixels,
        ImageLimits::HARD_MAX_PIXELS / 1_000_000,
    )?;
    app.images = ImageLimits::new(
        side.map_or(ImageLimits::DEFAULT_MAX_SIDE, |n| n as u32),
        megapixels.map_or(ImageLimits::DEFAULT_MAX_PIXELS, |n| n * 1_000_000),
    );

    apply_github(&mut app.forge, file.github)?;
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

// ---- context ---------------------------------------------------------------------------------------

const CONTEXT_KEYS: [&str; 6] = [
    "profile",
    "default_profile",
    "unverified",
    "knowledge",
    "constraints",
    "glossary",
];
const PROFILE_KEYS: [&str; 3] = ["max_tokens", "chars_per_token", "safety_percent"];

fn context_string(key: &str, v: &toml::Value) -> Result<String> {
    v.as_str()
        .map(str::to_string)
        .ok_or_else(|| invalid(format!("context.{key} must be a string")))
}

fn profile_number(name: &str, sub: &toml::Table, key: &str, default: u32) -> Result<u32> {
    match sub.get(key) {
        None => Ok(default),
        Some(v) => v
            .as_integer()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| {
                invalid(format!(
                    "context.profile.{}.{key} must be a non-negative integer",
                    show(name)
                ))
            }),
    }
}

/// Reads `[context]`. `profile` is either the name of the default profile (a string) or the table
/// of profiles `[context.profile.<name>]`; TOML has one key `profile`, so a file that defines
/// profiles names its default with `default_profile`.
fn build_context(table: toml::Table) -> Result<ContextConfig> {
    let mut cfg = ContextConfig::default();
    if let Some(key) = table.keys().find(|k| !CONTEXT_KEYS.contains(&k.as_str())) {
        return Err(invalid(format!("context: unknown key {}", show(key))));
    }

    let mut default_name: Option<String> = None;
    match table.get("profile") {
        None => {}
        Some(toml::Value::String(name)) => default_name = Some(name.clone()),
        Some(toml::Value::Table(profiles)) => {
            for (name, v) in profiles {
                let sub = v.as_table().ok_or_else(|| {
                    invalid(format!("context.profile.{} must be a table", show(name)))
                })?;
                if let Some(key) = sub.keys().find(|k| !PROFILE_KEYS.contains(&k.as_str())) {
                    return Err(invalid(format!(
                        "context.profile.{}: unknown key {}",
                        show(name),
                        show(key)
                    )));
                }
                let profile = Profile::new(
                    name,
                    profile_number(name, sub, "max_tokens", DEFAULT_MAX_TOKENS)?,
                    profile_number(name, sub, "chars_per_token", DEFAULT_CHARS_PER_TOKEN)?,
                    profile_number(name, sub, "safety_percent", DEFAULT_SAFETY_PERCENT)?,
                )
                .map_err(|e| invalid(format!("context.profile.{}: {e}", show(name))))?;
                cfg.profiles.insert(name.clone(), profile);
            }
        }
        Some(_) => {
            return Err(invalid(
                "context.profile must be a profile name or a table of profiles",
            ))
        }
    }
    if let Some(v) = table.get("default_profile") {
        if default_name.is_some() {
            return Err(invalid(
                "context.default_profile and a context.profile name are both set",
            ));
        }
        default_name = Some(context_string("default_profile", v)?);
    }
    if let Some(name) = default_name {
        cfg.default_profile = name;
    }

    if let Some(v) = table.get("unverified") {
        let text = context_string("unverified", v)?;
        cfg.policy = UnverifiedPolicy::parse(&text).ok_or_else(|| {
            invalid(format!(
                "context.unverified: {} is not known (use \"label\" or \"exclude\")",
                show(&text)
            ))
        })?;
    }
    if let Some(v) = table.get("knowledge") {
        cfg.knowledge = Some(context_string("knowledge", v)?);
    }
    if let Some(v) = table.get("constraints") {
        let items = v
            .as_array()
            .ok_or_else(|| invalid("context.constraints must be a list of strings"))?;
        for item in items {
            cfg.constraints.push(
                item.as_str()
                    .ok_or_else(|| invalid("context.constraints must be a list of strings"))?
                    .to_string(),
            );
        }
    }
    if let Some(v) = table.get("glossary") {
        let entries = v
            .as_table()
            .ok_or_else(|| invalid("context.glossary must be a table of term = \"meaning\""))?;
        for (term, meaning) in entries {
            let meaning = meaning.as_str().ok_or_else(|| {
                invalid(format!("context.glossary.{} must be a string", show(term)))
            })?;
            cfg.glossary.push(GlossaryEntry {
                term: term.clone(),
                meaning: meaning.to_string(),
            });
        }
    }
    Ok(cfg)
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

/// Reads the prompt of a fan-out from a file (`--spec-file`): a regular file of at most
/// [`MAX_SPEC`] bytes, UTF-8 text. Links and special files are refused before a byte is read.
pub fn load_spec(path: &Path) -> Result<String> {
    let bytes =
        read_regular(path, MAX_SPEC as u64).map_err(|e| read_error("the spec file", path, &e))?;
    String::from_utf8(bytes)
        .map_err(|_| invalid(format!("{}: the spec is not valid UTF-8", shown_path(path))))
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
        &["objective", "repo", "base", "id", "max_parallel", "context"],
    )?;
    for key in ["objective", "repo", "base", "id"] {
        check_string("run", obj, key, false)?;
    }
    check_string("run", obj, "context", false)?;
    if let Some(Value::Str(mode)) = obj.get("context") {
        if fleet_domain::status::ContextMode::parse(mode).is_none() {
            return Err(invalid("run: context must be \"off\" or \"compiled\""));
        }
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
    check_keys(&what, obj, &["id", "title", "spec", "after", "worker"])?;
    check_string(&what, obj, "title", true)?;
    for key in ["id", "spec", "worker"] {
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

    #[test]
    fn spec_files_are_read_as_bounded_regular_utf8_text() {
        let dir = TempDir::new("spec-file");
        let good = dir.write(
            "prompt.md",
            "Make the parser reject trailing commas.\n\nKeep the error text.\n",
        );
        assert_eq!(
            load_spec(&good).unwrap(),
            "Make the parser reject trailing commas.\n\nKeep the error text.\n"
        );
        let big = dir.write("big.md", &"x".repeat(MAX_SPEC + 1));
        assert_eq!(load_spec(&big).unwrap_err().code, ErrorCode::Limit);
        let binary = dir.join("binary.md");
        std::fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(load_spec(&binary).unwrap_err().code, ErrorCode::Invalid);
        let link = dir.join("link.md");
        std::os::unix::fs::symlink(&good, &link).unwrap();
        assert!(load_spec(&link).is_err(), "a symlink is refused");
        assert!(load_spec(dir.path()).is_err(), "a directory is refused");
        let missing = load_spec(&dir.join("nope\u{1b}.md")).unwrap_err();
        assert_eq!(missing.code, ErrorCode::NotFound);
        assert!(!missing.message.contains('\u{1b}'));
    }

    #[test]
    fn the_ranking_criteria_are_configurable_and_validated_strictly() {
        let default = parse("").unwrap().app.compare_rank;
        assert_eq!(default, fleet_fanout::rank::RankRule::default());
        let set = parse("[compare]\nrank = [\"diff\", \"verified\"]")
            .unwrap()
            .app
            .compare_rank;
        assert_eq!(set.formula(), "sort by (diff, verified), then by task id a-z; advisory only, nothing is merged or picked from it");
        for bad in [
            "[compare]\nrank = []",
            "[compare]\nrank = [\"luck\"]",
            "[compare]\nrank = [\"diff\", \"diff\"]",
            "[compare]\nrank = \"diff\"",
            "[compare]\nrank = [1]",
            "[compare]\nweights = [1]",
            "[compare]\nrank = [\"diff\"]\nauto_pick = true",
        ] {
            let err = parse(bad)
                .err()
                .unwrap_or_else(|| panic!("accepted: {bad}"));
            assert_eq!(err.code, ErrorCode::Invalid, "{bad}");
        }
        let err = parse("[compare]\nrank = [\"luck\"]").err().unwrap();
        assert!(err.message.contains("compare.rank"), "{}", err.message);
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
    fn web_image_limits_are_optional_and_bounded_by_the_hard_caps() {
        let dflt = parse("").unwrap().app.images;
        assert_eq!((dflt.max_side(), dflt.max_pixels()), (8192, 32_000_000));
        let l = parse("[web]\nimage_max_side = 4096\nimage_max_megapixels = 16")
            .unwrap()
            .app
            .images;
        assert_eq!((l.max_side(), l.max_pixels()), (4096, 16_000_000));
        let top = parse("[web]\nimage_max_side = 16384\nimage_max_megapixels = 64")
            .unwrap()
            .app
            .images;
        assert_eq!((top.max_side(), top.max_pixels()), (16_384, 64_000_000));
        // one key alone leaves the other at its default
        let one = parse("[web]\nimage_max_megapixels = 1").unwrap().app.images;
        assert_eq!((one.max_side(), one.max_pixels()), (8192, 1_000_000));

        for (text, key) in [
            ("[web]\nimage_max_side = 0", "image_max_side"),
            ("[web]\nimage_max_side = 16385", "image_max_side"),
            ("[web]\nimage_max_side = -1", "image_max_side"),
            ("[web]\nimage_max_side = 4096.5", "image_max_side"),
            ("[web]\nimage_max_side = \"big\"", "image_max_side"),
            ("[web]\nimage_max_megapixels = 0", "image_max_megapixels"),
            ("[web]\nimage_max_megapixels = 65", "image_max_megapixels"),
            ("[web]\nimage_max_megapixels = 1.5", "image_max_megapixels"),
            ("[web]\nimage_max_megapixels = true", "image_max_megapixels"),
        ] {
            let err = parse(text).err().unwrap_or_else(|| panic!("{text}"));
            assert_eq!(err.code, ErrorCode::Invalid, "{text}");
            assert!(err.message.contains(key), "{text}: {}", err.message);
        }
        assert!(parse("[web]\nimage_max_pixels = 4").is_err());
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

    #[test]
    fn no_context_table_gives_the_defaults() {
        let loaded = parse("").unwrap();
        assert_eq!(loaded.app.context, ContextConfig::default());
        assert_eq!(loaded.app.context.default_profile, "default");
        assert_eq!(loaded.app.context.policy, UnverifiedPolicy::Label);
    }

    #[test]
    fn the_context_table_is_read_completely() {
        let text = r#"
[context]
default_profile = "small"
unverified = "exclude"
knowledge = "/etc/fleet/knowledge.toml"
constraints = ["no network", "keep it small"]

[context.glossary]
goal = "something to verify"
task = "one unit of work"

[context.profile.small]
max_tokens = 8192

[context.profile.large]
max_tokens = 100000
chars_per_token = 3
safety_percent = 150

[workers.a]
command = "x"
profile = "large"
"#;
        let c = parse(text).unwrap().app;
        assert_eq!(c.context.default_profile, "small");
        assert_eq!(c.context.policy, UnverifiedPolicy::Exclude);
        assert_eq!(
            c.context.knowledge.as_deref(),
            Some("/etc/fleet/knowledge.toml")
        );
        assert_eq!(c.context.constraints, vec!["no network", "keep it small"]);
        assert_eq!(c.context.glossary.len(), 2);
        assert!(c.context.glossary.iter().any(|g| g.term == "goal"));
        assert_eq!(c.context.profiles["small"].max_tokens(), 8192);
        assert_eq!(c.context.profiles["large"].max_tokens(), 100_000);
        assert_eq!(
            c.context.profiles["large"].estimator().safety_percent(),
            150
        );
        assert!(
            c.context.profiles.contains_key("default"),
            "the built-in one stays"
        );
        let worker = c.workers.values().next().unwrap();
        assert_eq!(worker.profile.as_deref(), Some("large"));
    }

    #[test]
    fn a_default_profile_table_replaces_the_built_in_one() {
        let c = parse("[context.profile.default]\nmax_tokens = 16000\n")
            .unwrap()
            .app;
        assert_eq!(c.context.profiles["default"].max_tokens(), 16_000);
        assert_eq!(c.context.profiles.len(), 1);
    }

    #[test]
    fn the_default_profile_can_be_named_by_a_string() {
        let c = parse("[context]\nprofile = \"default\"\n").unwrap().app;
        assert_eq!(c.context.default_profile, "default");
        let err = parse("[context]\nprofile = \"nope\"\n").err().unwrap();
        assert!(err.message.contains("nope"), "{}", err.message);
    }

    #[test]
    fn bad_context_settings_are_errors_that_name_the_key() {
        let cases = [
            ("[context]\nsurprise = 1", "surprise"),
            ("[context]\nunverified = \"maybe\"", "unverified"),
            ("[context]\nunverified = 3", "context.unverified"),
            ("[context]\nknowledge = 3", "context.knowledge"),
            ("[context]\nknowledge = \"relative.toml\"", "absolute"),
            ("[context]\nconstraints = \"x\"", "context.constraints"),
            ("[context]\nconstraints = [1]", "context.constraints"),
            ("[context]\nglossary = [1]", "context.glossary"),
            ("[context.glossary]\nterm = 1", "term"),
            ("[context]\nprofile = 5", "context.profile"),
            (
                "[context]\nprofile = \"a\"\ndefault_profile = \"b\"",
                "both set",
            ),
            ("[context.profile.x]\nmax_tokens = \"big\"", "max_tokens"),
            ("[context.profile.x]\nmax_tokens = -1", "max_tokens"),
            ("[context.profile.x]\nmax_tokens = 10", "max_tokens"),
            (
                "[context.profile.x]\nchars_per_token = 0",
                "chars_per_token",
            ),
            ("[context.profile.x]\nsafety_percent = 10", "safety_percent"),
            ("[context.profile.x]\nmax_token = 9000", "max_token"),
            (
                "[context.profile.\"Bad Name\"]\nmax_tokens = 9000",
                "profile",
            ),
            ("[context.profile]\nx = 3", "context.profile"),
            ("[context]\ndefault_profile = \"missing\"", "missing"),
            (
                "[workers.a]\ncommand = \"x\"\nprofile = \"missing\"",
                "missing",
            ),
            ("[workers.a]\ncommand = \"x\"\nprofile = 3", "string"),
        ];
        for (text, needle) in cases {
            let err = parse(text)
                .err()
                .unwrap_or_else(|| panic!("accepted: {text}"));
            assert_eq!(err.code, ErrorCode::Invalid, "{text}");
            assert!(err.message.contains(needle), "{text}: {}", err.message);
        }
    }

    #[test]
    fn the_github_section_is_off_by_default_and_reads_every_setting() {
        let off = parse("").unwrap();
        assert!(!off.app.forge.enabled);
        assert_eq!(off.app.forge, fleet_forge::config::ForgeConfig::disabled());
        let loaded = parse(
            r#"
[github]
enabled = true
host = "GHE.example.org"
repo = "octo/repo"
remote = "upstream"
base = "develop"
rate_budget_per_hour = 120
rate_reserve = 20
rate_backoff_secs = 10
rate_backoff_max_secs = 300
gh = "/opt/homebrew/bin/gh"
env_pass = ["DBUS_SESSION_BUS_ADDRESS"]
push_credentials = "helper"
push_credential_helper = "/usr/local/bin/git-credential-osxkeychain"
push_url = "https://ghe.example.org/octo/repo.git"
allow_local_remote = true
timeout_secs = 15
push_timeout_secs = 90
"#,
        )
        .unwrap();
        let f = &loaded.app.forge;
        assert!(f.enabled && f.allow_local_remote);
        assert_eq!(f.host, "ghe.example.org");
        assert_eq!(f.allowed_hosts, ["ghe.example.org"]);
        assert_eq!(
            f.repo.as_ref().map(|r| r.full()),
            Some("ghe.example.org/octo/repo".to_string())
        );
        assert_eq!(
            (f.remote.as_str(), f.base.as_str()),
            ("upstream", "develop")
        );
        assert_eq!((f.rate.budget_per_hour, f.rate.reserve), (120, 20));
        assert_eq!(
            (f.rate.base_backoff_ms, f.rate.max_backoff_ms),
            (10_000, 300_000)
        );
        assert_eq!(f.gh.as_deref(), Some("/opt/homebrew/bin/gh"));
        assert_eq!(f.env_pass, ["DBUS_SESSION_BUS_ADDRESS"]);
        assert_eq!(
            f.push_credentials,
            fleet_forge::config::PushCredentials::Helper(
                "/usr/local/bin/git-credential-osxkeychain".into()
            )
        );
        assert_eq!((f.timeout_ms, f.push_timeout_ms), (15_000, 90_000));
        let gh = parse("[github]\nenabled = true\npush_credentials = \"gh\"").unwrap();
        assert_eq!(
            gh.app.forge.push_credentials,
            fleet_forge::config::PushCredentials::Gh
        );
        assert!(parse("[github]\nallowed_hosts = [\"github.com\", \"ghe.example.org\"]\nhost = \"ghe.example.org\"").is_ok());
    }

    #[test]
    fn the_github_section_refuses_what_could_carry_a_token_or_widen_the_reach() {
        for text in [
            "[github]\ntoken = \"x\"",
            "[github]\ngh_token = \"x\"",
            "[github]\nenabled = \"yes\"",
            "[github]\nrepo = \"not-a-repo\"",
            "[github]\nrepo = \"a/b/c\"",
            "[github]\nhost = \"bad host\"",
            "[github]\nhost = \"ghe.example.org\"\nallowed_hosts = [\"github.com\"]",
            "[github]\nremote = \"-evil\"",
            "[github]\nbase = \"a..b\"",
            "[github]\ngh = \"gh\"",
            "[github]\nenv_pass = [\"GH_TOKEN\"]",
            "[github]\nenv_pass = [\"MY_SECRET\"]",
            "[github]\npush_credentials = \"password\"",
            "[github]\npush_credentials = \"helper\"",
            "[github]\npush_credentials = \"gh\"\npush_credential_helper = \"/bin/x\"",
            "[github]\npush_credentials = \"helper\"\npush_credential_helper = \"relative\"",
            "[github]\npush_credentials = \"helper\"\npush_credential_helper = \"/a b/c\"",
            "[github]\npush_url = \"https://user:pw@github.com/o/r\"",
            "[github]\nrate_budget_per_hour = 0",
            "[github]\nrate_backoff_secs = 0",
            "[github]\nrate_backoff_secs = 100\nrate_backoff_max_secs = 10",
            "[github]\ntimeout_secs = 0",
            "[github]\npush_timeout_secs = 100000",
        ] {
            let err = parse(text)
                .err()
                .unwrap_or_else(|| panic!("accepted: {text}"));
            assert!(
                matches!(err.code, ErrorCode::Invalid | ErrorCode::Limit),
                "{text}: {err:?}"
            );
        }
    }
}
