//! Tests of the read-only groups view: state, keys and drawing.

use super::keys::{handle_key, Action};
use super::model::Model;
use super::ui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use fleet_domain::value::Value;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn list(ids: &[&str]) -> Value {
    Value::object([(
        "groups",
        Value::array(ids.iter().map(|id| {
            Value::object([
                ("id", Value::str(*id)),
                ("run", Value::str("r1")),
                ("title", Value::str("title\u{1b}[31m red\nline")),
                ("status", Value::str("finished")),
                ("winner", Value::Null),
                (
                    "counts",
                    Value::object([
                        ("members", Value::int(3u32)),
                        ("completed", Value::int(2u32)),
                        ("failed", Value::int(1u32)),
                    ]),
                ),
            ])
        })),
    )])
}

fn screen(m: &Model) -> String {
    let mut t = Terminal::new(TestBackend::new(100, 20)).unwrap();
    t.draw(|f| ui::render(f, m)).unwrap();
    let buf = t.backend().buffer().clone();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn g_opens_the_view_and_only_looking_keys_work_in_it() {
    let mut m = Model::new();
    assert_eq!(handle_key(&mut m, key(KeyCode::Char('g'))), Action::Refresh);
    assert!(m.groups_view);
    m.update_groups(&list(&["fan1", "fan2"]));
    assert_eq!(handle_key(&mut m, key(KeyCode::Char('j'))), Action::Refresh);
    assert_eq!(m.selected_group().unwrap().id, "fan2");
    // nothing here stops, retries, merges, approves or picks
    for c in ['s', 't', 'm', 'a', 'd', 'p', 'x'] {
        assert_eq!(
            handle_key(&mut m, key(KeyCode::Char(c))),
            Action::None,
            "{c}"
        );
    }
    assert_eq!(handle_key(&mut m, key(KeyCode::Enter)), Action::None);
    assert_eq!(handle_key(&mut m, key(KeyCode::Esc)), Action::None);
    assert!(!m.groups_view);
    assert_eq!(handle_key(&mut m, key(KeyCode::Char('g'))), Action::Refresh);
    assert_eq!(handle_key(&mut m, key(KeyCode::Char('q'))), Action::Quit);
}

#[test]
fn the_model_cleans_and_keeps_the_selection() {
    let mut m = Model::new();
    m.update_groups(&list(&["fan1", "fan2"]));
    assert!(!m.groups[0].title.contains('\u{1b}') && !m.groups[0].title.contains('\n'));
    m.move_group_selection(1);
    m.update_groups(&list(&["fan0", "fan1", "fan2"]));
    assert_eq!(m.selected_group().unwrap().id, "fan2");
    // text for a group that is not the selected one is dropped
    m.set_group_text("fan1", "late");
    assert!(m.group_text.is_empty());
    m.set_group_text("fan2", "line1\n\u{1b}]0;x\u{7}line2\nline3");
    assert!(!m.group_text.contains('\u{1b}'));
    m.scroll_group_text(100);
    assert_eq!(m.group_scroll, 2);
    m.scroll_group_text(-100);
    assert_eq!(m.group_scroll, 0);
    m.update_groups(&list(&[]));
    assert!(m.selected_group().is_none() && m.group_text.is_empty());
}

#[test]
fn the_view_shows_the_groups_the_comparison_and_how_to_pick() {
    let mut m = Model::new();
    m.groups_view = true;
    assert!(screen(&m).contains("There are no fan-out groups."));
    m.update_groups(&list(&["fan1"]));
    m.set_group_text("fan1", "group fan1 [finished]\nranking (advisory only): x");
    let s = screen(&m);
    assert!(s.contains("fan1 [finished] 2/3 ok"), "{s}");
    assert!(s.contains("Comparison of fan1 (advisory ranking)"), "{s}");
    assert!(s.contains("ranking (advisory only): x"), "{s}");
    assert!(s.contains("fleet pick <group> <task>"), "{s}");
    assert!(!s.contains('\u{1b}'));
}
