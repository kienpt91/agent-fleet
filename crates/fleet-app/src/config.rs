//! Settings of the orchestrator, as plain validated data. Parsing a file into this structure is
//! an adapter concern; the rules about what a valid configuration is live here.

use crate::hosts::{validate_hosts, HostDef};
use crate::ports::Identity;
use fleet_domain::brief::validate_template;
use fleet_domain::env::{is_valid_name, EnvPolicy};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::{show, HostName, WorkerName};
use fleet_domain::limits::{
    MAX_ENV_ALLOW, MAX_GATES_CONFIGURED, MAX_RETRIES_CEILING, MAX_SESSIONS_HARD,
    MAX_WORKERS_CONFIGURED,
};
use fleet_domain::quality::GateFormat;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerDef {
    pub name: WorkerName,
    pub command: String,
    /// Templates; see `fleet_domain::brief::PLACEHOLDERS`.
    pub args: Vec<String>,
    /// Extra variables passed through from the daemon's environment by name.
    pub env_pass: Vec<String>,
    /// Constant variables.
    pub env_set: Vec<(String, String)>,
    pub timeout_ms: Option<u64>,
    pub idle_timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateDef {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env_pass: Vec<String>,
    pub env_set: Vec<(String, String)>,
    pub format: GateFormat,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug)]
pub struct AppConfig {
    /// Concurrent pty sessions across all runs.
    pub max_sessions: usize,
    pub default_worker: Option<WorkerName>,
    pub max_retries: u32,
    pub kill_grace_ms: u64,
    pub approval_ttl_ms: u64,
    /// `merge` is a sensitive action: merging needs an approval.
    pub merge_needs_approval: bool,
    /// `external-command` is a sensitive action: the daemon runs approved worker requests.
    pub external_commands: bool,
    pub external_timeout_ms: u64,
    pub author: Identity,
    pub env: EnvPolicy,
    /// A snapshot of the daemon's environment; children only get the allowlisted part.
    pub parent_env: Vec<(String, String)>,
    pub workers: BTreeMap<WorkerName, WorkerDef>,
    pub gates: Vec<GateDef>,
    /// Remote hosts a task may run on (`[hosts.<name>]`); none unless the operator writes them.
    pub hosts: BTreeMap<HostName, HostDef>,
    /// Silence after which a question file makes the supervisor stop the worker.
    pub question_grace_ms: u64,
}

