//! Remote hosts as plain validated data: where a remote worktree may live and how it is reached.
//! Parsing `[hosts.<name>]` is an adapter concern; what a valid host is lives here and in
//! `fleet_domain::remote`.

use fleet_domain::env::is_valid_name;
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::{show, HostName};
use fleet_domain::limits::MAX_SESSIONS_HARD;
use fleet_domain::remote::{
    validate_address, validate_local_path, validate_remote_base, validate_remote_env,
    validate_remote_program, validate_user, RemoteLayout, MAX_REMOTE_ENV,
};
use std::collections::BTreeMap;

/// Most hosts one configuration may define.
pub const MAX_HOSTS_CONFIGURED: usize = 16;
/// Most worker commands one host may allow.
pub const MAX_ALLOWED_WORKERS: usize = 16;
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_SERVER_ALIVE_SECS: u64 = 15;
pub const DEFAULT_SERVER_ALIVE_COUNT: u32 = 3;
pub const DEFAULT_MAX_BUNDLE_BYTES: u64 = 256 * 1024 * 1024;
pub const DEFAULT_HOST_SESSIONS: usize = 2;

/// One `[hosts.<name>]` table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostDef {
    pub name: HostName,
    pub address: String,
    pub port: u16,
    pub user: String,
    /// The private key used for this host and no other: an absolute path on this machine. An
    /// agent is never consulted.
    pub identity_file: String,
    /// Absolute directory on the host below which every remote worktree lives.
    pub base_dir: String,
    /// The worker commands (`[workers.x] command`) that may run on this host, exactly as written.
    pub allowed_workers: Vec<String>,
    /// Concurrent remote sessions on this host.
    pub max_sessions: usize,
    /// The only variables a remote worker gets besides fleet's own (explicit values, never copied
    /// from the local environment).
    pub env: Vec<(String, String)>,
    /// Another configured host to reach this one through (`ssh -J`). Off unless written.
    pub proxy_jump: Option<HostName>,
    pub connect_timeout_secs: u64,
    pub server_alive_secs: u64,
    pub server_alive_count: u32,
    /// Most bytes of a git bundle in either direction.
    pub max_bundle_bytes: u64,
}

impl HostDef {
    /// A host with the defaults for everything the operator does not have to write.
    pub fn new(
        name: HostName,
        address: &str,
        user: &str,
        identity_file: &str,
        base_dir: &str,
    ) -> HostDef {
        HostDef {
            name,
            address: address.to_string(),
            port: 22,
            user: user.to_string(),
            identity_file: identity_file.to_string(),
            base_dir: base_dir.to_string(),
            allowed_workers: Vec::new(),
            max_sessions: DEFAULT_HOST_SESSIONS,
            env: Vec::new(),
            proxy_jump: None,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
            server_alive_secs: DEFAULT_SERVER_ALIVE_SECS,
            server_alive_count: DEFAULT_SERVER_ALIVE_COUNT,
            max_bundle_bytes: DEFAULT_MAX_BUNDLE_BYTES,
        }
    }

    pub fn validate(&self) -> Result<()> {
        let what = |msg: String| FleetError::invalid(format!("host {}: {msg}", self.name));
        let wrap = |e: FleetError| what(e.message);
        validate_address(&self.address).map_err(wrap)?;
        validate_user(&self.user).map_err(wrap)?;
        if self.port == 0 {
            return Err(what("the port must be between 1 and 65535".into()));
        }
        validate_local_path("identity file", &self.identity_file).map_err(wrap)?;
        validate_remote_base(&self.base_dir).map_err(wrap)?;
        if self.allowed_workers.len() > MAX_ALLOWED_WORKERS {
            return Err(what(format!(
                "more than {MAX_ALLOWED_WORKERS} allowed worker commands"
            )));
        }
        for w in &self.allowed_workers {
            validate_remote_program(w).map_err(wrap)?;
        }
        if self.max_sessions == 0 || self.max_sessions > MAX_SESSIONS_HARD {
            return Err(what(format!(
                "max_sessions must be between 1 and {MAX_SESSIONS_HARD}"
            )));
        }
        if self.env.len() > MAX_REMOTE_ENV {
            return Err(what(format!(
                "more than {MAX_REMOTE_ENV} environment variables"
            )));
        }
        for (k, v) in &self.env {
            validate_remote_env(k, v).map_err(wrap)?;
            if !is_valid_name(k) || k.starts_with("FLEET_") {
                return Err(what(format!(
                    "environment variable {} is not allowed (fleet sets FLEET_* itself)",
                    show(k)
                )));
            }
        }
        if self.proxy_jump.as_ref() == Some(&self.name) {
            return Err(what("proxy_jump names the host itself".into()));
        }
        if !(1..=120).contains(&self.connect_timeout_secs) {
            return Err(what("connect_timeout_secs must be between 1 and 120".into()));
        }
        if !(1..=300).contains(&self.server_alive_secs) {
            return Err(what("server_alive_secs must be between 1 and 300".into()));
        }
        if !(1..=20).contains(&self.server_alive_count) {
            return Err(what("server_alive_count must be between 1 and 20".into()));
        }
        if !(1024 * 1024..=2 * 1024 * 1024 * 1024).contains(&self.max_bundle_bytes) {
            return Err(what(
                "max_bundle_bytes must be between 1 MiB and 2 GiB".into(),
            ));
        }
        Ok(())
    }

