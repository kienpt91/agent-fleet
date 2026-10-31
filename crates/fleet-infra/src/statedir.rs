//! Where the daemon keeps its files, the single-instance lock, and the control-socket token.

use crate::fsx;
use fleet_domain::error::{FleetError, Result};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg, OFlag};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
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
    /// The control-socket capability token, next to the socket.
    pub token: PathBuf,
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
            token: control_dir.join(TOKEN_FILE),
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
        match (
            existing.file_name().map(|n| n.to_os_string()),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// File name of the control-socket capability token, in the directory of the socket.
pub const TOKEN_FILE: &str = "token";
const TOKEN_BYTES: usize = 32;
/// Characters in a token: 32 random bytes as lowercase hex.
pub const TOKEN_LEN: usize = TOKEN_BYTES * 2;
/// The largest token file that is read: one token plus room for a trailing newline.
const MAX_TOKEN_FILE: u64 = 128;

/// Where the token for `socket` lives: next to it.
pub fn token_path(socket: &Path) -> PathBuf {
    socket.with_file_name(TOKEN_FILE)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for b in bytes {
        out.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// A fresh token: 32 bytes from the kernel's random source, as 64 hex characters.
pub fn generate_token() -> Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| FleetError::io("reading random bytes for the control token", &e))?;
    Ok(hex(&bytes))
}

/// Creates the token file with a fresh token and returns the token. A leftover regular file from
/// an earlier daemon is replaced; a symlink or anything else at that path is refused, and the
/// new file is created exclusively without following links, mode 0600.
pub fn create_token_file(path: &Path) -> Result<String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => fs::remove_file(path)
            .map_err(|e| FleetError::io("removing the stale control token", &e))?,
        Ok(_) => {
            return Err(FleetError::conflict(format!(
                "{} exists and is not a regular file; refusing to replace it",
                path.display()
            )))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(FleetError::io("inspecting the control token path", &e)),
    }
    let token = generate_token()?;
    let write = || -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(OFlag::O_NOFOLLOW.bits())
            .open(path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(token.as_bytes())
    };
    if let Err(e) = write() {
        let _ = fs::remove_file(path);
        return Err(FleetError::io("writing the control token", &e));
    }
    Ok(token)
}

/// Removes the token file, but only if it is a regular file.
pub fn remove_token_file(path: &Path) {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_file() {
            let _ = fs::remove_file(path);
        }
    }
}

/// The token file must be a regular file of this user that nobody else can read or write.
pub fn verify_token_file(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            FleetError::forbidden(format!(
                "there is no control token at {}; the daemon creates it when it starts",
                path.display()
            ))
        } else {
            FleetError::io(&format!("the control token {}", path.display()), &e)
        }
    })?;
    if !meta.file_type().is_file() {
        return Err(FleetError::forbidden(format!(
            "{} is not a regular file (symlinks are refused)",
            path.display()
        )));
    }
    if meta.uid() != nix::unistd::geteuid().as_raw() {
        return Err(FleetError::forbidden(format!(
            "{} is owned by another user",
            path.display()
        )));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode != 0o600 {
        return Err(FleetError::forbidden(format!(
            "{} must have mode 0600 but has {mode:04o}",
            path.display()
        )));
    }
    Ok(())
}

/// Reads the token that belongs to `socket`. Refuses a token file that is not private to this
/// user, is larger than a token, or does not look like one. The token is never part of an error
/// message.
pub fn read_token(socket: &Path) -> Result<String> {
    let path = token_path(socket);
    verify_token_file(&path)?;
    let bytes = fsx::read_regular(&path, MAX_TOKEN_FILE)
        .map_err(|e| FleetError::forbidden(format!("the control token {} {e}", path.display())))?;
    let text = std::str::from_utf8(&bytes).unwrap_or("");
    let token = text.strip_suffix('\n').unwrap_or(text);
    if token.len() != TOKEN_LEN
        || !token
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(FleetError::forbidden(format!(
            "{} does not hold a control token",
            path.display()
        )));
    }
    Ok(token.to_string())
}

