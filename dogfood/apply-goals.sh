#!/bin/sh
# Adds the goals of goals.neo-roadmap.tsv to the goal ledger of a running daemon, each with the
# verification contract of its template, rendered for this run. Prints the commands by default;
# --apply runs them. Goals add nothing to the repository: they live in the daemon's event log.
#
#   apply-goals.sh --tsv FILE --run RUN --repo REPO --base BASE --tools DIR --container-run PATH
#                  --out DIR [--fleet PATH] [--skip-tasks id,id] [--apply]
#
# --run is the run id of the plan (neo-roadmap, or the id of a later stage); --base is the commit
# the run starts from (use `git rev-parse` of the branch at the time you load the plan: a hash does
# not move); --tools is the directory install-tools.sh filled; --container-run is your container
# wrapper (see container-run.example.sh); --out receives the rendered contracts (a directory
# outside the repository is best: contracts are read from it by `fleet goals add` only).
# --skip-tasks leaves out the goals of tasks that are not in this stage's plan (see plan-without.sh).
# Goals that already exist (a milestone from an earlier stage) are left as they are with --apply.
set -u
set -f

fail() { printf 'apply-goals: %s\n' "$1" >&2; exit 2; }

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || fail "cannot find the script directory"
tsv=; run=; repo=; base=; tools=; crun=; out=; fleet=fleet; skip=; apply=0
while [ $# -gt 0 ]; do
  case $1 in
    --apply) apply=1; shift ;;
    --tsv|--run|--repo|--base|--tools|--container-run|--out|--fleet|--skip-tasks)
      [ $# -ge 2 ] || fail "$1 needs a value"
      case $1 in
        --tsv) tsv=$2 ;; --run) run=$2 ;; --repo) repo=$2 ;; --base) base=$2 ;; --tools) tools=$2 ;;
        --container-run) crun=$2 ;; --out) out=$2 ;; --fleet) fleet=$2 ;; --skip-tasks) skip=$2 ;;
      esac
      shift 2 ;;
    *) fail "unknown argument $1" ;;
  esac
done
for v in tsv run repo base tools crun out; do
  eval "[ -n \"\${$v}\" ]" || fail "--$(printf '%s' "$v" | sed 's/crun/container-run/') is required"
done
[ -f "$tsv" ] || fail "cannot read $tsv"
mkdir -p "$out" || fail "cannot create $out"

q() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
tab=$(printf '\t')
count=0
while IFS="$tab" read -r goal parent task template params title intent; do
  case $goal in ""|"#"*) continue ;; esac
  [ -n "$intent" ] || fail "malformed row for $goal (7 tab-separated columns are needed)"
  case ",$skip," in *",$task,"*) continue ;; esac
  set -- goals add --id "$goal" --title "$title" --intent "$intent"
  [ "$parent" = "-" ] || set -- "$@" --parent "$parent"
  [ "$task" = "-" ] || set -- "$@" --task "$run/$task"
  if [ "$template" != "-" ]; then
    tpl=$here/contracts/$template.json
    [ -f "$tpl" ] || fail "no contract template $template"
    [ "$task" != "-" ] || fail "$goal has a contract but no task"
    args=
    for name in $(grep -o '@[A-Z_][A-Z0-9_]*@' "$tpl" | sort -u | tr -d '@'); do
      case $name in
        RUN) args="$args RUN=$run" ;;
        TASK) args="$args TASK=$task" ;;
        REPO) args="$args REPO=$repo" ;;
        BASE) args="$args BASE=$base" ;;
        TOOLS) args="$args TOOLS=$tools" ;;
        CONTAINER_RUN) args="$args CONTAINER_RUN=$crun" ;;
        LOCK_SHA256) ;;
        *)
          val=$(printf '%s' "$params" | tr ',' '\n' | sed -n "s/^$name=//p")
          [ -n "$val" ] || fail "$goal: the template $template needs $name=... in its params"
          args="$args $name=$val" ;;
      esac
    done
    "$here/render-contract.sh" "$tpl" "$out/$goal.json" $args >/dev/null || fail "$goal: the contract could not be rendered"
    set -- "$@" --contract "$out/$goal.json"
  fi
  count=$((count + 1))
  if [ "$apply" -eq 0 ]; then
    printf '%s' "$fleet"
    for a in "$@"; do printf ' '; q "$a"; done
    printf '\n'
  else
    if [ "$parent" = "-" ] && [ "$task" = "-" ] && "$fleet" goals show "$goal" >/dev/null 2>&1; then
      printf 'apply-goals: %s exists, left as it is\n' "$goal"
      continue
    fi
    "$fleet" "$@" || fail "fleet failed on goal $goal (the goals before it were added)"
  fi
done < "$tsv"
printf 'apply-goals: %s goal(s) %s\n' "$count" "$([ "$apply" -eq 1 ] && echo added || echo printed)" >&2
