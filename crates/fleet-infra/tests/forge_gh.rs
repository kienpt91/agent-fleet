//! The GitHub adapter against a fake `gh`: a script that replays fixtures and records how it was
//! started (arguments, environment, working directory). Nothing here reaches a network or a real
//! account; what a real `gh` prints today is the one thing these tests cannot prove.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_app::forge::ports::{
    CodeHost, ForgeAnswer, ForgeJobs, ForgeRequest, IssueTracker, ListQuery,
};
use fleet_domain::error::ErrorCode;
use fleet_domain::ids::Sha;
use fleet_domain::redact::Redactor;
use fleet_forge::config::ForgeConfig;
use fleet_forge::model::{CheckState, IssueState, PullState, ReviewState, StateFilter};
use fleet_forge::publish::PullRequestDraft;
use fleet_forge::rate::RateConfig;
use fleet_forge::repo::RepoRef;
use fleet_infra::forge_gh::{gh_env, resolve_gh, GhCli, GhSettings};
use fleet_infra::forge_jobs::{GhForgeJobs, UnavailableForge};
use fleet_infra::jobs::JobTracker;
use fleet_infra::testutil::{wait_until, TempDir};
use std::collections::BTreeMap;
use std::os::unix::fs::symlink;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A recognizable value that must appear nowhere it should not. It has no key shape: the point
/// is to find it, not to look like a credential.
const SENTINEL: &str = "fleet-test-sentinel-7f3a9c1d2e4b5a69";

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/gh")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn repo() -> RepoRef {
    RepoRef::new("github.com", "o", "r").unwrap()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        422 => "Unprocessable Entity",
        502 => "Bad Gateway",
        _ => "Status",
    }
}

/// What `gh api --include` prints: status line, headers, blank line, body.
fn http(status: u16, headers: &[(&str, &str)], body: &str) -> String {
    let mut s = format!("HTTP/2.0 {status} {}\r\n", reason(status));
    for (k, v) in headers {
        s.push_str(&format!("{k}: {v}\r\n"));
    }
    s.push_str("\r\n");
    s.push_str(body);
    s
}

fn key_of(endpoint: &str) -> String {
    endpoint
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                b as char
            } else {
                '_'
            }
        })
        .collect()
}

struct Sandbox {
    dir: TempDir,
    tracker: JobTracker,
}

