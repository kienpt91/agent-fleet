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
}

/// What a failed `git push` was.
#[derive(Debug, PartialEq, Eq)]
enum PushFailure {
    /// The remote answered and refused to move the ref: not a fast-forward, a hook declined.
    Refused,
    NotAuthenticated,
    Unreachable,
    Other,
}

/// The lines of the output of a failed push that are git's own words, in lower case.
///
/// Left out is what git only repeats and what somebody else wrote. The URL or path of the remote is
/// in the `To` line and in quotes in most messages, and its names are not git's words: a repository
/// can be called `403` or `connection`, and the directories of a test hold digits. What the remote
/// printed (`remote:` lines: a hook's output, a message of the server) is the remote's word, not
/// git's. Matching a marker in either would turn a refusal into an authentication failure.
fn git_lines(out: &Out, target: &str) -> Vec<String> {
    let target = target.to_ascii_lowercase();
    let mut lines = Vec::new();
    for stream in [&out.stdout, &out.stderr] {
        let text = String::from_utf8_lossy(stream);
        for raw in text.split(['\n', '\r']) {
            let line = raw.trim().to_ascii_lowercase();
            if line.is_empty() || line.starts_with("remote:") || line.starts_with("to ") {
                continue;
            }
            lines.push(if target.is_empty() {
                line
            } else {
                line.replace(&target, "")
            });
        }
    }
    lines
}

