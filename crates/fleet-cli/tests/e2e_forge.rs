//! End-to-end tests of the GitHub integration: the real `fleet` binary and a real daemon, a fake
//! `gh` first on the daemon's `PATH` (a script that replays the fixtures of `fleet-infra` and
//! records how it was started: arguments, environment, working directory) and a local bare
//! repository as the push remote. Nothing here reaches a network or a real account; what a real
//! `gh` prints today is the one thing these tests cannot prove.
//!
//! The fixtures follow the documented shape of the GitHub REST API; text that has the shape of a
//! secret is never stored in them: a placeholder stands for it and the value is built at run time.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod common;

use common::*;
use fleet_domain::value::Value;
use fleet_infra::testutil::{git, wait_until, TempDir};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A recognizable value that must appear nowhere it should not. It has no key shape: the point
/// is to find it, not to look like a credential.
const SENTINEL: &str = "fleet-test-sentinel-7f3a9c1d2e4b5a69";
const WORKER: &str = "echo \"$FLEET_TASK\" > \"$FLEET_TASK.txt\"";

// ---- fixtures --------------------------------------------------------------------------------------

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fleet-infra/tests/fixtures/gh")
        .join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
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
    match endpoint {
        "list_issues" | "list_pulls" => endpoint.to_string(),
        _ => endpoint
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() {
                    b as char
                } else {
                    '_'
                }
            })
            .collect(),
    }
}

/// The three secret shapes of the hostile issue, built at run time.
fn token_shape() -> String {
    format!("{}{}", "ghp_", "a1B2".repeat(10))
}

fn aws_shape() -> String {
    format!("{}{}", "AKIA", "ABCDEFGHIJKLMNOP")
}

fn hostile_issue() -> String {
    fixture("issue_hostile.json")
        .replace("@@TOKEN@@", &token_shape())
        .replace("@@AWS@@", &aws_shape())
        .replace("@@PRIVATE@@", "RSA PRIVATE")
}

fn issue_seven() -> String {
    fixture("issue_7.json").replace(
        "It crashes.",
        &format!("It crashes with {SENTINEL} inside."),
    )
}

// ---- the fake gh -----------------------------------------------------------------------------------

/// Looks up the answer of a call by the last argument (the endpoint), then by the number of the
/// call, then the default one. `probe.conf` switches on the probes of the origin test.
const FAKE_GH: &str = r#"#!/bin/bash
DIR='@DIR@'
n=$(cat "$DIR/count" 2>/dev/null || echo 0); n=$((n+1)); echo "$n" > "$DIR/count"
{ printf 'CALL %s\n' "$n"; for a in "$@"; do printf 'ARG %s\n' "$a"; done; } >> "$DIR/argv.log"
pwd > "$DIR/cwd.$n"
env | sort > "$DIR/env.$n"
echo $$ > "$DIR/pid.$n"
[ -e "$DIR/probe.conf" ] && . "$DIR/probe.sh"
for last; do :; done
case "$last" in
  repos/o/r/issues\?*) key=list_issues ;;
  repos/o/r/pulls\?*) key=list_pulls ;;
  *) key=$(printf '%s' "$last" | tr -c 'A-Za-z0-9' '_') ;;
esac
r="$DIR/fake/ep.$key"
[ -e "$r.out" ] || [ -e "$r.exit" ] || r="$DIR/fake/call.$n"
[ -e "$r.out" ] || [ -e "$r.exit" ] || r="$DIR/fake/default"
if [ -e "$r.sleep" ]; then sleep "$(cat "$r.sleep")" & echo $! > "$DIR/sleep.$n"; wait $!; fi
[ -e "$r.err" ] && cat "$r.err" >&2
if [ -e "$r.huge" ]; then head -c 4194304 /dev/zero | tr '\0' 'a'
elif [ -e "$r.reset" ]; then sed "s/@RESET@/$(( $(date +%s) + $(cat "$r.reset") ))/" "$r.out"
elif [ -e "$r.out" ]; then cat "$r.out"; fi
exit "$(cat "$r.exit" 2>/dev/null || echo 0)"
"#;

/// Sourced by the fake gh while `probe.conf` exists: runs the real `fleet status` against the
/// control socket and (when a port is known) a request to the dashboard, once from gh itself and
/// once from a detached descendant that outlives its parent shell.
const PROBE: &str = r#"
. "$DIR/probe.conf"
req() {
  local method=$1 path=$2 body=$3 out=$4 line
  echo "no reply" > "$out"
  exec 3<>/dev/tcp/127.0.0.1/$PORT || return 1
  printf '%s %s HTTP/1.1\r\nHost: 127.0.0.1:%s\r\nAuthorization: Bearer %s\r\nConnection: close\r\n\r\n' "$method" "$path" "$PORT" "$TOKEN" >&3
  while IFS= read -r -u 3 line || [ -n "$line" ]; do printf '%s\n' "$line"; done > "$out"
  exec 3<&-
}
probe_one() {
  local tag=$1
  "$FLEETBIN" --state-dir "$STATE" status > "$DIR/probe.$n.$tag.sock" 2>&1
  echo "exit=$?" >> "$DIR/probe.$n.$tag.sock"
  if [ -n "$PORT" ]; then req GET /api/status "" "$DIR/probe.$n.$tag.web"; fi
}
probe_one self
rm -f "$DIR/bg.$n.done"
( ( sleep 1; probe_one bg; : > "$DIR/bg.$n.done" ) & )
for i in $(seq 300); do [ -e "$DIR/bg.$n.done" ] && break; sleep 0.1; done
"#;

