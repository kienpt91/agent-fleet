//! Checks of the remote hosts for `fleet doctor` (offline) and `fleet host doctor <name>` (also
//! connects, once, and exercises what a remote task needs).

use super::client::{SshClient, Timing};
use super::exec::{Input, Output};
use super::known_hosts::HostKey;
use super::locate_ssh;
use crate::config_file::load_config;
use crate::doctor::{Check, Status};
use crate::jobs::JobTracker;
use crate::statedir::StateLayout;
use fleet_app::hosts::HostDef;
use fleet_domain::remote::RemoteCall;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

/// Text that a remote login shell must hand over unchanged for fleet to use the host.
const PROBES: [&str; 6] = [
    "plain",
    "it's \"quoted\"",
    "$(echo substituted) `echo substituted` ${HOME}",
    "a;b|c&d*?",
    "back\\slash \\' \\\\",
    "two\nlines\tand tab",
];

fn name(host: &HostDef, what: &str) -> String {
    format!("host {}: {what}", host.name)
}

fn identity_check(host: &HostDef) -> Check {
    let n = name(host, "identity file");
    let path = Path::new(&host.identity_file);
    match std::fs::symlink_metadata(path) {
        Err(e) => Check::new(n, Status::Fail, format!("{}: {e}", host.identity_file)),
        Ok(m) if !m.is_file() && !m.file_type().is_symlink() => Check::new(
            n,
            Status::Fail,
            format!("{} is not a file", host.identity_file),
        ),
        Ok(_) => match std::fs::metadata(path) {
            Err(e) => Check::new(n, Status::Fail, format!("{}: {e}", host.identity_file)),
            Ok(m) if m.permissions().mode() & 0o077 != 0 => Check::new(
                n,
                Status::Fail,
                format!(
                    "{} has mode {:o}; ssh refuses a private key that others can read (chmod 600)",
                    host.identity_file,
                    m.permissions().mode() & 0o777
                ),
            ),
            Ok(m) if m.uid() != nix::unistd::geteuid().as_raw() => Check::new(
                n,
                Status::Fail,
                format!("{} is owned by another user", host.identity_file),
            ),
            Ok(_) => Check::new(n, Status::Ok, host.identity_file.clone()),
        },
    }
}

fn pin_check(client: &SshClient, host: &HostDef, required: bool) -> Check {
    let n = name(host, "pinned key");
    match client.pinned(host) {
        Ok(Some(key)) => Check::new(n, Status::Ok, describe(&key)),
        Ok(None) => Check::new(
            n,
            if required { Status::Fail } else { Status::Info },
            format!("not pinned yet: run `fleet host trust {}`", host.name),
        ),
        Err(e) => Check::new(n, Status::Fail, e.message),
    }
}

pub fn describe(key: &HostKey) -> String {
    format!("{} {}", key.key_type, key.fingerprint())
}

/// `OpenSSH_9.2p1 Debian-2, OpenSSL ...` into (9, 2).
pub fn parse_openssh_version(text: &str) -> Option<(u32, u32)> {
    let rest = text.split("OpenSSH_").nth(1)?;
    let mut numbers = rest
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .map(|p| p.parse::<u32>());
    Some((numbers.next()?.ok()?, numbers.next()?.ok()?))
}

/// The oldest OpenSSH whose options this adapter relies on (`StrictHostKeyChecking=accept-new`
/// came with 7.6).
pub const MIN_OPENSSH: (u32, u32) = (7, 6);

