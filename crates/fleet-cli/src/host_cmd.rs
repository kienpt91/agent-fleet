//! `fleet host add | list | trust | remove | doctor`: the operator's side of remote hosts. These
//! commands never talk to the daemon: they edit `fleet.toml`, show and pin host keys, and check a
//! host. Pinning is the only way a key becomes trusted, and it needs a person at a terminal.

use crate::args::Parsed;
use fleet_app::hosts::HostDef;
use fleet_domain::error::{ErrorCode, FleetError, Result};
use fleet_domain::ids::{show, HostName};
use fleet_domain::remote::validate_port;
use fleet_domain::sanitize::sanitize_text;
use fleet_domain::value::Value;
use fleet_infra::config_file::{load_config, LoadedConfig};
use fleet_infra::doctor;
use fleet_infra::hosts_file;
use fleet_infra::jobs::JobTracker;
use fleet_infra::ssh::client::SshClient;
use fleet_infra::ssh::known_hosts::{remove_pin, HostKey};
use fleet_infra::ssh::{doctor as ssh_doctor, locate_ssh, trust};
use fleet_infra::statedir::StateLayout;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a scan of a host key may take.
const SCAN_TIMEOUT: Duration = Duration::from_secs(30);
/// What the person types to confirm a key.
const CONFIRM_WORD: &str = "yes";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostAdd {
    pub name: String,
    pub address: String,
    pub user: String,
    pub identity: PathBuf,
    pub base_dir: String,
    pub port: Option<u32>,
    pub workers: Vec<String>,
    pub max_sessions: Option<u64>,
    pub proxy_jump: Option<String>,
    pub env: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostAction {
    Add(Box<HostAdd>),
    List,
    Trust { name: String },
    Remove { name: String },
    Doctor { name: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostCommand {
    pub config: Option<PathBuf>,
    pub action: HostAction,
}

fn required(p: &mut Parsed, name: &str) -> Result<String> {
    p.opt(name)?
        .ok_or_else(|| FleetError::usage(format!("option {name} is required")))
}

fn name_arg(p: Parsed, what: &str) -> Result<String> {
    let mut rest = p.finish()?;
    match rest.len() {
        0 => Err(FleetError::usage(format!("missing {what}"))),
        1 => rest
            .pop()
            .ok_or_else(|| FleetError::internal("argument list changed")),
        _ => Err(FleetError::usage(format!(
            "unexpected argument {} after {what}",
            show(rest.get(1).map_or("", String::as_str))
        ))),
    }
}

/// Parses what follows `host`.
pub fn parse(mut p: Parsed) -> Result<HostCommand> {
    let config = p
        .opt("--config")?
        .map(|c| std::path::absolute(&c).map_err(|e| FleetError::usage(format!("--config: {e}"))))
        .transpose()?;
    let sub = p.take_word();
    let action = match sub.as_deref() {
        Some("add") => {
            let address = required(&mut p, "--address")?;
            let user = required(&mut p, "--user")?;
            let identity = required(&mut p, "--identity")?;
            let base_dir = required(&mut p, "--base-dir")?;
            let port = p
                .opt("--port")?
                .map(|s| {
                    s.parse::<u32>()
                        .map_err(|_| FleetError::usage("option --port expects a whole number"))
                })
                .transpose()?;
            let workers = p.multi("--worker")?;
            let max_sessions = p
                .opt("--max-sessions")?
                .map(|s| {
                    s.parse::<u64>().map_err(|_| {
                        FleetError::usage("option --max-sessions expects a whole number")
                    })
                })
                .transpose()?;
            let proxy_jump = p.opt("--proxy-jump")?;
            let env = p.multi("--env")?;
            let name = name_arg(p, "<name>")?;
            let identity = std::path::absolute(&identity)
                .map_err(|e| FleetError::usage(format!("--identity: {e}")))?;
            HostAction::Add(Box::new(HostAdd {
                name,
                address,
                user,
                identity,
                base_dir,
                port,
                workers,
                max_sessions,
                proxy_jump,
                env,
            }))
        }
        Some("list") => {
            p.finish().and_then(|rest| match rest.first() {
                None => Ok(()),
                Some(x) => Err(FleetError::usage(format!(
                    "unexpected argument {}",
                    show(x)
                ))),
            })?;
            HostAction::List
        }
        Some("trust") => HostAction::Trust {
            name: name_arg(p, "<name>")?,
        },
        Some("remove") => HostAction::Remove {
            name: name_arg(p, "<name>")?,
        },
        Some("doctor") => HostAction::Doctor {
            name: name_arg(p, "<name>")?,
        },
        Some(other) => {
            return Err(FleetError::usage(format!(
                "unknown host subcommand {} (expected add, list, trust, remove or doctor)",
                show(other)
            )))
        }
        None => {
            return Err(FleetError::usage(
                "host needs a subcommand: add, list, trust, remove or doctor",
            ))
        }
    };
    Ok(HostCommand { config, action })
}

// ---- running --------------------------------------------------------------------------------------

pub struct HostContext<'a> {
    pub state_dir: &'a Path,
    pub json: bool,
    pub env: &'a [(String, String)],
}

fn env_value<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
    env.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn write_out(out: &mut dyn Write, text: &str) -> Result<()> {
    out.write_all(text.as_bytes())
        .map_err(|e| FleetError::io("writing output", &e))
}

fn clean(text: &str) -> String {
    sanitize_text(text.as_bytes())
}

fn config_path(ctx: &HostContext<'_>, explicit: Option<&Path>) -> PathBuf {
    match explicit {
        Some(p) => p.to_path_buf(),
        None => StateLayout::new(ctx.state_dir.to_path_buf()).default_config,
    }
}

fn load(ctx: &HostContext<'_>, path: &Path) -> Result<LoadedConfig> {
    if !path.exists() {
        return Err(FleetError::not_found(format!(
            "{} does not exist; `fleet host add` creates it",
            path.display()
        )));
    }
    load_config(path, ctx.env.to_vec())
}

fn client(ctx: &HostContext<'_>, loaded: &LoadedConfig) -> Result<SshClient> {
    let ssh = locate_ssh(
        loaded.remote.ssh_binary.as_deref(),
        env_value(ctx.env, "PATH").unwrap_or(""),
    )?;
    Ok(SshClient::new(
        ssh,
        loaded.app.hosts.clone(),
        ctx.state_dir.to_path_buf(),
        JobTracker::new(),
    ))
}

fn host_of<'a>(loaded: &'a LoadedConfig, name: &str) -> Result<&'a HostDef> {
    let parsed = HostName::parse(name)?;
    loaded
        .app
        .hosts
        .get(&parsed)
        .ok_or_else(|| FleetError::not_found(format!("host {} is not configured", show(name))))
}

