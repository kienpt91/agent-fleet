//! Key bindings: a pure function from (model, key) to an [`Action`] for the event loop.
//!
//! Approvals carry the digest the user is looking at, so the daemon can refuse a decision when
//! the approval was replaced between render and keypress.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use fleet_domain::limits::MAX_ANSWER;

use super::model::{Focus, Model};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Refresh,
    Approve { id: String, digest: String },
    Deny { id: String, digest: String },
    ResolveGate { id: String, answer: String },
    OpenAttach(String),
    Stop(String),
    Retry(String),
    Merge(String),
}

pub fn handle_key(model: &mut Model, key: KeyEvent) -> Action {
    if key.kind == KeyEventKind::Release {
        return Action::None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Action::Quit;
    }
    if model.input.is_some() {
        return handle_input(model, key);
    }
    if model.show_help {
        return match key.code {
            KeyCode::Esc | KeyCode::Char('?') => {
                model.show_help = false;
                Action::None
            }
            KeyCode::Char('q') => Action::Quit,
            _ => Action::None,
        };
    }
    // Other modifiers (Alt, Ctrl) are not bindings; do not let Ctrl-D act as `d`.
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return Action::None;
    }
    if model.groups_view {
        return handle_groups(model, key);
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
        KeyCode::Char('?') => {
            model.show_help = true;
            Action::None
        }
        KeyCode::Char('j') | KeyCode::Down => {
            model.move_selection(1);
            Action::None
        }
        KeyCode::Char('k') | KeyCode::Up => {
            model.move_selection(-1);
            Action::None
        }
        KeyCode::Tab => {
            model.focus = model.next_focus();
            Action::None
        }
        KeyCode::Char('o') => {
            model.focus = if model.focus == Focus::Output {
                Focus::Tasks
            } else {
                Focus::Output
            };
            Action::None
        }
        KeyCode::Char('g') => {
            model.groups_view = true;
            Action::Refresh
        }
        KeyCode::Char('r') => Action::Refresh,
        KeyCode::Char('a') => decide(model, true),
        KeyCode::Char('d') => decide(model, false),
        KeyCode::Char('s') => selected_task(model).map_or(Action::None, Action::Stop),
        KeyCode::Char('t') => selected_task(model).map_or(Action::None, Action::Retry),
        KeyCode::Char('m') => selected_task(model).map_or(Action::None, Action::Merge),
        KeyCode::Enter => enter(model),
        _ => Action::None,
    }
}

/// The groups view only looks: moving, scrolling, refreshing and leaving. There is no key that
/// creates a group or picks a winner (a pick is a merge: `fleet pick <group> <task>`).
fn handle_groups(model: &mut Model, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Char('?') => {
            model.show_help = true;
            Action::None
        }
        KeyCode::Char('g') | KeyCode::Esc => {
            model.groups_view = false;
            Action::None
        }
        KeyCode::Char('j') | KeyCode::Down => {
            model.move_group_selection(1);
            Action::Refresh
        }
        KeyCode::Char('k') | KeyCode::Up => {
            model.move_group_selection(-1);
            Action::Refresh
        }
        KeyCode::PageDown => {
            model.scroll_group_text(10);
            Action::None
        }
        KeyCode::PageUp => {
            model.scroll_group_text(-10);
            Action::None
        }
        KeyCode::Char('r') => Action::Refresh,
        _ => Action::None,
    }
}

fn selected_task(model: &Model) -> Option<String> {
    model.selected_key()
}

fn decide(model: &mut Model, approve: bool) -> Action {
    if model.focus != Focus::Approvals {
        model.set_message("focus the approvals panel (Tab) to approve or deny");
        return Action::None;
    }
    let Some(a) = model.selected_approval() else {
        return Action::None;
    };
    let (id, digest) = (a.id.clone(), a.digest.clone());
    if approve {
        Action::Approve { id, digest }
    } else {
        Action::Deny { id, digest }
    }
}

fn enter(model: &mut Model) -> Action {
    match model.focus {
        Focus::Gates => {
            if model.selected_gate().is_some() {
                model.input = Some(String::new());
            }
            Action::None
        }
        Focus::Tasks | Focus::Output => {
            let Some(task) = model.selected_task() else {
                return Action::None;
            };
            if !task.live {
                model.set_message("task has no live session to attach to");
                return Action::None;
            }
            Action::OpenAttach(task.key())
        }
        Focus::Approvals | Focus::Messages => Action::None,
    }
}

