//! Application tests: merging completed tasks into the run's integration branch (with and without
//! approval), merge conflicts, dependency-merge conflicts and cleanup, driven through the command
//! interface with in-memory fakes for every port.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_app::api::Reply;
use fleet_app::testing::{test_config, Harness};
use fleet_domain::contract::{ContractRead, ResultStatus, WorkerResult};
use fleet_domain::error::{ErrorCode, FleetError};
use fleet_domain::events::DomainEvent;
use fleet_domain::ids::Sha;
use fleet_domain::value::Value;

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    Value::object(pairs)
}

fn s(x: &str) -> Value {
    Value::str(x)
}

fn list(items: &[&str]) -> Value {
    Value::array(items.iter().map(|i| Value::str(*i)))
}

fn new_run(h: &mut Harness) {
    let r = h
        .call(
            "run.create",
            obj(vec![
                ("objective", s("ship the feature")),
                ("repo", s("/repo")),
            ]),
        )
        .unwrap();
    assert_eq!(r.data().get("run").and_then(Value::as_str), Some("r1"));
}

fn add(h: &mut Harness, id: &str, after: &[&str]) {
    h.call(
        "task.add",
        obj(vec![
            ("run", s("r1")),
            ("id", s(id)),
            ("title", s(&format!("task {id}"))),
            ("spec", s("do the work")),
            ("after", list(after)),
        ]),
    )
    .unwrap();
}

fn start(h: &mut Harness) {
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
}

fn status(h: &mut Harness) -> Value {
    h.call("status", Value::Null).unwrap().data().clone()
}

fn task_doc(h: &mut Harness, id: &str) -> Value {
    let st = status(h);
    for run in st.get("runs").and_then(Value::as_array).unwrap() {
        for t in run.get("tasks").and_then(Value::as_array).unwrap() {
            if t.get("id").and_then(Value::as_str) == Some(id) {
                return t.clone();
            }
        }
    }
    panic!("task {id} not found");
}

fn task_status(h: &mut Harness, id: &str) -> String {
    task_doc(h, id)
        .get("status")
        .and_then(Value::as_str)
        .unwrap()
        .to_string()
}

fn is_null(h: &mut Harness, id: &str, field: &str) -> bool {
    task_doc(h, id).get(field).unwrap().is_null()
}

fn finish_ok(h: &mut Harness, task: &str) {
    let session = h.world.session_for(task).expect("task has a live session");
    let spec = h.world.session_spec(session).unwrap();
    h.world.write_result(
        &spec.cwd,
        ContractRead::Valid(WorkerResult {
            status: ResultStatus::Done,
            summary: format!("{task} done"),
        }),
    );
    h.world.mark_dirty(&spec.cwd);
    h.world.exit(session, 0);
    h.orch.tick();
}

fn count(h: &Harness, kind: &str) -> usize {
    h.world.event_kinds().iter().filter(|k| **k == kind).count()
}

fn last_brief(h: &Harness) -> String {
    h.world
        .briefs()
        .last()
        .expect("a brief was written")
        .1
        .clone()
}

/// Run r1 with the named independent tasks, all completed.
fn completed(tasks: &[&str], tweak: impl FnOnce(&mut fleet_app::config::AppConfig)) -> Harness {
    let mut cfg = test_config();
    cfg.approval_ttl_ms = 10_000;
    tweak(&mut cfg);
    let mut h = Harness::with_config(cfg);
    new_run(&mut h);
    for t in tasks {
        add(&mut h, t, &[]);
    }
    start(&mut h);
    for t in tasks {
        finish_ok(&mut h, t);
        assert_eq!(task_status(&mut h, t), "completed");
    }
    h
}

fn no_approval(c: &mut fleet_app::config::AppConfig) {
    c.merge_needs_approval = false;
}

fn merge(h: &mut Harness, task: &str) -> Result<Reply, FleetError> {
    h.call("merge", obj(vec![("task", s(task))]))
}

fn decide(h: &mut Harness, id: &str, approve: bool) -> Result<Reply, FleetError> {
    h.call(
        "approval.decide",
        obj(vec![("id", s(id)), ("approve", Value::Bool(approve))]),
    )
}

fn approvals(h: &mut Harness) -> Vec<Value> {
    h.call("approval.list", Value::Null)
        .unwrap()
        .data()
        .get("approvals")
        .and_then(Value::as_array)
        .unwrap()
        .to_vec()
}

