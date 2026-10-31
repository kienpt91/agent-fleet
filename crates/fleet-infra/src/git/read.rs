//! Reading the real repository at recorded commits, for the read-only workbench.
//!
//! Everything runs through the same hardened runner as every other git call of the daemon (empty
//! environment, fixed configuration, no hooks, filters, textconv, external diff, pager or replace
//! objects) in the repository's own directory, never in a worktree and never through a worktree's
//! `.git` file: what a worker can write is not consulted (ADR 16). Paths reach git only after
//! the domain validated them, as the literal `<commit>:<path>` of `ls-tree` or after `--` with
//! literal pathspecs, and file contents are read by object id after their size was checked.

use super::{Cap, GitVcs};
use fleet_app::ports::{DiffRequest, DirListing, EntryRead, EntryStat, RepoReader};
use fleet_domain::error::{FleetError, Result};
use fleet_domain::ids::{show, Sha};
use fleet_domain::repo::diff::{
    assemble_file, parse_file_patch, split_patch, DiffBudget, DiffFile, DiffSet, FileChange,
    FilePatch, NumStat, PatchShape,
};
use fleet_domain::repo::limits::RENAME_LIMIT;
use fleet_domain::repo::tree::{parse_mode, EntryKind, ModeClass, TreeEntry};
use fleet_domain::repo::RepoPath;
use std::ffi::OsStr;
use std::path::Path;

/// Bytes of `ls-tree` output read for one directory (about 4000 entries).
const LISTING_BYTES: usize = 512 * 1024;
/// Bytes of the raw and numstat listings of a diff.
const CHANGE_LIST_BYTES: usize = 2 * 1024 * 1024;
/// Bytes of patch text read for a diff; the domain budget is lower, this bounds git's output.
const PATCH_BYTES: usize = 8 * 1024 * 1024;
/// Ordinary replies (a size, a verification) are tiny.
const SMALL_BYTES: usize = 4096;

/// What `git ls-tree -z -l` says about one entry.
struct LsEntry {
    mode: u32,
    oid: String,
    size: Option<u64>,
    name: Vec<u8>,
}

/// The records of `ls-tree -z -l` output. A last record that was cut off is dropped.
fn parse_ls_tree(out: &[u8]) -> Vec<LsEntry> {
    let mut entries = Vec::new();
    for record in out.split_inclusive(|b| *b == 0) {
        let Some(record) = record.strip_suffix(&[0]) else {
            break;
        };
        let Some(tab) = record.iter().position(|b| *b == b'\t') else {
            continue;
        };
        let (Some(header), Some(name)) = (record.get(..tab), record.get(tab + 1..)) else {
            continue;
        };
        let Ok(header) = std::str::from_utf8(header) else {
            continue;
        };
        let mut fields = header.split_whitespace();
        let (Some(mode), Some(_kind), Some(oid), Some(size)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Some(mode) = parse_mode(mode) else {
            continue;
        };
        entries.push(LsEntry {
            mode,
            oid: oid.to_string(),
            size: size.parse().ok(),
            name: name.to_vec(),
        });
    }
    entries
}

/// `git diff --raw -z` output into changes: `:<modes> <oids> <status>` NUL path [NUL path].
fn parse_raw(out: &[u8]) -> Vec<FileChange> {
    let mut fields = out.split(|b| *b == 0);
    let mut changes = Vec::new();
    while let Some(meta) = fields.next() {
        let Some(meta) = meta.strip_prefix(b":") else {
            continue;
        };
        let Ok(meta) = std::str::from_utf8(meta) else {
            continue;
        };
        let mut parts = meta.split(' ');
        let (Some(old_mode), Some(new_mode), Some(_), Some(_), Some(status)) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            continue;
        };
        let (Some(old_mode), Some(new_mode)) = (parse_mode(old_mode), parse_mode(new_mode)) else {
            continue;
        };
        let letter = status.bytes().next().unwrap_or(b'M');
        let similarity = status.get(1..).and_then(|s| s.parse::<u8>().ok());
        let Some(first) = fields.next() else {
            break;
        };
        let (old_path, new_path) = if matches!(letter, b'R' | b'C') {
            let Some(second) = fields.next() else {
                break;
            };
            (first.to_vec(), second.to_vec())
        } else {
            (first.to_vec(), first.to_vec())
        };
        changes.push(FileChange {
            status: letter,
            old_mode,
            new_mode,
            old_path,
            new_path,
            similarity,
        });
    }
    changes
}

