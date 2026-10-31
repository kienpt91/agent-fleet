//! `fleet attach`: connects the user's terminal to a worker's pty session.
//!
//! The terminal is put in raw mode while attached (so the agent's own UI gets every key) and is
//! always reset afterwards, because a full-screen agent may die while the terminal is in the
//! alternate screen, with the cursor hidden or mouse reporting on. Ctrl-] detaches.

use std::io::{IsTerminal, Write};
use std::os::fd::AsFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use fleet_domain::error::{FleetError, Result};
use fleet_domain::sanitize::sanitize_text;
use fleet_infra::client::{AttachEvent, AttachReader, AttachWriter, Client};
use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

/// Ctrl-] (GS). Rarely used by agents or shells, and what telnet users already know.
pub const DETACH_KEY: u8 = 0x1d;
const STDIN_CHUNK: usize = 1024;
/// Written on exit so a TUI agent cannot leave the terminal in a state the passthrough sanitizer
/// let it set: alternate screen (47, 1047, 1049), hidden cursor (25), autowrap off (7), cursor
/// keys, blinking, mouse reporting and focus events (1, 12, 1000, 1002, 1003, 1004, 1006),
/// bracketed paste (2004) and synchronized updates (2026). Modes 7 and 25 are on by default, so
/// they are set; every other mode is switched off. The test below keeps this in step with the
/// sanitizer's list.
const TERMINAL_RESET: &str = concat!(
    "\x1b[0m",
    "\x1b[?25h\x1b[?7h",
    "\x1b[?1l\x1b[?12l",
    "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l",
    "\x1b[?2004l\x1b[?2026l",
    "\x1b[?47l\x1b[?1047l\x1b[?1048l\x1b[?1049l",
);
const RESIZE_POLL: Duration = Duration::from_millis(250);
const STOP_POLL: Duration = Duration::from_millis(50);

/// Splits one stdin chunk at the detach key: the bytes before it are forwarded to the session,
/// the key and everything after it are dropped. The scan is byte-wise on purpose: Ctrl-] inside
/// an escape sequence is still the detach key, so a worker cannot be made to swallow it.
pub fn scan_input(chunk: &[u8]) -> (Vec<u8>, bool) {
    match chunk.iter().position(|b| *b == DETACH_KEY) {
        Some(i) => {
            let (before, _) = chunk.split_at(i);
            (before.to_vec(), true)
        }
        None => (chunk.to_vec(), false),
    }
}

/// Raw mode for the lifetime of the guard; the reset runs on every exit path including unwinding.
struct RawGuard;

impl RawGuard {
    fn enter() -> Result<RawGuard> {
        crossterm::terminal::enable_raw_mode()
            .map_err(|e| FleetError::io("cannot enable raw terminal mode", &e))?;
        Ok(RawGuard)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let mut out = std::io::stdout();
        let _ = out.write_all(TERMINAL_RESET.as_bytes());
        let _ = out.flush();
    }
}

type SharedWriter = Arc<Mutex<AttachWriter>>;

fn lock(writer: &SharedWriter) -> MutexGuard<'_, AttachWriter> {
    writer.lock().unwrap_or_else(PoisonError::into_inner)
}

fn stdin_pump(writer: SharedWriter, stop: Arc<AtomicBool>, detached: Arc<AtomicBool>, tty: bool) {
    let stdin = std::io::stdin();
    let mut buf = [0u8; STDIN_CHUNK];
    while !stop.load(Ordering::Relaxed) {
        // Poll with a timeout so the thread notices `stop` instead of blocking in read forever
        // and stealing a keystroke from whatever owns the terminal next.
        let mut fds = [PollFd::new(
            stdin.as_fd(),
            PollFlags::POLLIN | PollFlags::POLLHUP,
        )];
        match poll(&mut fds, PollTimeout::from(100u16)) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(Errno::EINTR) => continue,
            Err(_) => break,
        }
        let ready = fds
            .first()
            .and_then(PollFd::revents)
            .is_some_and(|r| !r.is_empty());
        if !ready {
            continue;
        }
        let n = match nix::unistd::read(&stdin, &mut buf) {
            Ok(0) => {
                // A closed pipe only ends input; the session keeps running until it exits. A
                // terminal that reports EOF is gone, so detach.
                if tty {
                    let _ = lock(&writer).detach();
                    detached.store(true, Ordering::Relaxed);
                }
                break;
            }
            Ok(n) => n,
            Err(Errno::EINTR) => continue,
            Err(_) => break,
        };
        let (forward, detach) = scan_input(buf.get(..n).unwrap_or(&[]));
        let mut w = lock(&writer);
        if !forward.is_empty() && w.input(&forward).is_err() {
            break;
        }
        if detach {
            let _ = w.detach();
            detached.store(true, Ordering::Relaxed);
            break;
        }
    }
}

