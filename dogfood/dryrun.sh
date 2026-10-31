#!/bin/sh
# Dry run of the self-development setup with the scripted fake agent (examples/agents/fake-agent.sh),
# in a throwaway clone, a throwaway HOME and a throwaway state directory: it loads a stage of the
# roadmap plan, adds its goals with their rendered contracts, starts the run with one worker at a time
# and shows what happens. No real agent is started and nothing outside the temporary directory is
# changed.
#
#   dryrun.sh --fleet <fleet binary> --source <repository to clone> --container-run <wrapper> [--keep]
#
# --source is cloned (its committed state: commit first) and everything runs on the clone.
# --container-run is the wrapper the cargo checks of the contracts go through (RUNBOOK.md, step 2).
# The stage: the three starter tasks (one of which first touches a protected path, so its contract
# fails and the retry fixes it), a human-only task behind them and an agent task behind that, which
# therefore never starts. The output is the transcript RUNBOOK.md quotes.
set -eu

fail() { printf 'dryrun: %s\n' "$1" >&2; exit 2; }
fleet=; source=; crun=; keep=0
while [ $# -gt 0 ]; do
  case $1 in
    --keep) keep=1; shift ;;
    --fleet|--source|--container-run) [ $# -ge 2 ] || fail "$1 needs a value"
      case $1 in --fleet) fleet=$2 ;; --source) source=$2 ;; --container-run) crun=$2 ;; esac; shift 2 ;;
    *) fail "unknown argument $1" ;;
  esac