fn approval_status(h: &mut Harness, id: &str) -> String {
    approvals(h)
        .into_iter()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(id))
        .and_then(|a| a.get("status").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| panic!("approval {id} not listed"))
}

fn gates(h: &mut Harness) -> Vec<Value> {
    h.call("gate.list", Value::Null)
        .unwrap()
        .data()
        .get("gates")
        .and_then(Value::as_array)
        .unwrap()
        .to_vec()
}

fn resolve(h: &mut Harness, id: &str, answer: &str) -> Result<Reply, FleetError> {
    h.call(
        "gate.resolve",
        obj(vec![("id", s(id)), ("answer", s(answer))]),
    )
}

fn worktree_of(task: &str) -> String {
    format!("/repo/.fleet-worktrees/r1/{task}")
}

// ---- merging with approval ----------------------------------------------------------------------

#[test]
fn the_first_merge_asks_for_approval_and_creates_exactly_one_pending_approval() {
    let mut h = completed(&["a"], |_| {});
    let err = merge(&mut h, "a").unwrap_err();
    assert_eq!(err.code, ErrorCode::ApprovalRequired);
    assert_eq!(
        err.subject.as_deref(),
        Some("a1"),
        "the error names the approval to decide"
    );
    assert!(
        err.message.contains("a1") && err.message.contains("digest"),
        "{}",
        err.message
    );
    assert!(
        h.world.merges().is_empty(),
        "nothing is merged before the approval"
    );

    let list = approvals(&mut h);
    assert_eq!(list.len(), 1);
    assert_eq!(
        list[0].get("purpose").and_then(Value::as_str),
        Some("merge")
    );
    assert_eq!(
        list[0].get("status").and_then(Value::as_str),
        Some("pending")
    );
    assert_eq!(list[0].get("task").and_then(Value::as_str), Some("a"));
    assert_eq!(list[0].get("tool").and_then(Value::as_str), Some("git"));
    assert_eq!(
        list[0].get("command").and_then(Value::as_str),
        Some("merge")
    );
    let args: Vec<&str> = list[0]
        .get("args")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        args,
        vec![
            "fleet/r1/integration",
            "fleet/r1/a",
            "c".repeat(40).as_str()
        ]
    );
    assert_eq!(task_status(&mut h, "a"), "completed");
}

#[test]
fn asking_to_merge_again_reuses_the_pending_approval() {
    let mut h = completed(&["a"], |_| {});
    let first = merge(&mut h, "a").unwrap_err();
    let second = merge(&mut h, "a").unwrap_err();
    let third = merge(&mut h, "r1/a").unwrap_err();
    assert_eq!(first.subject, second.subject);
    assert_eq!(first.subject, third.subject);
    assert_eq!(count(&h, "approval_requested"), 1);
    assert_eq!(approvals(&mut h).len(), 1);
}

#[test]
fn after_approval_the_merge_succeeds_and_is_recorded() {
    let mut h = completed(&["a"], |_| {});
    merge(&mut h, "a").unwrap_err();
    decide(&mut h, "a1", true).unwrap();
    assert!(
        h.world.merges().is_empty(),
        "approving alone does not merge"
    );

    let reply = merge(&mut h, "a").unwrap();
    assert_eq!(
        reply.data().get("task").and_then(Value::as_str),
        Some("r1/a")
    );
    assert_eq!(
        reply.data().get("branch").and_then(Value::as_str),
        Some("fleet/r1/a")
    );
    assert_eq!(
        reply
            .data()
            .get("integration_branch")
            .and_then(Value::as_str),
        Some("fleet/r1/integration")
    );
    let sha = reply
        .data()
        .get("merge_sha")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();
    assert_eq!(
        h.world.merges(),
        vec![("fleet/r1/integration".to_string(), "fleet/r1/a".to_string())]
    );
    assert_eq!(
        task_doc(&mut h, "a").get("merged").and_then(Value::as_str),
        Some(sha.as_str())
    );
    assert_eq!(count(&h, "task_merged"), 1);
    assert_eq!(count(&h, "approval_consumed"), 1);
    assert_eq!(approval_status(&mut h, "a1"), "consumed");
}

fn sha_of(c: char) -> Sha {
    Sha::parse(&c.to_string().repeat(40)).unwrap()
}

