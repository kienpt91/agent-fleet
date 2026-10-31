//! Who is on the other end of a TCP connection?
//!
//! The control socket asks the kernel for the peer's pid (`SO_PEERCRED`). A TCP socket has no such
//! thing, so the dashboard finds the *client's* end of the connection in the kernel's socket
//! table (`/proc/net/tcp*` on Linux, `lsof` on macOS) and then the processes that hold that
//! socket. The same admission rules as for the control socket are applied to those pids by the
//! caller: a worker, or a process a worker started, is refused.
//!
//! A peer on the loopback interface that cannot be found (it already exited, its socket is
//! orphaned, `/proc` is hidden) is [`PeerIdentity::Unknown`] and refused: the check fails closed,
//! like the control socket's. A peer with a non-loopback address that is not in the table is a
//! different machine ([`PeerIdentity::NotLocal`]); only the token protects against that one.

use std::net::{IpAddr, SocketAddr};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerIdentity {
    /// The processes that hold the client's end of the connection (at most [`MAX_HOLDERS`]).
    Local(Vec<u32>),
    /// Not a socket of this machine.
    NotLocal,
    /// A loopback peer that could not be identified.
    Unknown,
}

/// More processes than this holding one socket is not a normal client.
pub const MAX_HOLDERS: usize = 16;

/// Parses one hex address of `/proc/net/tcp` (`0100007F:1F90`) or `/proc/net/tcp6`.
/// Addresses are printed as host-order 32-bit words, so on a little-endian host each word's bytes
/// are reversed.
pub fn parse_proc_addr(text: &str) -> Option<SocketAddr> {
    let (ip, port) = text.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |s: &str| -> Option<[u8; 4]> {
        let n = u32::from_str_radix(s, 16).ok()?;
        Some(if cfg!(target_endian = "little") {
            n.to_le_bytes()
        } else {
            n.to_be_bytes()
        })
    };
    let addr = match ip.len() {
        8 => IpAddr::from(word(ip)?),
        32 => {
            let mut bytes = [0u8; 16];
            for i in 0..4 {
                let w = word(ip.get(i * 8..i * 8 + 8)?)?;
                bytes.get_mut(i * 4..i * 4 + 4)?.copy_from_slice(&w);
            }
            IpAddr::from(bytes)
        }
        _ => return None,
    };
    Some(SocketAddr::new(addr.to_canonical(), port))
}

fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

/// Finds the socket whose local address is `local` and whose remote address is `remote` in the
/// text of `/proc/net/tcp` or `/proc/net/tcp6` and returns its inode (0 for a socket that no
/// process owns any more).
pub fn find_socket_inode(table: &str, local: SocketAddr, remote: SocketAddr) -> Option<u64> {
    let (local, remote) = (canonical(local), canonical(remote));
    for line in table.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (Some(l), Some(r), Some(inode)) = (fields.get(1), fields.get(2), fields.get(9)) else {
            continue;
        };
        if parse_proc_addr(l) == Some(local) && parse_proc_addr(r) == Some(remote) {
            return inode.parse().ok();
        }
    }
    None
}

/// Pids from the `-F pn` output of `lsof` whose socket runs from `local` to `remote`.
pub fn parse_lsof(text: &str, local: SocketAddr, remote: SocketAddr) -> Vec<u32> {
    let want = format!("{}->{}", lsof_addr(local), lsof_addr(remote));
    let mut pid: Option<u32> = None;
    let mut out: Vec<u32> = Vec::new();
    for line in text.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse().ok();
        } else if let Some(n) = line.strip_prefix('n') {
            if n.split_whitespace().next() == Some(want.as_str()) {
                if let Some(p) = pid {
                    if !out.contains(&p) && out.len() < MAX_HOLDERS {
                        out.push(p);
                    }
                }
            }
        }
    }
    out
}

fn lsof_addr(addr: SocketAddr) -> String {
    match addr.ip().to_canonical() {
        IpAddr::V4(ip) => format!("{ip}:{}", addr.port()),
        IpAddr::V6(ip) => format!("[{ip}]:{}", addr.port()),
    }
}

#[cfg(target_os = "linux")]
fn holders_of_inode(inode: u64) -> Vec<u32> {
    let wanted = format!("socket:[{inode}]");
    let mut out: Vec<u32> = Vec::new();
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in procs.flatten().take(32_768) {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten().take(8192) {
            if std::fs::read_link(fd.path()).is_ok_and(|t| t.as_os_str() == wanted.as_str()) {
                out.push(pid);
                break;
            }
        }
        if out.len() >= MAX_HOLDERS {
            break;
        }
    }
    out
}

/// Identifies the client end of an accepted connection: `peer` is the client's address as the
/// server sees it, `local` the server's.
#[cfg(target_os = "linux")]
pub fn identify(peer: SocketAddr, local: SocketAddr) -> PeerIdentity {
    let inode = ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|t| std::fs::read_to_string(t).ok())
        .find_map(|table| find_socket_inode(&table, peer, local));
    match inode {
        Some(inode) if inode > 0 => {
            let holders = holders_of_inode(inode);
            if holders.is_empty() {
                PeerIdentity::Unknown
            } else {
                PeerIdentity::Local(holders)
            }
        }
        Some(_) => PeerIdentity::Unknown,
        None => unlisted(peer),
    }
}

