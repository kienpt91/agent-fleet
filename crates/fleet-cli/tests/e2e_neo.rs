//! NEO milestone N1 through the real `fleet` binary and daemon: the goal ledger, verification
//! contracts run by fleet itself, the completion rule, hostile input and restarts. The worker is
//! a fake agent script that claims success every time; what it actually produced decides.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod common;

use common::*;
use fleet_domain::value::Value;
use fleet_infra::testutil::{git, TempDir};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A worker that always reports `done`. The artifact is right only once the brief says that a
/// verification failed (that is, on a retry): a fake agent that "fixes" what it is told about.
const WORKER: &str = r#"#!/bin/sh
set -eu
if grep -q 'verify feature' "$FLEET_BRIEF_FILE"; then
  echo ready > answer.txt
else
  echo wrong > answer.txt
fi
printf '{"version":1,"status":"done","summary":"all done, every test passes"}\n' > .fleet/result.json
"#;

/// A worker whose artifact is never right.
const LIAR: &str = r#"#!/bin/sh
set -eu
echo wrong > answer.txt
printf '{"version":1,"status":"done","summary":"all done, every test passes"}\n' > .fleet/result.json
"#;

fn contract_json(extra_checks: &str) -> String {
    format!(
        r#"{{"version":1,"checks":[
  {{"id":"answer","kind":"file","path":"answer.txt","assert":"contains","value":"ready"}},
  {{"id":"exists","kind":"command","program":"/bin/sh","args":["-c","test -f answer.txt"]}}{extra_checks}
]}}"#
    )
}

fn fleet_with(worker_body: &str, retries: &str) -> (Fleet, TempDir) {
    let mut f = Fleet::new();
    let script = f.script("neo-worker.sh", worker_body);
    f.write_cfg(&simple_cfg(&script, "").set("max_retries", retries));
    f.start();
    let files = TempDir::new("neo-files");
    (f, files)
}

fn load_plan(f: &Fleet) {
    let file = f.tmp.write(
        "plan.toml",
        &plan_toml(&f.repo, "r1", 2, &[t("a"), t("b").after(&["a"])]),
    );
    f.ok(&["plan", file.to_str().unwrap()]);
}

fn add_goal(f: &Fleet, files: &TempDir, contract: &str) -> PathBuf {
    let path = files.write("contract.json", contract);
    f.ok(&[
        "goals",
        "add",
        "--id",
        "feature",
        "--title",
        "The answer is ready",
        "--intent",
        "answer.txt says ready",
        "--task",
        "r1/a",
        "--contract",
        path.to_str().unwrap(),
    ]);
    path
}

fn show(f: &Fleet, goal: &str) -> Value {
    f.json(&["goals", "show", goal])
}

fn verdicts(goal: &Value) -> Vec<String> {
    jarr(goal, "history")
        .iter()
        .rev()
        .map(|e| jstr(e, "verdict").to_string())
        .collect()
}

fn task_verification(goal: &Value) -> String {
    jstr(&jarr(goal, "tasks")[0], "verification").to_string()
}

// ---- the demo: a failing, then a passing contract -------------------------------------------------

