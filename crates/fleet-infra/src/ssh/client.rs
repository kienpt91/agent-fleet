//! One configured set of hosts and the one `ssh` program: running a [`RemoteCall`] on a host,
//! explaining what went wrong, and remembering which hosts are down.

use super::argv::{client_env, ssh_args, Mode, Profile};
use super::exec::{self, Done, Ending, Input, Limits, Output, Spec};
use super::known_hosts::{host_field, pin_path, read_pin, HostKey};
use crate::jobs::JobTracker;
use fleet_app::hosts::HostDef;
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::{show, HostName};
use fleet_domain::redact::Redactor;
use fleet_domain::remote::{RemoteCall, DEFAULT_REMOTE_PATH};
use fleet_domain::sanitize::{head_str, sanitize_text};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a host that could not be reached is not tried again.
const DOWN_FOR: Duration = Duration::from_secs(120);

/// Time limits of one kind of call.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub total: Duration,
    pub stall: Duration,
}

/// The limits of every kind of call; tests make them short.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// Checks and small commands.
    pub quick: Timing,
    /// Sending the code and the brief.
    pub prepare: Timing,
    /// Committing and bringing the work back.
    pub collect: Timing,
    /// Signals and removals.
    pub cleanup: Timing,
}

impl Default for Timeouts {
    fn default() -> Timeouts {
        let t = |total: u64, stall: u64| Timing {
            total: Duration::from_secs(total),
            stall: Duration::from_secs(stall),
        };
        Timeouts {
            quick: t(60, 40),
            prepare: t(120, 60),
            collect: t(600, 120),
            cleanup: t(60, 30),
        }
    }
}

/// Why a call to a host did not work, in the terms an operator acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The key the host presented is not the pinned one.
    HostKeyChanged,
    /// There is no pin for the host (or its jump host).
    NotTrusted,
    /// The host refused our key.
    AuthRefused,
    /// The host could not be reached.
    Unreachable,
    /// The identity file is unusable.
    Identity,
    /// ssh or the remote command ended with an error.
    Remote,
    /// Time ran out or the connection went silent.
    Timeout,
    /// More output than allowed.
    Limit,
}

/// The arguments and environment of one ssh client process.
#[derive(Clone, Debug)]
pub struct Invocation {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

pub struct SshClient {
    pub ssh: PathBuf,
    pub hosts: BTreeMap<HostName, HostDef>,
    pub state: PathBuf,
    pub tracker: JobTracker,
    pub timeouts: Timeouts,
    /// Applied to what ssh and the remote programs print before it is shown or stored.
    redactor: Option<Arc<Redactor>>,
    down: Mutex<BTreeMap<HostName, Instant>>,
}

/// Text from the network or from a remote program as one short, safe line (or a few).
pub fn clean(text: &[u8], max: usize) -> String {
    let raw = String::from_utf8_lossy(text);
    let safe = sanitize_text(raw.as_bytes());
    head_str(safe.trim(), max).to_string()
}

impl SshClient {
    pub fn new(
        ssh: PathBuf,
        hosts: BTreeMap<HostName, HostDef>,
        state: PathBuf,
        tracker: JobTracker,
    ) -> SshClient {
        SshClient {
            ssh,
            hosts,
            state,
            tracker,
            timeouts: Timeouts::default(),
            redactor: None,
            down: Mutex::new(BTreeMap::new()),
        }
    }

    /// Redacts secrets from the text of errors (what a remote program printed).
    pub fn with_redactor(mut self, redactor: Arc<Redactor>) -> SshClient {
        self.redactor = Some(redactor);
        self
    }

    fn scrub(&self, text: String) -> String {
        match &self.redactor {
            Some(r) => crate::jobs::redact_block(r, &text),
            None => text,
        }
    }

    pub fn host(&self, name: &HostName) -> Result<&HostDef> {
        self.hosts.get(name).ok_or_else(|| {
            FleetError::not_found(format!("host {} is not configured", show(name.as_str())))
        })
    }

    pub fn pin_file(&self, host: &HostName) -> PathBuf {
        pin_path(&self.state, host)
    }

    /// The host's pinned key, when there is one.
    pub fn pinned(&self, host: &HostDef) -> Result<Option<HostKey>> {
        read_pin(
            &self.pin_file(&host.name),
            &host_field(&host.address, host.port),
        )
    }

