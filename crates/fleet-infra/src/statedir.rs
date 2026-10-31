//! Where the daemon keeps its files, and the single-instance lock.

use crate::fsx;
use fleet_domain::error::{FleetError, Result};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg, OFlag};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// `sockaddr_un.sun_path` holds 104 bytes on macOS and 108 on Linux, including the NUL.
const MAX_SOCKET_PATH: usize = 100;

/// Picks the state directory: explicit path, else `$XDG_STATE_HOME/agent-fleet`, else
/// `$HOME/.local/state/agent-fleet`. Empty or relative environment values are ignored, as the
/// XDG specification requires.
pub fn resolve_state_dir(
    explicit: Option<&Path>,
    xdg_state_home: Option<String>,
    home: Option<String>,
) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    let absolute = |v: Option<String>| v.filter(|s| Path::new(s).is_absolute());
    if let Some(xdg) = absolute(xdg_state_home) {
        return Ok(Path::new(&xdg).join("agent-fleet"));
    }
    if let Some(home) = absolute(home) {
        return Ok(Path::new(&home).join(".local/state/agent-fleet"));
    }
    Err(FleetError::invalid(
        "cannot locate a state directory: set HOME or XDG_STATE_HOME, or pass --state-dir",
    ))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateLayout {
    pub root: PathBuf,
    pub control_dir: PathBuf,
    pub socket: PathBuf,
    pub lock: PathBuf,
    pub events: PathBuf,
    pub head: PathBuf,
    pub logs_dir: PathBuf,
    pub default_config: PathBuf,
}

impl StateLayout {
    pub fn new(root: PathBuf) -> StateLayout {
        let control_dir = root.join("control");
        StateLayout {
            socket: control_dir.join("fleet.sock"),
            lock: root.join("daemon.lock"),
            events: root.join("events.log"),
            head: root.join("events.head"),
            logs_dir: root.join("logs"),
            default_config: root.join("fleet.toml"),
            control_dir,
            root,
        }
    }

    /// Creates the private directories. Refuses symlinks and directories of other users.
    pub fn prepare(&self) -> Result<()> {
        let len = self.socket.as_os_str().len();
        if len > MAX_SOCKET_PATH {
            return Err(FleetError::invalid(format!(
                "the control socket path would be {len} bytes long but unix sockets allow about \
                 {MAX_SOCKET_PATH}; use a shorter --state-dir (for example /tmp/fleet-<name>)"
            )));
        }
        fsx::ensure_private_dir(&self.root)?;
        fsx::ensure_private_dir(&self.control_dir)?;
        fsx::ensure_private_dir(&self.logs_dir)?;
        Ok(())
    }

    /// Workers run inside repositories and can write there, so the state directory (event log,
    /// control socket) must never be part of one.
    pub fn reject_inside(&self, forbidden: &[PathBuf]) -> Result<()> {
        let root = canonical_lenient(&self.root);
        for dir in forbidden {
            let dir = canonical_lenient(dir);
            if root.starts_with(&dir) {
                return Err(FleetError::invalid(format!(
                    "the state directory {} lies inside {}; choose a state directory outside \
                     the repositories workers can modify",
                    self.root.display(),
                    dir.display()
                )));
            }
        }
        Ok(())
    }
}

