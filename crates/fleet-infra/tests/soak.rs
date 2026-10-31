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

    daemon.stop();
    daemon.wait();
}
