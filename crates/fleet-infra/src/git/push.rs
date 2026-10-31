//! Pushing a task's branch to the remote, with the hardened git runner of the parent module.
//!
//! What this adds to the hardening of every git call (empty environment, hooks, fsmonitor, askpass,
//! ssh and credential helpers off, no prompts) is the rule for *this* call, which is the one place
//! git talks to the network:
//!
//! * **One ref, one commit.** The refspec is `<approved commit>:refs/heads/<branch>`: no `+`, no
//!   `--force`, no `--tags`, no `--follow-tags`, no `--all`, no `--mirror`, no `--delete`, no extra
//!   refs. A remote branch that has other commits makes the push fail; nothing is overwritten.
//! * **The URL is rebuilt, not passed through.** The remote's URL is read from the repository's
//!   configuration (which a worker can write), compared with the repository the approval names
//!   (`fleet_forge::push::push_target`), and git is given the URL that comparison produced.
//!   A repository whose configuration rewrites URLs (`url.<base>.insteadOf`), steers the HTTP
//!   transport (`http.*`, a proxy) or names a git proxy is refused: git would apply them to the
//!   URL after it was checked.
//! * **The branch must still be the approved commit** when git starts, read from the repository
//!   itself (the same read the merge and the evidence use).
//! * **Credentials are explicit.** Nothing of the operator's git configuration is read and no ssh
//!   key or agent is involved (an ssh remote is refused). `none`: no credential. `gh`: git runs
//!   `gh auth git-credential` as its credential helper for this one call. `helper`: git runs the
//!   one helper program the operator named by absolute path. The token, if any, is held by that
//!   helper and git; fleet never sees it, and none is in the environment.

use super::{GitVcs, Out};
use crate::jobs::JobTracker;
use fleet_app::forge::ports::{BranchPublisher, PushRequest};
use fleet_domain::env::EnvPolicy;
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::ids::{show, validate_ref_name};
use fleet_domain::sanitize::flatten_line;
use fleet_forge::config::{plain_absolute_path, ForgeConfig, PushCredentials};
use fleet_forge::push::push_target;
use fleet_forge::repo::{parse_remote, RemoteTarget};
use std::path::PathBuf;
use std::time::Duration;

pub struct GitPublisher {
    vcs: GitVcs,
    cfg: ForgeConfig,
    /// The resolved `gh`, when it is the credential helper.
    gh: Option<PathBuf>,
}

fn valid_remote_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl GitPublisher {
    /// `parent_env` is the daemon's environment: only `PATH` and `HOME` are taken from it, and
    /// that holds for the git child whatever `[env] allow` says.
    pub fn new(
        parent_env: Vec<(String, String)>,
        cfg: ForgeConfig,
        gh: Option<PathBuf>,
        tracker: JobTracker,
    ) -> GitPublisher {
        let policy = EnvPolicy::new(vec!["PATH".to_string(), "HOME".to_string()]);
        let timeout = Duration::from_millis(cfg.push_timeout_ms);
        GitPublisher {
            vcs: GitVcs::new(parent_env, policy)
                .with_tracker(tracker)
                .with_timeout(timeout),
            cfg,
            gh,
        }
    }

    /// Keys of the repository's configuration that change where or how git talks to a remote.
    fn refuse_transport_config(&self, repo: &std::path::Path) -> Result<()> {
        let listing = self
            .vcs
            .run_raw(repo, None, &[], &["config", "--list", "-z"])?;
        if !listing.ok() {
            return Ok(());
        }
        for entry in listing.stdout.split(|b| *b == 0) {
            let key = entry.split(|b| *b == b'\n').next().unwrap_or_default();
            let key = String::from_utf8_lossy(key).to_ascii_lowercase();
            let remote_proxy = key.starts_with("remote.")
                && (key.ends_with(".proxy") || key.ends_with(".proxyauthmethod"));
            if key.starts_with("url.")
                || key.starts_with("http.")
                || key.starts_with("https.")
                || key == "core.gitproxy"
                || remote_proxy
            {
                return Err(FleetError::forbidden(format!(
                    "the repository's configuration sets {}, which changes where or how git reaches a remote after fleet checked it; remove it (nothing was pushed)",
                    show(&key)
                )));
            }
        }
        Ok(())
    }

    /// The `-c` options that give this one call its credential helper.
    fn credential_args(&self) -> Result<Vec<String>> {
        let mut args = vec!["-c".to_string(), "credential.helper=".to_string()];
        match &self.cfg.push_credentials {
            PushCredentials::None => {}
            PushCredentials::Gh => {
                let gh = self.gh.as_ref().ok_or_else(|| {
                    FleetError::unavailable(
                        "push_credentials = \"gh\" needs gh, which was not found",
                    )
                })?;
                let path = gh.display().to_string();
                if !plain_absolute_path(&path) {
                    return Err(FleetError::invalid(format!(
                        "the path of gh ({}) has characters that cannot be used in a credential helper line",
                        show(&path)
                    )));
                }
                args.push("-c".to_string());
                args.push(format!("credential.helper=!{path} auth git-credential"));
            }
            PushCredentials::Helper(path) => {
                args.push("-c".to_string());
                args.push(format!("credential.helper={path}"));
            }
        }
        Ok(args)
    }

    fn failure(&self, out: &Out) -> FleetError {
        let detail = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        let low = detail.to_ascii_lowercase();
        let text = flatten_line(&detail, 300);
        if low.contains("terminal prompts disabled")
            || low.contains("authentication failed")
            || low.contains("could not read username")
            || low.contains("invalid credentials")
            || low.contains("permission denied")
            || low.contains("403")
        {
            return FleetError::unavailable(format!(
                "git could not authenticate to the remote ({text}). Set [github] push_credentials to \"gh\" (git then uses `gh auth git-credential`; sign in with `gh auth login`) or to \"helper\" with push_credential_helper; fleet reads none of your own git configuration. Nothing was pushed."
            ));
        }
        if low.contains("could not resolve host")
            || low.contains("unable to access")
            || low.contains("connection")
            || low.contains("timed out")
        {
            return FleetError::unavailable(format!(
                "git could not reach the remote ({text}). Nothing was pushed."
            ));
        }
        if low.contains("non-fast-forward")
            || low.contains("[rejected]")
            || low.contains("updates were rejected")
            || low.contains("fetch first")
            || low.contains("[remote rejected]")
        {
            return FleetError::conflict(format!(
                "the remote refused the push ({text}); fleet never forces a push, so the branch on the remote was not changed"
            ));
        }
        FleetError::new(ErrorCode::Unavailable, format!("git push failed: {text}"))
    }
}

