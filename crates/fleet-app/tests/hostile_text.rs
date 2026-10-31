//! Application tests for hostile text. Everything the web dashboard shows is one of the neutral
//! documents of the command interface (`status`, `gate.list`, `approval.list`, `events`,
//! `task.tail`), and the page renders them with `textContent`. These tests pin what that relies on:
//! control and bidi characters cannot be stored, ordinary markup is returned untouched (neither
//! mangled nor escaped twice), and text a worker produced is sanitized before it reaches a document.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_app::testing::{test_config, Harness, ToyDigest};
use fleet_domain::approval::Action;
use fleet_domain::contract::{
    ActionRequest, ContractRead, QuestionKind, WorkerQuestion, WorkerResult,
};
use fleet_domain::error::{ErrorCode, FleetError};
use fleet_domain::ids::TaskId;
use fleet_domain::output::OutputBuffer;
use fleet_domain::redact::Redactor;
use fleet_domain::value::Value;
use std::sync::Arc;

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    Value::object(pairs)
}

fn s(x: &str) -> Value {
    Value::str(x)
}

/// Single-line text that must be refused, with a name for the failure message.
const HOSTILE_LINES: &[(&str, &str)] = &[
    ("escape sequence", "build \u{1b}[31mred\u{1b}[0m"),
    ("osc title", "x \u{1b}]0;owned\u{7}"),
    ("osc 52 clipboard", "x \u{1b}]52;c;QUJD\u{7}"),
    (
        "osc 8 hyperlink",
        "\u{1b}]8;;http://evil.example\u{7}link\u{1b}]8;;\u{7}",
    ),
    ("nul", "a\0b"),
    ("newline", "line one\nline two"),
    ("carriage return", "a\rb"),
    ("tab", "a\tb"),
    ("backspace", "a\u{8}b"),
    ("right-to-left override", "gpj.\u{202e}exe"),
    ("left-to-right isolate", "a\u{2066}b"),
    ("pop directional isolate", "a\u{2069}b"),
    ("zero width space", "a\u{200b}b"),
    ("word joiner", "a\u{2060}b"),
    ("byte order mark", "a\u{feff}b"),
    ("c1 control", "a\u{85}b"),
    ("line separator", "a\u{2028}b"),
];

fn is_clean_char(c: char) -> bool {
    let n = c as u32;
    if n < 0x20 {
        return c == '\n' || c == '\t';
    }
    !(c.is_control() || (0x202A..=0x202E).contains(&n) || (0x2066..=0x2069).contains(&n))
}

/// Walks a document and fails on any string that still carries what the dashboard must never see.
fn assert_clean_value(v: &Value, what: &str) {
    match v {
        Value::Str(t) => assert_clean_text(t, what),
        Value::Array(items) => items.iter().for_each(|i| assert_clean_value(i, what)),
        Value::Object(m) => {
            for (k, i) in m {
                assert_clean_text(k, what);
                assert_clean_value(i, what);
            }
        }
        _ => {}
    }
}

fn assert_clean_text(t: &str, what: &str) {
    if let Some(bad) = t.chars().find(|c| !is_clean_char(*c)) {
        panic!("{what}: U+{:04X} survived in {t:?}", bad as u32);
    }
}

fn new_run(h: &mut Harness) {
    h.call(
        "run.create",
        obj(vec![
            ("objective", s("ship the feature")),
            ("repo", s("/repo")),
        ]),
    )
    .unwrap();
}

fn add(h: &mut Harness, id: &str, title: &str) -> Result<(), FleetError> {
    h.call(
        "task.add",
        obj(vec![
            ("run", s("r1")),
            ("id", s(id)),
            ("title", s(title)),
            ("spec", s("do the work")),
        ]),
    )
    .map(|_| ())
}

fn start(h: &mut Harness) {
    h.call("run.start", obj(vec![("run", s("r1"))])).unwrap();
}

/// A harness with run r1 and the given tasks, started.
fn harness(tasks: &[&str]) -> Harness {
    harness_with(tasks, |_| {})
}

