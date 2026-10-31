//! The origin rule with real processes: the daemon runs inside this test process, so every child
//! of this process is "something the daemon started". None of them is listed by the job tracker
//! and none shares a group with a job, which is what makes the ancestry rule (and only it)
//! responsible for refusing them. The child is this very test binary, re-run on one function.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use fleet_domain::error::ErrorCode;
use fleet_domain::value::Value;
use fleet_infra::client::Client;
use fleet_infra::daemon::{start_daemon, DaemonOptions};
use fleet_infra::testutil::TempDir;
use fleet_infra::web::WebOptions;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::Duration;

const SOCKET_VAR: &str = "FLEET_ORIGIN_TEST_SOCKET";
const WEB_VAR: &str = "FLEET_ORIGIN_TEST_WEB";
const MARKER: &str = "ORIGIN-REFUSED";

/// What a child does when it is started by the tests below: it plays a hostile client and prints
/// `ORIGIN-REFUSED` only if it was refused everywhere. In a normal test run (no variables) it
/// does nothing and passes.
#[test]
fn origin_test_child() {
    let (Ok(socket), Ok(web)) = (std::env::var(SOCKET_VAR), std::env::var(WEB_VAR)) else {
        return;
    };
    // the control socket: refused at accept, before the token is even looked at
    let refused_on_socket = match Client::connect(std::path::Path::new(&socket)) {
        Ok(mut c) => matches!(
            c.call("status", Value::Null),
            Err(e) if e.code == ErrorCode::Forbidden
        ),
        Err(e) => e.code == ErrorCode::Forbidden,
    };
    // the dashboard: the token is right, the Host is right
    let (addr, token) = web.split_once('@').expect("addr@token");
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "GET /api/status HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut reply = String::new();
    let _ = s.read_to_string(&mut reply);
    let refused_on_web = reply.starts_with("HTTP/1.1 403") && reply.contains("worker processes");
    if refused_on_socket && refused_on_web {
        println!("{MARKER}");
    } else {
        println!("NOT-REFUSED socket={refused_on_socket} web={refused_on_web} reply={reply:?}");
    }
}

fn run_child(socket: &std::path::Path, addr: &str, token: &str, via_shell: bool) -> String {
    let exe = std::env::current_exe().unwrap();
    let mut cmd = if via_shell {
        // a grandchild: the test binary started by a shell that this process started
        let mut c = Command::new("/bin/sh");
        c.arg("-c")
            .arg("\"$0\" --exact origin_test_child --nocapture; exit 0")
            .arg(&exe);
        c
    } else {
        let mut c = Command::new(&exe);
        c.args(["--exact", "origin_test_child", "--nocapture"]);
        c
    };
    let out = cmd
        .env(SOCKET_VAR, socket)
        .env(WEB_VAR, format!("{addr}@{token}"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn processes_started_by_the_daemon_process_are_refused_on_both_interfaces_though_nobody_listed_them(
) {
    let dir = TempDir::new("origin");
    let handle = start_daemon(DaemonOptions {
        state_dir: dir.join("s"),
        config_path: None,
        web: Some(WebOptions::new("127.0.0.1:0".parse().unwrap(), false)),
    })
    .unwrap();
    let web = handle.web().expect("the dashboard is on").clone();
    let addr = web.addr.to_string();

    // the control: this process (the daemon itself, as far as the origin check can tell) is served
    let mut me = Client::connect(handle.socket()).unwrap();
    assert!(me.call("status", Value::Null).is_ok());

    for via_shell in [false, true] {
        let out = run_child(handle.socket(), &addr, &web.token, via_shell);
        assert!(
            out.contains(MARKER),
            "a child (via shell: {via_shell}) was not refused everywhere:\n{out}"
        );
    }
    handle.stop();
    handle.wait();
}
