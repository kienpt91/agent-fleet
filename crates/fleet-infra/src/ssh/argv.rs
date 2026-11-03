//! The command line of the `ssh` client: a fixed list of options, built without a shell.
//!
//! Everything that could change what ssh trusts or forwards is stated here, on the command line,
//! and `-F /dev/null` keeps the user's own ssh configuration out: no agent, no forwarding, no
//! password, no local command, no multiplexing, one key file, one known_hosts file, no proxy unless
//! the operator wrote one. The remote command is one argument, assembled by
//! `fleet_domain::remote`.

use fleet_app::hosts::HostDef;
use fleet_domain::error::{FleetError, Result};
use fleet_domain::remote::validate_local_path;
use std::path::Path;

/// What the client is used for.
#[derive(Clone, Debug)]
pub enum Mode<'a> {
    /// Run a remote command, with or without a terminal.
    Command { tty: bool },
    /// Record the host's key in a scratch file and stop (`fleet host trust`): the only mode that
    /// accepts a key it does not know, and it writes it nowhere but `scratch`. No key is offered.
    Scan { scratch: &'a Path },
}

/// The host to reach and the files that pin it (and its jump host).
#[derive(Clone, Debug)]
pub struct Profile<'a> {
    /// The ssh program (the hop to a jump host runs it again).
    pub ssh: &'a Path,
    pub host: &'a HostDef,
    pub pin: &'a Path,
    /// The jump host and its pin, when the host is reached through one.
    pub jump: Option<(&'a HostDef, &'a Path)>,
}

/// The environment of every ssh client: no agent, no display, no askpass program, no shell
/// settings; messages in one language so that failures can be told apart.
pub fn client_env(ssh: &Path, term: bool) -> Vec<(String, String)> {
    let dir = ssh
        .parent()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    let mut path = String::from("/usr/bin:/bin");
    if !dir.is_empty() && dir != "/usr/bin" && dir != "/bin" {
        path = format!("{dir}:{path}");
    }
    let mut env = vec![
        ("PATH".to_string(), path),
        ("LC_ALL".to_string(), "C".to_string()),
    ];
    if term {
        env.push((
            "TERM".to_string(),
            fleet_domain::remote::REMOTE_TERM.to_string(),
        ));
    }
    env
}

/// Characters a shell reads as syntax or expands.
const INERT_FORBIDDEN: &str = "'\\\"$`;&|<>()*?![]{}#~";

/// The options every connection has, the hop to a jump host included.
fn hardening(h: &HostDef, o: &mut impl FnMut(&str, &str)) {
    o("GlobalKnownHostsFile", "/dev/null");
    o("IdentitiesOnly", "yes");
    o("IdentityAgent", "none");
    o("BatchMode", "yes");
    o("PasswordAuthentication", "no");
    o("KbdInteractiveAuthentication", "no");
    o("HostbasedAuthentication", "no");
    o("GSSAPIAuthentication", "no");
    o("NumberOfPasswordPrompts", "0");
    o("ForwardAgent", "no");
    o("ForwardX11", "no");
    o("ForwardX11Trusted", "no");
    o("ClearAllForwardings", "yes");
    o("PermitLocalCommand", "no");
    o("Tunnel", "no");
    o("ControlMaster", "no");
    o("ControlPath", "none");
    o("ControlPersist", "no");
    o("CheckHostIP", "no");
    o("VerifyHostKeyDNS", "no");
    o("UpdateHostKeys", "no");
    o("HashKnownHosts", "no");
    o("Compression", "no");
    o("EscapeChar", "none");
    o("LogLevel", "ERROR");
    o("ConnectionAttempts", "1");
    o("ConnectTimeout", &h.connect_timeout_secs.to_string());
    o("ServerAliveInterval", &h.server_alive_secs.to_string());
    o("ServerAliveCountMax", &h.server_alive_count.to_string());
}

