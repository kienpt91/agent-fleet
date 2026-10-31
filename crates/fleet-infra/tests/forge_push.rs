//! Pushing a task branch with the hardened git runner, to a local bare repository as the remote.
//! A wrapper script placed first on the PATH of the publisher records every git invocation (its
//! arguments and its environment) and passes it on to the real git, except where a test makes the
//! push fail the way a real HTTPS remote would (so that nothing here needs a network).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_app::forge::ports::{BranchPublisher, PushRequest};
use fleet_domain::error::ErrorCode;
use fleet_domain::ids::Sha;
use fleet_forge::config::{ForgeConfig, PushCredentials};
use fleet_forge::repo::RepoRef;
use fleet_infra::git::GitPublisher;
use fleet_infra::jobs::JobTracker;
use fleet_infra::testutil::{git, git_available, make_repo, TempDir};
use std::path::{Path, PathBuf};

const SENTINEL: &str = "fleet-test-sentinel-7f3a9c1d2e4b5a69";
const BRANCH: &str = "fleet/r1/a";

macro_rules! need_git {
    () => {
        if !git_available() {
            eprintln!("git is not available; skipping");
            return;
        }
    };
}

fn real_git() -> String {
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        let c = Path::new(dir).join("git");
        if c.is_file() {
            return c.display().to_string();
        }
    }
    panic!("no git on PATH");
}

struct Fx {
    t: TempDir,
    repo: PathBuf,
    bare: PathBuf,
    head: Sha,
    cfg: ForgeConfig,
}

impl Fx {
    fn new() -> Fx {
        let t = TempDir::new("forge-push");
        let repo = make_repo(&t);
        git(&repo, &["checkout", "-q", "-b", BRANCH]);
        std::fs::write(repo.join("feature.txt"), "the work\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "work"]);
        let head = Sha::parse(&git(&repo, &["rev-parse", "HEAD"])).unwrap();
        git(&repo, &["checkout", "-q", "main"]);
        let bare = t.join("remote.git");
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "-q", "--bare", "-b", "main"]);
        git(
            &repo,
            &["remote", "add", "origin", &bare.display().to_string()],
        );
        let mut cfg = ForgeConfig::disabled();
        cfg.enabled = true;
        cfg.allow_local_remote = true;
        t.script(
            "wrap/git",
            &format!(
                r#"#!/bin/sh
DIR='{dir}'
n=$(cat "$DIR/n" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$DIR/n"
{{ for a in "$@"; do printf '%s\n' "$a"; done; }} > "$DIR/argv.$n"
env | sort > "$DIR/env.$n"
for a in "$@"; do
  if [ "$a" = push ] && [ -e "$DIR/fail-push" ]; then cat "$DIR/fail-push" >&2; exit 1; fi
done
exec '{real}' "$@"
"#,
                dir = t.join("wrap").display(),
                real = real_git(),
            ),
        );
        Fx {
            t,
            repo,
            bare,
            head,
            cfg,
        }
    }

    fn publisher(&self) -> GitPublisher {
        self.publisher_with(self.cfg.clone(), None)
    }

    fn publisher_with(&self, cfg: ForgeConfig, gh: Option<PathBuf>) -> GitPublisher {
        let env = vec![
            (
                "PATH".to_string(),
                format!("{}:/usr/bin:/bin", self.t.join("wrap").display()),
            ),
            (
                "HOME".to_string(),
                self.t.join("home").display().to_string(),
            ),
            ("GH_TOKEN".to_string(), SENTINEL.to_string()),
            ("GITHUB_TOKEN".to_string(), SENTINEL.to_string()),
            ("SSH_AUTH_SOCK".to_string(), "/tmp/agent.sock".to_string()),
            ("GIT_ASKPASS".to_string(), "/tmp/evil-askpass".to_string()),
            ("FLEET_CONTROL_TOKEN".to_string(), SENTINEL.to_string()),
        ];
        GitPublisher::new(env, cfg, gh, JobTracker::new())
    }

    fn request(&self) -> PushRequest {
        PushRequest {
            repo_path: self.repo.display().to_string(),
            remote: "origin".into(),
            branch: BRANCH.into(),
            head: self.head.clone(),
            target: RepoRef::new("github.com", "o", "r").unwrap(),
        }
    }