fn harness_with(tasks: &[&str], tweak: impl FnOnce(&mut fleet_app::config::AppConfig)) -> Harness {
    let mut cfg = test_config();
    cfg.approval_ttl_ms = 10_000;
    tweak(&mut cfg);
    let mut h = Harness::with_config(cfg);
    new_run(&mut h);
    for t in tasks {
        add(&mut h, t, &format!("task {t}")).unwrap();
    }
    start(&mut h);
    h
}

fn data(h: &mut Harness, cmd: &str, args: Value) -> Value {
    h.call(cmd, args).unwrap().data().clone()
}

fn status(h: &mut Harness) -> Value {
    data(h, "status", Value::Null)
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

fn task_count(h: &mut Harness) -> usize {
    let st = status(h);
    st.get("runs")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|r| r.get("tasks").and_then(Value::as_array).unwrap().len())
        .sum()
}

fn field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("no string {key} in {v:?}"))
        .to_string()
}

fn gates(h: &mut Harness) -> Vec<Value> {
    data(h, "gate.list", Value::Null)
        .get("gates")
        .and_then(Value::as_array)
        .unwrap()
        .to_vec()
}

fn approvals(h: &mut Harness) -> Vec<Value> {
    data(h, "approval.list", Value::Null)
        .get("approvals")
        .and_then(Value::as_array)
        .unwrap()
        .to_vec()
}

fn events(h: &mut Harness) -> Value {
    data(h, "events", obj(vec![("limit", Value::int(500u64))]))
}

fn tail(h: &mut Harness, task: &str, since: Option<u64>) -> Value {
    let mut pairs = vec![("task", s(task)), ("max", Value::int(1u64 << 16))];
    if let Some(n) = since {
        pairs.push(("since", Value::int(n)));
    }
    data(h, "task.tail", obj(pairs))
}

fn session_dir(h: &Harness, task: &str) -> (fleet_app::ports::SessionId, String) {
    let id = h.world.session_for(task).expect("task has a live session");
    let cwd = h.world.session_spec(id).unwrap().cwd;
    (id, cwd)
}

/// The text of `raw` as the PTY adapter stores it: through the domain's own output buffer.
fn stored_like_the_adapter(raw: &[u8]) -> String {
    let mut buf = OutputBuffer::new(64 * 1024, Arc::new(Redactor::builtin()));
    buf.push(raw);
    buf.finish();
    buf.read_since(0, 64 * 1024).text
}

/// Terminal output with every trick: colours, an OSC 8 hyperlink, an OSC 52 clipboard write, a
/// title change, bidi overrides and isolates, zero-width characters, backspaces, NUL and CR.
fn hostile_output() -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"hello \x1b[31mred\x1b[0m world\n");
    v.extend_from_slice(b"\x1b]8;;http://evil.example\x07click here\x1b]8;;\x07 done\n");
    v.extend_from_slice(b"clip \x1b]52;c;QUJDRA==\x07 board\n");
    v.extend_from_slice(b"title \x1b]0;owned\x1b\\ set\n");
    v.extend_from_slice(
        "rtl \u{202e}txt.exe \u{2066}iso\u{2069} zw\u{200b}\u{2060}\u{feff}end\n".as_bytes(),
    );
    v.extend_from_slice(b"back\x08\x08\x08\x08space nul\0byte carriage\rreturn\n");
    v.extend_from_slice(b"\x1b[2J\x1b[H cleared\tkept tab\n");
    v
}

// ---- (1) hostile text cannot be stored -------------------------------------------------------

#[test]
fn control_and_bidi_characters_in_a_task_title_are_rejected_and_nothing_is_stored() {
    let mut h = Harness::new();
    new_run(&mut h);
    for (name, text) in HOSTILE_LINES {
        let err = add(&mut h, "t1", &format!("title {text}")).expect_err(name);
        assert_eq!(err.code, ErrorCode::Invalid, "{name}");
        assert_clean_text(&err.message, &format!("error message for {name}"));
    }
    assert_eq!(
        task_count(&mut h),
        0,
        "a rejected title must not create a task"
    );
    // The same id is still free: the rejections left no trace.
    add(&mut h, "t1", "a plain title").unwrap();
    assert_eq!(task_count(&mut h), 1);
}