    /// Refuses a host (or its jump host) that has no pin: no connection is made without one.
    pub fn require_pins(&self, host: &HostDef) -> Result<()> {
        let mut chain = vec![host];
        if let Some(j) = &host.proxy_jump {
            chain.push(self.host(j)?);
        }
        for h in chain {
            if self.pinned(h)?.is_none() {
                return Err(FleetError::forbidden(format!(
                    "host {} has no pinned key yet: compare its fingerprint and run `fleet host trust {}`",
                    h.name, h.name
                )));
            }
        }
        Ok(())
    }

    fn profile<'a>(
        &'a self,
        host: &'a HostDef,
        pins: &'a (PathBuf, Option<PathBuf>),
    ) -> Result<Profile<'a>> {
        let jump = match &host.proxy_jump {
            Some(j) => {
                let def = self.host(j)?;
                let jp = pins
                    .1
                    .as_deref()
                    .ok_or_else(|| FleetError::internal("jump pin"))?;
                Some((def, jp))
            }
            None => None,
        };
        Ok(Profile {
            ssh: &self.ssh,
            host,
            pin: &pins.0,
            jump,
        })
    }

    fn pins(&self, host: &HostDef) -> (PathBuf, Option<PathBuf>) {
        (
            self.pin_file(&host.name),
            host.proxy_jump.as_ref().map(|j| self.pin_file(j)),
        )
    }

    /// The `PATH` of remote git and helpers: the host's own `PATH` setting or a standard one.
    pub fn remote_path(host: &HostDef) -> &str {
        host.env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map_or(DEFAULT_REMOTE_PATH, |(_, v)| v.as_str())
    }

    /// The local command (ssh and its arguments) that runs `call` on `host`.
    pub fn command_for(
        &self,
        host: &HostDef,
        call: &RemoteCall<'_>,
        tty: bool,
    ) -> Result<Invocation> {
        self.require_pins(host)?;
        let line = call.assemble(Self::remote_path(host))?;
        let pins = self.pins(host);
        let profile = self.profile(host, &pins)?;
        let args = ssh_args(&profile, &Mode::Command { tty }, &line)?;
        Ok(Invocation {
            args,
            env: client_env(&self.ssh, tty),
        })
    }

    /// Runs `call` on `host` and waits for it (bounded).
    pub fn run(
        &self,
        host: &HostDef,
        call: &RemoteCall<'_>,
        input: Input,
        output: Output,
        timing: Timing,
    ) -> Result<Done> {
        if let Some(until) = self.down_until(&host.name) {
            return Err(FleetError::unavailable(format!(
                "host {} could not be reached a moment ago; not trying again for {} s",
                host.name,
                until.as_secs().max(1)
            )));
        }
        let invocation = self.command_for(host, call, false)?;
        let done = exec::run(
            &Spec {
                program: self.ssh.clone(),
                args: invocation.args,
                env: invocation.env,
                input,
                output,
                limits: Limits {
                    total: timing.total,
                    stall: timing.stall,
                },
            },
            &self.tracker,
        );
        if matches!(
            self.classify(&done),
            Some(Failure::Unreachable | Failure::Timeout)
        ) {
            self.mark_down(&host.name);
        }
        Ok(done)
    }

    fn down_until(&self, host: &HostName) -> Option<Duration> {
        let mut down = self
            .down
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match down.get(host) {
            Some(at) if at.elapsed() < DOWN_FOR => Some(DOWN_FOR.saturating_sub(at.elapsed())),
            Some(_) => {
                down.remove(host);
                None
            }
            None => None,
        }
    }

    fn mark_down(&self, host: &HostName) {
        self.down
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(host.clone(), Instant::now());
    }

    /// Forgets that a host was down (after an explicit check that worked).
    pub fn mark_up(&self, host: &HostName) {
        self.down
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(host);
    }

    /// What kind of failure `done` is, `None` when it worked or the remote command itself ended
    /// with an error of its own (exit code other than 255).
    pub fn classify(&self, done: &Done) -> Option<Failure> {
        match &done.ending {
            Ending::TimedOut | Ending::Stalled => Some(Failure::Timeout),
            Ending::OutputLimit => Some(Failure::Limit),
            Ending::SpawnFailed(_) => Some(Failure::Remote),
            Ending::Exited { code: Some(0), .. } => None,
            Ending::Exited {
                code: Some(255), ..
            }
            | Ending::Exited { code: None, .. } => {
                let text = String::from_utf8_lossy(&done.stderr);
                Some(classify_ssh_messages(&text))
            }
            Ending::Exited { .. } => None,
        }
    }

    /// An error for a call that did not succeed, with the reason in operator terms. `what` names
    /// the step ("sending the code").
    pub fn explain(&self, host: &HostDef, what: &str, done: &Done) -> FleetError {
        let detail = self.scrub(clean(&done.stderr, 300));
        let host_name = &host.name;
        match self.classify(done) {
            Some(Failure::HostKeyChanged) => FleetError::forbidden(format!(
                "{what}: the key of host {host_name} is not the pinned one (REMOTE HOST IDENTIFICATION HAS CHANGED); nothing was sent. If the host was reinstalled on purpose, compare the new fingerprint and run `fleet host trust {host_name}` again; otherwise treat it as an attack"
            )),
            Some(Failure::NotTrusted) => FleetError::forbidden(format!(
                "{what}: host {host_name} offered a key that is not pinned; run `fleet host trust {host_name}` after comparing its fingerprint"
            )),
            Some(Failure::AuthRefused) => FleetError::forbidden(format!(
                "{what}: host {host_name} refused the identity file of the host entry"
            )),
            Some(Failure::Identity) => FleetError::invalid(format!(
                "{what}: the identity file of host {host_name} cannot be used ({detail})"
            )),
            Some(Failure::Unreachable) => FleetError::unavailable(format!(
                "{what}: host {host_name} could not be reached ({detail})"
            )),
            Some(Failure::Timeout) => FleetError::new(
                fleet_domain::error::ErrorCode::Timeout,
                format!("{what}: the connection to host {host_name} timed out or went silent"),
            ),
            Some(Failure::Limit) => FleetError::limit(format!(
                "{what}: host {host_name} sent more data than allowed"
            )),
            Some(Failure::Remote) | None => match &done.ending {
                Ending::SpawnFailed(m) => {
                    FleetError::unavailable(format!("{what}: {}", clean(m.as_bytes(), 200)))
                }
                Ending::Exited { code, .. } => FleetError::internal(format!(
                    "{what}: the command on host {host_name} ended with {} ({detail})",
                    code.map_or("a signal".to_string(), |c| format!("code {c}"))
                )),
                _ => FleetError::internal(format!("{what}: failed ({detail})")),
            },
        }
    }

    /// The pin file path for tests and the CLI.
    pub fn state_dir(&self) -> &Path {
        &self.state
    }
}

