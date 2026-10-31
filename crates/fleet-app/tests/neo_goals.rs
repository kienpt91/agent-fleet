//! Application tests of the goal ledger: the commands, the read views, persistence through the
//! event log and consistency when the store fails, driven through the command interface with
//! in-memory fakes for every port.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_app::testing::{test_config, Harness};
use fleet_domain::error::ErrorCode;
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

/// A contract of command checks: `(id, mandatory)`.
fn contract(checks: &[(&str, bool)]) -> Value {
    obj(vec![
        ("version", Value::int(1)),
        (
            "checks",
            Value::array(checks.iter().map(|(id, mandatory)| {
                obj(vec![
                    ("id", s(id)),
                    ("kind", s("command")),
                    ("mandatory", Value::Bool(*mandatory)),
                    ("program", s("/usr/bin/make")),
                    ("args", list(&["test"])),
                ])
            })),
        ),
    ])
}

fn new_run(h: &mut Harness) {
    h.call(
        "run.create",
        obj(vec![("objective", s("ship")), ("repo", s("/repo"))]),
    )
    .unwrap();
}

fn add_task(h: &mut Harness, id: &str, after: &[&str]) {
    h.call(
        "task.add",
        obj(vec![
            ("run", s("r1")),
            ("id", s(id)),
            ("title", s(&format!("task {id}"))),
            ("after", list(after)),
        ]),
    )
    .unwrap();
}

fn goal_add(h: &mut Harness, pairs: Vec<(&str, Value)>) -> Value {
    h.call("goal.add", obj(pairs)).unwrap().data().clone()
}

fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap().to_string()
}

fn goals(h: &mut Harness) -> Vec<Value> {
    h.call("goal.list", Value::Null)
        .unwrap()
        .data()
        .get("goals")
        .and_then(Value::as_array)
        .unwrap()
        .to_vec()
}

fn goal(h: &mut Harness, id: &str) -> Value {
    h.call("goal.show", obj(vec![("goal", s(id))]))
        .unwrap()
        .data()
        .clone()
}

fn err_code(r: fleet_domain::error::Result<fleet_app::api::Reply>) -> ErrorCode {
    r.unwrap_err().code
}

#[test]
fn a_goal_is_added_with_a_contract_and_shown() {
    let mut h = Harness::new();
    new_run(&mut h);
    add_task(&mut h, "a", &[]);
    let reply = goal_add(
        &mut h,
        vec![
            ("id", s("ship")),
            ("title", s("Ship the feature")),
            ("intent", s("it works end to end")),
            ("tasks", list(&["r1/a"])),
            ("contract", contract(&[("tests", true), ("lint", false)])),
        ],
    );
    assert_eq!(text(&reply, "schema"), "fleet.goals.add/1");
    assert_eq!(text(&reply, "id"), "ship");
    assert_eq!(text(&reply, "status"), "open");
    assert_eq!(text(&reply, "contract_id").len(), 64);
    let kinds = h.world.event_kinds();
    assert!(
        kinds.ends_with(&["goal_added", "criteria_attached"]),
        "{kinds:?}"
    );

    let shown = goal(&mut h, "ship");
    assert_eq!(text(&shown, "title"), "Ship the feature");
    assert_eq!(text(&shown, "intent"), "it works end to end");
    assert_eq!(text(&shown, "contract"), "pending");
    let checks = shown.get("checks").and_then(Value::as_array).unwrap();
    assert_eq!(checks.len(), 2);
    assert_eq!(text(&checks[0], "check"), "tests");
    assert_eq!(checks[0].get("mandatory"), Some(&Value::Bool(true)));
    assert!(text(&checks[0], "detail").contains("/usr/bin/make test"));
    assert!(checks[0].get("latest").unwrap().is_null());
    let task = &shown.get("tasks").and_then(Value::as_array).unwrap()[0];
    assert_eq!(text(task, "task"), "r1/a");
    assert_eq!(text(task, "status"), "ready");
    assert_eq!(text(task, "verification"), "pending");
}

