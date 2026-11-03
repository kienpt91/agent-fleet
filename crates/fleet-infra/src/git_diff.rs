//! Parsing of what `git diff --name-status -z` and `git diff --numstat -z` print into a
//! [`DiffStat`]. Pure: the adapter runs git (hardened, in the repository, never in a worktree) and
//! hands the bytes over. The input is whatever git printed for a branch a worker wrote, so it is
//! parsed strictly and every path is kept as git gave it (lossily decoded when it is not UTF-8).

use fleet_domain::error::{FleetError, Result};
use fleet_fanout::diff::{ChangeKind, DiffStat, FileChange};
use std::collections::BTreeMap;

fn bad(what: &str) -> FleetError {
    FleetError::internal(format!("unexpected output from git diff: {what}"))
}

fn text(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).into_owned()
}

/// What `--numstat -z` said about one new path.
#[derive(Clone, Copy)]
struct Counts {
    insertions: u64,
    deletions: u64,
    binary: bool,
}

fn parse_numstat(out: &[u8]) -> Result<BTreeMap<String, Counts>> {
    let mut tokens = out.split(|b| *b == 0).filter(|t| !t.is_empty());
    let mut map = BTreeMap::new();
    while let Some(head) = tokens.next() {
        // `<ins>\t<del>\t<path>`; for a rename the path part is empty and the old and new paths
        // follow as two more NUL-separated tokens
        let mut parts = head.splitn(3, |b| *b == b'\t');
        let (Some(ins), Some(del), Some(rest)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(bad("a numstat record has no counts"));
        };
        let path = if rest.is_empty() {
            let _old = tokens
                .next()
                .ok_or_else(|| bad("a rename has no old path"))?;
            tokens
                .next()
                .ok_or_else(|| bad("a rename has no new path"))?
        } else {
            rest
        };
        let counts = if ins == b"-" && del == b"-" {
            Counts {
                insertions: 0,
                deletions: 0,
                binary: true,
            }
        } else {
            let number = |raw: &[u8]| -> Result<u64> {
                std::str::from_utf8(raw)
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .ok_or_else(|| bad("a line count is not a number"))
            };
            Counts {
                insertions: number(ins)?,
                deletions: number(del)?,
                binary: false,
            }
        };
        map.insert(text(path), counts);
    }
    Ok(map)
}

fn kind_of(letter: u8) -> ChangeKind {
    match letter {
        b'A' | b'C' => ChangeKind::Added,
        b'D' => ChangeKind::Deleted,
        b'R' => ChangeKind::Renamed,
        b'T' => ChangeKind::TypeChanged,
        _ => ChangeKind::Modified,
    }
}