#[test]
fn control_and_bidi_characters_in_hostile_ids_are_rejected() {
    let mut h = Harness::new();
    new_run(&mut h);
    for (name, text) in HOSTILE_LINES {
        let err = h
            .call(
                "task.add",
                obj(vec![
                    ("run", s("r1")),
                    ("id", s(&format!("t{text}"))),
                    ("title", s("fine")),
                ]),
            )
            .expect_err(name);
        assert_eq!(err.code, ErrorCode::Invalid, "{name}");
        assert_clean_text(&err.message, &format!("error message for {name}"));
    }
    assert_eq!(task_count(&mut h), 0);
}

#[test]
fn control_and_bidi_characters_in_a_run_objective_are_rejected() {
    // An objective is free text: line breaks and tabs are part of it by design (checked below),
    // every other control character and every invisible or bidi character is refused.
    let mut h = Harness::new();
    for (name, text) in HOSTILE_LINES {
        if matches!(*name, "newline" | "tab") {
            continue;
        }
        let err = h
            .call(
                "run.create",
                obj(vec![
                    ("objective", s(&format!("ship {text}"))),
                    ("repo", s("/repo")),
                ]),
            )
            .expect_err(name);
        assert_eq!(err.code, ErrorCode::Invalid, "{name}");
        assert_clean_text(&err.message, &format!("error message for {name}"));
    }
    assert!(status(&mut h)
        .get("runs")
        .and_then(Value::as_array)
        .unwrap()
        .is_empty());
}

#[test]
fn a_multi_line_objective_is_kept_with_its_line_breaks() {
    let mut h = Harness::new();
    let objective = "first line\n\tsecond line, indented";
    h.call(
        "run.create",
        obj(vec![("objective", s(objective)), ("repo", s("/repo"))]),
    )
    .unwrap();
    let st = status(&mut h);
    let run = &st.get("runs").and_then(Value::as_array).unwrap()[0];
    assert_eq!(field(run, "objective"), objective);
}

#[test]
fn hostile_branch_and_ref_names_are_rejected() {
    let mut h = Harness::new();
    for (name, text) in HOSTILE_LINES {
        let err = h
            .call(
                "run.create",
                obj(vec![
                    ("objective", s("ship")),
                    ("repo", s("/repo")),
                    ("base", s(&format!("feature/{text}"))),
                ]),
            )
            .expect_err(name);
        assert_eq!(err.code, ErrorCode::Invalid, "{name}");
        assert_clean_text(&err.message, &format!("error message for {name}"));
    }
    // Names that git itself or an option parser would misread are refused as well.
    for bad in [
        "--upload-pack=x",
        "a..b",
        "a b",
        "a@{1}",
        "a//b",
        ".hidden/x",
        "x.lock",
        "caf\u{e9}",
    ] {
        let err = h
            .call(
                "run.create",
                obj(vec![
                    ("objective", s("ship")),
                    ("repo", s("/repo")),
                    ("base", s(bad)),
                ]),
            )
            .expect_err(bad);
        assert_eq!(err.code, ErrorCode::Invalid, "{bad:?}");
    }
    assert!(status(&mut h)
        .get("runs")
        .and_then(Value::as_array)
        .unwrap()
        .is_empty());
}

#[test]
fn the_branches_shown_for_tasks_are_built_by_fleet_from_validated_ids() {
    let mut h = harness(&["a", "b"]);
    for id in ["a", "b"] {
        let doc = task_doc(&mut h, id);
        assert_eq!(field(&doc, "branch"), format!("fleet/r1/{id}"));
    }
    let st = status(&mut h);
    let run = &st.get("runs").and_then(Value::as_array).unwrap()[0];
    assert_eq!(field(run, "integration_branch"), "fleet/r1/integration");
}

// ---- (2) markup is data ------------------------------------------------------------------------

