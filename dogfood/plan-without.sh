#!/bin/sh
# Prints a plan file without the tasks that are already done, for the next stage of the roadmap.
#
#   plan-without.sh <plan.toml> <task-id>[,<task-id>...]
#
# fleet cannot mark a task completed by hand, so work that a person did (the human-only tasks)
# or that an earlier run merged is taken out of the plan before the next run is started. The
# listed tasks are removed and so is every mention of them in the `after` lists of the others.
# The plan files of this directory keep one task per `[[tasks]]` table with `id = "..."` and
# `after = [...]` on single lines; a list that spans lines is refused rather than guessed at.
# An id that is not in the plan is an error (a typo must not silently keep a task).
set -u
set -f

fail() { printf 'plan-without: %s\n' "$1" >&2; exit 2; }

[ $# -eq 2 ] || fail "usage: plan-without.sh <plan.toml> <task-id>[,<task-id>...]"
plan=$1; skip=$2
[ -f "$plan" ] && [ -r "$plan" ] || fail "cannot read $plan"
case $skip in *[!a-z0-9_,-]*|"") fail "the ids may contain only a-z 0-9 _ - and commas" ;; esac

result=$(awk -v skip="$skip" '
  function flush(   i) {
    if (n == 0) return
    if (!(id in drop)) {
      for (i = 1; i <= n; i++) if (buf[i] != DELETED) print buf[i]
    }
    n = 0; id = ""
  }
  function rewrite_after(line,   body, parts, k, out, count, item, kept) {
    body = line
    sub(/^[ \t]*after[ \t]*=[ \t]*\[/, "", body)
    sub(/\][ \t]*(#.*)?$/, "", body)
    count = split(body, parts, ",")
    out = ""; kept = 0
    for (k = 1; k <= count; k++) {
      item = parts[k]
      gsub(/^[ \t"]+|[ \t"]+$/, "", item)
      if (item == "") continue
      if (item in drop) continue
      out = out (kept ? ", " : "") "\"" item "\""
      kept++
    }
    if (kept == 0) return DELETED
    return "after = [" out "]"
  }
  BEGIN {
    DELETED = "\001deleted\001"
    m = split(skip, ids, ",")
    for (i = 1; i <= m; i++) if (ids[i] != "") drop[ids[i]] = 1
    n = 0; intask = 0; bad = 0
  }
  /^\[\[tasks\]\][ \t]*$/ { flush(); intask = 1; n = 1; buf[n] = $0; next }
  /^\[/ { if (intask) { flush(); intask = 0 } print; next }
  !intask { print; next }
  /^[ \t]*id[ \t]*=/ {
    v = $0; sub(/^[ \t]*id[ \t]*=[ \t]*"/, "", v); sub(/".*$/, "", v)
    id = v; present[v] = 1
    buf[++n] = $0; next
  }
  /^[ \t]*after[ \t]*=/ {
    if ($0 !~ /\][ \t]*(#.*)?$/) { print "plan-without: an after list spans several lines" > "/dev/stderr"; bad = 1 }
    buf[++n] = rewrite_after($0); next
  }
  { buf[++n] = $0 }
  END {
    flush()
    for (d in drop) if (!(d in present)) { print "plan-without: no task has the id " d > "/dev/stderr"; bad = 1 }
    exit bad ? 2 : 0
  }
' "$plan") || fail "cannot rewrite the plan (see above)"
printf '%s\n' "$result"
