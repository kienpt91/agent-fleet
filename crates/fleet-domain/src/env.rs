//! The environment handed to child processes: an explicit allowlist, never inheritance.
//!
//! The daemon's own environment may hold tokens and the location of its control socket. Workers,
//! quality gates, git and external commands all start from an empty environment and receive only
//! the variables named here, plus fixed values set by configuration and by fleet itself.

use std::collections::BTreeMap;

/// Variables passed through when the configuration does not say otherwise.
pub const DEFAULT_ALLOW: [&str; 10] = [
    "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "TZ", "TMPDIR",
];

/// Names that are never copied from the daemon's environment, even when allowlisted: they could
/// reveal the control socket or redirect git.
const FORBIDDEN_PREFIXES: [&str; 2] = ["FLEET_", "GIT_"];

pub fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && name.len() <= 128
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// True when `name` may be copied from the parent environment.
pub fn may_pass(name: &str) -> bool {
    is_valid_name(name) && !FORBIDDEN_PREFIXES.iter().any(|p| name.starts_with(p))
}

#[derive(Clone, Debug)]
pub struct EnvPolicy {
    allow: Vec<String>,
}

impl EnvPolicy {
    pub fn new(allow: Vec<String>) -> Self {
        EnvPolicy { allow }
    }

    pub fn default_policy() -> Self {
        EnvPolicy::new(DEFAULT_ALLOW.iter().map(|s| s.to_string()).collect())
    }

    pub fn allowed(&self) -> &[String] {
        &self.allow
    }

    /// Builds the child environment: allowlisted variables from `parent` (plus `extra_pass`),
    /// then `set` (configuration constants), then `forced` (values fleet needs the child to see).
    /// Later sources win. The result is sorted by name.
    pub fn build(
        &self,
        parent: &[(String, String)],
        extra_pass: &[String],
        set: &[(String, String)],
        forced: &[(String, String)],
    ) -> Vec<(String, String)> {
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        for (name, value) in parent {
            let listed = self.allow.iter().chain(extra_pass).any(|a| a == name);
            if listed && may_pass(name) && !value.contains('\0') {
                out.insert(name.clone(), value.clone());
            }
        }
        for (name, value) in set.iter().chain(forced) {
            if is_valid_name(name) && !value.contains('\0') {
                out.insert(name.clone(), value.clone());
            }
        }
        out.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn only_allowlisted_variables_pass() {
        let parent = pairs(&[
            ("PATH", "/usr/bin"),
            ("SECRET_TOKEN", "hunter2"),
            ("HOME", "/home/u"),
            ("AWS_SECRET_ACCESS_KEY", "x"),
        ]);
        let env = EnvPolicy::default_policy().build(&parent, &[], &[], &[]);
        assert_eq!(env, pairs(&[("HOME", "/home/u"), ("PATH", "/usr/bin")]));
    }

    #[test]
    fn extra_pass_adds_named_variables_only() {
        let parent = pairs(&[("PATH", "/bin"), ("MY_AGENT_KEY", "k"), ("OTHER", "o")]);
        let env =
            EnvPolicy::default_policy().build(&parent, &["MY_AGENT_KEY".to_string()], &[], &[]);
        assert!(env.contains(&("MY_AGENT_KEY".to_string(), "k".to_string())));
        assert!(!env.iter().any(|(k, _)| k == "OTHER"));
    }

    #[test]
    fn fleet_and_git_variables_are_never_copied_from_the_parent() {
        let parent = pairs(&[
            ("FLEET_SOCKET", "/run/x.sock"),
            ("FLEET_STATE_DIR", "/state"),
            ("GIT_DIR", "/x"),
            ("GIT_SSH_COMMAND", "evil"),
            ("PATH", "/bin"),
        ]);
        let allow = EnvPolicy::new(
            [
                "FLEET_SOCKET",
                "FLEET_STATE_DIR",
                "GIT_DIR",
                "GIT_SSH_COMMAND",
                "PATH",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        );
        assert_eq!(
            allow.build(&parent, &[], &[], &[]),
            pairs(&[("PATH", "/bin")])
        );
    }

    #[test]
    fn set_overrides_parent_and_forced_overrides_set() {
        let parent = pairs(&[("PATH", "/bin"), ("LANG", "C")]);
        let env = EnvPolicy::default_policy().build(
            &parent,
            &[],
            &pairs(&[("LANG", "en_US.UTF-8"), ("MODE", "ci")]),
            &pairs(&[("MODE", "fleet"), ("FLEET_TASK", "t1")]),
        );
        assert_eq!(
            env,
            pairs(&[
                ("FLEET_TASK", "t1"),
                ("LANG", "en_US.UTF-8"),
                ("MODE", "fleet"),
                ("PATH", "/bin")
            ])
        );
    }

    #[test]
    fn invalid_names_and_values_are_dropped() {
        let policy = EnvPolicy::new(vec![
            "BAD NAME".into(),
            "1X".into(),
            "OK".into(),
            "NUL".into(),
        ]);
        let parent = pairs(&[
            ("BAD NAME", "v"),
            ("1X", "v"),
            ("OK", "v"),
            ("NUL", "a\0b"),
            ("A=B", "x"),
        ]);
        let env = policy.build(&parent, &[], &pairs(&[("A=B", "x"), ("C", "a\0")]), &[]);
        assert_eq!(env, pairs(&[("OK", "v")]));
    }

    #[test]
    fn name_rules() {
        assert!(is_valid_name("PATH") && is_valid_name("_x1"));
        assert!(
            !is_valid_name("")
                && !is_valid_name("1a")
                && !is_valid_name("a-b")
                && !is_valid_name("a=b")
        );
        assert!(!is_valid_name(&"A".repeat(129)));
        assert!(may_pass("HOME") && !may_pass("FLEET_X") && !may_pass("GIT_X"));
    }

    #[test]
    fn default_allowlist_has_no_secrets() {
        for name in DEFAULT_ALLOW {
            let lower = name.to_ascii_lowercase();
            assert!(
                !lower.contains("token") && !lower.contains("secret") && !lower.contains("key")
            );
        }
    }
}