#[test]
fn the_adapter_is_asked_to_merge_exactly_the_commit_that_was_approved() {
    let mut h = completed(&["a"], |_| {});
    h.world.set_branch_head("fleet/r1/a", sha_of('e'));
    merge(&mut h, "a").unwrap_err();
    let approved_args: Vec<String> = approvals(&mut h)[0]
        .get("args")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    assert_eq!(approved_args[2], "e".repeat(40));
    decide(&mut h, "a1", true).unwrap();

    merge(&mut h, "a").unwrap();
    let requests = h.world.merge_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].task_branch, "fleet/r1/a");
    assert_eq!(requests[0].head, sha_of('e'));
}

#[test]
fn a_branch_that_moved_after_the_approval_is_not_sent_to_the_adapter_and_needs_a_new_approval() {
    let mut h = completed(&["a"], |_| {});
    h.world.set_branch_head("fleet/r1/a", sha_of('e'));
    merge(&mut h, "a").unwrap_err();
    decide(&mut h, "a1", true).unwrap();

    h.world.set_branch_head("fleet/r1/a", sha_of('f'));
    let err = merge(&mut h, "a").unwrap_err();
    assert_eq!(err.code, ErrorCode::ApprovalRequired);
    assert_eq!(err.subject.as_deref(), Some("a2"));
    assert!(h.world.merge_requests().is_empty());
    assert_eq!(approval_status(&mut h, "a1"), "approved");
    assert_eq!(count(&h, "approval_consumed"), 0);

    // The new approval is for the new commit, and that is what gets merged.
    decide(&mut h, "a2", true).unwrap();
    merge(&mut h, "a").unwrap();
    let requests = h.world.merge_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].head, sha_of('f'));
}

#[test]
fn without_approvals_the_adapter_still_gets_the_head_that_was_read() {
    let mut h = completed(&["a"], no_approval);
    h.world.set_branch_head("fleet/r1/a", sha_of('e'));
    merge(&mut h, "a").unwrap();
    let requests = h.world.merge_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].head, sha_of('e'));
}

#[test]
fn the_integration_branch_is_created_once_across_merges() {
    let mut h = completed(&["a", "b"], |_| {});
    for (task, approval) in [("a", "a1"), ("b", "a2")] {
        merge(&mut h, task).unwrap_err();
        decide(&mut h, approval, true).unwrap();
        merge(&mut h, task).unwrap();
    }
    assert_eq!(count(&h, "integration_created"), 1);
    assert_eq!(count(&h, "task_merged"), 2);
    assert_eq!(h.world.merges().len(), 2);
}

#[test]
fn an_approval_is_single_use_and_the_next_merge_needs_a_new_one() {
    let mut h = completed(&["a", "b"], |_| {});
    merge(&mut h, "a").unwrap_err();
    decide(&mut h, "a1", true).unwrap();
    merge(&mut h, "a").unwrap();
    assert_eq!(approval_status(&mut h, "a1"), "consumed");

    let err = merge(&mut h, "b").unwrap_err();
    assert_eq!(err.code, ErrorCode::ApprovalRequired);
    assert_eq!(
        err.subject.as_deref(),
        Some("a2"),
        "a consumed approval cannot be reused"
    );
    assert_eq!(h.world.merges().len(), 1);
    assert_eq!(approval_status(&mut h, "a1"), "consumed");
}

#[test]
fn an_approval_for_one_task_cannot_be_used_for_another() {
    let mut h = completed(&["a", "b"], |_| {});
    merge(&mut h, "a").unwrap_err();
    merge(&mut h, "b").unwrap_err();
    assert_eq!(approvals(&mut h).len(), 2);
    decide(&mut h, "a1", true).unwrap();

    let err = merge(&mut h, "b").unwrap_err();
    assert_eq!(err.code, ErrorCode::ApprovalRequired);
    assert_eq!(
        err.subject.as_deref(),
        Some("a2"),
        "b still waits for its own approval"
    );
    assert!(h.world.merges().is_empty());
    assert!(is_null(&mut h, "b", "merged"));

    merge(&mut h, "a").unwrap();
    assert_eq!(
        h.world.merges(),
        vec![("fleet/r1/integration".to_string(), "fleet/r1/a".to_string())]
    );
    assert_eq!(approval_status(&mut h, "a2"), "pending");
}

#[test]
fn a_denied_approval_cannot_be_used_and_asking_again_needs_a_new_one() {
    let mut h = completed(&["a"], |_| {});
    merge(&mut h, "a").unwrap_err();
    let reply = decide(&mut h, "a1", false).unwrap();
    assert_eq!(
        reply.data().get("decision").and_then(Value::as_str),
        Some("denied")
    );
    let err = merge(&mut h, "a").unwrap_err();
    assert_eq!(err.code, ErrorCode::ApprovalRequired);
    assert_eq!(err.subject.as_deref(), Some("a2"));
    assert!(h.world.merges().is_empty());
    assert_eq!(approval_status(&mut h, "a1"), "denied");
    assert_eq!(
        task_status(&mut h, "a"),
        "completed",
        "a denied merge leaves the task as it was"
    );
}

