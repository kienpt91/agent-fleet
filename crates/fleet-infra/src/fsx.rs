//! Safe file helpers for everything a worker can influence.
//!
//! Workers write into their own worktree, so any path below it may be a symlink, a FIFO, a huge
//! file or a directory swapped in at the last moment. These helpers never follow symlinks,
//! never block on a FIFO, check the type and size of the file they actually opened (not of the
//! path they looked at earlier) and read at most a fixed number of bytes.

use fleet_domain::error::{FleetError, Result};
use nix::errno::Errno;
use nix::fcntl::{openat, OFlag};
use nix::sys::stat::Mode;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadError {
    Missing,
    NotRegular(&'static str),
    TooLarge { size: u64, max: u64 },
    Io(String),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Missing => write!(f, "does not exist"),
            ReadError::NotRegular(why) => write!(f, "{why}"),
            ReadError::TooLarge { max, .. } => write!(f, "is larger than {max} bytes"),
            ReadError::Io(e) => write!(f, "cannot be read ({e})"),
        }
    }
}

impl std::error::Error for ReadError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// What `path` is, without following a final symlink. `None` when it does not exist or cannot
/// be examined.
pub fn lstat_kind(path: &Path) -> Option<FileKind> {
    let meta = fs::symlink_metadata(path).ok()?;
    let ft = meta.file_type();
    Some(if ft.is_symlink() {
        FileKind::Symlink
    } else if ft.is_file() {
        FileKind::File
    } else if ft.is_dir() {
        FileKind::Dir
    } else {
        FileKind::Other
    })
}

pub fn is_real_dir(path: &Path) -> bool {
    lstat_kind(path) == Some(FileKind::Dir)
}

/// Unlinks a file, symlink or FIFO. `unlink` never follows a symlink, and it refuses
/// directories, which the caller must handle explicitly.
pub fn remove_nofollow(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

fn read_flags() -> i32 {
    // O_NONBLOCK: opening a FIFO for reading must return at once instead of waiting for a writer.
    (OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC).bits()
}

fn map_open_error(e: &io::Error) -> ReadError {
    match e.kind() {
        io::ErrorKind::NotFound => ReadError::Missing,
        _ if e.raw_os_error() == Some(Errno::ELOOP as i32) => ReadError::NotRegular("is a symlink"),
        _ => ReadError::Io(e.kind().to_string()),
    }
}

/// Reads a regular file of at most `max` bytes. Symlinks, FIFOs, devices and directories are
/// rejected before any byte is read.
pub fn read_regular(path: &Path, max: u64) -> Result<Vec<u8>, ReadError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => return Err(map_open_error(&e)),
    };
    let ft = meta.file_type();
    if ft.is_symlink() {
        return Err(ReadError::NotRegular("is a symlink"));
    }
    if !ft.is_file() {
        return Err(ReadError::NotRegular("is not a regular file"));
    }
    if meta.len() > max {
        return Err(ReadError::TooLarge {
            size: meta.len(),
            max,
        });
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(read_flags())
        .open(path)
        .map_err(|e| map_open_error(&e))?;
    read_open(file, max)
}

/// Like [`read_regular`] for `dir/name`, but `dir` is opened first without following symlinks
/// and `name` is opened relative to that descriptor, so replacing a path component after the
/// check cannot redirect the read.
pub fn read_regular_in(dir: &Path, name: &str, max: u64) -> Result<Vec<u8>, ReadError> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(ReadError::Io("invalid file name".to_string()));
    }
    let dir_fd = nix::fcntl::open(
        dir,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| match e {
        Errno::ENOENT => ReadError::Missing,
        // Linux reports a symlink opened with O_DIRECTORY|O_NOFOLLOW as ENOTDIR.
        Errno::ELOOP | Errno::ENOTDIR => {
            ReadError::NotRegular("is inside a symlink or a non-directory")
        }
        other => ReadError::Io(other.desc().to_string()),
    })?;
    let fd = openat(
        &dir_fd,
        name,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| match e {
        Errno::ENOENT => ReadError::Missing,
        Errno::ELOOP => ReadError::NotRegular("is a symlink"),
        other => ReadError::Io(other.desc().to_string()),
    })?;
    read_open(File::from(fd), max)
}