    fn remote_refs(&self) -> Vec<String> {
        git(
            &self.bare,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        )
        .lines()
        .map(str::to_string)
        .collect()
    }

    /// The arguments of the last invocation that ran `push`.
    fn push_argv(&self) -> Vec<String> {
        let n: usize = self.t.read("wrap/n").trim().parse().unwrap();
        for i in (1..=n).rev() {
            let args: Vec<String> = self
                .t
                .read(&format!("wrap/argv.{i}"))
                .lines()
                .map(str::to_string)
                .collect();
            if args.iter().any(|a| a == "push") {
                return args;
            }
        }
        panic!("git push was never run");
    }

    fn push_env(&self) -> String {
        let n: usize = self.t.read("wrap/n").trim().parse().unwrap();
        for i in (1..=n).rev() {
            let args = self.t.read(&format!("wrap/argv.{i}"));
            if args.lines().any(|a| a == "push") {
                return self.t.read(&format!("wrap/env.{i}"));
            }
        }
        panic!("git push was never run");
    }

    fn pushed(&self) -> bool {
        let n: usize = self.t.read("wrap/n").trim().parse().unwrap_or(0);
        (1..=n).any(|i| {
            self.t
                .read(&format!("wrap/argv.{i}"))
                .lines()
                .any(|a| a == "push")
        })
    }
}

// ---- what is pushed ------------------------------------------------------------------------------------

#[test]
fn exactly_the_approved_commit_goes_to_exactly_one_ref_of_the_remote() {
    need_git!();
    let fx = Fx::new();
    git(&fx.repo, &["tag", "v1"]);
    git(&fx.repo, &["tag", "-a", "-m", "annotated", "v2", BRANCH]);
    git(&fx.repo, &["config", "push.followTags", "true"]);
    git(&fx.repo, &["branch", "other", BRANCH]);
    fx.publisher().push(&fx.request()).unwrap();
    assert_eq!(
        fx.remote_refs(),
        [format!("refs/heads/{BRANCH} {}", fx.head)]
    );
    // the local repository is as it was
    assert_eq!(
        git(&fx.repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "main"
    );
}

#[test]
fn the_push_is_a_plain_one_ref_push_without_force_tags_or_extra_refs() {
    need_git!();
    let fx = Fx::new();
    fx.publisher().push(&fx.request()).unwrap();
    let argv = fx.push_argv();
    let at = argv.iter().position(|a| a == "push").unwrap();
    let after = &argv[at..];
    let want_tail = [
        "push".to_string(),
        "--porcelain".into(),
        "--no-verify".into(),
        "--no-follow-tags".into(),
        "--no-recurse-submodules".into(),
        "--no-signed".into(),
        "--".into(),
        fx.bare.display().to_string(),
        format!("{}:refs/heads/{BRANCH}", fx.head),
    ];
    assert_eq!(after, want_tail);
    for a in &argv {
        assert!(
            !matches!(
                a.as_str(),
                "--force"
                    | "-f"
                    | "--force-with-lease"
                    | "--tags"
                    | "--all"
                    | "--mirror"
                    | "--delete"
                    | "-d"
                    | "--prune"
            ) && !a.starts_with('+')
                && !a.starts_with("--force")
                && !a.starts_with("--receive-pack")
                && !a.starts_with("--exec"),
            "{a}"
        );
    }
    let joined = argv.join("\n");
    assert!(
        joined.contains("credential.helper=\n") || joined.ends_with("credential.helper="),
        "{joined}"
    );
    assert!(joined.contains("protocol.file.allow=always"));
    assert!(joined.contains("http.sslVerify=true"));
}

#[test]
fn the_child_has_no_token_no_agent_and_no_askpass() {
    need_git!();
    let fx = Fx::new();
    fx.publisher().push(&fx.request()).unwrap();
    let env = fx.push_env();
    for name in [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "SSH_AUTH_SOCK",
        "FLEET_CONTROL_TOKEN",
    ] {
        assert!(
            !env.contains(&format!("{name}=")),
            "{name} reached git:\n{env}"
        );
    }
    assert!(!env.contains(SENTINEL), "{env}");
    assert!(!env.contains("evil-askpass"), "{env}");
    assert!(env.contains("GIT_TERMINAL_PROMPT=0"));
    assert!(env.contains("GIT_CONFIG_GLOBAL=/dev/null"));
}

#[test]
fn a_remote_branch_with_other_commits_is_never_overwritten() {
    need_git!();
    let fx = Fx::new();
    // someone else put an unrelated commit on that branch of the remote
    let other = TempDir::new("other-clone");
    let clone = other.join("c");
    git(
        other.path(),
        &["clone", "-q", &fx.bare.display().to_string(), "c"],
    );
    git(&clone, &["checkout", "-q", "-b", BRANCH]);
    std::fs::write(clone.join("theirs.txt"), "theirs\n").unwrap();
    git(&clone, &["add", "-A"]);
    git(&clone, &["commit", "-q", "-m", "theirs"]);
    git(&clone, &["push", "-q", "origin", BRANCH]);
    let theirs = git(&clone, &["rev-parse", "HEAD"]);
    let err = fx.publisher().push(&fx.request()).unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict, "{err:?}");
    assert!(err.message.contains("never forces"), "{}", err.message);
    assert_eq!(fx.remote_refs(), [format!("refs/heads/{BRANCH} {theirs}")]);
}

