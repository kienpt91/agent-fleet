//! Renders the dashboard into a headless buffer and prints it as text, so the README can show a
//! real screen without a terminal: `fleet status --json > status.json` then
//! `cargo run --example tui_screenshot -- status.json [tail.txt] [width] [height]`.

use fleet_cli::tui::model::Model;
use fleet_cli::tui::ui::render;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(status_path) = args.first() else {
        eprintln!("usage: tui_screenshot <status.json> [tail.txt] [width] [height]");
        std::process::exit(2);
    };
    let Ok(raw) = std::fs::read(status_path) else {
        eprintln!("cannot read {status_path}");
        std::process::exit(1);
    };
    let status = match fleet_infra::json::parse_value(&raw, 4 * 1024 * 1024) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("invalid status document: {e}");
            std::process::exit(1);
        }
    };
    let width: u16 = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(110);
    let height: u16 = args.get(3).and_then(|a| a.parse().ok()).unwrap_or(30);
    let mut model = Model::new();
    model.update_status(&status);
    if let (Some(path), Some(key)) = (args.get(1), model.selected_key()) {
        if let Ok(text) = std::fs::read_to_string(path) {
            model.set_tail(&key, &text);
        }
    }
    let mut terminal = match Terminal::new(TestBackend::new(width, height)) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cannot create the headless terminal: {e}");
            std::process::exit(1);
        }
    };
    if terminal.draw(|f| render(f, &model)).is_err() {
        eprintln!("rendering failed");
        std::process::exit(1);
    }
    let buffer = terminal.backend().buffer();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        println!("{}", line.trim_end());
    }
}