#[test]
fn an_approved_approval_that_expired_cannot_be_used() {
    let mut h = completed(&["a"], |_| {});
    merge(&mut h, "a").unwrap_err();
    decide(&mut h, "a1", true).unwrap();
    h.world.advance(10_000);
    let err = merge(&mut h, "a").unwrap_err();
    assert_eq!(err.code, ErrorCode::ApprovalRequired);
    assert_eq!(err.subject.as_deref(), Some("a2"));
    assert!(h.world.merges().is_empty());
    assert_eq!(approval_status(&mut h, "a1"), "expired");
    assert_eq!(count(&h, "approval_consumed"), 0);
}

#[test]
fn an_approval_still_valid_just_before_its_deadline_is_usable() {
    let mut h = completed(&["a"], |_| {});
    merge(&mut h, "a").unwrap_err();
    decide(&mut h, "a1", true).unwrap();
    h.world.advance(9_999);
    merge(&mut h, "a").unwrap();
    assert_eq!(count(&h, "task_merged"), 1);
}

#[test]
fn a_pending_merge_approval_that_expired_cannot_be_decided() {
    let mut h = completed(&["a"], |_| {});
    merge(&mut h, "a").unwrap_err();
    h.world.advance(10_000);
    assert_eq!(
        decide(&mut h, "a1", true).unwrap_err().code,
        ErrorCode::Conflict
    );
    let err = merge(&mut h, "a").unwrap_err();
    assert_eq!(
        err.subject.as_deref(),
        Some("a2"),
        "a fresh request replaces the lapsed one"
    );
    assert!(h.world.merges().is_empty());
}

#[test]
fn a_merge_approval_is_pinned_to_its_digest() {
    let mut h = completed(&["a"], |_| {});
    merge(&mut h, "a").unwrap_err();
    let wrong = h.call(
        "approval.decide",
        obj(vec![
            ("id", s("a1")),
            ("approve", Value::Bool(true)),
            ("digest", s(&"f".repeat(16))),
        ]),
    );
    assert_eq!(wrong.unwrap_err().code, ErrorCode::Invalid);
    assert_eq!(
        merge(&mut h, "a").unwrap_err().code,
        ErrorCode::ApprovalRequired
    );
    assert!(h.world.merges().is_empty());

    let digest = approvals(&mut h)[0]
        .get("digest")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();
    h.call(
        "approval.decide",
        obj(vec![
            ("id", s("a1")),
            ("approve", Value::Bool(true)),
            ("digest", s(&digest[..12])),
        ]),
    )
    .unwrap();
    merge(&mut h, "a").unwrap();
    assert_eq!(count(&h, "task_merged"), 1);
}

#[test]
fn merging_proceeds_immediately_when_no_approval_is_needed() {
    let mut h = completed(&["a"], no_approval);
    let reply = merge(&mut h, "a").unwrap();
    assert!(reply
        .data()
        .get("merge_sha")
        .and_then(Value::as_str)
        .is_some());
    assert!(approvals(&mut h).is_empty());
    assert_eq!(count(&h, "approval_requested"), 0);
    assert_eq!(count(&h, "approval_consumed"), 0);
    assert_eq!(count(&h, "task_merged"), 1);
    assert_eq!(count(&h, "integration_created"), 1);
    assert!(!is_null(&mut h, "a", "merged"));
}

// ---- what may be merged -------------------------------------------------------------------------

#[test]
fn only_completed_tasks_can_be_merged() {
    let mut cfg = test_config();
    cfg.max_retries = 0;
    let mut h = Harness::with_config(cfg);
    new_run(&mut h);
    add(&mut h, "running", &[]);
    add(&mut h, "failing", &[]);
    add(&mut h, "waiting", &["running"]);
    start(&mut h);
    let id = h.world.session_for("failing").unwrap();
    h.world.exit(id, 1);
    h.orch.tick();
    assert_eq!(task_status(&mut h, "running"), "running");
    assert_eq!(task_status(&mut h, "failing"), "failed");
    assert_eq!(task_status(&mut h, "waiting"), "pending");
    for t in ["running", "failing", "waiting"] {
        let err = merge(&mut h, t).unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict, "{t}");
        assert!(
            err.message.contains("only completed tasks can be merged"),
            "{}",
            err.message
        );
    }
    assert!(
        approvals(&mut h).is_empty(),
        "no approval is requested for a task that cannot be merged"
    );
    assert!(h.world.merges().is_empty());
}

