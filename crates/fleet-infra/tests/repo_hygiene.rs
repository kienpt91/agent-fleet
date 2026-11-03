//! What the documents of the repository must agree with, checked on the files themselves: the
//! dashboard's command list in SECURITY.md is the code's.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_infra::daemon::WEB_COMMANDS;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

#[test]
fn security_md_lists_every_command_of_the_dashboard_once_and_in_one_row() {
    let text = fs::read_to_string(root().join("SECURITY.md")).unwrap();
    let rows: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("| The web dashboard serves the operator only |"))
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "the dashboard row must exist exactly once, not {}",
        rows.len()
    );
    let row = rows[0];
    assert!(
        row.contains(&format!(
            "the {} commands of `WEB_COMMANDS`",
            WEB_COMMANDS.len()
        )),
        "the row must say how many commands there are ({})",
        WEB_COMMANDS.len()
    );
    let unique: BTreeSet<&&str> = WEB_COMMANDS.iter().collect();
    assert_eq!(
        unique.len(),
        WEB_COMMANDS.len(),
        "a command is listed twice"
    );
    for cmd in WEB_COMMANDS {
        assert!(
            row.contains(&format!("`{cmd}`")),
            "the dashboard row of SECURITY.md lacks `{cmd}`"
        );
    }
}

#[test]
fn the_remote_hosts_row_does_not_deny_the_read_only_message_commands() {
    let text = fs::read_to_string(root().join("SECURITY.md")).unwrap();
    assert!(
        !text.contains("no message command is offered"),
        "msg.list and msg.show are offered by the dashboard"
    );
    assert!(WEB_COMMANDS.contains(&"msg.list") && WEB_COMMANDS.contains(&"msg.show"));
    assert!(!WEB_COMMANDS.iter().any(|c| c.starts_with("host.")
        || *c == "msg.send"
        || *c == "msg.mute"
        || *c == "msg.unmute"));
}