struct Gh {
    dir: PathBuf,
}

impl Gh {
    fn new(tmp: &TempDir) -> Gh {
        let dir = tmp.join("ghfake");
        fs::create_dir_all(dir.join("fake")).unwrap();
        fs::create_dir_all(dir.join("bin")).unwrap();
        let d = dir.display().to_string();
        tmp.script("ghfake/bin/gh", &FAKE_GH.replace("@DIR@", &d));
        fs::write(dir.join("probe.sh"), PROBE).unwrap();
        Gh { dir }
    }

    fn bin(&self) -> PathBuf {
        self.dir.join("bin")
    }

    fn fake(&self) -> PathBuf {
        self.dir.join("fake")
    }

    fn put(&self, name: &str, text: &str) {
        fs::write(self.fake().join(name), text).unwrap();
    }

    fn rm(&self, name: &str) {
        let _ = fs::remove_file(self.fake().join(name));
    }

    /// The answer to the call whose last argument is `endpoint` (`list_issues` and `list_pulls`
    /// stand for the list endpoints, whatever their query).
    fn ep(&self, endpoint: &str, out: &str) {
        let k = key_of(endpoint);
        for stale in ["exit", "err", "sleep", "huge", "reset"] {
            self.rm(&format!("ep.{k}.{stale}"));
        }
        self.put(&format!("ep.{k}.out"), out);
    }

    fn ep_with(&self, endpoint: &str, out: &str, exit: i32, stderr: &str) {
        let k = key_of(endpoint);
        self.put(&format!("ep.{k}.out"), out);
        self.put(&format!("ep.{k}.exit"), &exit.to_string());
        if stderr.is_empty() {
            self.rm(&format!("ep.{k}.err"));
        } else {
            self.put(&format!("ep.{k}.err"), stderr);
        }
    }

    fn calls(&self) -> usize {
        fs::read_to_string(self.dir.join("count"))
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0)
    }

    /// Every call's arguments, in order.
    fn argv(&self) -> Vec<Vec<String>> {
        let text = fs::read_to_string(self.dir.join("argv.log")).unwrap_or_default();
        let mut calls: Vec<Vec<String>> = Vec::new();
        for line in text.split_inclusive('\n') {
            let line = line.strip_suffix('\n').unwrap_or(line);
            if line.starts_with("CALL ") {
                calls.push(Vec::new());
            } else if let Some(a) = line.strip_prefix("ARG ") {
                calls.last_mut().unwrap().push(a.to_string());
            } else if let Some(last) = calls.last_mut().and_then(|c| c.last_mut()) {
                last.push('\n');
                last.push_str(line);
            }
        }
        calls
    }

    fn posts(&self) -> Vec<Vec<String>> {
        self.argv()
            .into_iter()
            .filter(|c| c.iter().any(|a| a == "POST"))
            .collect()
    }

    fn env_of(&self, n: usize) -> BTreeMap<String, String> {
        fs::read_to_string(self.dir.join(format!("env.{n}")))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.dir.join(rel)).unwrap_or_default()
    }
}

// ---- the world ---------------------------------------------------------------------------------------

const GITHUB: &str = "[github]\nenabled = true\nrepo = \"o/r\"\nallow_local_remote = true\n";

struct W {
    f: Fleet,
    gh: Gh,
    bare: PathBuf,
    /// Everything the CLI printed, for the sentinel scans.
    printed: RefCell<String>,
}