#[test]
fn merging_an_unknown_task_is_not_found() {
    let mut h = completed(&["a"], no_approval);
    assert_eq!(merge(&mut h, "zzz").unwrap_err().code, ErrorCode::NotFound);
    assert_eq!(merge(&mut h, "r9/a").unwrap_err().code, ErrorCode::NotFound);
}

#[test]
fn a_task_cannot_be_merged_twice() {
    let mut h = completed(&["a"], no_approval);
    merge(&mut h, "a").unwrap();
    let err = merge(&mut h, "a").unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert!(err.message.contains("already merged"), "{}", err.message);
    assert_eq!(h.world.merges().len(), 1);
    assert_eq!(count(&h, "task_merged"), 1);
}

#[test]
fn a_second_merge_with_approvals_on_is_a_conflict_not_a_new_request() {
    let mut h = completed(&["a"], |_| {});
    merge(&mut h, "a").unwrap_err();
    decide(&mut h, "a1", true).unwrap();
    merge(&mut h, "a").unwrap();
    assert_eq!(merge(&mut h, "a").unwrap_err().code, ErrorCode::Conflict);
    assert_eq!(
        approvals(&mut h).len(),
        1,
        "no approval is requested for an already merged task"
    );
}

// ---- merge conflicts -----------------------------------------------------------------------------

#[test]
fn a_merge_conflict_aborts_opens_a_gate_and_blocks_the_task() {
    let mut h = completed(&["a"], no_approval);
    h.world.conflict_on("fleet/r1/a");
    let err = merge(&mut h, "a").unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert_eq!(
        err.subject.as_deref(),
        Some("g1"),
        "the error names the gate"
    );
    assert!(
        err.message.contains("src/lib.rs") && err.message.contains("aborted"),
        "{}",
        err.message
    );

    assert!(
        h.world.merges().is_empty(),
        "the integration branch was not changed"
    );
    assert_eq!(count(&h, "merge_conflicted"), 1);
    assert_eq!(count(&h, "task_merged"), 0);
    assert!(is_null(&mut h, "a", "merged"));
    assert_eq!(task_status(&mut h, "a"), "blocked");
    let block = task_doc(&mut h, "a").get("block").cloned().unwrap();
    assert_eq!(block.get("kind").and_then(Value::as_str), Some("conflict"));
    assert_eq!(block.get("subject").and_then(Value::as_str), Some("g1"));
    let g = gates(&mut h);
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].get("kind").and_then(Value::as_str), Some("conflict"));
    assert!(g[0]
        .get("text")
        .and_then(Value::as_str)
        .unwrap()
        .contains("conflicted in: src/lib.rs"));

    // while the conflict is open the task cannot be merged again
    assert_eq!(merge(&mut h, "a").unwrap_err().code, ErrorCode::Conflict);
    assert_eq!(count(&h, "decision_opened"), 1);
}

#[test]
fn the_conflicting_merge_events_come_in_a_consistent_order() {
    let mut h = completed(&["a"], no_approval);
    h.world.conflict_on("fleet/r1/a");
    merge(&mut h, "a").unwrap_err();
    let kinds = h.world.event_kinds();
    let at = |k: &str| kinds.iter().rposition(|x| *x == k).unwrap();
    assert!(at("merge_conflicted") < at("decision_opened"));
    assert!(at("decision_opened") < at("task_blocked"));
}

#[test]
fn resolving_the_conflict_gate_sends_the_task_back_to_work_with_the_conflict_in_the_brief() {
    let mut h = completed(&["a"], no_approval);
    h.world.conflict_on("fleet/r1/a");
    merge(&mut h, "a").unwrap_err();
    resolve(
        &mut h,
        "g1",
        "merge the integration branch into yours and keep both changes",
    )
    .unwrap();

    assert_eq!(task_status(&mut h, "a"), "running");
    let brief = last_brief(&h);
    assert!(brief.contains("conflicted in: src/lib.rs"), "{brief}");
    assert!(
        brief.contains(
            "Operator says: merge the integration branch into yours and keep both changes"
        ),
        "{brief}"
    );

    // the reworked task completes and can then be merged
    finish_ok(&mut h, "a");
    assert_eq!(task_status(&mut h, "a"), "completed");
    h.world.clear_conflicts();
    merge(&mut h, "a").unwrap();
    assert_eq!(h.world.merges().len(), 1);
    assert_eq!(count(&h, "task_merged"), 1);
    assert_eq!(
        count(&h, "integration_created"),
        1,
        "the branch was created by the first attempt and is not created again"
    );
}