#[test]
fn a_branch_that_moved_since_the_approval_is_not_pushed() {
    need_git!();
    let fx = Fx::new();
    git(&fx.repo, &["checkout", "-q", BRANCH]);
    std::fs::write(fx.repo.join("more.txt"), "more\n").unwrap();
    git(&fx.repo, &["add", "-A"]);
    git(&fx.repo, &["commit", "-q", "-m", "more"]);
    git(&fx.repo, &["checkout", "-q", "main"]);
    let err = fx.publisher().push(&fx.request()).unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert!(
        err.message.contains("moved after it was approved"),
        "{}",
        err.message
    );
    assert!(!fx.pushed(), "git push was never started");
    assert!(fx.remote_refs().is_empty());
}

#[test]
fn hooks_and_planted_configuration_do_not_run_during_a_push() {
    need_git!();
    let fx = Fx::new();
    let marker = fx.t.join("marker");
    let hook = format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display());
    fx.t.script("repo/.git/hooks/pre-push", &hook);
    fx.t.script("evil-hooks/pre-push", &hook);
    git(
        &fx.repo,
        &[
            "config",
            "core.hooksPath",
            &fx.t.join("evil-hooks").display().to_string(),
        ],
    );
    fx.t.script(
        "askpass",
        &format!(
            "#!/bin/sh\ntouch '{}'\necho x\n",
            fx.t.join("askpass-ran").display()
        ),
    );
    git(
        &fx.repo,
        &[
            "config",
            "core.askPass",
            &fx.t.join("askpass").display().to_string(),
        ],
    );
    git(
        &fx.repo,
        &[
            "config",
            "core.sshCommand",
            &fx.t.join("askpass").display().to_string(),
        ],
    );
    git(
        &fx.repo,
        &[
            "config",
            "credential.helper",
            &format!("!touch {}", fx.t.join("helper-ran").display()),
        ],
    );
    fx.publisher().push(&fx.request()).unwrap();
    assert!(!marker.exists(), "a pre-push hook ran");
    assert!(!fx.t.join("askpass-ran").exists());
    assert!(!fx.t.join("helper-ran").exists());
    assert_eq!(fx.remote_refs().len(), 1);
}

#[test]
fn configuration_that_changes_where_or_how_git_reaches_a_remote_stops_the_push() {
    need_git!();
    for (key, value) in [
        ("url.https://evil.example/.insteadOf", "https://github.com/"),
        (
            "url.https://evil.example/.pushInsteadOf",
            "https://github.com/",
        ),
        ("http.proxy", "http://evil.example:3128"),
        ("http.sslVerify", "false"),
        ("http.https://github.com/.extraHeader", "X: y"),
        ("core.gitProxy", "/tmp/evil"),
        ("remote.origin.proxy", "http://evil.example"),
    ] {
        let fx = Fx::new();
        git(&fx.repo, &["config", key, value]);
        let err = fx.publisher().push(&fx.request()).unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden, "{key}: {err:?}");
        assert!(
            err.message.contains("nothing was pushed"),
            "{key}: {}",
            err.message
        );
        assert!(!fx.pushed(), "{key}");
        assert!(fx.remote_refs().is_empty());
    }
}

