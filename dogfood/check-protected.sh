#!/bin/sh
# Fails when a range of commits changes a path that NEO may never change by itself.
#
#   check-protected.sh [--repo DIR] [--list FILE] [--except FILE]... <base>..<head>
#   check-protected.sh [--repo DIR] [--list FILE] [--except FILE]... <base>...<head>   (relative to their merge base)
#   check-protected.sh [--repo DIR] [--list FILE] [--except FILE]... <base> <head>
#
# The protected list is dogfood/protected-paths.json next to this script (or --list). Every
# `"path": "<glob>"` line of it is one entry, in the glob dialect of the neo-verify diff check
# (`*` and `?` inside a segment, `**` as a whole segment). Used by the human reviewer before a
# merge and as a command check of the contracts in dogfood/contracts/.
#
# Stricter than the diff check on purpose (defence in depth, it fails closed):
#   - paths are compared case-insensitively (a case-insensitive checkout maps `Security.md` to
#     `SECURITY.md`);
#   - renames are not followed: both the old and the new path are looked at;
#   - a path that git must quote (control characters including newline and tab, a backslash, a
#     double quote, non-ASCII bytes), a path segment that starts or ends with a space and a
#     `.git` segment are refused outright: a human has to look at them;
#   - a symlink or a submodule added or changed anywhere is refused (a link can point into a
#     protected path);
#   - git runs with no global or system configuration and without renames, external diff
#     drivers or text conversion.
#
# --except FILE is a REVIEWED EXCEPTION for one task (a flaky test that lives in a protected source
# file, see contracts/README.md): that exact path, and only it, is not reported as protected. It must
# be a Rust source or test file of a crate (`crates/<crate>/src/**.rs` or `crates/<crate>/tests/**.rs`), written
# without wildcards; a build script, the architecture tests, a manifest and anything else is refused. It
# does not excuse a symlink, a submodule or an odd path, and a path that differs from it in case is not
# excepted. Every exception is printed. The exception only means "a person reviews this diff": pair it with
# check-test-only.sh, which proves that nothing before the test module changed.
#
# Exit status: 0 nothing protected was touched, 1 something was, 2 usage or environment error
# (an unreadable list, a bad ref, git failing: never a pass).
set -u
set -f