impl Sandbox {
    fn new() -> Sandbox {
        let dir = TempDir::new("fake-gh");
        std::fs::create_dir_all(dir.join("fake")).unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let fake = dir.join("fake");
        let script = format!(
            r#"#!/bin/sh
DIR='{}'
n=$(cat "$DIR/count" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$DIR/count"
{{ printf 'CALL %s\n' "$n"; for a in "$@"; do printf 'ARG %s\n' "$a"; done; }} >> "$DIR/argv.log"
pwd > "$DIR/cwd.$n"
env | sort > "$DIR/env.$n"
echo $$ > "$DIR/pid.$n"
for last; do :; done
key=$(printf '%s' "$last" | tr -c 'A-Za-z0-9' '_')
r="$DIR/ep.$key"
[ -e "$r.out" ] || [ -e "$r.exit" ] || r="$DIR/call.$n"
[ -e "$r.out" ] || [ -e "$r.exit" ] || r="$DIR/default"
[ -e "$r.sleep" ] && sleep "$(cat "$r.sleep")"
[ -e "$r.err" ] && cat "$r.err" >&2
if [ -e "$r.huge" ]; then head -c 4194304 /dev/zero | tr '\0' 'a'; elif [ -e "$r.out" ]; then cat "$r.out"; fi
exit "$(cat "$r.exit" 2>/dev/null || echo 0)"
"#,
            fake.display()
        );
        dir.script("bin/gh", &script);
        Sandbox {
            dir,
            tracker: JobTracker::new(),
        }
    }

    fn gh_path(&self) -> std::path::PathBuf {
        self.dir.join("bin/gh")
    }

    fn put(&self, name: &str, text: &str) {
        std::fs::write(self.dir.join(&format!("fake/{name}")), text).unwrap();
    }

    /// The answer to the call whose last argument is `endpoint`.
    fn ep(&self, endpoint: &str, out: &str) {
        let k = key_of(endpoint);
        for stale in ["exit", "err"] {
            let _ = std::fs::remove_file(self.dir.join(&format!("fake/ep.{k}.{stale}")));
        }
        self.put(&format!("ep.{k}.out"), out);
    }

    fn ep_with(&self, endpoint: &str, out: &str, exit: i32, stderr: &str) {
        let k = key_of(endpoint);
        self.put(&format!("ep.{k}.out"), out);
        self.put(&format!("ep.{k}.exit"), &exit.to_string());
        if !stderr.is_empty() {
            self.put(&format!("ep.{k}.err"), stderr);
        }
    }

    fn calls(&self) -> usize {
        self.dir.read("fake/count").trim().parse().unwrap_or(0)
    }

    /// Every call's arguments, in order.
    fn argv(&self) -> Vec<Vec<String>> {
        let mut calls: Vec<Vec<String>> = Vec::new();
        for line in self.dir.read("fake/argv.log").split_inclusive('\n') {
            let line = line.strip_suffix('\n').unwrap_or(line);
            if line.starts_with("CALL ") {
                calls.push(Vec::new());
            } else if let Some(a) = line.strip_prefix("ARG ") {
                calls.last_mut().unwrap().push(a.to_string());
            } else if let Some(last) = calls.last_mut().and_then(|c| c.last_mut()) {
                // a line break inside an argument
                last.push('\n');
                last.push_str(line);
            }
        }
        calls
    }

    fn env_of(&self, n: usize) -> BTreeMap<String, String> {
        self.dir
            .read(&format!("fake/env.{n}"))
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn parent_env() -> Vec<(String, String)> {
        vec![
            ("HOME".into(), "/home/tester".into()),
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("GH_TOKEN".into(), SENTINEL.into()),
            ("GITHUB_TOKEN".into(), SENTINEL.into()),
            ("GH_ENTERPRISE_TOKEN".into(), SENTINEL.into()),
            ("GITHUB_ENTERPRISE_TOKEN".into(), SENTINEL.into()),
            ("GH_HOST".into(), "evil.example".into()),
            ("GH_REPO".into(), "evil/repo".into()),
            ("GH_CONFIG_DIR".into(), "/tmp/evil".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), SENTINEL.into()),
            ("SSH_AUTH_SOCK".into(), "/tmp/agent.sock".into()),
            ("FLEET_CONTROL_TOKEN".into(), SENTINEL.into()),
            (
                "DBUS_SESSION_BUS_ADDRESS".into(),
                "unix:path=/run/bus".into(),
            ),
        ]
    }

    fn settings(&self, cfg: &ForgeConfig) -> GhSettings {
        let parent = Sandbox::parent_env();
        let mut redactor = Redactor::builtin();
        redactor.add_env_values(parent.clone());
        GhSettings {
            program: self.gh_path(),
            host: cfg.host.clone(),
            env: gh_env(cfg, &parent).unwrap(),
            timeout_ms: cfg.timeout_ms,
            redactor: Arc::new(redactor),
            tracker: self.tracker.clone(),
            rate: cfg.rate,
        }
    }

    fn cli(&self) -> GhCli {
        self.cli_with(ForgeConfig::disabled())
    }

    fn cli_with(&self, cfg: ForgeConfig) -> GhCli {
        GhCli::new(self.settings(&cfg))
    }
}

fn q(state: StateFilter, page: u64, per_page: usize) -> ListQuery {
    ListQuery {
        state,
        page,
        per_page,
    }
}

// ---- how gh is started ---------------------------------------------------------------------------

#[test]
fn gh_gets_a_minimal_environment_and_never_a_token() {
    let sb = Sandbox::new();
    sb.ep("repos/o/r", &http(200, &[], &fixture("repo.json")));
    sb.cli().check_access(&repo()).unwrap();
    let env = sb.env_of(1);
    let shell_noise = ["PWD", "OLDPWD", "SHLVL", "_"];
    let names: Vec<&str> = env
        .keys()
        .map(String::as_str)
        .filter(|k| !shell_noise.contains(k))
        .collect();
    assert_eq!(
        names,
        [
            "GH_NO_UPDATE_NOTIFIER",
            "GH_PROMPT_DISABLED",
            "HOME",
            "PATH"
        ],
        "{env:?}"
    );
    assert_eq!(env["HOME"], "/home/tester");
    assert_eq!(env["PATH"], "/usr/bin:/bin");
    let dump = sb.dir.read("fake/env.1");
    assert!(
        !dump.contains(SENTINEL),
        "the token value reached the child: {dump}"
    );
    assert!(!dump.contains("evil"), "{dump}");
}

#[test]
fn the_enterprise_host_and_the_named_variables_are_the_only_additions() {
    let sb = Sandbox::new();
    sb.ep("repos/o/r", &http(200, &[], &fixture("repo.json")));
    let mut cfg = ForgeConfig::disabled();
    cfg.host = "ghe.example.org".into();
    cfg.allowed_hosts = vec!["ghe.example.org".into()];
    cfg.env_pass = vec!["DBUS_SESSION_BUS_ADDRESS".into(), "NOT_SET_ANYWHERE".into()];
    let cli = sb.cli_with(cfg);
    cli.check_access(&RepoRef::new("ghe.example.org", "o", "r").unwrap())
        .unwrap();
    let env = sb.env_of(1);
    assert_eq!(env["GH_HOST"], "ghe.example.org");
    assert_eq!(env["DBUS_SESSION_BUS_ADDRESS"], "unix:path=/run/bus");
    assert!(!env.contains_key("NOT_SET_ANYWHERE"));
    assert!(!sb.dir.read("fake/env.1").contains(SENTINEL));
    let argv = &sb.argv()[0];
    assert_eq!(argv[1..3], ["--hostname", "ghe.example.org"]);
}

#[test]
fn a_secret_named_variable_is_never_copied_even_when_asked() {
    let mut cfg = ForgeConfig::disabled();
    cfg.env_pass = vec!["GH_TOKEN".into()];
    assert!(cfg.validate().is_err());
    // and `gh_env` itself skips the scrubbed names if a configuration got past validation
    let env = gh_env(&cfg, &Sandbox::parent_env()).unwrap();
    assert!(env.iter().all(|(k, _)| k != "GH_TOKEN"), "{env:?}");
    assert!(env.iter().all(|(_, v)| v != SENTINEL), "{env:?}");
}

#[test]
fn without_a_home_gh_cannot_run_and_relative_path_entries_are_dropped() {
    let cfg = ForgeConfig::disabled();
    let err = gh_env(&cfg, &[("PATH".into(), "/usr/bin".into())]).unwrap_err();
    assert!(err.message.contains("HOME"), "{}", err.message);
    let err = gh_env(&cfg, &[("HOME".into(), "relative".into())]).unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    let env = gh_env(
        &cfg,
        &[
            ("HOME".into(), "/home/x".into()),
            ("PATH".into(), "bin:/usr/bin::.:/bin".into()),
        ],
    )
    .unwrap();
    let path = &env.iter().find(|(k, _)| k == "PATH").unwrap().1;
    assert_eq!(path, "/usr/bin:/bin");
}

#[test]
fn the_arguments_are_fixed_and_the_endpoint_comes_last() {
    let sb = Sandbox::new();
    let cli = sb.cli();
    sb.ep(
        "repos/o/r/issues?state=open&per_page=20&page=1&sort=updated&direction=desc",
        &http(200, &[], &fixture("issues_list.json")),
    );
    sb.ep(
        "repos/o/r/issues/7",
        &http(200, &[], &fixture("issue_7.json")),
    );
    cli.list_issues(&repo(), &q(StateFilter::Open, 1, 20))
        .unwrap();
    cli.get_issue(&repo(), 7).unwrap();
    let calls = sb.argv();
    let fixed = [
        "api",
        "--hostname",
        "github.com",
        "--include",
        "--method",
        "GET",
        "-H",
        "Accept: application/vnd.github+json",
        "-H",
        "X-GitHub-Api-Version: 2022-11-28",
    ];
    assert_eq!(calls[0][..10], fixed);
    assert_eq!(
        calls[0][10],
        "repos/o/r/issues?state=open&per_page=20&page=1&sort=updated&direction=desc"
    );
    assert_eq!(calls[0].len(), 11);
    assert_eq!(calls[1][..10], fixed);
    assert_eq!(calls[1][10], "repos/o/r/issues/7");
    assert_eq!(calls[1].len(), 11);
}

#[test]
fn gh_runs_in_the_root_directory_where_it_finds_no_repository() {
    let sb = Sandbox::new();
    sb.ep("repos/o/r", &http(200, &[], &fixture("repo.json")));
    sb.cli().check_access(&repo()).unwrap();
    assert_eq!(sb.dir.read("fake/cwd.1").trim(), "/");
}

// ---- reading ---------------------------------------------------------------------------------------

#[test]
fn issues_are_read_from_the_fixture_and_pull_requests_in_the_list_are_left_out() {
    let sb = Sandbox::new();
    sb.ep(
        "repos/o/r/issues?state=all&per_page=3&page=2&sort=updated&direction=desc",
        &http(
            200,
            &[("Link", "<https://api.github.com/x?page=3>; rel=\"next\", <https://api.github.com/x?page=5>; rel=\"last\"")],
            &fixture("issues_list.json"),
        ),
    );
    let page = sb
        .cli()
        .list_issues(&repo(), &q(StateFilter::All, 2, 3))
        .unwrap();
    assert_eq!(page.page, 2);
    assert!(page.has_more);
    assert_eq!(page.items.len(), 2, "the pull request is not an issue");
    let first = &page.items[0];
    assert_eq!(
        (first.number, first.title.as_str(), first.comments),
        (7, "Crash on start", 3)
    );
    assert_eq!(first.labels, ["bug", "p1"]);
    assert_eq!(first.state, IssueState::Open);
    assert_eq!(page.items[1].author, "ghost");
    assert_eq!(page.items[1].state, IssueState::Closed);
}

#[test]
fn an_issue_and_a_pull_request_are_read() {
    let sb = Sandbox::new();
    let cli = sb.cli();
    sb.ep(
        "repos/o/r/issues/7",
        &http(200, &[], &fixture("issue_7.json")),
    );
    sb.ep(
        "repos/o/r/pulls/9",
        &http(200, &[], &fixture("pull_9.json")),
    );
    sb.ep(
        "repos/o/r/pulls?state=open&per_page=20&page=1&sort=updated&direction=desc",
        &http(200, &[], &fixture("pulls_list.json")),
    );
    let issue = cli.get_issue(&repo(), 7).unwrap();
    assert_eq!(issue.body, "It crashes.\n\nSteps:\n1. run it");
    let pull = cli.get_pull(&repo(), 9).unwrap();
    assert_eq!(pull.summary.state, PullState::Open);
    assert!(pull.summary.draft);
    assert_eq!(pull.summary.comments, 3);
    assert_eq!(pull.summary.head_sha.as_str(), "a".repeat(40));
    let page = cli
        .list_pulls(&repo(), &q(StateFilter::Open, 1, 20))
        .unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.items[1].state, PullState::Merged);
    assert!(!page.has_more);
}

