//! Pure rendering of the dashboard. Everything drawn comes from [`Model`], whose strings were
//! sanitized on the way in; the output pane sanitizes once more because it shows the least
//! trusted text of all (what a worker printed).

use fleet_domain::sanitize::sanitize_text;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table};
use ratatui::Frame;

use super::model::{clean_line, Focus, Model, TaskRow};

pub const MIN_WIDTH: u16 = 40;
pub const MIN_HEIGHT: u16 = 10;
const DIGEST_PREFIX: usize = 12;
const KEY_HINTS: &str =
    "q quit  j/k move  Tab focus  Enter attach/answer  a/d approve/deny  s stop  t retry  m merge  g groups  r refresh  ? help";

pub fn status_style(status: &str) -> Style {
    match status {
        "pending" => Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        "ready" => Style::new().fg(Color::Cyan),
        "running" => Style::new().fg(Color::Yellow),
        "review" => Style::new().fg(Color::Magenta),
        "completed" => Style::new().fg(Color::Green),
        "failed" => Style::new().fg(Color::Red),
        "blocked" => Style::new().fg(Color::LightRed),
        _ => Style::new(),
    }
}

fn fit(s: &str, width: usize) -> String {
    s.chars().take(width).collect()
}

/// Time left until `expires_ms`, as `90s` / `2m05s`, or `expired`.
pub fn time_left(expires_ms: u64, now_ms: u64) -> String {
    let secs = expires_ms.saturating_sub(now_ms) / 1000;
    match secs {
        0 => "expired".to_string(),
        1..=59 => format!("{secs}s"),
        _ => format!("{}m{:02}s", secs / 60, secs % 60),
    }
}

pub fn header_text(model: &Model) -> String {
    let c = model.visible_counts();
    let at = |i: usize| c.get(i).copied().unwrap_or(0);
    let name = if model.daemon.version.is_empty() {
        "fleet".to_string()
    } else {
        format!("fleet {}", model.daemon.version)
    };
    let undo = if model.daemon.undo == "available" {
        "on"
    } else {
        "off"
    };
    let rss = model
        .daemon
        .rss_kb
        .map_or_else(|| "-".to_string(), |kb| format!("{}M", kb / 1024));
    let mut text = format!(
        "{name} | runs {} | pend {} rdy {} run {} rev {} ok {} fail {} blk {} | sess {}/{} | undo {undo} | rss {rss}",
        model.runs.len(),
        at(0),
        at(1),
        at(2),
        at(3),
        at(4),
        at(5),
        at(6),
        model.daemon.sessions,
        model.daemon.max_sessions,
    );
    if !model.connected {
        text.push_str(" | OFFLINE");
    }
    text
}

fn panel(title: String, focused: bool) -> Block<'static> {
    let border = if focused {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(title)
}

fn selection_style(focused: bool) -> Style {
    if focused {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new().add_modifier(Modifier::BOLD)
    }
}

/// First row index to show so that `selected` is visible in a window of `visible` rows.
fn window_start(selected: usize, visible: usize) -> usize {
    (selected.saturating_add(1)).saturating_sub(visible.max(1))
}

pub fn render(frame: &mut Frame, model: &Model) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        frame.render_widget(Paragraph::new("terminal too small"), area);
        return;
    }
    if model.groups_view {
        render_groups_view(frame, model, area);
        return;
    }
    let bottom = (area.height / 4).clamp(4, 8);
    let [header, body, panels, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(bottom),
        Constraint::Length(2),
    ])
    .areas(area);

    render_header(frame, model, header);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(body);
    render_tasks(frame, model, left);
    render_output(frame, model, right);
    let [approvals, gates] =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(panels);
    render_approvals(frame, model, approvals);
    render_gates(frame, model, gates);
    render_footer(frame, model, footer);
    if model.show_help {
        render_help(frame, area);
    }
}

const GROUP_HINTS: &str =
    "q quit  j/k group  PgUp/PgDn scroll  g back  ? help  view only: fleet pick <group> <task>";

