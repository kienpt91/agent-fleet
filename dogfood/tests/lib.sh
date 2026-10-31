#!/bin/sh
# Shared helpers of the shell tests (sourced, POSIX sh). Every test file ends with `finish`.
# Run them all with dogfood/tests/run.sh, inside the development container (needs git).
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
dogfood=$(CDPATH= cd -- "$here/.." && pwd)
repo_root=$(CDPATH= cd -- "$dogfood/.." && pwd)
pass=0
fail=0
work=$(mktemp -d "${TMPDIR:-/tmp}/dogfood-test.XXXXXX") || exit 2
trap 'rm -rf "$work"' EXIT HUP INT TERM
# Tests must not depend on the identity or the configuration of whoever runs them.
export HOME="$work/home"
mkdir -p "$HOME"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.invalid GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.invalid

ok() { pass=$((pass + 1)); }
bad() { fail=$((fail + 1)); printf 'FAIL: %s\n' "$1" >&2; [ -z "${2:-}" ] || printf '      %s\n' "$2" >&2; }

# expect_exit <status> <name> <command...>: runs the command, keeps its output in $out.
expect_exit() {
  want=$1; name=$2; shift 2
  out=$("$@" 2>&1)
  got=$?
  if [ "$got" -eq "$want" ]; then ok; else bad "$name (wanted exit $want, got $got)" "$out"; fi
}

# expect_out <name> <text>: $out contains the text (after expect_exit).
expect_out() {
  case $out in *"$2"*) ok ;; *) bad "$1 (output lacks: $2)" "$out" ;; esac
}

# expect_no_out <name> <text>: $out does not contain the text.
expect_no_out() {
  case $out in *"$2"*) bad "$1 (output has: $2)" "$out" ;; *) ok ;; esac
}

# expect_eq <name> <wanted> <got>
expect_eq() {
  if [ "$2" = "$3" ]; then ok; else bad "$1" "wanted '$2', got '$3'"; fi
}

# A repository in $work/<name> with one commit of the given files (paths relative, content "x").
new_repo() {
  dir=$work/$1; shift
  mkdir -p "$dir"
  git -C "$dir" init -q -b main
  for f in "$@"; do
    mkdir -p "$dir/$(dirname "$f")"
    printf 'x\n' > "$dir/$f"
  done
  git -C "$dir" add -A
  git -C "$dir" commit -q -m base --allow-empty
  printf '%s\n' "$dir"
}

commit_all() { git -C "$1" add -A && git -C "$1" commit -q -m "${2:-change}" --allow-empty; }

finish() {
  printf '%s: %d passed, %d failed\n' "$(basename "$0")" "$pass" "$fail"
  [ "$fail" -eq 0 ]
}
