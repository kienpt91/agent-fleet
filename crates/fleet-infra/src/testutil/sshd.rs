//! A throwaway OpenSSH server for tests: keys generated at run time in a temporary directory, an
//! ordinary user on a loopback port, forwarding and environment acceptance switched *on* so that
//! what stays off is attributable to the client's options. Nothing here is committed or reused.
//!
//! The tests need `ssh`, `ssh-keygen` and `/usr/sbin/sshd` and a passwd entry for the current user
//! (`scripts/ssh-env.sh` adds one in the test container). Without them a test is skipped, unless
//! the environment variable `FLEET_SSH_TESTS=1` demands them: then a missing piece fails the run.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use super::TempDir;
use fleet_app::hosts::HostDef;
use fleet_domain::ids::HostName;
use std::fs;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const SSHD: &str = "/usr/sbin/sshd";

pub struct TestSshd {
    pub dir: TempDir,
    pub port: u16,
    pub user: String,
    /// A private key the server accepts.
    pub client_key: PathBuf,
    /// A private key the server does not accept.
    pub other_key: PathBuf,
    child: Option<Child>,
}

fn required() -> bool {
    std::env::var("FLEET_SSH_TESTS").is_ok_and(|v| v == "1")
}

fn tool(name: &str) -> Option<PathBuf> {
    ["/usr/bin", "/bin", "/usr/local/bin"]
        .iter()
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
}

fn fail_or_skip(why: &str) -> Option<TestSshd> {
    assert!(!required(), "FLEET_SSH_TESTS=1 but {why}");
    eprintln!("skipped: {why}");
    None
}

pub fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind a port");
    l.local_addr().expect("address").port()
}

fn keygen(path: &Path) {
    let out = Command::new("/usr/bin/ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "fleet-test", "-f"])
        .arg(path)
        .output()
        .expect("run ssh-keygen");
    assert!(
        out.status.success(),
        "ssh-keygen: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

impl TestSshd {
    /// Starts a server, or `None` when this machine cannot run one (and the tests are optional).
    pub fn start() -> Option<TestSshd> {
        if tool("ssh").is_none() || tool("ssh-keygen").is_none() || !Path::new(SSHD).is_file() {
            return fail_or_skip("ssh, ssh-keygen or sshd is not installed");
        }
        let Ok(Some(user)) = nix::unistd::User::from_uid(nix::unistd::geteuid()) else {
            return fail_or_skip("the current user has no passwd entry (run scripts/ssh-env.sh)");
        };
        let dir = TempDir::new("sshd");
        let client_key = dir.join("client_ed25519");
        let other_key = dir.join("other_ed25519");
        keygen(&client_key);
        keygen(&other_key);
        keygen(&dir.join("host_ed25519"));
        let authorized = fs::read_to_string(dir.join("client_ed25519.pub")).unwrap();
        fs::write(dir.join("authorized_keys"), authorized).unwrap();
        let mut sshd = TestSshd {
            dir,
            port: free_port(),
            user: user.name,
            client_key,
            other_key,
            child: None,
        };
        sshd.launch();
        Some(sshd)
    }

    fn launch(&mut self) {
        let d = self.dir.path().display().to_string();
        let config = format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {d}/host_ed25519\nPidFile {d}/sshd.pid\n\
             AuthorizedKeysFile {d}/authorized_keys\nAllowUsers {user}\nPasswordAuthentication no\n\
             KbdInteractiveAuthentication no\nPubkeyAuthentication yes\nUsePAM no\nStrictModes no\n\
             AllowTcpForwarding yes\nAllowAgentForwarding yes\nX11Forwarding yes\nPermitTunnel yes\n\
             GatewayPorts no\nPermitTTY yes\nPermitUserEnvironment no\nAcceptEnv *\nPrintMotd no\n\
             ClientAliveInterval 0\nLogLevel VERBOSE\n",
            port = self.port,
            user = self.user,
        );
        fs::write(self.dir.join("sshd_config"), config).unwrap();
        let log = fs::File::create(self.dir.join("sshd.log")).unwrap();
        let child = Command::new(SSHD)
            .arg("-D")
            .arg("-e")
            .arg("-f")
            .arg(self.dir.join("sshd_config"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("start sshd");
        self.child = Some(child);
        let start = Instant::now();
        while TcpStream::connect(("127.0.0.1", self.port)).is_err() {
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "sshd did not come up: {}",
                self.dir.read("sshd.log")
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Replaces the host key (the server is restarted): what a reinstalled or impersonated host
    /// looks like.
    pub fn rotate_host_key(&mut self) {
        self.stop();
        let _ = fs::remove_file(self.dir.join("host_ed25519"));
        let _ = fs::remove_file(self.dir.join("host_ed25519.pub"));
        keygen(&self.dir.join("host_ed25519"));
        self.launch();
    }

    /// The server's public host key: `ssh-ed25519 AAAA...`.
    pub fn host_key(&self) -> String {
        let text = self.dir.read("host_ed25519.pub");
        let mut parts = text.split_whitespace();
        format!("{} {}", parts.next().unwrap(), parts.next().unwrap())
    }

    pub fn log(&self) -> String {
        self.dir.read("sshd.log")
    }

    /// A host entry for this server, working directory below the server's temporary directory.
    pub fn host_def(&self, name: &str, workers: &[&str]) -> HostDef {
        let mut h = HostDef::new(
            HostName::parse(name).unwrap(),
            "127.0.0.1",
            &self.user,
            &self.client_key.display().to_string(),
            &self.dir.join("remote-base").display().to_string(),
        );
        h.port = self.port;
        h.allowed_workers = workers.iter().map(|w| w.to_string()).collect();
        h.connect_timeout_secs = 5;
        h
    }
}

impl Drop for TestSshd {
    fn drop(&mut self) {
        self.stop();
    }
}