/// The groups view: the list of fan-out groups and the comparison of the selected one, as the
/// command line prints it. Everything in it was cleaned on the way into the model.
fn render_groups_view(frame: &mut Frame, model: &Model, area: Rect) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .areas(area);
    render_header(frame, model, header);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)]).areas(body);

    let block = panel(format!("Fan-out groups ({})", model.groups.len()), true);
    let inner = block.inner(left);
    frame.render_widget(block, left);
    let width = usize::from(inner.width);
    if model.groups.is_empty() {
        frame.render_widget(Paragraph::new("There are no fan-out groups."), inner);
    } else {
        let visible = usize::from(inner.height);
        let start = window_start(model.group_selected, visible);
        let rows: Vec<Line> = model
            .groups
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(i, g)| {
                let winner = if g.winner.is_empty() {
                    String::new()
                } else {
                    format!(" -> {}", g.winner)
                };
                let text = fit(
                    &format!(
                        "{} [{}] {}/{} ok{} {}",
                        g.id, g.status, g.completed, g.members, winner, g.title
                    ),
                    width,
                );
                Line::from(Span::styled(
                    text,
                    selection_style(i == model.group_selected),
                ))
            })
            .collect();
        frame.render_widget(Paragraph::new(rows), inner);
    }

    let title = match model.selected_group() {
        Some(g) => format!("Comparison of {} (advisory ranking)", g.id),
        None => "Comparison".to_string(),
    };
    let block = panel(title, false);
    let inner = block.inner(right);
    frame.render_widget(block, right);
    let width = usize::from(inner.width);
    let height = usize::from(inner.height);
    let lines: Vec<Line> = if model.group_text.is_empty() {
        vec![Line::from(Span::styled(
            if model.groups.is_empty() {
                "Create one with: fleet fanout --run <run> --title <text> --spec <text> --workers a,b"
            } else {
                "(loading the comparison)"
            },
            Style::new().fg(Color::DarkGray),
        ))]
    } else {
        model
            .group_text
            .lines()
            .skip(model.group_scroll)
            .take(height)
            .map(|l| {
                Line::from(fit(
                    &sanitize_text(l.replace('\t', "    ").as_bytes()),
                    width,
                ))
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), inner);

    let hints = Line::from(Span::styled(
        fit(GROUP_HINTS, usize::from(footer.width)),
        Style::new().fg(Color::DarkGray),
    ));
    let message = Line::from(Span::styled(
        fit(&clean_line(&model.message), usize::from(footer.width)),
        Style::new().fg(Color::Yellow),
    ));
    frame.render_widget(Paragraph::new(vec![hints, message]), footer);
    if model.show_help {
        render_help(frame, area);
    }
}

fn render_header(frame: &mut Frame, model: &Model, area: Rect) {
    let style = if model.connected {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
    };
    frame.render_widget(
        Paragraph::new(Span::styled(header_text(model), style)),
        area,
    );
}

fn render_tasks(frame: &mut Frame, model: &Model, area: Rect) {
    let focused = model.focus == Focus::Tasks;
    let block = panel(format!("Tasks ({})", model.tasks.len()), focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if model.tasks.is_empty() {
        let text = if model.loaded {
            "No runs or tasks yet. Load a plan or add tasks to a run to get started."
        } else {
            "Waiting for daemon status..."
        };
        frame.render_widget(
            Paragraph::new(text).wrap(ratatui::widgets::Wrap { trim: true }),
            inner,
        );
        return;
    }
    let visible = usize::from(inner.height.saturating_sub(1));
    let start = window_start(model.selected, visible);
    let rows: Vec<Row> = model
        .tasks
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(i, t)| task_row(t, i == model.selected, focused))
        .collect();
    let header = Row::new(["", "TASK", "STATUS", "DEPS", "TITLE"])
        .style(Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));
    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(10),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Min(5),
        ],
    )
    .header(header);
    frame.render_widget(table, inner);
}

fn task_row(t: &TaskRow, selected: bool, focused: bool) -> Row<'static> {
    let deps = if t.deps.is_empty() {
        "-".to_string()
    } else {
        format!("<{}", t.deps.join(","))
    };
    let row = Row::new([
        ratatui::widgets::Cell::from(if selected { ">" } else { " " }),
        ratatui::widgets::Cell::from(t.key()),
        ratatui::widgets::Cell::from(t.status.clone()).style(status_style(&t.status)),
        ratatui::widgets::Cell::from(deps).style(Style::new().fg(Color::DarkGray)),
        ratatui::widgets::Cell::from(t.title.clone()),
    ]);
    if selected {
        row.style(selection_style(focused))
    } else {
        row
    }
}