/// The `ssh` program: found, and new enough.
pub fn program_check(client: &SshClient) -> Check {
    let done = super::exec::run(
        &super::exec::Spec {
            program: client.ssh.clone(),
            args: vec!["-V".to_string()],
            env: super::argv::client_env(&client.ssh, false),
            input: Input::None,
            output: Output::Capture { max: 4096 },
            limits: super::exec::Limits {
                total: Duration::from_secs(10),
                stall: Duration::from_secs(10),
            },
        },
        &client.tracker,
    );
    let text =
        String::from_utf8_lossy(&done.stderr).into_owned() + &String::from_utf8_lossy(&done.stdout);
    let path = client.ssh.display().to_string();
    match parse_openssh_version(&text) {
        Some(v) if v >= MIN_OPENSSH => Check::new(
            "ssh program",
            Status::Ok,
            format!("{path} (OpenSSH {}.{})", v.0, v.1),
        ),
        Some(v) => Check::new(
            "ssh program",
            Status::Fail,
            format!(
                "{path} is OpenSSH {}.{}; {}.{} or newer is required",
                v.0, v.1, MIN_OPENSSH.0, MIN_OPENSSH.1
            ),
        ),
        None => Check::new(
            "ssh program",
            Status::Fail,
            format!(
                "{path} did not say which OpenSSH it is ({})",
                super::client::clean(text.as_bytes(), 80)
            ),
        ),
    }
}

/// What can be checked without a connection.
pub fn offline_checks(client: &SshClient, host: &HostDef, pin_required: bool) -> Vec<Check> {
    let mut out = vec![identity_check(host), pin_check(client, host, pin_required)];
    if let Some(jump) = &host.proxy_jump {
        out.push(match client.hosts.get(jump) {
            Some(j) => match client.pinned(j) {
                Ok(Some(_)) => Check::new(
                    name(host, "jump host"),
                    Status::Ok,
                    format!("{jump} is pinned"),
                ),
                Ok(None) => Check::new(
                    name(host, "jump host"),
                    if pin_required {
                        Status::Fail
                    } else {
                        Status::Info
                    },
                    format!("{jump} is not pinned yet: run `fleet host trust {jump}`"),
                ),
                Err(e) => Check::new(name(host, "jump host"), Status::Fail, e.message),
            },
            None => Check::new(
                name(host, "jump host"),
                Status::Fail,
                format!("{jump} is not configured"),
            ),
        });
    }
    out
}

/// Everything, with one connection per check that needs one.
pub fn full_checks(client: &SshClient, host: &HostDef) -> Vec<Check> {
    let mut out = vec![program_check(client)];
    out.extend(offline_checks(client, host, true));
    if out.iter().any(|c| c.status == Status::Fail) {
        out.push(Check::new(
            name(host, "connection"),
            Status::Info,
            "not tried: fix the checks above first",
        ));
        return out;
    }
    let timing = Timing {
        total: Duration::from_secs(60),
        stall: Duration::from_secs(30),
    };
    // The round trip: hostile text through this host's login shell, plus the system and user.
    let args: Vec<String> = PROBES.iter().map(|s| s.to_string()).collect();
    let started = Instant::now();
    let done = client.run(
        host,
        &RemoteCall::RoundTrip { args: &args },
        Input::None,
        Output::Capture { max: 64 * 1024 },
        timing,
    );
    let done = match done {
        Ok(d) => d,
        Err(e) => {
            out.push(Check::new(
                name(host, "connection"),
                Status::Fail,
                e.message,
            ));
            return out;
        }
    };
    if !done.success() {
        out.push(Check::new(
            name(host, "connection"),
            Status::Fail,
            client.explain(host, "connecting", &done).message,
        ));
        return out;
    }
    client.mark_up(&host.name);
    out.push(Check::new(
        name(host, "connection"),
        Status::Ok,
        format!(
            "{} ms, pinned key accepted, key file accepted",
            started.elapsed().as_millis()
        ),
    ));
    let mut expected: Vec<u8> = Vec::new();
    for a in &args {
        expected.extend_from_slice(a.as_bytes());
        expected.push(0);
    }
    let tail = done.stdout.strip_prefix(expected.as_slice());
    match tail {
        Some(rest) => {
            out.push(Check::new(
                name(host, "quoting"),
                Status::Ok,
                "the login shell reads quoted text unchanged",
            ));
            let text = String::from_utf8_lossy(rest);
            let mut lines = text.lines();
            let system = lines.next().unwrap_or("?");
            let uid = lines.next().unwrap_or("?");
            out.push(Check::new(
                name(host, "system"),
                Status::Ok,
                super::client::clean(system.as_bytes(), 40),
            ));
            out.push(if uid == "0" {
                Check::new(
                    name(host, "remote user"),
                    Status::Info,
                    "the remote user is root: a remote worker can change anything on that machine",
                )
            } else {
                Check::new(name(host, "remote user"), Status::Ok, format!("uid {}", super::client::clean(uid.as_bytes(), 12)))
            });
        }
        None => out.push(Check::new(
            name(host, "quoting"),
            Status::Fail,
            "the remote login shell does not hand quoted text over unchanged (fleet needs a POSIX shell such as sh, bash, dash or zsh as that user's login shell)",
        )),
    }
    // git on the host.
    out.push(
        match client.run(
            host,
            &RemoteCall::GitVersion,
            Input::None,
            Output::Capture { max: 4096 },
            timing,
        ) {
            Ok(d) if d.success() => Check::new(
                name(host, "git"),
                Status::Ok,
                super::client::clean(&d.stdout, 80),
            ),
            Ok(d) => Check::new(
                name(host, "git"),
                Status::Fail,
                format!(
                    "git is not usable on the host with PATH {} ({})",
                    SshClient::remote_path(host),
                    super::client::clean(&d.stderr, 120)
                ),
            ),
            Err(e) => Check::new(name(host, "git"), Status::Fail, e.message),
        },
    );
    // The base directory.
    out.push(
        match client.run(
            host,
            &RemoteCall::CheckBase {
                base: &host.base_dir,
            },
            Input::None,
            Output::Capture { max: 4096 },
            timing,
        ) {
            Ok(d) if d.success() && d.stdout.starts_with(b"ok") => Check::new(
                name(host, "base directory"),
                Status::Ok,
                format!("{} is writable", host.base_dir),
            ),
            Ok(d) => Check::new(
                name(host, "base directory"),
                Status::Fail,
                format!(
                    "{} cannot be used ({})",
                    host.base_dir,
                    super::client::clean(&d.stderr, 120)
                ),
            ),
            Err(e) => Check::new(name(host, "base directory"), Status::Fail, e.message),
        },
    );
    out
}