#[test]
fn a_failing_contract_then_a_passing_one() {
    let (f, files) = fleet_with(WORKER, "2");
    load_plan(&f);
    add_goal(&f, &files, &contract_json(""));
    let listed = f.ok(&["goals", "list"]).stdout;
    assert!(
        listed.contains("feature") && listed.contains("pending"),
        "{listed}"
    );
    f.ok(&["start", "--run", "r1"]);

    // attempt 1 claims success with the wrong answer: fleet's own run fails it, a retry follows
    // with the failing check quoted in the brief; attempt 2 passes and completes the task
    f.wait_task("r1", "a", &["completed"]);
    let a = f.task("r1", "a");
    assert_eq!(jnum(&a, "attempt"), 2, "{}", a.canonical());
    assert_eq!(jnum(&a, "retries_used"), 1);
    let brief = fs::read_to_string(f.worktree("r1", "a").join(".fleet/brief.md")).unwrap();
    assert!(
        brief.contains("Quality gate 'verify feature/answer' failed"),
        "{brief}"
    );
    assert!(brief.contains("> "), "the failure is quoted: {brief}");

    let goal = show(&f, "feature");
    assert_eq!(jstr(&goal, "schema"), "fleet.goals.show/1");
    assert_eq!(jstr(&goal, "status"), "verified", "{}", goal.canonical());
    assert_eq!(jstr(&goal, "contract"), "passed");
    assert_eq!(task_verification(&goal), "verified");
    // the history: attempt 1 (answer failed, exists passed), attempt 2 (both passed)
    assert_eq!(
        verdicts(&goal),
        ["failed", "passed", "passed", "passed"].map(String::from),
        "{}",
        goal.canonical()
    );
    let history = jarr(&goal, "history");
    let head = git(&f.repo, &["rev-parse", "fleet/r1/a"]);
    assert_eq!(
        jstr(&history[0], "commit"),
        head,
        "evidence names the commit it checked"
    );
    assert_eq!(jstr(&history[0], "task"), "a");
    assert!(jstr(&history[3], "commit").len() == 40);
    assert_ne!(
        jstr(&history[3], "commit"),
        head,
        "attempt 1 checked another commit"
    );

    // the dependent ran only after a completed
    f.wait_task("r1", "b", &["completed"]);
    let events = f.events_raw();
    let lines: Vec<&str> = events.lines().collect();
    let completed_a = lines
        .iter()
        .position(|l| l.contains("\"type\":\"task_completed\"") && l.contains("\"task\":\"a\""))
        .unwrap();
    let last_evidence = lines
        .iter()
        .rposition(|l| l.contains("\"type\":\"evidence_recorded\""))
        .unwrap();
    assert!(last_evidence < completed_a, "evidence precedes completion");
    assert!(events.contains("\"type\":\"goal_status_changed\""));

    // the human views
    let text = f.ok(&["goals", "show", "feature"]).stdout;
    assert!(text.contains("goal feature [verified]"), "{text}");
    assert!(text.contains("answer [file, mandatory] passed"), "{text}");
    let remaining = f.ok(&["goals", "remaining"]).stdout;
    assert!(remaining.contains("nothing is left to do"), "{remaining}");
    let rem = f.json(&["goals", "remaining"]);
    assert_eq!(jstr(&rem, "schema"), "fleet.goals.remaining/1");
    assert_eq!(jnum(&rem, "verified"), 1);

    // fleet verify on demand: a fresh independent run, exit 0, more evidence, nothing else changes
    let out = f.ok(&["verify", "r1/a"]);
    assert!(
        out.stdout.starts_with("verification of r1/a: PASSED"),
        "{}",
        out.stdout
    );
    let again = show(&f, "feature");
    assert_eq!(jarr(&again, "history").len(), 6);
    assert_eq!(jstr(&f.task("r1", "a"), "status"), "completed");
    let json = f.json(&["verify", "feature"]);
    assert_eq!(jstr(&json, "schema"), "fleet.verify/1");
    assert!(jbool(&json, "passed") && !jbool(&json, "running"));

    // the audit chain still verifies with the new events in it
    let log = f.ok(&["verify-log"]).stdout;
    assert!(log.starts_with("event log ok:"), "{log}");
}

// ---- completion is blocked without evidence ----------------------------------------------------------

#[test]
fn a_task_whose_work_cannot_be_proven_never_completes_and_its_dependents_never_start() {
    let (f, files) = fleet_with(LIAR, "0");
    load_plan(&f);
    add_goal(&f, &files, &contract_json(""));
    f.ok(&["start", "--run", "r1"]);
    f.wait_task("r1", "a", &["failed"]);
    // the worker said "done, every test passes"; fleet's evidence says otherwise
    let b = f.wait_task("r1", "b", &["blocked"]);
    assert_eq!(b.get("block").map(|x| jstr(x, "kind")), Some("dependency"));
    let goal = show(&f, "feature");
    assert_eq!(jstr(&goal, "status"), "open");
    assert_eq!(jstr(&goal, "contract"), "failed");
    assert_eq!(task_verification(&goal), "failed");
    assert!(!f.events_raw().contains("\"type\":\"task_completed\""));
    let rem = f.ok(&["goals", "remaining"]).stdout;
    assert!(rem.contains("task r1/a (failed) for goal feature"), "{rem}");

    // verify on demand reports the failure with exit 6 and the quoted evidence
    let out = f.cli(&["verify", "r1/a"]);
    assert_eq!(out.code, 6, "{out:?}");
    assert!(out.stdout.contains("NOT PASSED"), "{}", out.stdout);
    assert!(
        out.stdout.contains("answer [file, mandatory] failed"),
        "{}",
        out.stdout
    );
}

