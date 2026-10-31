//! A path inside a repository at a recorded commit.
//!
//! A [`RepoPath`] names a place in a git tree, never a place on a disk: it is only ever handed to
//! `git` as the path part of `<commit>:<path>` or after `--`. It is still validated strictly,
//! because a client supplies it and a repository (so a worker) controls what lives there: relative,
//! no `.` or `..` components, no empty components, no backslash, NUL, control, invisible or bidi
//! characters, no `.git` component, bounded length and depth. Nothing is normalized: `é` as one
//! character and as two are different names, as they are for git.

use super::limits::{MAX_COMPONENT_BYTES, MAX_PATH_BYTES, MAX_PATH_DEPTH};
use crate::error::{FleetError, Result};
use crate::ids::{is_invisible, show};
use std::fmt;

/// A validated, relative, `/`-separated path. The empty path is the root of the repository.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoPath(String);

/// Why one component is not acceptable, or `None` when it is.
pub fn component_problem(c: &str) -> Option<&'static str> {
    if c.is_empty() {
        return Some("has an empty component");
    }
    if c == "." || c == ".." {
        return Some("has a `.` or `..` component");
    }
    if c.eq_ignore_ascii_case(".git") {
        return Some("has a `.git` component");
    }
    if c.len() > MAX_COMPONENT_BYTES {
        return Some("has a component that is too long");
    }
    for ch in c.chars() {
        if ch == '\\' {
            return Some("contains a backslash");
        }
        if ch == '\0' {
            return Some("contains a NUL");
        }
        if ch.is_control() {
            return Some("contains a control character");
        }
        if is_invisible(ch) {
            return Some("contains an invisible or bidirectional control character");
        }
    }
    None
}

impl RepoPath {
    /// The root of the repository.
    pub fn root() -> RepoPath {
        RepoPath(String::new())
    }

    /// Validates a path. `""` is the root.
    pub fn parse(s: &str) -> Result<RepoPath> {
        if s.is_empty() {
            return Ok(RepoPath::root());
        }
        let bad = |why: &str| {
            Err(FleetError::invalid(format!(
                "repository path {} {why}",
                show(s)
            )))
        };
        if s.len() > MAX_PATH_BYTES {
            return bad("is too long");
        }
        if s.starts_with('/') {
            return bad("must be relative");
        }
        if s.ends_with('/') {
            return bad("must not end with a slash");
        }
        let mut depth = 0usize;
        for c in s.split('/') {
            if let Some(why) = component_problem(c) {
                return bad(why);
            }
            depth += 1;
        }
        if depth > MAX_PATH_DEPTH {
            return bad("is nested too deeply");
        }
        Ok(RepoPath(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The components, outermost first. The root has none.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|c| !c.is_empty())
    }

    pub fn depth(&self) -> usize {
        self.components().count()
    }

    /// The last component; empty for the root.
    pub fn name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or("")
    }

    /// The directory that holds this path (the root holds itself).
    pub fn parent(&self) -> RepoPath {
        match self.0.rfind('/') {
            Some(i) => RepoPath(self.0.get(..i).unwrap_or("").to_string()),
            None => RepoPath::root(),
        }
    }

    /// This path with one more component.
    pub fn join(&self, component: &str) -> Result<RepoPath> {
        if self.0.is_empty() {
            return RepoPath::parse(component);
        }
        if component.contains('/') {
            return Err(FleetError::invalid(format!(
                "path component {} contains a slash",
                show(component)
            )));
        }
        RepoPath::parse(&format!("{}/{component}", self.0))
    }

    /// The lower-case extension of the last component, without the dot.
    pub fn extension(&self) -> Option<String> {
        let name = self.name();
        let (stem, ext) = name.rsplit_once('.')?;
        if stem.is_empty() || ext.is_empty() {
            return None;
        }
        Some(ext.to_ascii_lowercase())
    }

