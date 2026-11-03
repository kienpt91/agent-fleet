//! Adversarial scenarios from the security review of the message channels, as regression tests at
//! the application layer: a refused-message flood must be bounded, one member must not be able to
//! mute a channel for the others, pending approvals must not starve the decision pool, a reply must
//! name a message the sender received, and the global caps must not let one run starve another.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_app::ports::OutboxRead;
use fleet_app::testing::{test_config, Harness};
use fleet_domain::contract::{ContractRead, QuestionKind, WorkerQuestion};
use fleet_domain::ids::{ChannelName, MessageId, TaskId};
use fleet_domain::msg::message::OutboxMessage;
use fleet_domain::status::{BlockReason, MessageKind};
use fleet_domain::value::Value;

fn s(x: &str) -> Value {
    Value::str(x)
}

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    Value::object(pairs)
}

fn channel(name: &str, members: &[&str], extra: Vec<(&str, Value)>) -> Value {
    let mut pairs = vec![
        ("name", s(name)),
        (
            "members",
            Value::array(members.iter().map(|m| s(&format!("task:{m}")))),
        ),
        ("max_messages_per_task", Value::int(200)),
        ("max_messages_per_minute", Value::int(120)),
    ];
    pairs.extend(extra);
    obj(pairs)
}

/// A run `run` with the given tasks and channels, started, every task running.
fn fleet(
    h: &mut Harness,
    run: &str,
    tasks: &[&str],
    channels: Vec<Value>,
) -> std::collections::BTreeMap<String, String> {
    let task = |id: &str| obj(vec![("id", s(id)), ("title", s(id)), ("spec", s("x"))]);
    h.call(
        "plan.load",
        obj(vec![
            (
                "new_run",
                obj(vec![
                    ("id", s(run)),
                    ("objective", s("o")),
                    ("repo", s("/repo")),
                ]),
            ),
            ("tasks", Value::array(tasks.iter().map(|t| task(t)))),
            ("channels", Value::array(channels)),
        ]),
    )
    .unwrap();
    h.call("run.start", obj(vec![("run", s(run))])).unwrap();
    h.orch.tick();
    tasks
        .iter()
        .map(|t| {
            let id = h
                .world
                .sessions_of_run(run)
                .into_iter()
                .find(|(task, _)| task == t)
                .map(|(_, id)| id)
                .unwrap_or_else(|| panic!("{run}/{t} is not running"));
            (t.to_string(), h.world.session_spec(id).unwrap().cwd)
        })
        .collect()
}

fn permissive() -> Harness {
    let mut cfg = test_config();
    cfg.max_sessions = 16;
    cfg.messaging.max_per_task_per_minute = 120;
    cfg.messaging.max_total_messages = 500;
    Harness::with_config(cfg)
}

fn list(h: &mut Harness) -> Value {
    h.call("msg.list", obj(vec![("limit", Value::int(500))]))
        .unwrap()
        .data()
        .clone()
}

fn arr(v: &Value, k: &str) -> Vec<Value> {
    v.get(k).and_then(Value::as_array).unwrap().to_vec()
}