#[test]
fn a_pull_request_status_is_four_reads_and_adds_up() {
    let sb = Sandbox::new();
    sb.ep(
        "repos/o/r/pulls/9",
        &http(200, &[], &fixture("pull_9.json")),
    );
    let sha = "a".repeat(40);
    sb.ep(
        &format!("repos/o/r/commits/{sha}/check-runs?per_page=100"),
        &http(200, &[], &fixture("check_runs.json")),
    );
    sb.ep(
        &format!("repos/o/r/commits/{sha}/status"),
        &http(200, &[], &fixture("commit_status.json")),
    );
    sb.ep(
        "repos/o/r/pulls/9/reviews?per_page=100",
        &http(200, &[], &fixture("reviews.json")),
    );
    let st = sb.cli().pull_status(&repo(), 9).unwrap();
    assert_eq!(sb.calls(), 4);
    assert_eq!(st.checks.state, CheckState::Failing);
    assert_eq!(
        (st.checks.passed, st.checks.failed, st.checks.pending),
        (1, 1, 1)
    );
    let reviews: Vec<(String, ReviewState)> = st
        .reviews
        .iter()
        .map(|r| (r.author.clone(), r.state))
        .collect();
    assert_eq!(
        reviews,
        [
            ("ann".to_string(), ReviewState::ChangesRequested),
            ("bob".to_string(), ReviewState::Approved)
        ]
    );
}

