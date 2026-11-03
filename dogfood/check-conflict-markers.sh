#!/bin/sh
# Fails when a tree holds merge conflict markers. Taking fleet's commits into another branch
# (`git cherry-pick -n <commit>` and re-authoring it) can leave `<<<<<<<`, `=======` and `>>>>>>>` in a file
# whose neighbouring lines changed, and a commit made without looking carries them into the history.
#
#   check-conflict-markers.sh --tree           the directory you are in (an export of a commit has no .git): a contract check
#   check-conflict-markers.sh --git [<dir>]    a repository, before you commit: unmerged index entries, and the tracked and
#                                              untracked, non-ignored files of its working tree
#
# A file is reported when it has a line that starts with `<<<<<<<` or `>>>>>>>` followed by a space or
# the end of the line (the shape git writes). A bare `=======` is not looked for on its own: it is
# also the underline of a heading in markdown and of a diff in a test; but a file that has one next to a
# start or end marker is reported with it, and `--git` lists the line numbers of all three. A report
# names the file and line, never the content. Binary files are not read.
# Exit status: 0 clean, 1 markers found, 2 cannot run (never a pass).
set -u
set -f

fail() { printf 'check-conflict-markers: %s\n' "$1" >&2; exit 2; }

mode=${1:-}
dir=.
case $mode in
  --tree) [ $# -eq 1 ] || fail "usage: check-conflict-markers.sh --tree | --git [<dir>]" ;;
  --git) [ $# -le 2 ] || fail "usage: check-conflict-markers.sh --tree | --git [<dir>]"; dir=${2:-.} ;;
  *) fail "usage: check-conflict-markers.sh --tree | --git [<dir>]" ;;
esac
[ -d "$dir" ] || fail "not a directory: $dir"

found=0

case $mode in
  --tree)
    list=$(mktemp "${TMPDIR:-/tmp}/ccm.XXXXXX") || fail "cannot create a temporary file"
    trap 'rm -f "$list" "$list.hits"' EXIT HUP INT TERM
    # no .git directory; NUL separated so any name is safe
    find "$dir" -name .git -prune -o -type f -print0 > "$list" || fail "cannot list the tree"
    if [ -s "$list" ]; then
      xargs -0 grep -I -l -E -e '^(<<<<<<<|>>>>>>>)( |$)' < "$list" > "$list.hits" 2>/dev/null || true
      if [ -s "$list.hits" ]; then
        while IFS= read -r f; do
          printf 'check-conflict-markers: %s: lines %s\n' "$f" "$(grep -I -n -E -e '^(<<<<<<<|=======|>>>>>>>)( |$)' "$f" | cut -d: -f1 | tr '\n' ' ')" >&2
        done < "$list.hits"
        rm -f "$list.hits"
        printf 'check-conflict-markers: conflict markers in the tree: resolve them\n' >&2
        exit 1
      fi
      rm -f "$list.hits"
    fi
    printf 'check-conflict-markers: clean (--tree)\n'
    ;;
  --git)
    g() { GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_NOSYSTEM=1 git -C "$dir" "$@"; }
    g rev-parse --git-dir >/dev/null 2>&1 || fail "$dir is not a git repository"
    unmerged=$(g ls-files -u 2>/dev/null | cut -f2 | sort -u) || fail "git ls-files failed"
    if [ -n "$unmerged" ]; then
      printf '%s\n' "$unmerged" | while IFS= read -r f; do printf 'check-conflict-markers: unmerged: %s\n' "$f" >&2; done
      found=1
    fi
    files=$(g ls-files -z --cached --others --exclude-standard 2>/dev/null | tr '\0' '\n') || fail "git ls-files failed"
    tmp=$(mktemp "${TMPDIR:-/tmp}/ccm.XXXXXX") || fail "cannot create a temporary file"
    trap 'rm -f "$tmp"' EXIT HUP INT TERM
    printf '%s\n' "$files" | while IFS= read -r f; do
      [ -f "$dir/$f" ] && [ ! -L "$dir/$f" ] || continue
      if grep -I -q -E -e '^(<<<<<<<|>>>>>>>)( |$)' "$dir/$f" 2>/dev/null; then
        printf 'check-conflict-markers: %s: lines %s\n' "$f" "$(grep -I -n -E -e '^(<<<<<<<|=======|>>>>>>>)( |$)' "$dir/$f" | cut -d: -f1 | tr '\n' ' ')" >> "$tmp"
      fi
    done
    if [ -s "$tmp" ]; then cat "$tmp" >&2; found=1; fi
    if [ "$found" -ne 0 ]; then
      printf 'check-conflict-markers: conflict markers or unmerged paths: resolve them before you commit\n' >&2
      exit 1
    fi
    printf 'check-conflict-markers: clean (--git)\n'
    ;;
esac
exit 0