/// Runs one host command; returns the exit code.
pub fn run(
    cmd: HostCommand,
    ctx: &HostContext<'_>,
    input: &mut dyn Read,
    out: &mut dyn Write,
) -> Result<i32> {
    let path = config_path(ctx, cmd.config.as_deref());
    match cmd.action {
        HostAction::Add(add) => run_add(&add, &path, out).map(|()| 0),
        HostAction::List => run_list(ctx, &path, out).map(|()| 0),
        HostAction::Trust { name } => run_trust(&name, ctx, &path, input, out).map(|()| 0),
        HostAction::Remove { name } => run_remove(&name, ctx, &path, out).map(|()| 0),
        HostAction::Doctor { name } => run_doctor(&name, ctx, &path, out),
    }
}

fn run_add(add: &HostAdd, path: &Path, out: &mut dyn Write) -> Result<()> {
    let name = HostName::parse(&add.name)?;
    let identity = add
        .identity
        .to_str()
        .ok_or_else(|| FleetError::invalid("--identity is not valid UTF-8"))?;
    let mut host = HostDef::new(name, &add.address, &add.user, identity, &add.base_dir);
    if let Some(p) = add.port {
        host.port = validate_port(p)?;
    }
    host.allowed_workers = add.workers.clone();
    if let Some(n) = add.max_sessions {
        host.max_sessions =
            usize::try_from(n).map_err(|_| FleetError::invalid("--max-sessions is too large"))?;
    }
    host.proxy_jump = add.proxy_jump.as_deref().map(HostName::parse).transpose()?;
    for pair in &add.env {
        let (k, v) = pair.split_once('=').ok_or_else(|| {
            FleetError::usage(format!("--env expects NAME=VALUE, got {}", show(pair)))
        })?;
        host.env.push((k.to_string(), v.to_string()));
    }
    host.validate()?;
    if !add.identity.is_file() {
        return Err(FleetError::not_found(format!(
            "the identity file {} does not exist (create a key pair for fleet with ssh-keygen and install its public half on the host)",
            add.identity.display()
        )));
    }
    let text = hosts_file::read_text(path)?.unwrap_or_default();
    let updated = hosts_file::append_host(&text, &host)?;
    hosts_file::write_text(path, &updated)?;
    write_out(
        out,
        &format!(
            "host {} added to {}\nnext: compare the host's key and pin it with `fleet host trust {}`, then check it with `fleet host doctor {}`\n",
            host.name,
            path.display(),
            host.name,
            host.name
        ),
    )
}