/// On macOS the socket table is read with `lsof` (a fixed absolute path, an empty environment, two
/// seconds at most; a missing or hung `lsof` leaves the peer unidentified, which is refused).
/// This code type-checks for macOS but has not been run there.
#[cfg(target_os = "macos")]
pub fn identify(peer: SocketAddr, local: SocketAddr) -> PeerIdentity {
    use std::process::Command;
    let out = crate::procid::output_within(
        Command::new("/usr/sbin/lsof")
            .args([
                "-nP",
                "-w",
                "-a",
                "-F",
                "pn",
                &format!("-iTCP:{}", peer.port()),
            ])
            .env_clear()
            .env("LC_ALL", "C"),
        crate::procid::TOOL_TIMEOUT,
    );
    let Some(out) = out else {
        return PeerIdentity::Unknown;
    };
    let Ok(text) = String::from_utf8(out.stdout) else {
        return PeerIdentity::Unknown;
    };
    let holders = parse_lsof(&text, peer, local);
    if holders.is_empty() {
        unlisted(peer)
    } else {
        PeerIdentity::Local(holders)
    }
}

/// Other platforms cannot look a TCP peer up: loopback clients are refused.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn identify(peer: SocketAddr, _local: SocketAddr) -> PeerIdentity {
    unlisted(peer)
}

/// A peer that is not in the socket table: a loopback one has exited or is hidden (refuse), any
/// other is a different machine.
fn unlisted(peer: SocketAddr) -> PeerIdentity {
    if peer.ip().to_canonical().is_loopback() {
        PeerIdentity::Unknown
    } else {
        PeerIdentity::NotLocal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::net::{TcpListener, TcpStream};

    const TABLE: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1F90 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 424242 1 0000000000000000 100 0 0 10 0\n   1: 0100007F:D431 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 535353 1 0000000000000000 100 0 0 10 0\n   2: 0100007F:D432 0100007F:1F90 06 00000000:00000000 03:00000000 00000000     0        0 0 3 0000000000000000\n";

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn proc_addresses_are_decoded() {
        if cfg!(target_endian = "little") {
            assert_eq!(parse_proc_addr("0100007F:1F90"), Some(sa("127.0.0.1:8080")));
            assert_eq!(
                parse_proc_addr("00000000000000000000000001000000:0050"),
                Some(sa("[::1]:80"))
            );
            assert_eq!(
                parse_proc_addr("0000000000000000FFFF00000100007F:0050"),
                Some(sa("127.0.0.1:80")),
                "an IPv4-mapped address is the IPv4 address"
            );
        }
        for bad in [
            "",
            "0100007F",
            "ZZ:1",
            "0100007F:ZZZZZ",
            "0100:50",
            "0100007F:1F90:1",
        ] {
            assert_eq!(parse_proc_addr(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_clients_own_socket_is_found_not_the_servers() {
        if !cfg!(target_endian = "little") {
            return;
        }
        // server 127.0.0.1:8080, client 127.0.0.1:54321 (0xD431)
        let client = sa("127.0.0.1:54321");
        let server = sa("127.0.0.1:8080");
        assert_eq!(find_socket_inode(TABLE, client, server), Some(535_353));
        assert_eq!(find_socket_inode(TABLE, server, client), Some(424_242));
        assert_eq!(
            find_socket_inode(TABLE, sa("127.0.0.1:54322"), server),
            Some(0),
            "an orphaned socket has inode 0"
        );
        assert_eq!(find_socket_inode(TABLE, sa("127.0.0.1:1"), server), None);
        assert_eq!(find_socket_inode("", client, server), None);
        assert_eq!(
            find_socket_inode("header\ngarbage line\n", client, server),
            None
        );
    }

    #[test]
    fn lsof_output_is_matched_on_the_exact_connection() {
        let text = "p100\nn127.0.0.1:54321->127.0.0.1:8080\np200\nn127.0.0.1:8080->127.0.0.1:54321\np300\nn*:8080\np100\nn127.0.0.1:54321->127.0.0.1:8080 (ESTABLISHED)\n";
        assert_eq!(
            parse_lsof(text, sa("127.0.0.1:54321"), sa("127.0.0.1:8080")),
            vec![100]
        );
        assert_eq!(
            parse_lsof(text, sa("127.0.0.1:8080"), sa("127.0.0.1:54321")),
            vec![200]
        );
        assert!(parse_lsof("garbage", sa("127.0.0.1:1"), sa("127.0.0.1:2")).is_empty());
        let v6 = "p7\nn[::1]:5000->[::1]:80\n";
        assert_eq!(parse_lsof(v6, sa("[::1]:5000"), sa("[::1]:80")), vec![7]);
    }

    #[test]
    fn unlisted_peers_are_refused_only_when_they_claim_to_be_local() {
        assert_eq!(unlisted(sa("127.0.0.1:1")), PeerIdentity::Unknown);
        assert_eq!(unlisted(sa("[::1]:1")), PeerIdentity::Unknown);
        assert_eq!(unlisted(sa("203.0.113.9:1")), PeerIdentity::NotLocal);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_real_loopback_client_is_identified_as_this_process() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server_side, peer) = listener.accept().unwrap();
        assert_eq!(client.local_addr().unwrap(), peer);
        match identify(peer, server_side.local_addr().unwrap()) {
            PeerIdentity::Local(pids) => assert!(pids.contains(&std::process::id()), "{pids:?}"),
            other => panic!("expected this process, got {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_client_that_is_gone_is_unknown_not_admitted() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server_side, peer) = listener.accept().unwrap();
        drop(client);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let found = identify(peer, server_side.local_addr().unwrap());
        assert_eq!(found, PeerIdentity::Unknown, "{found:?}");
    }
}