#[test]
fn a_conflict_while_merging_does_not_record_a_merge_and_the_task_can_conflict_again() {
    let mut h = completed(&["a"], no_approval);
    h.world.conflict_on("fleet/r1/a");
    merge(&mut h, "a").unwrap_err();
    resolve(&mut h, "g1", "try again").unwrap();
    finish_ok(&mut h, "a");
    // still conflicting: a second gate, not a silent success
    let err = merge(&mut h, "a").unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert_eq!(err.subject.as_deref(), Some("g2"));
    assert_eq!(count(&h, "task_merged"), 0);
    assert!(h.world.merges().is_empty());
}

#[test]
fn an_approval_is_used_up_by_a_merge_that_conflicts() {
    let mut h = completed(&["a"], |_| {});
    h.world.conflict_on("fleet/r1/a");
    merge(&mut h, "a").unwrap_err();
    decide(&mut h, "a1", true).unwrap();
    let err = merge(&mut h, "a").unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert_eq!(err.subject.as_deref(), Some("g1"));
    assert_eq!(approval_status(&mut h, "a1"), "consumed");

    resolve(&mut h, "g1", "fixed").unwrap();
    finish_ok(&mut h, "a");
    h.world.clear_conflicts();
    let again = merge(&mut h, "a").unwrap_err();
    assert_eq!(again.code, ErrorCode::ApprovalRequired);
    assert_eq!(
        again.subject.as_deref(),
        Some("a2"),
        "the reworked branch needs a new approval"
    );
    assert!(h.world.merges().is_empty());
}

#[test]
fn a_conflict_in_one_task_does_not_stop_others_from_merging() {
    let mut h = completed(&["a", "b"], no_approval);
    h.world.conflict_on("fleet/r1/a");
    merge(&mut h, "a").unwrap_err();
    merge(&mut h, "b").unwrap();
    assert_eq!(
        h.world.merges(),
        vec![("fleet/r1/integration".to_string(), "fleet/r1/b".to_string())]
    );
    assert_eq!(task_status(&mut h, "a"), "blocked");
    assert_eq!(task_status(&mut h, "b"), "completed");
}

#[test]
fn a_dependency_conflict_blocks_the_dependent_behind_a_gate_before_any_session_starts() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    add(&mut h, "b", &["a"]);
    h.world.conflict_on("fleet/r1/a");
    start(&mut h);
    finish_ok(&mut h, "a");
    assert_eq!(task_status(&mut h, "a"), "completed");

    assert_eq!(task_status(&mut h, "b"), "blocked");
    assert!(
        h.world.session_for("b").is_none(),
        "the worker never starts on a conflicted branch"
    );
    assert_eq!(h.world.session_ids().len(), 1);
    let block = task_doc(&mut h, "b").get("block").cloned().unwrap();
    assert_eq!(block.get("kind").and_then(Value::as_str), Some("conflict"));
    assert_eq!(block.get("subject").and_then(Value::as_str), Some("g1"));
    let g = gates(&mut h);
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].get("task").and_then(Value::as_str), Some("b"));
    assert_eq!(g[0].get("kind").and_then(Value::as_str), Some("conflict"));
    let text = g[0].get("text").and_then(Value::as_str).unwrap();
    assert!(
        text.contains("conflicted in: src/shared.rs") && text.contains("fleet/r1/b"),
        "{text}"
    );
    assert_eq!(
        h.world.worktree_merges(),
        vec![(worktree_of("b"), vec!["fleet/r1/a".to_string()])]
    );

    // nothing retries the merge on its own
    for _ in 0..3 {
        h.orch.tick();
    }
    assert_eq!(h.world.worktree_merges().len(), 1);
    assert_eq!(count(&h, "decision_opened"), 1);
    assert_eq!(count(&h, "task_dispatched"), 1);
}

