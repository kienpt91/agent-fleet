//! The local half of the remote worktree protocol: what goes out and what comes back.
//!
//! *Out:* a root commit that has exactly the tree of the task branch's head (no history, no other
//! branch) is made in the repository, kept under a quarantine ref and written to a git bundle.
//! *Back:* the remote's work arrives as another bundle. Nothing in it is believed: it is listed,
//! fetched with object checks into a quarantine ref, and must descend from the commit that was
//! sent, touch no `.git` or `.fleet` path, stay within the size and file-count limits and leave
//! the local worktree untouched since it was exported. Only its *tree* is used: the task branch
//! gets one new local commit, authored by fleet, with that tree on top of the old head. The remote's
//! commit objects, messages and authors are never part of local history.

use super::*;

/// Most commits a returned bundle may add on top of what was sent.
pub const MAX_REMOTE_COMMITS: u32 = 10_000;
/// Most files in the returned tree.
pub const MAX_REMOTE_FILES: usize = 100_000;
/// Most bytes of file content in the returned tree.
pub const MAX_REMOTE_TREE_BYTES: u64 = 1024 * 1024 * 1024;
/// Most paths the returned work may change.
pub const MAX_REMOTE_CHANGES: usize = 50_000;

/// The ref the remote must name in the bundle it returns.
pub const RETURN_REF: &str = "refs/heads/fleet-return";

fn quarantine(run: &RunId, task: &TaskId, leaf: &str) -> String {
    format!(
        "refs/fleet-remote/{}/{}/{leaf}",
        run.as_str(),
        task.as_str()
    )
}

/// What to import: the task, what was sent for it and the bundle that came back.
#[derive(Clone, Copy, Debug)]
pub struct ImportRequest<'a> {
    pub worktree: &'a str,
    pub run: &'a RunId,
    pub task: &'a TaskId,
    pub sent: &'a Sha,
    pub bundle: &'a Path,
    /// The host's name, for the commit message.
    pub host: &'a str,
}

/// What a verified import changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Imported {
    /// The new head of the task branch, when the work changed the tree.
    pub head: Option<Sha>,
    pub commits: u32,
    pub changed_paths: usize,
    pub tree_bytes: u64,
}

fn is_hostile_path(path: &[u8]) -> bool {
    let lower = String::from_utf8_lossy(path).to_ascii_lowercase();
    lower
        .split('/')
        .any(|c| c == ".git" || c.trim_end_matches([' ', '.']) == ".git" || c.starts_with(".git~"))
        || lower == ".fleet"
        || lower.starts_with(".fleet/")
}

impl GitVcs {
    /// Writes the bundle that carries the task branch's head tree to a remote host and returns the
    /// commit it holds. `bundle` must not exist; the bundle is refused when larger than `max_bytes`.
    pub fn export_for_remote(
        &self,
        worktree: &str,
        run: &RunId,
        task: &TaskId,
        identity: &Identity,
        bundle: &Path,
        max_bytes: u64,
    ) -> Result<Sha> {
        validate_identity(identity)?;
        let w = self.check_worktree(worktree)?;
        let branch = self.task_branch_of(&w)?;
        let head = self.real_branch_head(&w.repo, &branch)?;
        let tree = self.real_tree_of(&w.repo, &head)?;
        let sent_ref = quarantine(run, task, "sent");
        self.delete_ref(&w.repo, &sent_ref)?;
        let message = format!("fleet: remote export of {}/{}", run.as_str(), task.as_str());
        let made = self.run_ok(
            &w.repo,
            Some(identity),
            &["commit-tree", tree.as_str(), "-m", &message],
        )?;
        let sent = Sha::parse(made.text().trim())?;
        self.run_ok(&w.repo, None, &["update-ref", &sent_ref, sent.as_str()])?;
        let target = path_str(bundle)?;
        let result = self
            .run_ok(&w.repo, None, &["bundle", "create", "--quiet", target, &sent_ref])
            .and_then(|_| {
                let size = fs::metadata(bundle)
                    .map_err(|e| FleetError::io("cannot examine the bundle", &e))?
                    .len();
                if size > max_bytes {
                    Err(FleetError::limit(format!(
                        "the repository's files make a bundle of {size} bytes, more than the {max_bytes} this host allows (max_bundle_mib)"
                    )))
                } else {
                    Ok(())
                }
            });
        if let Err(e) = result {
            let _ = fs::remove_file(bundle);
            let _ = self.delete_ref(&w.repo, &sent_ref);
            return Err(e);
        }
        Ok(sent)
    }