/// `git diff --numstat -z` output: `adds TAB dels TAB path` NUL, and for a rename
/// `adds TAB dels TAB` NUL old NUL new NUL. Binary files have `-` counts.
fn parse_numstat(out: &[u8]) -> Vec<NumStat> {
    let mut fields = out.split(|b| *b == 0);
    let mut stats = Vec::new();
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        let mut parts = record.splitn(3, |b| *b == b'\t');
        let (Some(adds), Some(dels), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if path.is_empty() {
            // a rename or copy: the two paths follow
            fields.next();
            fields.next();
        }
        let num = |b: &[u8]| -> Option<u32> {
            if b == b"-" {
                None
            } else {
                std::str::from_utf8(b).ok().and_then(|s| s.parse().ok())
            }
        };
        stats.push(NumStat {
            additions: num(adds),
            deletions: num(dels),
        });
    }
    stats
}

/// What a raw status letter's patch looks like at its start.
fn shapes_of(letter: u8) -> &'static [PatchShape] {
    match letter {
        b'A' => &[PatchShape::New],
        b'D' => &[PatchShape::Deleted],
        b'R' => &[PatchShape::Rename],
        b'C' => &[PatchShape::Copy],
        // a type change is a deletion and a creation of the same path
        b'T' => &[PatchShape::Deleted, PatchShape::New],
        _ => &[PatchShape::Plain],
    }
}

/// Joins the patches of the pieces of one change into one (a type change has two).
fn join_patches(parts: Vec<FilePatch>) -> Option<FilePatch> {
    let mut iter = parts.into_iter();
    let mut first = iter.next()?;
    for next in iter {
        first.binary |= next.binary;
        first.truncated |= next.truncated;
        first.kept_additions = first.kept_additions.saturating_add(next.kept_additions);
        first.kept_deletions = first.kept_deletions.saturating_add(next.kept_deletions);
        first.hunks.extend(next.hunks);
    }
    Some(first)
}

fn not_found_commit(commit: &Sha) -> FleetError {
    FleetError::not_found(format!(
        "commit {} is not in the repository",
        show(commit.as_str())
    ))
}

impl GitVcs {
    /// The repository's own directory, once it is known to hold `commit`.
    fn read_dir_for(&self, repo: &str, commit: &Sha) -> Result<std::path::PathBuf> {
        let dir = self.open_repo(repo)?;
        let spec = format!("{}^{{commit}}", commit.as_str());
        let out = self.run(
            &dir,
            None,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &spec,
            ],
        )?;
        if !out.ok() {
            return Err(not_found_commit(commit));
        }
        Ok(dir)
    }

    /// One run of git for reading: literal pathspecs, an output cap that fails when it is
    /// exceeded.
    fn read_run(
        &self,
        dir: &Path,
        args: &[&str],
        cap: usize,
        truncate: bool,
    ) -> Result<(super::Out, bool)> {
        // the configuration of the repository is neutralized first, as for every other call
        let listing = self.run_raw_capped(
            dir,
            None,
            &[],
            &[],
            &["config", "--list", "-z"],
            Cap {
                bytes: super::MAX_OUTPUT,
                truncate: false,
            },
        )?;
        let overrides = if listing.0.ok() {
            super::dynamic_overrides(&listing.0.stdout)?
        } else {
            Vec::new()
        };
        let literal: [(&str, &OsStr); 1] = [("GIT_LITERAL_PATHSPECS", OsStr::new("1"))];
        let (out, cut) = self.run_raw_capped(
            dir,
            None,
            &overrides,
            &literal,
            args,
            Cap {
                bytes: cap,
                truncate,
            },
        )?;
        Ok((out, cut))
    }

    /// What is at `path` at `commit`; `None` when nothing is. The root is a directory.
    fn entry_at(&self, dir: &Path, commit: &Sha, path: &RepoPath) -> Result<Option<LsEntry>> {
        if path.is_root() {
            return Ok(Some(LsEntry {
                mode: 0o040_000,
                oid: String::new(),
                size: None,
                name: Vec::new(),
            }));
        }
        let (out, _) = self.read_run(
            dir,
            &[
                "ls-tree",
                "-z",
                "-l",
                "--full-tree",
                commit.as_str(),
                "--",
                path.as_str(),
            ],
            LISTING_BYTES,
            true,
        )?;
        if !out.ok() {
            return Err(self.failure(&["ls-tree"], &out));
        }
        Ok(parse_ls_tree(&out.stdout)
            .into_iter()
            .find(|e| e.name == path.as_str().as_bytes()))
    }

    /// The size git records for a blob, asked before the blob is read.
    fn blob_size(&self, dir: &Path, oid: &Sha) -> Result<u64> {
        let (out, _) = self.read_run(dir, &["cat-file", "-s", oid.as_str()], SMALL_BYTES, false)?;
        if !out.ok() {
            return Err(self.failure(&["cat-file"], &out));
        }
        out.text()
            .trim()
            .parse()
            .map_err(|_| FleetError::internal("git printed an object size that is not a number"))
    }
}

