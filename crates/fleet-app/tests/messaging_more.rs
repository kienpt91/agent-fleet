//! More application tests of the message channels: refused files and floods, the operator's
//! commands and their errors, plans that add channels to a run, approvals that are denied or lapse,
//! and the adapters failing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_app::ports::OutboxRead;
use fleet_app::testing::Harness;
use fleet_domain::error::ErrorCode;
use fleet_domain::status::{BlockReason, MessageKind};
use fleet_domain::value::Value;

fn s(x: &str) -> Value {
    Value::str(x)
}

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    Value::object(pairs)
}

fn channel(name: &str, extra: Vec<(&str, Value)>) -> Value {
    let mut pairs = vec![
        ("name", s(name)),
        ("members", Value::array([s("task:a"), s("task:b")])),
    ];
    pairs.extend(extra);
    obj(pairs)
}

fn started(channels: Vec<Value>) -> (Harness, String, String) {
    let mut h = Harness::new();
    let task = |id: &str| obj(vec![("id", s(id)), ("title", s(id)), ("spec", s("x"))]);
    h.call(
        "plan.load",
        obj(vec![
            (
                "new_run",
                obj(vec![("objective", s("o")), ("repo", s("/repo"))]),
            ),
            ("tasks", Value::array([task("a"), task("b")])),
            ("channels", Value::array(channels)),
        ]),
    )
    .unwrap();
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
    h.orch.tick();
    let wt = |h: &Harness, t: &str| {
        h.world
            .session_spec(h.world.session_for(t).unwrap())
            .unwrap()
            .cwd
    };
    let (a, b) = (wt(&h, "a"), wt(&h, "b"));
    (h, a, b)
}

fn list(h: &mut Harness) -> Value {
    h.call("msg.list", Value::Null).unwrap().data().clone()
}

fn arr(v: &Value, k: &str) -> Vec<Value> {
    v.get(k).and_then(Value::as_array).unwrap().to_vec()
}

