//! Directory entries of a tree at a commit.
//!
//! A name in a git tree is a byte string the repository controls. It is shown through
//! [`display_name`], which makes control characters, invisible and bidirectional characters and
//! bytes that are not UTF-8 visible as escapes, so the text cannot steer a terminal or a page or
//! pose as another name. An entry whose name is not a valid [`RepoPath`] component is still listed
//! (hiding it would hide what is in the repository) but cannot be addressed: its `path` is `None`.

use super::limits::MAX_NAME_CHARS;
use super::path::{component_problem, RepoPath};
use crate::ids::is_invisible;
use std::fmt::Write as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryKind {
    Dir,
    File,
    Symlink,
    Submodule,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::Dir => "dir",
            EntryKind::File => "file",
            EntryKind::Symlink => "symlink",
            EntryKind::Submodule => "submodule",
        }
    }

    /// The kind a git tree mode stands for. Unknown modes are plain files: git itself treats
    /// every `100xxx` mode as a regular file.
    pub fn from_mode(mode: u32) -> EntryKind {
        match mode & 0o170_000 {
            0o040_000 => EntryKind::Dir,
            0o120_000 => EntryKind::Symlink,
            0o160_000 => EntryKind::Submodule,
            _ => EntryKind::File,
        }
    }
}

/// What a file's mode says, without the permission bits that mean nothing in a repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeClass {
    Regular,
    Executable,
    /// Directories, links, submodules.
    Other,
}

impl ModeClass {
    pub fn as_str(self) -> &'static str {
        match self {
            ModeClass::Regular => "regular",
            ModeClass::Executable => "executable",
            ModeClass::Other => "other",
        }
    }

    pub fn from_mode(mode: u32) -> ModeClass {
        match EntryKind::from_mode(mode) {
            EntryKind::File if mode & 0o111 != 0 => ModeClass::Executable,
            EntryKind::File => ModeClass::Regular,
            _ => ModeClass::Other,
        }
    }
}

/// An octal mode as git prints it (`100644`, `040000`).
pub fn parse_mode(s: &str) -> Option<u32> {
    if s.is_empty() || s.len() > 8 {
        return None;
    }
    u32::from_str_radix(s, 8).ok()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    /// The name, safe to show.
    pub name: String,
    /// Where to ask for this entry; `None` when its name could not be requested back.
    pub path: Option<RepoPath>,
    pub kind: EntryKind,
    /// Bytes of a file or a link's target; unknown for directories and submodules.
    pub size: Option<u64>,
    pub mode: ModeClass,
}

impl TreeEntry {
    /// An entry of directory `parent` as git lists it.
    pub fn from_git(parent: &RepoPath, name: &[u8], mode: u32, size: Option<u64>) -> TreeEntry {
        let kind = EntryKind::from_mode(mode);
        let path = std::str::from_utf8(name)
            .ok()
            .filter(|n| component_problem(n).is_none())
            .and_then(|n| parent.join(n).ok());
        TreeEntry {
            name: display_name(name),
            path,
            kind,
            size: match kind {
                EntryKind::File | EntryKind::Symlink => size,
                EntryKind::Dir | EntryKind::Submodule => None,
            },
            mode: ModeClass::from_mode(mode),
        }
    }
}

/// Directories first, then by name (bytes of the displayed name, so the order is the same for
/// every client).
pub fn sort_entries(entries: &mut [TreeEntry]) {
    entries.sort_by(|a, b| {
        let a_dir = a.kind == EntryKind::Dir;
        let b_dir = b.kind == EntryKind::Dir;
        b_dir.cmp(&a_dir).then_with(|| a.name.cmp(&b.name))
    });
}

/// A name or path from git, made safe to show: invalid UTF-8 becomes `\xNN` per byte, control,
/// invisible and bidirectional characters become `\u{..}`, and the result is cut at `max` characters
/// (with an ellipsis).
pub fn display_text(bytes: &[u8], max: usize) -> String {
    let mut out = String::new();
    let mut chars = 0usize;
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            if chars >= max {
                out.push('\u{2026}');
                return out;
            }
            chars += 1;
            if c.is_control() || is_invisible(c) {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            } else {
                out.push(c);
            }
        }
        for b in chunk.invalid() {
            if chars >= max {
                out.push('\u{2026}');
                return out;
            }
            chars += 1;
            let _ = write!(out, "\\x{b:02x}");
        }
    }
    out
}

