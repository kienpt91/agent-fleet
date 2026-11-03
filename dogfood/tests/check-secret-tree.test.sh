#!/bin/sh
# Tests of dogfood/check-secret-tree.sh: it must find what the scanner finds, in a directory
# that is not a git repository, and never pass when it cannot run.
. "$(dirname "$0")/lib.sh"
cs="$dogfood/check-secret-tree.sh"

mk() { # name: an export-like directory with the real scanner and an allowlist
  d=$work/$1; mkdir -p "$d/scripts" "$d/src"
  cp "$repo_root/scripts/secret-scan.sh" "$d/scripts/"
  printf '# no entries\n' > "$d/.secret-scan-allow"
  printf 'fn main() {}\n' > "$d/src/main.rs"
  printf '%s\n' "$d"
}
in_dir() { ( cd "$1" && "$cs" ); }
fake_token() { printf 'ghp_'; printf '%036d' 0; }

d=$(mk clean)
expect_exit 0 "a clean export passes" in_dir "$d"
expect_out "and the scanner says so" "secret-scan: clean (--tree)"

d=$(mk token)
fake_token > "$d/src/config.txt"; printf '\n' >> "$d/src/config.txt"
expect_exit 1 "a token-shaped string is found" in_dir "$d"
expect_out "with the rule and the file, not the value" "src/config.txt:1: github-token"
expect_no_out "the value is not printed" "ghp_0000"

d=$(mk envfile)
printf 'A=1\n' > "$d/.env"; printf '.env\n' > "$d/.gitignore"
expect_exit 1 "a forbidden file name is found even when .gitignore lists it" in_dir "$d"
expect_out "named as such" "forbidden-file-name"

d=$(mk ignored)
mkdir -p "$d/build"; fake_token > "$d/build/out.txt"; printf 'build/\n' > "$d/.gitignore"
expect_exit 1 "a secret in an ignored path of the export is still found" in_dir "$d"

d=$(mk allowed)
fake_token > "$d/src/fixture.txt"; printf '\n' >> "$d/src/fixture.txt"
printf 'src/fixture.txt github-token  # a reviewed fixture\n' > "$d/.secret-scan-allow"
expect_exit 0 "the committed allowlist is honoured" in_dir "$d"

d=$(mk marker)
printf 'token: %s secret-scan: allow\n' "$(fake_token)" > "$d/src/note.txt"
expect_exit 0 "and so is the marker on the line" in_dir "$d"

d=$(mk noscanner)
rm "$d/scripts/secret-scan.sh"
expect_exit 2 "no scanner is an error, not a pass" in_dir "$d"

d=$(mk checkout)
git -C "$d" init -q
expect_exit 2 "a directory that already has a .git is refused" in_dir "$d"

d=$(mk hostile-config)
mkdir -p "$d/.config"; printf '[core]\n\tfsmonitor = %s/never-run\n' "$work" > "$d/gitconfig-lookalike"
printf '*.txt filter=evil\n' > "$d/.gitattributes"
printf '#!/bin/sh\ntouch %s/ran\n' "$work" > "$work/never-run"; chmod +x "$work/never-run"
printf 'hello\n' > "$d/src/a.txt"
expect_exit 0 "planted attributes and configuration lookalikes change nothing" in_dir "$d"
[ ! -e "$work/ran" ] && ok || bad "a planted program ran"

d=$(mk sub-dir-start)
expect_exit 2 "started outside the root of an export" sh -c "cd '$d/src' && '$cs'"

finish