fn detail_lines(t: &TaskRow, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut head = format!("{} attempt {}", t.title, t.attempt);
    if let Some(w) = &t.worker {
        head.push_str(&format!(" worker {w}"));
    }
    lines.push(Line::from(Span::styled(
        fit(&head, width),
        Style::new().add_modifier(Modifier::BOLD),
    )));
    if let Some(b) = &t.block {
        lines.push(Line::from(Span::styled(
            fit(
                &format!("blocked: {} {} {}", b.kind, b.subject, b.detail),
                width,
            ),
            status_style("blocked"),
        )));
    }
    if let Some(f) = &t.failure {
        lines.push(Line::from(Span::styled(
            fit(&format!("failure: {f}"), width),
            status_style("failed"),
        )));
    }
    if let Some(s) = &t.summary {
        lines.push(Line::from(fit(&format!("summary: {s}"), width)));
    }
    for g in &t.gates {
        let (mark, color) = if g.passed {
            ("pass", Color::Green)
        } else {
            ("FAIL", Color::Red)
        };
        lines.push(Line::from(Span::styled(
            fit(&format!("gate {} {mark} {}", g.name, g.summary), width),
            Style::new().fg(color),
        )));
    }
    lines
}

fn render_output(frame: &mut Frame, model: &Model, area: Rect) {
    let focused = model.focus == Focus::Output;
    let title = match model.selected_task() {
        Some(t) => format!("Output {}{}", t.key(), if t.live { " [live]" } else { "" }),
        None => "Output".to_string(),
    };
    let block = panel(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let (width, height) = (usize::from(inner.width), usize::from(inner.height));

    let Some(task) = model.selected_task() else {
        frame.render_widget(Paragraph::new("No task selected."), inner);
        return;
    };
    let mut details = detail_lines(task, width);
    details.truncate(4.min(height / 2));
    let detail_h = u16::try_from(details.len()).unwrap_or(0);
    let [top, out] =
        Layout::vertical([Constraint::Length(detail_h), Constraint::Min(0)]).areas(inner);
    frame.render_widget(Paragraph::new(details), top);

    let rows = usize::from(out.height);
    let mut lines: Vec<Line> = model
        .tail
        .lines()
        .rev()
        .take(rows)
        .map(|l| {
            Line::from(fit(
                &sanitize_text(l.replace('\t', "    ").as_bytes()),
                width,
            ))
        })
        .collect();
    lines.reverse();
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no output yet)",
            Style::new().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(Paragraph::new(lines), out);
}