#[test]
fn a_pull_request_is_created_with_the_approved_text_sent_literally() {
    let sb = Sandbox::new();
    sb.ep(
        "repos/o/r/pulls",
        &http(201, &[], &fixture("pull_created.json")),
    );
    let draft = PullRequestDraft {
        repo: repo(),
        base: "main".into(),
        head_branch: "fleet/r1/a".into(),
        head_sha: Sha::parse(&"a".repeat(40)).unwrap(),
        title: "Add `the` $(thing); rm -rf /".into(),
        body: "Line one\n@file.txt \"quoted\" -f evil=1\n"
            .trim_end()
            .into(),
        draft: true,
    };
    let created = sb.cli().create_pull(&draft).unwrap();
    assert_eq!(
        (created.number, created.url.as_str()),
        (12, "https://github.com/o/r/pull/12")
    );
    let argv = &sb.argv()[0];
    assert_eq!(argv[5], "POST");
    let tail = &argv[10..];
    assert_eq!(
        tail,
        [
            "-f",
            "title=Add `the` $(thing); rm -rf /",
            "-f",
            "body=Line one\n@file.txt \"quoted\" -f evil=1",
            "-f",
            "head=fleet/r1/a",
            "-f",
            "base=main",
            "-F",
            "draft=true",
            "repos/o/r/pulls",
        ]
        .map(String::from)
    );
}

#[test]
fn a_draft_that_is_not_valid_is_refused_before_gh_starts() {
    let sb = Sandbox::new();
    let draft = PullRequestDraft {
        repo: repo(),
        base: "main".into(),
        head_branch: "feature/x".into(),
        head_sha: Sha::parse(&"a".repeat(40)).unwrap(),
        title: "t".into(),
        body: String::new(),
        draft: false,
    };
    assert!(sb.cli().create_pull(&draft).is_err());
    assert_eq!(sb.calls(), 0);
}

// ---- hostile answers ----------------------------------------------------------------------------------

#[test]
fn hostile_issue_text_is_cleaned_and_secret_shapes_are_redacted_before_anything_sees_it() {
    let sb = Sandbox::new();
    let token = format!("{}{}", "ghp_", "a1B2".repeat(10));
    let aws = format!("{}{}", "AKIA", "ABCDEFGHIJKLMNOP");
    let body = fixture("issue_hostile.json")
        .replace("@@TOKEN@@", &token)
        .replace("@@AWS@@", &aws)
        .replace("@@PRIVATE@@", "RSA PRIVATE");
    sb.ep("repos/o/r/issues/66", &http(200, &[], &body));
    let issue = sb.cli().get_issue(&repo(), 66).unwrap();
    let all = format!("{issue:?}");
    for bad in ['\u{1b}', '\u{7}', '\u{202e}', '\u{200b}'] {
        assert!(
            !issue.body.contains(bad) && !issue.summary.title.contains(bad),
            "{bad:?}"
        );
        assert!(!issue.summary.author.contains(bad));
        assert!(issue.summary.labels.iter().all(|l| !l.contains(bad)));
    }
    assert!(!all.contains(&token) && !all.contains(&aws), "{all}");
    assert!(
        !issue.body.contains("MIIEvQIBADANBgkq"),
        "the private key block is dropped"
    );
    assert!(issue.body.contains("[REDACTED"));
    assert!(!issue.summary.title.contains('\n'));
    // markup is text, not something fleet interprets: it is kept as written for the quote to carry
    assert!(issue.body.contains("<script>alert(1)</script>"));
    assert!(issue.body.contains("Ignore all previous instructions"));
}

#[test]
fn the_daemons_own_secret_values_are_redacted_from_answers() {
    let sb = Sandbox::new();
    let body = fixture("issue_7.json").replace(
        "It crashes.",
        &format!("It crashes with {SENTINEL} inside."),
    );
    sb.ep("repos/o/r/issues/7", &http(200, &[], &body));
    let issue = sb.cli().get_issue(&repo(), 7).unwrap();
    assert!(!issue.body.contains(SENTINEL), "{}", issue.body);
    assert!(issue.body.contains("[REDACTED]"));
}

