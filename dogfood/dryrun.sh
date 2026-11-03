#!/bin/sh
# Dry run of the self-development setup with the scripted fake agent (examples/agents/fake-agent.sh),
# in a throwaway clone, a throwaway HOME and a throwaway state directory: it loads a stage of the
# roadmap plan, adds its goals with their rendered contracts, starts the run with one worker at a time
# and shows what happens. No real agent is started and nothing outside the temporary directory is
# changed.
#
#   dryrun.sh --fleet <fleet binary> --source <repository to clone> [--container-run <wrapper>] [--keep]
#
# --source is cloned (its committed state: commit first) and everything runs on the clone.
# --container-run is the wrapper the cargo checks of the contracts go through (RUNBOOK.md, section 2);
# without it the dry run uses container-run.inside.sh, a copy of which is made OUTSIDE the clone: run
# the dry run inside the development container, where no docker client exists.
# The stage: the three starter tasks (one of which first touches a protected path, so its contract
# fails and the retry fixes it), a human-only task behind them and an agent task behind that, which
# therefore never starts. Then the reviewer's steps: the checkers by hand, the merge approval and taking a
# task's commits into another branch under another identity with the conflict-marker check. The output
# is the transcript RUNBOOK.md quotes.
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
[ -n "$fleet" ] && [ -n "$source" ] || fail "usage: dryrun.sh --fleet F --source REPO [--container-run W] [--keep]"
case $fleet in /*) ;; *) fail "--fleet must be an absolute path" ;; esac
[ -z "$crun" ] || case $crun in /*) ;; *) fail "--container-run must be an absolute path" ;; esac
[ -x "$fleet" ] || fail "the fleet binary is not executable"
[ -z "$crun" ] || [ -x "$crun" ] || fail "the wrapper is not executable"

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
if [ -z "$crun" ]; then
  crun=$work/container-run
  cp "$clone/dogfood/container-run.inside.sh" "$crun" && chmod 755 "$crun"
  say "container-run = a copy of dogfood/container-run.inside.sh outside the clone ($crun)"
fi

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
keep_ids="starter-error-tests starter-dag-property starter-docs-glossary maint-docs-links n3-scaffold n3-source"
all_ids=$(sed -n '/^\[\[tasks\]\]/,$ s/^id = "\(.*\)"$/\1/p' "$clone/dogfood/plan.neo-roadmap.toml")
drop=
for id in $all_ids; do
  case " $keep_ids " in *" $id "*) ;; *) drop="$drop,$id" ;; esac
done
drop=${drop#,}
say "sh dogfood/plan-without.sh --repo <clone> dogfood/plan.neo-roadmap.toml <all but $(echo "$keep_ids" | wc -w | tr -d ' ') tasks> > stage.toml"
sh "$clone/dogfood/plan-without.sh" --repo "$clone" "$clone/dogfood/plan.neo-roadmap.toml" "$drop" > "$work/stage.toml"
grep -c '^\[\[tasks\]\]' "$work/stage.toml" | sed 's/^/tasks in the stage: /'

cd "$clone"
run "$fleet" plan "$work/stage.toml"
say "sh dogfood/apply-goals.sh --plan stage.toml --skip-tasks <the others> --apply ...   # renders one contract per goal (a dependent's scope also allows its dependencies') and adds the goals"
sh dogfood/apply-goals.sh --tsv dogfood/goals.neo-roadmap.tsv --plan "$work/stage.toml" --run neo-roadmap --repo "$clone" --base "$base" \
  --tools "$tools" --container-run "$crun" --out "$work/contracts" --fleet "$fleet" --skip-tasks "$drop" --apply
say "the scope check of maint-docs-links: it runs after the three starters, whose work fleet merges into its branch"
grep -A4 '"id": "scope"' "$work/contracts/g-maint-docs-links.json"
run "$fleet" goals list
run "$fleet" goals show g-starter-docs-glossary
run "$fleet" goals remaining

run "$fleet" start --run neo-roadmap --max-parallel 1

# Wait until nothing is running, in review or ready (a contract run can take many minutes). If a task that
# should finish ended `failed`, retry it by hand with `fleet retry`, as the runbook says: the `layers` check
# runs the whole fleet-infra suite, and at this base a few of its tests (the flaky ones of dogfood/trials/)
# fail now and then under load, which no task is to blame for. At most three rounds; 90 minutes in all.
i=0; rounds=0
while :; do
  until "$fleet" status | grep -q 'ready 0, running 0, review 0'; do
    i=$((i + 1)); [ "$i" -lt 5400 ] || { echo "timed out; last status:" >&2; "$fleet" status >&2; exit 1; }
    sleep 1
  done
  sleep 1
  "$fleet" status | grep -q ', completed 4,' && break
  rounds=$((rounds + 1)); [ "$rounds" -le 3 ] || { echo "a task did not complete after three retries:" >&2; "$fleet" status >&2; exit 1; }
  for t in starter-error-tests starter-dag-property starter-docs-glossary maint-docs-links; do
    say "fleet retry $t   # only a failed task can be retried; the others answer with a conflict"
    "$fleet" retry "$t" || true
  done
done
sleep 1
run "$fleet" status
run "$fleet" goals list
run "$fleet" goals show g-starter-dag-property
run "$fleet" goals show g-starter-docs-glossary
run "$fleet" goals show g-maint-docs-links
run "$fleet" goals remaining

# why did the first attempts fail? the output of every failed check, from the evidence in the ledger (the
# entries of `goals show --json` start with {"at_ms":; long outputs are cut to their last 12 lines)
failed_output() {
  "$fleet" goals show "$1" --json | awk '
    BEGIN { RS = "\\{\"at_ms\":" }
    /"verdict":"failed"/ {
      check = ""; if (match($0, /"check":"[^"]*"/)) check = substr($0, RSTART + 9, RLENGTH - 10)
      commit = ""; if (match($0, /"commit":"[0-9a-f]*"/)) commit = substr($0, RSTART + 10, 12)
      summary = ""; if (match($0, /"summary":"([^"\\]|\\.)*"/)) summary = substr($0, RSTART + 11, RLENGTH - 12)
      out = ""; if (match($0, /"output":"([^"\\]|\\.)*"/)) out = substr($0, RSTART + 10, RLENGTH - 11)
      gsub(/\\n/, "\n", out); gsub(/\\"/, "\"", out)
      n = split(out, lines, "\n"); first = n > 12 ? n - 11 : 1
      printf "  [%s @ %s] FAILED: %s\n", check, commit, summary
      for (i = first; i <= n; i++) if (lines[i] != "") printf "      %s\n", lines[i]
    }'
}
for g in g-starter-error-tests g-starter-dag-property; do
  say "the checks that failed for $g (fleet goals show $g --json)"
  failed_output "$g"
done
# `goals show` lists the newest 20 pieces of evidence only; the audit log has all of them
say "fleet events --limit 200 | grep ... # every failed protected-path check of the run (the daemon keeps the last 200 events in memory: enough here)"
"$fleet" events --limit 200 | grep -E 'check "protected-(diff|script)" failed' || echo "(none found)"
say "the worker that costs nothing: fleet tail n3-scaffold"
"$fleet" tail n3-scaffold || true

say "fleet verify r/task: run the contract again, by hand, on the task's current commit"
"$fleet" verify neo-roadmap/starter-docs-glossary || echo "exit $?"
say "fleet verify g-n3-scaffold   # the human-only contract; its task's worker failed, so there is no reviewed commit and no evidence can count"
"$fleet" verify g-n3-scaffold --task neo-roadmap/n3-scaffold || echo "exit $?"

# ---- the person reviews and merges ---------------------------------------------------------------------
say "git log --oneline $base..fleet/neo-roadmap/starter-dag-property   # what the retry committed"
git -C "$clone" log --oneline "$base..fleet/neo-roadmap/starter-dag-property"
say "git diff --stat $base fleet/neo-roadmap/starter-dag-property"
git -C "$clone" diff --stat "$base" fleet/neo-roadmap/starter-dag-property
run sh "$tools/check-protected.sh" --repo "$clone" "$base..fleet/neo-roadmap/starter-dag-property"
run sh "$tools/check-docs-claims.sh" --repo "$clone" "$base..fleet/neo-roadmap/starter-dag-property"
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

# ---- taking the work into the branch you keep (fleet's commits carry the identity fleet <fleet@localhost>) ----
say "git log --format='%h %an <%ae> %s' $base..fleet/neo-roadmap/starter-docs-glossary   # who fleet says made the commits"
git -C "$clone" log --format='%h %an <%ae> %s' "$base..fleet/neo-roadmap/starter-docs-glossary"
orig_branch=$(git -C "$clone" branch --show-current)
run git -C "$clone" switch -q -c owner-branch "$base"
say "git cherry-pick -n $base..fleet/neo-roadmap/starter-docs-glossary   # applied to the index and the files, not committed"
git -C "$clone" cherry-pick -n "$base..fleet/neo-roadmap/starter-docs-glossary"
run sh "$tools/check-conflict-markers.sh" --git "$clone"
say "git commit as the owner (the commit message is the owner's own)"
git -C "$clone" -c user.name="Owner Name" -c user.email=owner@example.invalid commit -q -m "Add the self-development terms to the glossary"
git -C "$clone" log --format='%h %an <%ae> %s' -1
git -C "$clone" switch -q "$orig_branch"
