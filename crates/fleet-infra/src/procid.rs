//! Reading and signalling processes, with protection against pid reuse.
//!
//! A pid alone does not identify a process: after the original exits the number can belong to an
//! unrelated one. Every signal sent on behalf of an earlier reading is therefore preceded by a
//! comparison of the process's start token with the one recorded when the worker was started
//! (domain rule: `ProcessIdentity::is_same_process`), and only a process group leader is signalled
//! as a group.

#[cfg(not(target_os = "linux"))]
use crate::toolrun::{output_within, TOOL_TIMEOUT};
use fleet_app::ports::SignalOutcome;
use fleet_domain::supervision::{Lineage, ProcInfo, ProcessIdentity};
use nix::errno::Errno;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
#[cfg(not(target_os = "linux"))]
use std::process::Command;
use std::time::{Duration, Instant};

/// Longest ancestry chain followed when looking for a worker ancestor. A longer one is not a
/// complete lineage (see [`Lineage`]) and is refused by the origin check.
const MAX_ANCESTRY: usize = 64;

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
    let out = output_within(
        Command::new("/bin/ps")
            .args(["-o", columns, "-p", &pid.to_string()])
            .env_clear()
            .env("LC_ALL", "C"),
        TOOL_TIMEOUT,
    )?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let line = text.lines().next()?.trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// Reads the kernel's boot id: a UUID that changes with every boot.
#[cfg(target_os = "linux")]
fn read_boot_id() -> Option<String> {
    let text = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    parse_boot_id(&text)
}

/// Accepts the text of `boot_id`: a short hexadecimal UUID, possibly with dashes.
pub fn parse_boot_id(text: &str) -> Option<String> {
    let id = text.trim();
    let valid =
        !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    valid.then(|| id.to_string())
}

/// The start token of a Linux process: `<boot_id>:<start ticks>`. The tick count restarts from
/// zero at every boot, so without the boot id a process of an earlier boot with a recorded pid
/// and ticks could be mistaken for a new one that happens to match.
pub fn boot_scoped_token(boot_id: &str, start_ticks: &str) -> String {
    format!("{boot_id}:{start_ticks}")
}

/// A token that stays the same for the lifetime of one process and differs for any later process
/// that reuses the pid. `None` when no such process exists or it cannot be read. On Linux it is
/// scoped to the boot (`<boot_id>:<ticks>`); elsewhere it is the process's start time.
pub fn start_token(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let stat = read_stat(pid)?;
        Some(boot_scoped_token(&read_boot_id()?, &stat.start_ticks))
    }
    #[cfg(not(target_os = "linux"))]
    {
        ps(pid, "lstart=")
    }
}

/// Every process as `(pid, ppid)`, as far as the platform lets this user see them. `None` when
/// the table could not be read at all (an unreadable `/proc`, a `ps` that failed or hung): a
/// supervisor that gets nothing back is blind and must say so rather than see no descendants.
pub fn process_table() -> Option<Vec<(u32, u32)>> {
    #[cfg(target_os = "linux")]
    let table: Vec<(u32, u32)> = {
        let dir = std::fs::read_dir("/proc").ok()?;
        dir.flatten()
            .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()))
            .filter_map(|pid| read_stat(pid).map(|s| (pid, s.ppid)))
            .collect()
    };
    #[cfg(not(target_os = "linux"))]
    let table: Vec<(u32, u32)> = {
        let out = output_within(
            Command::new("/bin/ps")
                .args(["-A", "-o", "pid=,ppid="])
                .env_clear()
                .env("LC_ALL", "C"),
            TOOL_TIMEOUT,
        )?;
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
            })
            .collect()
    };
    // at least this process is always in a real table
    (!table.is_empty()).then_some(table)
}

/// The pids below `root` in a process table (children, their children, ...), not `root` itself.
pub fn descendants_in(table: &[(u32, u32)], root: u32) -> Vec<u32> {
    let mut found: Vec<u32> = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for (pid, ppid) in table {
            if *ppid == parent && *pid != root && !found.contains(pid) {
                found.push(*pid);
                frontier.push(*pid);
            }
        }
        if found.len() > MAX_DESCENDANTS {
            break;
        }
    }
    found
}

/// Most descendants of one job child that are followed.
const MAX_DESCENDANTS: usize = 4096;