/// The command ssh runs to reach a host through its jump host: a second ssh client with the same
/// fixed options for the *jump host* (its own key file and its own pin; `ProxyJump` would start a
/// client with default settings: the user's known_hosts and keys). Every word is validated text
/// without white space, quotes or shell characters, so that the one string ssh hands to a shell is
/// exactly these words.
fn hop_command(ssh: &Path, jump: &HostDef, jump_pin: &Path) -> Result<String> {
    let ssh = path_text("ssh program", ssh)?;
    let mut opts: Vec<String> = Vec::new();
    let mut o = |k: &str, v: &str| opts.push(format!("{k}={v}"));
    o("StrictHostKeyChecking", "yes");
    o(
        "UserKnownHostsFile",
        &path_text("known hosts file", jump_pin)?,
    );
    o(
        "IdentityFile",
        &path_text("identity file", Path::new(&jump.identity_file))?,
    );
    o("PubkeyAuthentication", "yes");
    o("PreferredAuthentications", "publickey");
    o("ProxyCommand", "none");
    o("ProxyJump", "none");
    hardening(jump, &mut o);
    let mut words: Vec<String> = vec![
        ssh,
        "-F".into(),
        "/dev/null".into(),
        "-a".into(),
        "-x".into(),
        "-k".into(),
    ];
    for opt in opts {
        words.push("-o".into());
        words.push(opt);
    }
    words.extend([
        "-W".into(),
        "%h:%p".into(),
        "-p".into(),
        jump.port.to_string(),
        "-l".into(),
        jump.user.clone(),
        "--".into(),
        jump.address.clone(),
    ]);
    if let Some(bad) = words.iter().find(|w| {
        w.is_empty()
            || w.chars()
                .any(|c| c.is_whitespace() || INERT_FORBIDDEN.contains(c))
    }) {
        return Err(FleetError::invalid(format!(
            "the jump host command contains {bad:?}, which a shell would read"
        )));
    }
    Ok(words.join(" "))
}

/// The arguments of the ssh client (without the program) for `profile`, running `command`.
pub fn ssh_args(profile: &Profile<'_>, mode: &Mode<'_>, command: &str) -> Result<Vec<String>> {
    let h = profile.host;
    let mut opts: Vec<String> = Vec::new();
    let mut o = |k: &str, v: &str| opts.push(format!("{k}={v}"));
    match mode {
        Mode::Command { .. } => {
            o("StrictHostKeyChecking", "yes");
            o(
                "UserKnownHostsFile",
                &path_text("known hosts file", profile.pin)?,
            );
            let identity = path_text("identity file", Path::new(&h.identity_file))?;
            o("IdentityFile", &identity);
            o("PubkeyAuthentication", "yes");
            o("PreferredAuthentications", "publickey");
        }
        Mode::Scan { scratch } => {
            // The scratch file is where ssh adds a key it did not know; the existing pin is not
            // read and not written. The jump host's key (a separate client, see `hop_command`)
            // must be pinned already.
            o("StrictHostKeyChecking", "accept-new");
            o("UserKnownHostsFile", &path_text("scratch file", scratch)?);
            o("PubkeyAuthentication", "no");
            o("PreferredAuthentications", "none");
        }
    }
    o("ProxyJump", "none");
    match profile.jump {
        Some((jump, jump_pin)) => {
            o("ProxyCommand", &hop_command(profile.ssh, jump, jump_pin)?);
        }
        None => o("ProxyCommand", "none"),
    }
    hardening(h, &mut o);

    if command.contains('\0') || command.is_empty() {
        return Err(FleetError::invalid(
            "the remote command is empty or contains NUL",
        ));
    }
    let mut args: Vec<String> = vec![
        "-F".into(),
        "/dev/null".into(),
        // Redundant with the options above, and short: no agent, no X11, no GSSAPI delegation.
        "-a".into(),
        "-x".into(),
        "-k".into(),
    ];
    for opt in opts {
        args.push("-o".into());
        args.push(opt);
    }
    match mode {
        Mode::Command { tty: true } => args.push("-tt".into()),
        _ => args.push("-T".into()),
    }
    args.push("-p".into());
    args.push(h.port.to_string());
    args.push("-l".into());
    args.push(h.user.clone());
    args.push("--".into());
    args.push(h.address.clone());
    args.push(command.to_string());
    Ok(args)
}

