//! Repositories on a forge host and the URLs of git remotes.
//!
//! Everything here is validated text: a host, an owner and a name are short and use a closed
//! alphabet, so they can be placed in an API path or an argument without quoting. A remote URL
//! that carries a user name or a password is refused outright (a credential in a URL ends up in
//! process lists, logs and `.git/config`).

use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::show;

const MAX_HOST: usize = 253;
const MAX_OWNER: usize = 39;
const MAX_NAME: usize = 100;
const MAX_LOCAL_PATH: usize = 1024;

/// A repository: `host/owner/name`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoRef {
    host: String,
    owner: String,
    name: String,
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= MAX_HOST
        && host.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

fn valid_owner(owner: &str) -> bool {
    !owner.is_empty()
        && owner.len() <= MAX_OWNER
        && !owner.starts_with('-')
        && !owner.ends_with('-')
        && owner.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.starts_with(['-', '.'])
        && !name.ends_with('.')
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl RepoRef {
    pub fn new(host: &str, owner: &str, name: &str) -> Result<RepoRef> {
        let host = host.to_ascii_lowercase();
        if !valid_host(&host) {
            return Err(FleetError::invalid(format!(
                "{} is not a host name (lower case letters, digits, '-' and '.')",
                show(&host)
            )));
        }
        if !valid_owner(owner) {
            return Err(FleetError::invalid(format!(
                "{} is not a repository owner (1-{MAX_OWNER} letters, digits or '-')",
                show(owner)
            )));
        }
        if !valid_name(name) {
            return Err(FleetError::invalid(format!(
                "{} is not a repository name (letters, digits, '-', '_' and '.')",
                show(name)
            )));
        }
        Ok(RepoRef {
            host,
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }

    /// `owner/name` on `host`.
    pub fn from_slug(host: &str, slug: &str) -> Result<RepoRef> {
        match slug.split_once('/') {
            Some((owner, name)) if !name.contains('/') => RepoRef::new(host, owner, name),
            _ => Err(FleetError::invalid(format!(
                "{} is not a repository: write it as owner/name",
                show(slug)
            ))),
        }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `owner/name`.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// `host/owner/name`: the form that appears in approvals and events.
    pub fn full(&self) -> String {
        format!("{}/{}/{}", self.host, self.owner, self.name)
    }

    /// The REST path of the repository, relative to the API root.
    pub fn api_path(&self) -> String {
        format!("repos/{}/{}", self.owner, self.name)
    }

    /// The HTTPS URL git pushes to.
    pub fn https_url(&self) -> String {
        format!("https://{}/{}/{}.git", self.host, self.owner, self.name)
    }
}

impl std::fmt::Display for RepoRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.full())
    }
}

/// Where a git remote points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteTarget {
    /// `https://host/owner/name[.git]`.
    Https(RepoRef),
    /// `ssh://git@host/owner/name[.git]` or `git@host:owner/name[.git]`.
    Ssh(RepoRef),
    /// An absolute path or a `file://` URL: a repository on this machine.
    Local(String),
}

impl RemoteTarget {
    /// The repository a remote on a forge names; a local path names none.
    pub fn repo(&self) -> Option<&RepoRef> {
        match self {
            RemoteTarget::Https(r) | RemoteTarget::Ssh(r) => Some(r),
            RemoteTarget::Local(_) => None,
        }
    }
}

fn plain_text(url: &str) -> bool {
    url.chars()
        .all(|c| !c.is_control() && !c.is_whitespace() && !matches!(c, '"' | '\'' | '\\' | '`'))
}

fn split_repo_path(host: &str, path: &str) -> Result<RepoRef> {
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    RepoRef::from_slug(host, path)
}