/// SIGKILLs one process, but only if it is still the process that was recorded under `token`.
/// Returns true when a signal was sent.
pub fn kill_if_same(pid: u32, token: &str) -> bool {
    let id = ProcessIdentity::new(pid, token);
    if !id.is_same_process(start_token(pid).as_deref()) {
        return false;
    }
    match to_pid(pid) {
        Some(p) => nix::sys::signal::kill(p, Signal::SIGKILL).is_ok(),
        None => false,
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

/// The process and its ancestors, nearest first. The last element is pid 1 when the walk got
/// there (it is not read from the OS, only recorded: reading it is unnecessary and may be
/// forbidden where processes of other users are hidden). `complete` is true only when the walk
/// reached init or a parent outside this pid namespace; a walk that stopped at the cap or at an
/// ancestor that could not be read is not complete.
pub fn lineage(pid: u32) -> Lineage {
    let mut chain = Vec::new();
    if pid <= 1 {
        return Lineage {
            chain,
            complete: false,
        };
    }
    let mut cur = pid;
    while chain.len() < MAX_ANCESTRY {
        let Some(info) = proc_info(cur) else {
            return Lineage {
                chain,
                complete: false,
            };
        };
        let next = info.ppid;
        chain.push(info);
        if next == 0 || next == cur {
            return Lineage {
                chain,
                complete: true,
            };
        }
        if next == 1 {
            chain.push(ProcInfo {
                pid: 1,
                ppid: 0,
                pgid: 1,
            });
            return Lineage {
                chain,
                complete: true,
            };
        }
        cur = next;
    }
    Lineage {
        chain,
        complete: false,
    }
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
    #[test]
    fn descendants_follow_parent_links_and_skip_the_root_and_cycles() {
        let table = [
            (10, 1),
            (11, 10),
            (12, 11),
            (13, 10),
            (20, 1),
            (21, 20),
            (10, 10),
        ];
        let mut d = descendants_in(&table, 10);
        d.sort_unstable();
        assert_eq!(d, vec![11, 12, 13]);
        assert!(descendants_in(&table, 12).is_empty());
        assert!(descendants_in(&[], 10).is_empty());
        // a cycle that does not include the root terminates
        assert_eq!(descendants_in(&[(5, 6), (6, 5)], 7), Vec::<u32>::new());
    }

    #[test]
    fn the_process_table_lists_this_process_under_its_parent() {
        let me = std::process::id();
        let table = process_table().unwrap();
        let parent = table.iter().find(|(p, _)| *p == me).map(|(_, pp)| *pp);
        assert!(parent.is_some_and(|pp| pp != 0), "{parent:?}");
    }

    #[test]
    fn kill_if_same_needs_the_recorded_start_token() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let token = start_token(pid).unwrap();
        assert!(!kill_if_same(pid, "not-the-token"));
        assert!(child.try_wait().unwrap().is_none(), "still running");
        assert!(!kill_if_same(pid, ""));
        assert!(kill_if_same(pid, &token));
        let status = child.wait().unwrap();
        assert!(!status.success());
        // the process is gone: nothing is signalled any more
        assert!(!kill_if_same(pid, &token));
        assert!(!kill_if_same(0, "x") && !kill_if_same(1, "x"));
    }

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
    fn boot_ids_are_parsed_strictly() {
        let id = "3f2b8c1e-9a4d-4c55-8e0a-1b2c3d4e5f60\n";
        assert_eq!(
            parse_boot_id(id).as_deref(),
            Some("3f2b8c1e-9a4d-4c55-8e0a-1b2c3d4e5f60")
        );
        assert_eq!(parse_boot_id(""), None);
        assert_eq!(parse_boot_id("\n"), None);
        assert_eq!(parse_boot_id("not a uuid"), None);
        assert_eq!(
            parse_boot_id("abc:def"),
            None,
            "the separator must not appear in an id"
        );
        assert_eq!(parse_boot_id(&"a".repeat(65)), None);
    }

    #[test]
    fn the_token_differs_between_boots_with_equal_ticks() {
        let a = boot_scoped_token("aaaa-1111", "987654");
        let b = boot_scoped_token("bbbb-2222", "987654");
        assert_eq!(a, "aaaa-1111:987654");
        assert_ne!(a, b);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_linux_token_is_boot_id_colon_ticks_of_the_live_process() {
        let me = std::process::id();
        let token = start_token(me).unwrap();
        let boot = read_boot_id().unwrap();
        let ticks = read_stat(me).unwrap().start_ticks;
        assert_eq!(token, format!("{boot}:{ticks}"));
        let (left, right) = token.rsplit_once(':').unwrap();
        assert_eq!(left, boot);
        assert!(right.bytes().all(|b| b.is_ascii_digit()));
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
    fn lineage_starts_with_the_process_itself_and_is_bounded() {
        let Lineage { chain, .. } = lineage(std::process::id());
        assert_eq!(chain.first().map(|p| p.pid), Some(std::process::id()));
        assert!(chain.len() <= MAX_ANCESTRY);
        assert!(lineage(0).chain.is_empty() && !lineage(0).complete);
        assert!(lineage(1).chain.is_empty() && !lineage(1).complete);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_normal_lineage_is_complete_and_ends_at_init() {
        let l = lineage(std::process::id());
        assert!(l.complete, "{:?}", l.chain);
        let last = l.chain.last().unwrap();
        assert!(last.pid == 1 || last.ppid == 0, "{:?}", l.chain);
    }

    /// Starts `depth` nested subshells and returns the pid of a `sleep` at the bottom.
    #[cfg(target_os = "linux")]
    fn nested_sleeper(depth: u32) -> (std::process::Child, u32) {
        use std::io::{BufRead, BufReader};
        let script = format!(
            "f() {{ if [ \"$1\" -le 0 ]; then sleep 60 & echo $!; wait; else ( f $(($1 - 1)) ); fi; }}; f {depth}"
        );
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", &script])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let pid = line.trim().parse().unwrap();
        (child, pid)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_lineage_that_hits_the_cap_is_not_complete_and_a_short_one_is() {
        let (mut shallow, pid) = nested_sleeper(3);
        let l = lineage(pid);
        assert!(l.chain.len() >= 4, "{:?}", l.chain);
        assert!(l.complete, "{:?}", l.chain);
        assert!(
            l.chain.iter().any(|p| p.pid == std::process::id()),
            "this process is an ancestor"
        );
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), Signal::SIGKILL);
        let _ = shallow.kill();
        let _ = shallow.wait();

        let (mut deep, pid) = nested_sleeper(MAX_ANCESTRY as u32 + 6);
        let l = lineage(pid);
        assert_eq!(l.chain.len(), MAX_ANCESTRY);
        assert!(
            !l.complete,
            "a walk that was cut short must not look complete"
        );
        assert!(
            !l.chain.iter().any(|p| p.pid == std::process::id()),
            "the daemon (this process) is beyond the cap: exactly the case that must be refused"
        );
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), Signal::SIGKILL);
        let _ = deep.kill();
        let _ = deep.wait();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_lineage_that_cannot_be_read_is_not_complete() {
        // a pid that does not exist
        let l = lineage(4_194_000);
        assert!(l.chain.is_empty() && !l.complete);
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