fn render_approvals(frame: &mut Frame, model: &Model, area: Rect) {
    let focused = model.focus == Focus::Approvals;
    let block = panel(format!("Approvals ({})", model.approvals.len()), focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if model.approvals.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "none pending",
                Style::new().fg(Color::DarkGray),
            )),
            inner,
        );
        return;
    }
    let width = usize::from(inner.width);
    let visible = usize::from(inner.height);
    let start = window_start(model.approval_selected, visible);
    let lines: Vec<Line> = model
        .approvals
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(i, a)| {
            let selected = i == model.approval_selected;
            // Digest before the free-form action text: it is what the user is approving, so it
            // must survive truncation.
            let text = format!(
                "{} {} {} {}/{} {} {}",
                if selected { ">" } else { " " },
                a.id,
                a.purpose,
                a.run,
                a.task,
                fit(&a.digest, DIGEST_PREFIX),
                time_left(a.expires_ms, model.now_ms),
            );
            let text = format!("{text} {}", a.display);
            let style = if selected {
                selection_style(focused)
            } else {
                Style::new()
            };
            Line::from(Span::styled(fit(&text, width), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_gates(frame: &mut Frame, model: &Model, area: Rect) {
    let focused = model.focus == Focus::Gates;
    let block = panel(format!("Decisions ({})", model.gates.len()), focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if model.gates.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("none open", Style::new().fg(Color::DarkGray))),
            inner,
        );
        return;
    }
    let width = usize::from(inner.width);
    let visible = usize::from(inner.height);
    let start = window_start(model.gate_selected, visible);
    let lines: Vec<Line> = model
        .gates
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(i, g)| {
            let selected = i == model.gate_selected;
            let text = format!(
                "{} {} {}/{} {}",
                if selected { ">" } else { " " },
                g.id,
                g.run,
                g.task,
                g.text
            );
            let style = if selected {
                selection_style(focused)
            } else {
                Style::new()
            };
            Line::from(Span::styled(fit(&text, width), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_footer(frame: &mut Frame, model: &Model, area: Rect) {
    let width = usize::from(area.width);
    let second = match &model.input {
        Some(answer) => {
            let choices = model
                .selected_gate()
                .map(|g| g.choices.join("/"))
                .filter(|c| !c.is_empty())
                .map_or_else(String::new, |c| format!(" ({c})"));
            let prompt = format!("answer{choices}> ");
            // Show the end of a long answer so the cursor position stays visible.
            let room = width.saturating_sub(prompt.chars().count() + 1);
            let count = answer.chars().count();
            let shown: String = answer
                .chars()
                .filter(|c| !c.is_control() && !fleet_domain::ids::is_invisible(*c))
                .skip(count.saturating_sub(room))
                .collect();
            Line::from(vec![
                Span::styled(prompt, Style::new().fg(Color::Cyan)),
                Span::raw(shown),
                Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED)),
            ])
        }
        None => Line::from(Span::styled(
            fit(&clean_line(&model.message), width),
            Style::new().fg(Color::Yellow),
        )),
    };
    let hints = Line::from(Span::styled(
        fit(KEY_HINTS, width),
        Style::new().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(vec![hints, second]), area);
}

fn render_help(frame: &mut Frame, area: Rect) {
    let width = area.width.saturating_sub(4).min(64);
    let height = area.height.saturating_sub(2).min(16);
    let rect = Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    );
    let lines = [
        "j / k, Up / Down   move selection",
        "Tab                cycle panels",
        "o                  focus output pane",
        "Enter              attach to a live task",
        "                   (Ctrl-] detaches)",
        "Enter on decision  type an answer, Enter sends",
        "a / d              approve / deny (approvals panel)",
        "s / t / m          stop / retry / merge task",
        "g                  fan-out groups and comparison (view only)",
        "r                  refresh now",
        "?                  close this help",
        "q, Esc             quit",
    ];
    let text: Vec<Line> = lines.iter().map(|l| Line::from(*l)).collect();
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("Help")),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::model::fixtures::*;
    use fleet_domain::value::Value;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    fn draw(model: &Model, w: u16, h: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        terminal.draw(|f| render(f, model)).expect("draw");
        terminal.backend().buffer().clone()
    }

    fn lines(buf: &Buffer) -> Vec<String> {
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf.cell((x, y)).map_or(" ", |c| c.symbol()))
                    .collect::<String>()
            })
            .collect()
    }

    fn screen(model: &Model, w: u16, h: u16) -> String {
        lines(&draw(model, w, h)).join("\n")
    }

    fn sample() -> Model {
        let s = status(vec![
            task("t1", "one", "running", &[]),
            task("t2", "two", "pending", &["t1"]),
            task("t3", "three", "failed", &[]),
        ]);
        let digest = "0123456789abcdef0123";
        let s = with(s, "approvals", vec![approval("a1", digest)]);
        let s = with(
            s,
            "gates",
            vec![gate("g1", "which database should I use?", true)],
        );
        let mut m = Model::new();
        m.update_status(&s);
        m.set_tail("r1/t1", "compiling\nlinking\n");
        m
    }

    #[test]
    fn full_render_header_and_task_row_are_exact() {
        let m = sample();
        let rows = lines(&draw(&m, 100, 30));
        assert_eq!(
            rows.first().map(|l| l.trim_end().to_string()).as_deref(),
            Some("fleet 0.1.0 | runs 1 | pend 1 rdy 0 run 1 rev 0 ok 0 fail 1 blk 0 | sess 2/8 | undo on | rss 20M")
        );
        // Whitespace-normalised: column padding is a layout detail, the cell contents are not.
        let left_panel = |row: &str| {
            let inner: String = row.chars().skip(1).take(43).collect();
            inner.split_whitespace().collect::<Vec<_>>().join(" ")
        };
        let task_rows: Vec<String> = rows
            .iter()
            .skip(1)
            .take(20)
            .map(|r| left_panel(r))
            .filter(|r| r.contains("r1/t"))
            .collect();
        assert_eq!(
            task_rows,
            vec![
                "> r1/t1 running - one".to_string(),
                "r1/t2 pending <t1 two".to_string(),
                "r1/t3 failed - three".to_string(),
            ]
        );
    }

    #[test]
    fn shows_approval_digest_prefix_and_gate_text() {
        let text = screen(&sample(), 100, 30);
        assert!(text.contains("0123456789ab"), "{text}");
        assert!(!text.contains("0123456789abc"), "digest must be cut at 12");
        assert!(text.contains("a1 merge r1/t1"));
        assert!(text.contains("1m30s"));
        assert!(text.contains("g1 r1/t1 which database"));
        assert!(text.contains("compiling"));
        assert!(text.contains("Output r1/t1 [live]"));
    }

    #[test]
    fn empty_status_renders_an_empty_state() {
        let mut m = Model::new();
        assert!(screen(&m, 100, 30).contains("Waiting for daemon status"));
        m.update_status(&status(vec![]));
        let text = screen(&m, 100, 30);
        assert!(text.contains("No runs or tasks yet"), "{text}");
        assert!(text.contains("No task selected"));
        assert!(text.contains("none pending") && text.contains("none open"));
    }

    #[test]
    fn hostile_text_never_reaches_the_buffer_as_control_characters() {
        let evil = "t\x1b]52;c;AAAA\x07\x1b[2J\u{202e}evil\x00\x07";
        let mut denied_digest = approval("a1", "d\x1b[31mXX\u{202e}");
        if let Value::Object(m) = &mut denied_digest {
            m.insert("display".into(), Value::str("rm\x1b[2J\u{2066}x"));
        }
        let mut t = task("t1", evil, "running", &[]);
        if let Value::Object(m) = &mut t {
            m.insert("summary".into(), Value::str(evil));
            m.insert("failure".into(), Value::str(evil));
            m.insert("worker".into(), Value::str(evil));
            m.insert(
                "block".into(),
                Value::object([
                    ("kind", Value::str(evil)),
                    ("subject", Value::str(evil)),
                    ("detail", Value::str(evil)),
                ]),
            );
        }
        let s = with(status(vec![t]), "approvals", vec![denied_digest]);
        let s = with(s, "gates", vec![gate("g\x1b[1m1", evil, true)]);
        let mut m = Model::new();
        m.update_status(&s);
        m.set_tail("r1/t1", &format!("{evil}\n\t{evil}\r\n"));
        m.message = evil.to_string();
        m.input = Some(evil.to_string());
        m.show_help = false;
        let buf = draw(&m, 120, 30);
        for cell in buf.content() {
            for c in cell.symbol().chars() {
                assert!(
                    !c.is_control() && !fleet_domain::ids::is_invisible(c),
                    "control or invisible char {c:?} in buffer"
                );
            }
        }
    }

    #[test]
    fn output_pane_shows_only_the_last_lines_and_truncates_long_ones() {
        let mut m = sample();
        let mut tail = String::new();
        for i in 0..100 {
            tail.push_str(&format!("line-{i}\n"));
        }
        tail.push_str(&"x".repeat(500));
        tail.push('\n');
        m.set_tail("r1/t1", &tail);
        let text = screen(&m, 100, 30);
        assert!(!text.contains("line-0") && !text.contains("line-50"));
        assert!(text.contains("line-99"));
        // Nothing wraps or spills: the longest run of x is at most the pane width.
        let longest = text.split(|c| c != 'x').map(str::len).max().unwrap_or(0);
        assert!(longest <= 55, "{longest}");
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        let m = sample();
        for (w, h) in [
            (0, 0),
            (1, 1),
            (10, 3),
            (39, 9),
            (39, 30),
            (100, 9),
            (40, 10),
            (41, 11),
        ] {
            let text = screen(&m, w, h);
            if (w < MIN_WIDTH || h < MIN_HEIGHT) && w >= 18 && h >= 1 {
                assert!(text.contains("terminal too small"), "{w}x{h}");
            }
        }
        let mut with_overlays = sample();
        with_overlays.show_help = true;
        with_overlays.input = Some("typing".into());
        for (w, h) in [(40, 10), (45, 12), (60, 10)] {
            let _ = screen(&with_overlays, w, h);
        }
    }

    #[test]
    fn help_overlay_is_drawn_only_when_enabled() {
        let mut m = sample();
        assert!(!screen(&m, 100, 30).contains("cycle panels"));
        m.show_help = true;
        assert!(screen(&m, 100, 30).contains("cycle panels"));
    }

    #[test]
    fn answer_input_line_shows_prompt_and_text() {
        let mut m = sample();
        m.input = Some("use sqlite".into());
        let text = screen(&m, 100, 30);
        assert!(text.contains("answer> use sqlite"), "{text}");
    }

    #[test]
    fn long_task_lists_scroll_to_keep_the_selection_visible() {
        let many: Vec<Value> = (0..60)
            .map(|i| task(&format!("t{i}"), "x", "pending", &[]))
            .collect();
        let mut m = Model::new();
        m.update_status(&status(many));
        m.move_selection(50);
        let text = screen(&m, 100, 30);
        assert!(text.contains("> r1/t50"), "{text}");
        assert!(!text.contains("r1/t0 "));
    }

    #[test]
    fn offline_marker_and_time_left_formatting() {
        let mut m = sample();
        m.connected = false;
        assert!(header_text(&m).ends_with("| OFFLINE"));
        assert_eq!(time_left(10_000, 20_000), "expired");
        assert_eq!(time_left(70_000, 10_000), "1m00s");
        assert_eq!(time_left(15_000, 10_000), "5s");
    }

    #[test]
    fn window_start_keeps_selection_in_view() {
        assert_eq!(window_start(0, 5), 0);
        assert_eq!(window_start(4, 5), 0);
        assert_eq!(window_start(5, 5), 1);
        assert_eq!(window_start(9, 0), 9);
    }
}