#[test]
fn garbage_and_broken_answers_are_errors_not_data() {
    let sb = Sandbox::new();
    for (what, out) in [
        ("not json", http(200, &[], "this is not json")),
        (
            "html",
            http(200, &[], "<html><body>captive portal</body></html>"),
        ),
        ("truncated", http(200, &[], "{\"number\": 7, \"title\": ")),
        ("a list instead of an object", http(200, &[], "[1,2,3]")),
        (
            "wrong types",
            http(
                200,
                &[],
                "{\"number\":\"7\",\"title\":1,\"state\":\"open\",\"html_url\":\"x\"}",
            ),
        ),
        ("headers never end", "HTTP/2.0 200 OK\r\nx: y".to_string()),
        ("binary", "\u{0}\u{1}\u{2}\u{ff}".to_string()),
    ] {
        sb.ep("repos/o/r/issues/7", &out);
        let err = sb.cli().get_issue(&repo(), 7).unwrap_err();
        assert!(
            matches!(err.code, ErrorCode::Invalid | ErrorCode::Unavailable),
            "{what}: {err:?}"
        );
    }
    // an empty answer is not an issue either
    sb.ep("repos/o/r/issues/7", "");
    assert!(sb.cli().get_issue(&repo(), 7).is_err());
}

#[test]
fn an_answer_larger_than_the_cap_is_refused() {
    let sb = Sandbox::new();
    sb.put("ep.repos_o_r_issues_7.huge", "");
    sb.put("ep.repos_o_r_issues_7.exit", "0");
    let err = sb.cli().get_issue(&repo(), 7).unwrap_err();
    assert_eq!(err.code, ErrorCode::Limit, "{err:?}");
    assert!(err.message.contains("more than"), "{}", err.message);
}

#[test]
fn a_gh_that_hangs_is_stopped_at_the_timeout_and_leaves_nothing_running() {
    let sb = Sandbox::new();
    sb.put("default.sleep", "30");
    sb.put("default.out", "");
    let mut cfg = ForgeConfig::disabled();
    cfg.timeout_ms = 1_000;
    let cli = sb.cli_with(cfg);
    let started = std::time::Instant::now();
    let err = cli.get_issue(&repo(), 7).unwrap_err();
    assert_eq!(err.code, ErrorCode::Timeout, "{err:?}");
    assert!(err.message.contains("Nothing was changed"));
    assert!(started.elapsed().as_secs() < 10);
    let pid: i32 = sb.dir.read("fake/pid.1").trim().parse().unwrap();
    let gone = wait_until(5_000, || {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err()
    });
    assert!(gone, "gh (pid {pid}) is still running");
    assert!(
        sb.tracker.live().is_empty(),
        "the group is no longer listed"
    );
}

#[test]
fn a_gh_that_cannot_be_started_is_unavailable() {
    let sb = Sandbox::new();
    let mut s = sb.settings(&ForgeConfig::disabled());
    s.program = sb.dir.join("bin/nope");
    let err = GhCli::new(s).get_issue(&repo(), 7).unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable, "{err:?}");
    assert!(err.message.contains("could not be started"));
}

#[test]
fn gh_and_its_children_are_listed_in_the_tracker_while_they_run() {
    let sb = Sandbox::new();
    sb.put("default.sleep", "3");
    sb.put("default.out", &http(200, &[], &fixture("issue_7.json")));
    let cli = Arc::new(sb.cli());
    let worker = {
        let cli = Arc::clone(&cli);
        std::thread::spawn(move || cli.get_issue(&repo(), 7))
    };
    assert!(
        wait_until(5_000, || !sb.tracker.live().is_empty()),
        "never listed"
    );
    let live = sb.tracker.live();
    assert_eq!(live[0].pid, live[0].pgid, "gh leads its own process group");
    assert!(worker.join().unwrap().is_ok());
    assert!(
        wait_until(5_000, || sb.tracker.live().is_empty()),
        "still listed"
    );
}

// ---- failures ----------------------------------------------------------------------------------------------

#[test]
fn not_signed_in_offline_missing_and_refused_are_told_apart_and_say_nothing_changed() {
    let sb = Sandbox::new();
    let cli = sb.cli();
    let ep = "repos/o/r/issues/7";
    // gh refuses to run without a sign-in
    sb.ep_with(
        ep,
        "",
        4,
        "To get started with GitHub CLI, please run:  gh auth login\n",
    );
    let err = cli.get_issue(&repo(), 7).unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    assert!(
        err.message.contains("not signed in")
            && err.message.contains("never sees or stores a token"),
        "{}",
        err.message
    );
    // a token GitHub does not accept
    sb.ep_with(
        ep,
        &http(401, &[], "{\"message\":\"Bad credentials\"}"),
        1,
        "gh: Bad credentials (HTTP 401)\n",
    );
    assert!(cli
        .get_issue(&repo(), 7)
        .unwrap_err()
        .message
        .contains("not signed in"));
    // no network
    sb.ep_with(
        ep,
        "",
        1,
        "error connecting to api.github.com\ncheck your internet connection\n",
    );
    let err = cli.get_issue(&repo(), 7).unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    assert!(
        err.message.contains("network") && err.message.contains("Nothing was changed"),
        "{}",
        err.message
    );
    // missing
    sb.ep_with(
        ep,
        &http(404, &[], &fixture("not_found.json")),
        1,
        "gh: Not Found (HTTP 404)\n",
    );
    assert_eq!(
        cli.get_issue(&repo(), 7).unwrap_err().code,
        ErrorCode::NotFound
    );
    // refused for this account
    sb.ep_with(
        ep,
        &http(
            403,
            &[],
            "{\"message\":\"Resource not accessible by integration\"}",
        ),
        1,
        "gh: Resource not accessible (HTTP 403)\n",
    );
    assert_eq!(
        cli.get_issue(&repo(), 7).unwrap_err().code,
        ErrorCode::Forbidden
    );
    // a server error
    sb.ep_with(ep, &http(502, &[], ""), 1, "gh: Bad Gateway (HTTP 502)\n");
    assert!(cli
        .get_issue(&repo(), 7)
        .unwrap_err()
        .message
        .contains("server error"));
    // the rest of the process was never affected: gh was started once per call and not again
    assert_eq!(sb.calls(), 6);
}

