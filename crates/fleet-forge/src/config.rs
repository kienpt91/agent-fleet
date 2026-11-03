//! The `[github]` section of `fleet.toml`, as validated data. Reading the file is an adapter's
//! job; what a valid configuration is lives here. The integration is off unless `enabled = true`.

use crate::rate::RateConfig;
use crate::repo::RepoRef;
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::{show, validate_ref_name};

/// How `git push` proves who the operator is. fleet never holds a credential itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushCredentials {
    /// No credential: works for remotes that need none (and for local repositories).
    None,
    /// `gh`'s own git credential helper (`gh auth git-credential`), run by git as a child.
    Gh,
    /// A credential helper program named by absolute path (for example the one that talks to the
    /// system keychain). Only that one helper is configured for the push; the operator's own git
    /// configuration is never read.
    Helper(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeConfig {
    pub enabled: bool,
    /// API host. `github.com`, or a GitHub Enterprise Server host.
    pub host: String,
    /// The repository; `None` means it is read from the configured remote of a run's repository.
    pub repo: Option<RepoRef>,
    /// The git remote branches are pushed to.
    pub remote: String,
    /// Hosts a remote may name (a remote on another host is refused).
    pub allowed_hosts: Vec<String>,
    /// The base branch of a new pull request when the operator gives none.
    pub base: String,
    pub rate: RateConfig,
    /// The `gh` program, as an absolute path; otherwise it is searched for once at start-up.
    pub gh: Option<String>,
    /// Extra environment variables copied from the daemon's environment into `gh` (by name).
    pub env_pass: Vec<String>,
    pub push_credentials: PushCredentials,
    /// The URL branches are pushed to, instead of the URL of the remote.
    pub push_url: Option<String>,
    /// Accept a remote that is a path on this machine (a mirror, or a test repository).
    pub allow_local_remote: bool,
    /// How long one `gh` call may take.
    pub timeout_ms: u64,
    /// How long one `git push` may take.
    pub push_timeout_ms: u64,
}

pub const DEFAULT_HOST: &str = "github.com";
pub const MAX_ALLOWED_HOSTS: usize = 8;
pub const MAX_ENV_PASS: usize = 8;

/// Words that mark a variable as carrying a secret; such a variable is never passed to `gh`.
const SECRET_PARTS: [&str; 8] = [
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "KEY",
    "CREDENTIAL",
    "AUTH",
    "COOKIE",
];
/// Prefixes of variables that steer `gh`, `git`, ssh or fleet itself.
const FORBIDDEN_PREFIXES: [&str; 5] = ["GH_", "GITHUB_", "GIT_", "FLEET_", "SSH_"];

/// Variables scrubbed from the environment of every `gh` and `git` child, whatever the
/// configuration says.
pub const SCRUBBED: [&str; 6] = [
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GH_HOST",
    "GH_REPO",
];

fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('-')
}

/// A program path that can be placed in a `!` credential helper line without quoting.
pub fn plain_absolute_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() <= 300
        && !path.contains("..")
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+'))
}

impl ForgeConfig {
    /// The configuration when there is no `[github]` section.
    pub fn disabled() -> ForgeConfig {
        ForgeConfig {
            enabled: false,
            host: DEFAULT_HOST.to_string(),
            repo: None,
            remote: "origin".to_string(),
            allowed_hosts: vec![DEFAULT_HOST.to_string()],
            base: "main".to_string(),
            rate: RateConfig::default(),
            gh: None,
            env_pass: Vec::new(),
            push_credentials: PushCredentials::None,
            push_url: None,
            allow_local_remote: false,
            timeout_ms: 30_000,
            push_timeout_ms: 120_000,
        }
    }