fn path_text(what: &str, path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .ok_or_else(|| FleetError::invalid(format!("{what} is not valid UTF-8")))?;
    validate_local_path(what, text)?;
    Ok(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_domain::ids::HostName;

    fn host(name: &str) -> HostDef {
        let mut h = HostDef::new(
            HostName::parse(name).unwrap(),
            "build.example.org",
            "fleet",
            "/keys/id_ed25519",
            "/srv/fleet",
        );
        h.port = 2222;
        h
    }

    fn ssh() -> &'static Path {
        Path::new("/usr/bin/ssh")
    }

    fn opts(args: &[String]) -> Vec<String> {
        args.windows(2)
            .filter(|w| w[0] == "-o")
            .map(|w| w[1].clone())
            .collect()
    }

    fn has(args: &[String], opt: &str) -> bool {
        opts(args).iter().any(|o| o == opt)
    }

    #[test]
    fn a_command_is_one_argument_after_a_double_dash_and_the_host_cannot_be_an_option() {
        let h = host("build");
        let pin = Path::new("/state/hosts/build.known_hosts");
        let p = Profile {
            ssh: ssh(),
            host: &h,
            pin,
            jump: None,
        };
        let args = ssh_args(&p, &Mode::Command { tty: false }, "echo 'a b'").unwrap();
        let n = args.len();
        assert_eq!(
            args[n - 3..].to_vec(),
            vec!["--", "build.example.org", "echo 'a b'"],
            "{args:?}"
        );
    }

    #[test]
    fn every_security_setting_is_on_the_command_line() {
        let h = host("build");
        let pin = Path::new("/state/hosts/build.known_hosts");
        let p = Profile {
            ssh: ssh(),
            host: &h,
            pin,
            jump: None,
        };
        let args = ssh_args(&p, &Mode::Command { tty: true }, "cmd").unwrap();
        assert_eq!(&args[0..2], ["-F", "/dev/null"]);
        for want in [
            "StrictHostKeyChecking=yes",
            "UserKnownHostsFile=/state/hosts/build.known_hosts",
            "GlobalKnownHostsFile=/dev/null",
            "IdentitiesOnly=yes",
            "IdentityFile=/keys/id_ed25519",
            "IdentityAgent=none",
            "BatchMode=yes",
            "PasswordAuthentication=no",
            "KbdInteractiveAuthentication=no",
            "PreferredAuthentications=publickey",
            "ForwardAgent=no",
            "ForwardX11=no",
            "ClearAllForwardings=yes",
            "PermitLocalCommand=no",
            "ControlMaster=no",
            "ControlPath=none",
            "ProxyCommand=none",
            "ProxyJump=none",
            "UpdateHostKeys=no",
            "CheckHostIP=no",
            "ConnectTimeout=10",
            "ServerAliveInterval=15",
            "ServerAliveCountMax=3",
        ] {
            assert!(has(&args, want), "missing {want}");
        }
        assert!(args.contains(&"-tt".to_string()));
        assert!(!args.contains(&"-T".to_string()));
        for forbidden in [
            "-A", "-X", "-Y", "-L", "-R", "-D", "-J", "-w", "-N", "-f", "-M", "-S",
        ] {
            assert!(!args.contains(&forbidden.to_string()), "{forbidden}");
        }
        assert!(opts(&args)
            .iter()
            .all(|o| !o.starts_with("SendEnv") && !o.starts_with("SetEnv")));
        let ports: Vec<_> = args
            .windows(2)
            .filter(|w| w[0] == "-p")
            .map(|w| w[1].clone())
            .collect();
        assert_eq!(ports, ["2222"]);
        assert!(args.windows(2).any(|w| w[0] == "-l" && w[1] == "fleet"));
    }

    #[test]
    fn a_non_terminal_call_uses_t_and_a_jump_host_gets_its_own_strict_client() {
        let h = host("target");
        let mut j = host("bastion");
        j.address = "10.1.2.3".into();
        j.port = 22;
        j.user = "jumper".into();
        j.identity_file = "/keys/bastion_key".into();
        let pin = Path::new("/s/hosts/target.known_hosts");
        let jpin = Path::new("/s/hosts/bastion.known_hosts");
        let p = Profile {
            ssh: ssh(),
            host: &h,
            pin,
            jump: Some((&j, jpin)),
        };
        let args = ssh_args(&p, &Mode::Command { tty: false }, "cmd").unwrap();
        assert!(args.contains(&"-T".to_string()));
        assert!(
            has(&args, "UserKnownHostsFile=/s/hosts/target.known_hosts"),
            "the target's pin only"
        );
        assert!(has(&args, "IdentityFile=/keys/id_ed25519"));
        // No ProxyJump (its client would start with default settings): a command of our own.
        assert!(has(&args, "ProxyJump=none"));
        let proxy = opts(&args)
            .into_iter()
            .find(|o| o.starts_with("ProxyCommand="))
            .unwrap();
        let words: Vec<&str> = proxy["ProxyCommand=".len()..].split(' ').collect();
        assert_eq!(words[0], "/usr/bin/ssh");
        assert_eq!(&words[1..3], ["-F", "/dev/null"]);
        let joined = words.join(" ");
        for want in [
            "StrictHostKeyChecking=yes",
            "UserKnownHostsFile=/s/hosts/bastion.known_hosts",
            "IdentityFile=/keys/bastion_key",
            "IdentitiesOnly=yes",
            "IdentityAgent=none",
            "BatchMode=yes",
            "ForwardAgent=no",
            "ClearAllForwardings=yes",
            "ProxyCommand=none",
            "ProxyJump=none",
            "-W %h:%p",
            "-p 22 -l jumper -- 10.1.2.3",
        ] {
            assert!(joined.contains(want), "missing {want} in {joined}");
        }
        assert!(!joined.contains("target.known_hosts") && !joined.contains("id_ed25519"));
        assert!(!joined.contains(' ') || words.iter().all(|w| !w.is_empty()));
    }

    #[test]
    fn a_jump_host_command_with_anything_a_shell_would_read_is_refused() {
        let h = host("target");
        let mut j = host("bastion");
        let pin = Path::new("/s/hosts/target.known_hosts");
        let jpin = Path::new("/s/hosts/bastion.known_hosts");
        j.address = "10.1.2.3;id".into();
        let p = Profile {
            ssh: ssh(),
            host: &h,
            pin,
            jump: Some((&j, jpin)),
        };
        assert!(ssh_args(&p, &Mode::Command { tty: false }, "cmd").is_err());
        let mut j = host("bastion");
        j.user = "a b".into();
        let p = Profile {
            ssh: ssh(),
            host: &h,
            pin,
            jump: Some((&j, jpin)),
        };
        assert!(ssh_args(&p, &Mode::Command { tty: false }, "cmd").is_err());
        let j = host("bastion");
        for bad in ["/usr/bin/ss h", "/usr/bin/ssh;id"] {
            let p = Profile {
                ssh: Path::new(bad),
                host: &h,
                pin,
                jump: Some((&j, jpin)),
            };
            assert!(
                ssh_args(&p, &Mode::Command { tty: false }, "cmd").is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_scan_accepts_a_new_key_into_a_scratch_file_only_and_offers_no_key() {
        let h = host("build");
        let pin = Path::new("/s/hosts/build.known_hosts");
        let scratch = Path::new("/tmp/scan-1/known_hosts");
        let p = Profile {
            ssh: ssh(),
            host: &h,
            pin,
            jump: None,
        };
        let args = ssh_args(&p, &Mode::Scan { scratch }, "true").unwrap();
        assert!(has(&args, "StrictHostKeyChecking=accept-new"));
        assert!(has(&args, "UserKnownHostsFile=/tmp/scan-1/known_hosts"));
        assert!(has(&args, "PubkeyAuthentication=no"));
        assert!(!opts(&args).iter().any(|o| o.starts_with("IdentityFile")));
        assert!(
            !args.iter().any(|a| a.contains("build.known_hosts")),
            "the existing pin is not touched"
        );
    }

    #[test]
    fn paths_with_spaces_percent_or_tilde_are_refused() {
        let h = host("build");
        for bad in ["/a b/k", "/a%d/k", "~/k", "/a\"b", "/a'b", "rel/k"] {
            let p = Profile {
                ssh: ssh(),
                host: &h,
                pin: Path::new(bad),
                jump: None,
            };
            assert!(
                ssh_args(&p, &Mode::Command { tty: false }, "c").is_err(),
                "{bad}"
            );
        }
        let mut h2 = host("build");
        h2.identity_file = "/k ey".into();
        let p = Profile {
            ssh: ssh(),
            host: &h2,
            pin: Path::new("/p"),
            jump: None,
        };
        assert!(ssh_args(&p, &Mode::Command { tty: false }, "c").is_err());
        assert!(ssh_args(
            &Profile {
                ssh: ssh(),
                host: &h,
                pin: Path::new("/p"),
                jump: None
            },
            &Mode::Command { tty: false },
            ""
        )
        .is_err());
    }

    #[test]
    fn the_client_environment_holds_a_path_a_language_and_optionally_a_terminal() {
        let env = client_env(Path::new("/opt/ssh/bin/ssh"), true);
        let names: Vec<_> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["PATH", "LC_ALL", "TERM"]);
        assert!(env[0].1.starts_with("/opt/ssh/bin:"));
        let plain = client_env(Path::new("/usr/bin/ssh"), false);
        assert_eq!(
            plain,
            vec![
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("LC_ALL".into(), "C".into())
            ]
        );
    }
}