    /// The remote directories of this host.
    pub fn layout(&self) -> Result<RemoteLayout> {
        RemoteLayout::new(&self.base_dir)
    }

    /// True when `command` is one of the worker commands this host allows.
    pub fn allows_worker(&self, command: &str) -> bool {
        self.allowed_workers.iter().any(|w| w == command)
    }
}

/// Checks the hosts of a configuration together: each valid, jump hosts defined, and a jump host
/// is not itself reached through another host.
pub fn validate_hosts(hosts: &BTreeMap<HostName, HostDef>) -> Result<()> {
    if hosts.len() > MAX_HOSTS_CONFIGURED {
        return Err(FleetError::invalid(format!(
            "more than {MAX_HOSTS_CONFIGURED} hosts are configured"
        )));
    }
    for (name, host) in hosts {
        if *name != host.name {
            return Err(FleetError::invalid("host table key and name differ"));
        }
        host.validate()?;
        if let Some(jump) = &host.proxy_jump {
            match hosts.get(jump) {
                None => {
                    return Err(FleetError::invalid(format!(
                        "host {name}: proxy_jump {jump} is not a configured host"
                    )))
                }
                Some(j) if j.proxy_jump.is_some() => {
                    return Err(FleetError::invalid(format!(
                        "host {name}: proxy_jump {jump} is itself reached through another host (one hop only)"
                    )))
                }
                Some(_) => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str) -> HostDef {
        let mut h = HostDef::new(
            HostName::parse(name).unwrap(),
            "build.example.org",
            "fleet",
            "/home/me/.ssh/fleet_ed25519",
            "/srv/fleet",
        );
        h.allowed_workers = vec!["agent".into()];
        h
    }

    #[test]
    fn a_complete_host_is_valid_and_knows_its_workers() {
        let h = host("build");
        h.validate().unwrap();
        assert!(h.allows_worker("agent"));
        assert!(!h.allows_worker("sh"));
        assert_eq!(h.layout().unwrap().base(), "/srv/fleet");
    }

    #[test]
    fn every_field_is_checked() {
        let cases: Vec<(&str, Box<dyn Fn(&mut HostDef)>)> = vec![
            ("address", Box::new(|h| h.address = "-oProxyCommand=x".into())),
            ("user", Box::new(|h| h.user = "a b".into())),
            ("port", Box::new(|h| h.port = 0)),
            ("identity", Box::new(|h| h.identity_file = "relative/key".into())),
            ("identity chars", Box::new(|h| h.identity_file = "/k ey".into())),
            ("base", Box::new(|h| h.base_dir = "/srv/../etc".into())),
            ("worker", Box::new(|h| h.allowed_workers = vec!["rm -rf".into()])),
            ("sessions", Box::new(|h| h.max_sessions = 0)),
            ("env name", Box::new(|h| h.env = vec![("A B".into(), "x".into())])),
            ("env fleet", Box::new(|h| h.env = vec![("FLEET_RUN".into(), "x".into())])),
            ("env nul", Box::new(|h| h.env = vec![("A".into(), "x\0".into())])),
            ("self jump", Box::new(|h| h.proxy_jump = Some(h.name.clone()))),
            ("connect", Box::new(|h| h.connect_timeout_secs = 0)),
            ("alive", Box::new(|h| h.server_alive_secs = 301)),
            ("count", Box::new(|h| h.server_alive_count = 0)),
            ("bundle", Box::new(|h| h.max_bundle_bytes = 10)),
        ];
        for (label, change) in cases {
            let mut h = host("build");
            change(&mut h);
            assert!(h.validate().is_err(), "{label}");
        }
    }

    #[test]
    fn jump_hosts_must_exist_and_be_direct() {
        let mut all = BTreeMap::new();
        let mut a = host("a");
        a.proxy_jump = Some(HostName::parse("b").unwrap());
        all.insert(a.name.clone(), a);
        assert!(validate_hosts(&all).is_err());
        let b = host("b");
        all.insert(b.name.clone(), b);
        validate_hosts(&all).unwrap();
        let mut b2 = host("b");
        b2.proxy_jump = Some(HostName::parse("c").unwrap());
        all.insert(b2.name.clone(), b2);
        all.insert(HostName::parse("c").unwrap(), host("c"));
        assert!(validate_hosts(&all).is_err());
    }

    #[test]
    fn too_many_hosts_are_refused() {
        let mut all = BTreeMap::new();
        for i in 0..=MAX_HOSTS_CONFIGURED {
            let h = host(&format!("h{i}"));
            all.insert(h.name.clone(), h);
        }
        assert!(validate_hosts(&all).is_err());
    }
}
