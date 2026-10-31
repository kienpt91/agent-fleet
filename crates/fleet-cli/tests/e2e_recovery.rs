//! Crash recovery, graceful shutdown, the tamper-evident audit log and the single-daemon rule,
//! all against the real binary.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

mod common;

use common::*;
use fleet_infra::testutil::{wait_until, TempDir};
use std::fs;
use std::path::Path;

/// Changes the first digit of the `"ts"` field of record `record` (1-based): the line still
/// parses as JSON, but its hash no longer matches.
fn flip_ts_digit(path: &Path, record: usize) {
    let mut bytes = fs::read(path).unwrap();
    let mut start = 0usize;
    for _ in 1..record {
        start += bytes[start..].iter().position(|b| *b == b'\n').unwrap() + 1;
    }
    let end = start + bytes[start..].iter().position(|b| *b == b'\n').unwrap();
    let key = b"\"ts\":";
    let rel = bytes[start..end]
        .windows(key.len())
        .position(|w| w == key)
        .expect("a ts field");
    let at = start + rel + key.len();
    bytes[at] = if bytes[at] == b'9' {
        b'8'
    } else {
        bytes[at] + 1
    };
    fs::write(path, bytes).unwrap();
}

/// A fleet with a loaded (not started) run, shut down gracefully, so that its audit log holds a
/// handful of records.
fn stopped_fleet_with_a_log() -> Fleet {
    let mut f = Fleet::new();
    let s = f.script("w.sh", "exit 0");
    f.write_cfg(&simple_cfg(&s, ""));
    f.start();
    let planned = f.plan_only("r1", 1, &[t("a"), t("b").after(&["a"])]);
    assert_eq!(planned.code, 0, "{planned:?}");
    assert_eq!(f.shutdown(), 0);
    f
}

fn log_lines(f: &Fleet) -> Vec<String> {
    f.events_raw().lines().map(str::to_string).collect()
}

// ---- crash recovery ------------------------------------------------------------------------------

#[test]
fn a_killed_daemon_is_recovered_orphans_are_stopped_and_decisions_survive() {
    let out = TempDir::new("o");
    let mut f = Fleet::new();
    // `slow` ignores the hangup that closing its terminal sends, so it outlives the daemon; its
    // second start finishes at once
    let body = fill(
        "case \"$FLEET_TASK\" in\n  quick) echo q > q.txt ;;\n  ask)\n    if grep -q '^Answer: ' \"$FLEET_BRIEF_FILE\"; then exit 0; fi\n    printf '{\"version\":1,\"kind\":\"question\",\"text\":\"which?\",\"choices\":[\"a\",\"b\"]}' > .fleet/question.json ;;\n  slow)\n    echo $$ > @OUT@/slow.pid\n    if [ -f @OUT@/second ]; then echo second > s.txt; exit 0; fi\n    trap '' HUP\n    sleep 300 &\n    echo $! > @OUT@/sleep.pid\n    wait\n    ;;\nesac\n",
        &[("OUT", out.path())],
    );
    let s = f.script("w.sh", &body);
    f.write_cfg(&simple_cfg(&s, ""));
    f.start();
    f.plan_and_start("r1", 3, &[t("quick"), t("slow"), t("ask")]);
    f.wait_task("r1", "quick", &["completed"]);
    f.wait_blocked("r1", "ask", "question");
    f.wait_live("r1", "slow");
    let slow_pid = wait_pid_file(&out.join("slow.pid"));
    let sleep_pid = wait_pid_file(&out.join("sleep.pid"));
    // an open gate (ask) and a pending approval (merge of quick)
    let merge = f.cli(&["merge", "quick"]);
    assert_eq!(merge.code, 5, "{merge:?}");
    let gates_before = f.gates();
    let approvals_before = f.approvals();
    assert_eq!(gates_before.len(), 1);
    assert_eq!(approvals_before.len(), 1);
    let quick_before = f.task("r1", "quick");

    f.kill_daemon();
    // the worker is on its own now
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        pid_alive(slow_pid),
        "the worker was expected to outlive a SIGKILLed daemon"
    );
    fs::write(out.join("second"), "").unwrap();

    // restart on the same state directory (the stale socket of the dead daemon is replaced)
    f.start();
    assert!(
        wait_until(WAIT_MS, || !pid_alive(slow_pid) && !pid_alive(sleep_pid)),
        "the orphaned worker was not terminated"
    );
    let recent = f.recent_lines();
    assert!(
        recent.iter().any(|l| l.contains("r1/slow orphan:")),
        "no orphan_reconciled in {recent:?}"
    );
    let raw = f.events_raw();
    assert!(raw.contains("\"type\":\"orphan_reconciled\""));
    assert_eq!(raw.matches("\"type\":\"daemon_started\"").count(), 2);

    // the task goes back in line without using a retry and now completes
    let slow = f.wait_task("r1", "slow", &["completed"]);
    assert_eq!(jnum(&slow, "attempt"), 2, "{}", slow.canonical());
    assert_eq!(jnum(&slow, "retries_used"), 0);
    // earlier work is untouched
    let quick_after = f.task("r1", "quick");
    assert_eq!(jstr(&quick_after, "status"), "completed");
    assert_eq!(jstr(&quick_after, "head"), jstr(&quick_before, "head"));
    assert_eq!(jnum(&quick_after, "attempt"), 1);
    // decisions survive the restart unchanged
    let gates_after = f.gates();
    let approvals_after = f.approvals();
    assert_eq!(
        gates_before
            .iter()
            .map(|g| g.canonical())
            .collect::<Vec<_>>(),
        gates_after
            .iter()
            .map(|g| g.canonical())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        approvals_before
            .iter()
            .map(|a| a.canonical())
            .collect::<Vec<_>>(),
        approvals_after
            .iter()
            .map(|a| a.canonical())
            .collect::<Vec<_>>()
    );
    assert_eq!(jstr(&approvals_after[0], "status"), "pending");
    assert_eq!(jstr(&f.task("r1", "ask"), "status"), "blocked");
    // and still work: approve and merge the earlier task, answer the question
    f.ok(&["approve", "a1"]);
    f.ok(&["merge", "quick"]);
    f.ok(&["gate", "resolve", "g1", "--answer", "a"]);
    assert_eq!(f.shutdown(), 0);
    assert!(f.cli(&["verify-log"]).code == 0);
}