/// [`display_text`] with the longest name length.
pub fn display_name(bytes: &[u8]) -> String {
    display_text(bytes, MAX_NAME_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> RepoPath {
        RepoPath::root()
    }

    #[test]
    fn modes_map_to_kinds_and_classes() {
        for (mode, kind, class) in [
            (0o100_644, EntryKind::File, ModeClass::Regular),
            (0o100_755, EntryKind::File, ModeClass::Executable),
            (0o100_664, EntryKind::File, ModeClass::Regular),
            (0o040_000, EntryKind::Dir, ModeClass::Other),
            (0o120_000, EntryKind::Symlink, ModeClass::Other),
            (0o160_000, EntryKind::Submodule, ModeClass::Other),
            (0o100_000, EntryKind::File, ModeClass::Regular),
        ] {
            assert_eq!(EntryKind::from_mode(mode), kind, "{mode:o}");
            assert_eq!(ModeClass::from_mode(mode), class, "{mode:o}");
        }
        assert_eq!(parse_mode("100644"), Some(0o100_644));
        assert_eq!(parse_mode("040000"), Some(0o040_000));
        for bad in ["", "9", "1006440000", "10064x", "-1"] {
            assert_eq!(parse_mode(bad), None, "{bad:?}");
        }
        for k in [
            EntryKind::Dir,
            EntryKind::File,
            EntryKind::Symlink,
            EntryKind::Submodule,
        ] {
            assert!(!k.as_str().is_empty());
        }
        assert_eq!(ModeClass::Executable.as_str(), "executable");
        assert_eq!(ModeClass::Regular.as_str(), "regular");
        assert_eq!(ModeClass::Other.as_str(), "other");
    }

    #[test]
    fn names_are_made_visible_not_interpreted() {
        assert_eq!(display_name(b"plain.rs"), "plain.rs");
        assert_eq!(display_name("é名前".as_bytes()), "é名前");
        assert_eq!(display_name(b"a\x1b[31mred"), "a\\u{1b}[31mred");
        assert_eq!(display_name(b"a\nb"), "a\\u{a}b");
        assert_eq!(
            display_name("txt.exe\u{202e}".as_bytes()),
            "txt.exe\\u{202e}"
        );
        assert_eq!(display_name("a\u{200b}b".as_bytes()), "a\\u{200b}b");
        assert_eq!(display_name(b"bad\xff\xfeend"), "bad\\xff\\xfeend");
        assert_eq!(
            display_name(b"<script>alert(1)</script>"),
            "<script>alert(1)</script>"
        );
        assert_eq!(display_name(b""), "");
        let long = "x".repeat(MAX_NAME_CHARS + 50);
        let shown = display_name(long.as_bytes());
        assert_eq!(shown.chars().count(), MAX_NAME_CHARS + 1);
        assert!(shown.ends_with('\u{2026}'));
        // invalid bytes also count against the cap
        let junk = vec![0xffu8; 1000];
        assert!(display_name(&junk).chars().count() <= MAX_NAME_CHARS * 4 + 4);
    }

    #[test]
    fn an_entry_is_addressable_only_when_its_name_is_a_valid_component() {
        let parent = RepoPath::parse("src").unwrap();
        let good = TreeEntry::from_git(&parent, b"main.rs", 0o100_644, Some(12));
        assert_eq!(
            good.path.as_ref().map(RepoPath::as_str),
            Some("src/main.rs")
        );
        assert_eq!(good.size, Some(12));
        assert_eq!(good.kind, EntryKind::File);
        for hostile in [
            &b"a\nb"[..],
            b"a\\b",
            b"bad\xff",
            "a\u{202e}b".as_bytes(),
            b".git",
            b"..",
            b".",
            b"",
            b"a\0b",
        ] {
            let e = TreeEntry::from_git(&parent, hostile, 0o100_644, Some(1));
            assert!(e.path.is_none(), "{hostile:?}");
            assert!(
                !e.name.contains('\n') && !e.name.contains('\0'),
                "{:?}",
                e.name
            );
        }
        let long = vec![b'a'; 300];
        assert!(TreeEntry::from_git(&root(), &long, 0o100_644, None)
            .path
            .is_none());
    }

    #[test]
    fn sizes_are_only_kept_for_files_and_links() {
        let r = root();
        assert_eq!(TreeEntry::from_git(&r, b"d", 0o040_000, Some(9)).size, None);
        assert_eq!(TreeEntry::from_git(&r, b"m", 0o160_000, Some(9)).size, None);
        assert_eq!(
            TreeEntry::from_git(&r, b"l", 0o120_000, Some(9)).size,
            Some(9)
        );
        assert_eq!(
            TreeEntry::from_git(&r, b"f", 0o100_755, Some(9)).mode,
            ModeClass::Executable
        );
    }

    #[test]
    fn directories_come_first_then_names_in_byte_order() {
        let r = root();
        let mut v = vec![
            TreeEntry::from_git(&r, b"b.txt", 0o100_644, Some(1)),
            TreeEntry::from_git(&r, b"Z", 0o040_000, None),
            TreeEntry::from_git(&r, b"a", 0o040_000, None),
            TreeEntry::from_git(&r, b"B.txt", 0o100_644, Some(1)),
            TreeEntry::from_git(&r, b"link", 0o120_000, Some(3)),
            TreeEntry::from_git(&r, b"mod", 0o160_000, None),
        ];
        sort_entries(&mut v);
        let names: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["Z", "a", "B.txt", "b.txt", "link", "mod"]);
    }
}
