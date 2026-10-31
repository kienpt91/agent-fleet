//! Running the small read-only helper programs (`ps`, `lsof`) that some platforms need to look at
//! the process and socket tables, under a timeout. Kept apart from `procid` so that the web
//! module, which may not import `procid`, can use the same bounded runner.

use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// The longest a helper program (`ps`, `lsof`) may take. They read kernel tables and answer in
/// milliseconds; one that hangs must not stall a job's supervision loop or a web request.
pub const TOOL_TIMEOUT: Duration = Duration::from_secs(2);

/// Runs `command` with no stdin, captures its output and kills it when it takes longer than
/// `limit`. `None` when it could not be started, did not finish in time or exited unsuccessfully.
/// At most 8 MiB of standard output are kept (a process table of a busy machine is a few hundred
/// kilobytes); stderr is discarded.
#[cfg_attr(all(target_os = "linux", not(test)), allow(dead_code))]
pub(crate) fn output_within(command: &mut Command, limit: Duration) -> Option<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = stdout.by_ref().take(8 << 20).read_to_end(&mut buf);
        let _ = send.send(buf);
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    // The reader ends when the pipe closes: with the child, or once it was killed. A process the
    // tool left behind that still holds the pipe is not waited for beyond the deadline.
    let status = status?;
    let stdout = receive
        .recv_timeout(
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(50)),
        )
        .ok()?;
    status.success().then_some(Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_helper_that_hangs_is_killed_at_the_deadline_and_one_that_answers_is_read() {
        let started = Instant::now();
        let hung = output_within(
            Command::new("/bin/sleep").arg("30"),
            Duration::from_millis(150),
        );
        assert!(hung.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited {:?}",
            started.elapsed()
        );
        let out = output_within(
            Command::new("/bin/echo").arg("pid ppid"),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "pid ppid");
        assert!(output_within(&mut Command::new("/bin/false"), Duration::from_secs(5)).is_none());
        assert!(output_within(
            &mut Command::new("/nonexistent/tool"),
            Duration::from_secs(1)
        )
        .is_none());
        // a helper that keeps the pipe open through a child it leaves behind does not block us
        let started = Instant::now();
        let held = output_within(
            Command::new("/bin/sh").args(["-c", "sleep 30 & echo x"]),
            Duration::from_millis(300),
        );
        assert!(held.is_none(), "the pipe is still held by the sleeper");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