#[test]
fn graceful_shutdown_stops_workers_requeues_tasks_and_exits_zero() {
    let out = TempDir::new("o");
    let mut f = Fleet::new();
    let body = fill(
        "case \"$FLEET_TASK\" in\n  quick) echo q > q.txt ;;\n  slow) echo $$ >> @OUT@/slow.pids; sleep 300 ;;\nesac\n",
        &[("OUT", out.path())],
    );
    let s = f.script("w.sh", &body);
    f.write_cfg(&simple_cfg(&s, ""));
    f.start();
    f.plan_and_start("r1", 2, &[t("quick"), t("slow")]);
    f.wait_task("r1", "quick", &["completed"]);
    f.wait_live("r1", "slow");
    let worker = wait_pid_file(&out.join("slow.pids"));
    assert!(pid_alive(worker));

    assert_eq!(f.shutdown(), 0, "{}", f.daemon_log());
    assert!(
        wait_until(WAIT_MS, || !pid_alive(worker)),
        "the worker survived the shutdown"
    );
    // the socket is gone with the daemon
    assert_eq!(f.cli(&["status"]).code, 3);
    // the audit log says what happened: stopped by the daemon, back in line, no retry used
    let raw = f.events_raw();
    assert!(raw.contains("\"outcome\":\"orphaned\""), "{raw}");
    let requeue = raw
        .lines()
        .find(|l| l.contains("\"type\":\"retry_queued\""))
        .unwrap_or_else(|| panic!("no retry_queued in {raw}"));
    assert!(requeue.contains("\"retries_used\":0"), "{requeue}");
    assert!(f.cli(&["verify-log"]).code == 0);

    // the next daemon picks the task up again as attempt 2 with the retry budget intact
    f.start();
    let slow = f.wait_live("r1", "slow");
    assert_eq!(jnum(&slow, "attempt"), 2, "{}", slow.canonical());
    assert_eq!(jnum(&slow, "retries_used"), 0);
    assert_eq!(jstr(&f.task("r1", "quick"), "status"), "completed");
    assert_eq!(f.shutdown(), 0);
}

// ---- the audit log -----------------------------------------------------------------------------------