/// The number that follows `marker` in `line`: the status of "returned error: 403".
fn status_after(line: &str, marker: &str) -> Option<u16> {
    let rest = line.split_once(marker)?.1;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Tells the failures apart by what git says, most specific first: a ref status line (it exists
/// only when the remote was reached and let git in), then authentication, then the network.
fn classify_push(lines: &[String]) -> PushFailure {
    // porcelain: "!\t<from>:<to>\t[rejected] (fetch first)"; for people: " ! [rejected] a -> b (...)"
    let refused = |l: &String| {
        l.starts_with('!') && (l.contains("[rejected]") || l.contains("[remote rejected]"))
    };
    if lines.iter().any(refused) {
        return PushFailure::Refused;
    }
    let says = |phrases: &[&str]| lines.iter().any(|l| phrases.iter().any(|p| l.contains(p)));
    let http_refusal = |l: &String| {
        ["returned error: ", "rpc failed; http "]
            .iter()
            .any(|m| matches!(status_after(l, m), Some(401 | 403)))
    };
    if says(&[
        "terminal prompts disabled",
        "authentication failed",
        "could not read username",
        "could not read password",
        "invalid credentials",
        "permission denied",
    ]) || lines.iter().any(http_refusal)
    {
        return PushFailure::NotAuthenticated;
    }
    if says(&[
        "could not resolve host",
        "unable to access",
        "failed to connect",
        "connection refused",
        "connection reset",
        "network is unreachable",
        "timed out",
    ]) {
        return PushFailure::Unreachable;
    }
    PushFailure::Other
}

fn failure(out: &Out, target: &str) -> FleetError {
    let detail = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    let text = flatten_line(&detail, 300);
    match classify_push(&git_lines(out, target)) {
        PushFailure::Refused => FleetError::conflict(format!(
            "the remote refused the push ({text}); fleet never forces a push, so the branch on the remote was not changed"
        )),
        PushFailure::NotAuthenticated => FleetError::unavailable(format!(
            "git could not authenticate to the remote ({text}). Set [github] push_credentials to \"gh\" (git then uses `gh auth git-credential`; sign in with `gh auth login`) or to \"helper\" with push_credential_helper; fleet reads none of your own git configuration. Nothing was pushed."
        )),
        PushFailure::Unreachable => FleetError::unavailable(format!(
            "git could not reach the remote ({text}). Nothing was pushed."
        )),
        PushFailure::Other => {
            FleetError::new(ErrorCode::Unavailable, format!("git push failed: {text}"))
        }
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
        args.push(target.clone());
        args.push(format!("{}:refs/heads/{}", req.head.as_str(), req.branch));
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self.vcs.run(&repo, None, &argv)?;
        if out.ok() {
            Ok(())
        } else {
            Err(failure(&out, &target))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A commit whose name has `403` in it: one in about a hundred commits does.
    const SHA_WITH_403: &str = "c6254030ca394c36317ee4cf1e750bb78ac19d69";

    fn kind(stdout: &str, stderr: &str, target: &str) -> PushFailure {
        let out = Out {
            code: Some(1),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        };
        classify_push(&git_lines(&out, target))
    }

    /// What git 2.39 prints for a push that is not a fast-forward, as the porcelain output on
    /// stdout and the rest on stderr.
    fn refusal(target: &str, why: &str) -> (String, String) {
        (
            format!(
                "To {target}\n!\t{SHA_WITH_403}:refs/heads/fleet/r1/a\t[rejected] ({why})\nDone\n"
            ),
            format!(
                "error: failed to push some refs to '{target}'\nhint: Updates were rejected because the remote contains work that you do\nhint: not have locally.\n"
            ),
        )
    }

    #[test]
    fn a_refused_ref_is_a_refusal_whatever_numbers_or_names_the_text_holds() {
        // the commit id above holds 403, and so may the pid and the clock reading in a path
        for target in [
            "/tmp/fleet-test-forge-push-24034-0-115403385/remote.git",
            "/srv/connection/403.git",
            "https://github.com/o/403.git",
            "https://github.com/o/connection.git",
        ] {
            for why in ["fetch first", "non-fast-forward"] {
                let (stdout, stderr) = refusal(target, why);
                assert_eq!(
                    kind(&stdout, &stderr, target),
                    PushFailure::Refused,
                    "{target}"
                );
                // git prints the URL in the form it got it, and with a slash after it for https
                let quoted = format!("{target}/");
                assert_eq!(kind(&stdout, &stderr, &quoted), PushFailure::Refused);
            }
        }
        // the same line the way git shows it to people, with the space in front
        assert_eq!(
            kind(
                "",
                " ! [rejected]        a -> b (non-fast-forward)\nerror: failed to push some refs\n",
                "/x"
            ),
            PushFailure::Refused
        );
        assert_eq!(
            kind(
                "",
                "! [remote rejected] a -> b (protected branch hook declined)\n",
                "/x"
            ),
            PushFailure::Refused
        );
    }

    #[test]
    fn what_the_remote_prints_is_not_gits_word() {
        let target = "https://github.com/o/r.git";
        // a hook declines and talks about everything
        let stdout = format!(
            "To {target}\n!\t{SHA_WITH_403}:refs/heads/fleet/r1/a\t[remote rejected] (pre-receive hook declined)\nDone\n"
        );
        let stderr = format!(
            "remote: 403 Forbidden\nremote: authentication failed, permission denied\nremote: connection timed out; could not resolve host\nerror: failed to push some refs to '{target}'\n"
        );
        assert_eq!(kind(&stdout, &stderr, target), PushFailure::Refused);
        // the same words from the remote, with nothing of git's own to say about them
        assert_eq!(
            kind(
                "",
                "remote: Permission denied\nremote: HTTP 403 returned error: 403\nfatal: the remote end hung up unexpectedly\n",
                target
            ),
            PushFailure::Other
        );
        // a carriage return does not hide the prefix of the next line
        assert_eq!(
            kind(
                "",
                "remote: ok\rremote: Authentication failed\nfatal: oops\n",
                target
            ),
            PushFailure::Other
        );
    }

    #[test]
    fn authentication_failures_are_told_by_gits_words_not_by_a_name() {
        for (target, stderr) in [
            (
                "https://github.com/o/connection.git",
                "fatal: could not read Username for 'https://github.com': terminal prompts disabled\n",
            ),
            (
                "https://github.com/o/fetch-first.git",
                "remote: Invalid username or password.\nfatal: Authentication failed for 'https://github.com/o/fetch-first.git/'\n",
            ),
            (
                "https://github.com/o/r.git",
                "remote: Permission to o/r.git denied to u.\nfatal: unable to access 'https://github.com/o/r.git/': The requested URL returned error: 403\n",
            ),
            (
                "https://github.com/o/r.git",
                "error: RPC failed; HTTP 401 curl 22 The requested URL returned error: 401\nfatal: the remote end hung up unexpectedly\n",
            ),
            (
                "https://github.com/o/r.git",
                "fatal: could not read Password for 'https://github.com': terminal prompts disabled\n",
            ),
        ] {
            assert_eq!(kind("", stderr, target), PushFailure::NotAuthenticated, "{stderr}");
        }
    }

    #[test]
    fn a_number_in_a_name_is_not_an_http_status() {
        for (target, stderr) in [
            (
                "https://github.com/o/403.git",
                "fatal: unable to access 'https://github.com/o/403.git/': Could not resolve host: github.com\n",
            ),
            (
                "https://github.com/o/r403.git",
                "fatal: unable to access 'https://github.com/o/r403.git/': Failed to connect to github.com port 443 after 0 ms: Couldn't connect to server\n",
            ),
            (
                "https://github.com/o/403.git",
                "fatal: unable to access 'https://github.com/o/403.git/': Connection timed out after 300001 milliseconds\n",
            ),
        ] {
            assert_eq!(kind("", stderr, target), PushFailure::Unreachable, "{stderr}");
        }
        // other statuses, and a status that is only part of a longer number, are not a refusal of
        // the credentials
        assert_eq!(
            kind("", "fatal: unable to access 'https://x.example/r.git/': The requested URL returned error: 4031\n", "https://x.example/r.git"),
            PushFailure::Unreachable
        );
        assert_eq!(
            kind(
                "",
                "fatal: repository 'https://github.com/o/403.git/' not found\n",
                "https://github.com/o/403.git"
            ),
            PushFailure::Other
        );
    }

    #[test]
    fn the_url_of_the_remote_is_not_read_for_markers() {
        // a target no real URL has (it has spaces), to prove that it is cut out of what is read
        let target = "/srv/permission denied/connection timed out";
        let stderr = format!("error: failed to push some refs to '{target}'\n");
        assert_eq!(kind("", &stderr, target), PushFailure::Other);
        assert_eq!(
            kind("", &stderr, "/srv/other"),
            PushFailure::NotAuthenticated
        );
    }

    #[test]
    fn the_texts_of_the_failures_are_the_ones_the_commands_show() {
        let out = |stderr: &str| Out {
            code: Some(1),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        };
        let e = failure(&out("fatal: unable to access 'https://h/x.git/': The requested URL returned error: 403\n"), "https://h/x.git");
        assert_eq!(e.code, ErrorCode::Unavailable);
        assert!(
            e.message.contains("could not authenticate"),
            "{}",
            e.message
        );
        let e = failure(
            &out("! [rejected] a -> b (fetch first)\n"),
            "https://h/x.git",
        );
        assert_eq!(e.code, ErrorCode::Conflict);
        assert!(e.message.contains("never forces"), "{}", e.message);
        let e = failure(
            &out("fatal: unable to access 'https://h/x.git/': Could not resolve host: h\n"),
            "https://h/x.git",
        );
        assert!(
            e.message.contains("could not reach the remote"),
            "{}",
            e.message
        );
        let e = failure(&out("something strange\n"), "https://h/x.git");
        assert!(e.message.contains("git push failed"), "{}", e.message);
    }
}
