//! Human views of the context pack commands: `fleet context show|diff|verify|explain` and
//! `fleet handoff`. Every value comes from the daemon and is cleaned before it reaches the
//! terminal; the rendered pack and the handoff text keep their line breaks but nothing else.

use crate::render::{clean, items, multiline, prefix, table, text, text_or_dash};
use fleet_domain::value::Value;
use std::fmt::Write as _;

/// Hex digits of a digest shown in a table.
const DIGEST_CELL: usize = 16;

/// A sub-field of an object field (`tokens.a`), cleaned; empty when absent.
fn pair(v: &Value, key: &str, sub: &str) -> String {
    v.get(key).map(|o| text(o, sub)).unwrap_or_default()
}

/// A free-text string field with its line breaks.
fn body(v: &Value, key: &str) -> String {
    multiline(v.get(key).and_then(Value::as_str).unwrap_or(""))
}

fn flag(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool) == Some(true)
}

fn digest_cell(v: &Value, key: &str) -> String {
    prefix(&text(v, key), DIGEST_CELL)
}

fn run_task(v: &Value) -> String {
    let run = text(v, "run");
    let task = text(v, "task");
    if run.is_empty() {
        task
    } else {
        format!("{run}/{task}")
    }
}

/// One row per layer of a pack: what was dropped and which flags are set.
fn layer_notes(layer: &Value) -> String {
    let mut notes: Vec<String> = Vec::new();
    let dropped: Vec<String> = items(layer, "dropped")
        .iter()
        .filter_map(Value::as_str)
        .map(clean)
        .collect();
    if !dropped.is_empty() {
        notes.push(format!("dropped {}", dropped.join(", ")));
    }
    for key in ["unverified", "excluded", "clipped"] {
        if flag(layer, key) {
            notes.push(key.to_string());
        }
    }
    if notes.is_empty() {
        "-".to_string()
    } else {
        notes.join("; ")
    }
}