fn run_list(ctx: &HostContext<'_>, path: &Path, out: &mut dyn Write) -> Result<()> {
    if !path.exists() {
        return write_out(
            out,
            if ctx.json {
                "{\"hosts\":[],\"schema\":\"fleet.hosts/1\"}\n"
            } else {
                "no hosts configured\n"
            },
        );
    }
    let loaded = load(ctx, path)?;
    let client = client(ctx, &loaded).ok();
    let mut rows: Vec<[String; 6]> = Vec::new();
    let mut values = Vec::new();
    for h in loaded.app.hosts.values() {
        let key = client
            .as_ref()
            .and_then(|c| c.pinned(h).ok().flatten())
            .or_else(|| {
                let c = StateLayout::new(ctx.state_dir.to_path_buf());
                let pin = fleet_infra::ssh::known_hosts::pin_path(&c.root, &h.name);
                fleet_infra::ssh::known_hosts::read_pin(
                    &pin,
                    &fleet_infra::ssh::known_hosts::host_field(&h.address, h.port),
                )
                .ok()
                .flatten()
            });
        let pinned = key
            .as_ref()
            .map_or("not pinned".to_string(), ssh_doctor::describe);
        rows.push([
            h.name.to_string(),
            format!("{}:{}", h.address, h.port),
            h.user.clone(),
            h.base_dir.clone(),
            if h.allowed_workers.is_empty() {
                "-".to_string()
            } else {
                h.allowed_workers.join(",")
            },
            pinned,
        ]);
        values.push(Value::object([
            ("name", Value::str(h.name.as_str())),
            ("address", Value::str(&h.address)),
            ("port", Value::int(h.port)),
            ("user", Value::str(&h.user)),
            ("base_dir", Value::str(&h.base_dir)),
            (
                "allowed_workers",
                Value::array(h.allowed_workers.iter().map(Value::str)),
            ),
            ("max_sessions", Value::int(h.max_sessions as u64)),
            (
                "proxy_jump",
                h.proxy_jump
                    .as_ref()
                    .map_or(Value::Null, |j| Value::str(j.as_str())),
            ),
            (
                "pinned",
                key.as_ref()
                    .map_or(Value::Null, |k| Value::str(ssh_doctor::describe(k))),
            ),
        ]));
    }
    if ctx.json {
        let doc = Value::object([
            ("schema", Value::str("fleet.hosts/1")),
            ("hosts", Value::array(values)),
        ]);
        let mut text = doc.canonical();
        text.push('\n');
        return write_out(out, &text);
    }
    if rows.is_empty() {
        return write_out(out, "no hosts configured\n");
    }
    let headers = [
        "NAME",
        "ADDRESS",
        "USER",
        "BASE DIRECTORY",
        "WORKERS",
        "PINNED KEY",
    ]
    .map(String::from);
    let mut all = vec![headers];
    all.extend(rows);
    let mut widths = [0usize; 5];
    for row in &all {
        for (w, cell) in widths.iter_mut().zip(row.iter()) {
            *w = (*w).max(clean(cell).chars().count());
        }
    }
    let mut text = String::new();
    for row in &all {
        let mut line = String::new();
        for (i, cell) in row.iter().enumerate() {
            let cell = clean(cell);
            match widths.get(i) {
                Some(w) => line.push_str(&format!("{cell:<w$}  ")),
                None => line.push_str(&cell),
            }
        }
        text.push_str(line.trim_end());
        text.push('\n');
    }
    write_out(out, &text)
}