#[test]
fn a_validation_error_shows_the_cleaned_message_of_github() {
    let sb = Sandbox::new();
    sb.ep_with(
        "repos/o/r/pulls",
        &http(422, &[], &fixture("validation_failed.json")),
        1,
        "gh: Validation Failed (HTTP 422)\n",
    );
    let draft = PullRequestDraft {
        repo: repo(),
        base: "main".into(),
        head_branch: "fleet/r1/a".into(),
        head_sha: Sha::parse(&"a".repeat(40)).unwrap(),
        title: "t".into(),
        body: String::new(),
        draft: false,
    };
    let err = sb.cli().create_pull(&draft).unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid);
    assert!(err.message.contains("Validation Failed"), "{}", err.message);
    assert!(
        !err.message.contains('\u{1b}') && !err.message.contains('\n'),
        "{:?}",
        err.message
    );
}

fn limited_answer() -> String {
    http(
        403,
        &[
            ("x-ratelimit-limit", "5000"),
            ("x-ratelimit-remaining", "0"),
            ("x-ratelimit-reset", "1000000120"),
        ],
        &fixture("rate_limited.json"),
    )
}

#[test]
fn a_rate_limit_blocks_further_calls_without_starting_gh_and_comes_back_after_the_wait() {
    let sb = Sandbox::new();
    let clock = Arc::new(AtomicU64::new(1_000_000_000_000));
    let c = Arc::clone(&clock);
    let cli = sb
        .cli()
        .with_clock(move || c.load(Ordering::SeqCst))
        .with_jitter(|| 0);
    sb.ep_with(
        "repos/o/r",
        &limited_answer(),
        1,
        "gh: API rate limit exceeded for user ID 1. (HTTP 403)\n",
    );
    let err = cli.check_access(&repo()).unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    assert!(
        err.message.contains("slow down") && err.message.contains("120 s"),
        "{}",
        err.message
    );
    assert_eq!(sb.calls(), 1);
    // asking again does not start gh and does not spin: the answer is immediate and says when
    for _ in 0..20 {
        let again = cli.check_access(&repo()).unwrap_err();
        assert!(
            again.message.contains("no request is made"),
            "{}",
            again.message
        );
    }
    assert_eq!(sb.calls(), 1, "gh was not started while blocked");
    let st = cli.rate_status();
    assert_eq!(st.state, "limited");
    assert_eq!(st.until_ms, Some(1_000_000_120_000));
    // a pre-check used by the runner is also a refusal, not a call
    assert!(cli.check_rate().is_err());
    // after the reset the next call goes through
    clock.fetch_add(121_000, Ordering::SeqCst);
    sb.ep(
        "repos/o/r",
        &http(
            200,
            &[
                ("x-ratelimit-remaining", "4990"),
                ("x-ratelimit-reset", "1000003600"),
            ],
            &fixture("repo.json"),
        ),
    );
    cli.check_access(&repo()).unwrap();
    assert_eq!(sb.calls(), 2);
    assert_eq!(cli.rate_status().state, "ok");
}

#[test]
fn a_secondary_limit_with_a_retry_after_header_is_honoured_with_jitter() {
    let sb = Sandbox::new();
    let clock = Arc::new(AtomicU64::new(5_000_000));
    let c = Arc::clone(&clock);
    let cli = sb
        .cli()
        .with_clock(move || c.load(Ordering::SeqCst))
        .with_jitter(|| 1000);
    sb.ep_with(
        "repos/o/r",
        &http(
            403,
            &[("retry-after", "60")],
            "{\"message\":\"You have exceeded a secondary rate limit.\"}",
        ),
        1,
        "gh: You have exceeded a secondary rate limit. (HTTP 403)\n",
    );
    cli.check_access(&repo()).unwrap_err();
    let until = cli.rate_status().until_ms.unwrap();
    assert_eq!(
        until,
        5_000_000 + 60_000 + 15_000,
        "the wait the forge asked for plus a quarter of jitter"
    );
}

#[test]
fn the_limit_is_also_recognised_from_what_gh_prints_when_no_headers_came() {
    let sb = Sandbox::new();
    let cli = sb.cli().with_jitter(|| 0);
    sb.ep_with(
        "repos/o/r",
        "",
        1,
        "gh: API rate limit exceeded for user ID 1. (HTTP 403)\n",
    );
    let err = cli.check_access(&repo()).unwrap_err();
    assert!(err.message.contains("slow down"), "{}", err.message);
    assert_eq!(cli.rate_status().state, "limited");
}