fail_usage() { printf 'check-protected: %s\n' "$1" >&2; exit 2; }

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || fail_usage "cannot find the script directory"
repo=.
list="$script_dir/protected-paths.json"
range=
excepts=
nl='
'
while [ $# -gt 0 ]; do
  case $1 in
    --repo) [ $# -ge 2 ] || fail_usage "--repo needs a directory"; repo=$2; shift 2 ;;
    --list) [ $# -ge 2 ] || fail_usage "--list needs a file"; list=$2; shift 2 ;;
    --except)
      [ $# -ge 2 ] || fail_usage "--except needs a path"
      case $2 in
        ""|*[!A-Za-z0-9._/-]*|/*|*//*|*/|*..*) fail_usage "--except: a plain relative path without wildcards is needed" ;;
      esac
      case $2 in
        */build.rs|crates/fleet-cli/tests/architecture.rs) fail_usage "--except: $2 can never be excepted" ;;
        crates/*/src/*.rs|crates/*/tests/*.rs) ;;
        *) fail_usage "--except: only a Rust source or test file of a crate can be excepted (crates/<crate>/src/**.rs or crates/<crate>/tests/**.rs)" ;;
      esac
      excepts="$excepts$2$nl"; shift 2 ;;
    --) shift; break ;;
    -*) fail_usage "unknown option $1" ;;
    *) break ;;
  esac
done
case $# in
  1) range=$1 ;;
  2) range="$1..$2" ;;
  *) fail_usage "usage: check-protected.sh [--repo DIR] [--list FILE] [--except FILE]... <base>..<head> | <base> <head>" ;;
esac
[ -d "$repo" ] || fail_usage "not a directory: $repo"
[ -f "$list" ] && [ -r "$list" ] || fail_usage "cannot read the protected list $list"
# A range is made of ref characters only, and never starts with a dash (no option injection).
case $range in
  -*|*[!A-Za-z0-9._/@^~-]*|"") fail_usage "refusing the range: it must consist of ref characters only" ;;
esac
case $range in
  *..*) ;;
  *) fail_usage "the range must look like <base>..<head>" ;;
esac

lower() { printf '%s' "$1" | tr 'ABCDEFGHIJKLMNOPQRSTUVWXYZ' 'abcdefghijklmnopqrstuvwxyz'; }

# ---- the protected list ----------------------------------------------------------------------------
patterns=
entries=0
nl='
'
add_pattern() { patterns="$patterns$nl$1"; }
paths=$(sed -n 's/.*"path": *"\([^"]*\)".*/\1/p' "$list") || fail_usage "cannot read $list"
[ -n "$paths" ] || fail_usage "no entries found in $list"
declared=$(( $(grep -o '"path"' "$list" | wc -l) + 0 ))
for raw in $paths; do
  entries=$((entries + 1))
  p=$(lower "$raw")
  case $p in
    ""|/*|*/|*[!a-z0-9._*/?-]*|*..*|*//*) fail_usage "unusable entry in the protected list: $raw" ;;
  esac
  case $p in
    */\*\*)
      base=${p%/\*\*}
      add_pattern "$base"
      add_pattern "$base/*"
      ;;
    *)
      add_pattern "$(printf '%s' "$p" | sed 's/\*\*/*/g')"
      case $p in
        */\*\*/*) add_pattern "$(printf '%s' "$p" | sed 's|/\*\*/|/|g; s/\*\*/*/g')" ;;
      esac
      ;;
  esac
done
[ "$entries" -eq "$declared" ] || fail_usage "the protected list has $declared \"path\" keys but $entries usable entries (one entry per line)"

# Prints the pattern that matches the (lowercased) path and returns 0, or returns 1.
protected_by() {
  for pat in $patterns; do
    [ -n "$pat" ] || continue
    case $1 in
      $pat) printf '%s' "$pat"; return 0 ;;
    esac
  done
  return 1
}

# ---- what changed ----------------------------------------------------------------------------------
raw=$(GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0 \
  git -C "$repo" -c core.quotePath=true -c diff.renames=false -c core.fsmonitor=false \
  diff --raw --no-abbrev --no-renames --no-ext-diff --no-textconv "$range" -- 2>/dev/null) \
  || fail_usage "git could not diff $range in $repo"

tab=$(printf '\t')
violations=0
total=0
report() { printf 'check-protected: %s\n' "$1" >&2; violations=$((violations + 1)); }

# Reads the raw lines through a here-document file so the loop runs in this shell.
tmp=$(mktemp "${TMPDIR:-/tmp}/check-protected.XXXXXX") || fail_usage "cannot create a temporary file"
trap 'rm -f "$tmp"' EXIT HUP INT TERM
printf '%s\n' "$raw" > "$tmp"
while IFS= read -r line; do
  [ -n "$line" ] || continue
  total=$((total + 1))
  meta=${line%%"$tab"*}
  path=${line#*"$tab"}
  case $line in
    ":"*"$tab"*) ;;
    *) report "cannot read a line of the diff"; continue ;;
  esac
  # ":<old mode> <new mode> <old sha> <new sha> <status>"
  set -- $meta
  new_mode=${2:-}
  case $new_mode in
    120000) report "SYMLINK added or changed: $path (a link may point into a protected path; a human decides)" ;;
    160000) report "SUBMODULE added or changed: $path" ;;
  esac
  case $path in
    '"'*) report "UNUSUAL PATH, git had to quote it: $path"; continue ;;
    " "*|*" "|*" /"*|*"/ "*) report "UNUSUAL PATH, a segment starts or ends with a space: $path"; continue ;;
  esac
  lp=$(lower "$path")
  case $lp in
    .git|.git/*|*/.git|*/.git/*) report "UNUSUAL PATH, a .git segment: $path"; continue ;;
  esac
  if hit=$(protected_by "$lp"); then
    excepted=0
    # a reviewed exception is an exact, case-sensitive match of the whole path
    if [ -n "$excepts" ]; then
      oldifs=$IFS; IFS=$nl
      for e in $excepts; do [ "$path" = "$e" ] && excepted=1; done
      IFS=$oldifs
    fi
    if [ "$excepted" -eq 1 ]; then
      printf 'check-protected: REVIEWED EXCEPTION: %s is protected (matches %s) and is changed here; a person reads this diff\n' "$path" "$hit" >&2
    else
      report "PROTECTED: $path (matches $hit)"
    fi
  fi
done < "$tmp"

if [ "$violations" -gt 0 ]; then
  printf 'check-protected: %s problem(s) in %s changed path(s) of %s; these paths need a human\n' \
    "$violations" "$total" "$range" >&2
  exit 1
fi
printf 'check-protected: %s changed path(s) in %s, none protected (%s entries)%s\n' "$total" "$range" "$entries" "$([ -n "$excepts" ] && printf ', exceptions: %s' "$(printf '%s' "$excepts" | tr '\n' ' ')")"
exit 0