#[test]
fn goals_form_a_tree_and_the_list_keeps_parents_first() {
    let mut h = Harness::new();
    goal_add(&mut h, vec![("id", s("root")), ("title", s("Root"))]);
    goal_add(
        &mut h,
        vec![
            ("id", s("leaf")),
            ("parent", s("root")),
            ("title", s("Leaf")),
        ],
    );
    let auto = goal_add(&mut h, vec![("title", s("Second root"))]);
    assert_eq!(text(&auto, "id"), "goal-3");
    let rows = goals(&mut h);
    let ids: Vec<String> = rows.iter().map(|g| text(g, "id")).collect();
    assert_eq!(ids, ["root", "leaf", "goal-3"]);
    assert_eq!(rows[0].get("children").and_then(Value::as_u64), Some(1));
    assert_eq!(rows[1].get("depth").and_then(Value::as_u64), Some(2));
    assert_eq!(text(&rows[1], "parent"), "root");
    assert!(rows[0].get("parent").unwrap().is_null());
    let shown = goal(&mut h, "root");
    assert_eq!(
        shown.get("child_ids").and_then(Value::as_array).unwrap()[0].as_str(),
        Some("leaf")
    );
}

#[test]
fn adding_is_validated_and_nothing_is_recorded_on_failure() {
    let mut h = Harness::new();
    new_run(&mut h);
    add_task(&mut h, "a", &[]);
    goal_add(&mut h, vec![("id", s("g")), ("title", s("G"))]);
    let before = h.world.stored_events().len();
    let add = |h: &mut Harness, pairs: Vec<(&str, Value)>| err_code(h.call("goal.add", obj(pairs)));
    assert_eq!(
        add(&mut h, vec![("id", s("g")), ("title", s("dup"))]),
        ErrorCode::Conflict
    );
    assert_eq!(add(&mut h, vec![("title", s(""))]), ErrorCode::Invalid);
    assert_eq!(
        add(&mut h, vec![("title", s("x")), ("parent", s("nope"))]),
        ErrorCode::NotFound
    );
    assert_eq!(
        add(
            &mut h,
            vec![("title", s("x")), ("tasks", list(&["r1/zzz"]))]
        ),
        ErrorCode::NotFound
    );
    assert_eq!(
        add(&mut h, vec![("title", s("x")), ("tasks", list(&["ghost"]))]),
        ErrorCode::NotFound
    );
    assert_eq!(
        add(&mut h, vec![("title", s("x")), ("id", s("Bad Id"))]),
        ErrorCode::Invalid
    );
    assert_eq!(
        add(
            &mut h,
            vec![("title", s("x")), ("contract", s("not an object"))]
        ),
        ErrorCode::Invalid
    );
    assert_eq!(
        add(
            &mut h,
            vec![
                ("title", s("x")),
                ("contract", obj(vec![("version", Value::int(2))]))
            ]
        ),
        ErrorCode::Invalid
    );
    assert_eq!(
        add(
            &mut h,
            vec![("title", s("x")), ("contract", contract(&[("t", false)]))]
        ),
        ErrorCode::Invalid
    );
    assert_eq!(h.world.stored_events().len(), before);
    assert_eq!(goals(&mut h).len(), 1);
}

#[test]
fn a_bare_task_id_names_a_task_when_it_is_unique() {
    let mut h = Harness::new();
    new_run(&mut h);
    add_task(&mut h, "a", &[]);
    goal_add(
        &mut h,
        vec![("id", s("g")), ("title", s("G")), ("tasks", list(&["a"]))],
    );
    let shown = goal(&mut h, "g");
    let task = &shown.get("tasks").and_then(Value::as_array).unwrap()[0];
    assert_eq!(text(task, "task"), "r1/a");
}

#[test]
fn criteria_can_be_attached_later_and_replaced() {
    let mut h = Harness::new();
    goal_add(&mut h, vec![("id", s("g")), ("title", s("G"))]);
    let first = h
        .call(
            "goal.attach",
            obj(vec![
                ("goal", s("g")),
                ("contract", contract(&[("a", true)])),
            ]),
        )
        .unwrap();
    assert_eq!(text(first.data(), "schema"), "fleet.goals.attach/1");
    let id1 = text(first.data(), "contract_id");
    assert_eq!(text(&goal(&mut h, "g"), "contract_id"), id1);
    assert_eq!(
        err_code(h.call(
            "goal.attach",
            obj(vec![
                ("goal", s("g")),
                ("contract", contract(&[("a", true)]))
            ])
        )),
        ErrorCode::Conflict,
        "the same contract twice"
    );
    let second = h
        .call(
            "goal.attach",
            obj(vec![
                ("goal", s("g")),
                ("contract", contract(&[("a", true), ("b", true)])),
            ]),
        )
        .unwrap();
    assert_ne!(text(second.data(), "contract_id"), id1);
    assert_eq!(
        err_code(h.call(
            "goal.attach",
            obj(vec![
                ("goal", s("nope")),
                ("contract", contract(&[("a", true)]))
            ])
        )),
        ErrorCode::NotFound
    );
    assert_eq!(
        err_code(h.call(
            "goal.attach",
            obj(vec![("goal", s("g")), ("contract", s("x"))])
        )),
        ErrorCode::Invalid
    );
}