#[test]
fn markup_quotes_and_ampersands_in_a_title_come_back_byte_for_byte() {
    let titles = [
        "<script>alert(1)</script>",
        "<img src=x onerror=alert(1)>",
        "Tom & \"Jerry\" 'quoted' <b>bold</b> &amp; &lt;already&gt; &#x3c;",
        "</textarea></title><svg/onload=alert(1)>",
        "`${7*7}` {{constructor.constructor('x')()}} %s %n \\u003cscript\\u003e",
        "javascript:alert(1)",
        "\u{1F600} caf\u{e9} \u{4e2d}\u{6587} \u{200d}",
    ];
    let mut h = Harness::new();
    new_run(&mut h);
    for (i, title) in titles.iter().enumerate() {
        add(&mut h, &format!("t{i}"), title).unwrap();
    }
    for (i, title) in titles.iter().enumerate() {
        let doc = task_doc(&mut h, &format!("t{i}"));
        assert_eq!(field(&doc, "title"), *title, "title {i} was changed");
    }
    // The canonical text of the document holds the markup as plain JSON strings: nothing was
    // entity-encoded on the way, so the page's textContent shows exactly what was typed.
    let text = status(&mut h).canonical();
    for needle in [
        "<script>alert(1)</script>",
        "<img src=x onerror=alert(1)>",
        "Tom & ",
    ] {
        assert!(text.contains(needle), "{needle} is missing from {text}");
    }
    assert!(!text.contains("&lt;script"), "{text}");
    assert!(!text.contains("&amp;amp;"), "{text}");
    assert!(!text.contains("&quot;"), "{text}");
}

#[test]
fn markup_in_titles_objectives_and_gate_text_is_not_escaped_in_any_document() {
    let title = "<script>alert(1)</script>";
    let objective = "<img src=x onerror=alert(1)> & \"more\"";
    let mut h = Harness::new();
    h.call(
        "run.create",
        obj(vec![("objective", s(objective)), ("repo", s("/repo"))]),
    )
    .unwrap();
    add(&mut h, "a", title).unwrap();
    start(&mut h);

    let st = status(&mut h);
    let run = &st.get("runs").and_then(Value::as_array).unwrap()[0];
    assert_eq!(field(run, "objective"), objective);
    assert_eq!(field(&task_doc(&mut h, "a"), "title"), title);

    // A worker question that contains markup is parsed (sanitized) and shown unchanged.
    let (id, dir) = session_dir(&h, "a");
    let question = WorkerQuestion::from_value(&obj(vec![
        ("version", Value::int(1u64)),
        ("kind", s("question")),
        ("text", s("Use <b>sqlite</b> & \"wal\"?")),
        ("choices", Value::array([s("<i>yes</i>"), s("no & never")])),
    ]))
    .unwrap();
    h.world.write_question(&dir, ContractRead::Valid(question));
    h.world.exit(id, 0);
    h.orch.tick();

    let g = gates(&mut h);
    assert_eq!(g.len(), 1);
    assert_eq!(field(&g[0], "text"), "Use <b>sqlite</b> & \"wal\"?");
    let choices: Vec<&str> = g[0]
        .get("choices")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(choices, ["<i>yes</i>", "no & never"]);

    // The events feed carries the title as typed, too.
    let feed = events(&mut h).canonical();
    assert!(!feed.contains("&lt;"), "{feed}");
    assert!(!feed.contains("&amp;"), "{feed}");
}

// ---- (3) worker output is sanitized -------------------------------------------------------------

#[test]
fn the_output_buffer_the_adapter_uses_removes_every_escape_and_bidi_character() {
    let text = stored_like_the_adapter(&hostile_output());
    assert_clean_text(&text, "stored output");
    assert!(text.contains("hello red world"), "{text:?}");
    assert!(text.contains("click here"), "{text:?}");
    assert!(text.contains("kept tab"), "{text:?}");
    assert!(
        !text.contains("evil.example"),
        "the hyperlink target survived: {text:?}"
    );
    assert!(
        !text.contains("QUJD"),
        "the clipboard payload survived: {text:?}"
    );
    assert!(
        !text.contains("owned"),
        "the title payload survived: {text:?}"
    );
}