// ---- where ---------------------------------------------------------------------------------------------------

#[test]
fn only_a_remote_that_names_the_approved_repository_over_https_is_pushed_to() {
    need_git!();
    let fx = Fx::new();
    for (url, code) in [
        ("https://github.com/o/other.git", ErrorCode::Conflict),
        ("https://evil.example/o/r.git", ErrorCode::Forbidden),
        ("git@github.com:o/r.git", ErrorCode::Invalid),
        ("ssh://git@github.com/o/r.git", ErrorCode::Invalid),
        ("https://user:pw@github.com/o/r.git", ErrorCode::Invalid),
        ("relative/path", ErrorCode::Invalid),
    ] {
        git(&fx.repo, &["remote", "set-url", "origin", url]);
        let err = fx.publisher().push(&fx.request()).unwrap_err();
        assert_eq!(err.code, code, "{url}: {}", err.message);
        assert!(!fx.pushed(), "{url}");
    }
    // a local path needs the explicit option
    git(
        &fx.repo,
        &[
            "remote",
            "set-url",
            "origin",
            &fx.bare.display().to_string(),
        ],
    );
    let mut cfg = fx.cfg.clone();
    cfg.allow_local_remote = false;
    let err = fx
        .publisher_with(cfg, None)
        .push(&fx.request())
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Forbidden);
    assert!(!fx.pushed());
}

#[test]
fn a_push_url_in_the_configuration_replaces_the_one_of_the_repository() {
    need_git!();
    let fx = Fx::new();
    git(
        &fx.repo,
        &["remote", "set-url", "origin", "git@github.com:o/r.git"],
    );
    let mut cfg = fx.cfg.clone();
    cfg.push_url = Some(fx.bare.display().to_string());
    fx.publisher_with(cfg, None).push(&fx.request()).unwrap();
    assert_eq!(fx.remote_refs().len(), 1);
}

#[test]
fn only_fleet_branches_and_valid_remote_names_are_pushed() {
    need_git!();
    let fx = Fx::new();
    let mut r = fx.request();
    r.branch = "main".into();
    assert_eq!(
        fx.publisher().push(&r).unwrap_err().code,
        ErrorCode::Invalid
    );
    r.branch = "fleet/../x".into();
    assert!(fx.publisher().push(&r).is_err());
    let mut r = fx.request();
    r.remote = "--upload-pack=touch x".into();
    assert_eq!(
        fx.publisher().push(&r).unwrap_err().code,
        ErrorCode::Invalid
    );
    assert!(fx
        .publisher()
        .remote_url(&fx.repo.display().to_string(), "--upload-pack=x")
        .is_err());
    assert!(!fx.pushed());
}

#[test]
fn the_remote_url_is_read_from_the_repository_and_a_missing_remote_is_not_found() {
    need_git!();
    let fx = Fx::new();
    let p = fx.publisher();
    let repo = fx.repo.display().to_string();
    assert_eq!(
        p.remote_url(&repo, "origin").unwrap(),
        fx.bare.display().to_string()
    );
    assert_eq!(
        p.remote_url(&repo, "nope").unwrap_err().code,
        ErrorCode::NotFound
    );
    git(
        &fx.repo,
        &[
            "config",
            "remote.origin.pushurl",
            "https://github.com/o/r.git",
        ],
    );
    assert_eq!(
        p.remote_url(&repo, "origin").unwrap(),
        "https://github.com/o/r.git",
        "git pushes to the push URL"
    );
    assert!(p.remote_url("/nonexistent/repo", "origin").is_err());
}

// ---- credentials ------------------------------------------------------------------------------------------------

fn https_remote(fx: &Fx) {
    git(
        &fx.repo,
        &["remote", "set-url", "origin", "https://github.com/o/r.git"],
    );
}