#[test]
fn a_worker_report_is_unverified_until_fleets_own_run_finished() {
    let (f, files) = fleet_with(WORKER_ALWAYS_READY, "0");
    load_plan(&f);
    // a contract whose last check takes a while: the task sits in review, unverified
    add_goal(
        &f,
        &files,
        &contract_json(
            r#",
  {"id":"slow","kind":"command","program":"/bin/sh","args":["-c","sleep 3"]}"#,
        ),
    );
    f.ok(&["start", "--run", "r1"]);
    let started = Instant::now();
    let mut seen = Vec::new();
    loop {
        let goal = show(&f, "feature");
        let t = &jarr(&goal, "tasks")[0];
        seen.push((
            jstr(t, "status").to_string(),
            jstr(t, "verification").to_string(),
        ));
        if jstr(t, "status") == "completed" {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(40), "{seen:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        seen.contains(&("review".to_string(), "unverified".to_string())),
        "the finished worker's task is shown as unverified while the contract runs: {seen:?}"
    );
    assert!(
        !seen.contains(&("completed".to_string(), "unverified".to_string())),
        "{seen:?}"
    );
    assert_eq!(jstr(&show(&f, "feature"), "status"), "verified");
}

const WORKER_ALWAYS_READY: &str = r#"#!/bin/sh
set -eu
echo ready > answer.txt
printf '{"version":1,"status":"done","summary":"done"}\n' > .fleet/result.json
"#;

// ---- the worker cannot reach the ledger -------------------------------------------------------------------

#[test]
fn a_worker_cannot_change_the_ledger_or_run_verification() {
    let out = TempDir::new("neo-origin");
    let mut f = Fleet::new();
    let body = fill(
        "case \"$FLEET_TASK\" in\n  spy)\n    while [ ! -f @OUT@/go ]; do sleep 0.1; done\n    @FLEET@ --state-dir @STATE@ goals status feature abandoned > @OUT@/status.out 2>&1; echo $? > @OUT@/status.code\n    @FLEET@ --state-dir @STATE@ goals add --title injected > @OUT@/add.out 2>&1; echo $? > @OUT@/add.code\n    @FLEET@ --state-dir @STATE@ goals list > @OUT@/list.out 2>&1; echo $? > @OUT@/list.code\n    @FLEET@ --state-dir @STATE@ verify feature > @OUT@/verify.out 2>&1; echo $? > @OUT@/verify.code\n    touch @OUT@/all.done\n    ;;\nesac\nprintf '{\"version\":1,\"status\":\"done\",\"summary\":\"d\"}\\n' > .fleet/result.json\n",
        &[("OUT", out.path()), ("FLEET", Path::new(FLEET_BIN)), ("STATE", &f.state)],
    );
    let s = f.script("w.sh", &format!("#!/bin/sh\n{body}"));
    f.write_cfg(&simple_cfg(&s, ""));
    f.start();
    f.plan_and_start("r1", 2, &[t("spy")]);
    f.wait_live("r1", "spy");
    f.ok(&["goals", "add", "--id", "feature", "--title", "Ledger entry"]);
    fs::write(out.join("go"), "").unwrap();
    wait_file(&out.join("all.done"));
    for name in ["status", "add", "list", "verify"] {
        let code = out.read(&format!("{name}.code"));
        let text = out.read(&format!("{name}.out"));
        assert_eq!(code.trim(), "10", "{name}: {text}");
        assert!(text.contains("worker processes"), "{name}: {text}");
    }
    let goals = f.json(&["goals", "list"]);
    assert_eq!(jarr(&goals, "goals").len(), 1);
    assert_eq!(jstr(&jarr(&goals, "goals")[0], "status"), "open");
}

// ---- restarts and crashes ---------------------------------------------------------------------------------

#[test]
fn the_ledger_and_the_evidence_survive_a_crash_and_a_restart() {
    let (mut f, files) = fleet_with(WORKER_ALWAYS_READY, "0");
    load_plan(&f);
    let marker = files.join("proceed");
    // the last check waits for a marker file (at most a minute), so the test decides when the
    // verification can finish
    let wait_check = format!(
        r#",
  {{"id":"gate","kind":"command","timeout_ms":120000,"program":"/bin/sh","args":["-c","i=0; while [ ! -f {} ] && [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done; test -f {}"]}}"#,
        marker.display(),
        marker.display()
    );
    add_goal(&f, &files, &contract_json(&wait_check));
    f.ok(&[
        "goals",
        "add",
        "--id",
        "side",
        "--title",
        "Another goal",
        "--parent",
        "feature",
    ]);
    f.ok(&["start", "--run", "r1"]);
    f.wait_task("r1", "a", &["review"]);
    let before = show(&f, "feature");
    assert_eq!(task_verification(&before), "unverified");
    assert!(
        jarr(&before, "history").is_empty(),
        "no evidence while the checks run"
    );

    // the daemon is killed in the middle of the verification: no cleanup, no goodbye
    f.kill_daemon();
    f.start();
    assert_eq!(
        jstr(&f.task("r1", "a"), "status"),
        "review",
        "the task kept its place"
    );
    let after = show(&f, "feature");
    assert_eq!(jstr(&after, "status"), "open");
    assert_eq!(jstr(&after, "title"), "The answer is ready");
    assert_eq!(jstr(&after, "contract_id"), jstr(&before, "contract_id"));
    assert_eq!(jarr(&after, "child_ids").len(), 1, "{}", after.canonical());
    assert_eq!(
        f.json(&["goals", "list"]).get("total").map(|v| v.as_u64()),
        Some(Some(2))
    );
    assert!(!f.events_raw().contains("\"type\":\"task_completed\""));

    // the restarted daemon runs the contract again (the lost run recorded nothing); let it pass
    fs::write(&marker, "").unwrap();
    f.wait_task("r1", "a", &["completed"]);
    let done = show(&f, "feature");
    assert_eq!(jstr(&done, "contract"), "passed");
    assert_eq!(
        jarr(&done, "history").len(),
        3,
        "one run of three checks: {}",
        done.canonical()
    );
    // `feature` has a child that is not verified, so it is not verified itself (children count)
    assert_eq!(jstr(&done, "status"), "open");
    assert_eq!(jstr(&show(&f, "side"), "status"), "open");

    // a graceful restart keeps all of it too, and the chain verifies
    assert_eq!(f.shutdown(), 0);
    f.start();
    let again = show(&f, "feature");
    assert_eq!(again.canonical(), done.canonical());
    assert_eq!(jarr(&f.json(&["goals", "list"]), "goals").len(), 2);
    assert!(f.ok(&["verify-log"]).stdout.starts_with("event log ok:"));
    // abandoning the unfinished child lets the parent be verified from the evidence it has
    f.ok(&[
        "goals",
        "status",
        "side",
        "abandoned",
        "--reason",
        "out of scope",
    ]);
    assert_eq!(jstr(&show(&f, "feature"), "status"), "verified");
}

// ---- errors, hostile input and schemas --------------------------------------------------------------------

#[test]
fn bad_input_is_refused_with_the_right_exit_codes_and_nothing_is_recorded() {
    let (f, files) = fleet_with(WORKER, "2");
    load_plan(&f);
    let before = f.events_raw().lines().count();
    let write =
        |name: &str, content: &str| files.write(name, content).to_str().unwrap().to_string();
    let wrong_schema = write(
        "wrong.json",
        r#"{"version":1,"checks":[{"id":"x","kind":"shell","program":"/bin/sh"}]}"#,
    );
    let relative = write(
        "relative.json",
        r#"{"version":1,"checks":[{"id":"x","kind":"command","program":"sh"}]}"#,
    );
    let traversal = write(
        "traversal.json",
        r#"{"version":1,"checks":[{"id":"x","kind":"file","path":"../../etc/passwd","assert":"exists"}]}"#,
    );
    let unknown_key = write(
        "unknown.json",
        r#"{"version":1,"checks":[{"id":"x","kind":"file","path":"a","assert":"exists","run":"rm -rf /"}]}"#,
    );
    let broken = write("broken.json", "{\"version\": ");
    let toml = write("c.toml", "version = 1");
    for (name, path) in [
        ("schema", &wrong_schema),
        ("relative program", &relative),
        ("path traversal", &traversal),
        ("unknown key", &unknown_key),
        ("broken json", &broken),
        ("toml", &toml),
    ] {
        let out = f.cli(&["goals", "add", "--title", "t", "--contract", path]);
        assert_eq!(out.code, 7, "{name}: {out:?}");
        assert_eq!(out.stdout, "", "{name}");
        assert!(out.stderr.starts_with("fleet: "), "{name}: {}", out.stderr);
    }
    let missing = f.cli(&[
        "goals",
        "add",
        "--title",
        "t",
        "--contract",
        "/nonexistent/c.json",
    ]);
    assert_ne!(missing.code, 0);
    // hostile titles never get in
    let hostile = f.cli(&["goals", "add", "--title", "pwn\u{1b}]0;owned\u{7}"]);
    assert_eq!(hostile.code, 7, "{hostile:?}");
    assert!(!hostile.stderr.contains('\u{1b}'), "{:?}", hostile.stderr);
    assert_eq!(
        f.events_raw().lines().count(),
        before,
        "nothing was recorded"
    );

    f.ok(&["goals", "add", "--id", "g", "--title", "G"]);
    assert_eq!(f.cli(&["goals", "show", "nope"]).code, 4);
    assert_eq!(
        f.cli(&["goals", "status", "g", "verified"]).code,
        10,
        "verified is never set by hand"
    );
    assert_eq!(f.cli(&["goals", "status", "g", "finished"]).code, 7);
    assert_eq!(
        f.cli(&["goals", "status", "g", "open"]).code,
        6,
        "already open"
    );
    assert_eq!(
        f.cli(&["goals", "add", "--id", "g", "--title", "again"])
            .code,
        6
    );
    assert_eq!(
        f.cli(&["goals", "add", "--title", "t", "--task", "r1/ghost"])
            .code,
        4
    );
    assert_eq!(f.cli(&["verify", "r1/a"]).code, 4, "no goal names the task");
    assert_eq!(f.cli(&["verify", "g"]).code, 6, "a goal without a contract");
    assert_eq!(f.cli(&["verify", "r1/ghost"]).code, 4);
    assert_eq!(f.cli(&["goals", "frobnicate"]).code, 2);
}

#[test]
fn json_documents_have_stable_schemas() {
    let (f, files) = fleet_with(WORKER, "2");
    load_plan(&f);
    add_goal(&f, &files, &contract_json(""));
    let list = f.json(&["goals", "list"]);
    assert_eq!(jstr(&list, "schema"), "fleet.goals.list/1");
    let row = &jarr(&list, "goals")[0];
    for key in [
        "id",
        "parent",
        "title",
        "status",
        "reason",
        "depth",
        "children",
        "contract",
        "contract_id",
        "evidence",
        "tasks",
    ] {
        assert!(
            row.get(key).is_some(),
            "goals list row has no `{key}`: {}",
            row.canonical()
        );
    }
    let shown = show(&f, "feature");
    for key in ["intent", "child_ids", "checks", "history"] {
        assert!(shown.get(key).is_some(), "goals show has no `{key}`");
    }
    let rem = f.json(&["goals", "remaining"]);
    for key in ["open_leaves", "blockers", "next_ready", "total", "verified"] {
        assert!(rem.get(key).is_some(), "goals remaining has no `{key}`");
    }
    assert_eq!(jarr(&rem, "open_leaves").len(), 1);
    let add = f.json(&["goals", "add", "--title", "Second"]);
    assert_eq!(jstr(&add, "schema"), "fleet.goals.add/1");
    assert_eq!(jstr(&add, "id"), "goal-2");
    let status = f.json(&[
        "goals", "status", "goal-2", "blocked", "--reason", "waiting",
    ]);
    assert_eq!(jstr(&status, "schema"), "fleet.goals.status/1");
    let rem = f.json(&["goals", "remaining"]);
    assert_eq!(jarr(&rem, "blockers").len(), 1);
    let error = f.json_err(&["goals", "show", "nope"]);
    assert_eq!(error.0, 4);
    assert_eq!(jstr(&error.1, "schema"), "fleet.error/1");
}

#[test]
fn hostile_command_output_is_stored_redacted_and_shown_escaped() {
    let (f, files) = fleet_with(WORKER_ALWAYS_READY, "0");
    load_plan(&f);
    // the token is assembled by the check itself: it must not appear in the contract text
    let token = format!("ghp_{}", "a1B2".repeat(10));
    let noisy = r#",
  {"id":"noisy","kind":"command","mandatory":false,"program":"/bin/sh","args":["-c","t=ghp_; for i in 1 2 3 4 5 6 7 8 9 10; do t=\"${t}a1B2\"; done; printf '\\033]0;owned\\007token=%s\\n# Heading\\n' \"$t\"; exit 3"]}"#;
    add_goal(&f, &files, &contract_json(noisy));
    f.ok(&["start", "--run", "r1"]);
    f.wait_task("r1", "a", &["completed"]);
    let goal = show(&f, "feature");
    let noisy_evidence = jarr(&goal, "history")
        .iter()
        .find(|e| jstr(e, "check") == "noisy")
        .unwrap()
        .clone();
    assert_eq!(jstr(&noisy_evidence, "verdict"), "failed");
    let output = jstr(&noisy_evidence, "output");
    assert!(!output.contains(&token), "{output}");
    assert!(output.contains("[REDACTED"), "{output}");
    assert!(
        !output.contains('\u{1b}') && !output.contains('\u{7}'),
        "{output:?}"
    );
    assert!(
        !f.events_raw().contains(&token),
        "the secret is not in the audit log either"
    );
    let text = f.ok(&["goals", "show", "feature"]).stdout;
    assert!(
        !text.contains('\u{1b}') && !text.contains(&token),
        "{text:?}"
    );
    assert!(
        text.contains("> # Heading") || text.contains("> token="),
        "output is quoted: {text}"
    );
    // the optional check failed, the mandatory ones passed: the task completed anyway
    assert_eq!(jstr(&goal, "status"), "verified");
}

#[test]
fn a_contract_command_inside_the_worktree_is_never_run() {
    let (f, files) = fleet_with(
        r#"#!/bin/sh
set -eu
printf '#!/bin/sh\ntouch "$FLEET_WORKTREE/../planted-ran"\n' > planted.sh
chmod +x planted.sh
printf '{"version":1,"status":"done","summary":"d"}\n' > .fleet/result.json
"#,
        "0",
    );
    load_plan(&f);
    let planted = f.worktree("r1", "a").join("planted.sh");
    let contract = format!(
        r#"{{"version":1,"checks":[{{"id":"planted","kind":"command","program":"{}"}}]}}"#,
        planted.display()
    );
    add_goal(&f, &files, &contract);
    f.ok(&["start", "--run", "r1"]);
    f.wait_task("r1", "a", &["failed"]);
    let goal = show(&f, "feature");
    let evidence = &jarr(&goal, "history")[0];
    assert_eq!(jstr(evidence, "verdict"), "error");
    assert!(
        jstr(evidence, "summary").contains("refused"),
        "{}",
        evidence.canonical()
    );
    assert!(!f.worktree("r1", "a").join("../planted-ran").exists());
}