fn resize_pump(writer: SharedWriter, stop: Arc<AtomicBool>, initial: (u16, u16)) {
    let mut last = initial;
    let ticks = RESIZE_POLL.as_millis() / STOP_POLL.as_millis();
    'outer: loop {
        for _ in 0..ticks {
            if stop.load(Ordering::Relaxed) {
                break 'outer;
            }
            thread::sleep(STOP_POLL);
        }
        let Ok(size) = crossterm::terminal::size() else {
            continue;
        };
        if size != last {
            last = size;
            if lock(&writer).resize(size.0, size.1).is_err() {
                break;
            }
        }
    }
}

/// Copies session output to the terminal. Returns the exit text when the session ended.
fn pump_output(
    reader: &mut AttachReader,
    raw: bool,
    detached: &AtomicBool,
) -> Result<Option<String>> {
    let mut stdout = std::io::stdout();
    loop {
        let event = match reader.next_event() {
            Ok(Some(e)) => e,
            Ok(None) => return Ok(None),
            // The server closes the connection after a detach; that is not a failure.
            Err(_) if detached.load(Ordering::Relaxed) => return Ok(None),
            Err(e) => return Err(e),
        };
        match event {
            AttachEvent::Output(text) => {
                stdout
                    .write_all(text.as_bytes())
                    .and_then(|()| stdout.flush())
                    .map_err(|e| FleetError::io("cannot write to the terminal", &e))?;
            }
            AttachEvent::Notice(text) => {
                let eol = if raw { "\r\n" } else { "\n" };
                let line = format!("[fleet] {}{eol}", sanitize_text(text.as_bytes()).trim());
                let _ = std::io::stderr().write_all(line.as_bytes());
            }
            AttachEvent::Exit(text) => return Ok(Some(text)),
        }
    }
}

fn join(handle: Option<JoinHandle<()>>) {
    if let Some(h) = handle {
        let _ = h.join();
    }
}