fn text(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn count(h: &Harness, kind: &str) -> usize {
    h.world.event_kinds().iter().filter(|k| **k == kind).count()
}

fn send(h: &Harness, wt: &str, name: &str, to: &str, body: &str) {
    h.world
        .write_message(wt, name, "talk", Some(to), MessageKind::Info, "s", body);
}

fn reply(h: &Harness, wt: &str, name: &str, to: &str, parent: &str, body: &str) {
    h.world.write_outbox(
        wt,
        name,
        OutboxRead::Valid(OutboxMessage {
            channel: ChannelName::parse("talk").unwrap(),
            to: Some(TaskId::parse(to).unwrap()),
            kind: MessageKind::Info,
            subject: "re".to_string(),
            body: body.to_string(),
            in_reply_to: Some(MessageId::parse(parent).unwrap()),
        }),
    );
}

fn ticks(h: &mut Harness, n: usize) {
    for _ in 0..n {
        h.orch.tick();
    }
}

// ---- MSG-DOS-LOG-01 ----------------------------------------------------------------------------

#[test]
fn invalid_files_below_the_per_minute_threshold_cannot_fill_the_log() {
    let mut h = permissive();
    let wt = fleet(
        &mut h,
        "r1",
        &["a", "b"],
        vec![channel("talk", &["a", "b"], vec![])],
    );
    let a = &wt["a"];
    let mut file = 0;
    // 19 hostile files a minute (the old per-minute threshold is 20), for two hours
    for _minute in 0..120 {
        for _ in 0..19 {
            file += 1;
            h.world.write_outbox(
                a,
                &format!("{file}.json"),
                OutboxRead::Invalid(BlockReason::Invalid, "not a message".to_string()),
            );
        }
        ticks(&mut h, 3);
        h.world.advance(61_000);
    }
    let blocked = count(&h, "message_blocked");
    assert!(
        blocked <= 130,
        "a task that writes {file} invalid files adds {blocked} events to the log"
    );
    let doc = list(&mut h);
    let muted = arr(&doc, "muted_tasks");
    assert_eq!(
        muted.len(),
        1,
        "the task ran out of its refusal budget: {doc:?}"
    );
    assert_eq!(text(&muted[0], "reason"), "refusal-budget");
    // once muted its outbox is not read and nothing more is recorded
    let before = h.world.stored_events().len();
    for n in 0..20 {
        h.world.write_outbox(
            a,
            &format!("{}.json", file + n + 1),
            OutboxRead::Invalid(BlockReason::Invalid, "more".to_string()),
        );
    }
    ticks(&mut h, 5);
    assert_eq!(h.world.stored_events().len(), before);
}

#[test]
fn secret_shaped_messages_below_the_per_minute_threshold_also_run_out_of_budget() {
    let mut h = permissive();
    let wt = fleet(
        &mut h,
        "r1",
        &["a", "b"],
        vec![channel("talk", &["a", "b"], vec![])],
    );
    let key = format!("{}{}", "ghp_", "z".repeat(36));
    let mut file = 0;
    for _minute in 0..30 {
        for _ in 0..15 {
            file += 1;
            send(
                &h,
                &wt["a"],
                &format!("{file}.json"),
                "b",
                &format!("{key} {file}"),
            );
        }
        ticks(&mut h, 3);
        h.world.advance(61_000);
    }
    assert!(
        count(&h, "message_blocked") <= 130,
        "{}",
        count(&h, "message_blocked")
    );
    assert_eq!(arr(&list(&mut h), "muted_tasks").len(), 1);
}

// ---- MSG-GRIEF-MUTE-01 -------------------------------------------------------------------------

#[test]
fn one_way_traffic_from_one_member_cannot_mute_a_shared_channel() {
    let mut h = permissive();
    let wt = fleet(
        &mut h,
        "r1",
        &["a", "b", "c"],
        vec![channel("talk", &["a", "b", "c"], vec![])],
    );
    for n in 1..=30 {
        send(
            &h,
            &wt["a"],
            &format!("{n}.json"),
            "b",
            &format!("one-way message {n}"),
        );
        ticks(&mut h, 1);
    }
    let doc = list(&mut h);
    let ch = &arr(&doc, "channels")[0];
    assert_eq!(ch.get("muted"), Some(&Value::Bool(false)), "{ch:?}");
    assert!(arr(&doc, "muted_tasks").is_empty());
    // the honest members still talk
    send(&h, &wt["b"], "1.json", "c", "hello from b");
    ticks(&mut h, 2);
    assert_eq!(h.world.inbox(&wt["c"]).len(), 1);
}

#[test]
fn a_chain_of_replies_to_ones_own_messages_cannot_mute_a_shared_channel() {
    let mut h = permissive();
    let wt = fleet(
        &mut h,
        "r1",
        &["a", "b", "c"],
        vec![channel("talk", &["a", "b", "c"], vec![])],
    );
    send(&h, &wt["a"], "1.json", "b", "start");
    ticks(&mut h, 2);
    for n in 2..=10 {
        reply(
            &h,
            &wt["a"],
            &format!("{n}.json"),
            "b",
            &format!("m{}", n - 1),
            &format!("self reply {n}"),
        );
        ticks(&mut h, 2);
    }
    let doc = list(&mut h);
    assert_eq!(
        arr(&doc, "channels")[0].get("muted"),
        Some(&Value::Bool(false)),
        "{doc:?}"
    );
    assert!(arr(&doc, "muted_tasks").is_empty());
    assert!(
        arr(&doc, "messages")
            .iter()
            .any(|m| text(m, "reason") == "unknown-reply"),
        "a reply to one's own message is refused: {doc:?}"
    );
}

#[test]
fn a_real_ping_pong_mutes_only_the_offender_while_others_can_still_talk() {
    let mut cfg = test_config();
    cfg.max_sessions = 16;
    cfg.messaging.max_reply_depth = 3;
    cfg.messaging.max_pair_exchanges = 50;
    cfg.messaging.max_per_task_per_minute = 120;
    let mut h = Harness::with_config(cfg);
    let wt = fleet(
        &mut h,
        "r1",
        &["a", "b", "c"],
        vec![channel("talk", &["a", "b", "c"], vec![])],
    );
    for i in 0..8 {
        let (from, to) = if i % 2 == 0 { ("a", "b") } else { ("b", "a") };
        send(
            &h,
            &wt[from],
            &format!("{}.json", i + 1),
            to,
            &format!("round {i}"),
        );
        ticks(&mut h, 2);
    }
    let doc = list(&mut h);
    assert_eq!(
        arr(&doc, "channels")[0].get("muted"),
        Some(&Value::Bool(false)),
        "the channel has a healthy third member: only the sender that crossed the limit is muted: {doc:?}"
    );
    let muted = arr(&doc, "muted_tasks");
    assert_eq!(muted.len(), 1, "{doc:?}");
    assert_eq!(text(&muted[0], "channel"), "talk");
    assert_eq!(text(&muted[0], "reason"), "reply-depth");
    // a person is asked, and the third member keeps working
    assert_eq!(
        arr(h.call("gate.list", Value::Null).unwrap().data(), "gates").len(),
        1
    );
    send(&h, &wt["c"], "1.json", "b", "c is fine");
    ticks(&mut h, 2);
    assert!(h
        .world
        .inbox(&wt["b"])
        .iter()
        .any(|i| i.content.contains("c is fine")));
    // answering the gate lifts the mute
    h.call(
        "gate.resolve",
        obj(vec![("id", s("g1")), ("answer", s("unmute"))]),
    )
    .unwrap();
    assert!(arr(&list(&mut h), "muted_tasks").is_empty());
}

#[test]
fn hopping_between_channels_does_not_reset_a_ping_pong() {
    let mut cfg = test_config();
    cfg.max_sessions = 16;
    cfg.messaging.max_reply_depth = 3;
    cfg.messaging.max_pair_exchanges = 50;
    cfg.messaging.max_per_task_per_minute = 120;
    let mut h = Harness::with_config(cfg);
    let wt = fleet(
        &mut h,
        "r1",
        &["a", "b"],
        vec![
            channel("talk", &["a", "b"], vec![]),
            channel("other", &["a", "b"], vec![]),
        ],
    );
    for i in 0..10 {
        let (from, to) = if i % 2 == 0 { ("a", "b") } else { ("b", "a") };
        let name = if i % 2 == 0 { "talk" } else { "other" };
        h.world.write_message(
            &wt[from],
            &format!("{}.json", i + 1),
            name,
            Some(to),
            MessageKind::Info,
            "s",
            &format!("hop {i}"),
        );
        ticks(&mut h, 2);
    }
    let blocked = arr(&list(&mut h), "messages")
        .iter()
        .filter(|m| text(m, "status") == "blocked")
        .count();
    assert!(blocked >= 1, "the chain continues across channels");
}

// ---- MSG-DOS-DECISIONS-01 ----------------------------------------------------------------------

#[test]
fn pending_message_approvals_cannot_take_the_whole_decision_pool() {
    let mut h = permissive();
    let wt = fleet(
        &mut h,
        "r1",
        &["a", "b"],
        vec![channel(
            "talk",
            &["a", "b"],
            vec![("requires_approval", Value::Bool(true))],
        )],
    );
    for n in 1..=60 {
        send(
            &h,
            &wt["a"],
            &format!("{n}.json"),
            "b",
            &format!("please approve {n}"),
        );
        ticks(&mut h, 1);
    }
    let approvals = h.call("approval.list", Value::Null).unwrap();
    let pending = arr(approvals.data(), "approvals")
        .iter()
        .filter(|a| text(a, "status") == "pending")
        .count();
    assert!(
        pending <= 32,
        "{pending} approvals wait for a person on one channel"
    );
    let refused = arr(&list(&mut h), "messages")
        .iter()
        .filter(|m| text(m, "status") == "blocked" && text(m, "reason") == "quota")
        .count();
    assert!(refused > 0, "the rest was refused, not recorded as pending");
    // a worker's own question still reaches a person while the channel's approvals are full
    let dir = wt["b"].clone();
    h.world.write_question(
        &dir,
        ContractRead::Valid(WorkerQuestion {
            kind: QuestionKind::Question,
            text: "which database?".to_string(),
            choices: vec!["sqlite".to_string(), "postgres".to_string()],
            action: None,
        }),
    );
    let id = h.world.session_for("b").unwrap();
    h.world.exit(id, 0);
    ticks(&mut h, 2);
    let gates = h.call("gate.list", Value::Null).unwrap();
    assert!(
        arr(gates.data(), "gates")
            .iter()
            .any(|g| text(g, "kind") == "question" && text(g, "status") != "closed"),
        "{gates:?}"
    );
}

// ---- MSG-INFO-REPLY-01 -------------------------------------------------------------------------

#[test]
fn a_reply_must_name_a_message_the_sender_received() {
    let mut h = permissive();
    let wt = fleet(
        &mut h,
        "r1",
        &["a", "b", "c"],
        vec![channel("talk", &["a", "b", "c"], vec![])],
    );
    send(&h, &wt["a"], "1.json", "b", "for b only");
    ticks(&mut h, 2);
    // c was not a party: it may not attach to the message (nor learn that it exists)
    reply(&h, &wt["c"], "1.json", "a", "m1", "eavesdropping");
    // a replying to its own message is not an exchange
    reply(&h, &wt["a"], "2.json", "b", "m1", "talking to myself");
    // b received it and may answer
    reply(&h, &wt["b"], "1.json", "a", "m1", "an answer");
    ticks(&mut h, 3);
    let messages = arr(&list(&mut h), "messages");
    let status_of = |body_from: &str| {
        messages
            .iter()
            .find(|m| {
                text(m, "from") == format!("task:{body_from}")
                    && text(m, "kind") == "info"
                    && text(m, "subject") != "s"
            })
            .map(|m| (text(m, "status"), text(m, "reason")))
    };
    assert_eq!(
        status_of("c"),
        Some(("blocked".to_string(), "unknown-reply".to_string()))
    );
    assert_eq!(status_of("b").map(|x| x.0), Some("delivered".to_string()));
    // the reply a wrote to its own message is refused too (a's first message was accepted)
    let own = messages
        .iter()
        .filter(|m| text(m, "from") == "task:a" && text(m, "status") == "blocked")
        .count();
    assert_eq!(own, 1, "{messages:?}");
}

// ---- MSG-BUDGET-01 -----------------------------------------------------------------------------

#[test]
fn one_run_cannot_use_up_the_message_cap_of_another() {
    let mut cfg = test_config();
    cfg.max_sessions = 16;
    cfg.messaging.max_total_messages = 3;
    cfg.messaging.max_per_task_per_minute = 120;
    let mut h = Harness::with_config(cfg);
    let r1 = fleet(
        &mut h,
        "r1",
        &["a", "b"],
        vec![channel("talk", &["a", "b"], vec![])],
    );
    let r2 = fleet(
        &mut h,
        "r2",
        &["a", "b"],
        vec![channel("talk", &["a", "b"], vec![])],
    );
    for n in 1..=5 {
        send(
            &h,
            &r1["a"],
            &format!("{n}.json"),
            "b",
            &format!("run one {n}"),
        );
        ticks(&mut h, 1);
    }
    send(&h, &r2["a"], "1.json", "b", "run two");
    ticks(&mut h, 2);
    assert_eq!(h.world.inbox(&r1["b"]).len(), 3, "run one used its three");
    assert_eq!(
        h.world.inbox(&r2["b"]).len(),
        1,
        "run two is not starved by run one"
    );
}

#[test]
fn one_sender_cannot_fill_a_recipients_inbox() {
    let mut h = permissive();
    let wt = fleet(
        &mut h,
        "r1",
        &["a", "b", "c"],
        vec![channel("talk", &["a", "b", "c"], vec![])],
    );
    for n in 1..=90 {
        send(
            &h,
            &wt["a"],
            &format!("{n}.json"),
            "b",
            &format!("flood {n}"),
        );
        ticks(&mut h, 1);
        h.world.advance(7_000);
    }
    ticks(&mut h, 10);
    let from_a = h
        .world
        .inbox(&wt["b"])
        .iter()
        .filter(|i| i.content.contains("\"from\":\"task:a\""))
        .count();
    assert!(
        from_a <= 64,
        "{from_a} of one sender's messages sit in one inbox"
    );
    // an honest member's message still arrives
    send(&h, &wt["c"], "1.json", "b", "from c");
    ticks(&mut h, 3);
    assert!(h
        .world
        .inbox(&wt["b"])
        .iter()
        .any(|i| i.content.contains("from c")));
}