#[test]
fn an_intact_log_verifies_and_reports_its_head() {
    let f = stopped_fleet_with_a_log();
    let lines = log_lines(&f);
    assert!(lines.len() >= 4, "{lines:?}");
    let out = f.ok(&["verify-log"]);
    assert!(
        out.stdout
            .starts_with(&format!("event log ok: {} records", lines.len())),
        "{out:?}"
    );
    let j = f.json(&["verify-log"]);
    assert_eq!(jstr(&j, "schema"), "fleet.verify-log/1");
    assert_eq!(jnum(&j, "records"), lines.len() as u64);
    let last = parse_json(lines.last().unwrap());
    assert_eq!(jstr(&j, "head_hash"), jstr(&last, "hash"));
    // the sidecar records the same head
    let head = fs::read_to_string(f.state.join("events.head")).unwrap();
    assert_eq!(
        head.trim(),
        format!("{} {}", lines.len(), jstr(&last, "hash"))
    );
    // and so does doctor
    let doc = f.ok(&["doctor"]);
    assert!(doc.stdout.contains("hash chain intact"), "{doc:?}");
}

#[test]
fn a_modified_record_is_detected_named_and_refused_by_the_daemon() {
    let mut f = stopped_fleet_with_a_log();
    flip_ts_digit(&f.events_log_path(), 2);

    let out = f.cli(&["verify-log"]);
    assert_eq!(out.code, 11, "{out:?}");
    assert!(out.stderr.contains("record 2"), "{out:?}");
    let (code, err) = f.json_err(&["verify-log"]);
    assert_eq!(code, 11);
    let e = err.get("error").unwrap();
    assert_eq!(jstr(e, "code"), "tampered");
    assert!(jstr(e, "message").contains("record 2"));

    // doctor reports the failure
    assert_eq!(f.cli(&["doctor"]).code, 13);

    // a daemon on this state directory refuses to serve instead of trusting the log
    let refused = f.daemon_refused();
    assert_eq!(refused.code, 11, "{refused:?}");
    assert!(refused.stderr.starts_with("fleet: "), "{refused:?}");
    assert!(refused.stderr.contains("record 2"), "{refused:?}");
    assert_eq!(
        f.cli(&["status"]).code,
        3,
        "a daemon is serving a tampered log"
    );
    // the log was not "repaired" or rewritten by the refusal
    assert_eq!(f.cli(&["verify-log"]).code, 11);
    // the guard has nothing to stop
    f.kill_daemon();
}

#[test]
fn a_changed_byte_in_a_body_is_detected() {
    let f = stopped_fleet_with_a_log();
    // change a letter inside the text of a record: `run_created` -> `run_createe`
    let path = f.events_log_path();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("end-to-end test run"));
    fs::write(
        &path,
        text.replacen("end-to-end test run", "end-to-end test rum", 1),
    )
    .unwrap();
    let out = f.cli(&["verify-log"]);
    assert_eq!(out.code, 11, "{out:?}");
    assert!(out.stderr.contains("was modified"), "{out:?}");
}

#[test]
fn a_deleted_last_line_is_detected_through_the_head_file() {
    let mut f = stopped_fleet_with_a_log();
    let lines = log_lines(&f);
    let kept = &lines[..lines.len() - 1];
    // the shortened chain is perfectly consistent on its own; only events.head can tell
    fs::write(f.events_log_path(), format!("{}\n", kept.join("\n"))).unwrap();
    let out = f.cli(&["verify-log"]);
    assert_eq!(out.code, 11, "{out:?}");
    assert!(
        out.stderr.contains(&format!("{} records", kept.len())),
        "{out:?}"
    );
    let refused = f.daemon_refused();
    assert_eq!(refused.code, 11, "{refused:?}");
    f.kill_daemon();
}

#[test]
fn verify_log_without_a_log_is_not_found() {
    let f = Fleet::new();
    let out = f.cli(&["verify-log"]);
    assert_eq!(out.code, 4, "{out:?}");
}

// ---- one daemon per state directory --------------------------------------------------------------------

#[test]
fn a_second_daemon_on_the_same_state_directory_is_refused() {
    let mut f = Fleet::new();
    let s = f.script("w.sh", "exit 0");
    f.write_cfg(&simple_cfg(&s, ""));
    f.start();
    let first_pid = jnum(f.status().get("daemon").unwrap(), "pid");
    let refused = f.daemon_refused();
    assert_eq!(refused.code, 6, "{refused:?}");
    assert!(
        refused.stderr.contains("another fleet daemon"),
        "{refused:?}"
    );
    // the first daemon is unaffected: same process, same socket, still answering
    let st = f.status();
    assert_eq!(jnum(st.get("daemon").unwrap(), "pid"), first_pid);
    assert_eq!(
        jnum(st.get("daemon").unwrap(), "pid"),
        u64::from(f.daemon_pid())
    );
    assert!(f.state.join("control/fleet.sock").exists());
    assert_eq!(f.shutdown(), 0);
}
