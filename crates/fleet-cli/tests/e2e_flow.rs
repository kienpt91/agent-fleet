//! End-to-end orchestration through the real `fleet` binary: the scripted demo, dependency
//! flows, parallelism, plan validation, worker failure semantics, output hygiene, timeouts and
//! quality gates.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod common;

use common::*;
use fleet_infra::testutil::{git, wait_until};
use std::collections::BTreeSet;
use std::fs;

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

// ---- the demo ----------------------------------------------------------------------------------

#[test]
fn demo_flow_end_to_end() {
    let mut f = Fleet::with_cfg(&demo_cfg());
    let head_before = git(&f.repo, &["rev-parse", "HEAD"]);
    let readme_before = fs::read_to_string(f.repo.join("README.md")).unwrap();
    let lib_before = fs::read_to_string(f.repo.join("src/lib.txt")).unwrap();
    f.start();

    // plan + start
    let plan = demo_plan_file(&f);
    let out = f.ok(&["plan", plan.to_str().unwrap()]);
    assert!(out.stdout.contains("plan loaded into run r1"), "{out:?}");
    assert!(out.stdout.contains("(4 tasks)"), "{out:?}");
    f.ok(&["start", "--run", "r1", "--max-parallel", "2"]);

    // the api task asks a question and waits
    f.wait_for("the api question", WAIT_MS, |st| {
        !jarr(st, "gates").is_empty()
    });
    let api = f.wait_blocked("r1", "api", "question");
    assert_eq!(api.get("block").map(|b| jstr(b, "subject")), Some("g1"));

    let gl = f.json(&["gate", "list"]);
    assert_eq!(jstr(&gl, "schema"), "fleet.gate.list/1");
    let gates = jarr(&gl, "gates");
    assert_eq!(gates.len(), 1, "{}", gl.canonical());
    assert_eq!(jstr(&gates[0], "id"), "g1");
    assert_eq!(jstr(&gates[0], "task"), "api");
    assert_eq!(jstr(&gates[0], "kind"), "question");
    assert!(jbool(&gates[0], "open"));
    let choices: Vec<&str> = jarr(&gates[0], "choices")
        .iter()
        .filter_map(|c| c.as_str())
        .collect();
    assert_eq!(choices, ["axum", "actix"]);
    let text = f.ok(&["gate", "list"]).stdout;
    assert!(text.contains("g1") && text.contains("axum"), "{text}");

    // an answer outside the offered choices is refused and leaves the gate open
    let bad = f.cli(&["gate", "resolve", "g1", "--answer", "rocket"]);
    assert_eq!(bad.code, 7, "{bad:?}");
    assert_eq!(f.gates().len(), 1);
    f.ok(&["gate", "resolve", "g1", "--answer", "axum"]);

    // everything completes
    f.wait_for("all four tasks completed", 60_000, |st| {
        jarr(st, "runs")
            .iter()
            .any(|r| r.get("counts").map(|c| jnum(c, "completed")) == Some(4))
    });

    // release failed its quality gate once and was retried with the failure in its brief
    let release = f.task("r1", "release");
    assert_eq!(jnum(&release, "attempt"), 2, "{}", release.canonical());
    assert_eq!(jnum(&release, "retries_used"), 1);
    let brief =
        fs::read_to_string(f.repo.join(".fleet-worktrees/r1/release/.fleet/brief.md")).unwrap();
    assert!(brief.contains("Quality gate 'changelog' failed"), "{brief}");
    assert!(brief.contains("This is attempt 2"), "{brief}");
    let gates = jarr(&release, "gates");
    assert_eq!(gates.len(), 1);
    assert!(jbool(&gates[0], "passed"));
    // api was started a second time because of the answer, which does not use a retry
    let api = f.task("r1", "api");
    assert_eq!(jnum(&api, "attempt"), 2);
    assert_eq!(jnum(&api, "retries_used"), 0);
    let api_brief =
        fs::read_to_string(f.repo.join(".fleet-worktrees/r1/api/.fleet/brief.md")).unwrap();
    assert!(api_brief.contains("Operator says: axum"), "{api_brief}");
    // the worker's own question is quoted, not presented as fleet's words
    assert!(
        api_brief.contains("\n  > Which HTTP framework should the API use?"),
        "{api_brief}"
    );

    // worktrees and branches
    for task in ["schema", "api", "docs", "release"] {
        let t = f.task("r1", task);
        assert_eq!(jstr(&t, "status"), "completed");
        assert_eq!(jstr(&t, "branch"), format!("fleet/r1/{task}"));
        assert_eq!(
            jstr(&t, "worktree"),
            f.worktree("r1", task).to_str().unwrap()
        );
        assert!(f.worktree("r1", task).join(".git").exists());
        let branches = git(&f.repo, &["branch", "--list", &format!("fleet/r1/{task}")]);
        assert!(branches.contains(&format!("fleet/r1/{task}")), "{branches}");
        // commits made by fleet carry the configured author
        let who = git(
            &f.repo,
            &[
                "log",
                "-1",
                "--format=%an|%ae|%cn|%ce",
                &format!("fleet/r1/{task}"),
            ],
        );
        assert_eq!(
            who,
            "Fleet Test|fleet@example.invalid|Fleet Test|fleet@example.invalid"
        );
        let subject = git(
            &f.repo,
            &["log", "-1", "--format=%s", &format!("fleet/r1/{task}")],
        );
        assert!(
            subject.starts_with(&format!("fleet: {task} - ")),
            "{subject}"
        );
    }
    // the release worktree holds the work of its dependencies and its own file
    let rel = f.worktree("r1", "release");
    assert!(rel.join("docs.md").exists());
    assert!(rel.join("db/schema.sql").exists());
    assert!(fs::read_to_string(rel.join("CHANGELOG.md"))
        .unwrap()
        .contains("# Release notes"));

    // merging needs an approval, bound to a digest
    let first = f.cli(&["merge", "schema"]);
    assert_eq!(first.code, 5, "{first:?}");
    assert!(first.stderr.contains("approval a1"), "{first:?}");
    assert!(first.stderr.contains("fleet approve a1"), "{first:?}");
    let approvals = f.approvals();
    assert_eq!(approvals.len(), 1);
    assert_eq!(jstr(&approvals[0], "id"), "a1");
    assert_eq!(jstr(&approvals[0], "status"), "pending");
    assert_eq!(jstr(&approvals[0], "purpose"), "merge");
    assert_eq!(jstr(&approvals[0], "task"), "schema");
    let digest = jstr(&approvals[0], "digest").to_string();
    assert!(is_hex64(&digest), "{digest}");
    let st = f.status();
    let in_status = jarr(&st, "approvals");
    assert_eq!(in_status.len(), 1);
    assert_eq!(jstr(&in_status[0], "digest"), digest);
    // a wrong digest is refused and the approval stays pending
    let wrong = "0".repeat(64);
    let refused = f.cli(&["approve", "a1", "--digest", &wrong]);
    assert_eq!(refused.code, 7, "{refused:?}");
    assert_eq!(jstr(&f.approvals()[0], "status"), "pending");
    f.ok(&["approve", "a1", "--digest", &digest]);
    let merged = f.ok(&["merge", "schema"]);
    assert!(
        merged.stdout.contains("merged r1/schema")
            && merged.stdout.contains("fleet/r1/integration"),
        "{merged:?}"
    );
    let log = git(&f.repo, &["log", "--format=%s", "fleet/r1/integration"]);
    assert!(
        log.contains("fleet: merge schema into fleet/r1/integration"),
        "{log}"
    );
    // merging the same task again is a conflict
    assert_eq!(f.cli(&["merge", "schema"]).code, 6);

    for task in ["docs", "api", "release"] {
        let out = merge_with_approval(&f, task);
        assert!(out.stdout.contains(&format!("merged r1/{task}")), "{out:?}");
    }
    let tree = git(
        &f.repo,
        &["ls-tree", "-r", "--name-only", "fleet/r1/integration"],
    );
    let files: BTreeSet<&str> = tree.lines().collect();
    for want in [
        "README.md",
        "src/lib.txt",
        "db/schema.sql",
        "docs.md",
        "api.txt",
        "CHANGELOG.md",
    ] {
        assert!(files.contains(want), "{want} missing from {tree}");
    }
    assert_eq!(
        git(&f.repo, &["show", "fleet/r1/integration:api.txt"]),
        "framework: axum"
    );
    for task in ["schema", "api", "docs", "release"] {
        assert!(!jstr(&f.task("r1", task), "merged").is_empty());
    }

    // the user's repository is untouched
    assert_eq!(git(&f.repo, &["status", "--porcelain"]), "");
    assert_eq!(git(&f.repo, &["branch", "--show-current"]), "main");
    assert_eq!(git(&f.repo, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git(&f.repo, &["rev-list", "--count", "main"]), "1");
    assert_eq!(
        fs::read_to_string(f.repo.join("README.md")).unwrap(),
        readme_before
    );
    assert_eq!(
        fs::read_to_string(f.repo.join("src/lib.txt")).unwrap(),
        lib_before
    );
    let top: BTreeSet<String> = fs::read_dir(&f.repo)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let expected: BTreeSet<String> = [".git", ".fleet-worktrees", "README.md", "src"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(top, expected);
    let exclude = fs::read_to_string(f.repo.join(".git/info/exclude")).unwrap();
    assert!(
        exclude.lines().any(|l| l.trim() == ".fleet-worktrees"),
        "{exclude}"
    );

    // the audit log of the whole run is intact
    let v = f.ok(&["verify-log"]);
    assert!(v.stdout.starts_with("event log ok:"), "{v:?}");
}

#[test]
fn cleanup_removes_merged_worktrees_and_deletes_only_merged_branches() {
    let f = demo_until_completed();
    for task in ["schema", "docs", "api", "release"] {
        merge_with_approval(&f, task);
    }
    // a second run whose task completes but is never merged
    let plan = f
        .tmp
        .write("plan-r2.toml", &plan_toml(&f.repo, "r2", 1, &[t("docs")]));
    f.ok(&["plan", plan.to_str().unwrap()]);
    f.ok(&["start", "--run", "r2"]);
    f.wait_task("r2", "docs", &["completed"]);
    assert!(f.worktree("r2", "docs").join("docs.md").exists());

    let out = f.json(&["cleanup", "--branches"]);
    assert_eq!(jstr(&out, "schema"), "fleet.cleanup/1");
    let removed: BTreeSet<&str> = jarr(&out, "removed")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for task in ["schema", "docs", "api", "release"] {
        assert!(
            removed.contains(format!("r1/{task}").as_str()),
            "{}",
            out.canonical()
        );
        assert!(
            !f.worktree("r1", task).exists(),
            "r1/{task} worktree survived"
        );
    }
    assert!(!removed.contains("r2/docs"));
    let deleted: BTreeSet<&str> = jarr(&out, "branches_deleted")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(deleted.len(), 4, "{}", out.canonical());

    let branches = git(
        &f.repo,
        &["branch", "--list", "fleet/*", "--format=%(refname:short)"],
    );
    let branches: BTreeSet<&str> = branches.lines().map(str::trim).collect();
    for task in ["schema", "docs", "api", "release"] {
        assert!(
            !branches.contains(format!("fleet/r1/{task}").as_str()),
            "merged branch fleet/r1/{task} survived: {branches:?}"
        );
    }
    assert!(branches.contains("fleet/r1/integration"), "{branches:?}");
    assert!(
        branches.contains("fleet/r2/docs"),
        "unmerged branch was deleted: {branches:?}"
    );
    assert!(f.worktree("r2", "docs").join("docs.md").exists());
    // everything merged is still reachable from the integration branch
    assert_eq!(
        git(&f.repo, &["show", "fleet/r1/integration:docs.md"])
            .lines()
            .next(),
        Some("# Docs")
    );
    assert_eq!(git(&f.repo, &["status", "--porcelain"]), "");
}

#[test]
fn dependency_work_is_merged_into_the_dependent_worktree_before_it_starts() {
    let out = fleet_infra::testutil::TempDir::new("o");
    let mut f = Fleet::new();
    let body = fill(
        "case \"$FLEET_TASK\" in\n  a) echo from-a > a.txt ;;\n  b) ls > @OUT@/b-ls.txt; cat a.txt > @OUT@/b-a.txt ;;\nesac\n",
        &[("OUT", out.path())],
    );
    let s = f.script("w.sh", &body);
    f.write_cfg(&simple_cfg(&s, ""));
    f.start();
    f.plan_and_start("r1", 2, &[t("a"), t("b").after(&["a"])]);
    f.wait_task("r1", "b", &["completed"]);
    assert!(
        out.read("b-ls.txt").lines().any(|l| l == "a.txt"),
        "{}",
        out.read("b-ls.txt")
    );
    assert_eq!(out.read("b-a.txt").trim(), "from-a");
    assert!(f.worktree("r1", "b").join("a.txt").exists());
    // the dependency's commit is part of b's branch
    let files = git(&f.repo, &["ls-tree", "-r", "--name-only", "fleet/r1/b"]);
    assert!(files.lines().any(|l| l == "a.txt"), "{files}");
}

// ---- parallelism -------------------------------------------------------------------------------

#[test]
fn max_parallel_one_never_runs_two_tasks_at_once() {
    let out = fleet_infra::testutil::TempDir::new("o");
    let mut f = Fleet::new();
    let body = fill(
        "echo \"start $FLEET_TASK\" >> @OUT@/log.txt\nsleep 1\necho \"end $FLEET_TASK\" >> @OUT@/log.txt\n",
        &[("OUT", out.path())],
    );
    let s = f.script("w.sh", &body);
    f.write_cfg(&simple_cfg(&s, ""));
    f.start();
    f.plan_and_start("r1", 1, &[t("x"), t("y"), t("z")]);
    for task in ["x", "y", "z"] {
        f.wait_task("r1", task, &["completed"]);
    }
    let log = out.read("log.txt");
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 6, "{log}");
    for pair in lines.chunks(2) {
        let name = pair[0]
            .strip_prefix("start ")
            .unwrap_or_else(|| panic!("{log}"));
        assert_eq!(pair[1], format!("end {name}"), "tasks overlapped: {log}");
    }
}

#[test]
fn max_parallel_two_runs_tasks_side_by_side() {
    let out = fleet_infra::testutil::TempDir::new("o");
    let mut f = Fleet::new();
    // each worker waits (up to 30 s) until the other one has started: only a concurrent
    // schedule lets both of them see each other
    let body = fill(
        "other=q\n[ \"$FLEET_TASK\" = q ] && other=p\ntouch @OUT@/started-$FLEET_TASK\ni=0\nwhile [ ! -f @OUT@/started-$other ] && [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done\nif [ -f @OUT@/started-$other ]; then echo met > @OUT@/met-$FLEET_TASK; fi\n",
        &[("OUT", out.path())],
    );
    let s = f.script("w.sh", &body);
    f.write_cfg(&simple_cfg(&s, ""));
    f.start();
    f.plan_and_start("r1", 2, &[t("p"), t("q")]);
    f.wait_task("r1", "p", &["completed"]);
    f.wait_task("r1", "q", &["completed"]);
    assert!(
        out.exists("met-p") && out.exists("met-q"),
        "the tasks did not overlap"
    );
}

// ---- plan and run validation --------------------------------------------------------------------

fn noop_fleet() -> Fleet {
    let mut f = Fleet::new();
    let s = f.script("w.sh", "exit 0");
    f.write_cfg(&simple_cfg(&s, ""));
    f.start();
    f
}

fn assert_no_runs(f: &Fleet) {
    let st = f.status();
    assert!(jarr(&st, "runs").is_empty(), "{}", st.canonical());
    assert!(!f.repo.join(".fleet-worktrees").exists());
    assert_eq!(git(&f.repo, &["branch", "--list"]), "* main");
}

#[test]
fn a_cycle_in_a_plan_is_invalid_and_creates_nothing() {
    let f = noop_fleet();
    let out = f.plan_only("r1", 2, &[t("a").after(&["b"]), t("b").after(&["a"])]);
    assert_eq!(out.code, 7, "{out:?}");
    assert!(out.stderr.starts_with("fleet: "), "{out:?}");
    assert_no_runs(&f);
    // the same file as JSON reports the stable error code
    let file = f.tmp.join("plan-r1.toml");
    let (code, err) = f.json_err(&["plan", file.to_str().unwrap()]);
    assert_eq!(code, 7);
    assert_eq!(jstr(err.get("error").unwrap(), "code"), "invalid");
}

#[test]
fn an_unknown_dependency_in_a_plan_is_invalid() {
    let f = noop_fleet();
    let out = f.plan_only("r1", 2, &[t("a").after(&["ghost"])]);
    assert_eq!(out.code, 7, "{out:?}");
    assert!(out.stderr.contains("ghost"), "{out:?}");
    assert_no_runs(&f);
}

#[test]
fn a_run_for_a_directory_that_is_not_a_repository_fails() {
    let f = noop_fleet();
    let plain = f.tmp.join("plain");
    fs::create_dir_all(&plain).unwrap();
    let out = f.cli(&[
        "run",
        "create",
        "--objective",
        "x",
        "--repo",
        plain.to_str().unwrap(),
    ]);
    assert!(out.code == 7 || out.code == 4, "{out:?}");
    assert!(jarr(&f.status(), "runs").is_empty());
    assert!(!plain.join(".fleet-worktrees").exists());
}

#[test]
fn a_hostile_base_ref_is_refused() {
    let f = noop_fleet();
    let repo = f.repo.to_str().unwrap();
    let out = f.cli(&[
        "run",
        "create",
        "--objective",
        "x",
        "--repo",
        repo,
        "--base=--upload-pack=x",
    ]);
    assert_eq!(out.code, 7, "{out:?}");
    // the space-separated form never even reaches the daemon: a value that looks like an option
    // must use `=`
    let out = f.cli(&[
        "run",
        "create",
        "--objective",
        "x",
        "--repo",
        repo,
        "--base",
        "--upload-pack=x",
    ]);
    assert_eq!(out.code, 2, "{out:?}");
    // other shapes git would treat as options
    let arg = "--base=-x";
    let out = f.cli(&["run", "create", "--objective", "x", "--repo", repo, arg]);
    assert_eq!(out.code, 7, "base -x: {out:?}");
    assert!(jarr(&f.status(), "runs").is_empty());
}

// ---- worker failure semantics -----------------------------------------------------------------

#[test]
fn a_failing_worker_is_retried_then_failed_and_an_operator_retry_resets_it() {
    let mut f = Fleet::new();
    let flag = f.tmp.join("ok-now");
    let body = fill(
        "[ -f @FLAG@ ] && exit 0\necho 'worker: about to fail'\nexit 3\n",
        &[("FLAG", &flag)],
    );
    let s = f.script("w.sh", &body);
    f.write_cfg(&simple_cfg(&s, "").set("max_retries", "1"));
    f.start();
    f.plan_and_start("r1", 1, &[t("t")]);
    let task = f.wait_task("r1", "t", &["failed"]);
    assert_eq!(jnum(&task, "attempt"), 2, "{}", task.canonical());
    assert_eq!(jnum(&task, "retries_used"), 1);
    let failure = jstr(&task, "failure");
    assert!(failure.contains("exited with code 3"), "{failure}");
    // the retry brief carries what happened in the first attempt
    let brief = fs::read_to_string(f.worktree("r1", "t").join(".fleet/brief.md")).unwrap();
    assert!(brief.contains("Attempt 1 failed"), "{brief}");

    // the worker succeeds from now on
    // now the worker succeeds
    fs::write(&flag, "").unwrap();
    f.ok(&["retry", "r1/t"]);
    let task = f.wait_task("r1", "t", &["completed"]);
    assert_eq!(jnum(&task, "retries_used"), 0, "{}", task.canonical());
    assert!(task.get("failure").is_some_and(|v| v.is_null()));
    let again = f.cli(&["retry", "r1/t"]);
    assert_eq!(again.code, 6, "{again:?}");
}

#[test]
fn stop_ends_a_running_worker_without_a_retry() {
    let mut f = Fleet::new();
    let pid = f.tmp.join("w.pid");
    let body = fill("echo $$ > @PID@\nsleep 300\n", &[("PID", &pid)]);
    let s = f.script("w.sh", &body);
    f.write_cfg(&simple_cfg(&s, ""));
    f.start();
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_live("r1", "t");
    let worker = wait_pid_file(&pid);
    assert!(pid_alive(worker));
    let out = f.ok(&["stop", "t"]);
    assert!(out.stdout.contains("stop requested for t"), "{out:?}");
    let task = f.wait_task("r1", "t", &["failed"]);
    assert_eq!(jstr(&task, "failure"), "stopped by an operator");
    assert_eq!(jnum(&task, "attempt"), 1);
    assert_eq!(jnum(&task, "retries_used"), 0);
    assert!(
        wait_until(WAIT_MS, || !pid_alive(worker)),
        "the worker survived"
    );
    // still not restarted a moment later
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(jnum(&f.task("r1", "t"), "attempt"), 1);
}

#[test]
fn send_reaches_a_worker_blocked_on_read() {
    let mut f = Fleet::new();
    let out = fleet_infra::testutil::TempDir::new("o");
    let body = fill(
        "echo ready\nread line\nprintf '%s' \"$line\" > @OUT@/got.txt\n",
        &[("OUT", out.path())],
    );
    let s = f.script("w.sh", &body);
    f.write_cfg(&simple_cfg(&s, ""));
    f.start();
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_live("r1", "t");
    let sent = f.ok(&["send", "t", "hello", "worker"]);
    assert!(sent.stdout.contains("sent to t"), "{sent:?}");
    wait_file(&out.join("got.txt"));
    assert!(wait_until(WAIT_MS, || out.read("got.txt") == "hello worker"));
    f.wait_task("r1", "t", &["completed"]);
    // there is nobody to send to any more
    let late = f.cli(&["send", "t", "more"]);
    assert_eq!(late.code, 6, "{late:?}");
}

#[test]
fn tail_shows_sanitized_output() {
    let mut f = Fleet::new();
    let body = "printf 'plain-start\\n'\nprintf '\\033[31mred\\033[0m\\n'\nprintf '\\033]52;c;SGVsbG8=\\007\\n'\nprintf 'bidi:\\342\\200\\256evil\\n'\nprintf 'plain-end\\n'\n";
    let s = f.script("w.sh", body);
    f.write_cfg(&simple_cfg(&s, ""));
    f.start();
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_task("r1", "t", &["completed"]);

    let text = f.ok(&["tail", "t"]).stdout;
    let json = f.json(&["tail", "t"]);
    assert_eq!(jstr(&json, "schema"), "fleet.tail/1");
    let from_json = jstr(&json, "text").to_string();
    for shown in [&text, &from_json] {
        assert!(
            shown.contains("plain-start") && shown.contains("plain-end"),
            "{shown:?}"
        );
        assert!(shown.contains("red"), "{shown:?}");
        assert!(
            !shown.contains('\u{1b}'),
            "ESC reached the terminal: {shown:?}"
        );
        assert!(
            !shown.contains('\u{202e}'),
            "bidi override survived: {shown:?}"
        );
        assert!(!shown.contains('\u{7}'), "BEL survived: {shown:?}");
        assert!(
            !shown.contains("SGVsbG8="),
            "the clipboard payload survived: {shown:?}"
        );
    }
    // the stored log is sanitized as well
    for file in walk_files(&f.state.join("logs")) {
        let raw = fs::read(&file).unwrap();
        assert!(!raw.contains(&0x1b), "{} holds ESC", file.display());
        assert!(!String::from_utf8_lossy(&raw).contains('\u{202e}'));
    }
}

#[test]
fn secrets_are_redacted_in_tail_and_in_the_stored_log() {
    let mut f = Fleet::new();
    // a key-shaped string built at run time, so no fixture file holds one
    let body_chars = "aB3dE5gH7k".repeat(4);
    let secret = format!("ghp_{body_chars}");
    let body = format!("echo \"my token is {secret} end\"\nread wait\n");
    let s = f.script("w.sh", &body);
    f.write_cfg(&simple_cfg(&s, ""));
    f.start();
    f.plan_and_start("r1", 1, &[t("t")]);
    f.wait_live("r1", "t");
    // live output (the in-memory ring)
    let mut live = String::new();
    assert!(
        wait_until(WAIT_MS, || {
            live = f.ok(&["tail", "t"]).stdout;
            live.contains("my token is")
        }),
        "no output: {live:?}"
    );
    assert!(
        !live.contains(&body_chars) && !live.contains(&secret),
        "{live:?}"
    );
    // end the task so that its output is stored
    f.ok(&["stop", "t"]);
    f.wait_task("r1", "t", &["failed"]);
    let stored = f.ok(&["tail", "t"]).stdout;
    assert!(stored.contains("my token is"), "{stored:?}");
    assert!(!stored.contains(&body_chars), "{stored:?}");
    let logs = walk_files(&f.state.join("logs"));
    assert!(!logs.is_empty(), "no stored log");
    assert!(
        logs.iter()
            .any(|p| fs::read_to_string(p).unwrap().contains("my token is")),
        "the stored log lost the output"
    );
    assert!(
        tree_contains(&f.state, &body_chars).is_none(),
        "the secret reached {:?}",
        tree_contains(&f.state, &body_chars)
    );
}

// ---- timeouts ------------------------------------------------------------------------------------

#[test]
fn a_worker_over_its_time_limit_gets_term_then_kill() {
    let mut f = Fleet::new();
    let out = fleet_infra::testutil::TempDir::new("o");
    // the worker notes TERM but keeps going, so only KILL ends it
    let body = fill(
        "echo $$ > @OUT@/pid\ntrap 'echo term >> @OUT@/sig' TERM\nwhile :; do sleep 1; done\n",
        &[("OUT", out.path())],
    );
    let s = f.script("w.sh", &body);
    f.write_cfg(
        &simple_cfg(&s, "timeout_secs = 2")
            .set("max_retries", "0")
            .set("kill_grace_secs", "2"),
    );
    f.start();
    f.plan_and_start("r1", 1, &[t("t")]);
    let worker = wait_pid_file(&out.join("pid"));
    let task = f.wait_task("r1", "t", &["failed"]);
    assert!(
        jstr(&task, "failure").contains("time limit"),
        "{}",
        task.canonical()
    );
    assert!(
        out.read("sig").contains("term"),
        "TERM was never sent first"
    );
    assert!(
        wait_until(WAIT_MS, || !pid_alive(worker)),
        "the worker survived KILL"
    );
}

#[test]
fn a_timed_out_worker_is_retried_until_the_budget_is_used() {
    let mut f = Fleet::new();
    let out = fleet_infra::testutil::TempDir::new("o");
    let body = fill("echo $$ >> @OUT@/pids\nsleep 300\n", &[("OUT", out.path())]);
    let s = f.script("w.sh", &body);
    f.write_cfg(&simple_cfg(&s, "timeout_secs = 2").set("max_retries", "1"));
    f.start();
    f.plan_and_start("r1", 1, &[t("t")]);
    let task = f.wait_task("r1", "t", &["failed"]);
    assert_eq!(jnum(&task, "attempt"), 2, "{}", task.canonical());
    assert!(jstr(&task, "failure").contains("time limit"));
    let pids: Vec<u32> = out
        .read("pids")
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    assert_eq!(pids.len(), 2, "{pids:?}");
    for p in pids {
        assert!(wait_until(WAIT_MS, || !pid_alive(p)), "worker {p} survived");
    }
}

#[test]
fn a_silent_worker_hits_its_idle_timeout() {
    let mut f = Fleet::new();
    let s = f.script("w.sh", "sleep 300");
    f.write_cfg(&simple_cfg(&s, "idle_timeout_secs = 2").set("max_retries", "0"));
    f.start();
    f.plan_and_start("r1", 1, &[t("t")]);
    let task = f.wait_task("r1", "t", &["failed"]);
    assert!(
        jstr(&task, "failure").contains("silent"),
        "{}",
        task.canonical()
    );
}

#[test]
fn output_keeps_a_worker_from_hitting_the_idle_timeout() {
    let mut f = Fleet::new();
    let s = f.script(
        "w.sh",
        "i=0\nwhile [ $i -lt 6 ]; do echo tick $i; sleep 1; i=$((i+1)); done\n",
    );
    f.write_cfg(&simple_cfg(&s, "idle_timeout_secs = 3").set("max_retries", "0"));
    f.start();
    f.plan_and_start("r1", 1, &[t("t")]);
    let task = f.wait_task("r1", "t", &["completed"]);
    assert_eq!(jnum(&task, "attempt"), 1);
}

// ---- quality gates in refcheck-json format --------------------------------------------------------

const REFCHECK_GATE: &str = "if [ -f broken.flag ]; then\n  printf '{\"version\":1,\"summary\":{\"errors\":1},\"findings\":[{\"file\":\"src/a.ts\",\"line\":3,\"column\":1,\"severity\":\"error\",\"rule\":\"missing-export\",\"message\":\"nope\"}]}\\n'\n  exit 1\nfi\nprintf '{\"version\":1,\"summary\":{\"errors\":0},\"findings\":[]}\\n'\nexit 0\n";

fn refcheck_fleet(worker_body: &str, gate_body: &str, retries: &str) -> Fleet {
    let mut f = Fleet::new();
    let w = f.script("w.sh", worker_body);
    let g = f.script("gate.sh", gate_body);
    f.write_cfg(
        &simple_cfg(&w, "")
            .gate(
                "refcheck",
                &g,
                &[],
                "format = \"refcheck-json\"\ntimeout_secs = 20",
            )
            .set("max_retries", retries),
    );
    f.start();
    f
}

#[test]
fn a_clean_refcheck_report_passes_the_gate() {
    let f = refcheck_fleet("echo fine > ok.txt", REFCHECK_GATE, "2");
    f.plan_and_start("r1", 1, &[t("t")]);
    let task = f.wait_task("r1", "t", &["completed"]);
    assert_eq!(jnum(&task, "attempt"), 1);
    let gates = jarr(&task, "gates");
    assert_eq!(gates.len(), 1, "{}", task.canonical());
    assert_eq!(jstr(&gates[0], "name"), "refcheck");
    assert!(jbool(&gates[0], "passed"));
}

#[test]
fn refcheck_errors_fail_the_task_and_reach_the_retry_brief() {
    let worker = "if grep -q 'Quality gate' \"$FLEET_BRIEF_FILE\"; then rm -f broken.flag; else touch broken.flag; fi";
    let f = refcheck_fleet(worker, REFCHECK_GATE, "2");
    f.plan_and_start("r1", 1, &[t("t")]);
    let task = f.wait_task("r1", "t", &["completed"]);
    assert_eq!(jnum(&task, "attempt"), 2, "{}", task.canonical());
    assert_eq!(jnum(&task, "retries_used"), 1);
    let brief = fs::read_to_string(f.worktree("r1", "t").join(".fleet/brief.md")).unwrap();
    assert!(brief.contains("Quality gate 'refcheck' failed"), "{brief}");
    assert!(
        brief.contains("missing-export"),
        "the finding is missing: {brief}"
    );
    assert!(brief.contains("src/a.ts"), "{brief}");
}

#[test]
fn refcheck_errors_without_retries_fail_the_task_for_good() {
    let f = refcheck_fleet("touch broken.flag", REFCHECK_GATE, "0");
    f.plan_and_start("r1", 1, &[t("t")]);
    let task = f.wait_task("r1", "t", &["failed"]);
    assert!(
        jstr(&task, "failure").contains("quality gate failed: refcheck"),
        "{}",
        task.canonical()
    );
    let gates = jarr(&task, "gates");
    assert_eq!(gates.len(), 1);
    assert!(!jbool(&gates[0], "passed"));
    assert!(
        jstr(&gates[0], "summary").contains("1 error"),
        "{}",
        task.canonical()
    );
}

#[test]
fn garbage_from_a_refcheck_gate_fails_it() {
    let f = refcheck_fleet(
        "echo work > w.txt",
        "echo 'this is not json at all'\nexit 0\n",
        "0",
    );
    f.plan_and_start("r1", 1, &[t("t")]);
    let task = f.wait_task("r1", "t", &["failed"]);
    let gates = jarr(&task, "gates");
    assert_eq!(gates.len(), 1, "{}", task.canonical());
    assert!(!jbool(&gates[0], "passed"));
    assert!(
        jstr(&gates[0], "summary").contains("not valid JSON"),
        "{}",
        task.canonical()
    );
}
