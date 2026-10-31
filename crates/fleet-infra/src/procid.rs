//! Reading and signalling processes, with protection against pid reuse.
//!
//! A pid alone does not identify a process: after the original exits the number can belong to an
//! unrelated one. Every signal sent on behalf of an earlier reading is therefore preceded by a
//! comparison of the process's start token with the one recorded when the worker was started
//! (domain rule: `ProcessIdentity::is_same_process`), and only a process group leader is signalled
//! as a group.

use fleet_app::ports::SignalOutcome;
use fleet_domain::supervision::{ProcInfo, ProcessIdentity};
use nix::errno::Errno;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use std::time::{Duration, Instant};

/// Longest ancestry chain followed when looking for a worker ancestor.
const MAX_ANCESTRY: usize = 32;

#[cfg(target_os = "linux")]
fn read_stat(pid: u32) -> Option<ProcStat> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&text)
}

/// The fields of `/proc/<pid>/stat` fleet needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcStat {
    pub ppid: u32,
    pub pgid: u32,
    pub start_ticks: String,
}

/// Parses `/proc/<pid>/stat`. The command name sits in parentheses and may itself contain spaces
/// and parentheses, so the fields are located after the LAST closing parenthesis.
pub fn parse_stat(text: &str) -> Option<ProcStat> {
    let close = text.rfind(')')?;
    let rest = text.get(close + 1..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // fields[0] = state, [1] = ppid, [2] = pgrp, [3] = session, ..., [19] = starttime
    Some(ProcStat {
        ppid: fields.get(1)?.parse().ok()?,
        pgid: fields.get(2)?.parse().ok()?,
        start_ticks: (*fields.get(19)?).to_string(),
    })
}

#[cfg(not(target_os = "linux"))]
fn ps(pid: u32, columns: &str) -> Option<String> {
    use std::process::{Command, Stdio};
    let out = Command::new("/bin/ps")
        .args(["-o", columns, "-p", &pid.to_string()])
        .env_clear()
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let line = text.lines().next()?.trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// A token that stays the same for the lifetime of one process and differs for any later process
/// that reuses the pid. `None` when no such process exists or it cannot be read.
pub fn start_token(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        read_stat(pid).map(|s| s.start_ticks)
    }
    #[cfg(not(target_os = "linux"))]
    {
        ps(pid, "lstart=")
    }
}

/// Parent and process group of a process.
pub fn proc_info(pid: u32) -> Option<ProcInfo> {
    if pid == 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        read_stat(pid).map(|s| ProcInfo {
            pid,
            ppid: s.ppid,
            pgid: s.pgid,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let line = ps(pid, "ppid=,pgid=")?;
        let mut it = line.split_whitespace();
        Some(ProcInfo {
            pid,
            ppid: it.next()?.parse().ok()?,
            pgid: it.next()?.parse().ok()?,
        })
    }
}

/// The process and its ancestors, nearest first, ending before pid 1.
pub fn ancestry(pid: u32) -> Vec<ProcInfo> {
    let mut chain = Vec::new();
    let mut cur = pid;
    while chain.len() < MAX_ANCESTRY && cur > 1 {
        let Some(info) = proc_info(cur) else { break };
        let next = info.ppid;
        chain.push(info);
        if next == cur {
            break;
        }
        cur = next;
    }
    chain
}

fn to_pid(pid: u32) -> Option<Pid> {
    i32::try_from(pid)
        .ok()
        .filter(|p| *p > 1)
        .map(Pid::from_raw)
}

/// True while any process of the group exists.
pub fn group_exists(pgid: u32) -> bool {
    match to_pid(pgid) {
        Some(p) => killpg(p, None).is_ok(),
        None => false,
    }
}

/// Signals the group led by `identity.pid` after verifying that the process is still the one that
/// was started and that it leads its own group. Never signals on a mismatch or on doubt.
pub fn signal_verified(identity: &ProcessIdentity, signal: Signal) -> SignalOutcome {
    let current = start_token(identity.pid);
    if current.is_none() {
        return SignalOutcome::Gone;
    }
    if !identity.is_same_process(current.as_deref()) {
        return SignalOutcome::Refused(format!(
            "pid {} now belongs to a different process (start time differs)",
            identity.pid
        ));
    }
    match proc_info(identity.pid) {
        Some(info) if info.pgid == identity.pid => {}
        Some(_) => {
            return SignalOutcome::Refused(format!(
                "pid {} does not lead a process group",
                identity.pid
            ));
        }
        None => return SignalOutcome::Gone,
    }
    let Some(pid) = to_pid(identity.pid) else {
        return SignalOutcome::Refused("refusing to signal pid 0 or 1".to_string());
    };
    match killpg(pid, signal) {
        Ok(()) => SignalOutcome::Sent,
        Err(Errno::ESRCH) => SignalOutcome::Gone,
        Err(e) => SignalOutcome::Refused(format!("signal failed: {e}")),
    }
}

/// Stops leftover members of a group whose leader was just reaped. The leader is gone, so the pid
/// cannot be verified any more; the group id cannot be reused while the group has members, and
/// the call happens right after the reap, which is the only window in which a recycled pid could
/// be mistaken (documented residual risk).
pub fn kill_group_leftovers(pgid: u32) {
    if let Some(pid) = to_pid(pgid) {
        if killpg(pid, None).is_ok() {
            let _ = killpg(pid, Signal::SIGKILL);
        }
    }
}

/// Terminates the process group of a process recorded by an earlier daemon run: TERM, wait up to
/// `grace_ms`, then KILL. Blocking; used at startup only.
pub fn terminate_group_blocking(identity: &ProcessIdentity, grace_ms: u64) -> SignalOutcome {
    match signal_verified(identity, Signal::SIGTERM) {
        SignalOutcome::Sent => {}
        other => return other,
    }
    let deadline = Instant::now() + Duration::from_millis(grace_ms);
    while Instant::now() < deadline {
        if !identity_alive(identity) {
            return SignalOutcome::Sent;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    match signal_verified(identity, Signal::SIGKILL) {
        SignalOutcome::Gone => SignalOutcome::Sent,
        other => {
            // Give the kernel a moment to deliver, then report what happened.
            std::thread::sleep(Duration::from_millis(50));
            other
        }
    }
}

fn identity_alive(identity: &ProcessIdentity) -> bool {
    identity.is_same_process(start_token(identity.pid).as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn spawn_sleeper(trap_term: bool) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        let script = if trap_term {
            "trap '' TERM; while :; do sleep 1; done"
        } else {
            "sleep 30"
        };
        Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap()
    }

    #[test]
    fn stat_parser_handles_hostile_command_names() {
        let line = "42 (we ird) name) S 7 42 42 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 12345 67 18446744073709551615";
        let s = parse_stat(line).unwrap();
        assert_eq!((s.ppid, s.pgid, s.start_ticks.as_str()), (7, 42, "987654"));
        assert!(parse_stat("garbage").is_none());
        assert!(parse_stat("1 (x) S 1").is_none());
        assert!(parse_stat("1 (x) S a b c d e f g h i j k l m n o p q 5").is_none());
    }

    #[test]
    fn own_process_has_a_token_and_a_parent() {
        let me = std::process::id();
        assert!(start_token(me).is_some());
        let info = proc_info(me).unwrap();
        assert!(info.ppid >= 1);
        assert_eq!(start_token(me), start_token(me), "the token is stable");
    }

    #[test]
    fn missing_processes_have_no_token() {
        assert_eq!(start_token(0), None);
        assert_eq!(start_token(u32::MAX), None);
        assert!(proc_info(u32::MAX).is_none());
    }

    #[test]
    fn ancestry_starts_with_the_process_itself_and_is_bounded() {
        let chain = ancestry(std::process::id());
        assert_eq!(chain.first().map(|p| p.pid), Some(std::process::id()));
        assert!(chain.len() <= MAX_ANCESTRY);
        assert!(ancestry(0).is_empty());
        assert!(ancestry(1).is_empty());
    }

    #[test]
    fn verified_signal_terminates_the_group() {
        let mut child = spawn_sleeper(false);
        let id = ProcessIdentity::new(child.id(), start_token(child.id()).unwrap());
        assert_eq!(signal_verified(&id, Signal::SIGTERM), SignalOutcome::Sent);
        let status = child.wait().unwrap();
        assert!(!status.success());
    }

    #[test]
    fn a_wrong_start_token_is_refused_and_the_process_survives() {
        let mut child = spawn_sleeper(false);
        let real = start_token(child.id()).unwrap();
        let forged = ProcessIdentity::new(child.id(), format!("{real}0"));
        match signal_verified(&forged, Signal::SIGKILL) {
            SignalOutcome::Refused(why) => assert!(why.contains("different process"), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "the process must still be running"
        );
        let genuine = ProcessIdentity::new(child.id(), real);
        assert_eq!(
            signal_verified(&genuine, Signal::SIGKILL),
            SignalOutcome::Sent
        );
        child.wait().unwrap();
    }

    #[test]
    fn a_process_that_is_not_a_group_leader_is_refused() {
        // This test process is not the leader of its own group when run by the harness.
        let me = std::process::id();
        let id = ProcessIdentity::new(me, start_token(me).unwrap());
        if proc_info(me).map(|i| i.pgid) != Some(me) {
            assert!(matches!(
                signal_verified(&id, Signal::SIGCONT),
                SignalOutcome::Refused(_)
            ));
        }
    }

    #[test]
    fn signalling_a_gone_process_reports_gone() {
        let mut child = spawn_sleeper(false);
        let id = ProcessIdentity::new(child.id(), start_token(child.id()).unwrap());
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(signal_verified(&id, Signal::SIGTERM), SignalOutcome::Gone);
    }

    #[test]
    fn pids_zero_and_one_are_never_signalled() {
        let init = ProcessIdentity::new(1, start_token(1).unwrap_or_else(|| "x".into()));
        assert!(!matches!(
            signal_verified(&init, Signal::SIGTERM),
            SignalOutcome::Sent
        ));
        let zero = ProcessIdentity::new(0, "x");
        assert_eq!(signal_verified(&zero, Signal::SIGTERM), SignalOutcome::Gone);
        assert!(!group_exists(0) && !group_exists(1));
    }

    #[test]
    fn terminate_escalates_to_kill_for_a_process_that_ignores_term() {
        let mut child = spawn_sleeper(true);
        // wait until the shell has installed its trap
        std::thread::sleep(Duration::from_millis(200));
        let id = ProcessIdentity::new(child.id(), start_token(child.id()).unwrap());
        let started = Instant::now();
        assert_eq!(terminate_group_blocking(&id, 300), SignalOutcome::Sent);
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "TERM was given its grace period"
        );
        let status = child.wait().unwrap();
        assert!(!status.success());
    }

    #[test]
    fn terminate_refuses_a_reused_pid() {
        let mut child = spawn_sleeper(false);
        let id = ProcessIdentity::new(child.id(), "1");
        assert!(matches!(
            terminate_group_blocking(&id, 100),
            SignalOutcome::Refused(_)
        ));
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