/// The offline checks of every configured host, for `fleet doctor`. Nothing when no host is
/// configured or the configuration cannot be read (`check_config` reports that).
pub fn config_checks(
    explicit: Option<&Path>,
    layout: &StateLayout,
    env: &[(String, String)],
    tracker: &JobTracker,
) -> Vec<Check> {
    let path = explicit.unwrap_or(&layout.default_config);
    if std::fs::symlink_metadata(path).is_err() {
        return Vec::new();
    }
    let Ok(loaded) = load_config(path, env.to_vec()) else {
        return Vec::new();
    };
    if loaded.app.hosts.is_empty() {
        return Vec::new();
    }
    let path_env = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map_or("", |(_, v)| v.as_str());
    let ssh = match locate_ssh(loaded.remote.ssh_binary.as_deref(), path_env) {
        Ok(p) => p,
        Err(e) => return vec![Check::new("ssh program", Status::Fail, e.message)],
    };
    let mut out = vec![Check::new(
        "ssh program",
        Status::Ok,
        ssh.display().to_string(),
    )];
    let client = SshClient::new(
        ssh,
        loaded.app.hosts.clone(),
        layout.root.clone(),
        tracker.clone(),
    );
    for host in loaded.app.hosts.values() {
        out.extend(offline_checks(&client, host, false));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openssh_versions_are_read_from_the_banner() {
        assert_eq!(
            parse_openssh_version("OpenSSH_9.2p1 Debian-2+deb12u10, OpenSSL 3.0.20"),
            Some((9, 2))
        );
        assert_eq!(
            parse_openssh_version("OpenSSH_8.9p1 Ubuntu-3ubuntu0.1"),
            Some((8, 9))
        );
        assert_eq!(
            parse_openssh_version("OpenSSH_for_Windows_8.1p1"),
            Some((8, 1))
        );
        assert_eq!(parse_openssh_version("OpenSSH_9"), None);
        assert_eq!(parse_openssh_version("dropbear v2022.83"), None);
        assert_eq!(parse_openssh_version(""), None);
        assert!((7, 5) < MIN_OPENSSH && (7, 6) >= MIN_OPENSSH && (8, 0) >= MIN_OPENSSH);
    }
}