#[test]
fn status_changes_are_validated_and_verified_cannot_be_set() {
    let mut h = Harness::new();
    goal_add(&mut h, vec![("id", s("g")), ("title", s("G"))]);
    let r = h
        .call(
            "goal.status",
            obj(vec![("goal", s("g")), ("status", s("in_progress"))]),
        )
        .unwrap();
    assert_eq!(text(r.data(), "status"), "in_progress");
    h.call(
        "goal.status",
        obj(vec![
            ("goal", s("g")),
            ("status", s("blocked")),
            ("reason", s("waiting for the API key")),
        ]),
    )
    .unwrap();
    assert_eq!(
        text(&goal(&mut h, "g"), "reason"),
        "waiting for the API key"
    );
    let set = |h: &mut Harness, status: &str| {
        err_code(h.call(
            "goal.status",
            obj(vec![("goal", s("g")), ("status", s(status))]),
        ))
    };
    assert_eq!(set(&mut h, "verified"), ErrorCode::Forbidden);
    assert_eq!(set(&mut h, "blocked"), ErrorCode::Conflict);
    assert_eq!(set(&mut h, "finished"), ErrorCode::Invalid);
    assert_eq!(
        err_code(h.call(
            "goal.status",
            obj(vec![("goal", s("nope")), ("status", s("open"))])
        )),
        ErrorCode::NotFound
    );
}

#[test]
fn remaining_lists_open_leaves_blockers_and_the_tasks_that_can_start() {
    let mut h = Harness::new();
    new_run(&mut h);
    add_task(&mut h, "a", &[]);
    add_task(&mut h, "b", &["a"]);
    goal_add(&mut h, vec![("id", s("root")), ("title", s("Root"))]);
    goal_add(
        &mut h,
        vec![
            ("id", s("one")),
            ("parent", s("root")),
            ("title", s("One")),
            ("tasks", list(&["r1/a"])),
        ],
    );
    goal_add(
        &mut h,
        vec![
            ("id", s("two")),
            ("parent", s("root")),
            ("title", s("Two")),
            ("tasks", list(&["r1/b"])),
        ],
    );
    goal_add(
        &mut h,
        vec![
            ("id", s("stuck")),
            ("parent", s("root")),
            ("title", s("Stuck")),
        ],
    );
    h.call(
        "goal.status",
        obj(vec![
            ("goal", s("stuck")),
            ("status", s("blocked")),
            ("reason", s("needs a decision")),
        ]),
    )
    .unwrap();
    // task a is ready (promoted at start); b waits for a
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
    let r = h
        .call("goal.remaining", Value::Null)
        .unwrap()
        .data()
        .clone();
    assert_eq!(text(&r, "schema"), "fleet.goals.remaining/1");
    assert_eq!(r.get("total").and_then(Value::as_u64), Some(4));
    assert_eq!(r.get("verified").and_then(Value::as_u64), Some(0));
    let leaves: Vec<String> = r
        .get("open_leaves")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|g| text(g, "id"))
        .collect();
    assert_eq!(leaves, ["one", "two"]);
    let blockers = r.get("blockers").and_then(Value::as_array).unwrap();
    assert_eq!(blockers.len(), 1);
    assert_eq!(text(&blockers[0], "kind"), "goal");
    assert_eq!(text(&blockers[0], "goal"), "stuck");
    assert_eq!(text(&blockers[0], "detail"), "needs a decision");
    // the worker of `a` was dispatched at once (a free slot), so nothing is merely "ready"
    let next = r.get("next_ready").and_then(Value::as_array).unwrap();
    assert!(next.is_empty(), "{next:?}");
}

