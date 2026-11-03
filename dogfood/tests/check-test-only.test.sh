#!/bin/sh
# Tests of dogfood/check-test-only.sh.
. "$(dirname "$0")/lib.sh"
ct="$dogfood/check-test-only.sh"
F=crates/demo/src/lib.rs

base_src() {
  cat <<'RS'
//! A demo module.
use std::fmt;

pub fn add(a: u32, b: u32) -> u32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(1, 2), 3);
    }
}
RS
}
mk() { # name: a repository whose base commit has the demo file
  r=$(new_repo "$1" README.md)
  mkdir -p "$r/crates/demo/src"
  base_src > "$r/$F"
  commit_all "$r" base
  git -C "$r" tag -f base >/dev/null
  printf '%s\n' "$r"
}
run() { "$ct" --repo "$1" --file "${2:-$F}" "${3:-base..HEAD}"; }

r=$(mk same)
commit_all "$r" nothing
expect_exit 0 "no change at all" run "$r"
expect_out "says what is identical" "up to and including"

r=$(mk test-change)
sed 's/assert_eq!(add(1, 2), 3);/assert_eq!(add(1, 2), 3);\n        assert_eq!(add(2, 2), 4);/' "$r/$F" > "$r/x" && mv "$r/x" "$r/$F"
printf '\n    #[test]\n    fn more() {\n        assert!(true);\n    }\n' >> "$r/$F"
commit_all "$r" tests
expect_exit 0 "a change inside the test module" run "$r"

r=$(mk prod-change)
sed 's/a + b/a + b + 0/' "$r/$F" > "$r/x" && mv "$r/x" "$r/$F"
commit_all "$r" prod
expect_exit 1 "a change of production code above the test module" run "$r"
expect_out "is named as production code" "production code is not a test-only change"

r=$(mk import)
sed 's/use std::fmt;/use std::fmt;\nuse std::io;/' "$r/$F" > "$r/x" && mv "$r/x" "$r/$F"
commit_all "$r" import
expect_exit 1 "an added import above the module" run "$r"

r=$(mk comment)
sed 's|//! A demo module.|//! A demo module, edited.|' "$r/$F" > "$r/x" && mv "$r/x" "$r/$F"
commit_all "$r" doc
expect_exit 1 "even a comment above the module counts: the person decides" run "$r"

r=$(mk attr)
sed 's/^#\[cfg(test)\]$/#[cfg(not(test))]/' "$r/$F" > "$r/x" && mv "$r/x" "$r/$F"
commit_all "$r" attr
expect_exit 1 "the marker itself changed" run "$r"

r=$(mk moved)
{ printf 'pub fn sneaky() {}\n'; cat "$r/$F"; } > "$r/x" && mv "$r/x" "$r/$F"
commit_all "$r" moved
expect_exit 1 "an item inserted above the module moves the marker" run "$r"

r=$(mk removed)
sed -n '1,7p' "$r/$F" > "$r/x" && mv "$r/x" "$r/$F"
commit_all "$r" removed
expect_exit 1 "the test module removed" run "$r"
expect_out "is said" "no test module marker"

r=$(mk deleted)
git -C "$r" rm -q "$F"; commit_all "$r" deleted
expect_exit 1 "the file deleted" run "$r"

r=$(mk after-module)
printf '\npub fn after() {}\n' >> "$r/$F"
commit_all "$r" after
expect_exit 0 "a change below the marker is not detected (documented: a person reads the diff)" run "$r"

r=$(mk other-file)
printf 'x\n' >> "$r/README.md"; commit_all "$r" other
expect_exit 0 "a change of another file does not matter to this check" run "$r"

# ---- cannot judge ---------------------------------------------------------------------------------------
r=$(new_repo nomod README.md)
mkdir -p "$r/crates/demo/src"; printf 'pub fn a() {}\n' > "$r/$F"; commit_all "$r" base; git -C "$r" tag -f base >/dev/null
commit_all "$r" next
expect_exit 2 "a file without a test module" run "$r"
r=$(new_repo import-first README.md)
mkdir -p "$r/crates/demo/src"; printf '#[cfg(test)]\nuse std::io;\n\npub fn a() {}\n\n#[cfg(test)]\nmod tests {\n}\n' > "$r/$F"
commit_all "$r" base; git -C "$r" tag -f base >/dev/null; commit_all "$r" next
expect_exit 2 "a test-only import before the module is refused, not guessed at" run "$r"
r=$(mk pubmod)
printf '#[cfg(test)]\npub(crate) mod test_support {\n}\n\n#[cfg(test)]\nmod tests {\n}\n' >> "$r/$F"
git -C "$r" add -A; git -C "$r" commit -q -m support; git -C "$r" tag -f base >/dev/null
printf '// x\n' >> "$r/$F"; commit_all "$r" tail
expect_exit 0 "a pub(crate) test_support module counts as a test module" run "$r"
r=$(mk missingfile)
expect_exit 2 "a file that does not exist at the base" run "$r" crates/demo/src/nope.rs

# ---- arguments ------------------------------------------------------------------------------------------
r=$(mk args)
commit_all "$r" c
expect_exit 2 "a path outside crates" run "$r" README.md
expect_exit 2 "a build script" run "$r" crates/demo/build.rs
expect_exit 2 "a path with .." run "$r" crates/demo/src/../x.rs
expect_exit 2 "a path with a wildcard" run "$r" 'crates/demo/src/*.rs'
expect_exit 2 "an absolute path" run "$r" /etc/passwd
expect_exit 2 "an unknown base" run "$r" "$F" nope..HEAD
expect_exit 2 "a range with an option" run "$r" "$F" --output=x..HEAD
expect_exit 2 "no range" "$ct" --repo "$r" --file "$F"
expect_exit 2 "not a directory" "$ct" --repo "$work/none" --file "$F" base..HEAD
expect_exit 2 "an unknown option" "$ct" --frob base..HEAD

finish