#[test]
fn sanitized_output_reaches_task_tail_live_and_after_the_task_ended() {
    let mut h = harness_with(&["a"], |c| c.max_retries = 0);
    let (id, dir) = session_dir(&h, "a");
    let stored = stored_like_the_adapter(&hostile_output());
    h.world.print(id, &stored);

    // Live, newest part (no offset) and from an offset (what the SSE stream does).
    let snapshot = tail(&mut h, "a", None);
    assert_clean_value(&snapshot, "live task.tail");
    assert_eq!(field(&snapshot, "text"), stored);
    let chunk = tail(&mut h, "a", Some(0));
    assert_clean_value(&chunk, "live task.tail since 0");
    assert_eq!(field(&chunk, "text"), stored);
    assert!(field(&chunk, "text").contains("hello red world"));

    // The worker fails: the stored log and the failure text in status are clean as well.
    h.world.write_result(
        &dir,
        ContractRead::Valid(WorkerResult {
            status: fleet_domain::contract::ResultStatus::Failed,
            summary: "gave up".to_string(),
        }),
    );
    h.world.exit(id, 1);
    for _ in 0..3 {
        h.orch.tick();
    }
    let ended = tail(&mut h, "a", None);
    assert_eq!(ended.get("live").and_then(Value::as_bool), Some(false));
    assert_clean_value(&ended, "task.tail after the task ended");
    assert_clean_value(&status(&mut h), "status after a failed task");
    assert_clean_value(&events(&mut h), "events after a failed task");
}

#[test]
fn raw_terminal_output_cannot_reach_failure_text_or_events_unsanitized() {
    // Even if a session adapter handed over raw bytes, the summary, failure and event lines
    // built from the output of a failed worker must be clean.
    let mut h = harness(&["a"]);
    let (id, _dir) = session_dir(&h, "a");
    let raw = String::from_utf8_lossy(&hostile_output()).into_owned();
    h.world.print(id, &raw);
    h.world.exit(id, 3);
    for _ in 0..3 {
        h.orch.tick();
    }
    assert_clean_value(&status(&mut h), "status");
    assert_clean_value(&events(&mut h), "events");
    assert_clean_value(&data(&mut h, "approval.list", Value::Null), "approval.list");
    assert_clean_value(&data(&mut h, "gate.list", Value::Null), "gate.list");
}

// ---- (4) worker files are sanitized when parsed ----------------------------------------------------

fn dirty() -> &'static str {
    "all \u{1b}[31mdone\u{1b}[0m \u{1b}]8;;http://evil.example\u{7}link\u{1b}]8;;\u{7} \u{202e}reversed\u{2066} nul\u{0}byte\r\nsecond\u{8}line \u{200b}zw"
}

#[test]
fn a_hostile_result_summary_is_sanitized_before_it_appears_in_status() {
    let mut h = harness(&["a"]);
    let (id, dir) = session_dir(&h, "a");
    let parsed = WorkerResult::from_value(&obj(vec![
        ("version", Value::int(1u64)),
        ("status", s("done")),
        ("summary", s(dirty())),
    ]))
    .unwrap();
    assert_clean_text(&parsed.summary, "parsed summary");
    assert!(parsed.summary.contains("done"), "{:?}", parsed.summary);
    h.world
        .write_result(&dir, ContractRead::Valid(parsed.clone()));
    h.world.mark_dirty(&dir);
    h.world.exit(id, 0);
    h.orch.tick();

    let doc = task_doc(&mut h, "a");
    assert_eq!(field(&doc, "summary"), parsed.summary);
    assert_clean_value(&status(&mut h), "status");
    assert_clean_value(&events(&mut h), "events");
}

#[test]
fn a_hostile_failure_summary_is_sanitized_before_it_appears_in_status() {
    let mut h = harness_with(&["a"], |c| c.max_retries = 0);
    let (id, dir) = session_dir(&h, "a");
    let parsed = WorkerResult::from_value(&obj(vec![
        ("version", Value::int(1u64)),
        ("status", s("failed")),
        ("summary", s(dirty())),
    ]))
    .unwrap();
    h.world.write_result(&dir, ContractRead::Valid(parsed));
    h.world.exit(id, 1);
    for _ in 0..4 {
        h.orch.tick();
    }
    let doc = task_doc(&mut h, "a");
    assert_eq!(field(&doc, "status"), "failed", "{doc:?}");
    assert!(field(&doc, "failure").contains("reversed"), "{doc:?}");
    assert_clean_value(&status(&mut h), "status");
    assert_clean_value(&events(&mut h), "events");
}