/// Opens `rel` (a relative path of `/`-separated, non-empty, non-dot components) below `root`
/// without following a symlink anywhere on the way: every directory component is opened relative
/// to the descriptor of the one before, with `O_NOFOLLOW | O_DIRECTORY`, and so is the final
/// file. A worker that replaces a component by a link to somewhere else (or swaps it later) cannot
/// make the open land outside `root`. The result is not yet known to be a regular file; check it
/// on the open descriptor ([`read_open`] does).
pub fn open_beneath(root: &Path, rel: &str) -> Result<File, ReadError> {
    let parts: Vec<&str> = rel.split('/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == ".." || p.contains('\0'))
    {
        return Err(ReadError::Io("invalid path".to_string()));
    }
    let Some((last, dirs)) = parts.split_last() else {
        return Err(ReadError::Io("invalid path".to_string()));
    };
    let inside_link = || ReadError::NotRegular("is inside a symlink or a non-directory");
    let dir_flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC;
    let mut dir = nix::fcntl::open(root, dir_flags, Mode::empty()).map_err(|e| match e {
        Errno::ENOENT => ReadError::Missing,
        Errno::ELOOP | Errno::ENOTDIR => inside_link(),
        other => ReadError::Io(other.desc().to_string()),
    })?;
    for part in dirs {
        dir = openat(&dir, *part, dir_flags, Mode::empty()).map_err(|e| match e {
            Errno::ENOENT => ReadError::Missing,
            Errno::ELOOP | Errno::ENOTDIR => inside_link(),
            other => ReadError::Io(other.desc().to_string()),
        })?;
    }
    let fd = openat(
        &dir,
        *last,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| match e {
        Errno::ENOENT => ReadError::Missing,
        Errno::ELOOP => ReadError::NotRegular("is a symlink"),
        other => ReadError::Io(other.desc().to_string()),
    })?;
    Ok(File::from(fd))
}

/// Re-checks the open descriptor (the path may have changed since it was examined) and reads
/// with a hard byte cap, so a file that grows during the read stays bounded.
pub fn read_open(file: File, max: u64) -> Result<Vec<u8>, ReadError> {
    let meta = file
        .metadata()
        .map_err(|e| ReadError::Io(e.kind().to_string()))?;
    if !meta.file_type().is_file() {
        return Err(ReadError::NotRegular("is not a regular file"));
    }
    if meta.len() > max {
        return Err(ReadError::TooLarge {
            size: meta.len(),
            max,
        });
    }
    let capacity = usize::try_from(meta.len()).unwrap_or(0);
    let mut buf = Vec::with_capacity(capacity);
    file.take(max.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| ReadError::Io(e.kind().to_string()))?;
    let size = u64::try_from(buf.len()).unwrap_or(u64::MAX);
    if size > max {
        return Err(ReadError::TooLarge { size, max });
    }
    Ok(buf)
}

/// Makes sure `path` is a private directory owned by this user. A missing leaf is created with
/// mode 0700; an existing directory must be real (not a symlink), owned by the effective uid,
/// and has group/other permissions removed.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    if check_existing_dir(path)? {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| FleetError::io(&format!("cannot create {}", parent.display()), &e))?;
        }
    }
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => Ok(()),
        // Created concurrently: validate what is there instead of trusting it.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            if check_existing_dir(path)? {
                Ok(())
            } else {
                Err(FleetError::io(
                    &format!("cannot create {}", path.display()),
                    &e,
                ))
            }
        }
        Err(e) => Err(FleetError::io(
            &format!("cannot create {}", path.display()),
            &e,
        )),
    }
}

