//! What the committed text of the repository must not contain or must agree with, checked on the
//! files themselves: no home directory or private mail address of a person, and the documents say
//! what the code offers (the dashboard's command list). Reviewed exceptions are lines of
//! `.secret-scan-allow` (`<path> <rule>  # reason`), the file the secret scanner reads too; the rules
//! here are `owner-home-path` and `personal-mail`. The scanner itself does not apply them: it also
//! scans the history, which cannot be rewritten, and these rules are about the tree.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_infra::daemon::WEB_COMMANDS;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

/// Every committed-looking text file: relative path and content. Binary files, `.git`, build output
/// and worktrees are left out.
fn text_files() -> Vec<(String, String)> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, String)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if dir == base && matches!(name.as_str(), ".git" | "target" | ".fleet-worktrees") {
                continue;
            }
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                walk(&path, base, out);
            } else if kind.is_file() {
                if let Ok(text) = fs::read_to_string(&path) {
                    let rel = path.strip_prefix(base).unwrap();
                    let rel = rel
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("/");
                    out.push((rel, text));
                }
            }
        }
    }
    let base = root();
    let mut out = Vec::new();
    walk(&base, &base, &mut out);
    out.sort();
    out
}

/// `(path, rule)` pairs of the reviewed exceptions, each with its reason.
fn exceptions() -> Vec<(String, String)> {
    let text = fs::read_to_string(root().join(".secret-scan-allow")).unwrap();
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (entry, reason) = l.split_once('#').unwrap_or((l, ""));
            assert!(
                !reason.trim().is_empty(),
                "an exception needs a reason: {l}"
            );
            let mut words = entry.split_whitespace();
            (
                words.next().unwrap().to_string(),
                words.next().unwrap().to_string(),
            )
        })
        .collect()
}

fn excepted(path: &str, rule: &str) -> bool {
    exceptions().iter().any(|(p, r)| p == path && r == rule)
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "._-".contains(c)
}

/// The names that stand for "your user name" in a document.
const PLACEHOLDER_USERS: [&str; 5] = ["you", "me", "user", "name", "example"];

/// The `/Users/<name>` directories a line names, other than placeholders.
fn home_paths(line: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find("/Users/") {
        let after = &rest[at + "/Users/".len()..];
        let name: String = after.chars().take_while(|c| is_name_char(*c)).collect();
        if !name.is_empty() && !PLACEHOLDER_USERS.contains(&name.as_str()) {
            found.push(format!("/Users/{name}"));
        }
        rest = after;
    }
    found
}

/// Mail providers of private persons: an address there is somebody's own, not a role or a fixture.
const PERSONAL_MAIL_DOMAINS: [&str; 10] = [
    "gmail.com",
    "googlemail.com",
    "outlook.com",
    "hotmail.com",
    "live.com",
    "yahoo.com",
    "icloud.com",
    "me.com",
    "proton.me",
    "protonmail.com",
];

/// The addresses at a personal provider that a line contains.
fn personal_mail(line: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (at, _) in line.match_indices('@') {
        let local: String = line[..at]
            .chars()
            .rev()
            .take_while(|c| is_name_char(*c) || *c == '+')
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let domain: String = line[at + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || ".-".contains(*c))
            .collect();
        let domain = domain.trim_end_matches('.').to_ascii_lowercase();
        if !local.is_empty() && PERSONAL_MAIL_DOMAINS.contains(&domain.as_str()) {
            found.push(format!("{local}@{domain}"));
        }
    }
    found
}

#[test]
fn the_detectors_find_what_they_are_for_and_leave_placeholders_alone() {
    // the fixtures are assembled here so that this file does not contain what it looks for
    let users = ["/Us", "ers"].concat();
    let gmail = ["gmail", ".com"].concat();
    assert_eq!(
        home_paths(&format!("command = \"{users}/alice/.local/bin/omp\"")),
        [format!("{users}/alice")]
    );
    assert!(home_paths(&format!(
        "{users}/you/.fleet and {users}/<name>/x and {users}/"
    ))
    .is_empty());
    assert_eq!(
        home_paths(&format!("a {users}/bob/x then {users}/carol-1/y")),
        [format!("{users}/bob"), format!("{users}/carol-1")]
    );
    assert_eq!(
        personal_mail(&format!("write to Some.One+tag@{}.", gmail.to_uppercase())),
        [format!("Some.One+tag@{gmail}")]
    );
    assert!(personal_mail(&format!(
        "ci@example.invalid, git@github.com:o/r.git, @{gmail}"
    ))
    .is_empty());
}

#[test]
fn no_home_directory_of_a_person_is_committed() {
    let mut hits = Vec::new();
    for (path, text) in text_files() {
        if excepted(&path, "owner-home-path") {
            continue;
        }
        for (n, line) in text.lines().enumerate() {
            for h in home_paths(line) {
                hits.push(format!("{path}:{}: {h}", n + 1));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "a person's home directory is committed (use a placeholder such as /usr/local/bin/omp, $HOME or /Users/you):\n{}",
        hits.join("\n")
    );
}

#[test]
fn no_private_mail_address_is_committed_except_the_reviewed_ones() {
    let mut hits = Vec::new();
    for (path, text) in text_files() {
        if excepted(&path, "personal-mail") {
            continue;
        }
        for (n, line) in text.lines().enumerate() {
            for m in personal_mail(line) {
                hits.push(format!("{path}:{}: {m}", n + 1));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "a private mail address is committed (add a reviewed line to .secret-scan-allow if it must be):\n{}",
        hits.join("\n")
    );
}

#[test]
fn every_exception_of_these_rules_is_still_needed() {
    let files = text_files();
    for (path, rule) in exceptions() {
        let needed = |hit: &dyn Fn(&str) -> bool| {
            files
                .iter()
                .find(|(p, _)| *p == path)
                .is_some_and(|(_, t)| t.lines().any(hit))
        };
        match rule.as_str() {
            "owner-home-path" => assert!(
                needed(&|l| !home_paths(l).is_empty()),
                "{path} no longer has a home path: delete its exception"
            ),
            "personal-mail" => assert!(
                needed(&|l| !personal_mail(l).is_empty()),
                "{path} no longer has a private address: delete its exception"
            ),
            _ => {}
        }
    }
}

// ---- the documents say what the code offers ---------------------------------------------------------

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

#[test]
fn the_ssh_test_image_says_in_a_label_that_it_is_for_tests_only() {
    // its /etc/passwd is world-writable so that the tests can add a user: it must never be taken for a service image
    let text = fs::read_to_string(root().join("docker/Dockerfile.ssh")).unwrap();
    assert!(
        text.contains("chmod 666 /etc/passwd"),
        "the reason for the warning changed"
    );
    assert!(
        text.starts_with("# WARNING: CI AND TEST ONLY"),
        "no warning at the top"
    );
    assert!(
        text.lines()
            .any(|l| l.trim_start().starts_with("LABEL ") && l.contains("TEST ONLY")),
        "no LABEL that says test only"
    );
    assert!(text.contains("org.fleet.test-only=\"true\""));
}