#[test]
fn a_hostile_question_and_its_choices_are_sanitized_in_status_and_gate_list() {
    let mut h = harness(&["a"]);
    let (id, dir) = session_dir(&h, "a");
    let parsed = WorkerQuestion::from_value(&obj(vec![
        ("version", Value::int(1u64)),
        ("kind", s("question")),
        ("text", s(dirty())),
        (
            "choices",
            Value::array([
                s("\u{1b}[1mfirst\u{1b}[0m"),
                s("sec\u{202e}ond"),
                s("th\u{0}ird\r"),
            ]),
        ),
    ]))
    .unwrap();
    assert_clean_text(&parsed.text, "parsed question");
    for c in &parsed.choices {
        assert_clean_text(c, "parsed choice");
    }
    h.world
        .write_question(&dir, ContractRead::Valid(parsed.clone()));
    h.world.exit(id, 0);
    h.orch.tick();

    let g = gates(&mut h);
    assert_eq!(g.len(), 1);
    assert_eq!(field(&g[0], "text"), parsed.text);
    assert_clean_value(&Value::array(g), "gate.list");
    let st = status(&mut h);
    assert_eq!(st.get("gates").and_then(Value::as_array).unwrap().len(), 1);
    assert_clean_value(&st, "status");
    assert_clean_value(&events(&mut h), "events");
}

#[test]
fn invisible_characters_in_a_question_are_made_visible_not_dropped_silently() {
    let parsed = WorkerQuestion::from_value(&obj(vec![
        ("version", Value::int(1u64)),
        ("kind", s("question")),
        ("text", s("keep \u{200b}\u{202e}this")),
    ]))
    .unwrap();
    assert_clean_text(&parsed.text, "parsed question");
    assert!(parsed.text.contains("\\u{202e}"), "{:?}", parsed.text);
    assert!(parsed.text.contains("keep") && parsed.text.contains("this"));
}

// ---- (5) approvals show exactly what runs -----------------------------------------------------------

fn request(h: &mut Harness, task: &str, text: &str, action: Option<ActionRequest>) {
    let (id, dir) = session_dir(h, task);
    h.world.write_question(
        &dir,
        ContractRead::Valid(WorkerQuestion {
            kind: QuestionKind::Approval,
            text: text.to_string(),
            choices: vec![],
            action,
        }),
    );
    h.world.exit(id, 0);
    h.orch.tick();
}