/// `Ok(true)` when `path` exists and is acceptable, `Ok(false)` when it does not exist.
fn check_existing_dir(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(FleetError::invalid(format!(
                "{} is a symlink; refusing to use it",
                path.display()
            )))
        }
        Ok(m) if !m.is_dir() => {
            return Err(FleetError::invalid(format!(
                "{} exists and is not a directory",
                path.display()
            )))
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            return Err(FleetError::io(
                &format!("cannot examine {}", path.display()),
                &e,
            ))
        }
    }
    // Work on the opened descriptor so a swap after the lstat cannot change what is chmod-ed.
    let fd = nix::fcntl::open(
        path,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| FleetError::invalid(format!("cannot open {}: {}", path.display(), e.desc())))?;
    let dir = File::from(fd);
    let meta = dir
        .metadata()
        .map_err(|e| FleetError::io(&format!("cannot examine {}", path.display()), &e))?;
    if meta.uid() != nix::unistd::geteuid().as_raw() {
        return Err(FleetError::forbidden(format!(
            "{} is owned by another user",
            path.display()
        )));
    }
    if meta.mode() & 0o077 != 0 {
        dir.set_permissions(fs::Permissions::from_mode(0o700))
            .map_err(|e| FleetError::io(&format!("cannot restrict {}", path.display()), &e))?;
    }
    Ok(true)
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `bytes` to `path` through a 0600 temp file in the same directory and a rename, so a
/// reader sees the old or the new content, never a mix.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Err(FleetError::invalid(format!(
            "{} has no usable file name",
            path.display()
        )));
    };
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!(".{name}.tmp.{}.{n}", std::process::id()));
    let result = (|| -> io::Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(OFlag::O_NOFOLLOW.bits())
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(FleetError::io(
            &format!("cannot write {}", path.display()),
            &e,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{wait_until, TempDir};
    use std::os::unix::fs::symlink;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn mkfifo(path: &Path) {
        nix::unistd::mkfifo(path, Mode::S_IRWXU).unwrap();
    }

    #[test]
    fn open_beneath_opens_nested_files_and_refuses_links_everywhere() {
        let t = TempDir::new("fsx-beneath");
        t.write("root/a/b/c.txt", "deep");
        t.write("outside/secret.txt", "secret");
        let root = t.join("root");
        let read = |rel: &str| open_beneath(&root, rel).and_then(|f| read_open(f, 100));
        assert_eq!(read("a/b/c.txt").unwrap(), b"deep");
        assert_eq!(read("a/b/nope.txt"), Err(ReadError::Missing));
        assert_eq!(read("a/nope/c.txt"), Err(ReadError::Missing));
        assert!(matches!(read("a/b"), Err(ReadError::NotRegular(_))));
        // a final symlink, and a symlink in the middle that leads out of the root
        symlink(t.join("outside/secret.txt"), root.join("a/link.txt")).unwrap();
        symlink(t.join("outside"), root.join("a/linkdir")).unwrap();
        assert_eq!(
            read("a/link.txt"),
            Err(ReadError::NotRegular("is a symlink"))
        );
        assert!(matches!(
            read("a/linkdir/secret.txt"),
            Err(ReadError::NotRegular(_))
        ));
        // a file used as a directory
        assert!(matches!(read("a/b/c.txt/x"), Err(ReadError::NotRegular(_))));
        // a root that is itself a link is refused
        symlink(&root, t.join("rootlink")).unwrap();
        assert!(open_beneath(&t.join("rootlink"), "a/b/c.txt").is_err());
        assert_eq!(
            open_beneath(&t.join("missing-root"), "x").unwrap_err(),
            ReadError::Missing
        );
        // malformed relative paths never reach the file system
        for bad in [
            "",
            "/a",
            "a//b",
            "a/./b",
            "../outside/secret.txt",
            "a/../a",
            "a\0b",
        ] {
            assert!(
                matches!(open_beneath(&root, bad), Err(ReadError::Io(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn open_beneath_does_not_block_on_a_fifo() {
        let t = TempDir::new("fsx-beneath-fifo");
        t.write("root/.keep", "");
        mkfifo(&t.join("root/pipe"));
        let file = open_beneath(&t.join("root"), "pipe").unwrap();
        assert!(matches!(read_open(file, 10), Err(ReadError::NotRegular(_))));
    }

    #[test]
    fn reads_a_regular_file() {
        let t = TempDir::new("fsx-ok");
        let p = t.write("a.txt", "hello");
        assert_eq!(read_regular(&p, 100).unwrap(), b"hello");
        assert_eq!(read_regular_in(t.path(), "a.txt", 100).unwrap(), b"hello");
    }

    #[test]
    fn exactly_max_is_accepted_and_one_more_is_not() {
        let t = TempDir::new("fsx-max");
        let p = t.write("a.txt", "12345");
        assert_eq!(read_regular(&p, 5).unwrap(), b"12345");
        assert_eq!(
            read_regular(&p, 4),
            Err(ReadError::TooLarge { size: 5, max: 4 })
        );
        assert!(matches!(
            read_regular_in(t.path(), "a.txt", 4),
            Err(ReadError::TooLarge { .. })
        ));
        assert_eq!(
            read_regular(&p, 4).unwrap_err().to_string(),
            "is larger than 4 bytes"
        );
    }

    #[test]
    fn missing_files_are_reported_as_missing() {
        let t = TempDir::new("fsx-missing");
        assert_eq!(read_regular(&t.join("nope"), 10), Err(ReadError::Missing));
        assert_eq!(
            read_regular_in(t.path(), "nope", 10),
            Err(ReadError::Missing)
        );
        assert_eq!(
            read_regular_in(&t.join("nodir"), "nope", 10),
            Err(ReadError::Missing)
        );
    }

    #[test]
    fn a_symlink_to_a_huge_file_is_rejected_without_reading() {
        let t = TempDir::new("fsx-link");
        let big = t.join("big");
        let f = File::create(&big).unwrap();
        f.set_len(512 * 1024 * 1024).unwrap();
        symlink(&big, t.join("link")).unwrap();
        let err = read_regular(&t.join("link"), 1024).unwrap_err();
        assert_eq!(err, ReadError::NotRegular("is a symlink"));
        assert_eq!(err.to_string(), "is a symlink");
        let err = read_regular_in(t.path(), "link", 1024).unwrap_err();
        assert_eq!(err, ReadError::NotRegular("is a symlink"));
    }

    #[test]
    fn a_symlink_to_a_directory_is_rejected() {
        let t = TempDir::new("fsx-dirlink");
        fs::create_dir(t.join("d")).unwrap();
        symlink(t.join("d"), t.join("link")).unwrap();
        assert!(matches!(
            read_regular(&t.join("link"), 10),
            Err(ReadError::NotRegular(_))
        ));
        assert!(matches!(
            read_regular_in(t.path(), "link", 10),
            Err(ReadError::NotRegular(_))
        ));
    }

    #[test]
    fn a_directory_is_not_a_regular_file() {
        let t = TempDir::new("fsx-dir");
        fs::create_dir(t.join("d")).unwrap();
        let err = read_regular(&t.join("d"), 10).unwrap_err();
        assert_eq!(err.to_string(), "is not a regular file");
        assert!(matches!(
            read_regular_in(t.path(), "d", 10),
            Err(ReadError::NotRegular(_))
        ));
    }

    #[test]
    fn a_fifo_returns_promptly() {
        let t = TempDir::new("fsx-fifo");
        let p = t.join("pipe");
        mkfifo(&p);
        for via_dir in [false, true] {
            let done = Arc::new(AtomicBool::new(false));
            let flag = done.clone();
            let path = p.clone();
            let dir = t.path().to_path_buf();
            let handle = std::thread::spawn(move || {
                let r = if via_dir {
                    read_regular_in(&dir, "pipe", 100)
                } else {
                    read_regular(&path, 100)
                };
                flag.store(true, Ordering::SeqCst);
                r
            });
            assert!(
                wait_until(3000, || done.load(Ordering::SeqCst)),
                "blocked on a FIFO"
            );
            assert!(matches!(
                handle.join().unwrap(),
                Err(ReadError::NotRegular(_))
            ));
        }
    }

    #[test]
    fn a_swapped_directory_symlink_cannot_redirect_read_regular_in() {
        let t = TempDir::new("fsx-swap");
        let outside = t.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("result.json"), "secret").unwrap();
        // `.fleet` is a symlink to a directory that holds a file with the expected name.
        symlink(&outside, t.join(".fleet")).unwrap();
        let err = read_regular_in(&t.join(".fleet"), "result.json", 100).unwrap_err();
        assert!(matches!(err, ReadError::NotRegular(_)), "{err:?}");
        // A real directory is read normally.
        let t2 = TempDir::new("fsx-swap-ok");
        t2.write(".fleet/result.json", "ok");
        assert_eq!(
            read_regular_in(&t2.join(".fleet"), "result.json", 100).unwrap(),
            b"ok"
        );
    }

    #[test]
    fn bad_names_are_rejected() {
        let t = TempDir::new("fsx-names");
        for name in ["", ".", "..", "a/b", "../x"] {
            assert!(matches!(
                read_regular_in(t.path(), name, 10),
                Err(ReadError::Io(_))
            ));
        }
    }

    #[test]
    fn ensure_private_dir_creates_and_tightens() {
        let t = TempDir::new("fsx-private");
        let d = t.join("a/b/c");
        ensure_private_dir(&d).unwrap();
        assert_eq!(fs::metadata(&d).unwrap().mode() & 0o777, 0o700);
        let open = t.join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&open).unwrap();
        assert_eq!(fs::metadata(&open).unwrap().mode() & 0o777, 0o700);
        // Idempotent.
        ensure_private_dir(&open).unwrap();
    }

    #[test]
    fn ensure_private_dir_refuses_symlinks_and_files() {
        let t = TempDir::new("fsx-private-bad");
        fs::create_dir(t.join("real")).unwrap();
        symlink(t.join("real"), t.join("link")).unwrap();
        let err = ensure_private_dir(&t.join("link")).unwrap_err();
        assert!(err.message.contains("symlink"));
        t.write("file", "x");
        assert!(ensure_private_dir(&t.join("file")).is_err());
        // A dangling symlink is still a symlink and must not be created through.
        symlink(t.join("nowhere"), t.join("dangling")).unwrap();
        assert!(ensure_private_dir(&t.join("dangling")).is_err());
        assert!(!t.exists("nowhere"));
    }

    #[test]
    fn write_private_atomic_replaces_content_with_mode_0600() {
        let t = TempDir::new("fsx-atomic");
        let p = t.join("f");
        write_private_atomic(&p, b"one").unwrap();
        write_private_atomic(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert_eq!(fs::metadata(&p).unwrap().mode() & 0o777, 0o600);
        let leftovers: Vec<_> = fs::read_dir(t.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "temp file must not remain");
    }

    #[test]
    fn write_private_atomic_cleans_up_when_the_target_is_a_directory() {
        let t = TempDir::new("fsx-atomic-dir");
        fs::create_dir(t.join("d")).unwrap();
        assert!(write_private_atomic(&t.join("d"), b"x").is_err());
        assert_eq!(fs::read_dir(t.path()).unwrap().count(), 1);
    }

    #[test]
    fn lstat_kind_and_remove_nofollow_do_not_follow_links() {
        let t = TempDir::new("fsx-kind");
        let target = t.write("target", "keep");
        fs::create_dir(t.join("d")).unwrap();
        symlink(&target, t.join("link")).unwrap();
        mkfifo(&t.join("pipe"));
        assert_eq!(lstat_kind(&target), Some(FileKind::File));
        assert_eq!(lstat_kind(&t.join("d")), Some(FileKind::Dir));
        assert_eq!(lstat_kind(&t.join("link")), Some(FileKind::Symlink));
        assert_eq!(lstat_kind(&t.join("pipe")), Some(FileKind::Other));
        assert_eq!(lstat_kind(&t.join("none")), None);
        assert!(is_real_dir(&t.join("d")));
        assert!(!is_real_dir(&t.join("link")));
        remove_nofollow(&t.join("link")).unwrap();
        remove_nofollow(&t.join("pipe")).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"keep");
        assert!(remove_nofollow(&t.join("d")).is_err());
        assert!(t.exists("d"));
    }
}