/// The change described by the two outputs of one `git diff base head` (same options: `-z`, the same
/// rename detection).
pub(crate) fn parse(name_status: &[u8], numstat: &[u8]) -> Result<DiffStat> {
    let counts = parse_numstat(numstat)?;
    let mut tokens = name_status.split(|b| *b == 0).filter(|t| !t.is_empty());
    let mut changes = Vec::new();
    while let Some(status) = tokens.next() {
        let letter = *status
            .first()
            .ok_or_else(|| bad("a status record is empty"))?;
        let (old_path, path) = if letter == b'R' || letter == b'C' {
            let old = tokens
                .next()
                .ok_or_else(|| bad("a rename has no old path"))?;
            let new = tokens
                .next()
                .ok_or_else(|| bad("a rename has no new path"))?;
            (Some(text(old)), text(new))
        } else {
            let p = tokens.next().ok_or_else(|| bad("a status has no path"))?;
            (None, text(p))
        };
        let c = counts.get(&path).copied().unwrap_or(Counts {
            insertions: 0,
            deletions: 0,
            binary: false,
        });
        changes.push(FileChange {
            path,
            old_path,
            kind: kind_of(letter),
            insertions: c.insertions,
            deletions: c.deletions,
            binary: c.binary,
        });
    }
    Ok(DiffStat::from_changes(changes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_changes() {
        let names = b"M\0src/a.rs\0A\0new.txt\0D\0gone.txt\0T\0link\0";
        let nums = b"3\t1\tsrc/a.rs\0 5\t0\tnew.txt\0".to_vec();
        // a stray space before a count is not a number: strict
        assert!(parse(names, &nums).is_err());
        let nums = b"3\t1\tsrc/a.rs\x005\t0\tnew.txt\x000\t7\tgone.txt\x000\t0\tlink\0";
        let stat = parse(names, nums).unwrap();
        assert_eq!(stat.files, 4);
        assert_eq!((stat.insertions, stat.deletions), (8, 8));
        let kinds: Vec<(&str, ChangeKind)> = stat
            .changes
            .iter()
            .map(|c| (c.path.as_str(), c.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                ("gone.txt", ChangeKind::Deleted),
                ("link", ChangeKind::TypeChanged),
                ("new.txt", ChangeKind::Added),
                ("src/a.rs", ChangeKind::Modified),
            ]
        );
    }

    #[test]
    fn renames_carry_both_paths_and_counts_of_the_new_one() {
        let names = b"R087\0old/name.rs\0new/name.rs\0M\0x\0";
        let nums = b"2\t1\t\0old/name.rs\0new/name.rs\0" as &[u8];
        let mut all = nums.to_vec();
        all.extend_from_slice(b"1\t1\tx\0");
        let stat = parse(names, &all).unwrap();
        let r = stat
            .changes
            .iter()
            .find(|c| c.kind == ChangeKind::Renamed)
            .unwrap();
        assert_eq!(r.path, "new/name.rs");
        assert_eq!(r.old_path.as_deref(), Some("old/name.rs"));
        assert_eq!((r.insertions, r.deletions), (2, 1));
        assert_eq!(stat.files, 2);
    }

    #[test]
    fn binary_files_have_no_counts_and_are_counted_apart() {
        let stat = parse(
            b"A\0logo.png\0M\0a.txt\0",
            b"-\t-\tlogo.png\x004\t0\ta.txt\0",
        )
        .unwrap();
        assert_eq!(stat.binary_files, 1);
        assert_eq!(stat.size(), 4 + fleet_fanout::diff::BINARY_FILE_WEIGHT);
        let png = stat.changes.iter().find(|c| c.path == "logo.png").unwrap();
        assert!(png.binary);
        assert_eq!((png.insertions, png.deletions), (0, 0));
    }

    #[test]
    fn paths_with_tabs_newlines_and_control_characters_survive() {
        let odd = "dir/odd\tname\nwith \u{1b}[31m.txt";
        let names = format!("A\0{odd}\0");
        let nums = format!("1\t0\t{odd}\0");
        let stat = parse(names.as_bytes(), nums.as_bytes()).unwrap();
        assert_eq!(stat.changes[0].path, odd);
        assert_eq!(stat.changes[0].insertions, 1);
    }

    #[test]
    fn a_path_that_is_not_utf8_is_decoded_lossily_not_dropped() {
        let mut names = b"A\0bad-".to_vec();
        names.extend_from_slice(&[0xff, 0xfe]);
        names.extend_from_slice(b".txt\0");
        let mut nums = b"1\t0\tbad-".to_vec();
        nums.extend_from_slice(&[0xff, 0xfe]);
        nums.extend_from_slice(b".txt\0");
        let stat = parse(&names, &nums).unwrap();
        assert_eq!(stat.files, 1);
        assert!(stat.changes[0].path.starts_with("bad-"));
        assert_eq!(stat.changes[0].insertions, 1);
    }

    #[test]
    fn nothing_changed_is_an_empty_stat() {
        assert_eq!(parse(b"", b"").unwrap(), DiffStat::default());
    }

    #[test]
    fn truncated_or_garbled_output_is_an_error_not_a_guess() {
        for (names, nums) in [
            (b"M\0" as &[u8], b"" as &[u8]),
            (b"R100\0old\0", b""),
            (b"", b"garbage\0"),
            (b"", b"x\ty\tpath\0"),
            (b"", b"1\t2\t\0only-old\0"),
        ] {
            assert!(parse(names, nums).is_err(), "{names:?} {nums:?}");
        }
    }

    #[test]
    fn a_mode_only_change_without_counts_is_a_modified_file_with_none() {
        let stat = parse(b"M\0script.sh\0", b"0\t0\tscript.sh\0").unwrap();
        assert_eq!(stat.files, 1);
        // no lines, but a change: it weighs what a change without lines weighs
        assert_eq!(stat.size(), fleet_fanout::diff::LINELESS_CHANGE_WEIGHT);
    }
}