fn text(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

#[test]
fn refused_files_are_recorded_and_a_flood_mutes_the_task_and_opens_a_gate() {
    let (mut h, a, _b) = started(vec![channel("talk", vec![])]);
    for n in 1..=30 {
        h.world.write_outbox(
            &a,
            &format!("{n}.json"),
            OutboxRead::Invalid(BlockReason::Invalid, "not a message".to_string()),
        );
    }
    for _ in 0..6 {
        h.orch.tick();
    }
    let doc = list(&mut h);
    let muted = arr(&doc, "muted_tasks");
    assert_eq!(muted.len(), 1, "{doc:?}");
    assert_eq!(text(&muted[0], "reason"), "flood");
    let gates = h.call("gate.list", Value::Null).unwrap();
    let gates = arr(gates.data(), "gates");
    assert_eq!(gates.len(), 1);
    assert_eq!(text(&gates[0], "kind"), "channel");
    assert!(text(&gates[0], "text").contains("refused messages"));
    // a muted task's outbox is no longer read: files pile up untouched
    let left = h.world.outbox_names(&a).len();
    h.orch.tick();
    h.orch.tick();
    assert_eq!(h.world.outbox_names(&a).len(), left);
    // unmuting the task by hand answers the gate
    h.call("msg.unmute", obj(vec![("target", s("a"))])).unwrap();
    let gates = h.call("gate.list", Value::Null).unwrap();
    assert_eq!(
        gates.data().get("gates").and_then(Value::as_array).unwrap()[0].get("open"),
        Some(&Value::Bool(false))
    );
    assert!(arr(&list(&mut h), "muted_tasks").is_empty());
}

#[test]
fn an_unreadable_outbox_is_reported_once_and_a_failed_rename_does_not_duplicate() {
    let (mut h, a, b) = started(vec![channel("talk", vec![])]);
    h.world.refuse_outbox(&a, "the outbox is a link");
    h.orch.tick();
    h.orch.tick();
    assert_eq!(
        h.world
            .event_kinds()
            .iter()
            .filter(|k| **k == "warning")
            .count(),
        1
    );
    // a file whose rename fails is read again, and is still one message
    let (mut h2, a2, b2) = started(vec![channel("talk", vec![])]);
    let _ = (b, h);
    h2.world.fail_settle(true);
    h2.world.write_message(
        &a2,
        "1.json",
        "talk",
        Some("b"),
        MessageKind::Info,
        "once",
        "only once",
    );
    for _ in 0..4 {
        h2.orch.tick();
    }
    let submitted = h2
        .world
        .event_kinds()
        .iter()
        .filter(|k| **k == "message_submitted")
        .count();
    assert_eq!(submitted, 1);
    assert_eq!(h2.world.inbox(&b2).len(), 1);
}

#[test]
fn the_operators_commands_report_their_errors() {
    let (mut h, a, _b) = started(vec![channel("talk", vec![])]);
    h.world.write_message(
        &a,
        "1.json",
        "talk",
        Some("b"),
        MessageKind::Question,
        "hello",
        "the body",
    );
    h.orch.tick();
    h.orch.tick();
    let shown = h.call("msg.show", obj(vec![("id", s("m1"))])).unwrap();
    let m = shown.data().get("message").unwrap();
    assert_eq!(text(m, "body"), "the body");
    assert_eq!(text(m, "label"), "from task a (unverified peer message)");
    assert_eq!(
        h.call("msg.show", obj(vec![("id", s("m99"))]))
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert_eq!(
        h.call("msg.list", obj(vec![("channel", s("nosuch"))]))
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    // filters: by channel, by task, by limit
    let filtered = h
        .call(
            "msg.list",
            obj(vec![
                ("channel", s("r1/talk")),
                ("task", s("b")),
                ("limit", Value::int(1)),
            ]),
        )
        .unwrap();
    assert_eq!(arr(filtered.data(), "messages").len(), 1);
    // muting what does not exist, or muting twice
    assert_eq!(
        h.call("msg.mute", obj(vec![("target", s("nothing"))]))
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    h.call("msg.mute", obj(vec![("target", s("talk"))]))
        .unwrap();
    assert_eq!(
        h.call("msg.mute", obj(vec![("target", s("talk"))]))
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    h.call("msg.unmute", obj(vec![("target", s("talk"))]))
        .unwrap();
    assert_eq!(
        h.call("msg.unmute", obj(vec![("target", s("talk"))]))
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    // a direct channel needs a recipient; an unknown channel is not found
    let no_to = h
        .call(
            "msg.send",
            obj(vec![
                ("channel", s("talk")),
                ("subject", s("x")),
                ("body", s("y")),
            ]),
        )
        .unwrap_err();
    assert_eq!(no_to.code, ErrorCode::Invalid);
    assert!(no_to.message.contains("bad-recipient"), "{}", no_to.message);
    assert_eq!(
        h.call(
            "msg.send",
            obj(vec![
                ("channel", s("none")),
                ("subject", s("x")),
                ("body", s("y"))
            ])
        )
        .unwrap_err()
        .code,
        ErrorCode::NotFound
    );
    assert!(h
        .call(
            "msg.send",
            obj(vec![
                ("channel", s("talk")),
                ("to", s("b")),
                ("kind", s("approval")),
                ("subject", s("x")),
                ("body", s("y"))
            ])
        )
        .is_err());
}

#[test]
fn a_name_that_is_both_a_channel_and_a_task_must_be_written_with_its_kind() {
    let (mut h, _a, _b) = started(vec![channel("b", vec![])]);
    let err = h
        .call("msg.mute", obj(vec![("target", s("b"))]))
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Invalid);
    assert!(
        err.message.contains("channel:") && err.message.contains("task:"),
        "{}",
        err.message
    );
    let r = h
        .call("msg.mute", obj(vec![("target", s("task:b"))]))
        .unwrap();
    assert_eq!(text(r.data(), "kind"), "task");
    let r = h
        .call("msg.mute", obj(vec![("target", s("channel:b"))]))
        .unwrap();
    assert_eq!(text(r.data(), "kind"), "channel");
}

#[test]
fn a_plan_can_add_channels_to_an_existing_run_but_not_twice() {
    let (mut h, _a, _b) = started(vec![]);
    let plan = |name: &str, id: &str| {
        obj(vec![
            ("run", s("r1")),
            (
                "tasks",
                Value::array([obj(vec![("id", s(id)), ("title", s(id))])]),
            ),
            ("channels", Value::array([channel(name, vec![])])),
        ])
    };
    let r = h.call("plan.load", plan("late", "c")).unwrap();
    assert_eq!(arr(r.data(), "channels").len(), 1);
    let err = h.call("plan.load", plan("late", "d")).unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert_eq!(arr(&list(&mut h), "channels").len(), 1);
}

#[test]
fn a_denied_approval_blocks_the_message_and_an_unanswered_one_expires() {
    let (mut h, a, b) = started(vec![channel(
        "talk",
        vec![
            ("requires_approval", Value::Bool(true)),
            ("ttl_secs", Value::int(60)),
        ],
    )]);
    h.world.write_message(
        &a,
        "1.json",
        "talk",
        Some("b"),
        MessageKind::Request,
        "one",
        "first",
    );
    h.world.write_message(
        &a,
        "2.json",
        "talk",
        Some("b"),
        MessageKind::Request,
        "two",
        "second",
    );
    h.orch.tick();
    h.orch.tick();
    let approvals = h.call("approval.list", Value::Null).unwrap();
    let approvals = arr(approvals.data(), "approvals");
    assert_eq!(approvals.len(), 2);
    let digest = text(&approvals[0], "digest");
    h.call(
        "approval.decide",
        obj(vec![
            ("id", s("a1")),
            ("approve", Value::Bool(false)),
            ("digest", s(&digest)),
        ]),
    )
    .unwrap();
    h.orch.tick();
    h.world.advance(120_000);
    h.orch.tick();
    h.orch.tick();
    assert!(h.world.inbox(&b).is_empty());
    let messages = arr(&list(&mut h), "messages");
    let by = |id: &str| {
        messages
            .iter()
            .find(|m| text(m, "id") == id)
            .unwrap()
            .clone()
    };
    assert_eq!(text(&by("m1"), "status"), "blocked");
    assert_eq!(text(&by("m1"), "reason"), "denied");
    assert_eq!(text(&by("m2"), "status"), "expired");
}

#[test]
fn a_task_whose_message_directories_cannot_be_made_fails_its_attempt_not_the_daemon() {
    let mut h = Harness::new();
    h.world.fail_prepare_mail(true);
    let task = |id: &str| obj(vec![("id", s(id)), ("title", s(id))]);
    h.call(
        "plan.load",
        obj(vec![
            (
                "new_run",
                obj(vec![("objective", s("o")), ("repo", s("/repo"))]),
            ),
            ("tasks", Value::array([task("a"), task("b")])),
            ("channels", Value::array([channel("talk", vec![])])),
        ]),
    )
    .unwrap();
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
    h.orch.tick();
    assert!(h.world.session_for("a").is_none());
    let kinds = h.world.event_kinds();
    assert!(kinds.contains(&"session_ended"), "{kinds:?}");
    assert!(kinds.contains(&"retry_queued") || kinds.contains(&"task_failed"));
}

#[test]
fn a_channel_with_nobody_left_is_muted_by_fleet_and_says_why() {
    let (mut h, _a, _b) = started(vec![channel("talk", vec![])]);
    let b = h.world.session_for("b").unwrap();
    h.world.exit(b, 1);
    // no retries left makes b fail for good
    for _ in 0..10 {
        h.orch.tick();
        if let Some(id) = h.world.session_for("b") {
            h.world.exit(id, 1);
        }
    }
    let doc = list(&mut h);
    let c = &arr(&doc, "channels")[0];
    assert_eq!(c.get("muted"), Some(&Value::Bool(true)));
    assert_eq!(text(c, "mute_reason"), "endpoint-ended");
    assert_eq!(text(c, "mute_by"), "auto");
    assert!(arr(&doc, "messages").is_empty());
}
