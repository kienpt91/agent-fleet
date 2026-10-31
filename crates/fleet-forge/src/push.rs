//! Where a branch may be pushed. The URL of a remote is configuration of the repository, which
//! workers can reach; before anything is pushed it is read again, parsed, and compared with the
//! repository the approval names. The URL that git is given is rebuilt from that comparison, never
//! passed through as it was written.

use crate::config::ForgeConfig;
use crate::repo::{parse_remote, RemoteTarget, RepoRef};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::show;

/// Same host, owner and name; GitHub names are not case sensitive.
pub fn same_repo(a: &RepoRef, b: &RepoRef) -> bool {
    a.host() == b.host()
        && a.owner().eq_ignore_ascii_case(b.owner())
        && a.name().eq_ignore_ascii_case(b.name())
}

/// The URL to push to, for `expected`, given the URL of the configured remote (`remote_url`) and
/// the configuration. Refuses a remote that names another repository or host, one on a host that
/// is not allowed, an ssh remote, and a local path unless `allow_local_remote` is set.
pub fn push_target(cfg: &ForgeConfig, remote_url: &str, expected: &RepoRef) -> Result<String> {
    let url = cfg.push_url.as_deref().unwrap_or(remote_url);
    match parse_remote(url)? {
        RemoteTarget::Https(r) => {
            if !cfg.allowed_hosts.iter().any(|h| h == r.host()) {
                return Err(FleetError::forbidden(format!(
                    "the remote {} is on a host that is not in allowed_hosts",
                    show(&r.full())
                )));
            }
            if !same_repo(&r, expected) {
                return Err(FleetError::conflict(format!(
                    "the remote {} names {} but the pull request is for {}; nothing was pushed",
                    show(&cfg.remote),
                    show(&r.full()),
                    show(&expected.full())
                )));
            }
            Ok(r.https_url())
        }
        RemoteTarget::Ssh(_) => Err(FleetError::invalid(format!(
            "the remote {} is an ssh address; fleet pushes over https only, so that no ssh agent or key is involved. Set [github] push_url to the https URL of the repository (and push_credentials)",
            show(&cfg.remote)
        ))),
        RemoteTarget::Local(path) => {
            if cfg.allow_local_remote {
                Ok(path)
            } else {
                Err(FleetError::forbidden(format!(
                    "the remote {} is a path on this machine; set [github] allow_local_remote = true to push there",
                    show(&cfg.remote)
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ForgeConfig {
        ForgeConfig::disabled()
    }

    fn repo() -> RepoRef {
        RepoRef::new("github.com", "Octo", "Repo").unwrap()
    }

    #[test]
    fn an_https_remote_for_the_same_repository_is_rebuilt() {
        let url = push_target(&cfg(), "https://github.com/octo/repo", &repo()).unwrap();
        assert_eq!(url, "https://github.com/octo/repo.git");
        assert!(same_repo(
            &repo(),
            &RepoRef::new("github.com", "OCTO", "REPO").unwrap()
        ));
    }

    #[test]
    fn another_repository_host_or_transport_is_refused() {
        let c = cfg();
        for url in [
            "https://github.com/octo/other",
            "https://evil.example/octo/repo",
            "ssh://git@github.com/octo/repo.git",
            "git@github.com:octo/repo.git",
            "/srv/git/repo.git",
            "file:///srv/git/repo.git",
            "https://user:pw@github.com/octo/repo",
            "ext::sh -c id",
            "",
        ] {
            assert!(push_target(&c, url, &repo()).is_err(), "{url}");
        }
    }

    #[test]
    fn a_local_remote_needs_the_explicit_option() {
        let mut c = cfg();
        assert!(push_target(&c, "/srv/git/repo.git", &repo()).is_err());
        c.allow_local_remote = true;
        assert_eq!(
            push_target(&c, "/srv/git/repo.git", &repo()).unwrap(),
            "/srv/git/repo.git"
        );
        assert_eq!(
            push_target(&c, "file:///srv/git/repo.git", &repo()).unwrap(),
            "/srv/git/repo.git"
        );
        // the option does not let a forge URL name another repository
        assert!(push_target(&c, "https://github.com/octo/other", &repo()).is_err());
    }

    #[test]
    fn push_url_replaces_the_remotes_url() {
        let mut c = cfg();
        c.push_url = Some("https://github.com/octo/repo.git".into());
        assert!(push_target(&c, "git@github.com:octo/repo.git", &repo()).is_ok());
        c.push_url = Some("https://github.com/octo/other.git".into());
        assert!(push_target(&c, "https://github.com/octo/repo", &repo()).is_err());
    }

    #[test]
    fn hosts_must_be_allowed() {
        let mut c = cfg();
        c.allowed_hosts = vec!["ghe.example.org".into()];
        c.host = "ghe.example.org".into();
        let r = RepoRef::new("github.com", "o", "r").unwrap();
        assert!(push_target(&c, "https://github.com/o/r", &r).is_err());
        let r = RepoRef::new("ghe.example.org", "o", "r").unwrap();
        assert!(push_target(&c, "https://ghe.example.org/o/r", &r).is_ok());
    }
}