fn args_of(a: &Value) -> Vec<String> {
    a.get("args")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn an_approval_shows_the_exact_command_and_every_argument_and_a_matching_digest() {
    let mut h = harness(&["a"]);
    let command = "/usr/bin/git";
    let args = [
        "push",
        "--force",
        "origin",
        "main branch",
        "",
        "a\"b",
        "back\\slash",
        "<img src=x onerror=alert(1)>",
    ];
    request(
        &mut h,
        "a",
        "may I push?",
        Some(ActionRequest {
            command: command.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
        }),
    );
    let list = approvals(&mut h);
    assert_eq!(list.len(), 1);
    let doc = &list[0];
    assert_eq!(field(doc, "tool"), "command");
    assert_eq!(field(doc, "command"), command);
    assert_eq!(args_of(doc), args);

    // The digest is the one the domain computes from the action through the same digest port.
    let action = Action::command(
        command,
        &args.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
    );
    let expected = action.digest(&TaskId::parse("a").unwrap(), &ToyDigest);
    assert_eq!(field(doc, "digest"), expected);
    assert_eq!(expected.len(), 64);
    assert!(expected.chars().all(|c| c.is_ascii_hexdigit()));

    // `display` quotes every part, so each argument is one visible item.
    let display = field(doc, "display");
    assert_eq!(display, action.display());
    assert!(
        display.starts_with("command: \"/usr/bin/git\" \"push\" \"--force\""),
        "{display}"
    );
    assert!(
        display.contains("\"main branch\" \"\" \"a\\\"b\" \"back\\\\slash\""),
        "{display}"
    );
    assert_clean_text(&display, "display");
    assert!(!display.contains('\n'));
}

#[test]
fn a_hostile_argument_cannot_make_the_display_look_like_two_actions() {
    let mut h = harness(&["a"]);
    // The contract parser refuses these, so they cannot come from a worker file (checked in the
    // next test); here the approval layer is given them directly to prove the display holds anyway.
    let args = [
        "ok\n/usr/bin/rm -rf /",
        "\u{1b}[2K\u{1b}[1Acommand: \"/bin/true\"",
        "evil\u{202e}eurt/nib/",
        "nul\0byte",
        "cr\rreturn",
    ];
    request(
        &mut h,
        "a",
        "may I?",
        Some(ActionRequest {
            command: "/bin/echo".to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
        }),
    );
    let list = approvals(&mut h);
    assert_eq!(list.len(), 1);
    let doc = &list[0];
    // The exact arguments are still listed one by one (the page renders them with textContent).
    assert_eq!(args_of(doc), args);
    let display = field(doc, "display");
    assert!(
        !display.contains('\n') && !display.contains('\r'),
        "{display:?}"
    );
    assert!(!display.contains('\u{1b}'), "{display:?}");
    assert!(!display.contains('\0'), "{display:?}");
    assert!(
        !display
            .chars()
            .any(|c| (0x202A..=0x202E).contains(&(c as u32))),
        "{display:?}"
    );
    assert_clean_text(&display, "display");
    // Exactly one `command:` label, at the start: the hostile text is escaped inside quotes.
    assert!(display.starts_with("command: \"/bin/echo\" "), "{display}");
    assert_eq!(display.matches("command: ").count(), 2, "{display}");
    assert!(display.contains("\\n"), "{display}");
    assert!(display.contains("\\u{1b}"), "{display}");
    // Every part is quoted: the number of unescaped quotes is two per part.
    let mut unescaped = 0;
    let mut escaped = false;
    for c in display.chars() {
        match (escaped, c) {
            (true, _) => escaped = false,
            (false, '\\') => escaped = true,
            (false, '"') => unescaped += 1,
            _ => {}
        }
    }
    assert_eq!(unescaped, 2 * (1 + args.len()), "{display}");

    // And the digest binds the exact arguments, boundaries included.
    let action = Action::command(
        "/bin/echo",
        &args.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
    );
    assert_eq!(
        field(doc, "digest"),
        action.digest(&TaskId::parse("a").unwrap(), &ToyDigest)
    );
}

#[test]
fn hostile_commands_and_arguments_in_a_worker_file_are_refused_by_the_parser() {
    let mk = |command: &str, args: Vec<Value>| {
        WorkerQuestion::from_value(&obj(vec![
            ("version", Value::int(1u64)),
            ("kind", s("approval")),
            ("text", s("may I?")),
            (
                "action",
                obj(vec![("command", s(command)), ("args", Value::array(args))]),
            ),
        ]))
    };
    for (name, text) in HOSTILE_LINES {
        let arg = mk("/bin/echo", vec![s(&format!("x{text}"))]);
        assert!(arg.is_err(), "an argument with {name} was accepted");
        let cmd = mk(&format!("/bin/e{text}"), vec![]);
        assert!(cmd.is_err(), "a command with {name} was accepted");
    }
    assert!(mk("/bin/echo", vec![s("plain"), s("two words")]).is_ok());
}

#[test]
fn an_approval_without_a_command_shows_the_sanitized_question_and_no_command() {
    let mut h = harness(&["a"]);
    let (id, dir) = session_dir(&h, "a");
    let parsed = WorkerQuestion::from_value(&obj(vec![
        ("version", Value::int(1u64)),
        ("kind", s("approval")),
        ("text", s("deploy? \u{1b}[31mnow\u{1b}[0m \u{202e}yes")),
    ]))
    .unwrap();
    h.world.write_question(&dir, ContractRead::Valid(parsed));
    h.world.exit(id, 0);
    h.orch.tick();
    let list = approvals(&mut h);
    assert_eq!(list.len(), 1);
    assert_eq!(field(&list[0], "tool"), "manual");
    assert_clean_value(&Value::array(list.clone()), "approval.list");
    let digest = field(&list[0], "digest");
    assert_eq!(digest.len(), 64);
    assert_clean_value(&status(&mut h), "status");
}
