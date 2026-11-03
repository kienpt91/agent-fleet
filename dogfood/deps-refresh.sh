#!/bin/sh
# Refreshes Cargo.lock under the 7 day policy of SECURITY.md. Run by a person, never by a task:
# Cargo.toml and Cargo.lock are protected paths.
#
#   deps-refresh.sh --repo <repo> --container-run <wrapper> --node <node> --age-check <check-crates-age.mjs>
#                   [--cutoff 2026-10-24T00:00:00Z]
#
# 1. refuses a repository with uncommitted changes (so Cargo.lock can be put back with git);
# 2. runs `cargo update` in the container, with network (DOGFOOD_MODE=online for this call only);
# 3. runs the age check on the result with the cutoff (default: now minus 7 days, UTC);
# 4. when any locked version is newer than the cutoff it puts Cargo.lock back and exits 1: the
#    versions that are too new must be pinned back by hand (`cargo update -p <crate> --precise <ver>`);
# 5. otherwise it leaves the new Cargo.lock in the working tree, uncommitted, and prints the
#    diffstat. You review the diff (new build scripts and procedural-macro crates against
#    SECURITY.md), run the layered tests, and commit it yourself.
# Exit status: 0 refreshed and clean, 1 too new (lock restored), 2 error.
set -u
set -f

fail() { printf 'deps-refresh: %s\n' "$1" >&2; exit 2; }

repo=; crun=; node=; age=; cutoff=
while [ $# -gt 0 ]; do
  [ $# -ge 2 ] || fail "$1 needs a value"
  case $1 in
    --repo) repo=$2 ;; --container-run) crun=$2 ;; --node) node=$2 ;; --age-check) age=$2 ;; --cutoff) cutoff=$2 ;;
    *) fail "unknown argument $1" ;;
  esac
  shift 2
done
[ -n "$repo" ] && [ -n "$crun" ] && [ -n "$node" ] && [ -n "$age" ] || fail "usage: deps-refresh.sh --repo R --container-run W --node N --age-check S [--cutoff ISO]"
for p in "$crun" "$node" "$age"; do case $p in /*) ;; *) fail "$p must be an absolute path" ;; esac; done
[ -x "$crun" ] && [ -x "$node" ] && [ -f "$age" ] || fail "the wrapper, node or the age check script is missing"
[ -d "$repo/.git" ] || fail "$repo is not a repository"
if [ -z "$cutoff" ]; then
  cutoff=$(date -u -d '7 days ago' +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -v-7d +%Y-%m-%dT%H:%M:%SZ 2>/dev/null) \
    || fail "cannot work out the cutoff; give --cutoff"
fi
case $cutoff in
  [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]Z) ;;
  *) fail "the cutoff must look like 2026-10-24T00:00:00Z" ;;
esac

git_() { GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_NOSYSTEM=1 git -C "$repo" "$@"; }
[ -z "$(git_ status --porcelain 2>/dev/null)" ] || fail "the repository has uncommitted changes: commit or stash them first"

printf 'deps-refresh: cargo update (network on for this step only)\n'
( cd "$repo" && DOGFOOD_MODE=online "$crun" 'cargo update' ) || { git_ checkout -- Cargo.lock; fail "cargo update failed; Cargo.lock was put back"; }
if [ -z "$(git_ status --porcelain Cargo.lock)" ]; then
  printf 'deps-refresh: Cargo.lock did not change: nothing to refresh\n'
  exit 0
fi

printf 'deps-refresh: age check with cutoff %s\n' "$cutoff"
result=$("$node" "$age" "$repo" "$cutoff" 2>&1)
status=$?
printf '%s\n' "$result" | tail -n 15
too_new=$(printf '%s\n' "$result" | sed -n 's/.*too new: *\([0-9][0-9]*\).*/\1/p' | tail -n 1)
if [ -z "$too_new" ]; then
  git_ checkout -- Cargo.lock
  fail "the age check did not give a clear answer (exit $status); Cargo.lock was put back"
fi
if [ "$too_new" -ne 0 ]; then
  git_ checkout -- Cargo.lock
  printf 'deps-refresh: %s version(s) are newer than %s; Cargo.lock was put back. Pin them back and run again.\n' "$too_new" "$cutoff" >&2
  exit 1
fi
if [ "$status" -ne 0 ]; then
  git_ checkout -- Cargo.lock
  fail "the age check exited $status although it found nothing too new; Cargo.lock was put back"
fi
git_ diff --stat -- Cargo.lock
printf 'deps-refresh: clean. Review the diff, run the layered tests and commit Cargo.lock yourself.\n'