/// What a person is shown, and what they must do.
pub fn trust_prompt(host: &HostDef, scanned: &trust::Scanned, pinned: Option<&HostKey>) -> String {
    let mut text = format!(
        "host {} ({}:{}) presented this key:\n  type:        {}\n  fingerprint: {}\n",
        host.name,
        host.address,
        host.port,
        scanned.key.key_type,
        scanned.key.fingerprint()
    );
    match pinned {
        None => text.push_str("nothing is pinned for this host yet.\n"),
        Some(old) => text.push_str(&format!(
            "WARNING: a different key is pinned now ({} {}). A host that was reinstalled has a new key; so does a machine that pretends to be it.\n",
            old.key_type,
            old.fingerprint()
        )),
    }
    text.push_str(
        "Compare the fingerprint with one you know to be the host's own, over another channel (for example on its console: `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`).\n",
    );
    text
}

/// True when the answer is the confirmation word, nothing else.
pub fn is_confirmation(answer: &str) -> bool {
    answer.trim() == CONFIRM_WORD
}

fn run_trust(
    name: &str,
    ctx: &HostContext<'_>,
    path: &Path,
    input: &mut dyn Read,
    out: &mut dyn Write,
) -> Result<()> {
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !interactive {
        return Err(FleetError::forbidden(
            "`fleet host trust` needs a terminal: it shows a key fingerprint and waits for you to confirm it, and nothing pins a key without that",
        ));
    }
    let loaded = load(ctx, path)?;
    let host = host_of(&loaded, name)?;
    let client = client(ctx, &loaded)?;
    let pinned = client.pinned(host)?;
    let scanned = trust::scan(&client, host, SCAN_TIMEOUT)?;
    if pinned.as_ref() == Some(&scanned.key) {
        return write_out(
            out,
            &format!(
                "host {} presented {} {}, which is the key already pinned; nothing changed\n",
                host.name,
                scanned.key.key_type,
                scanned.key.fingerprint()
            ),
        );
    }
    write_out(out, &trust_prompt(host, &scanned, pinned.as_ref()))?;
    write_out(
        out,
        &format!(
            "Type {CONFIRM_WORD} to pin this key for host {}: ",
            host.name
        ),
    )?;
    out.flush()
        .map_err(|e| FleetError::io("writing output", &e))?;
    let mut line = String::new();
    // The answer is read from the same stdin the command line was given (a terminal, checked
    // above), at most 200 bytes of it.
    std::io::BufReader::new(input.take(200))
        .read_line(&mut line)
        .map_err(|e| FleetError::io("reading the answer", &e))?;
    if !is_confirmation(&line) {
        return Err(FleetError::new(
            ErrorCode::Forbidden,
            "not confirmed; nothing was pinned",
        ));
    }
    trust::pin(&client, host, &scanned)?;
    write_out(
        out,
        &format!(
            "pinned {} for host {}; check the host with `fleet host doctor {}`\n",
            scanned.key.fingerprint(),
            host.name,
            host.name
        ),
    )
}

fn run_remove(name: &str, ctx: &HostContext<'_>, path: &Path, out: &mut dyn Write) -> Result<()> {
    let host_name = HostName::parse(name)?;
    let text = hosts_file::read_text(path)?
        .ok_or_else(|| FleetError::not_found(format!("{} does not exist", path.display())))?;
    let loaded = load(ctx, path)?;
    if let Some(user) = loaded
        .app
        .hosts
        .values()
        .find(|h| h.proxy_jump.as_ref() == Some(&host_name))
    {
        return Err(FleetError::conflict(format!(
            "host {} is the jump host of {}; remove that one first",
            host_name, user.name
        )));
    }
    let updated = hosts_file::remove_host(&text, &host_name)?;
    hosts_file::write_text(path, &updated)?;
    let pin = fleet_infra::ssh::known_hosts::pin_path(ctx.state_dir, &host_name);
    remove_pin(&pin)?;
    write_out(
        out,
        &format!(
            "host {host_name} removed from {} and its pinned key forgotten; tasks that name it will fail until it is added again\n",
            path.display()
        ),
    )
}