#[test]
fn ready_tasks_and_failed_tasks_show_up_in_remaining() {
    let mut cfg = test_config();
    cfg.max_sessions = 1;
    cfg.max_retries = 0;
    let mut h = Harness::with_config(cfg);
    new_run(&mut h);
    for t in ["a", "b", "c"] {
        add_task(&mut h, t, &[]);
        goal_add(
            &mut h,
            vec![
                ("id", s(&format!("g{t}"))),
                ("title", s(&format!("Goal {t}"))),
                ("tasks", list(&[&format!("r1/{t}")])),
            ],
        );
    }
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
    let r = h
        .call("goal.remaining", Value::Null)
        .unwrap()
        .data()
        .clone();
    let next: Vec<String> = r
        .get("next_ready")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|t| text(t, "task"))
        .collect();
    assert_eq!(
        next,
        ["r1/b", "r1/c"],
        "a runs, the others wait for the one slot"
    );
    // a's worker fails and no retry is left: a is a blocker, b takes the slot, c still waits
    let session = h.world.session_for("a").unwrap();
    h.world.exit(session, 1);
    h.orch.tick();
    let r = h
        .call("goal.remaining", Value::Null)
        .unwrap()
        .data()
        .clone();
    let blockers = r.get("blockers").and_then(Value::as_array).unwrap();
    assert_eq!(blockers.len(), 1);
    assert_eq!(text(&blockers[0], "kind"), "task");
    assert_eq!(text(&blockers[0], "task"), "r1/a");
    assert_eq!(text(&blockers[0], "goal"), "ga");
    assert_eq!(text(&blockers[0], "status"), "failed");
    let next = r.get("next_ready").and_then(Value::as_array).unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(text(&next[0], "task"), "r1/c");
}

#[test]
fn goals_and_contracts_survive_a_restart() {
    let mut h = Harness::new();
    new_run(&mut h);
    add_task(&mut h, "a", &[]);
    goal_add(
        &mut h,
        vec![
            ("id", s("g")),
            ("title", s("G")),
            ("tasks", list(&["r1/a"])),
            ("contract", contract(&[("tests", true)])),
        ],
    );
    h.call(
        "goal.status",
        obj(vec![("goal", s("g")), ("status", s("in_progress"))]),
    )
    .unwrap();
    let before = goal(&mut h, "g");
    h.restart(test_config()).unwrap();
    let after = goal(&mut h, "g");
    assert_eq!(before, after);
    // ids keep counting after the replay
    let next = goal_add(&mut h, vec![("title", s("next"))]);
    assert_eq!(text(&next, "id"), "goal-2");
}

#[test]
fn a_store_failure_changes_nothing() {
    let mut h = Harness::new();
    goal_add(&mut h, vec![("id", s("g")), ("title", s("G"))]);
    h.world.fail_appends(true);
    assert!(h
        .call("goal.add", obj(vec![("id", s("h")), ("title", s("H"))]))
        .is_err());
    assert!(h
        .call(
            "goal.status",
            obj(vec![("goal", s("g")), ("status", s("blocked"))])
        )
        .is_err());
    assert!(h
        .call(
            "goal.attach",
            obj(vec![
                ("goal", s("g")),
                ("contract", contract(&[("a", true)]))
            ])
        )
        .is_err());
    h.world.fail_appends(false);
    let rows = goals(&mut h);
    assert_eq!(rows.len(), 1);
    assert_eq!(text(&rows[0], "status"), "open");
    assert!(rows[0].get("contract_id").unwrap().is_null());
}

#[test]
fn hostile_text_is_kept_as_data_and_bounded() {
    let mut h = Harness::new();
    let hostile = "\u{1b}]0;pwned\u{7}";
    assert_eq!(
        err_code(h.call("goal.add", obj(vec![("title", s(hostile))]))),
        ErrorCode::Invalid,
        "control characters never enter the ledger"
    );
    assert_eq!(
        err_code(h.call(
            "goal.add",
            obj(vec![("title", s("ok")), ("intent", s(hostile))])
        )),
        ErrorCode::Invalid
    );
    let long = "x".repeat(200_000);
    assert_eq!(
        err_code(h.call("goal.add", obj(vec![("title", s(&long))]))),
        ErrorCode::Invalid
    );
    assert!(goals(&mut h).is_empty());
}

#[test]
fn unknown_goals_are_not_found_by_the_read_commands() {
    let mut h = Harness::new();
    assert_eq!(
        err_code(h.call("goal.show", obj(vec![("goal", s("nope"))]))),
        ErrorCode::NotFound
    );
    assert_eq!(
        err_code(h.call("goal.show", obj(vec![("goal", s("../x"))]))),
        ErrorCode::Invalid
    );
    let empty = h.call("goal.list", Value::Null).unwrap().data().clone();
    assert_eq!(empty.get("total").and_then(Value::as_u64), Some(0));
    let r = h
        .call("goal.remaining", Value::Null)
        .unwrap()
        .data()
        .clone();
    assert!(r
        .get("open_leaves")
        .and_then(Value::as_array)
        .unwrap()
        .is_empty());
}