impl RepoReader for GitVcs {
    fn list_dir(
        &self,
        repo: &str,
        commit: &Sha,
        dir_path: &RepoPath,
        limit: usize,
    ) -> Result<DirListing> {
        let dir = self.read_dir_for(repo, commit)?;
        match self.entry_at(&dir, commit, dir_path)? {
            None => {
                return Err(FleetError::not_found(format!(
                    "{} does not exist at that commit",
                    show(dir_path.as_str())
                )))
            }
            Some(e) if EntryKind::from_mode(e.mode) != EntryKind::Dir => {
                return Err(FleetError::invalid(format!(
                    "{} is not a directory",
                    show(dir_path.as_str())
                )))
            }
            Some(_) => {}
        }
        let treeish = if dir_path.is_root() {
            commit.as_str().to_string()
        } else {
            format!("{}:{}", commit.as_str(), dir_path.as_str())
        };
        let (out, cut) = self.read_run(
            &dir,
            &["ls-tree", "-z", "-l", &treeish],
            LISTING_BYTES,
            true,
        )?;
        if !out.ok() && !cut {
            return Err(self.failure(&["ls-tree"], &out));
        }
        let all = parse_ls_tree(&out.stdout);
        let truncated = cut || all.len() > limit;
        let entries = all
            .into_iter()
            .take(limit)
            .map(|e| TreeEntry::from_git(dir_path, &e.name, e.mode, e.size))
            .collect();
        Ok(DirListing { entries, truncated })
    }

    fn read_entry(
        &self,
        repo: &str,
        commit: &Sha,
        path: &RepoPath,
        max_bytes: usize,
    ) -> Result<Option<EntryRead>> {
        let dir = self.read_dir_for(repo, commit)?;
        let Some(e) = self.entry_at(&dir, commit, path)? else {
            return Ok(None);
        };
        let kind = EntryKind::from_mode(e.mode);
        let mode = ModeClass::from_mode(e.mode);
        match kind {
            EntryKind::Dir => Ok(Some(EntryRead {
                stat: EntryStat {
                    kind,
                    mode,
                    size: None,
                    oid: e.oid,
                },
                content: None,
            })),
            EntryKind::Submodule => Ok(Some(EntryRead {
                stat: EntryStat {
                    kind,
                    mode,
                    size: None,
                    oid: e.oid,
                },
                content: None,
            })),
            EntryKind::File | EntryKind::Symlink => {
                let oid = Sha::parse(&e.oid)?;
                // the size is asked for (and compared with the limit) before a byte is read
                let size = self.blob_size(&dir, &oid)?;
                let content = if size > max_bytes as u64 {
                    None
                } else {
                    let (out, _) = self.read_run(
                        &dir,
                        &["cat-file", "blob", oid.as_str()],
                        max_bytes.max(1),
                        false,
                    )?;
                    if !out.ok() {
                        return Err(self.failure(&["cat-file"], &out));
                    }
                    if out.stdout.len() as u64 != size {
                        return Err(FleetError::internal(
                            "git returned a blob whose length is not the size it announced",
                        ));
                    }
                    Some(out.stdout)
                };
                Ok(Some(EntryRead {
                    stat: EntryStat {
                        kind,
                        mode,
                        size: Some(size),
                        oid: e.oid,
                    },
                    content,
                }))
            }
        }
    }