done
[ -n "$fleet" ] && [ -n "$source" ] && [ -n "$crun" ] || fail "usage: dryrun.sh --fleet F --source REPO --container-run W [--keep]"
case $fleet in /*) ;; *) fail "--fleet must be an absolute path" ;; esac
case $crun in /*) ;; *) fail "--container-run must be an absolute path" ;; esac
[ -x "$fleet" ] && [ -x "$crun" ] || fail "the fleet binary or the wrapper is not executable"

work=$(mktemp -d "${TMPDIR:-/tmp}/dogfood-dryrun.XXXXXX")
export HOME="$work/home"
export XDG_STATE_HOME="$work/state"
mkdir -p "$HOME"
clone=$work/clone
tools=$work/tools
daemon=

say() { printf '\n$ %s\n' "$*"; }
run() { say "$*"; "$@"; }
cleanup() {
  "$fleet" shutdown >/dev/null 2>&1 || true
  [ -z "$daemon" ] || kill "$daemon" 2>/dev/null || true
  [ "$keep" -eq 1 ] || rm -rf "$work"
}
trap cleanup EXIT

say "git clone <source> clone   # a throwaway clone; the worker never sees any other checkout"
git clone -q --no-hardlinks "$source" "$clone"
git -C "$clone" config user.name dryrun
git -C "$clone" config user.email dryrun@example.invalid
base=$(git -C "$clone" rev-parse HEAD)
echo "base commit $base"

run sh "$clone/dogfood/install-tools.sh" --repo "$clone" --ref HEAD --dest "$tools"

# The dry-run configuration is the template with the real agent replaced by the fake one and the
# optional gate dropped; everything else is the template as it is.
say "dryrun config: dogfood/fleet.dogfood.example.toml with the worker command replaced (diff)"
awk -v fake="$clone/examples/agents/fake-agent.sh" '
  /^\[workers\.agent\]/ { print; print "command = \"" fake "\""; print "args = []"; skip = 1; next }
  skip && /^\[/ { skip = 0 }
  skip && /^timeout_secs|^idle_timeout_secs/ { skip = 0 }
  skip { next }
  /^\[\[gates\]\]/ { gate = 1; next }
  gate && /^$/ { gate = 0; next }
  gate { next }
  { print }
' "$clone/dogfood/fleet.dogfood.example.toml" > "$work/fleet.toml"
diff "$clone/dogfood/fleet.dogfood.example.toml" "$work/fleet.toml" | grep -E '^[<>] (command|args|name|timeout_secs = 1800|\[\[gates|  )' | head -n 30 || true

"$fleet" daemon --config "$work/fleet.toml" 2> "$work/daemon.log" &
daemon=$!
i=0; until "$fleet" status >/dev/null 2>&1; do i=$((i + 1)); [ "$i" -lt 200 ] || { cat "$work/daemon.log" >&2; fail "the daemon did not start"; }; sleep 0.1; done

# ---- the stage: starters, one human-only barrier and the task behind it ----------------------------
keep_ids="starter-error-tests starter-value-property starter-docs-glossary n3-scaffold n3-source"
all_ids=$(sed -n '/^\[\[tasks\]\]/,$ s/^id = "\(.*\)"$/\1/p' "$clone/dogfood/plan.neo-roadmap.toml")
drop=
for id in $all_ids; do
  case " $keep_ids " in *" $id "*) ;; *) drop="$drop,$id" ;; esac
done
drop=${drop#,}
say "sh dogfood/plan-without.sh dogfood/plan.neo-roadmap.toml <all but $(echo "$keep_ids" | wc -w | tr -d ' ') tasks> > stage.toml"
sh "$clone/dogfood/plan-without.sh" "$clone/dogfood/plan.neo-roadmap.toml" "$drop" > "$work/stage.toml"
grep -c '^\[\[tasks\]\]' "$work/stage.toml" | sed 's/^/tasks in the stage: /'

cd "$clone"
run "$fleet" plan "$work/stage.toml"
say "sh dogfood/apply-goals.sh --skip-tasks <the others> --apply ...   # renders one contract per goal and adds the goals"
sh dogfood/apply-goals.sh --tsv dogfood/goals.neo-roadmap.tsv --run neo-roadmap --repo "$clone" --base "$base" \
  --tools "$tools" --container-run "$crun" --out "$work/contracts" --fleet "$fleet" --skip-tasks "$drop" --apply
run "$fleet" goals list
run "$fleet" goals show g-starter-docs-glossary
run "$fleet" goals remaining

run "$fleet" start --run neo-roadmap --max-parallel 1

# wait until nothing is waiting, running or in review (a contract run can take many minutes)
i=0
until "$fleet" status | grep -q 'ready 0, running 0, review 0'; do
  i=$((i + 1)); [ "$i" -lt 7200 ] || { echo "timed out" >&2; exit 1; }
  sleep 1
done
sleep 1
run "$fleet" status
run "$fleet" goals list
run "$fleet" goals show g-starter-value-property
run "$fleet" goals show g-starter-docs-glossary
run "$fleet" goals remaining
say "the worker that costs nothing: fleet tail n3-scaffold"
"$fleet" tail n3-scaffold || true

say "fleet verify r/task: run the contract again, by hand, on the task's current commit"
"$fleet" verify neo-roadmap/starter-docs-glossary || echo "exit $?"
say "fleet verify g-n3-scaffold: the human-only contract can not pass"
"$fleet" verify g-n3-scaffold --task neo-roadmap/n3-scaffold || echo "exit $?"

# ---- the person reviews and merges ---------------------------------------------------------------------
say "git log --oneline $base..fleet/neo-roadmap/starter-value-property   # what the retry committed"
git -C "$clone" log --oneline "$base..fleet/neo-roadmap/starter-value-property"
say "git diff --stat $base fleet/neo-roadmap/starter-value-property"
git -C "$clone" diff --stat "$base" fleet/neo-roadmap/starter-value-property
run sh "$tools/check-protected.sh" --repo "$clone" "$base..fleet/neo-roadmap/starter-value-property"
run sh "$tools/check-docs-claims.sh" --repo "$clone" "$base..fleet/neo-roadmap/starter-value-property"
say "fleet merge starter-docs-glossary   # needs an approval bound to the exact commit"
"$fleet" merge starter-docs-glossary || echo "exit $?"
run "$fleet" approval list
aid=$("$fleet" approval list --json | sed -n 's/.*"id":"\(a[0-9]*\)".*/\1/p' | head -n 1)
run "$fleet" approve "$aid"
run "$fleet" merge starter-docs-glossary
run "$fleet" verify-log
say "git status --short   # the clone's own branch and working tree were never touched"
git -C "$clone" status --short
git -C "$clone" branch --show-current
