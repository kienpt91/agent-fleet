#!/bin/sh
# Prints a plan file for the next stage of the roadmap: without the tasks that are already done,
# for a repository and run id you give.
#
#   plan-without.sh [--repo <absolute path>] [--run-id <id>] <plan.toml> <task-id>[,<task-id>...]
#
# fleet cannot mark a task completed by hand, so work that a person did (the human-only tasks)
# or that an earlier run merged is taken out of the plan before the next run is started. The
# listed tasks are removed and so is every mention of them in the `after` lists of the others.
#
# --repo sets `repo` of the [run] table. ALWAYS give it: fleet does not make the `repo` of a plan file
# absolute, so a relative one is resolved by the daemon against ITS working directory, and the run
# would use whatever repository that is. The plan file of this directory carries a placeholder
# that names no repository, so loading it unchanged fails instead of using the wrong one.
# --run-id sets `id` of the [run] table (a new stage needs a new run id).
#
# The plan files of this directory keep one task per `[[tasks]]` table with `id = "..."` and
# `after = [...]` on single lines; a list that spans lines is refused rather than guessed at.
# An id that is not in the plan is an error (a typo must not silently keep a task). Nothing is
# printed unless the whole plan could be rewritten.
set -u
set -f

fail() { printf 'plan-without: %s\n' "$1" >&2; exit 2; }

repo=; runid=
while [ $# -gt 0 ]; do
  case $1 in
    --repo) [ $# -ge 2 ] || fail "--repo needs a value"; repo=$2; shift 2 ;;
    --run-id) [ $# -ge 2 ] || fail "--run-id needs a value"; runid=$2; shift 2 ;;
    --) shift; break ;;
    -*) fail "unknown option $1" ;;
    *) break ;;
  esac
done
[ $# -eq 2 ] || fail "usage: plan-without.sh [--repo DIR] [--run-id ID] <plan.toml> <task-id>[,<task-id>...]"
plan=$1; skip=$2
[ -f "$plan" ] && [ -r "$plan" ] || fail "cannot read $plan"
if [ "$skip" = none ]; then skip=; else
  case $skip in *[!a-z0-9_,-]*|"") fail "the ids may contain only a-z 0-9 _ - and commas (or the single word none)" ;; esac
fi
if [ -n "$repo" ]; then
  case $repo in /*) ;; *) fail "--repo must be an absolute path" ;; esac
  case $repo in *[!A-Za-z0-9_./-]*) fail "--repo may contain only letters, digits and . _ / -" ;; esac
  case "/${repo#/}/" in */../*|*//*|*/./*) fail "--repo must not contain .. or . or empty segments" ;; esac
fi
if [ -n "$runid" ]; then
  case $runid in [a-z0-9]*) ;; *) fail "--run-id must start with a lowercase letter or digit" ;; esac
  case $runid in *[!a-z0-9_-]*) fail "--run-id may contain only a-z 0-9 _ -" ;; esac
fi

result=$(awk -v skip="$skip" -v newrepo="$repo" -v newid="$runid" '
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
    n = 0; intask = 0; inrun = 0; bad = 0
  }
  /^\[\[tasks\]\][ \t]*$/ { flush(); intask = 1; inrun = 0; n = 1; buf[n] = $0; next }
  /^\[run\][ \t]*$/ { inrun = 1; print; next }
  /^\[/ { if (intask) { flush(); intask = 0 } inrun = 0; print; next }
  !intask {
    if (inrun && newrepo != "" && $0 ~ /^repo[ \t]*=/) { print "repo = \"" newrepo "\""; seen_repo = 1; next }
    if (inrun && newid != "" && $0 ~ /^id[ \t]*=/) { print "id = \"" newid "\""; seen_id = 1; next }
    print; next
  }
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
    if (newrepo != "" && !seen_repo) { print "plan-without: the plan has no repo key in its [run] table" > "/dev/stderr"; bad = 1 }
    if (newid != "" && !seen_id) { print "plan-without: the plan has no id key in its [run] table" > "/dev/stderr"; bad = 1 }
    exit bad ? 2 : 0
  }
' "$plan") || fail "cannot rewrite the plan (see above)"
printf '%s\n' "$result"