fn run_doctor(name: &str, ctx: &HostContext<'_>, path: &Path, out: &mut dyn Write) -> Result<i32> {
    let loaded = load(ctx, path)?;
    let host = host_of(&loaded, name)?;
    let client = client(ctx, &loaded)?;
    let checks = ssh_doctor::full_checks(&client, host);
    let text = if ctx.json {
        let mut t = doctor::to_value(&checks).canonical();
        t.push('\n');
        t
    } else {
        doctor::render_text(&checks)
    };
    write_out(out, &text)?;
    Ok(if doctor::is_healthy(&checks) {
        0
    } else {
        ErrorCode::Unhealthy.exit_code()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn parse_args(a: &[&str]) -> Result<HostCommand> {
        parse(Parsed::new(&argv(a)).unwrap())
    }

    #[test]
    fn the_subcommands_and_their_options_are_parsed() {
        let c = parse_args(&[
            "add",
            "build",
            "--address",
            "h.example",
            "--user",
            "fleet",
            "--identity",
            "/k",
            "--base-dir",
            "/srv/f",
            "--port",
            "2222",
            "--worker",
            "agent",
            "--worker",
            "other",
            "--max-sessions",
            "3",
            "--proxy-jump",
            "bastion",
            "--env",
            "A=b",
        ])
        .unwrap();
        let HostAction::Add(add) = c.action else {
            panic!("{c:?}")
        };
        assert_eq!(add.name, "build");
        assert_eq!(add.port, Some(2222));
        assert_eq!(add.workers, ["agent", "other"]);
        assert_eq!(add.max_sessions, Some(3));
        assert_eq!(add.proxy_jump.as_deref(), Some("bastion"));
        assert_eq!(add.env, ["A=b"]);
        assert_eq!(parse_args(&["list"]).unwrap().action, HostAction::List);
        assert_eq!(
            parse_args(&["trust", "build"]).unwrap().action,
            HostAction::Trust {
                name: "build".into()
            }
        );
        assert_eq!(
            parse_args(&["remove", "build"]).unwrap().action,
            HostAction::Remove {
                name: "build".into()
            }
        );
        assert_eq!(
            parse_args(&["doctor", "build"]).unwrap().action,
            HostAction::Doctor {
                name: "build".into()
            }
        );
        let with_config = parse_args(&["--config", "/c.toml", "list"]).unwrap();
        assert_eq!(with_config.config, Some(PathBuf::from("/c.toml")));
    }

    #[test]
    fn mistakes_are_usage_errors() {
        for bad in [
            vec![],
            vec!["frobnicate"],
            vec!["trust"],
            vec!["trust", "a", "b"],
            vec!["list", "extra"],
            vec!["add", "build"],
            vec![
                "add",
                "build",
                "--address",
                "h",
                "--user",
                "u",
                "--identity",
                "/k",
            ],
            vec![
                "add",
                "--address",
                "h",
                "--user",
                "u",
                "--identity",
                "/k",
                "--base-dir",
                "/b",
            ],
            vec![
                "add",
                "x",
                "--address",
                "h",
                "--user",
                "u",
                "--identity",
                "/k",
                "--base-dir",
                "/b",
                "--port",
                "x",
            ],
            vec!["trust", "a", "--bogus"],
        ] {
            let err = parse_args(&bad).unwrap_err();
            assert_eq!(err.code, ErrorCode::Usage, "{bad:?}");
        }
    }

    #[test]
    fn only_the_word_yes_confirms() {
        assert!(is_confirmation("yes\n"));
        assert!(is_confirmation(" yes "));
        for no in ["", "y", "YES", "Yes", "yes please", "no", "yes\nno", "\n"] {
            assert!(!is_confirmation(no), "{no:?}");
        }
    }

    #[test]
    fn the_prompt_shows_the_fingerprint_and_warns_about_a_replaced_pin() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&11u32.to_be_bytes());
        blob.extend_from_slice(b"ssh-ed25519");
        blob.extend_from_slice(&32u32.to_be_bytes());
        blob.extend_from_slice(&[9u8; 32]);
        let b64 = {
            const A: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut s = String::new();
            for chunk in blob.chunks(3) {
                let b = [
                    chunk[0],
                    *chunk.get(1).unwrap_or(&0),
                    *chunk.get(2).unwrap_or(&0),
                ];
                let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
                for i in 0..4 {
                    if i <= chunk.len() {
                        s.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
                    } else {
                        s.push('=');
                    }
                }
            }
            s
        };
        let key = HostKey {
            key_type: "ssh-ed25519".into(),
            base64: b64,
        };
        let host = HostDef::new(
            HostName::parse("build").unwrap(),
            "h.example",
            "u",
            "/k",
            "/b",
        );
        let scanned = trust::Scanned {
            key: key.clone(),
            field: "h.example".into(),
        };
        let first = trust_prompt(&host, &scanned, None);
        assert!(
            first.contains(&key.fingerprint()) && first.contains("nothing is pinned"),
            "{first}"
        );
        let again = trust_prompt(&host, &scanned, Some(&key));
        assert!(again.contains("WARNING"), "{again}");
    }
}