#[test]
fn the_own_budget_stops_calls_before_gh_is_started() {
    let sb = Sandbox::new();
    let mut cfg = ForgeConfig::disabled();
    cfg.rate = RateConfig {
        budget_per_hour: 2,
        ..RateConfig::default()
    };
    let clock = Arc::new(AtomicU64::new(1_000));
    let c = Arc::clone(&clock);
    let cli = sb
        .cli_with(cfg)
        .with_clock(move || c.load(Ordering::SeqCst));
    sb.ep("repos/o/r", &http(200, &[], &fixture("repo.json")));
    cli.check_access(&repo()).unwrap();
    cli.check_access(&repo()).unwrap();
    let err = cli.check_access(&repo()).unwrap_err();
    assert!(err.message.contains("budget"), "{}", err.message);
    assert_eq!(sb.calls(), 2);
    assert_eq!(cli.rate_status().state, "budget");
    clock.fetch_add(3_600_001, Ordering::SeqCst);
    cli.check_access(&repo()).unwrap();
    assert_eq!(sb.calls(), 3);
}

// ---- which gh -----------------------------------------------------------------------------------------------------

fn env_with_path(path: &str) -> Vec<(String, String)> {
    vec![
        ("HOME".into(), "/home/x".into()),
        ("PATH".into(), path.into()),
    ]
}

#[test]
fn gh_is_found_once_in_an_absolute_path_entry_and_relative_entries_are_never_used() {
    let sb = Sandbox::new();
    let cfg = ForgeConfig::disabled();
    let found = resolve_gh(
        &cfg,
        &env_with_path(&format!("{}/bin:/usr/bin", sb.dir.path().display())),
    )
    .unwrap();
    assert_eq!(found.program, std::fs::canonicalize(sb.gh_path()).unwrap());
    assert_eq!(found.skipped_relative, 0);
    // a gh that is reachable only through a relative entry is not found, and the entry is counted
    let rel = TempDir::new("rel-gh");
    rel.script("gh", "#!/bin/sh\nexit 0\n");
    let old = std::env::current_dir().unwrap();
    let _ = old; // the test never changes directory: the relative entry would resolve there
    let err = resolve_gh(&cfg, &env_with_path("bin:.:/nonexistent")).unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    assert!(
        err.message.contains("absolute entry of PATH"),
        "{}",
        err.message
    );
    let skipped = resolve_gh(
        &cfg,
        &env_with_path(&format!("bin::{}/bin", sb.dir.path().display())),
    )
    .unwrap();
    assert_eq!(skipped.skipped_relative, 2);
}

#[test]
fn gh_is_resolved_through_symlinks_and_must_be_an_executable_file() {
    let sb = Sandbox::new();
    let other = TempDir::new("links");
    symlink(sb.gh_path(), other.join("gh")).unwrap();
    let found = resolve_gh(
        &ForgeConfig::disabled(),
        &env_with_path(&other.path().display().to_string()),
    )
    .unwrap();
    assert_eq!(
        found.program,
        std::fs::canonicalize(sb.gh_path()).unwrap(),
        "what is checked later is what runs"
    );
    // not executable
    let plain = TempDir::new("plain");
    plain.write("gh", "#!/bin/sh\n");
    let err = resolve_gh(
        &ForgeConfig::disabled(),
        &env_with_path(&plain.path().display().to_string()),
    )
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    // a directory called gh
    let dirs = TempDir::new("dirs");
    std::fs::create_dir_all(dirs.join("gh")).unwrap();
    assert!(resolve_gh(
        &ForgeConfig::disabled(),
        &env_with_path(&dirs.path().display().to_string())
    )
    .is_err());
}

#[test]
fn a_configured_gh_must_be_absolute_and_is_used_instead_of_a_search() {
    let sb = Sandbox::new();
    let mut cfg = ForgeConfig::disabled();
    cfg.gh = Some("gh".into());
    assert!(resolve_gh(&cfg, &env_with_path("/usr/bin")).is_err());
    cfg.gh = Some(sb.gh_path().display().to_string());
    let found = resolve_gh(&cfg, &env_with_path("/nonexistent")).unwrap();
    assert_eq!(found.program, std::fs::canonicalize(sb.gh_path()).unwrap());
    cfg.gh = Some(sb.dir.join("bin/missing").display().to_string());
    assert!(resolve_gh(&cfg, &env_with_path("/usr/bin")).is_err());
}

#[test]
fn a_gh_inside_a_repository_or_a_worktree_is_not_trusted() {
    let repo_dir = TempDir::new("repo-with-gh");
    let planted = repo_dir.script("tools/gh", "#!/bin/sh\nexit 0\n");
    let repo_path = repo_dir.path().display().to_string();
    let sb = Sandbox::new();
    let mut s = sb.settings(&ForgeConfig::disabled());
    s.program = planted.clone();
    let cli = GhCli::new(s);
    let err = cli
        .check_trusted(std::slice::from_ref(&repo_path))
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Forbidden);
    assert!(err.message.contains("not trusted"), "{}", err.message);
    // a program elsewhere is fine, and a list of repositories is checked entirely
    assert!(sb
        .cli()
        .check_trusted(&[repo_path.clone(), "/nonexistent".into()])
        .is_ok());
    // through a symlink from outside the repository
    let outside = TempDir::new("outside");
    symlink(&planted, outside.join("gh")).unwrap();
    let found = resolve_gh(
        &ForgeConfig::disabled(),
        &env_with_path(&outside.path().display().to_string()),
    )
    .unwrap();
    let mut s = sb.settings(&ForgeConfig::disabled());
    s.program = found.program;
    assert!(GhCli::new(s).check_trusted(&[repo_path]).is_err());
}