    /// Where a link written in a document that lives in directory `self` points, when it points
    /// into the repository: `a/b.md`, `./b.md`, `../c/d.png` and `/e.md` (from the root). A query or a
    /// fragment is dropped, percent escapes are decoded, and anything that would leave the
    /// repository (too many `..`), is not UTF-8, or is not a valid path gives `None`. The result is
    /// computed on the text of the target only; nothing is looked up.
    pub fn resolve_link(&self, target: &str) -> Option<RepoPath> {
        if target.len() > MAX_PATH_BYTES * 3 {
            return None;
        }
        let end = target.find(['#', '?']).unwrap_or(target.len());
        let raw = target.get(..end)?;
        if raw.is_empty() {
            return None;
        }
        let decoded = percent_decode(raw)?;
        let mut stack: Vec<&str> = Vec::new();
        if !decoded.starts_with('/') {
            stack.extend(self.components());
        }
        for part in decoded.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    stack.pop()?;
                }
                other => stack.push(other),
            }
        }
        if stack.is_empty() {
            return None;
        }
        RepoPath::parse(&stack.join("/")).ok()
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `%41` into `A`; a malformed escape, a decoded NUL or bytes that are not UTF-8 give `None`.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            if !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).ok().filter(|t| !t.contains('\0'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> RepoPath {
        RepoPath::parse(s).unwrap_or_else(|e| panic!("{s:?}: {}", e.message))
    }

    #[test]
    fn plain_paths_and_the_root_are_accepted() {
        assert!(ok("").is_root());
        assert_eq!(ok("a").as_str(), "a");
        assert_eq!(ok("src/lib.rs").depth(), 2);
        assert_eq!(ok("src/lib.rs").name(), "lib.rs");
        assert_eq!(ok("src/lib.rs").parent(), ok("src"));
        assert_eq!(ok("lib.rs").parent(), RepoPath::root());
        assert_eq!(RepoPath::root().parent(), RepoPath::root());
        assert_eq!(RepoPath::root().depth(), 0);
        assert_eq!(RepoPath::root().name(), "");
        for fine in [
            "a b",
            "dir/.gitignore",
            ".github/workflows/ci.yml",
            "-rf",
            "a:b",
            "é/ü",
            "名前/ファイル.md",
            "a%2e%2e/b",
            "a..b",
            "...",
            "x/.../y",
            "-",
        ] {
            assert!(RepoPath::parse(fine).is_ok(), "{fine:?}");
        }
    }

    #[test]
    fn traversal_and_hostile_forms_are_refused() {
        let long = "a".repeat(MAX_PATH_BYTES + 1);
        let component = "b".repeat(MAX_COMPONENT_BYTES + 1);
        let deep = vec!["d"; MAX_PATH_DEPTH + 1].join("/");
        for bad in [
            "..",
            "../x",
            "a/..",
            "a/../b",
            "./a",
            "a/./b",
            ".",
            "/abs",
            "/",
            "//",
            "a//b",
            "a/",
            "a\\b",
            "..\\x",
            "a\\..\\b",
            "a\0b",
            "\0",
            "a\nb",
            "a\rb",
            "a\tb",
            "a\u{1b}[31mb",
            "a\u{9b}b",
            "a\u{7f}",
            "a\u{202e}txt.exe",
            "\u{2066}a",
            "a\u{200b}b",
            "a\u{feff}",
            "a\u{061c}",
            ".git",
            ".GIT/config",
            "a/.Git/hooks/x",
            long.as_str(),
            component.as_str(),
            deep.as_str(),
        ] {
            assert!(RepoPath::parse(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn lookalikes_are_different_names_not_traversal() {
        // full-width dots and slash, and a division slash, are ordinary characters
        for fine in [
            "\u{ff0e}\u{ff0e}",
            "\u{ff0e}\u{ff0e}\u{ff0f}x",
            "a\u{2215}b",
            "a\u{2044}b",
        ] {
            assert!(RepoPath::parse(fine).is_ok(), "{fine:?}");
        }
        // nothing is normalized: composed and decomposed forms are two names
        assert_ne!(ok("\u{e9}"), ok("e\u{301}"));
        // a percent escape is not decoded by the validator
        assert_eq!(ok("%2e%2e").as_str(), "%2e%2e");
    }

    #[test]
    fn join_validates_the_component() {
        assert_eq!(ok("a").join("b").unwrap(), ok("a/b"));
        assert_eq!(RepoPath::root().join("b").unwrap(), ok("b"));
        for bad in ["", "..", ".", "x/y", "a\\b", ".git", "a\u{202e}"] {
            assert!(ok("a").join(bad).is_err(), "{bad:?}");
        }
        let deep = vec!["d"; MAX_PATH_DEPTH].join("/");
        assert!(ok(&deep).join("x").is_err());
    }

    #[test]
    fn the_extension_is_lower_case_and_needs_a_stem() {
        assert_eq!(ok("a/README.MD").extension().as_deref(), Some("md"));
        assert_eq!(ok("a.tar.gz").extension().as_deref(), Some("gz"));
        assert_eq!(ok(".gitignore").extension(), None);
        assert_eq!(ok("noext").extension(), None);
        assert_eq!(ok("trailing.").extension(), None);
        assert_eq!(RepoPath::root().extension(), None);
    }

    #[test]
    fn links_resolve_inside_the_repository_only() {
        let dir = ok("docs/guide");
        let r = |t: &str| dir.resolve_link(t).map(|p| p.as_str().to_string());
        assert_eq!(r("a.md").as_deref(), Some("docs/guide/a.md"));
        assert_eq!(r("./a.md").as_deref(), Some("docs/guide/a.md"));
        assert_eq!(r("../img/x.png").as_deref(), Some("docs/img/x.png"));
        assert_eq!(r("../../README.md").as_deref(), Some("README.md"));
        assert_eq!(r("/top.md").as_deref(), Some("top.md"));
        assert_eq!(r("/a/../b.md").as_deref(), Some("b.md"));
        assert_eq!(r("a%20b.md").as_deref(), Some("docs/guide/a b.md"));
        assert_eq!(r("a.md#section").as_deref(), Some("docs/guide/a.md"));
        assert_eq!(r("a.md?x=1#y").as_deref(), Some("docs/guide/a.md"));
        assert_eq!(r("sub//x.md").as_deref(), Some("docs/guide/sub/x.md"));
        assert_eq!(r("%2e%2e/x.md").as_deref(), Some("docs/x.md"));
        assert_eq!(r("..").as_deref(), Some("docs"));
        assert_eq!(r("./").as_deref(), Some("docs/guide"));
        for bad in [
            "",
            "#top",
            "?q",
            "../../../etc/passwd",
            "../../..",
            "/../x",
            "../..",
            "a\\b",
            "%2e%2e%2f%2e%2e%2f%2e%2e%2fx",
            "%zz",
            "%e9",
            "%00",
            "a%0ab",
            "%5c..%5cx",
            ".git/config",
            "../.git/config",
            "/",
        ] {
            assert_eq!(r(bad), None, "{bad:?}");
        }
        assert_eq!(
            RepoPath::root().resolve_link("a/../b").unwrap().as_str(),
            "b"
        );
        assert_eq!(RepoPath::root().resolve_link("../b"), None);
        let long = format!("{}.md", "a".repeat(MAX_PATH_BYTES * 3));
        assert_eq!(dir.resolve_link(&long), None);
    }

    #[test]
    fn percent_decoding_is_strict() {
        assert_eq!(percent_decode("a%41").as_deref(), Some("aA"));
        assert_eq!(percent_decode("%c3%a9").as_deref(), Some("\u{e9}"));
        assert_eq!(percent_decode("%c3"), None);
        assert_eq!(percent_decode("%"), None);
        assert_eq!(percent_decode("%4"), None);
        assert_eq!(percent_decode("%00"), None);
    }

    /// Whatever bytes arrive, a path that parses never has a form that could leave the tree.
    #[test]
    fn random_inputs_never_yield_a_dangerous_path() {
        let alphabet: Vec<char> = "ab./\\ %\u{0}\u{202e}\u{e9}\n-.git".chars().collect();
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..20_000 {
            let len = usize::try_from(next() % 12).unwrap_or(0);
            let s: String = (0..len)
                .map(|_| alphabet[usize::try_from(next()).unwrap_or(0) % alphabet.len()])
                .collect();
            for p in [RepoPath::parse(&s).ok(), RepoPath::root().resolve_link(&s)]
                .into_iter()
                .flatten()
            {
                assert!(!p.as_str().starts_with('/'), "{s:?}");
                assert!(!p.as_str().contains('\\'), "{s:?}");
                assert!(!p.as_str().contains('\0'), "{s:?}");
                assert!(
                    p.components().all(|c| component_problem(c).is_none()),
                    "{s:?}"
                );
                assert!(p.depth() <= MAX_PATH_DEPTH);
            }
        }
    }
}