/// Reads the URL of a git remote. Anything that is not one of the forms above is refused: a
/// relative path, `http://`, `git://`, `ext::` transports, option-like text, a URL with a user
/// name or password (except the `git@` of the ssh forms), a query or a fragment.
pub fn parse_remote(url: &str) -> Result<RemoteTarget> {
    let refuse = |why: &str| {
        Err(FleetError::invalid(format!(
            "the remote URL {} is not usable: {why}",
            show(url)
        )))
    };
    if url.is_empty() || url.len() > MAX_LOCAL_PATH {
        return refuse("it is empty or too long");
    }
    if url.starts_with('-') {
        return refuse("it looks like an option");
    }
    if !plain_text(url) {
        return refuse("it contains control characters, white space or quotes");
    }
    if let Some(rest) = url.strip_prefix("https://") {
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        if authority.contains('@') {
            return refuse("it carries a user name or a password");
        }
        if authority.contains(':') {
            return refuse("it names a port");
        }
        if path.contains(['?', '#']) {
            return refuse("it has a query or a fragment");
        }
        return split_repo_path(authority, path).map(RemoteTarget::Https);
    }
    if let Some(rest) = url.strip_prefix("ssh://") {
        let Some(rest) = rest.strip_prefix("git@") else {
            return refuse("an ssh URL must use the user git, with no password");
        };
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        if authority.contains(['@', ':']) {
            return refuse("it carries more than a host");
        }
        if path.contains(['?', '#']) {
            return refuse("it has a query or a fragment");
        }
        return split_repo_path(authority, path).map(RemoteTarget::Ssh);
    }
    if let Some(rest) = url.strip_prefix("file://") {
        if rest.starts_with('/') && !rest.contains(['?', '#']) {
            return Ok(RemoteTarget::Local(rest.to_string()));
        }
        return refuse("a file URL needs an absolute path");
    }
    if url.starts_with('/') {
        return Ok(RemoteTarget::Local(url.to_string()));
    }
    if let Some(rest) = url.strip_prefix("git@") {
        if let Some((host, path)) = rest.split_once(':') {
            if !host.contains('/') && !path.starts_with('/') && !path.contains(['?', '#']) {
                return split_repo_path(host, path).map(RemoteTarget::Ssh);
            }
        }
        return refuse("not an scp-style address");
    }
    refuse("only https://, ssh://git@, git@host:path and absolute local paths are understood")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(host: &str, owner: &str, name: &str) -> RepoRef {
        RepoRef::new(host, owner, name).unwrap()
    }

    #[test]
    fn a_repository_has_one_text_form_per_use() {
        let r = repo("GitHub.com", "octo-org", "my.repo_1");
        assert_eq!(r.host(), "github.com");
        assert_eq!(r.slug(), "octo-org/my.repo_1");
        assert_eq!(r.full(), "github.com/octo-org/my.repo_1");
        assert_eq!(r.api_path(), "repos/octo-org/my.repo_1");
        assert_eq!(r.https_url(), "https://github.com/octo-org/my.repo_1.git");
        assert_eq!(
            RepoRef::from_slug("github.com", "a/b").unwrap(),
            repo("github.com", "a", "b")
        );
    }

    #[test]
    fn hostile_names_are_refused() {
        for (host, owner, name) in [
            ("", "a", "b"),
            ("git hub.com", "a", "b"),
            ("github.com/x", "a", "b"),
            ("-x.com", "a", "b"),
            ("github..com", "a", "b"),
            ("github.com", "", "b"),
            ("github.com", "a/b", "c"),
            ("github.com", "-a", "b"),
            ("github.com", "a_b", "c"),
            ("github.com", &"a".repeat(40), "c"),
            ("github.com", "a", ""),
            ("github.com", "a", ".."),
            ("github.com", "a", ".hidden"),
            ("github.com", "a", "-opt"),
            ("github.com", "a", "b c"),
            ("github.com", "a", "b\u{1b}[31m"),
            ("github.com", "a", "b\u{202e}c"),
            ("github.com", "a", "b;rm"),
            ("github.com", "a", &"n".repeat(101)),
        ] {
            assert!(
                RepoRef::new(host, owner, name).is_err(),
                "{host:?} {owner:?} {name:?}"
            );
        }
        for slug in ["a", "a/b/c", "/b", "a/", ""] {
            assert!(RepoRef::from_slug("github.com", slug).is_err(), "{slug:?}");
        }
    }

    #[test]
    fn remote_urls_of_the_known_forms_are_read() {
        let want = repo("github.com", "o", "r");
        for url in [
            "https://github.com/o/r",
            "https://github.com/o/r.git",
            "https://github.com/o/r/",
            "ssh://git@github.com/o/r.git",
            "git@github.com:o/r.git",
            "git@github.com:o/r",
        ] {
            let t = parse_remote(url).unwrap();
            assert_eq!(t.repo(), Some(&want), "{url}");
        }
        assert!(matches!(
            parse_remote("https://github.com/o/r").unwrap(),
            RemoteTarget::Https(_)
        ));
        assert!(matches!(
            parse_remote("git@github.com:o/r").unwrap(),
            RemoteTarget::Ssh(_)
        ));
        assert_eq!(
            parse_remote("/srv/git/r.git").unwrap(),
            RemoteTarget::Local("/srv/git/r.git".into())
        );
        assert_eq!(
            parse_remote("file:///srv/git/r.git").unwrap(),
            RemoteTarget::Local("/srv/git/r.git".into())
        );
        assert_eq!(parse_remote("/srv/git/r.git").unwrap().repo(), None);
    }

    #[test]
    fn urls_that_carry_credentials_or_odd_transports_are_refused() {
        for url in [
            "",
            "-oProxyCommand=x",
            "https://user@github.com/o/r",
            "https://user:pw@github.com/o/r",
            "https://github.com:8443/o/r",
            "https://github.com/o/r?x=1",
            "https://github.com/o/r#frag",
            "https://github.com/o",
            "https://github.com/o/r/extra",
            "http://github.com/o/r",
            "git://github.com/o/r",
            "ext::sh -c id",
            "ssh://user:pw@github.com/o/r",
            "ssh://bob@github.com/o/r",
            "ssh://git:pw@github.com/o/r",
            "ssh://git@github.com:22/o/r",
            "git@github.com:/o/r",
            "git@github.com/o/r",
            "relative/path",
            "./r",
            "file://host/path",
            "https://github.com/o/r\n",
            "https://github.com/o/r\u{1b}[0m",
            "https://gith ub.com/o/r",
        ] {
            assert!(parse_remote(url).is_err(), "{url:?}");
        }
        let long = format!("/{}", "a".repeat(2000));
        assert!(parse_remote(&long).is_err());
    }
}