    fn diff(&self, repo: &str, base: &Sha, head: &Sha, req: &DiffRequest) -> Result<DiffSet> {
        let dir = self.read_dir_for(repo, base)?;
        // the head commit must exist too
        let spec = format!("{}^{{commit}}", head.as_str());
        let check = self.read_run(
            &dir,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &spec,
            ],
            SMALL_BYTES,
            false,
        )?;
        if !check.0.ok() {
            return Err(not_found_commit(head));
        }
        let rename_limit = format!("-l{RENAME_LIMIT}");
        let context = format!("-U{}", req.context);
        // every option that decides how the diff looks is given here, so no configuration of the
        // repository can change the format that is parsed below
        let fixed = [
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-abbrev",
            "--ignore-submodules=none",
            "--submodule=short",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "-M",
            rename_limit.as_str(),
        ];
        let tail = [base.as_str(), head.as_str(), "--"];
        let run = |extra: &[&str], cap: usize| -> Result<(Vec<u8>, bool)> {
            let mut args: Vec<&str> = vec!["diff"];
            args.extend_from_slice(extra);
            args.extend_from_slice(&fixed);
            args.extend_from_slice(&tail);
            let (out, cut) = self.read_run(&dir, &args, cap, true)?;
            if !out.ok() && !cut {
                return Err(self.failure(&["diff"], &out));
            }
            Ok((out.stdout, cut))
        };
        let (raw, raw_cut) = run(&["--raw", "-z"], CHANGE_LIST_BYTES)?;
        let (numstat, _) = run(&["--numstat", "-z"], CHANGE_LIST_BYTES)?;
        let (patch, patch_cut) = run(&["-p", context.as_str()], PATCH_BYTES)?;