    fn delete_ref(&self, repo: &Path, full_ref: &str) -> Result<()> {
        if self.ref_exists(repo, full_ref)? {
            self.run_ok(repo, None, &["update-ref", "-d", full_ref])?;
        }
        Ok(())
    }

    /// Removes the quarantine refs of a task (idempotent; best effort for the caller).
    pub fn drop_remote_refs(&self, worktree: &str, run: &RunId, task: &TaskId) -> Result<()> {
        let w = self.check_worktree(worktree)?;
        for leaf in ["sent", "return"] {
            self.delete_ref(&w.repo, &quarantine(run, task, leaf))?;
        }
        Ok(())
    }

    /// Verifies the bundle a remote returned and turns its tree into the task branch's next commit.
    /// See the module comment for the checks. On any failure the branch and the worktree are as
    /// they were.
    pub fn import_from_remote(
        &self,
        req: &ImportRequest<'_>,
        identity: &Identity,
    ) -> Result<Imported> {
        let ImportRequest {
            worktree,
            run,
            task,
            sent,
            bundle,
            host,
        } = *req;
        validate_identity(identity)?;
        let w = self.check_worktree(worktree)?;
        let branch = self.task_branch_of(&w)?;
        let old = self.real_branch_head(&w.repo, &branch)?;
        let old_tree = self.real_tree_of(&w.repo, &old)?;
        let sent_ref = quarantine(run, task, "sent");
        let return_ref = quarantine(run, task, "return");
        // What was sent is still what the branch holds, and nothing was touched locally since.
        let sent_tree = self.real_tree_of(&w.repo, sent).map_err(|_| {
            FleetError::conflict("the commit that was sent is no longer in the repository")
        })?;
        if sent_tree != old_tree {
            return Err(FleetError::conflict(
                "the task branch changed while the remote worker ran; its work was not applied",
            ));
        }
        self.require_clean(&w)?;

        let path = path_str(bundle)?;
        // The bundle names exactly one ref, the one the protocol uses.
        let heads = self.run_ok(&w.repo, None, &["bundle", "list-heads", path])?;
        let text = heads.text();
        let mut lines = text.lines();
        let (Some(line), None) = (lines.next(), lines.next()) else {
            return Err(FleetError::forbidden(
                "the remote's bundle does not name exactly one ref; refused",
            ));
        };
        let mut parts = line.split(' ');
        let (Some(claimed), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
            return Err(FleetError::forbidden(
                "the remote's bundle header is not understood",
            ));
        };
        if name != RETURN_REF {
            return Err(FleetError::forbidden(format!(
                "the remote's bundle names {} instead of {RETURN_REF}; refused",
                safe_text(name, 80)
            )));
        }
        let claimed = Sha::parse(claimed)
            .map_err(|_| FleetError::forbidden("the remote's bundle names no valid commit"))?;

        self.delete_ref(&w.repo, &return_ref)?;
        let outcome = self.verify_and_apply(
            &w,
            &branch,
            &old,
            &old_tree,
            sent,
            &claimed,
            path,
            &return_ref,
            identity,
            host,
        );
        let _ = self.delete_ref(&w.repo, &return_ref);
        let _ = self.delete_ref(&w.repo, &sent_ref);
        outcome
    }