impl AppConfig {
    pub fn new(parent_env: Vec<(String, String)>) -> AppConfig {
        AppConfig {
            max_sessions: 8,
            default_worker: None,
            max_retries: 2,
            kill_grace_ms: 5000,
            approval_ttl_ms: 15 * 60 * 1000,
            merge_needs_approval: true,
            external_commands: true,
            external_timeout_ms: 10 * 60 * 1000,
            author: Identity {
                name: "fleet".to_string(),
                email: "fleet@localhost".to_string(),
            },
            env: EnvPolicy::default_policy(),
            parent_env,
            workers: BTreeMap::new(),
            gates: Vec::new(),
            hosts: BTreeMap::new(),
            question_grace_ms: 2000,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.max_sessions == 0 || self.max_sessions > MAX_SESSIONS_HARD {
            return Err(FleetError::invalid(format!(
                "max_sessions must be between 1 and {MAX_SESSIONS_HARD}"
            )));
        }
        if self.max_retries > MAX_RETRIES_CEILING {
            return Err(FleetError::invalid(format!(
                "max_retries must be at most {MAX_RETRIES_CEILING}"
            )));
        }
        if self.approval_ttl_ms == 0 {
            return Err(FleetError::invalid("approval_ttl must be positive"));
        }
        if self.env.allowed().len() > MAX_ENV_ALLOW {
            return Err(FleetError::invalid(format!(
                "the environment allowlist has more than {MAX_ENV_ALLOW} entries"
            )));
        }
        for name in self.env.allowed() {
            if !is_valid_name(name) {
                return Err(FleetError::invalid(format!(
                    "{} is not a valid environment variable name",
                    show(name)
                )));
            }
        }
        if self.workers.len() > MAX_WORKERS_CONFIGURED {
            return Err(FleetError::invalid(format!(
                "more than {MAX_WORKERS_CONFIGURED} workers are configured"
            )));
        }
        if self.gates.len() > MAX_GATES_CONFIGURED {
            return Err(FleetError::invalid(format!(
                "more than {MAX_GATES_CONFIGURED} quality gates are configured"
            )));
        }
        if let Some(d) = &self.default_worker {
            if !self.workers.contains_key(d) {
                return Err(FleetError::invalid(format!(
                    "default_worker {d} is not defined"
                )));
            }
        }
        for (name, w) in &self.workers {
            if *name != w.name {
                return Err(FleetError::invalid("worker table key and name differ"));
            }
            check_command(
                &format!("worker {name}"),
                &w.command,
                &w.args,
                &w.env_pass,
                &w.env_set,
            )?;
            if w.timeout_ms == Some(0) || w.idle_timeout_ms == Some(0) {
                return Err(FleetError::invalid(format!(
                    "worker {name}: timeouts must be positive"
                )));
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for g in &self.gates {
            if g.name.is_empty()
                || g.name.len() > 40
                || g.name.chars().any(|c| c.is_control() || c.is_whitespace())
            {
                return Err(FleetError::invalid(format!(
                    "gate name {} must be 1-40 characters without spaces",
                    show(&g.name)
                )));
            }
            if !seen.insert(&g.name) {
                return Err(FleetError::invalid(format!(
                    "gate {} is defined twice",
                    g.name
                )));
            }
            check_command(
                &format!("gate {}", g.name),
                &g.command,
                &g.args,
                &g.env_pass,
                &g.env_set,
            )?;
            if g.timeout_ms == 0 {
                return Err(FleetError::invalid(format!(
                    "gate {}: timeout must be positive",
                    g.name
                )));
            }
        }
        validate_hosts(&self.hosts)?;
        Ok(())
    }

    /// The worker to use for a task that does not name one.
    pub fn worker_for(&self, requested: Option<&WorkerName>) -> Result<&WorkerDef> {
        let name = requested.or(self.default_worker.as_ref()).ok_or_else(|| {
            FleetError::invalid("no worker was named and no default_worker is configured")
        })?;
        self.workers.get(name).ok_or_else(|| {
            FleetError::invalid(format!("worker {name} is not defined in the configuration"))
        })
    }
}

fn check_command(
    what: &str,
    command: &str,
    args: &[String],
    env_pass: &[String],
    env_set: &[(String, String)],
) -> Result<()> {
    if command.trim().is_empty() || command.contains('\0') {
        return Err(FleetError::invalid(format!("{what}: command is empty")));
    }
    for a in args {
        validate_template(a).map_err(|e| FleetError::invalid(format!("{what}: {}", e.message)))?;
    }
    for n in env_pass.iter().chain(env_set.iter().map(|(k, _)| k)) {
        if !is_valid_name(n) {
            return Err(FleetError::invalid(format!(
                "{what}: {} is not a valid environment variable name",
                show(n)
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(name: &str) -> WorkerDef {
        WorkerDef {
            name: WorkerName::parse(name).unwrap(),
            command: "agent".into(),
            args: vec!["--task".into(), "{task}".into()],
            env_pass: vec![],
            env_set: vec![],
            timeout_ms: Some(1000),
            idle_timeout_ms: None,
        }
    }

    fn gate(name: &str) -> GateDef {
        GateDef {
            name: name.into(),
            command: "true".into(),
            args: vec![],
            env_pass: vec![],
            env_set: vec![],
            format: GateFormat::Exit,
            timeout_ms: 1000,
        }
    }

    fn with_worker() -> AppConfig {
        let mut c = AppConfig::new(vec![]);
        let w = worker("agent");
        c.default_worker = Some(w.name.clone());
        c.workers.insert(w.name.clone(), w);
        c
    }

    #[test]
    fn defaults_validate() {
        assert!(AppConfig::new(vec![]).validate().is_ok());
        assert!(with_worker().validate().is_ok());
    }

    #[test]
    fn limits_are_enforced() {
        let mut c = AppConfig::new(vec![]);
        c.max_sessions = 0;
        assert!(c.validate().is_err());
        c.max_sessions = MAX_SESSIONS_HARD + 1;
        assert!(c.validate().is_err());
        let mut c = AppConfig::new(vec![]);
        c.max_retries = MAX_RETRIES_CEILING + 1;
        assert!(c.validate().is_err());
        let mut c = AppConfig::new(vec![]);
        c.approval_ttl_ms = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn default_worker_must_exist() {
        let mut c = AppConfig::new(vec![]);
        c.default_worker = Some(WorkerName::parse("ghost").unwrap());
        assert!(c.validate().is_err());
    }

    #[test]
    fn worker_rules() {
        let mut c = with_worker();
        c.workers.values_mut().for_each(|w| w.command = " ".into());
        assert!(c.validate().is_err());
        let mut c = with_worker();
        c.workers
            .values_mut()
            .for_each(|w| w.args = vec!["{nope}".into()]);
        assert!(c.validate().is_err());
        let mut c = with_worker();
        c.workers
            .values_mut()
            .for_each(|w| w.env_pass = vec!["BAD NAME".into()]);
        assert!(c.validate().is_err());
        let mut c = with_worker();
        c.workers.values_mut().for_each(|w| w.timeout_ms = Some(0));
        assert!(c.validate().is_err());
    }

    #[test]
    fn gate_rules() {
        let mut c = AppConfig::new(vec![]);
        c.gates = vec![gate("tests"), gate("tests")];
        assert!(c.validate().is_err());
        c.gates = vec![gate("two words")];
        assert!(c.validate().is_err());
        c.gates = vec![gate("")];
        assert!(c.validate().is_err());
        let mut g = gate("x");
        g.timeout_ms = 0;
        c.gates = vec![g];
        assert!(c.validate().is_err());
        c.gates = vec![gate("lint"), gate("tests")];
        assert!(c.validate().is_ok());
        c.gates = (0..=MAX_GATES_CONFIGURED)
            .map(|i| gate(&format!("g{i}")))
            .collect();
        assert!(c.validate().is_err());
    }

    #[test]
    fn worker_lookup() {
        let c = with_worker();
        assert_eq!(c.worker_for(None).unwrap().command, "agent");
        let other = WorkerName::parse("other").unwrap();
        assert!(c.worker_for(Some(&other)).is_err());
        assert!(AppConfig::new(vec![]).worker_for(None).is_err());
    }
}