/// Canonicalizes the longest existing prefix and appends the rest, so a state directory that
/// does not exist yet can still be compared.
fn canonical_lenient(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(c) = fs::canonicalize(&existing) {
            let mut out = c;
            // `file_name` never yields `..`, so the leftover parts are plain names.
            out.extend(rest.iter().rev());
            return out;
        }
        match (existing.file_name().map(|n| n.to_os_string()), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Exclusive lock proving that this process is the only daemon for a state directory. The
/// kernel drops an `flock` when its holder dies, so a crash cannot leave a stale lock behind.
pub struct DaemonLock {
    _lock: Flock<std::fs::File>,
}

impl DaemonLock {
    pub fn acquire(path: &Path) -> Result<DaemonLock> {
        // No truncate on open: that would erase the pid of a running daemon before we know
        // whether we hold the lock.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(OFlag::O_NOFOLLOW.bits())
            .open(path)
            .map_err(|e| FleetError::io(&format!("cannot open lock {}", path.display()), &e))?;
        let meta = file
            .metadata()
            .map_err(|e| FleetError::io(&format!("cannot examine lock {}", path.display()), &e))?;
        if !meta.is_file() {
            return Err(FleetError::invalid(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        if meta.uid() != nix::unistd::geteuid().as_raw() {
            return Err(FleetError::forbidden(format!(
                "{} is owned by another user",
                path.display()
            )));
        }
        let mut lock = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(l) => l,
            Err((_, Errno::EWOULDBLOCK)) => {
                return Err(FleetError::conflict(format!(
                    "another fleet daemon already holds {}",
                    path.display()
                )))
            }
            Err((_, errno)) => {
                return Err(FleetError::internal(format!(
                    "cannot lock {}: {}",
                    path.display(),
                    errno.desc()
                )))
            }
        };
        // The pid is informational for humans; the lock itself is the source of truth.
        lock.set_len(0)
            .and_then(|()| lock.write_all(format!("{}\n", std::process::id()).as_bytes()))
            .map_err(|e| FleetError::io("cannot record pid in the lock file", &e))?;
        Ok(DaemonLock { _lock: lock })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use fleet_domain::error::ErrorCode;
    use std::os::unix::fs::symlink;

    #[test]
    fn layout_paths() {
        let l = StateLayout::new(PathBuf::from("/s"));
        assert_eq!(l.root, Path::new("/s"));
        assert_eq!(l.control_dir, Path::new("/s/control"));
        assert_eq!(l.socket, Path::new("/s/control/fleet.sock"));
        assert_eq!(l.lock, Path::new("/s/daemon.lock"));
        assert_eq!(l.events, Path::new("/s/events.log"));
        assert_eq!(l.head, Path::new("/s/events.head"));
        assert_eq!(l.logs_dir, Path::new("/s/logs"));
        assert_eq!(l.default_config, Path::new("/s/fleet.toml"));
    }

    #[test]
    fn resolution_order() {
        let some = |s: &str| Some(s.to_string());
        assert_eq!(
            resolve_state_dir(Some(Path::new("/x")), some("/xdg"), some("/h")).unwrap(),
            Path::new("/x")
        );
        assert_eq!(
            resolve_state_dir(None, some("/xdg"), some("/h")).unwrap(),
            Path::new("/xdg/agent-fleet")
        );
        assert_eq!(
            resolve_state_dir(None, None, some("/h")).unwrap(),
            Path::new("/h/.local/state/agent-fleet")
        );
        // Empty and relative values are ignored.
        assert_eq!(
            resolve_state_dir(None, some(""), some("/h")).unwrap(),
            Path::new("/h/.local/state/agent-fleet")
        );
        assert_eq!(
            resolve_state_dir(None, some("rel"), some("/h")).unwrap(),
            Path::new("/h/.local/state/agent-fleet")
        );
        let err = resolve_state_dir(None, None, None).unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);
        assert!(resolve_state_dir(None, some(""), some("rel")).is_err());
    }

    #[test]
    fn prepare_creates_private_directories() {
        let t = TempDir::new("state-prepare");
        let l = StateLayout::new(t.join("s"));
        l.prepare().unwrap();
        for d in [&l.root, &l.control_dir, &l.logs_dir] {
            assert_eq!(fs::metadata(d).unwrap().mode() & 0o777, 0o700, "{d:?}");
        }
        // Loosened permissions are tightened on the next start.
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&l.control_dir, fs::Permissions::from_mode(0o755)).unwrap();
        l.prepare().unwrap();
        assert_eq!(fs::metadata(&l.control_dir).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn prepare_refuses_a_symlinked_state_dir() {
        let t = TempDir::new("state-link");
        fs::create_dir(t.join("real")).unwrap();
        symlink(t.join("real"), t.join("s")).unwrap();
        let err = StateLayout::new(t.join("s")).prepare().unwrap_err();
        assert!(err.message.contains("symlink"));
        assert!(!t.exists("real/control"));
    }

    #[test]
    fn prepare_rejects_an_overlong_socket_path() {
        let t = TempDir::new("state-long");
        let l = StateLayout::new(t.join(&"d".repeat(120)));
        let err = l.prepare().unwrap_err();
        assert_eq!(err.code, ErrorCode::Invalid);
        assert!(err.message.contains("--state-dir"));
        assert!(!l.root.exists(), "nothing is created for an unusable path");
    }

    #[test]
    fn the_lock_is_exclusive_and_released_on_drop() {
        let t = TempDir::new("state-lock");
        let path = t.join("daemon.lock");
        let first = DaemonLock::acquire(&path).unwrap();
        let err = DaemonLock::acquire(&path).err().unwrap();
        assert_eq!(err.code, ErrorCode::Conflict);
        // The failed attempt must not have erased the holder's pid.
        assert_eq!(t.read("daemon.lock").trim(), std::process::id().to_string());
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        drop(first);
        let _second = DaemonLock::acquire(&path).unwrap();
    }

    #[test]
    fn the_lock_refuses_a_symlink() {
        let t = TempDir::new("state-lock-link");
        let target = t.write("target", "x");
        symlink(&target, t.join("daemon.lock")).unwrap();
        assert!(DaemonLock::acquire(&t.join("daemon.lock")).is_err());
        assert_eq!(t.read("target"), "x");
    }

    #[test]
    fn reject_inside_compares_canonical_paths() {
        let t = TempDir::new("state-inside");
        let repo = t.join("repo");
        fs::create_dir(&repo).unwrap();
        let outside = t.join("elsewhere");
        fs::create_dir(&outside).unwrap();
        let forbidden = vec![repo.clone()];

        // Equal, nested (existing or not yet created) and via a `..` or symlink detour.
        assert!(StateLayout::new(repo.clone()).reject_inside(&forbidden).is_err());
        assert!(StateLayout::new(repo.join("a/b")).reject_inside(&forbidden).is_err());
        assert!(StateLayout::new(outside.join("../repo/.state"))
            .reject_inside(&forbidden)
            .is_err());
        symlink(&repo, outside.join("link")).unwrap();
        assert!(StateLayout::new(outside.join("link/state"))
            .reject_inside(&forbidden)
            .is_err());

        // A sibling whose name merely starts with the same text is fine.
        assert!(StateLayout::new(t.join("repo-state")).reject_inside(&forbidden).is_ok());
        assert!(StateLayout::new(outside.join("state")).reject_inside(&forbidden).is_ok());
        assert!(StateLayout::new(outside.join("state")).reject_inside(&[]).is_ok());
    }
}