impl W {
    /// A fleet with the fake gh first on its `PATH` and a local bare repository as `origin`.
    /// `github` is the text of the `[github]` section (`None`: no section at all).
    fn new(github: Option<&str>) -> W {
        let mut f = Fleet::new();
        let gh = Gh::new(&f.tmp);
        f.path_env = format!("{}:{}", gh.bin().display(), system_path());
        let bare = f.tmp.join("remote.git");
        fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "-q", "--bare", "-b", "main"]);
        git(
            &f.repo,
            &["remote", "add", "origin", &bare.display().to_string()],
        );
        let worker = f.script("w.sh", WORKER);
        let mut cfg = simple_cfg(&worker, "").set("max_retries", "0");
        if let Some(g) = github {
            cfg = cfg.raw(g);
        }
        f.write_cfg(&cfg);
        W {
            f,
            gh,
            bare,
            printed: RefCell::new(String::new()),
        }
    }

    fn enabled() -> W {
        W::new(Some(GITHUB))
    }

    fn with(extra: &str) -> W {
        W::new(Some(&format!("{GITHUB}{extra}\n")))
    }

    /// Answers the endpoints the flows use.
    fn serve(&self) {
        let g = &self.gh;
        g.ep("repos/o/r", &http(200, &[], &fixture("repo.json")));
        g.ep("list_issues", &http(200, &[], &fixture("issues_list.json")));
        g.ep("list_pulls", &http(200, &[], &fixture("pulls_list.json")));
        g.ep("repos/o/r/issues/7", &http(200, &[], &issue_seven()));
        g.ep("repos/o/r/issues/66", &http(200, &[], &hostile_issue()));
        g.ep(
            "repos/o/r/pulls",
            &http(201, &[], &fixture("pull_created.json")),
        );
    }

    fn cli(&self, args: &[&str]) -> Out {
        let out = self.f.cli(args);
        let mut p = self.printed.borrow_mut();
        p.push_str(&format!(
            "$ fleet {args:?}\n{}\n{}\n",
            out.stdout, out.stderr
        ));
        out
    }

    fn ok(&self, args: &[&str]) -> Out {
        let out = self.cli(args);
        assert_eq!(
            out.code,
            0,
            "fleet {args:?}: {out:?}\n{}",
            self.f.daemon_log()
        );
        out
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut a = args.to_vec();
        a.push("--json");
        self.ok(&a).json()
    }

    /// A run with one finished task, so that issues can become tasks of it.
    fn seed(&self) {
        self.f.plan_and_start("r1", 2, &[t("seed")]);
        self.f.wait_task("r1", "seed", &["completed"]);
    }

    /// Makes a task of issue `n` and waits until the worker completed it.
    fn issue_task(&self, n: &str, id: &str) {
        let out = self.ok(&["issue", "to-task", n, "--id", id]);
        assert!(
            out.stdout.contains(&format!("r1/{id}")) && out.stdout.contains("untrusted"),
            "{out:?}"
        );
        self.f.wait_task("r1", id, &["completed"]);
    }

    fn approvals(&self) -> Vec<Value> {
        jarr(&self.json(&["approval", "list"]), "approvals").to_vec()
    }

    fn head_of(&self, branch: &str) -> String {
        safe_git(&self.f.repo, &["rev-parse", branch])
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

    /// The scan of everything: the tree of the test (but the answers the fake gh serves) and what
    /// the CLI printed.
    fn assert_no_sentinel(&self, what: &str) {
        assert!(
            !self.printed.borrow().contains(what),
            "the CLI printed {what}"
        );
        let served = self.gh.fake();
        for p in walk_files(self.f.tmp.path()) {
            if p.starts_with(&served) && p.extension().is_some_and(|e| e == "out") {
                continue;
            }
            let bytes = fs::read(&p).unwrap_or_default();
            let n = what.as_bytes();
            assert!(
                !bytes.windows(n.len()).any(|w| w == n),
                "{} contains {what}",
                p.display()
            );
        }
    }
}

fn text(o: &Out) -> String {
    format!("{}{}", o.stdout, o.stderr)
}

fn pending_id(w: &W, out: &Out) -> String {
    assert_eq!(out.code, 5, "{out:?}");
    let id = out
        .stderr
        .lines()
        .find_map(|l| l.strip_prefix("run: fleet approve "))
        .unwrap_or_else(|| panic!("no approval id in {out:?}"))
        .trim()
        .to_string();
    let found = w.approvals().into_iter().find(|a| jstr(a, "id") == id);
    assert_eq!(
        jstr(&found.expect("the approval is listed"), "status"),
        "pending"
    );
    id
}

fn approval_status(w: &W, id: &str) -> String {
    let all = w.approvals();
    let a = all
        .iter()
        .find(|a| jstr(a, "id") == id)
        .unwrap_or_else(|| panic!("no approval {id}"));
    jstr(a, "status").to_string()
}

// ---- a tiny HTTP client for the dashboard --------------------------------------------------------------

fn web_get(port: u16, path: &str, token: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect to the dashboard");
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .strip_prefix("HTTP/1.1 ")
        .and_then(|r| r.split(' ').next())
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("bad response {text:?}"));
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

// ---- 1. the sentinel ---------------------------------------------------------------------------------