#[test]
fn resolving_a_dependency_conflict_starts_the_dependent_with_the_operator_note() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    add(&mut h, "b", &["a"]);
    h.world.conflict_on("fleet/r1/a");
    start(&mut h);
    finish_ok(&mut h, "a");
    assert_eq!(task_status(&mut h, "b"), "blocked");

    h.world.clear_conflicts();
    resolve(&mut h, "g1", "merged by hand").unwrap();
    assert_eq!(task_status(&mut h, "b"), "running");
    assert!(h.world.session_for("b").is_some());
    let brief = last_brief(&h);
    assert!(brief.contains("Operator says: merged by hand"), "{brief}");
    assert!(
        brief.contains("a done"),
        "the dependency summary is still in the brief: {brief}"
    );
    assert_eq!(h.world.worktree_merges().len(), 2);
}

#[test]
fn a_dependency_conflict_that_persists_opens_a_fresh_gate_instead_of_starting_the_worker() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    add(&mut h, "b", &["a"]);
    h.world.conflict_on("fleet/r1/a");
    start(&mut h);
    finish_ok(&mut h, "a");
    resolve(&mut h, "g1", "I think it is fine").unwrap();
    assert_eq!(task_status(&mut h, "b"), "blocked");
    assert!(h.world.session_for("b").is_none());
    let open: Vec<Value> = gates(&mut h)
        .into_iter()
        .filter(|g| g.get("open").and_then(Value::as_bool) == Some(true))
        .collect();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].get("id").and_then(Value::as_str), Some("g2"));
}

// ---- cleanup -------------------------------------------------------------------------------------

/// Run r1 with: a completed and merged, b failed, c running, d completed but not merged.
fn mixed() -> Harness {
    let mut cfg = test_config();
    cfg.merge_needs_approval = false;
    cfg.max_retries = 0;
    let mut h = Harness::with_config(cfg);
    new_run(&mut h);
    for t in ["a", "b", "c", "d"] {
        add(&mut h, t, &[]);
    }
    start(&mut h);
    finish_ok(&mut h, "a");
    let b = h.world.session_for("b").unwrap();
    h.world.exit(b, 1);
    h.orch.tick();
    finish_ok(&mut h, "d");
    merge(&mut h, "a").unwrap();
    assert_eq!(task_status(&mut h, "a"), "completed");
    assert_eq!(task_status(&mut h, "b"), "failed");
    assert_eq!(task_status(&mut h, "c"), "running");
    assert_eq!(task_status(&mut h, "d"), "completed");
    h
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter_map(|x| x.as_str().map(str::to_string))
        .collect()
}

#[test]
fn cleanup_removes_the_worktrees_of_merged_and_failed_tasks_only() {
    let mut h = mixed();
    let reply = h.call("cleanup", Value::Null).unwrap();
    assert_eq!(
        strings(reply.data(), "removed"),
        vec!["r1/a".to_string(), "r1/b".to_string()]
    );
    assert!(strings(reply.data(), "branches_deleted").is_empty());
    assert_eq!(
        h.world.removed_worktrees(),
        vec![(worktree_of("a"), false), (worktree_of("b"), false)],
        "branches are kept unless asked for"
    );
    assert_eq!(count(&h, "worktree_removed"), 2);
    assert!(is_null(&mut h, "a", "worktree"));
    assert!(is_null(&mut h, "b", "worktree"));
    assert!(
        !is_null(&mut h, "c", "worktree"),
        "a running task's worktree is never touched"
    );
    assert!(
        !is_null(&mut h, "d", "worktree"),
        "an unmerged completed task keeps its worktree"
    );
    assert!(
        h.world.saved_log("r1", "a").is_none(),
        "the stored output of a cleaned task is dropped"
    );
    assert!(h.world.saved_log("r1", "d").is_some());
    assert_eq!(task_status(&mut h, "c"), "running");
    assert!(h.world.session_for("c").is_some());
}

#[test]
fn cleanup_is_idempotent() {
    let mut h = mixed();
    h.call("cleanup", Value::Null).unwrap();
    let again = h.call("cleanup", Value::Null).unwrap();
    assert!(strings(again.data(), "removed").is_empty());
    assert_eq!(h.world.removed_worktrees().len(), 2);
    assert_eq!(count(&h, "worktree_removed"), 2);
}

#[test]
fn cleanup_deletes_merged_branches_only_when_asked_and_only_when_git_agrees() {
    let mut h = mixed();
    h.world.mark_merged("fleet/r1/b"); // git says so, but fleet never merged b
    let reply = h
        .call("cleanup", obj(vec![("branches", Value::Bool(true))]))
        .unwrap();
    assert_eq!(
        strings(reply.data(), "branches_deleted"),
        vec!["fleet/r1/a".to_string()]
    );
    assert_eq!(
        h.world.removed_worktrees(),
        vec![
            (worktree_of("a"), false),
            ("fleet/r1/a".to_string(), true),
            (worktree_of("b"), false)
        ],
        "a failed task's branch is never deleted, whatever git says"
    );
}

