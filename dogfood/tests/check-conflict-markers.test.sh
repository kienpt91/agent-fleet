#!/bin/sh
# Tests of dogfood/check-conflict-markers.sh.
. "$(dirname "$0")/lib.sh"
cc="$dogfood/check-conflict-markers.sh"
in_dir() { ( cd "$1" && "$cc" --tree ); }
# marker lines are assembled at run time so this file holds none itself
lt=$(printf '<<<<<<<'); eq=$(printf '======='); gt=$(printf '>>>>>>>')

d=$work/clean; mkdir -p "$d/src" "$d/docs"
printf 'fn main() {}\n' > "$d/src/main.rs"
printf '# Title\n%s\n\nText\n' "$eq" > "$d/docs/heading.md"
printf 'a heading\n=========\n' > "$d/docs/h2.md"
printf 'x %s y\n' "$lt" > "$d/docs/inline.md"
expect_exit 0 "a clean tree passes (an underline of equal signs and inline text are not markers)" in_dir "$d"
expect_out "and says so" "clean (--tree)"

d=$work/full; mkdir -p "$d/src"
printf 'a\n%s HEAD\nours\n%s\ntheirs\n%s feature\nb\n' "$lt" "$eq" "$gt" > "$d/src/x.rs"
expect_exit 1 "a full conflict block" in_dir "$d"
expect_out "the file is named" "src/x.rs"
expect_out "with the lines of all three markers" "lines 2 4 6"
expect_no_out "and no content" "ours"

d=$work/start-only; mkdir -p "$d"
printf '%s\nx\n' "$lt" > "$d/a.txt"
expect_exit 1 "a start marker alone, at the end of a line too" in_dir "$d"
d=$work/end-only; mkdir -p "$d"
printf 'x\n%s some-branch\n' "$gt" > "$d/a.txt"
expect_exit 1 "an end marker alone" in_dir "$d"
d=$work/space-name; mkdir -p "$d/dir with space"
printf '%s\n' "$lt" > "$d/dir with space/my file.txt"
expect_exit 1 "a path with spaces" in_dir "$d"
expect_out "is named whole" "dir with space/my file.txt"
d=$work/hidden; mkdir -p "$d/.hidden"
printf '%s\n' "$gt" > "$d/.hidden/f"
expect_exit 1 "a hidden directory is looked at too" in_dir "$d"
d=$work/near; mkdir -p "$d"
printf '<<<<<< six\n>>>>>>>> eight\n <<<<<<< indented\n%sx\n' "$lt" > "$d/a.txt"
expect_exit 0 "near misses are not markers (six, eight, indented, a suffix)" in_dir "$d"
d=$work/binary; mkdir -p "$d"
printf '\000\001%s\n' "$lt" > "$d/blob.bin"
expect_exit 0 "a binary file is not read" in_dir "$d"
d=$work/gitdir; mkdir -p "$d/.git"
printf '%s\n' "$lt" > "$d/.git/MERGE_MSG"
expect_exit 0 "the .git directory is not part of the tree" in_dir "$d"
d=$work/empty; mkdir -p "$d"
expect_exit 0 "an empty directory" in_dir "$d"

# ---- --git: a repository before a commit -------------------------------------------------------------
r=$(new_repo g1 src/a.rs docs/b.md)
expect_exit 0 "a clean repository" "$cc" --git "$r"
printf '%s\nours\n%s\ntheirs\n%s\n' "$lt" "$eq" "$gt" > "$r/src/a.rs"
expect_exit 1 "markers in a tracked file" "$cc" --git "$r"
expect_out "the file and lines" "src/a.rs: lines 1 3 5"
git -C "$r" checkout -q -- src/a.rs
printf '%s\n' "$lt" > "$r/untracked.txt"
expect_exit 1 "markers in an untracked file" "$cc" --git "$r"
rm "$r/untracked.txt"
printf '%s\n' "$lt" > "$r/ignored.txt"; printf 'ignored.txt\n' > "$r/.gitignore"
expect_exit 0 "but not in an ignored one" "$cc" --git "$r"
rm "$r/ignored.txt" "$r/.gitignore"

# a real cherry-pick -n conflict, as in the runbook
r=$(new_repo g2 notes.txt)
printf 'one\ntwo\nthree\n' > "$r/notes.txt"; commit_all "$r" base
git -C "$r" checkout -q -b feature
printf 'one\nTWO FROM FLEET\nthree\n' > "$r/notes.txt"; commit_all "$r" fleet
fleet_commit=$(git -C "$r" rev-parse HEAD)
git -C "$r" checkout -q main
printf 'one\ntwo changed meanwhile\nthree\n' > "$r/notes.txt"; commit_all "$r" main
git -C "$r" cherry-pick -n "$fleet_commit" >/dev/null 2>&1 || true
expect_exit 1 "a conflicted cherry-pick -n is caught" "$cc" --git "$r"
expect_out "as unmerged" "unmerged: notes.txt"
expect_out "and as markers" "notes.txt: lines"
git -C "$r" reset -q --hard
git -C "$r" cherry-pick -n "$fleet_commit" >/dev/null 2>&1 || true
printf 'one\nresolved\nthree\n' > "$r/notes.txt"
expect_exit 1 "resolved in the file but still unmerged in the index" "$cc" --git "$r"
git -C "$r" add notes.txt
expect_exit 0 "resolved and staged" "$cc" --git "$r"

expect_exit 2 "not a repository" "$cc" --git "$work/clean"
expect_exit 2 "not a directory" "$cc" --git "$work/nope"
expect_exit 2 "no mode" "$cc"
expect_exit 2 "an unknown mode" "$cc" --staged
expect_exit 2 "--tree takes no directory" "$cc" --tree "$work"

finish