/// Attaches to `task` until it detaches (Ctrl-]) or the session ends. Returns the exit code.
pub fn run_attach(socket: &Path, task: &str) -> Result<i32> {
    let client = Client::connect(socket)?;
    let interactive = std::io::stdin().is_terminal();
    let size = crossterm::terminal::size().unwrap_or((80, 24));
    let stream = client.attach(task, size.0, size.1)?;
    let (mut reader, writer) = stream.split();
    let guard = if interactive {
        Some(RawGuard::enter()?)
    } else {
        None
    };

    let writer: SharedWriter = Arc::new(Mutex::new(writer));
    let stop = Arc::new(AtomicBool::new(false));
    let detached = Arc::new(AtomicBool::new(false));

    let spawn = |name: &str, f: Box<dyn FnOnce() + Send>| {
        thread::Builder::new()
            .name(name.to_string())
            .spawn(f)
            .map_err(|e| FleetError::io("cannot start attach thread", &e))
    };
    let stdin_handle = {
        let (w, s, d) = (writer.clone(), stop.clone(), detached.clone());
        spawn(
            "attach-stdin",
            Box::new(move || stdin_pump(w, s, d, interactive)),
        )
    };
    let resize_handle = if interactive {
        let (w, s) = (writer.clone(), stop.clone());
        Some(spawn(
            "attach-resize",
            Box::new(move || resize_pump(w, s, size)),
        ))
    } else {
        None
    };

    let outcome = match (&stdin_handle, &resize_handle) {
        (Err(e), _) | (_, Some(Err(e))) => Err(e.clone()),
        _ => pump_output(&mut reader, interactive, &detached),
    };
    stop.store(true, Ordering::Relaxed);
    join(stdin_handle.ok());
    join(resize_handle.and_then(|r| r.ok()));
    drop(guard);

    let exit = outcome?;
    if let Some(text) = exit {
        let line = sanitize_text(text.as_bytes());
        let _ = writeln!(std::io::stderr(), "[fleet] {}", line.trim());
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_detach_key_forwards_everything() {
        assert_eq!(scan_input(b"hello\r"), (b"hello\r".to_vec(), false));
        assert_eq!(scan_input(b""), (Vec::new(), false));
    }

    #[test]
    fn detach_in_the_middle_forwards_only_the_prefix() {
        assert_eq!(scan_input(b"ab\x1dcd"), (b"ab".to_vec(), true));
    }

    #[test]
    fn detach_at_the_start_forwards_nothing() {
        assert_eq!(scan_input(b"\x1drm -rf"), (Vec::new(), true));
        assert_eq!(scan_input(b"\x1d"), (Vec::new(), true));
    }

    #[test]
    fn chunks_are_scanned_independently() {
        let first = scan_input(b"abc");
        let second = scan_input(b"de\x1d");
        let third = scan_input(b"never sent");
        assert_eq!(first, (b"abc".to_vec(), false));
        assert_eq!(second, (b"de".to_vec(), true));
        // The caller stops after the first detach, but the function itself stays pure.
        assert_eq!(third, (b"never sent".to_vec(), false));
    }

    #[test]
    fn detach_key_inside_an_escape_sequence_still_detaches() {
        assert_eq!(scan_input(b"\x1b[1;\x1d5A"), (b"\x1b[1;".to_vec(), true));
        assert_eq!(scan_input(b"\x1b\x1d"), (b"\x1b".to_vec(), true));
    }

    #[test]
    fn only_the_first_detach_key_counts() {
        assert_eq!(scan_input(b"a\x1db\x1dc"), (b"a".to_vec(), true));
    }

    #[test]
    fn utf8_text_around_the_key_is_preserved() {
        let (fwd, detach) = scan_input("héllo\u{1d}x".as_bytes());
        assert!(detach);
        assert_eq!(String::from_utf8(fwd).ok().as_deref(), Some("héllo"));
    }

    /// Private modes a worker's output may set or reset on the attached terminal: found by
    /// asking the sanitizer the attach path really uses, so a mode added there without a reset
    /// here fails this test.
    fn modes_passed_through() -> Vec<u32> {
        (0..=4000u32)
            .filter(|n| {
                ["h", "l"].iter().all(|f| {
                    let mut s = fleet_domain::sanitize::Sanitizer::new(
                        fleet_domain::sanitize::Mode::Passthrough,
                    );
                    let mut out = String::new();
                    let seq = format!("\x1b[?{n}{f}");
                    s.feed(seq.as_bytes(), &mut out);
                    s.finish(&mut out);
                    out == seq
                })
            })
            .collect()
    }

    #[test]
    fn the_passthrough_sanitizer_allows_exactly_the_modes_the_reset_knows() {
        assert_eq!(
            modes_passed_through(),
            vec![1, 7, 12, 25, 47, 1000, 1002, 1003, 1004, 1006, 1047, 1048, 1049, 2004, 2026]
        );
    }

    #[test]
    fn the_terminal_reset_undoes_every_mode_a_worker_can_set() {
        for mode in modes_passed_through() {
            // Autowrap (7) and the cursor (25) are on by default and must be switched back on;
            // everything else is off by default.
            let default_on = matches!(mode, 7 | 25);
            let wanted = format!("\x1b[?{mode}{}", if default_on { 'h' } else { 'l' });
            let opposite = format!("\x1b[?{mode}{}", if default_on { 'l' } else { 'h' });
            assert!(
                TERMINAL_RESET.contains(&wanted),
                "mode {mode}: no {wanted:?}"
            );
            assert!(
                !TERMINAL_RESET.contains(&opposite),
                "mode {mode}: the reset must not also contain {opposite:?}"
            );
        }
        assert!(
            TERMINAL_RESET.starts_with("\x1b[0m"),
            "attributes are reset too"
        );
    }
}
