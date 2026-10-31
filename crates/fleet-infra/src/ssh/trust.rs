//! Learning a host's key for the operator to confirm. Nothing here pins anything: `scan` only
//! reports what the host presents; the CLI shows the fingerprint, asks a person on a terminal, and
//! only then calls [`pin`].

use super::argv::{client_env, ssh_args, Mode, Profile};
use super::client::{clean, SshClient};
use super::exec::{self, Input, Limits, Output, Spec};
use super::known_hosts::{host_field, parse_scan_result, write_pin, HostKey};
use crate::fsx;
use fleet_app::hosts::HostDef;
use fleet_domain::error::{FleetError, Result};
use std::fs;
use std::time::Duration;

/// What a host presented.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scanned {
    pub key: HostKey,
    /// The host field a pin for this host has (`address` or `[address]:port`).
    pub field: String,
}

/// Connects to `host` once, without offering a key and without running anything, and returns the
/// host key it presented. The key is accepted into a scratch file only. A jump host must be pinned
/// already: its key is never learned this way.
pub fn scan(client: &SshClient, host: &HostDef, total: Duration) -> Result<Scanned> {
    let jump = match &host.proxy_jump {
        Some(j) => {
            let def = client.host(j)?;
            if client.pinned(def)?.is_none() {
                return Err(FleetError::forbidden(format!(
                    "host {} is reached through {j}, which has no pinned key yet: run `fleet host trust {j}` first",
                    host.name
                )));
            }
            Some(def)
        }
        None => None,
    };
    let root = client.state_dir().join("remote");
    fsx::ensure_private_dir(&root)?;
    let dir = root.join(format!("scan-{}-{}", std::process::id(), host.name));
    let _ = fs::remove_dir_all(&dir);
    fsx::ensure_private_dir(&dir)?;
    let scratch = dir.join("known_hosts");
    let result = scan_into(client, host, jump, &scratch, total);
    let _ = fs::remove_dir_all(&dir);
    result
}

fn scan_into(
    client: &SshClient,
    host: &HostDef,
    jump: Option<&HostDef>,
    scratch: &std::path::Path,
    total: Duration,
) -> Result<Scanned> {
    let pin = client.pin_file(&host.name);
    let jump_pin = jump.map(|j| client.pin_file(&j.name));
    let profile = Profile {
        ssh: &client.ssh,
        host,
        pin: &pin,
        jump: match (jump, jump_pin.as_deref()) {
            (Some(j), Some(p)) => Some((j, p)),
            _ => None,
        },
    };
    let args = ssh_args(&profile, &Mode::Scan { scratch }, "true")?;
    let done = exec::run(
        &Spec {
            program: client.ssh.clone(),
            args,
            env: client_env(&client.ssh, false),
            input: Input::None,
            output: Output::Capture { max: 4096 },
            limits: Limits {
                total,
                stall: total,
            },
        },
        &client.tracker,
    );
    let field = host_field(&host.address, host.port);
    let text = match fs::read_to_string(scratch) {
        Ok(t) => t,
        Err(_) => {
            return Err(FleetError::unavailable(format!(
                "host {} presented no key ({})",
                host.name,
                clean(&done.stderr, 300)
            )))
        }
    };
    let key = parse_scan_result(&text, &field)?;
    Ok(Scanned { key, field })
}

/// Pins `key` for `host`, replacing an earlier pin. Called only after a person confirmed.
pub fn pin(client: &SshClient, host: &HostDef, scanned: &Scanned) -> Result<()> {
    write_pin(&client.pin_file(&host.name), &scanned.field, &scanned.key)?;
    client.mark_up(&host.name);
    Ok(())
}