/// Every token-shaped variable of the daemon's environment holds the sentinel; the flow reads
/// issues, makes a task, opens a pull request, and looks at every view. The sentinel is nowhere
/// afterwards: not in the files, not in what the CLI printed, not in the status document and not
/// in what the dashboard served; and `gh` itself saw the minimal environment and nothing else.
#[test]
fn a_token_in_the_daemons_environment_reaches_neither_gh_nor_any_output() {
    let mut w = W::enabled();
    for name in [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
        "FLEET_CONTROL_TOKEN",
        "FLEET_WEB_TOKEN",
        "FLEET_API_KEY",
        "FLEET_SECRET",
    ] {
        w.f.daemon_env
            .push((name.to_string(), SENTINEL.to_string()));
    }
    w.serve();
    let info = w.f.start_web(&[]);
    w.seed();

    w.ok(&["github", "status"]);
    w.ok(&["issue", "list"]);
    let shown = w.ok(&["issue", "show", "7"]);
    assert!(shown.stdout.contains("[REDACTED"), "{shown:?}");
    w.issue_task("7", "fix7");
    let first = w.cli(&[
        "pr",
        "create",
        "--task",
        "r1/fix7",
        "--title",
        "Fix the crash",
    ]);
    let id = pending_id(&w, &first);
    w.ok(&["approve", &id]);
    let opened = w.ok(&[
        "pr",
        "create",
        "--task",
        "r1/fix7",
        "--title",
        "Fix the crash",
    ]);
    assert!(
        opened.stdout.contains("pull request #12 opened"),
        "{opened:?}"
    );
    w.ok(&["pr", "list"]);

    for args in [
        &["status"][..],
        &["events", "--limit", "200"],
        &["approval", "list"],
        &["github", "status"],
        &["issue", "show", "7"],
        &["issue", "list"],
        &["doctor"],
    ] {
        let mut a = args.to_vec();
        w.cli(&a);
        a.push("--json");
        w.cli(&a);
    }
    let status = w.f.cli(&["status", "--json"]);
    assert!(!text(&status).contains(SENTINEL), "the status document");
    assert!(!w.f.events_raw().contains(SENTINEL), "the audit log");

    // what the dashboard serves
    for path in [
        "/api/status",
        "/api/events?limit=200",
        "/api/goals",
        "/api/health",
        "/api/log",
        "/api/forge",
    ] {
        let (code, body) = web_get(info.port, path, &info.token);
        assert_eq!(code, 200, "{path}: {body}");
        assert!(!body.contains(SENTINEL), "{path} served the sentinel");
        w.printed.borrow_mut().push_str(&body);
    }
    assert!(
        info.stderr_lines().iter().all(|l| !l.contains(SENTINEL)),
        "the daemon's stderr"
    );

    // gh's own view: exactly the minimal environment, in the root directory
    let calls = w.gh.calls();
    assert!(calls >= 6, "{calls} calls");
    for n in 1..=calls {
        let env = w.gh.env_of(n);
        let keys: BTreeSet<&str> = env.keys().map(String::as_str).collect();
        let allowed: BTreeSet<&str> = [
            "HOME",
            "PATH",
            "GH_PROMPT_DISABLED",
            "GH_NO_UPDATE_NOTIFIER",
            "PWD",
            "SHLVL",
            "_",
            "OLDPWD",
        ]
        .into_iter()
        .collect();
        assert!(keys.is_subset(&allowed), "call {n}: {keys:?}");
        for must in [
            "HOME",
            "PATH",
            "GH_PROMPT_DISABLED",
            "GH_NO_UPDATE_NOTIFIER",
        ] {
            assert!(keys.contains(must), "call {n} lacks {must}: {keys:?}");
        }
        assert_eq!(env["GH_PROMPT_DISABLED"], "1");
        assert_eq!(env["GH_NO_UPDATE_NOTIFIER"], "1");
        assert_eq!(w.gh.read(&format!("cwd.{n}")).trim(), "/");
        assert!(
            env["PATH"].split(':').all(|d| d.starts_with('/')),
            "{}",
            env["PATH"]
        );
    }
    w.assert_no_sentinel(SENTINEL);
}

// ---- 2. origin ---------------------------------------------------------------------------------------

/// `gh` and a descendant of it that outlives its parent shell run the real `fleet status` against
/// the control socket and ask the dashboard for the status with the real token: all refused as
/// processes of the daemon, while the operator is answered.
#[test]
fn gh_and_its_descendants_are_refused_on_the_socket_and_on_the_web_port() {
    let mut w = W::enabled();
    w.serve();
    let info = w.f.start_web(&[]);
    fs::write(
        w.gh.dir.join("probe.conf"),
        format!(
            "FLEETBIN='{}'\nSTATE='{}'\nPORT={}\nTOKEN={}\n",
            FLEET_BIN,
            w.f.state.display(),
            info.port,
            info.token
        ),
    )
    .unwrap();

    // the call itself works: the probes are what gh did on the side
    let list = w.ok(&["issue", "list"]);
    assert!(list.stdout.contains("Crash on start"), "{list:?}");
    assert!(w.gh.dir.join("bg.1.done").exists(), "the descendant ran");

    for tag in ["self", "bg"] {
        let sock = w.gh.read(&format!("probe.1.{tag}.sock"));
        assert!(
            sock.lines().any(|l| l == "exit=10") && sock.contains("worker processes"),
            "{tag}: the control socket answered gh's process: {sock}"
        );
        assert!(!sock.contains("r1/") && !sock.contains("schema"), "{sock}");
        let web = w.gh.read(&format!("probe.1.{tag}.web"));
        assert!(
            web.starts_with("HTTP/1.1 403")
                && web.contains("\"code\":\"forbidden\"")
                && web.contains("worker processes"),
            "{tag}: the dashboard answered gh's process: {web}"
        );
        for secret in [
            "fleet.status/1",
            "\"approvals\"",
            "\"runs\"",
            info.token.as_str(),
        ] {
            assert!(!web.contains(secret), "{tag}: {web}");
        }
    }

    // the very same requests from this process are answered: the refusal is about the origin
    assert_eq!(
        w.f.status().get("schema").and_then(Value::as_str),
        Some("fleet.status/1")
    );
    let (code, body) = web_get(info.port, "/api/status", &info.token);
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("fleet.status/1"));
}

// ---- 3. the whole flow -----------------------------------------------------------------------------