// ---- the runner ----------------------------------------------------------------------------------------------------

fn jobs_for(sb: &Sandbox) -> GhForgeJobs {
    let cli = Arc::new(sb.cli());
    let dir = TempDir::new("pub");
    let cfg = ForgeConfig::disabled();
    let publisher = Arc::new(fleet_infra::git::GitPublisher::new(
        Sandbox::parent_env(),
        cfg,
        None,
        sb.tracker.clone(),
    ));
    let _ = dir;
    GhForgeJobs::new(cli, publisher, String::new())
}

fn wait_for(
    jobs: &mut GhForgeJobs,
    n: usize,
) -> Vec<(u64, fleet_domain::error::Result<ForgeAnswer>)> {
    let mut got = Vec::new();
    assert!(
        wait_until(15_000, || {
            got.extend(jobs.poll());
            got.len() >= n
        }),
        "only {} of {n} requests finished",
        got.len()
    );
    got
}

#[test]
fn requests_run_on_their_own_threads_and_come_back_by_ticket() {
    let sb = Sandbox::new();
    sb.ep(
        "repos/o/r/issues/7",
        &http(200, &[], &fixture("issue_7.json")),
    );
    let mut jobs = jobs_for(&sb);
    let st = jobs.status(&[]);
    assert!(st.ready, "{}", st.detail);
    assert!(st.detail.contains("gh at "));
    jobs.start(
        5,
        ForgeRequest::ShowIssue {
            repo: repo(),
            number: 7,
        },
        &[],
    )
    .unwrap();
    jobs.start(
        6,
        ForgeRequest::ShowIssue {
            repo: repo(),
            number: 404,
        },
        &[],
    )
    .unwrap();
    let mut got = wait_for(&mut jobs, 2);
    got.sort_by_key(|(t, _)| *t);
    assert!(matches!(&got[0].1, Ok(ForgeAnswer::Issue(i)) if i.summary.number == 7));
    assert!(
        got[1].1.is_err(),
        "the unknown issue fails inside its own ticket"
    );
    assert_eq!(jobs.running(), 0);
}

#[test]
fn the_runner_refuses_work_while_the_rate_gate_is_closed_and_when_gh_is_untrusted() {
    let sb = Sandbox::new();
    sb.ep_with(
        "repos/o/r",
        &limited_answer(),
        1,
        "gh: API rate limit exceeded for user ID 1. (HTTP 403)\n",
    );
    let mut jobs = jobs_for(&sb);
    jobs.start(1, ForgeRequest::Preflight { repo: repo() }, &[])
        .unwrap();
    let got = wait_for(&mut jobs, 1);
    assert!(got[0].1.is_err());
    let calls = sb.calls();
    let err = jobs
        .start(2, ForgeRequest::Preflight { repo: repo() }, &[])
        .unwrap_err();
    assert!(
        err.message.contains("no request is made"),
        "{}",
        err.message
    );
    assert_eq!(sb.calls(), calls, "no thread, no process");
    assert_eq!(
        jobs.status(&[]).rate.get("state").and_then(|v| v.as_str()),
        Some("limited")
    );
    // an untrusted gh is refused before anything
    let repo_dir = TempDir::new("untrusted");
    let planted = repo_dir.script("gh", "#!/bin/sh\nexit 0\n");
    let mut s = sb.settings(&ForgeConfig::disabled());
    s.program = planted;
    let mut jobs = GhForgeJobs::new(
        Arc::new(GhCli::new(s)),
        Arc::new(fleet_infra::git::GitPublisher::new(
            Sandbox::parent_env(),
            ForgeConfig::disabled(),
            None,
            JobTracker::new(),
        )),
        String::new(),
    );
    let repos = [repo_dir.path().display().to_string()];
    let st = jobs.status(&repos);
    assert!(
        !st.ready && st.detail.contains("not trusted"),
        "{}",
        st.detail
    );
    assert!(jobs
        .start(1, ForgeRequest::Preflight { repo: repo() }, &repos)
        .is_err());
}

#[test]
fn at_most_four_requests_run_at_once() {
    let sb = Sandbox::new();
    sb.put("default.sleep", "3");
    sb.put("default.out", &http(200, &[], &fixture("repo.json")));
    let mut jobs = jobs_for(&sb);
    for t in 1..=4 {
        jobs.start(t, ForgeRequest::Preflight { repo: repo() }, &[])
            .unwrap();
    }
    assert_eq!(jobs.running(), 4);
    let err = jobs
        .start(5, ForgeRequest::Preflight { repo: repo() }, &[])
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Limit);
    let got = wait_for(&mut jobs, 4);
    assert!(got.iter().all(|(_, r)| r.is_ok()));
    assert!(wait_until(5_000, || jobs.running() == 0));
}

#[test]
fn an_integration_whose_gh_cannot_be_used_refuses_everything_with_the_reason() {
    let mut jobs = UnavailableForge::new("gh was not found in an absolute entry of PATH");
    let st = jobs.status(&[]);
    assert!(!st.ready && st.detail.contains("not found"));
    let err = jobs
        .start(1, ForgeRequest::Preflight { repo: repo() }, &[])
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Unavailable);
    assert!(jobs.poll().is_empty());
    assert_eq!(jobs.running(), 0);
    assert!(jobs.remote_url("/repo", "origin").is_err());
}