impl BranchPublisher for GitPublisher {
    fn remote_url(&self, repo_path: &str, remote: &str) -> Result<String> {
        if !valid_remote_name(remote) {
            return Err(FleetError::invalid(format!(
                "{} is not a remote name",
                show(remote)
            )));
        }
        let repo = self.vcs.open_repo(repo_path)?;
        for key in [
            format!("remote.{remote}.pushurl"),
            format!("remote.{remote}.url"),
        ] {
            let out = self.vcs.run(&repo, None, &["config", "--get", &key])?;
            if out.ok() {
                let url = out.text();
                if !url.is_empty() {
                    return Ok(url);
                }
            }
        }
        Err(FleetError::not_found(format!(
            "the remote {} is not configured in the repository",
            show(remote)
        )))
    }

    fn push(&self, req: &PushRequest) -> Result<()> {
        if !valid_remote_name(&req.remote) {
            return Err(FleetError::invalid(format!(
                "{} is not a remote name",
                show(&req.remote)
            )));
        }
        validate_ref_name(&req.branch)?;
        if !req.branch.starts_with("fleet/") {
            return Err(FleetError::invalid(format!(
                "only branches fleet created are pushed, not {}",
                show(&req.branch)
            )));
        }
        let repo = self.vcs.open_repo(&req.repo_path)?;
        let url = self.remote_url(&req.repo_path, &req.remote)?;
        let target = push_target(&self.cfg, &url, &req.target)?;
        self.refuse_transport_config(&repo)?;
        let head = self.vcs.real_branch_head(&repo, &req.branch)?;
        if head != req.head {
            return Err(FleetError::conflict(format!(
                "the branch {} moved after it was approved (now {}); nothing was pushed",
                show(&req.branch),
                head.as_str().get(..12).unwrap_or(head.as_str())
            )));
        }
        let mut args: Vec<String> = Vec::new();
        args.extend(self.credential_args()?);
        let protocol = match parse_remote(&target)? {
            RemoteTarget::Local(_) => "protocol.file.allow=always",
            _ => "protocol.https.allow=always",
        };
        for pair in [
            protocol,
            "http.sslVerify=true",
            "http.proxy=",
            "push.followTags=false",
            "push.recurseSubmodules=no",
            "push.default=nothing",
            "push.pushOption=",
        ] {
            args.push("-c".to_string());
            args.push(pair.to_string());
        }
        for word in [
            "push",
            "--porcelain",
            "--no-verify",
            "--no-follow-tags",
            "--no-recurse-submodules",
            "--no-signed",
            "--",
        ] {
            args.push(word.to_string());
        }
        args.push(target);
        args.push(format!("{}:refs/heads/{}", req.head.as_str(), req.branch));
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.vcs.run(&repo, None, &argv)?;
        if out.ok() {
            Ok(())
        } else {
            Err(self.failure(&out))
        }
    }
}