/// Sorts the text ssh printed before it gave up (exit code 255).
pub fn classify_ssh_messages(text: &str) -> Failure {
    let has = |needle: &str| text.contains(needle);
    if has("REMOTE HOST IDENTIFICATION HAS CHANGED") || has("Offending") {
        Failure::HostKeyChanged
    } else if has("Host key verification failed") || has("strict checking") {
        Failure::NotTrusted
    } else if has("Permission denied") {
        Failure::AuthRefused
    } else if has("Load key")
        || has("no such identity")
        || has("bad permissions")
        || has("UNPROTECTED PRIVATE KEY")
        || has("invalid format")
        || has("incorrect passphrase")
    {
        Failure::Identity
    } else if has("Connection refused")
        || has("timed out")
        || has("No route to host")
        || has("Could not resolve")
        || has("Network is unreachable")
        || has("Connection closed")
        || has("Connection reset")
        || has("kex_exchange_identification")
        || has("Name or service not known")
    {
        Failure::Unreachable
    } else {
        Failure::Remote
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_messages_are_sorted_by_what_the_operator_does_about_them() {
        assert_eq!(
            classify_ssh_messages("@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@\nHost key verification failed."),
            Failure::HostKeyChanged
        );
        assert_eq!(
            classify_ssh_messages("No ED25519 host key is known for [127.0.0.1]:22 and you have requested strict checking.\nHost key verification failed."),
            Failure::NotTrusted
        );
        assert_eq!(
            classify_ssh_messages("fleet@h: Permission denied (publickey)."),
            Failure::AuthRefused
        );
        assert_eq!(
            classify_ssh_messages("ssh: connect to host h port 22: Connection refused"),
            Failure::Unreachable
        );
        assert_eq!(
            classify_ssh_messages("ssh: Could not resolve hostname h: Name or service not known"),
            Failure::Unreachable
        );
        assert_eq!(
            classify_ssh_messages("Load key \"/k\": invalid format"),
            Failure::Identity
        );
        assert_eq!(classify_ssh_messages("something else"), Failure::Remote);
    }

    #[test]
    fn clean_text_is_short_and_free_of_terminal_controls() {
        let t = clean(b"\x1b[31mred\x1b]0;title\x07 text\n\rmore", 100);
        assert!(!t.contains('\u{1b}') && !t.contains('\u{7}'), "{t:?}");
        assert!(clean(&vec![b'a'; 1000], 50).len() <= 60);
    }
}