#[test]
fn an_issue_becomes_a_task_and_its_pull_request_needs_exactly_the_approved_text() {
    let w = W::enabled();
    w.serve();
    let mut w = w;
    w.f.start();
    w.seed();

    let list = w.ok(&["issue", "list"]);
    assert!(
        list.stdout.contains("untrusted") && list.stdout.contains("Crash on start"),
        "{list:?}"
    );
    let shown = w.ok(&["issue", "show", "7"]);
    assert!(
        shown.stdout.contains("  > Steps:") && shown.stdout.contains("#7"),
        "{shown:?}"
    );
    w.issue_task("7", "fix7");
    let again = w.cli(&["issue", "to-task", "7"]);
    assert_eq!(again.code, 6, "{again:?}");

    let args = |extra: &[&'static str]| -> Vec<&'static str> {
        let mut a = vec!["pr", "create", "--task", "r1/fix7"];
        a.extend_from_slice(extra);
        a
    };
    let base = ["--title", "Fix the crash", "--body", "Closes #7"];
    let calls_before = w.gh.calls();
    let first = w.cli(&args(&base));
    let id = pending_id(&w, &first);
    assert!(
        first.stderr.contains(&format!("approval {id}")),
        "{first:?}"
    );
    assert!(
        first.stderr.contains(&format!("fleet approve {id}")),
        "{first:?}"
    );

    let head = w.head_of("fleet/r1/fix7");
    let listed = w.approvals();
    let a = listed.iter().find(|a| jstr(a, "id") == id).unwrap();
    assert_eq!(jstr(a, "purpose"), "pull-request");
    assert_eq!(jstr(a, "tool"), "forge");
    let cargs: Vec<&str> = jarr(a, "args").iter().filter_map(Value::as_str).collect();
    assert_eq!(cargs.len(), 7, "{cargs:?}");
    assert!(cargs[0].ends_with("o/r"), "{cargs:?}");
    assert_eq!(
        &cargs[1..],
        [
            "main",
            "fleet/r1/fix7",
            head.as_str(),
            "Fix the crash",
            "Closes #7",
            "ready"
        ]
    );
    w.ok(&["approve", &id]);

    // anything else is another pull request: a new approval, and nothing was pushed or sent
    for (what, extra) in [
        (
            "title",
            vec!["--title", "Another title", "--body", "Closes #7"],
        ),
        (
            "body",
            vec!["--title", "Fix the crash", "--body", "Closes #7 and more"],
        ),
        (
            "base",
            vec![
                "--title",
                "Fix the crash",
                "--body",
                "Closes #7",
                "--base",
                "develop",
            ],
        ),
        (
            "draft",
            vec!["--title", "Fix the crash", "--body", "Closes #7", "--draft"],
        ),
    ] {
        let out = w.cli(&args(&extra));
        let other = pending_id(&w, &out);
        assert_ne!(other, id, "{what}");
        assert_eq!(approval_status(&w, &id), "approved", "{what}");
        assert!(w.remote_refs().is_empty(), "{what}: something was pushed");
        assert_eq!(w.gh.calls(), calls_before, "{what}: gh was started");
    }

    // the approved one goes through
    let opened = w.ok(&args(&base));
    assert!(
        opened
            .stdout
            .contains("pull request #12 opened for r1/fix7"),
        "{opened:?}"
    );
    assert_eq!(
        w.remote_refs(),
        vec![format!("refs/heads/fleet/r1/fix7 {head}")]
    );
    let posts = w.gh.posts();
    assert_eq!(posts.len(), 1, "{posts:?}");
    let post = &posts[0];
    assert_eq!(post.last().map(String::as_str), Some("repos/o/r/pulls"));
    for pair in [
        ["-f", "title=Fix the crash"],
        ["-f", "body=Closes #7"],
        ["-f", "head=fleet/r1/fix7"],
        ["-f", "base=main"],
        ["-F", "draft=false"],
    ] {
        assert!(
            post.windows(2).any(|w| w == pair),
            "{pair:?} missing from {post:?}"
        );
    }
    assert_eq!(approval_status(&w, &id), "consumed");
    assert!(w
        .f
        .events_raw()
        .contains("\"type\":\"pull_request_opened\""));

    // the same commit again: a conflict, and the approval cannot be spent again
    let second = w.cli(&args(&base));
    assert_eq!(second.code, 6, "{second:?}");
    assert!(second.stderr.contains("already"), "{second:?}");
    assert_eq!(w.gh.posts().len(), 1);
    assert_ne!(w.cli(&["approve", &id]).code, 0);
    assert_eq!(approval_status(&w, &id), "consumed");

    // a new commit on the task branch is another pull request: the old approval does not apply
    let wt = w.f.worktree("r1", "fix7");
    fs::write(wt.join("more.txt"), "more\n").unwrap();
    git(&wt, &["add", "more.txt"]);
    git(&wt, &["commit", "-q", "-m", "more work"]);
    let new_head = w.head_of("fleet/r1/fix7");
    assert_ne!(new_head, head);
    let moved = w.cli(&args(&base));
    let id2 = pending_id(&w, &moved);
    assert_ne!(id2, id);
    assert_eq!(approval_status(&w, &id), "consumed");
    assert_eq!(
        w.remote_refs(),
        vec![format!("refs/heads/fleet/r1/fix7 {head}")]
    );
    assert_eq!(w.gh.posts().len(), 1);
    w.ok(&["approve", &id2]);
    w.ok(&args(&base));
    assert_eq!(
        w.remote_refs(),
        vec![format!("refs/heads/fleet/r1/fix7 {new_head}")]
    );
    assert_eq!(w.gh.posts().len(), 2);
    assert_eq!(approval_status(&w, &id2), "consumed");

    // the links are visible and the daemon is healthy
    let status = w.json(&["github", "status"]);
    assert_eq!(jnum(&status, "issues_linked"), 1);
    assert!(
        jnum(&status, "pull_requests") >= 1,
        "{}",
        status.canonical()
    );
}

// ---- 4. hostile text ------------------------------------------------------------------------------

fn has_terminal_hazard(text: &str) -> Option<char> {
    text.chars().find(|c| {
        matches!(
            *c,
            '\u{1b}' | '\u{7}' | '\u{9b}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
                | '\u{2060}' | '\u{2066}'..='\u{2069}' | '\u{feff}'
        )
    })
}

#[test]
fn hostile_issue_text_is_inert_in_the_output_the_brief_and_the_approval() {
    let mut w = W::enabled();
    w.serve();
    w.gh.ep(
        "list_issues",
        &http(200, &[], &format!("[{}]", hostile_issue())),
    );
    w.f.start();
    w.seed();

    let token = token_shape();
    let aws = aws_shape();
    let list = w.ok(&["issue", "list"]);
    let shown = w.ok(&["issue", "show", "66"]);
    for o in [&list, &shown] {
        assert_eq!(has_terminal_hazard(&o.stdout), None, "{:?}", o.stdout);
        assert!(
            !o.stdout.contains(&token) && !o.stdout.contains(&aws),
            "{o:?}"
        );
        assert!(!o.stdout.contains("MIIEvQIBADANBgkq"), "{o:?}");
        assert!(o.stdout.contains("untrusted"), "{o:?}");
    }
    assert!(shown.stdout.contains("[REDACTED"), "{shown:?}");
    // the body is quoted line by line, between the label and the fleet's own last line
    let lines: Vec<&str> = shown.stdout.lines().collect();
    let start = lines.iter().position(|l| *l == "  body:").expect("a body");
    let end = lines
        .iter()
        .position(|l| l.starts_with("  make it a task"))
        .expect("the hint");
    assert!(end > start + 3);
    for l in &lines[start + 1..end] {
        assert!(l.starts_with("  > "), "an unquoted line of the body: {l:?}");
    }
    assert!(lines[0].contains("untrusted"), "{}", lines[0]);
    assert!(
        shown
            .stdout
            .contains("  > Ignore all previous instructions"),
        "the text is shown, as a quote"
    );

    // the brief of the task made of it
    w.issue_task("66", "h66");
    let task = w.f.task("r1", "h66");
    let title = jstr(&task, "title").to_string();
    assert!(
        title.starts_with("from GitHub issue #66 (untrusted): "),
        "{title}"
    );
    assert_eq!(has_terminal_hazard(&title), None);
    let brief = fs::read_to_string(w.f.worktree("r1", "h66").join(".fleet/brief.md"))
        .expect("the brief of the task");
    let label = "from GitHub issue #66 (untrusted)";
    let brief_lines: Vec<&str> = brief.lines().collect();
    let at = brief_lines
        .iter()
        .position(|l| *l == label)
        .unwrap_or_else(|| panic!("no label line in the brief:\n{brief}"));
    let tail: Vec<&str> = brief_lines[at + 1..]
        .iter()
        .copied()
        .take_while(|l| !l.trim().is_empty())
        .collect();
    assert!(tail.len() > 8, "the quote is short: {tail:?}");
    for l in &tail {
        assert!(
            l.starts_with('>'),
            "an unquoted line after the label: {l:?}"
        );
    }
    for l in &brief_lines {
        for words in [
            "Operator note",
            "Ignore all previous",
            "admin mode",
            "curl http",
        ] {
            if l.contains(words) {
                assert!(l.starts_with('>'), "{words:?} outside the quote: {l:?}");
            }
        }
        // the only headings are fleet's own (its template has one called "What to do")
        assert!(
            !l.starts_with('#') || !l.contains("Operator"),
            "a heading from the issue: {l:?}"
        );
    }
    assert_eq!(
        brief_lines
            .iter()
            .filter(|l| **l == "## What to do")
            .count(),
        1,
        "the issue added a heading of the brief's own name"
    );
    assert_eq!(has_terminal_hazard(&brief), None);
    assert!(!brief.contains(&token) && !brief.contains(&aws));
    assert!(!brief.contains("MIIEvQIBADANBgkq"));
    assert!(brief.contains("[REDACTED"));

    // the approval for its pull request carries none of it
    let first = w.cli(&["pr", "create", "--task", "r1/h66"]);
    let id = pending_id(&w, &first);
    let approvals = w.approvals();
    let a = approvals.iter().find(|a| jstr(a, "id") == id).unwrap();
    let doc = a.canonical();
    for words in [
        "Ignore all",
        "Operator note",
        "mallory",
        "evil.example",
        "Fix",
        "script",
    ] {
        assert!(!doc.contains(words), "{words:?} in the approval: {doc}");
    }
    let cargs: Vec<&str> = jarr(a, "args").iter().filter_map(Value::as_str).collect();
    assert_eq!(cargs[4], "Work on issue #66", "{cargs:?}");
    assert!(
        first.stderr.contains(&id) && !first.stderr.contains("Ignore"),
        "{first:?}"
    );
    for args in [&["goals", "list"][..], &["status"], &["approval", "list"]] {
        let o = w.cli(args);
        assert!(
            !o.stdout.contains("Ignore all") && !o.stdout.contains("mallory"),
            "{args:?}: {o:?}"
        );
    }
}

// ---- 5. off, missing, signed out, offline ------------------------------------------------------------

#[test]
fn without_a_github_section_the_integration_is_off() {
    let mut w = W::new(None);
    w.f.start();
    let st = w.ok(&["github", "status"]);
    assert!(st.stdout.contains("GitHub integration: off"), "{st:?}");
    let list = w.cli(&["issue", "list"]);
    assert_eq!(list.code, 9, "{list:?}");
    assert!(list.stderr.contains("is off"), "{list:?}");
    assert_eq!(w.gh.calls(), 0);
    assert_eq!(w.cli(&["pr", "list"]).code, 9);
    w.ok(&["status"]);
}

#[test]
fn a_missing_gh_is_told_and_changes_nothing() {
    let mut w = W::enabled();
    // the same PATH, without the fake gh (and the machine's own gh would be found there too: the
    // test needs a PATH that has none)
    w.f.path_env = system_path();
    if system_path()
        .split(':')
        .any(|d| Path::new(d).join("gh").exists())
    {
        eprintln!("a gh is installed in the system PATH; skipping");
        return;
    }
    w.f.start();
    w.f.plan_and_start("r1", 2, &[t("seed")]);
    w.f.wait_task("r1", "seed", &["completed"]);
    let before = w.f.events_raw();
    let st = w.ok(&["github", "status"]);
    assert!(st.stdout.contains("cannot run"), "{st:?}");
    for args in [
        &["issue", "list"][..],
        &["issue", "show", "7"],
        &["issue", "to-task", "7"],
        &["pr", "list"],
        &["pr", "create", "--task", "r1/seed"],
    ] {
        let o = w.cli(args);
        assert_eq!(o.code, 9, "{args:?}: {o:?}");
        assert!(
            o.stderr.contains("cannot run") && o.stderr.contains("gh was not found"),
            "{args:?}: {o:?}"
        );
        assert!(o.stderr.contains("Nothing was changed"), "{o:?}");
    }
    w.ok(&["status"]);
    assert_eq!(w.f.events_raw(), before, "the audit log changed");
    assert!(w.approvals().is_empty());
}

#[test]
fn a_signed_out_gh_and_an_offline_one_are_told_apart_and_change_nothing() {
    let mut w = W::enabled();
    w.serve();
    w.f.start();
    w.seed();
    let before = w.f.events_raw();

    // gh refuses to run without a sign-in
    w.gh.ep_with(
        "repos/o/r/issues/7",
        "",
        4,
        "To get started with GitHub CLI, please run:  gh auth login\n",
    );
    let o = w.cli(&["issue", "show", "7"]);
    assert_eq!(o.code, 9, "{o:?}");
    assert!(
        o.stderr.contains("not signed in") && o.stderr.contains("never sees or stores a token"),
        "{o:?}"
    );
    let o = w.cli(&["issue", "to-task", "7"]);
    assert_eq!(o.code, 9, "{o:?}");
    assert!(o.stderr.contains("never sees or stores a token"), "{o:?}");

    // no network
    w.gh.ep_with(
        "repos/o/r/issues/7",
        "",
        1,
        "error connecting to api.github.com\ncheck your internet connection\n",
    );
    let o = w.cli(&["issue", "show", "7"]);
    assert_eq!(o.code, 9, "{o:?}");
    assert!(
        o.stderr.contains("network") && o.stderr.contains("Nothing was changed"),
        "{o:?}"
    );
    w.ok(&["status"]);
    assert_eq!(w.f.events_raw(), before, "the audit log changed");
    assert_eq!(w.approvals().len(), 0);
    assert_eq!(
        w.json(&["github", "status"])
            .get("issues_linked")
            .and_then(Value::as_u64),
        Some(0)
    );
}

#[test]
fn a_pull_request_whose_preflight_fails_does_not_spend_the_approval() {
    let mut w = W::enabled();
    w.serve();
    w.f.start();
    w.seed();
    w.issue_task("7", "fix7");
    let create = [
        "pr", "create", "--task", "r1/fix7", "--title", "Fix", "--body", "b",
    ];
    let first = w.cli(&create);
    let id = pending_id(&w, &first);
    w.ok(&["approve", &id]);

    w.gh.ep_with(
        "repos/o/r",
        "",
        1,
        "error connecting to api.github.com\ncheck your internet connection\n",
    );
    let failed = w.cli(&create);
    assert_eq!(failed.code, 9, "{failed:?}");
    assert!(failed.stderr.contains("network"), "{failed:?}");
    assert_eq!(
        approval_status(&w, &id),
        "approved",
        "the approval was spent"
    );
    assert!(w.remote_refs().is_empty(), "something was pushed");
    assert!(w.gh.posts().is_empty());

    // signed out in the middle of it: the same
    w.gh.ep_with(
        "repos/o/r",
        "",
        4,
        "To get started with GitHub CLI, please run:  gh auth login\n",
    );
    assert_eq!(w.cli(&create).code, 9);
    assert_eq!(approval_status(&w, &id), "approved");

    // and when GitHub answers again, the same approval is enough
    w.serve();
    let opened = w.ok(&create);
    assert!(
        opened.stdout.contains("pull request #12 opened"),
        "{opened:?}"
    );
    assert_eq!(approval_status(&w, &id), "consumed");
    assert_eq!(w.remote_refs().len(), 1);
}

// ---- 6. the rate limit ------------------------------------------------------------------------------

#[test]
fn a_rate_limit_stops_the_requests_and_comes_back_after_the_wait() {
    let mut w = W::enabled();
    w.serve();
    w.f.start();
    let ep = "repos/o/r/issues/7";
    w.gh.ep_with(
        ep,
        &http(
            403,
            &[
                ("x-ratelimit-limit", "5000"),
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", "@RESET@"),
            ],
            &fixture("rate_limited.json"),
        ),
        1,
        "gh: API rate limit exceeded for user ID 1. (HTTP 403)\n",
    );
    w.gh.put(&format!("ep.{}.reset", key_of(ep)), "2");

    let first = w.cli(&["issue", "show", "7"]);
    assert_eq!(first.code, 9, "{first:?}");
    assert!(first.stderr.contains("slow down"), "{first:?}");
    assert_eq!(w.gh.calls(), 1);

    // asking again does not start gh, whatever is asked
    for args in [
        &["issue", "show", "7"][..],
        &["issue", "list"],
        &["pr", "list"],
    ] {
        let again = w.cli(args);
        assert_eq!(again.code, 9, "{args:?}: {again:?}");
        assert!(again.stderr.contains("no request is made"), "{again:?}");
    }
    assert_eq!(w.gh.calls(), 1, "gh was started while blocked");
    let st = w.ok(&["github", "status"]);
    assert!(st.stdout.contains("rate: limited"), "{st:?}");
    let until = w
        .json(&["github", "status"])
        .get("rate")
        .and_then(|r| r.get("until_ms"))
        .and_then(Value::as_u64)
        .expect("the status says until when");

    // after the wait a command works (the gate waits at least its first back-off)
    w.gh.ep(ep, &http(200, &[], &fixture("issue_7.json")));
    let now_ms = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
    };
    let wait = until.saturating_sub(now_ms()) + 300;
    assert!(wait < 60_000, "{wait} ms");
    std::thread::sleep(Duration::from_millis(wait));
    let ok = w.ok(&["issue", "show", "7"]);
    assert!(ok.stdout.contains("Crash on start"), "{ok:?}");
    assert_eq!(w.gh.calls(), 2);
    assert!(w.ok(&["github", "status"]).stdout.contains("rate: ok"));
}