fn credential_flags(argv: &[String]) -> Vec<String> {
    argv.windows(2)
        .filter(|w| w[0] == "-c" && w[1].starts_with("credential.helper"))
        .map(|w| w[1].clone())
        .collect()
}

#[test]
fn with_no_credentials_git_gets_none_and_the_failure_says_how_to_set_them_up() {
    need_git!();
    let fx = Fx::new();
    https_remote(&fx);
    fx.t.write(
        "wrap/fail-push",
        "fatal: could not read Username for 'https://github.com': terminal prompts disabled\n",
    );
    let err = fx.publisher().push(&fx.request()).unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    assert!(
        err.message.contains("could not authenticate") && err.message.contains("push_credentials"),
        "{}",
        err.message
    );
    assert!(err.message.contains("Nothing was pushed"));
    let argv = fx.push_argv();
    // the static pin comes first and the call adds the reset; no helper is named
    assert_eq!(
        credential_flags(&argv),
        ["credential.helper=", "credential.helper="]
    );
    assert!(argv.contains(&"protocol.https.allow=always".to_string()));
    assert!(argv.contains(&"https://github.com/o/r.git".to_string()));
    assert!(!argv.contains(&"protocol.file.allow=always".to_string()));
    assert!(!fx.push_env().contains(SENTINEL));
}

#[test]
fn gh_can_be_the_credential_helper_of_this_one_push() {
    need_git!();
    let fx = Fx::new();
    https_remote(&fx);
    fx.t.write("wrap/fail-push", "fatal: unable to access 'https://github.com/o/r.git/': Could not resolve host: github.com\n");
    let mut cfg = fx.cfg.clone();
    cfg.push_credentials = PushCredentials::Gh;
    let err = fx
        .publisher_with(cfg.clone(), Some(PathBuf::from("/opt/homebrew/bin/gh")))
        .push(&fx.request())
        .unwrap_err();
    assert!(
        err.message.contains("could not reach the remote"),
        "{}",
        err.message
    );
    assert_eq!(
        credential_flags(&fx.push_argv()),
        [
            "credential.helper=",
            "credential.helper=",
            "credential.helper=!/opt/homebrew/bin/gh auth git-credential"
        ]
    );
    // without a gh the mode cannot work; a path git would have to quote is refused
    let err = fx
        .publisher_with(cfg.clone(), None)
        .push(&fx.request())
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    let err = fx
        .publisher_with(cfg, Some(PathBuf::from("/tmp/a b/gh;touch x")))
        .push(&fx.request())
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid);
}

#[test]
fn a_named_helper_program_is_the_only_helper_configured() {
    need_git!();
    let fx = Fx::new();
    https_remote(&fx);
    fx.t.write("wrap/fail-push", "remote: Permission denied\nfatal: Authentication failed for 'https://github.com/o/r.git/'\n");
    let mut cfg = fx.cfg.clone();
    cfg.push_credentials =
        PushCredentials::Helper("/usr/local/bin/git-credential-osxkeychain".into());
    let err = fx
        .publisher_with(cfg, None)
        .push(&fx.request())
        .unwrap_err();
    assert!(
        err.message.contains("could not authenticate"),
        "{}",
        err.message
    );
    assert_eq!(
        credential_flags(&fx.push_argv()),
        [
            "credential.helper=",
            "credential.helper=",
            "credential.helper=/usr/local/bin/git-credential-osxkeychain"
        ]
    );
}

#[test]
fn a_rejected_push_and_an_unknown_failure_are_reported_without_the_terminal_control_in_them() {
    need_git!();
    let fx = Fx::new();
    https_remote(&fx);
    fx.t.write(
        "wrap/fail-push",
        " ! [rejected]        a -> b (non-fast-forward)\nerror: failed to push some refs\n",
    );
    let err = fx.publisher().push(&fx.request()).unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    fx.t.write(
        "wrap/fail-push",
        "something \u{1b}[31mstrange\u{1b}[0m \u{202e}happened\n",
    );
    let err = fx.publisher().push(&fx.request()).unwrap_err();
    assert!(err.message.contains("git push failed"), "{}", err.message);
    assert!(
        !err.message.contains('\u{1b}') && !err.message.contains('\u{202e}'),
        "{:?}",
        err.message
    );
}
