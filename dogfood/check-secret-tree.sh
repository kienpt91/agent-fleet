#!/bin/sh
# Runs the repository's secret scanner (`scripts/secret-scan.sh --tree`) on the directory it is
# started in. A verification check runs in an export of a commit: the files and nothing else, so
# there is no git repository for the scanner to ask which files are tracked. This builds a
# throwaway index for exactly those files (ignored files included: a file the export holds is a
# file the commit holds, whatever .gitignore says) in a git directory outside the export, then
# runs the scanner of the export itself. The scanner and the allowlist it reads are the committed
# ones; both are protected paths, and the diff check of the same contract fails if a task
# changed them.
#
#   cd <export of a commit> && check-secret-tree.sh
#
# Exit status: 0 clean, 1 findings (the scanner's own), 2 cannot run (never a pass).
set -u

fail_usage() { printf 'check-secret-tree: %s\n' "$1" >&2; exit 2; }

[ -f scripts/secret-scan.sh ] || fail_usage "scripts/secret-scan.sh is not here: run this in the root of an export of a commit"
[ ! -e .git ] || fail_usage "there is a .git here: this is a checkout, run 'sh scripts/secret-scan.sh --tree' in it instead"

gitdir=$(mktemp -d "${TMPDIR:-/tmp}/check-secret-tree.XXXXXX") || fail_usage "cannot create a temporary directory"
trap 'rm -rf "$gitdir"' EXIT HUP INT TERM

export GIT_DIR="$gitdir" GIT_WORK_TREE="$PWD"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_ATTR_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0
git init -q 2>/dev/null || fail_usage "git init failed"
git add -A -f >/dev/null 2>&1 || fail_usage "git add failed"
sh scripts/secret-scan.sh --tree
