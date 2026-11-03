#!/bin/sh
# Proves that a task changed only the TEST MODULE of one Rust file: everything before the file's
# `#[cfg(test)]` / `mod tests` pair is byte-identical in the base and in the task's head. This is the
# mechanical half of a reviewed exception (check-protected.sh --except): a person still reads the diff,
# but the production code above the test module cannot have changed behind the diff's back.
#
#   check-test-only.sh --repo DIR --file crates/<crate>/src/x.rs <base>..<head>
#
# The module marker is the first line that is exactly `#[cfg(test)]` and is followed by a line of the
# form `mod <name> {` (also `pub mod` and `pub(crate) mod`: a file may have a test_support module
# first). It must exist in the base and in the head. What is compared is the text above and including
# that attribute line (so a changed `use` of the production part, an edit of the marker or an inserted item
# above it fails). It does NOT prove that nothing after the test modules is production code (tests are by
# convention the last items of a file): that is what the person who approves the merge reads. A file
# that has no such module, or whose first `#[cfg(test)]` attribute is on something else (a test-only
# import, say), is refused as "cannot judge": fails closed.
#
# Exit status: 0 only the test module changed (or nothing), 1 something above it changed, 2 cannot judge.
set -u
set -f

fail() { printf 'check-test-only: %s\n' "$1" >&2; exit 2; }

repo=.; file=; range=
while [ $# -gt 0 ]; do
  case $1 in
    --repo) [ $# -ge 2 ] || fail "--repo needs a directory"; repo=$2; shift 2 ;;
    --file) [ $# -ge 2 ] || fail "--file needs a path"; file=$2; shift 2 ;;
    --) shift; break ;;
    -*) fail "unknown option $1" ;;
    *) break ;;
  esac
done
case $# in
  1) range=$1 ;;
  2) range="$1..$2" ;;
  *) fail "usage: check-test-only.sh --repo DIR --file PATH <base>..<head>" ;;
esac
[ -d "$repo" ] || fail "not a directory: $repo"
case $file in
  ""|*[!A-Za-z0-9._/-]*|/*|*//*|*/|*..*) fail "--file: a plain relative path without wildcards is needed" ;;
  crates/*/src/*.rs|crates/*/tests/*.rs) ;;
  *) fail "--file: only a Rust source or test file of a crate can be judged" ;;
esac
case $range in
  -*|*[!A-Za-z0-9._/@^~-]*|"") fail "refusing the range: it must consist of ref characters only" ;;
esac
case $range in *..*) ;; *) fail "the range must look like <base>..<head>" ;; esac
base=${range%%..*}
head=${range#*..}
head=${head#.}
[ -n "$base" ] && [ -n "$head" ] || fail "the range must look like <base>..<head>"

g() { GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0 git -C "$repo" "$@"; }
tmp=$(mktemp -d "${TMPDIR:-/tmp}/check-test-only.XXXXXX") || fail "cannot create a temporary directory"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

# the same file at the two commits; a blob only, no checkout, no filters
g rev-parse --verify --quiet "$base^{commit}" >/dev/null || fail "$base is not a commit of $repo"
g rev-parse --verify --quiet "$head^{commit}" >/dev/null || fail "$head is not a commit of $repo"
g cat-file blob "$base:$file" > "$tmp/base" 2>/dev/null || fail "$file does not exist at $base"
if ! g cat-file blob "$head:$file" > "$tmp/head" 2>/dev/null; then
  printf 'check-test-only: %s was deleted or does not exist at %s\n' "$file" "$head" >&2
  exit 1
fi

# the line number of the marker: the first `#[cfg(test)]` line must be followed by `mod <name> {`
marker() { # file -> prints the line number or nothing
  awk '
    prev_cfg { if ($0 ~ /^(pub(\([a-z]+\))? )?mod [A-Za-z_][A-Za-z0-9_]* \{[ \t]*$/) { print NR - 1; exit } else { exit 2 } }
    $0 == "#[cfg(test)]" { prev_cfg = 1 }
  ' "$1"
}
mb=$(marker "$tmp/base"); sb=$?
mh=$(marker "$tmp/head"); sh_=$?
[ -n "$mb" ] && [ "$sb" -eq 0 ] || fail "$file has no \`#[cfg(test)]\` followed by \`mod <name> {\` at $base (or another #[cfg(test)] item comes first): cannot judge"
[ -n "$mh" ] && [ "$sh_" -eq 0 ] || { printf 'check-test-only: %s has no test module marker at %s any more\n' "$file" "$head" >&2; exit 1; }

# everything above the marker, byte for byte (the marker line itself is part of what is compared)
head -n "$((mb))" "$tmp/base" > "$tmp/base.above"
head -n "$((mh))" "$tmp/head" > "$tmp/head.above"
if [ "$mb" -ne "$mh" ] || ! cmp -s "$tmp/base.above" "$tmp/head.above"; then
  printf 'check-test-only: the text of %s above its test module changed between %s and %s: production code is not a test-only change\n' "$file" "$base" "$head" >&2
  exit 1
fi
printf 'check-test-only: %s: the first %s line(s), up to and including `#[cfg(test)]`, are identical in %s and %s\n' "$file" "$mb" "$base" "$head"
exit 0
