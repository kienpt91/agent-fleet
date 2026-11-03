//! Helpers for tests: temporary directories, a git repository factory and executable scripts.
//! Std only. Used by unit tests and by the integration tests of this crate and `fleet-cli`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

pub mod sshd;

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A directory under the system temp dir that is removed on drop.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "fleet-test-{label}-{}-{n}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        // Resolve symlinks (macOS /var -> /private/var) so paths compare equal to git's output.
        let path = fs::canonicalize(&path).unwrap_or(path);
        TempDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, rel: &str) -> PathBuf {
        self.path.join(rel)
    }

    pub fn write(&self, rel: &str, content: &str) -> PathBuf {
        let p = self.path.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(&p, content).expect("write file");
        p
    }

    /// Writes an executable script (`#!/bin/sh` is up to the caller).
    pub fn script(&self, rel: &str, content: &str) -> PathBuf {
        let p = self.write(rel, content);
        let mut perm = fs::metadata(&p).expect("stat").permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&p, perm).expect("chmod");
        p
    }

    pub fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.path.join(rel)).unwrap_or_default()
    }

    pub fn exists(&self, rel: &str) -> bool {
        fs::symlink_metadata(self.path.join(rel)).is_ok()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Make everything writable again so removal cannot fail on read-only test fixtures.
        fn relax(p: &Path) {
            if let Ok(meta) = fs::symlink_metadata(p) {
                if meta.is_dir() {
                    let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o700));
                    if let Ok(rd) = fs::read_dir(p) {
                        for e in rd.flatten() {
                            relax(&e.path());
                        }
                    }
                }
            }
        }
        relax(&self.path);
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// True when a `git` binary can be run.
pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Runs plain git (no hardening) for building test fixtures. Panics on failure.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A git repository on branch `main` with one commit containing `README.md` and `src/lib.txt`.
pub fn make_repo(dir: &TempDir) -> PathBuf {
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).expect("create repo dir");
    git(&repo, &["init", "-q", "-b", "main"]);
    let mut f = fs::File::create(repo.join("README.md")).expect("create file");
    writeln!(f, "# fixture").expect("write");
    fs::create_dir_all(repo.join("src")).expect("mkdir src");
    fs::write(repo.join("src/lib.txt"), "one\ntwo\nthree\n").expect("write lib");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    repo
}

/// Waits until `cond` is true or `timeout_ms` elapsed; returns the final value of `cond`.
pub fn wait_until(timeout_ms: u64, mut cond: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    loop {
        if cond() {
            return true;
        }
        if start.elapsed().as_millis() as u64 >= timeout_ms {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Runs a program on a real pseudo-terminal (so that it sees a terminal on stdin and stdout), types
/// `send` whenever the output so far contains the paired `expect`, and returns the exit code and
/// everything it printed. The environment is exactly `env`. Panics after `timeout`.
pub fn run_in_pty(
    program: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    script: &[(&str, &str)],
    timeout: std::time::Duration,
) -> (i32, String) {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::sync::{Arc, Mutex};
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open a pty");
    let mut cmd = CommandBuilder::new(program);
    cmd.args(args);
    cmd.env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = pair.slave.spawn_command(cmd).expect("spawn on the pty");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("pty reader");
    let mut writer = pair.master.take_writer().expect("pty writer");
    let seen = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&seen);
    let reader_thread = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    });
    let start = std::time::Instant::now();
    let mut consumed = 0usize;
    for (expect, send) in script {
        loop {
            let text = seen.lock().unwrap().clone();
            if let Some(at) = text[consumed..].find(expect) {
                consumed += at + expect.len();
                break;
            }
            if start.elapsed() > timeout {
                let _ = child.kill();
                panic!("waiting for {expect:?} on the terminal; saw: {text}");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        writer.write_all(send.as_bytes()).expect("type");
        writer.flush().expect("flush");
    }
    let code = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status.exit_code() as i32;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            panic!("the program did not finish; saw: {}", seen.lock().unwrap());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    drop(writer);
    let _ = reader_thread.join();
    let out = seen.lock().unwrap().clone();
    (code, out)
}