/// `fleet context show`: a header, the layer table, a blank line and the rendered pack.
pub fn render_show(v: &Value) -> String {
    let mut out = format!("context pack of {}\n", run_task(v));
    let _ = writeln!(out, "digest:  {}", text_or_dash(v, "digest"));
    let _ = writeln!(out, "stable:  {}", text_or_dash(v, "stable_digest"));
    let _ = writeln!(out, "profile: {}", text_or_dash(v, "profile"));
    let _ = writeln!(
        out,
        "tokens {} of {}",
        text(v, "tokens"),
        text(v, "max_tokens")
    );
    let rows: Vec<Vec<String>> = items(v, "layers")
        .iter()
        .map(|l| {
            vec![
                text(l, "id"),
                text(l, "name"),
                format!("{}/{}", text(l, "tokens"), text(l, "budget")),
                digest_cell(l, "digest"),
                text_or_dash(l, "trust"),
                text(l, "items"),
                layer_notes(l),
            ]
        })
        .collect();
    out.push_str(&table(
        &["#", "LAYER", "TOKENS", "DIGEST", "TRUST", "ITEMS", "NOTES"],
        &[4, 24, 16, DIGEST_CELL, 16, 8, 120],
        &rows,
        "  ",
    ));
    out.push('\n');
    let text = body(v, "text");
    out.push_str(&text);
    if !text.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// The per-layer comparison of two packs; returns the table, the number of layers and the
/// labels of those that changed.
fn layer_comparison(v: &Value, a_name: &str, b_name: &str) -> (String, usize, Vec<String>) {
    let layers = items(v, "layers");
    let mut changed: Vec<String> = Vec::new();
    let rows: Vec<Vec<String>> = layers
        .iter()
        .map(|l| {
            let is_changed = flag(l, "changed");
            if is_changed {
                changed.push(format!("{} {}", text(l, "id"), text(l, "name")));
            }
            let da = pair(l, "digest", "a");
            let db = pair(l, "digest", "b");
            let digest = if is_changed {
                format!(
                    "{} -> {}",
                    prefix(&da, DIGEST_CELL),
                    prefix(&db, DIGEST_CELL)
                )
            } else {
                prefix(&da, DIGEST_CELL)
            };
            vec![
                text(l, "id"),
                text(l, "name"),
                if is_changed { "CHANGED" } else { "same" }.to_string(),
                format!("{} -> {}", pair(l, "tokens", "a"), pair(l, "tokens", "b")),
                digest,
            ]
        })
        .collect();
    let tokens_head = format!("TOKENS ({a_name} -> {b_name})");
    let header = ["#", "LAYER", "STATE", tokens_head.as_str(), "DIGEST"];
    let t = table(&header, &[4, 24, 8, 24, 40], &rows, "  ");
    (t, layers.len(), changed)
}

fn summary_line(count: usize, changed: &[String]) -> String {
    if changed.is_empty() {
        match count {
            0 => "no layers\n".to_string(),
            1 => "layer 1 identical\n".to_string(),
            n => format!("layers 1-{n} identical\n"),
        }
    } else {
        format!(
            "{} of {count} layers changed: {}\n",
            changed.len(),
            changed.join(", ")
        )
    }
}

/// `fleet context diff`.
pub fn render_diff(v: &Value) -> String {
    let mut out = format!("context diff of {}\n", run_task(v));
    let _ = writeln!(out, "a: {}", text_or_dash(v, "a"));
    let _ = writeln!(out, "b: {}", text_or_dash(v, "b"));
    let _ = writeln!(
        out,
        "tokens {} -> {}",
        pair(v, "tokens", "a"),
        pair(v, "tokens", "b")
    );
    let (t, count, changed) = layer_comparison(v, "a", "b");
    out.push_str(&t);
    out.push_str(&summary_line(count, &changed));
    if flag(v, "same") {
        out.push_str("the packs are identical\n");
    } else if flag(v, "stable_same") {
        out.push_str("the stable part of the packs is identical\n");
    }
    out
}

/// `fleet context verify`.
pub fn render_verify(v: &Value) -> String {
    let current = text_or_dash(v, "current");
    if flag(v, "match") {
        return format!("OK: the pack compiled now has the digest {current}\n");
    }
    let mut out = format!(
        "DIFFERS: the pack compiled now has the digest {current}, the stored pack {}\n",
        text_or_dash(v, "stored")
    );
    let (t, count, changed) = layer_comparison(v, "stored", "now");
    out.push_str(&t);
    out.push_str(&summary_line(count, &changed));
    if flag(v, "stable_same") {
        out.push_str("the stable part of the pack is identical\n");
    }
    out
}

/// `fleet context explain`.
pub fn render_explain(v: &Value) -> String {
    let empty = Value::Null;
    let profile = v.get("profile").unwrap_or(&empty);
    let mut out = format!(
        "profile {}: {} tokens at most, {} characters per token, {}% safety margin\n",
        text_or_dash(profile, "name"),
        text(profile, "max_tokens"),
        text(profile, "chars_per_token"),
        text(profile, "safety_percent")
    );
    let _ = writeln!(out, "estimator: {}", text_or_dash(v, "estimator"));
    let _ = writeln!(out, "policy: {}", text_or_dash(v, "policy"));
    let knowledge = text(v, "knowledge_file");
    let _ = writeln!(
        out,
        "knowledge file: {}",
        if knowledge.is_empty() {
            "none"
        } else {
            &knowledge
        }
    );
    if let Some(subject) = v.get("subject").filter(|s| !s.is_null()) {
        let _ = writeln!(out, "for task {}", run_task(subject));
    }
    let rows: Vec<Vec<String>> = items(v, "layers")
        .iter()
        .map(|l| {
            vec![
                text(l, "id"),
                text(l, "name"),
                format!("{}%", text(l, "percent")),
                text(l, "budget"),
                text(l, "purpose"),
                if flag(l, "sheddable") {
                    "dropped when the pack is too large"
                } else {
                    "never cut"
                }
                .to_string(),
            ]
        })
        .collect();
    out.push_str(&table(
        &["#", "LAYER", "SHARE", "BUDGET", "PURPOSE", "WHEN TOO LARGE"],
        &[4, 24, 8, 10, 80, 40],
        &rows,
        "  ",
    ));
    let order: Vec<String> = items(v, "shrink_order")
        .iter()
        .map(|n| match n {
            Value::Int(i) => i.to_string(),
            Value::Str(s) => clean(s),
            _ => String::new(),
        })
        .filter(|s| !s.is_empty())
        .collect();
    if order.is_empty() {
        out.push_str("no layer is shrunk: a pack that is too large is refused\n");
    } else {
        let _ = writeln!(
            out,
            "when the pack is too large, layers are shrunk in this order: {} (the first goes first)",
            order.join(", ")
        );
    }
    out
}

/// `fleet handoff`: the human rendering the daemon made.
pub fn render_handoff(v: &Value) -> String {
    let text = body(v, "text");
    if text.is_empty() || text.ends_with('\n') {
        text
    } else {
        format!("{text}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> Value {
        Value::str(x)
    }

    fn layer(id: u64, name: &str, dropped: &[&str]) -> Value {
        Value::object([
            ("id", Value::int(id)),
            ("name", s(name)),
            ("tokens", Value::int(120u64)),
            ("budget", Value::int(500u64)),
            ("digest", s(&"ab".repeat(32))),
            ("trust", s("trusted")),
            ("items", Value::int(3u64)),
            ("dropped", Value::array(dropped.iter().map(|d| s(d)))),
            ("unverified", Value::Bool(false)),
            ("excluded", Value::Bool(false)),
            ("clipped", Value::Bool(true)),
        ])
    }

    fn show_doc(text: &str, dropped: &[&str]) -> Value {
        Value::object([
            ("schema", s("fleet.context.show/1")),
            ("run", s("r1")),
            ("task", s("t1")),
            ("digest", s(&"cd".repeat(32))),
            ("stable_digest", s(&"ef".repeat(32))),
            ("profile", s("default")),
            ("tokens", Value::int(900u64)),
            ("max_tokens", Value::int(4000u64)),
            (
                "layers",
                Value::array([layer(1, "charter", &[]), layer(2, "knowledge", dropped)]),
            ),
            ("manifest", Value::object([("x", Value::int(1u64))])),
            ("text", s(text)),
        ])
    }

    #[test]
    fn show_has_a_header_a_layer_table_and_the_pack_text() {
        let out = render_show(&show_doc("# Pack\nline two\n", &["a.md", "b.md"]));
        assert!(out.starts_with("context pack of r1/t1\n"), "{out}");
        assert!(out.contains(&format!("digest:  {}\n", "cd".repeat(32))));
        assert!(out.contains("profile: default\n"));
        assert!(out.contains("tokens 900 of 4000\n"));
        assert!(out.contains("120/500"));
        assert!(out.contains(&"ab".repeat(8)));
        assert!(!out.contains(&"ab".repeat(9)), "digests are cut: {out}");
        assert!(out.contains("dropped a.md, b.md"), "{out}");
        assert!(out.contains("clipped"));
        assert!(out.ends_with("\n\n# Pack\nline two\n"), "{out:?}");
    }

    #[test]
    fn hostile_values_are_neutralised() {
        let hostile = "\u{1b}[31mred\u{1b}]0;title\u{7}x\u{0}";
        let mut doc = show_doc(&format!("pack {hostile}\nnext"), &[hostile]);
        if let Value::Object(m) = &mut doc {
            m.insert("profile".into(), s(hostile));
        }
        for out in [
            render_show(&doc),
            render_handoff(&Value::object([("text", s(hostile))])),
        ] {
            assert!(!out.chars().any(|c| c.is_control() && c != '\n'), "{out:?}");
        }
    }

    fn diff_layer(id: u64, changed: bool) -> Value {
        Value::object([
            ("id", Value::int(id)),
            ("name", s("layer")),
            ("changed", Value::Bool(changed)),
            (
                "tokens",
                Value::object([("a", Value::int(10u64)), ("b", Value::int(12u64))]),
            ),
            (
                "digest",
                Value::object([("a", s(&"11".repeat(32))), ("b", s(&"22".repeat(32)))]),
            ),
        ])
    }

    fn diff_doc(changed: &[bool], same: bool) -> Value {
        Value::object([
            ("schema", s("fleet.context.diff/1")),
            ("run", s("r1")),
            ("task", s("t1")),
            ("a", s("aaaaaaaa")),
            ("b", s("bbbbbbbb")),
            ("same", Value::Bool(same)),
            ("stable_same", Value::Bool(same)),
            (
                "tokens",
                Value::object([("a", Value::int(40u64)), ("b", Value::int(44u64))]),
            ),
            (
                "layers",
                Value::array(
                    changed
                        .iter()
                        .enumerate()
                        .map(|(i, c)| diff_layer(i as u64 + 1, *c)),
                ),
            ),
        ])
    }

    #[test]
    fn diff_marks_changed_layers_and_summarises() {
        let out = render_diff(&diff_doc(&[false, true, false, false, false], false));
        assert!(out.starts_with("context diff of r1/t1\n"), "{out}");
        assert!(out.contains("tokens 40 -> 44"));
        assert!(out.contains("CHANGED"));
        assert!(out.contains("same"));
        assert!(out.contains("10 -> 12"));
        assert!(out.contains("1 of 5 layers changed: 2 layer"), "{out}");
        let out = render_diff(&diff_doc(&[false; 5], true));
        assert!(out.contains("layers 1-5 identical\n"), "{out}");
        assert!(!out.contains("CHANGED"));
        assert!(out.contains("the packs are identical"));
    }

    #[test]
    fn verify_says_ok_or_differs() {
        let ok = Value::object([
            ("match", Value::Bool(true)),
            ("stored", s("abc")),
            ("current", s("abc")),
        ]);
        assert_eq!(
            render_verify(&ok),
            "OK: the pack compiled now has the digest abc\n"
        );
        let mut bad = diff_doc(&[false, true], false);
        if let Value::Object(m) = &mut bad {
            m.insert("match".into(), Value::Bool(false));
            m.insert("stored".into(), s("old"));
            m.insert("current".into(), s("new"));
        }
        let out = render_verify(&bad);
        assert!(out.starts_with("DIFFERS: "), "{out}");
        assert!(out.contains("digest new"));
        assert!(out.contains("stored pack old"));
        assert!(out.contains("1 of 2 layers changed: 2 layer"), "{out}");
    }

    fn explain_doc(subject: Value) -> Value {
        let l = |id: u64, name: &str, sheddable: bool| {
            Value::object([
                ("id", Value::int(id)),
                ("name", s(name)),
                ("purpose", s("why \u{1b}[1mit\u{1b}[0m exists")),
                ("percent", Value::int(25u64)),
                ("budget", Value::int(1000u64)),
                ("sheddable", Value::Bool(sheddable)),
            ])
        };
        Value::object([
            ("schema", s("fleet.context.explain/1")),
            (
                "profile",
                Value::object([
                    ("name", s("default")),
                    ("max_tokens", Value::int(4000u64)),
                    ("chars_per_token", Value::int(4u64)),
                    ("safety_percent", Value::int(10u64)),
                ]),
            ),
            ("policy", s("label")),
            ("knowledge_file", Value::Null),
            ("estimator", s("ceil(chars / chars_per_token)")),
            (
                "shrink_order",
                Value::array([Value::int(4u64), Value::int(2u64)]),
            ),
            (
                "layers",
                Value::array([l(1, "charter", false), l(2, "knowledge", true)]),
            ),
            ("subject", subject),
        ])
    }

    #[test]
    fn explain_lists_profile_layers_and_shrink_order() {
        let out = render_explain(&explain_doc(Value::Null));
        assert!(
            out.starts_with(
                "profile default: 4000 tokens at most, 4 characters per token, 10% safety margin\n"
            ),
            "{out}"
        );
        assert!(out.contains("estimator: ceil(chars / chars_per_token)"));
        assert!(out.contains("policy: label"));
        assert!(out.contains("knowledge file: none"));
        assert!(out.contains("never cut"));
        assert!(out.contains("dropped when the pack is too large"));
        assert!(out.contains("25%"));
        assert!(out.contains("in this order: 4, 2"), "{out}");
        assert!(!out.contains("for task"));
        assert!(!out.contains('\u{1b}'), "{out:?}");
        let subject = Value::object([("run", s("r1")), ("task", s("t1"))]);
        let out = render_explain(&explain_doc(subject));
        assert!(out.contains("for task r1/t1\n"), "{out}");
    }

    #[test]
    fn handoff_is_the_text_with_a_final_newline() {
        let doc = Value::object([("text", s("handoff\nbody"))]);
        assert_eq!(render_handoff(&doc), "handoff\nbody\n");
        let doc = Value::object([("text", s("done\n"))]);
        assert_eq!(render_handoff(&doc), "done\n");
    }
}