    fn require_clean(&self, w: &Worktree) -> Result<()> {
        let status = self.run_in_ok(
            w,
            None,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--no-renames",
                "--untracked-files=normal",
                "--ignore-submodules=none",
            ],
        )?;
        let dirty = status
            .stdout
            .split(|b| *b == 0)
            .filter(|entry| entry.len() > 3)
            .any(|entry| {
                let path = entry.get(3..).unwrap_or_default();
                path != b".fleet" && !path.starts_with(b".fleet/")
            });
        if dirty {
            return Err(FleetError::conflict(
                "the local worktree has changes that are not committed; the remote work was not applied over them",
            ));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn verify_and_apply(
        &self,
        w: &Worktree,
        branch: &str,
        old: &Sha,
        old_tree: &Sha,
        sent: &Sha,
        claimed: &Sha,
        bundle: &str,
        return_ref: &str,
        identity: &Identity,
        host: &str,
    ) -> Result<Imported> {
        let refspec = format!("+{RETURN_REF}:{return_ref}");
        self.run_ok(
            &w.repo,
            None,
            &[
                // The bundle is a file of ours that this call names: the one source allowed.
                // (Git does not check the objects of a bundle when it fetches from one; the
                // `fsck` below does.)
                "-c",
                "protocol.file.allow=always",
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-write-fetch-head",
                "--no-recurse-submodules",
                bundle,
                &refspec,
            ],
        )
        .map_err(|e| {
            FleetError::forbidden(format!(
                "the remote's bundle was refused by git: {}",
                e.message
            ))
        })?;
        let tip = {
            let spec = format!("{return_ref}^{{commit}}");
            let out = self.run_ok(
                &w.repo,
                None,
                &["rev-parse", "--verify", "--end-of-options", &spec],
            )?;
            Sha::parse(out.text().trim())?
        };
        if tip != *claimed {
            return Err(FleetError::forbidden(
                "the commit the bundle delivered is not the one its header named; refused",
            ));
        }
        // Every object reachable from the tip is read and checked the way a push would check it
        // (a `.git` entry in a tree, malformed objects). That is the tree that was sent plus what
        // the remote added, not the repository's history.
        let fsck = self.run(
            &w.repo,
            None,
            &[
                "fsck",
                "--strict",
                "--no-dangling",
                "--no-progress",
                tip.as_str(),
            ],
        )?;
        if fsck.code != Some(0) {
            return Err(FleetError::forbidden(format!(
                "the remote's objects failed git's checks: {}",
                safe_text(String::from_utf8_lossy(&fsck.stderr).trim(), 160)
            )));
        }
        // It builds on what was sent: unrelated history and rewritten history are not that.
        let ancestry = self.run(
            &w.repo,
            None,
            &["merge-base", "--is-ancestor", sent.as_str(), tip.as_str()],
        )?;
        if ancestry.code != Some(0) {
            return Err(FleetError::forbidden(
                "the remote's work does not build on the commit that was sent; refused",
            ));
        }
        let range = format!("{}..{}", sent.as_str(), tip.as_str());
        let count = self.run_ok(&w.repo, None, &["rev-list", "--count", &range])?;
        let commits: u32 = count.text().trim().parse().unwrap_or(u32::MAX);
        if commits > MAX_REMOTE_COMMITS {
            return Err(FleetError::limit(format!(
                "the remote's work has more than {MAX_REMOTE_COMMITS} commits"
            )));
        }
        // Paths it touched.
        let diff = self.run_ok(
            &w.repo,
            None,
            &[
                "diff-tree",
                "-r",
                "--name-only",
                "-z",
                "--no-renames",
                "--no-ext-diff",
                sent.as_str(),
                tip.as_str(),
                "--",
            ],
        )?;
        let changed: Vec<&[u8]> = diff
            .stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .collect();
        if changed.len() > MAX_REMOTE_CHANGES {
            return Err(FleetError::limit(format!(
                "the remote's work changes more than {MAX_REMOTE_CHANGES} paths"
            )));
        }
        if let Some(bad) = changed.iter().find(|p| is_hostile_path(p)) {
            return Err(FleetError::forbidden(format!(
                "the remote's work touches {}, which a task may not change; refused",
                safe_text(&String::from_utf8_lossy(bad), 80)
            )));
        }
        // Size of what a checkout would write.
        let listing = self.run_ok(&w.repo, None, &["ls-tree", "-r", "-l", "-z", tip.as_str()])?;
        let mut files = 0usize;
        let mut bytes = 0u64;
        for entry in listing.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            files += 1;
            let meta = entry.split(|b| *b == b'\t').next().unwrap_or_default();
            let size = String::from_utf8_lossy(meta)
                .split_whitespace()
                .nth(3)
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            bytes = bytes.saturating_add(size);
        }
        if files > MAX_REMOTE_FILES || bytes > MAX_REMOTE_TREE_BYTES {
            return Err(FleetError::limit(format!(
                "the remote's tree has {files} files and {bytes} bytes, more than the limits ({MAX_REMOTE_FILES} files, {MAX_REMOTE_TREE_BYTES} bytes)"
            )));
        }
        let tree = self.real_tree_of(&w.repo, &tip)?;
        if tree == *old_tree {
            return Ok(Imported {
                head: None,
                commits,
                changed_paths: changed.len(),
                tree_bytes: bytes,
            });
        }
        // One local commit with the verified tree on top of the old head.
        let message = clean_message(
            &format!("fleet: changes made on host {host}"),
            "fleet: remote task changes",
        );
        let made = self.run_ok(
            &w.repo,
            Some(identity),
            &[
                "commit-tree",
                tree.as_str(),
                "-p",
                old.as_str(),
                "-m",
                &message,
            ],
        )?;
        let new = Sha::parse(made.text().trim())?;
        let full = format!("refs/heads/{branch}");
        self.run_ok(
            &w.repo,
            None,
            &[
                "update-ref",
                "-m",
                "fleet: remote work",
                &full,
                new.as_str(),
                old.as_str(),
            ],
        )?;
        let applied = self.run_in_ok(
            w,
            None,
            &["reset", "--hard", "-q", "--no-recurse-submodules"],
        );
        if let Err(e) = applied {
            // Put the branch back so that nothing half-applied is merged later.
            let _ = self.run_ok(
                &w.repo,
                None,
                &["update-ref", &full, old.as_str(), new.as_str()],
            );
            return Err(e);
        }
        Ok(Imported {
            head: Some(new),
            commits,
            changed_paths: changed.len(),
            tree_bytes: bytes,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::testutil::{git, git_available, make_repo, TempDir};
    use fleet_domain::env::EnvPolicy;

    fn ident() -> Identity {
        Identity {
            name: "fleet".into(),
            email: "fleet@localhost".into(),
        }
    }

    struct Fx {
        dir: TempDir,
        vcs: GitVcs,
        repo: PathBuf,
        wt: String,
        run: RunId,
        task: TaskId,
    }

    impl Fx {
        fn new() -> Option<Fx> {
            if !git_available() {
                return None;
            }
            let dir = TempDir::new("remote-sync");
            let repo = make_repo(&dir);
            let vcs = GitVcs::new(
                vec![(
                    "PATH".to_string(),
                    std::env::var("PATH").unwrap_or_default(),
                )],
                EnvPolicy::default_policy(),
            );
            let run = RunId::parse("r1").unwrap();
            let task = TaskId::parse("t1").unwrap();
            let base = vcs.resolve_base(repo.to_str().unwrap(), "HEAD").unwrap();
            let wt = vcs
                .prepare_worktree(
                    repo.to_str().unwrap(),
                    &run,
                    &task,
                    &task_branch("r1", "t1"),
                    &base.sha,
                )
                .unwrap();
            fs::create_dir_all(format!("{wt}/.fleet")).unwrap();
            Some(Fx {
                dir,
                vcs,
                repo,
                wt,
                run,
                task,
            })
        }

        fn export(&self) -> (Sha, PathBuf) {
            let bundle = self.dir.join("out.bundle");
            let _ = fs::remove_file(&bundle);
            let sent = self
                .vcs
                .export_for_remote(&self.wt, &self.run, &self.task, &ident(), &bundle, 1 << 30)
                .unwrap();
            (sent, bundle)
        }

        /// What a remote host does: unpack the sent bundle into a fresh repository, let `work`
        /// change it, and bundle the result the way the protocol says.
        fn remote(&self, sent_bundle: &Path, sent: &Sha, work: impl FnOnce(&Path)) -> PathBuf {
            let remote = self.dir.join("remote");
            let _ = fs::remove_dir_all(&remote);
            fs::create_dir_all(&remote).unwrap();
            git(&remote, &["init", "-q", "."]);
            git(
                &remote,
                &[
                    "fetch",
                    "-q",
                    "--no-tags",
                    sent_bundle.to_str().unwrap(),
                    "+refs/fleet-remote/*:refs/fleet-in/*",
                ],
            );
            git(&remote, &["checkout", "-q", "-b", "work", sent.as_str()]);
            work(&remote);
            let out = self.dir.join("back.bundle");
            let _ = fs::remove_file(&out);
            git(&remote, &["update-ref", "refs/heads/fleet-return", "HEAD"]);
            git(
                &remote,
                &[
                    "bundle",
                    "create",
                    "-q",
                    out.to_str().unwrap(),
                    "refs/heads/fleet-return",
                    &format!("^{}", sent.as_str()),
                ],
            );
            out
        }

        fn import(&self, sent: &Sha, bundle: &Path) -> Result<Imported> {
            self.vcs.import_from_remote(
                &ImportRequest {
                    worktree: &self.wt,
                    run: &self.run,
                    task: &self.task,
                    sent,
                    bundle,
                    host: "build",
                },
                &ident(),
            )
        }

        fn head(&self) -> String {
            git(Path::new(&self.wt), &["rev-parse", "HEAD"])
        }

        fn refs(&self) -> String {
            git(&self.repo, &["for-each-ref", "refs/fleet-remote"])
        }
    }

    fn commit_file(dir: &Path, name: &str, content: &str) {
        if let Some(parent) = Path::new(name).parent() {
            fs::create_dir_all(dir.join(parent)).unwrap();
        }
        fs::write(dir.join(name), content).unwrap();
        git(dir, &["add", "-f", "-A"]);
        git(dir, &["commit", "-q", "-m", "work"]);
    }

    #[test]
    fn the_export_is_one_root_commit_with_the_head_tree_and_survives_until_the_import() {
        let Some(fx) = Fx::new() else { return };
        let (sent, bundle) = fx.export();
        assert!(bundle.is_file());
        let root = git(&fx.repo, &["rev-list", "--max-parents=0", sent.as_str()]);
        assert_eq!(root, sent.as_str(), "no parent");
        assert_eq!(
            git(
                &fx.repo,
                &["rev-parse", &format!("{}^{{tree}}", sent.as_str())]
            ),
            git(Path::new(&fx.wt), &["rev-parse", "HEAD^{tree}"])
        );
        assert!(fx.refs().contains("refs/fleet-remote/r1/t1/sent"));
        // The bundle holds that one commit and nothing of the repository's history or branches.
        let heads = git(
            &fx.repo,
            &["bundle", "list-heads", bundle.to_str().unwrap()],
        );
        assert_eq!(heads.lines().count(), 1, "{heads}");
        fx.vcs.drop_remote_refs(&fx.wt, &fx.run, &fx.task).unwrap();
        assert_eq!(fx.refs(), "");
        fx.vcs.drop_remote_refs(&fx.wt, &fx.run, &fx.task).unwrap();
    }

    #[test]
    fn a_too_large_export_is_refused_and_leaves_nothing() {
        let Some(fx) = Fx::new() else { return };
        let bundle = fx.dir.join("big.bundle");
        let err = fx
            .vcs
            .export_for_remote(&fx.wt, &fx.run, &fx.task, &ident(), &bundle, 10)
            .unwrap_err();
        assert!(err.message.contains("bundle"), "{}", err.message);
        assert!(!bundle.exists());
        assert_eq!(fx.refs(), "");
    }

    #[test]
    fn honest_work_becomes_one_local_commit_with_the_returned_tree() {
        let Some(fx) = Fx::new() else { return };
        let before = fx.head();
        let (sent, bundle) = fx.export();
        let back = fx.remote(&bundle, &sent, |r| {
            commit_file(r, "added.txt", "new\n");
            commit_file(r, "src/lib.txt", "changed\n");
        });
        let got = fx.import(&sent, &back).unwrap();
        assert_eq!(got.commits, 2);
        assert_eq!(got.changed_paths, 2);
        assert_eq!(got.head.as_ref().unwrap().as_str(), fx.head());
        assert_eq!(git(Path::new(&fx.wt), &["rev-parse", "HEAD~1"]), before);
        assert_eq!(
            git(Path::new(&fx.wt), &["log", "-1", "--format=%an <%ae>"]),
            "fleet <fleet@localhost>"
        );
        assert_eq!(
            fs::read_to_string(format!("{}/added.txt", fx.wt)).unwrap(),
            "new\n"
        );
        assert_eq!(git(Path::new(&fx.wt), &["status", "--porcelain"]), "");
        assert_eq!(fx.refs(), "", "the quarantine refs are gone");
    }

    #[test]
    fn work_that_leaves_the_files_as_they_were_changes_nothing() {
        let Some(fx) = Fx::new() else { return };
        let before = fx.head();
        let (sent, bundle) = fx.export();
        let back = fx.remote(&bundle, &sent, |r| {
            git(r, &["commit", "-q", "--allow-empty", "-m", "nothing"]);
        });
        let got = fx.import(&sent, &back).unwrap();
        assert_eq!(got.head, None);
        assert_eq!(fx.head(), before);
    }

    #[test]
    fn a_bundle_that_names_another_ref_or_several_is_refused() {
        let Some(fx) = Fx::new() else { return };
        let (sent, bundle) = fx.export();
        let back = fx.remote(&bundle, &sent, |r| commit_file(r, "a.txt", "a\n"));
        // Another name for the same commit.
        let remote = fx.dir.join("remote");
        let other = fx.dir.join("other.bundle");
        git(&remote, &["update-ref", "refs/heads/elsewhere", "HEAD"]);
        git(
            &remote,
            &[
                "bundle",
                "create",
                "-q",
                other.to_str().unwrap(),
                "refs/heads/elsewhere",
                &format!("^{}", sent.as_str()),
            ],
        );
        let err = fx.import(&sent, &other).unwrap_err();
        assert!(err.message.contains("instead of"), "{}", err.message);
        let two = fx.dir.join("two.bundle");
        git(
            &remote,
            &[
                "bundle",
                "create",
                "-q",
                two.to_str().unwrap(),
                "refs/heads/fleet-return",
                "refs/heads/elsewhere",
                &format!("^{}", sent.as_str()),
            ],
        );
        let err = fx.import(&sent, &two).unwrap_err();
        assert!(err.message.contains("exactly one ref"), "{}", err.message);
        // The honest one still works afterwards.
        assert!(fx.import(&sent, &back).unwrap().head.is_some());
    }

    #[test]
    fn history_that_does_not_build_on_what_was_sent_is_refused() {
        let Some(fx) = Fx::new() else { return };
        let before = fx.head();
        let (sent, bundle) = fx.export();
        let back = fx.remote(&bundle, &sent, |r| {
            git(r, &["checkout", "-q", "--orphan", "other"]);
            git(r, &["rm", "-rfq", "."]);
            commit_file(r, "evil.txt", "evil\n");
            git(r, &["update-ref", "refs/heads/work", "HEAD"]);
        });
        let err = fx.import(&sent, &back).unwrap_err();
        assert!(err.message.contains("does not build on"), "{}", err.message);
        assert_eq!(fx.head(), before);
        assert!(!Path::new(&fx.wt).join("evil.txt").exists());
        assert_eq!(fx.refs(), "");
    }

    #[test]
    fn paths_under_fleet_or_named_git_are_refused() {
        for bad in [
            ".fleet/x",
            ".FLEET/x",
            ".fleet",
            "a/.git/config",
            ".GIT/x",
            ".git ",
            "sub/.git.",
        ] {
            assert!(is_hostile_path(bad.as_bytes()), "{bad}");
        }
        for ok in [
            ".gitignore",
            ".github/workflows/x",
            "fleet/x",
            "a/.fleeting",
            "x.git",
        ] {
            assert!(!is_hostile_path(ok.as_bytes()), "{ok}");
        }
        let Some(fx) = Fx::new() else { return };
        let (sent, bundle) = fx.export();
        let back = fx.remote(&bundle, &sent, |r| {
            commit_file(r, ".fleet/result.json", "{}")
        });
        let err = fx.import(&sent, &back).unwrap_err();
        assert!(err.message.contains("touches"), "{}", err.message);
    }

    #[test]
    fn a_tree_with_a_dot_git_entry_is_refused_by_git_itself() {
        let Some(fx) = Fx::new() else { return };
        let (sent, bundle) = fx.export();
        let back = fx.remote(&bundle, &sent, |r| {
            // A tree that git's own commands will not build (`update-index` refuses `.git`), made by hand.
            let blob = git(r, &["hash-object", "-w", "--stdin"]);
            let _ = blob;
            let empty = git(r, &["hash-object", "-t", "tree", "-w", "--stdin"]);
            let tree_input = format!("040000 tree {empty}\t.git\n");
            let tree = {
                let mut c = std::process::Command::new("git");
                c.args(["mktree", "--missing"])
                    .current_dir(r)
                    .env("GIT_CONFIG_GLOBAL", "/dev/null");
                use std::io::Write;
                let mut child = c
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(tree_input.as_bytes())
                    .unwrap();
                String::from_utf8(child.wait_with_output().unwrap().stdout)
                    .unwrap()
                    .trim()
                    .to_string()
            };
            let commit = git(
                r,
                &[
                    "-c",
                    "user.name=x",
                    "-c",
                    "user.email=x@x",
                    "commit-tree",
                    &tree,
                    "-p",
                    "HEAD",
                    "-m",
                    "x",
                ],
            );
            git(r, &["update-ref", "HEAD", &commit]);
        });
        let err = fx.import(&sent, &back).unwrap_err();
        assert!(
            err.message.contains("failed git's checks"),
            "{}",
            err.message
        );
        assert!(err.message.contains("hasDotgit"), "{}", err.message);
        assert_eq!(fx.refs(), "");
    }

    #[test]
    fn work_is_not_applied_over_a_branch_that_moved_or_a_worktree_that_changed() {
        let Some(fx) = Fx::new() else { return };
        let (sent, bundle) = fx.export();
        let back = fx.remote(&bundle, &sent, |r| commit_file(r, "a.txt", "a\n"));
        // Local uncommitted change.
        fs::write(format!("{}/local.txt", fx.wt), "mine\n").unwrap();
        let err = fx.import(&sent, &back).unwrap_err();
        assert!(err.message.contains("not committed"), "{}", err.message);
        fs::remove_file(format!("{}/local.txt", fx.wt)).unwrap();
        // The branch moved.
        git(
            Path::new(&fx.wt),
            &[
                "-c",
                "user.name=x",
                "-c",
                "user.email=x@x",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "moved",
            ],
        );
        commit_file(Path::new(&fx.wt), "moved.txt", "m\n");
        let err = fx.import(&sent, &back).unwrap_err();
        assert!(
            err.message.contains("changed while the remote worker ran"),
            "{}",
            err.message
        );
    }

    #[test]
    fn a_bundle_with_a_missing_prerequisite_or_garbage_is_refused() {
        let Some(fx) = Fx::new() else { return };
        let (sent, bundle) = fx.export();
        let garbage = fx.dir.join("garbage.bundle");
        fs::write(&garbage, "not a bundle at all").unwrap();
        assert!(fx.import(&sent, &garbage).is_err());
        // A bundle made against a commit this repository does not have.
        let back = fx.remote(&bundle, &sent, |r| {
            commit_file(r, "a.txt", "a\n");
            commit_file(r, "b.txt", "b\n");
        });
        let remote = fx.dir.join("remote");
        let prereq = git(&remote, &["rev-parse", "HEAD~1"]);
        let odd = fx.dir.join("odd.bundle");
        git(
            &remote,
            &[
                "bundle",
                "create",
                "-q",
                odd.to_str().unwrap(),
                "refs/heads/fleet-return",
                &format!("^{prereq}"),
            ],
        );
        let err = fx.import(&sent, &odd).unwrap_err();
        assert!(err.message.contains("refused"), "{}", err.message);
        assert!(fx.import(&sent, &back).is_ok());
    }
}