    pub fn validate(&self) -> Result<()> {
        let invalid = |m: String| Err(FleetError::invalid(format!("[github] {m}")));
        if self.allowed_hosts.is_empty() || self.allowed_hosts.len() > MAX_ALLOWED_HOSTS {
            return invalid(format!("allowed_hosts needs 1-{MAX_ALLOWED_HOSTS} hosts"));
        }
        for h in &self.allowed_hosts {
            // a host name is validated the way a repository's is
            RepoRef::new(h, "o", "r").map_err(|e| {
                FleetError::invalid(format!("[github] allowed_hosts: {}", e.message))
            })?;
        }
        if !self.allowed_hosts.contains(&self.host) {
            return invalid(format!("host {} is not in allowed_hosts", show(&self.host)));
        }
        RepoRef::new(&self.host, "o", "r")
            .map_err(|e| FleetError::invalid(format!("[github] host: {}", e.message)))?;
        if let Some(r) = &self.repo {
            if r.host() != self.host {
                return invalid("repo names another host than `host`".to_string());
            }
        }
        if !name_ok(&self.remote) {
            return invalid(format!(
                "remote {} is not a remote name",
                show(&self.remote)
            ));
        }
        validate_ref_name(&self.base)?;
        if self.rate.budget_per_hour == 0 || self.rate.budget_per_hour > 5000 {
            return invalid("rate_budget_per_hour must be between 1 and 5000".to_string());
        }
        if self.rate.reserve > 1000 {
            return invalid("rate_reserve must be at most 1000".to_string());
        }
        if let Some(gh) = &self.gh {
            if !gh.starts_with('/') || gh.contains('\0') || gh.len() > 1024 {
                return invalid("gh must be an absolute path".to_string());
            }
        }
        if self.env_pass.len() > MAX_ENV_PASS {
            return invalid(format!("env_pass lists more than {MAX_ENV_PASS} names"));
        }
        for n in &self.env_pass {
            let upper = n.to_ascii_uppercase();
            if !fleet_domain::env::is_valid_name(n)
                || FORBIDDEN_PREFIXES.iter().any(|p| upper.starts_with(p))
                || SECRET_PARTS.iter().any(|p| upper.contains(p))
                || n == "PATH"
                || n == "HOME"
            {
                return invalid(format!(
                    "env_pass: {} cannot be passed to gh (secrets, GH_*, GITHUB_*, GIT_*, SSH_* and FLEET_* never are)",
                    show(n)
                ));
            }
        }
        if let PushCredentials::Helper(path) = &self.push_credentials {
            if !plain_absolute_path(path) {
                return invalid("push_credential_helper must be an absolute path of letters, digits and / . _ - +".to_string());
            }
        }
        if let Some(url) = &self.push_url {
            crate::repo::parse_remote(url)?;
        }
        for (what, ms) in [
            ("timeout_secs", self.timeout_ms),
            ("push_timeout_secs", self.push_timeout_ms),
        ] {
            if !(1_000..=600_000).contains(&ms) {
                return invalid(format!("{what} must be between 1 and 600"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_valid_and_off() {
        let c = ForgeConfig::disabled();
        assert!(!c.enabled);
        assert!(c.validate().is_ok());
        assert_eq!(c.push_credentials, PushCredentials::None);
        assert!(!c.allow_local_remote);
    }

    fn bad(edit: impl FnOnce(&mut ForgeConfig)) {
        let mut c = ForgeConfig::disabled();
        c.enabled = true;
        edit(&mut c);
        assert!(c.validate().is_err(), "{c:?}");
    }

    #[test]
    fn every_invalid_setting_is_refused() {
        bad(|c| c.allowed_hosts = vec![]);
        bad(|c| c.allowed_hosts = vec!["not a host".into()]);
        bad(|c| c.allowed_hosts = (0..9).map(|i| format!("h{i}.example")).collect());
        bad(|c| c.host = "other.example".into());
        bad(|c| c.repo = Some(RepoRef::new("other.example", "o", "r").unwrap()));
        bad(|c| c.remote = "-upload-pack".into());
        bad(|c| c.remote = "a b".into());
        bad(|c| c.base = "-x".into());
        bad(|c| c.base = "a..b".into());
        bad(|c| c.rate.budget_per_hour = 0);
        bad(|c| c.rate.budget_per_hour = 9999);
        bad(|c| c.gh = Some("gh".into()));
        bad(|c| c.gh = Some("./gh".into()));
        bad(|c| c.env_pass = (0..9).map(|i| format!("V{i}")).collect());
        for name in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "MY_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "SSH_AUTH_SOCK",
            "GIT_DIR",
            "FLEET_SOCKET",
            "PATH",
            "HOME",
            "bad name",
            "gh_host",
        ] {
            bad(|c| c.env_pass = vec![name.into()]);
        }
        bad(|c| c.push_credentials = PushCredentials::Helper("helper".into()));
        bad(|c| c.push_credentials = PushCredentials::Helper("/a/b c".into()));
        bad(|c| c.push_credentials = PushCredentials::Helper("/a/$(x)".into()));
        bad(|c| c.push_credentials = PushCredentials::Helper("/a/../b".into()));
        bad(|c| c.push_url = Some("https://user:pw@github.com/o/r".into()));
        bad(|c| c.timeout_ms = 0);
        bad(|c| c.push_timeout_ms = 3_600_000);
    }

    #[test]
    fn sensible_settings_are_accepted() {
        let mut c = ForgeConfig::disabled();
        c.enabled = true;
        c.host = "ghe.example.org".into();
        c.allowed_hosts = vec!["github.com".into(), "ghe.example.org".into()];
        c.repo = Some(RepoRef::new("ghe.example.org", "o", "r").unwrap());
        c.env_pass = vec!["DBUS_SESSION_BUS_ADDRESS".into(), "XDG_RUNTIME_DIR".into()];
        c.push_credentials = PushCredentials::Helper("/usr/local/bin/git-credential-helper".into());
        c.push_url = Some("https://ghe.example.org/o/r.git".into());
        assert!(c.validate().is_ok());
        assert!(plain_absolute_path("/opt/homebrew/bin/gh"));
        assert!(!plain_absolute_path("/a b"));
    }
}