#[test]
fn cleanup_keeps_a_branch_that_git_does_not_report_as_merged() {
    let mut h = mixed();
    h.world.unmark_merged("fleet/r1/a");
    let reply = h
        .call("cleanup", obj(vec![("branches", Value::Bool(true))]))
        .unwrap();
    assert!(strings(reply.data(), "branches_deleted").is_empty());
    assert!(!h
        .world
        .removed_worktrees()
        .iter()
        .any(|(_, delete)| *delete));
    assert_eq!(
        strings(reply.data(), "removed"),
        vec!["r1/a".to_string(), "r1/b".to_string()]
    );

    // later, once git agrees, a second cleanup deletes the branch of the already cleaned task
    h.world.mark_merged("fleet/r1/a");
    let later = h
        .call("cleanup", obj(vec![("branches", Value::Bool(true))]))
        .unwrap();
    assert_eq!(
        strings(later.data(), "branches_deleted"),
        vec!["fleet/r1/a".to_string()]
    );
    assert!(strings(later.data(), "removed").is_empty());
    assert_eq!(
        h.world.removed_worktrees().last(),
        Some(&("fleet/r1/a".to_string(), true))
    );
}

#[test]
fn cleanup_without_branches_never_deletes_one() {
    let mut h = mixed();
    h.call("cleanup", obj(vec![("branches", Value::Bool(false))]))
        .unwrap();
    h.call("cleanup", Value::Null).unwrap();
    assert!(h
        .world
        .removed_worktrees()
        .iter()
        .all(|(_, delete)| !*delete));
}

#[test]
fn cleanup_can_be_limited_to_one_run() {
    let mut cfg = test_config();
    cfg.max_retries = 0;
    let mut h = Harness::with_config(cfg);
    for run in ["r1", "r2"] {
        h.call(
            "run.create",
            obj(vec![("objective", s("o")), ("repo", s("/repo"))]),
        )
        .unwrap();
        let task = if run == "r1" { "x1" } else { "x2" };
        h.call(
            "task.add",
            obj(vec![("run", s(run)), ("id", s(task)), ("title", s(task))]),
        )
        .unwrap();
        h.call("run.start", obj(vec![("run", s(run))])).unwrap();
        let id = h.world.session_for(task).unwrap();
        h.world.exit(id, 1);
        h.orch.tick();
    }
    assert_eq!(task_status(&mut h, "x1"), "failed");
    assert_eq!(task_status(&mut h, "x2"), "failed");
    let reply = h.call("cleanup", obj(vec![("run", s("r1"))])).unwrap();
    assert_eq!(strings(reply.data(), "removed"), vec!["r1/x1".to_string()]);
    assert_eq!(h.world.removed_worktrees().len(), 1);
    assert!(is_null(&mut h, "x1", "worktree"));
    assert!(!is_null(&mut h, "x2", "worktree"));
    let rest = h.call("cleanup", Value::Null).unwrap();
    assert_eq!(strings(rest.data(), "removed"), vec!["r2/x2".to_string()]);
}

#[test]
fn cleanup_skips_failed_tasks_that_never_had_a_worktree() {
    let mut h = Harness::new();
    new_run(&mut h);
    add(&mut h, "a", &[]);
    h.call("task.cancel", obj(vec![("task", s("a"))])).unwrap();
    assert_eq!(task_status(&mut h, "a"), "failed");
    let reply = h.call("cleanup", Value::Null).unwrap();
    assert!(strings(reply.data(), "removed").is_empty());
    assert!(h.world.removed_worktrees().is_empty());
    assert_eq!(count(&h, "worktree_removed"), 0);
}

#[test]
fn cleanup_records_worktree_removed_for_each_removed_task() {
    let mut h = mixed();
    h.call("cleanup", Value::Null).unwrap();
    let removed: Vec<String> = h
        .world
        .stored_events()
        .into_iter()
        .filter_map(|e| match e {
            DomainEvent::WorktreeRemoved { run, task } => Some(format!("{run}/{task}")),
            _ => None,
        })
        .collect();
    assert_eq!(removed, vec!["r1/a".to_string(), "r1/b".to_string()]);
}
