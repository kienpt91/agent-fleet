//! Finding the `ssh` program once, as an absolute path nobody who can write in a repository
//! controls.

use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::show;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The directory name of fleet's worktrees below a repository.
const WORKTREES: &str = ".fleet-worktrees";

/// Resolves `name` from the `PATH` text `path_env`: only absolute entries count (an empty or
/// relative entry would mean "the current directory", which a worker may control), and what is
/// found is refused when
/// * it is not a regular, executable file,
/// * it, or the directory it is in, is writable by group or others, or owned by someone other
///   than root or the current user,
/// * it lies inside a git work tree (a directory with a `.git` entry among its ancestors) or below
///   a `.fleet-worktrees` directory, where a worker's files live.
///
/// Returns the canonical path.
pub fn resolve_tool(name: &str, path_env: &str) -> Result<PathBuf> {
    let mut refusal: Option<String> = None;
    for dir in path_env.split(':') {
        let dir = Path::new(dir);
        if !dir.is_absolute() {
            continue;
        }
        let candidate = dir.join(name);
        if !fs::metadata(&candidate).is_ok_and(|m| m.is_file()) {
            continue;
        }
        match check_tool(&candidate) {
            Ok(path) => return Ok(path),
            Err(why) => refusal = refusal.or(Some(why)),
        }
    }
    Err(FleetError::unavailable(match refusal {
        Some(why) => format!("{name} was refused: {why}"),
        None => format!("{name} was not found in an absolute entry of PATH"),
    }))
}

/// [`resolve_tool`] for a path given in the configuration: it must be absolute and pass the same
/// checks.
pub fn check_configured_tool(path: &str) -> Result<PathBuf> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(FleetError::invalid(format!(
            "the configured program {} must be an absolute path",
            show(path)
        )));
    }
    check_tool(p).map_err(|why| FleetError::unavailable(format!("{}: {why}", show(path))))
}

fn check_tool(candidate: &Path) -> std::result::Result<PathBuf, String> {
    let canonical = fs::canonicalize(candidate).map_err(|e| format!("cannot resolve it ({e})"))?;
    let meta = fs::metadata(&canonical).map_err(|e| format!("cannot examine it ({e})"))?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err("it is not an executable file".to_string());
    }
    let me = nix::unistd::geteuid().as_raw();
    let trusted_owner = |uid: u32| uid == 0 || uid == me;
    if meta.permissions().mode() & 0o022 != 0 || !trusted_owner(meta.uid()) {
        return Err(format!(
            "{} is writable by others or owned by another user",
            canonical.display()
        ));
    }
    if let Some(dir) = canonical.parent() {
        let dmeta = fs::metadata(dir).map_err(|e| format!("cannot examine its directory ({e})"))?;
        // A sticky directory (like /tmp) is not trusted either: anyone can plant a file there.
        if dmeta.permissions().mode() & 0o022 != 0 || !trusted_owner(dmeta.uid()) {
            return Err(format!(
                "its directory {} is writable by others or owned by another user",
                dir.display()
            ));
        }
    }
    for ancestor in canonical.ancestors().skip(1) {
        if ancestor.file_name().is_some_and(|n| n == WORKTREES) {
            return Err("it lies inside a fleet worktree directory".to_string());
        }
        if fs::symlink_metadata(ancestor.join(".git")).is_ok() {
            return Err(format!(
                "it lies inside the git work tree {}",
                ancestor.display()
            ));
        }
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::os::unix::fs::symlink;

    fn tool_in(dir: &TempDir, rel: &str) -> PathBuf {
        let p = dir.script(rel, "#!/bin/sh\nexit 0\n");
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn private(dir: &TempDir, rel: &str) {
        fs::create_dir_all(dir.join(rel)).unwrap();
        fs::set_permissions(dir.join(rel), fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_tool_in_a_trusted_absolute_entry_is_found_and_made_canonical() {
        let dir = TempDir::new("tool");
        private(&dir, "bin");
        let real = tool_in(&dir, "bin/ssh");
        let found = resolve_tool(
            "ssh",
            &format!("relative/bin::{}", dir.join("bin").display()),
        )
        .unwrap();
        assert_eq!(found, real);
        let link = dir.join("bin/ssh-link");
        symlink(&real, &link).unwrap();
        assert_eq!(
            resolve_tool("ssh-link", &dir.join("bin").display().to_string()).unwrap(),
            real
        );
    }

    #[test]
    fn relative_and_empty_entries_are_never_searched() {
        let dir = TempDir::new("tool-rel");
        private(&dir, "bin");
        tool_in(&dir, "bin/ssh");
        for path in ["bin", ".", "", ":", "./bin:bin"] {
            assert!(resolve_tool("ssh", path).is_err(), "{path:?}");
        }
    }

    #[test]
    fn a_tool_inside_a_repository_or_a_worktree_directory_is_refused() {
        let dir = TempDir::new("tool-repo");
        fs::create_dir_all(dir.join("repo/.git")).unwrap();
        private(&dir, "repo/bin");
        tool_in(&dir, "repo/bin/ssh");
        let err = resolve_tool("ssh", &dir.join("repo/bin").display().to_string()).unwrap_err();
        assert!(err.message.contains("git work tree"), "{}", err.message);
        private(&dir, "x/.fleet-worktrees/r/t");
        tool_in(&dir, "x/.fleet-worktrees/r/t/ssh");
        let err = resolve_tool(
            "ssh",
            &dir.join("x/.fleet-worktrees/r/t").display().to_string(),
        )
        .unwrap_err();
        assert!(err.message.contains("worktree"), "{}", err.message);
    }

    #[test]
    fn a_writable_tool_or_directory_is_refused() {
        let dir = TempDir::new("tool-w");
        private(&dir, "bin");
        let p = tool_in(&dir, "bin/ssh");
        fs::set_permissions(&p, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(resolve_tool("ssh", &dir.join("bin").display().to_string()).is_err());
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(dir.join("bin"), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(resolve_tool("ssh", &dir.join("bin").display().to_string()).is_err());
    }

    #[test]
    fn a_missing_or_non_executable_tool_is_unavailable() {
        let dir = TempDir::new("tool-none");
        private(&dir, "bin");
        let err = resolve_tool("ssh", &dir.join("bin").display().to_string()).unwrap_err();
        assert!(err.message.contains("not found"), "{}", err.message);
        let p = dir.write("bin/ssh", "data");
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(resolve_tool("ssh", &dir.join("bin").display().to_string()).is_err());
    }

    #[test]
    fn a_configured_path_must_be_absolute_and_pass_the_same_checks() {
        assert!(check_configured_tool("ssh").is_err());
        assert!(check_configured_tool("./ssh").is_err());
        let dir = TempDir::new("tool-cfg");
        private(&dir, "bin");
        let p = tool_in(&dir, "bin/ssh");
        assert_eq!(check_configured_tool(&p.display().to_string()).unwrap(), p);
        assert!(check_configured_tool(&dir.join("bin/none").display().to_string()).is_err());
    }
}