        let changes = parse_raw(&raw);
        let stats = parse_numstat(&numstat);
        let chunks = split_patch(&patch);
        let mut budget: DiffBudget = req.budget;
        let mut chunk_at = 0usize;
        let mut in_sync = true;
        let mut files: Vec<DiffFile> = Vec::new();
        for (i, change) in changes.iter().enumerate().take(req.max_files) {
            let shapes = shapes_of(change.status);
            let mut parts: Vec<FilePatch> = Vec::new();
            if in_sync {
                for want in shapes {
                    let Some(chunk) = chunks.get(chunk_at) else {
                        break;
                    };
                    chunk_at += 1;
                    if budget.exhausted() {
                        continue;
                    }
                    let p = parse_file_patch(chunk, &mut budget);
                    if p.shape != *want {
                        // the patch and the file list disagree: nothing after this can be matched
                        in_sync = false;
                        parts.clear();
                        break;
                    }
                    parts.push(p);
                }
            }
            files.push(assemble_file(
                change,
                stats.get(i).copied(),
                join_patches(parts),
            ));
        }
        let left_out = changes.len().saturating_sub(req.max_files);
        let mut notes: Vec<String> = Vec::new();
        if left_out > 0 {
            notes.push(format!("{left_out} more changed files are not shown."));
        }
        if raw_cut {
            notes.push("The list of changed files was cut: there are more of them.".to_string());
        }
        if patch_cut && !raw_cut && left_out == 0 {
            notes.push("The patch text was cut: files near the end may show no lines.".to_string());
        }
        if !in_sync {
            notes.push(
                "Some lines could not be matched to their files and are not shown.".to_string(),
            );
        }
        Ok(DiffSet {
            base: base.clone(),
            head: head.clone(),
            context: req.context,
            files,
            truncated: left_out > 0 || raw_cut,
            notice: (!notes.is_empty()).then(|| notes.join(" ")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_domain::error::ErrorCode;

    #[test]
    fn ls_tree_records_are_parsed_and_a_cut_record_is_dropped() {
        let sha = "a".repeat(40);
        let out = format!(
            "100644 blob {sha}      12\ta file\0040000 tree {sha}       -\tdir\0120000 blob {sha}       3\tl\0160000 commit {sha}       -\tm\0100644 blob {sha}       5\tcut"
        );
        let got = parse_ls_tree(out.as_bytes());
        assert_eq!(got.len(), 4, "the last record had no terminator");
        assert_eq!(
            (got[0].mode, got[0].size, got[0].name.as_slice()),
            (0o100_644, Some(12), &b"a file"[..])
        );
        assert_eq!((got[1].mode, got[1].size), (0o040_000, None));
        assert_eq!(got[3].mode, 0o160_000);
        // names may hold tabs and anything else but NUL
        let out = format!("100644 blob {sha}       1\ta\tb\nc\0");
        let got = parse_ls_tree(out.as_bytes());
        assert_eq!(got[0].name, b"a\tb\nc");
        assert!(parse_ls_tree(b"garbage\0").is_empty());
        assert!(parse_ls_tree(b"").is_empty());
        assert!(parse_ls_tree(b"xx yy\tname\0").is_empty());
    }

    #[test]
    fn raw_listings_with_renames_are_parsed() {
        let z = "\0";
        let out = format!(
            ":100644 100644 {a} {b} M{z}src/a.rs{z}:000000 100644 {a} {b} A{z}new{z}:100644 100644 {a} {b} R087{z}old name{z}new name{z}:100644 000000 {a} {b} D{z}gone{z}",
            a = "1".repeat(40),
            b = "2".repeat(40)
        );
        let got = parse_raw(out.as_bytes());
        assert_eq!(got.len(), 4);
        assert_eq!(got[0].status, b'M');
        assert_eq!(got[0].old_path, b"src/a.rs");
        assert_eq!(
            (got[1].status, got[1].old_mode, got[1].new_mode),
            (b'A', 0, 0o100_644)
        );
        assert_eq!(got[2].status, b'R');
        assert_eq!(
            (got[2].old_path.as_slice(), got[2].new_path.as_slice()),
            (&b"old name"[..], &b"new name"[..])
        );
        assert_eq!(got[2].similarity, Some(87));
        assert_eq!(got[3].status, b'D');
        assert!(parse_raw(b"").is_empty());
        assert!(parse_raw(b"junk\0").is_empty());
        assert!(parse_raw(b":1 2\0").is_empty());
    }

    #[test]
    fn numstat_listings_with_binary_files_and_renames_are_parsed() {
        let out = "1\t2\tsrc/a.rs\0-\t-\timg.png\x003\t0\t\0old\0new\0";
        let got = parse_numstat(out.as_bytes());
        assert_eq!(got.len(), 3);
        assert_eq!((got[0].additions, got[0].deletions), (Some(1), Some(2)));
        assert_eq!((got[1].additions, got[1].deletions), (None, None));
        assert_eq!((got[2].additions, got[2].deletions), (Some(3), Some(0)));
        assert!(parse_numstat(b"").is_empty());
        assert!(parse_numstat(b"nonsense\0").is_empty());
    }

    #[test]
    fn a_type_change_expects_two_patches() {
        assert_eq!(shapes_of(b'T'), &[PatchShape::Deleted, PatchShape::New]);
        assert_eq!(shapes_of(b'M'), &[PatchShape::Plain]);
        assert_eq!(shapes_of(b'R'), &[PatchShape::Rename]);
        assert_eq!(shapes_of(b'?'), &[PatchShape::Plain]);
        assert!(join_patches(Vec::new()).is_none());
    }

    #[test]
    fn unknown_commits_are_not_found() {
        let e = not_found_commit(&Sha::parse(&"a".repeat(40)).unwrap());
        assert_eq!(e.code, ErrorCode::NotFound);
    }
}