// ---- 7. timeouts and garbage ---------------------------------------------------------------------------

#[test]
fn a_gh_that_hangs_is_killed_and_garbage_or_too_much_output_is_an_error() {
    let mut w = W::with("timeout_secs = 1");
    w.serve();
    w.f.start();
    let before = w.f.events_raw();
    let ep = "repos/o/r/issues/7";

    // hangs: killed with what it started, the CLI says so
    w.gh.ep(ep, "");
    w.gh.put(&format!("ep.{}.sleep", key_of(ep)), "30");
    let start = Instant::now();
    let o = w.cli(&["issue", "show", "7"]);
    assert_eq!(o.code, 12, "{o:?}");
    assert!(o.stderr.contains("did not answer within 1 s"), "{o:?}");
    assert!(
        start.elapsed() < Duration::from_secs(20),
        "{:?}",
        start.elapsed()
    );
    let gone = |file: &str| {
        let pid = wait_pid_file(&w.gh.dir.join(file));
        wait_until(10_000, || !pid_alive(pid))
    };
    assert!(gone("pid.1"), "gh is still running");
    assert!(gone("sleep.1"), "a child of gh is still running");

    // not JSON, no status line, nothing at all
    for (what, out) in [
        ("plain text", "this is not json".to_string()),
        ("an HTML page", http(200, &[], "<html>proxy login</html>")),
        ("nothing", String::new()),
    ] {
        w.gh.ep(ep, &out);
        let o = w.cli(&["issue", "show", "7"]);
        assert!(o.code == 7 || o.code == 9, "{what}: {o:?}");
        assert!(!o.stderr.is_empty() && o.stdout.is_empty(), "{what}: {o:?}");
        assert!(!o.stderr.contains("Crash"), "{what}: {o:?}");
    }

    // more than 2 MiB
    w.gh.ep(ep, "");
    w.gh.put(&format!("ep.{}.huge", key_of(ep)), "");
    let o = w.cli(&["issue", "show", "7"]);
    assert_eq!(o.code, 8, "{o:?}");
    assert!(o.stderr.contains("more than"), "{o:?}");
    assert!(o.stdout.is_empty());

    // nothing of this changed anything, and the next good answer is read normally
    assert_eq!(w.f.events_raw(), before, "the audit log changed");
    w.gh.ep(ep, &http(200, &[], &fixture("issue_7.json")));
    assert!(w
        .ok(&["issue", "show", "7"])
        .stdout
        .contains("Crash on start"));
}