/// Compares a presented token with the real one. The time taken depends only on the lengths
/// (which are not secret), never on how many leading characters match.
pub fn tokens_equal(presented: &str, expected: &str) -> bool {
    let (a, b) = (presented.as_bytes(), expected.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
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
        assert_eq!(l.token, Path::new("/s/control/token"));
        assert_eq!(
            token_path(&l.socket),
            l.token,
            "the token sits next to the socket"
        );
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
        assert!(StateLayout::new(repo.clone())
            .reject_inside(&forbidden)
            .is_err());
        assert!(StateLayout::new(repo.join("a/b"))
            .reject_inside(&forbidden)
            .is_err());
        assert!(StateLayout::new(outside.join("../repo/.state"))
            .reject_inside(&forbidden)
            .is_err());
        symlink(&repo, outside.join("link")).unwrap();
        assert!(StateLayout::new(outside.join("link/state"))
            .reject_inside(&forbidden)
            .is_err());

        // A sibling whose name merely starts with the same text is fine.
        assert!(StateLayout::new(t.join("repo-state"))
            .reject_inside(&forbidden)
            .is_ok());
        assert!(StateLayout::new(outside.join("state"))
            .reject_inside(&forbidden)
            .is_ok());
        assert!(StateLayout::new(outside.join("state"))
            .reject_inside(&[])
            .is_ok());
    }

    fn is_hex_token(s: &str) -> bool {
        s.len() == TOKEN_LEN && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    }

    #[test]
    fn tokens_are_64_hex_characters_and_never_repeat() {
        let tokens: Vec<String> = (0..8).map(|_| generate_token().unwrap()).collect();
        for t in &tokens {
            assert!(is_hex_token(t), "{t}");
        }
        let distinct: std::collections::BTreeSet<&String> = tokens.iter().collect();
        assert_eq!(
            distinct.len(),
            tokens.len(),
            "tokens must come from real randomness"
        );
    }

    #[test]
    fn a_token_file_is_private_and_replaces_a_stale_one() {
        let t = TempDir::new("state-token");
        let path = t.join("token");
        let first = create_token_file(&path).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(t.read("token"), first);
        verify_token_file(&path).unwrap();

        let second = create_token_file(&path).unwrap();
        assert_ne!(first, second, "a new daemon run gets a new token");
        assert_eq!(t.read("token"), second);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn a_token_path_that_is_a_symlink_or_directory_is_refused_and_untouched() {
        let t = TempDir::new("state-token-link");
        let target = t.write("target", "precious");
        symlink(&target, t.join("token")).unwrap();
        let err = create_token_file(&t.join("token")).unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
        assert_eq!(
            t.read("target"),
            "precious",
            "nothing is written through a symlink"
        );
        assert!(fs::symlink_metadata(t.join("token"))
            .unwrap()
            .file_type()
            .is_symlink());

        fs::create_dir(t.join("dir-token")).unwrap();
        assert!(create_token_file(&t.join("dir-token")).is_err());
        remove_token_file(&t.join("token"));
        assert!(
            t.exists("token"),
            "remove_token_file only removes regular files"
        );
    }

    #[test]
    fn the_client_side_refuses_a_token_that_is_not_private_or_not_a_token() {
        let t = TempDir::new("state-token-read");
        let socket = t.join("fleet.sock");
        let path = token_path(&socket);
        assert_eq!(read_token(&socket).unwrap_err().code, ErrorCode::Forbidden);

        let token = create_token_file(&path).unwrap();
        assert_eq!(read_token(&socket).unwrap(), token);

        // group/other access: the secret is already shared
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let err = read_token(&socket).unwrap_err();
        assert_eq!(err.code, ErrorCode::Forbidden);
        assert!(err.message.contains("0600"), "{}", err.message);
        assert!(
            !err.message.contains(&token),
            "an error must never print the token"
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        // wrong shape, too large, symlink
        for bad in ["short", &"g".repeat(TOKEN_LEN), &"a".repeat(TOKEN_LEN + 1)] {
            fs::write(&path, bad).unwrap();
            assert_eq!(
                read_token(&socket).unwrap_err().code,
                ErrorCode::Forbidden,
                "{bad}"
            );
        }
        fs::write(&path, "a".repeat(10_000)).unwrap();
        assert_eq!(read_token(&socket).unwrap_err().code, ErrorCode::Forbidden);
        fs::remove_file(&path).unwrap();
        let real = t.write("real", &"a".repeat(TOKEN_LEN));
        fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&real, &path).unwrap();
        assert_eq!(read_token(&socket).unwrap_err().code, ErrorCode::Forbidden);
    }

    #[test]
    fn token_comparison_is_exact() {
        let token = "ab".repeat(32);
        assert!(tokens_equal(&token, &token));
        assert!(!tokens_equal("", &token));
        assert!(!tokens_equal(&token, ""));
        assert!(!tokens_equal(&token[..63], &token));
        assert!(!tokens_equal(&format!("{token}0"), &token));
        let mut last_differs = token.clone();
        last_differs.replace_range(63.., "c");
        assert!(!tokens_equal(&last_differs, &token));
        let mut first_differs = token.clone();
        first_differs.replace_range(..1, "c");
        assert!(!tokens_equal(&first_differs, &token));
    }
}
