#!/bin/sh
# Fails when a range of commits edits a number that documents a measurement: test counts,
# coverage figures, the number of checks of a script, the locked-crate count of the age check,
# throughput and memory figures of the soak run. Those numbers are produced by the human-run
# verification (scripts/test-layers.sh, scripts/coverage.sh, the soak test, check-crates-age) and
# pasted in by a person; a task must not change them, whatever it believes the counts are.
#
#   check-docs-claims.sh [--repo DIR] <base>..<head> | <base>...<head> | <base> <head>
#
# Looks at README.md and everything under docs/. Both the removed and the added lines of the
# diff are examined, so an edit and a deletion are caught alike. Exit status: 0 nothing like it
# changed, 1 a claim line changed, 2 usage or environment error (never a pass).
set -u
set -f

fail_usage() { printf 'check-docs-claims: %s\n' "$1" >&2; exit 2; }

repo=.
while [ $# -gt 0 ]; do
  case $1 in
    --repo) [ $# -ge 2 ] || fail_usage "--repo needs a directory"; repo=$2; shift 2 ;;
    --) shift; break ;;
    -*) fail_usage "unknown option $1" ;;
    *) break ;;
  esac
done
case $# in
  1) range=$1 ;;
  2) range="$1..$2" ;;
  *) fail_usage "usage: check-docs-claims.sh [--repo DIR] <base>..<head> | <base> <head>" ;;
esac
[ -d "$repo" ] || fail_usage "not a directory: $repo"
case $range in
  -*|*[!A-Za-z0-9._/@^~-]*|"") fail_usage "refusing the range: it must consist of ref characters only" ;;
esac
case $range in *..*) ;; *) fail_usage "the range must look like <base>..<head>" ;; esac

# One extended regular expression per line. A changed line that matches any of them is a claim.
claims='[0-9][0-9,.]*[[:space:]]+(tests|checks|locked crates|records)([^[:alnum:]]|$)
^[[:space:]]*\|[[:space:]]*(Domain|Application|Adapters|Soak|End to end)[[:space:]]*\|
^[[:space:]]*(domain|application|infrastructure|cli|overall)[[:space:]]+[0-9]+[[:space:]]+[0-9]+
[0-9]+\.[0-9]%
MiB/s
[0-9]+ passed, [0-9]+ failed
growth after warm-up'

diff_out=$(GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0 \
  git -C "$repo" -c core.fsmonitor=false -c core.quotePath=true \
  diff -U0 --no-color --no-renames --no-ext-diff --no-textconv "$range" -- README.md docs 2>/dev/null) \
  || fail_usage "git could not diff $range in $repo"

# The changed content lines (not the file headers), printable characters only.
changed=$(printf '%s\n' "$diff_out" | awk '
  /^diff --git / { header = 1; next }
  /^@@/ { header = 0; next }
  header { next }
  /^[+-]/ { print substr($0, 2) }
' | LC_ALL=C tr -cd '[:print:]\n')

hits=$(printf '%s\n' "$changed" | grep -E -i -e "$claims" || true)
if [ -n "$hits" ]; then
  count=$(printf '%s\n' "$hits" | wc -l | tr -d ' ')
  printf 'check-docs-claims: %s changed line(s) of README.md or docs/ state a measured number; only the human-run verification may change them:\n' "$count" >&2
  printf '%s\n' "$hits" | head -n 10 | cut -c1-200 | sed 's/^/  /' >&2
  exit 1
fi
lines=$(printf '%s\n' "$changed" | grep -c . || true)
printf 'check-docs-claims: %s changed documentation line(s) in %s, no measured number among them\n' "$lines" "$range"
exit 0