fn handle_input(model: &mut Model, key: KeyEvent) -> Action {
    match key.code {
        KeyCode::Esc => {
            model.input = None;
            Action::None
        }
        KeyCode::Enter => {
            let Some(id) = model.selected_gate().map(|g| g.id.clone()) else {
                // The gate was resolved while the user was typing.
                model.input = None;
                return Action::None;
            };
            let answer = model.input.take().unwrap_or_default();
            if answer.trim().is_empty() {
                model.input = Some(answer);
                return Action::None;
            }
            Action::ResolveGate { id, answer }
        }
        KeyCode::Backspace => {
            if let Some(s) = model.input.as_mut() {
                s.pop();
            }
            Action::None
        }
        KeyCode::Char(c)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !c.is_control()
                && !fleet_domain::ids::is_invisible(c) =>
        {
            if let Some(s) = model.input.as_mut() {
                if s.len() + c.len_utf8() <= MAX_ANSWER {
                    s.push(c);
                }
            }
            Action::None
        }
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::model::fixtures::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ch(c: char) -> KeyEvent {
        press(KeyCode::Char(c))
    }

    fn model() -> Model {
        let s = status(vec![
            task("t1", "one", "running", &[]),
            task("t2", "two", "pending", &[]),
        ]);
        let s = with(
            s,
            "approvals",
            vec![approval("a1", "dig1"), approval("a2", "dig2")],
        );
        let s = with(s, "gates", vec![gate("g1", "which db?", true)]);
        let mut m = Model::new();
        m.update_status(&s);
        m
    }

    #[test]
    fn quit_keys_and_help_precedence() {
        let mut m = model();
        assert_eq!(handle_key(&mut m, ch('q')), Action::Quit);
        assert_eq!(handle_key(&mut m, press(KeyCode::Esc)), Action::Quit);
        assert_eq!(
            handle_key(
                &mut m,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            Action::Quit
        );
        // Esc closes help first, the next Esc quits.
        assert_eq!(handle_key(&mut m, ch('?')), Action::None);
        assert!(m.show_help);
        assert_eq!(handle_key(&mut m, press(KeyCode::Esc)), Action::None);
        assert!(!m.show_help);
        assert_eq!(handle_key(&mut m, press(KeyCode::Esc)), Action::Quit);
    }

    #[test]
    fn help_toggles_and_swallows_other_keys() {
        let mut m = model();
        handle_key(&mut m, ch('?'));
        assert!(m.show_help);
        assert_eq!(handle_key(&mut m, ch('s')), Action::None);
        assert_eq!(handle_key(&mut m, ch('j')), Action::None);
        assert_eq!(m.selected, 0);
        handle_key(&mut m, ch('?'));
        assert!(!m.show_help);
    }

    #[test]
    fn movement_and_focus() {
        let mut m = model();
        handle_key(&mut m, ch('j'));
        assert_eq!(m.selected, 1);
        handle_key(&mut m, press(KeyCode::Down));
        assert_eq!(m.selected, 1);
        handle_key(&mut m, press(KeyCode::Up));
        handle_key(&mut m, ch('k'));
        assert_eq!(m.selected, 0);
        handle_key(&mut m, press(KeyCode::Tab));
        assert_eq!(m.focus, Focus::Gates);
        handle_key(&mut m, ch('o'));
        assert_eq!(m.focus, Focus::Output);
        handle_key(&mut m, ch('o'));
        assert_eq!(m.focus, Focus::Tasks);
    }

    #[test]
    fn messages_pane_is_focusable_only_with_channels_and_is_read_only() {
        let mut m = model();
        for _ in 0..3 {
            handle_key(&mut m, press(KeyCode::Tab));
            assert_ne!(m.focus, Focus::Messages);
        }
        m.update_messages(&msg_list(
            vec![msg_channel("review", true, "frozen")],
            vec![msg_message("m1", "hi", "blocked", "why")],
        ));
        m.focus = Focus::Approvals;
        handle_key(&mut m, press(KeyCode::Tab));
        assert_eq!(m.focus, Focus::Messages);
        handle_key(&mut m, ch('j'));
        assert_eq!(m.msg_selected, 1);
        for code in [KeyCode::Enter, KeyCode::Char('a'), KeyCode::Char('d')] {
            assert_eq!(handle_key(&mut m, press(code)), Action::None);
        }
        assert!(m.input.is_none());
        handle_key(&mut m, press(KeyCode::Tab));
        assert_eq!(m.focus, Focus::Output);
    }

    #[test]
    fn approve_and_deny_only_in_approvals_focus() {
        let mut m = model();
        assert_eq!(handle_key(&mut m, ch('a')), Action::None);
        assert_eq!(handle_key(&mut m, ch('d')), Action::None);
        m.focus = Focus::Approvals;
        handle_key(&mut m, ch('j'));
        assert_eq!(
            handle_key(&mut m, ch('a')),
            Action::Approve {
                id: "a2".into(),
                digest: "dig2".into()
            }
        );
        assert_eq!(
            handle_key(&mut m, ch('d')),
            Action::Deny {
                id: "a2".into(),
                digest: "dig2".into()
            }
        );
    }

    #[test]
    fn hotkeys_do_nothing_without_a_selection() {
        let mut m = Model::new();
        m.update_status(&status(vec![]));
        m.focus = Focus::Approvals;
        for c in ['a', 'd', 's', 't', 'm'] {
            assert_eq!(handle_key(&mut m, ch(c)), Action::None, "{c}");
        }
        assert_eq!(handle_key(&mut m, press(KeyCode::Enter)), Action::None);
        m.focus = Focus::Gates;
        handle_key(&mut m, press(KeyCode::Enter));
        assert!(m.input.is_none());
        m.focus = Focus::Tasks;
        assert_eq!(handle_key(&mut m, press(KeyCode::Enter)), Action::None);
    }

    #[test]
    fn task_actions_use_the_selected_key() {
        let mut m = model();
        handle_key(&mut m, ch('j'));
        assert_eq!(handle_key(&mut m, ch('s')), Action::Stop("r1/t2".into()));
        assert_eq!(handle_key(&mut m, ch('t')), Action::Retry("r1/t2".into()));
        assert_eq!(handle_key(&mut m, ch('m')), Action::Merge("r1/t2".into()));
        assert_eq!(handle_key(&mut m, ch('r')), Action::Refresh);
    }

    #[test]
    fn enter_attaches_only_to_live_tasks() {
        let mut m = model();
        assert_eq!(
            handle_key(&mut m, press(KeyCode::Enter)),
            Action::OpenAttach("r1/t1".into())
        );
        handle_key(&mut m, ch('j'));
        assert_eq!(handle_key(&mut m, press(KeyCode::Enter)), Action::None);
        assert!(m.message.contains("no live session"));
    }

    #[test]
    fn modified_keys_are_not_bindings() {
        let mut m = model();
        m.focus = Focus::Approvals;
        let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert_eq!(handle_key(&mut m, ctrl_d), Action::None);
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert_eq!(handle_key(&mut m, release), Action::None);
    }

    #[test]
    fn answer_input_flow() {
        let mut m = model();
        m.focus = Focus::Gates;
        handle_key(&mut m, press(KeyCode::Enter));
        assert_eq!(m.input.as_deref(), Some(""));
        // Hotkeys are text while typing.
        for c in "sqlx".chars() {
            assert_eq!(handle_key(&mut m, ch(c)), Action::None);
        }
        handle_key(&mut m, press(KeyCode::Backspace));
        assert_eq!(m.input.as_deref(), Some("sql"));
        assert_eq!(
            handle_key(&mut m, press(KeyCode::Enter)),
            Action::ResolveGate {
                id: "g1".into(),
                answer: "sql".into()
            }
        );
        assert!(m.input.is_none());
    }

    #[test]
    fn answer_input_cancel_and_empty_submit() {
        let mut m = model();
        m.focus = Focus::Gates;
        handle_key(&mut m, press(KeyCode::Enter));
        // An empty answer is not submitted and the input stays open.
        assert_eq!(handle_key(&mut m, press(KeyCode::Enter)), Action::None);
        assert!(m.input.is_some());
        handle_key(&mut m, ch('x'));
        assert_eq!(handle_key(&mut m, press(KeyCode::Esc)), Action::None);
        assert!(m.input.is_none());
    }

    #[test]
    fn answer_input_is_bounded_and_rejects_control_characters() {
        let mut m = model();
        m.focus = Focus::Gates;
        handle_key(&mut m, press(KeyCode::Enter));
        handle_key(&mut m, ch('\u{7}'));
        handle_key(&mut m, ch('\u{1b}'));
        assert_eq!(m.input.as_deref(), Some(""));
        for _ in 0..(MAX_ANSWER + 100) {
            handle_key(&mut m, ch('x'));
        }
        assert_eq!(m.input.as_ref().map(String::len), Some(MAX_ANSWER));
    }

    #[test]
    fn gate_resolved_while_typing_closes_the_input() {
        let mut m = model();
        m.focus = Focus::Gates;
        handle_key(&mut m, press(KeyCode::Enter));
        handle_key(&mut m, ch('x'));
        m.update_status(&with(
            status(vec![task("t1", "one", "running", &[])]),
            "gates",
            vec![],
        ));
        assert_eq!(handle_key(&mut m, press(KeyCode::Enter)), Action::None);
        assert!(m.input.is_none());
    }
}
