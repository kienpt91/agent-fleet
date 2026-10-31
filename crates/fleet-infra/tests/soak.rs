//! Memory soak: several workers print as fast as the terminal allows for a while. The daemon runs
//! in this test process, so the process's own resident memory is the daemon's.
//!
//! The run length is `FLEET_SOAK_SECS` (default 20 seconds). The README numbers come from a longer
//! run of this test in release mode:
//! `FLEET_SOAK_SECS=120 cargo test --release -p fleet-infra --test soak -- --nocapture`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_domain::value::Value;
use fleet_infra::client::Client;
use fleet_infra::daemon::{start_daemon, DaemonOptions};
use fleet_infra::memory::rss_kb;
use fleet_infra::testutil::{make_repo, TempDir};
use fleet_infra::web::WebOptions;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const WORKERS: usize = 8;
/// Output ring per session, deliberately small so the cap is exercised hard.
const RING_BYTES: usize = 64 * 1024;

fn soak_secs() -> u64 {
    std::env::var("FLEET_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n >= 5)
        .unwrap_or(20)
}

// ---- the connected dashboard clients ----------------------------------------------------------

/// What the SSE client saw. Atomics only: the client keeps no output, just counters.
#[derive(Default)]
struct SseStats {
    bytes: AtomicU64,
    /// `output` events received, per third of the run.
    events: [AtomicU64; 3],
    reconnects: AtomicU64,
    /// Connection attempts that did not get a 200.
    refused: AtomicU64,
    /// Streams open at once; the client holds at most one.
    open_now: AtomicU64,
    max_open: AtomicU64,
    last_id: AtomicU64,
}

impl SseStats {
    fn total_events(&self) -> u64 {
        self.events.iter().map(|e| e.load(Ordering::Relaxed)).sum()
    }
}

fn web_connect(addr: SocketAddr) -> std::io::Result<TcpStream> {
    let s = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    s.set_read_timeout(Some(Duration::from_millis(200)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(s)
}

fn get_request(addr: SocketAddr, token: &str, path: &str, extra: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\n{extra}Connection: close\r\n\r\n"
    )
}

/// One request, whole response (bounded), returns the status code.
fn web_get(addr: SocketAddr, token: &str, path: &str) -> Option<u16> {
    let mut s = web_connect(addr).ok()?;
    s.write_all(get_request(addr, token, path, "").as_bytes())
        .ok()?;
    let mut head = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut total = 0usize;
    let give_up = Instant::now() + Duration::from_secs(10);
    while total < (4 << 20) && Instant::now() < give_up {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                if head.len() < 32 {
                    head.extend_from_slice(&chunk[..n.min(32)]);
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
    }
    let line = String::from_utf8_lossy(&head).into_owned();
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Streams once. Returns after `event: limit`/`end`, a server close, or when `stop` is set.
/// Lines are parsed as they arrive; only the first bytes of a line are kept.
fn sse_once(
    addr: SocketAddr,
    token: &str,
    task: &str,
    stats: &SseStats,
    started: Instant,
    secs: u64,
    stop: &AtomicBool,
) {
    let Ok(mut s) = web_connect(addr) else {
        stats.refused.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let last = stats.last_id.load(Ordering::Relaxed);
    let extra = if last > 0 {
        format!("Last-Event-ID: {last}\r\nAccept: text/event-stream\r\n")
    } else {
        "Accept: text/event-stream\r\n".to_string()
    };
    let path = format!("/api/stream?task={task}");
    if s.write_all(get_request(addr, token, &path, &extra).as_bytes())
        .is_err()
    {
        stats.refused.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let now = stats.open_now.fetch_add(1, Ordering::Relaxed) + 1;
    stats.max_open.fetch_max(now, Ordering::Relaxed);
    let mut in_head = true;
    let mut head_tail = [0u8; 4];
    let mut head_seen = 0usize;
    let mut status_line: Vec<u8> = Vec::new();
    let mut line_head: Vec<u8> = Vec::with_capacity(32);
    let mut in_line = false;
    let mut event_name: [u8; 8] = [0; 8];
    let mut event_len = 0usize;
    let mut chunk = [0u8; 16 * 1024];
    'outer: while !stop.load(Ordering::Relaxed) {
        let n = match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(_) => break,
        };
        stats.bytes.fetch_add(n as u64, Ordering::Relaxed);
        for &b in &chunk[..n] {
            if in_head {
                if status_line.len() < 64 && head_seen < 64 {
                    status_line.push(b);
                }
                head_seen += 1;
                head_tail.rotate_left(1);
                head_tail[3] = b;
                if head_tail == *b"\r\n\r\n" {
                    in_head = false;
                    let first = String::from_utf8_lossy(&status_line).into_owned();
                    if first.split_whitespace().nth(1) != Some("200") {
                        stats.refused.fetch_add(1, Ordering::Relaxed);
                        break 'outer;
                    }
                } else if head_seen > 8192 {
                    break 'outer;
                }
                continue;
            }
            if b == b'\n' {
                if line_head.starts_with(b"event: ") {
                    let name = &line_head[7..];
                    event_len = name.len().min(8);
                    event_name[..event_len].copy_from_slice(&name[..event_len]);
                    if &event_name[..event_len] == b"output" {
                        let third = ((started.elapsed().as_secs() * 3) / secs.max(1)).min(2);
                        stats.events[third as usize].fetch_add(1, Ordering::Relaxed);
                    }
                } else if line_head.starts_with(b"id: ") {
                    if let Some(id) = std::str::from_utf8(&line_head[4..])
                        .ok()
                        .and_then(|t| t.parse::<u64>().ok())
                    {
                        stats.last_id.store(id, Ordering::Relaxed);
                    }
                } else if line_head.is_empty() && in_line {
                    // blank line: the event is complete
                    if matches!(&event_name[..event_len], b"limit" | b"end") {
                        break 'outer;
                    }
                    event_len = 0;
                }
                line_head.clear();
                in_line = true;
            } else if line_head.len() < 32 && b != b'\r' {
                line_head.push(b);
            }
        }
    }
    stats.open_now.fetch_sub(1, Ordering::Relaxed);
}

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    Value::object(pairs)
}

#[test]
fn memory_stays_flat_while_chatty_workers_flood_their_terminals() {
    let secs = soak_secs();
    let dir = TempDir::new("soak");
    let repo = make_repo(&dir);
    // The flood contains escape sequences, a bidi override and a token-shaped string, so the
    // sanitizer and the redactor run on every byte as well.
    let token = format!("ghp_{}", "a1B2".repeat(10));
    dir.script(
        "flood.sh",
        &format!(
            "#!/bin/sh\ntimeout \"$FLOOD_SECS\" yes \"$(printf 'chatty \\033[31mred\\033[0m \\033]52;c;AAAA\\007 \\342\\200\\256 {token} 0123456789 abcdefghijklmnopqrstuvwxyz')\" || true\nprintf '{{\"version\":1,\"status\":\"done\",\"summary\":\"flooded\"}}' > .fleet/result.json\n"
        ),
    );
    let config = dir.write(
        "fleet.toml",
        &format!(
            "[daemon]\nmax_sessions = {WORKERS}\ndefault_worker = \"flood\"\nmax_retries = 0\nring_bytes = {RING_BYTES}\napproval_ttl_secs = 600\n\n[env]\nallow = [\"PATH\", \"HOME\", \"LANG\", \"TERM\"]\n\n[workers.flood]\ncommand = \"{}\"\ntimeout_secs = {}\nenv_set = {{ FLOOD_SECS = \"{secs}\" }}\n",
            dir.join("flood.sh").display(),
            secs + 60
        ),
    );
    let state = dir.join("s");
    let daemon = start_daemon(DaemonOptions {
        state_dir: state,
        config_path: Some(config),
        web: Some(WebOptions::new("127.0.0.1:0".parse().unwrap(), false)),
    })
    .unwrap();
    let mut client = Client::connect(daemon.socket()).unwrap();
    client
        .call(
            "run.create",
            obj(vec![
                ("objective", Value::str("soak")),
                ("repo", Value::str(repo.to_string_lossy())),
                ("max_parallel", Value::int(WORKERS as u64)),
            ]),
        )
        .unwrap();
    for i in 0..WORKERS {
        client
            .call(
                "task.add",
                obj(vec![
                    ("run", Value::str("r1")),
                    ("id", Value::str(format!("w{i}"))),
                    ("title", Value::str(format!("flood {i}"))),
                ]),
            )
            .unwrap();
    }
    client
        .call("run.start", obj(vec![("run", Value::str("r1"))]))
        .unwrap();

    let started = Instant::now();
    let web = daemon.web().expect("web dashboard enabled").clone();
    let max_connections = fleet_infra::web::WebLimits::default().max_connections;
    let stop_clients = Arc::new(AtomicBool::new(false));
    let sse = Arc::new(SseStats::default());
    let sse_thread = {
        let (stats, stop, token) = (
            Arc::clone(&sse),
            Arc::clone(&stop_clients),
            web.token.clone(),
        );
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                sse_once(web.addr, &token, "w0", &stats, started, secs, &stop);
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                stats.reconnects.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    };
    let polls = Arc::new(AtomicU64::new(0));
    let poll_failures = Arc::new(AtomicU64::new(0));
    let poll_thread = {
        let (stop, token) = (Arc::clone(&stop_clients), web.token.clone());
        let (polls, failures) = (Arc::clone(&polls), Arc::clone(&poll_failures));
        let addr = web.addr;
        std::thread::spawn(move || {
            let mut health_done = false;
            while !stop.load(Ordering::Relaxed) {
                let ok = web_get(addr, &token, "/api/status") == Some(200);
                polls.fetch_add(1, Ordering::Relaxed);
                if !ok {
                    failures.fetch_add(1, Ordering::Relaxed);
                }
                if !health_done {
                    health_done = true;
                    if web_get(addr, &token, "/api/health") != Some(200) {
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
                let wake = Instant::now() + Duration::from_millis(500);
                while Instant::now() < wake && !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        })
    };
    let mut samples: Vec<(u64, u64)> = Vec::new(); // (seconds since start, rss kB)
    let mut max_tail = 0usize;
    let deadline = Instant::now() + Duration::from_secs(secs + 90);
    let mut finished = false;
    let mut output_bytes = 0u64;
    while Instant::now() < deadline {
        if let Some(kb) = rss_kb() {
            samples.push((started.elapsed().as_secs(), kb));
        }
        let status = client.call("status", Value::Null).unwrap();
        output_bytes = status
            .get("daemon")
            .and_then(|d| d.get("output_bytes"))
            .and_then(Value::as_u64)
            .unwrap_or(output_bytes);
        let tasks: Vec<Value> = status
            .get("runs")
            .and_then(Value::as_array)
            .and_then(|r| r.first())
            .and_then(|r| r.get("tasks"))
            .and_then(Value::as_array)
            .map(<[Value]>::to_vec)
            .unwrap_or_default();
        if tasks
            .iter()
            .all(|t| t.get("status").and_then(Value::as_str) == Some("completed"))
            && !tasks.is_empty()
        {
            finished = true;
            break;
        }
        // the bounded tail of a live worker never exceeds its ring
        if let Ok(tail) = client.call(
            "task.tail",
            obj(vec![
                ("task", Value::str("w0")),
                ("max", Value::int(1u64 << 20)),
            ]),
        ) {
            let len = tail.get("text").and_then(Value::as_str).map_or(0, str::len);
            max_tail = max_tail.max(len);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    stop_clients.store(true, Ordering::Relaxed);
    sse_thread.join().unwrap();
    poll_thread.join().unwrap();
    assert!(finished, "workers did not finish; samples: {samples:?}");
    let partial_line = fleet_domain::limits::MAX_PARTIAL_LINE;
    assert!(
        max_tail <= RING_BYTES + partial_line,
        "tail of {max_tail} bytes exceeds the {RING_BYTES} byte ring plus one partial line"
    );

    // Stored output never contains the flood's escape sequences or the secret.
    let log = std::fs::read_dir(dir.join("s/logs/r1"))
        .unwrap()
        .filter_map(Result::ok)
        .next()
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .unwrap_or_default();
    assert!(
        !log.contains('\u{1b}') && !log.contains('\u{202e}') && !log.contains(&token),
        "stored output was not sanitized"
    );
    assert!(log.contains("[REDACTED:github-token]"));

    // Flatness: after the first fifth of the run (buffers are full by then) memory must not grow.
    let cutoff = secs / 5;
    let steady: Vec<u64> = samples
        .iter()
        .filter(|(t, _)| *t >= cutoff.max(3))
        .map(|(_, kb)| *kb)
        .collect();
    assert!(steady.len() >= 4, "too few samples: {samples:?}");
    let first = steady.first().copied().unwrap();
    let max = steady.iter().copied().max().unwrap();
    let min = steady.iter().copied().min().unwrap();
    let baseline = samples.first().map(|(_, kb)| *kb).unwrap_or(0);
    let mib = output_bytes as f64 / (1024.0 * 1024.0);
    eprintln!(
        "soak: {WORKERS} workers x {secs}s, {mib:.0} MiB of terminal output ({:.0} MiB/s), rss start {baseline} kB, steady min {min} kB / max {max} kB / first {first} kB, growth after warm-up {} kB, {} samples",
        mib / secs as f64,
        max.saturating_sub(first),
        samples.len()
    );
    assert!(
        max.saturating_sub(first) <= 4 * 1024,
        "memory grew by {} kB after warm-up: {samples:?}",
        max.saturating_sub(first)
    );
    assert!(max < 256 * 1024, "absolute memory {max} kB is too high");
    assert!(
        output_bytes > 10 * 1024 * 1024,
        "the flood produced only {output_bytes} bytes"
    );

    // The dashboard clients were served throughout.
    let per_third: Vec<u64> = sse
        .events
        .iter()
        .map(|e| e.load(Ordering::Relaxed))
        .collect();
    let sse_bytes = sse.bytes.load(Ordering::Relaxed);
    eprintln!(
        "soak web: sse client {sse_bytes} bytes, {} output events (per third {per_third:?}), {} reconnects, {} refused, max {} open stream(s); {} status polls ({} failed); rss start {baseline} kB, steady min {min} kB / max {max} kB, growth after warm-up {} kB",
        sse.total_events(),
        sse.reconnects.load(Ordering::Relaxed),
        sse.refused.load(Ordering::Relaxed),
        sse.max_open.load(Ordering::Relaxed),
        polls.load(Ordering::Relaxed),
        poll_failures.load(Ordering::Relaxed),
        max.saturating_sub(first)
    );
    assert!(sse_bytes > 0, "the SSE client received nothing");
    for (i, n) in per_third.iter().enumerate() {
        assert!(
            *n > 0,
            "no output event in third {i} of the run: {per_third:?}"
        );
    }
    assert_eq!(
        poll_failures.load(Ordering::Relaxed),
        0,
        "dashboard polls failed"
    );
    assert!(polls.load(Ordering::Relaxed) > 0);
    assert!(
        sse.max_open.load(Ordering::Relaxed) as usize <= max_connections,
        "the client held more connections than the server allows"
    );
    // The server still answers a new request after the flood.
    assert_eq!(web_get(web.addr, &web.token, "/api/status"), Some(200));

    daemon.stop();
    daemon.wait();
    // The web threads have ended: nothing listens any more.
    assert!(
        TcpStream::connect_timeout(&web.addr, Duration::from_secs(2)).is_err(),
        "the web listener outlived the daemon"
    );
}
