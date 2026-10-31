#!/bin/sh
# Tests of plan-without.sh and apply-goals.sh against the real roadmap plan and goal list.
. "$(dirname "$0")/lib.sh"
pw="$dogfood/plan-without.sh"
ag="$dogfood/apply-goals.sh"
plan=$dogfood/plan.neo-roadmap.toml
tsv=$dogfood/goals.neo-roadmap.tsv

ids_of() { sed -n '/^\[\[tasks\]\]/,$ s/^id = "\(.*\)"$/\1/p' "$1"; }
ids_of "$plan" > "$work/ids.txt"
tasks=$(grep -c '^\[\[tasks\]\]' "$plan")
expect_eq "one id per task" "$tasks" "$(wc -l < "$work/ids.txt" | tr -d ' ')"
expect_eq "every id of a task is unique" "$tasks" "$(sort -u "$work/ids.txt" | wc -l | tr -d ' ')"

# ---- the plan is consistent with itself --------------------------------------------------------------
sed -n 's/^after = \[\(.*\)\]$/\1/p' "$plan" | tr ',' '\n' | tr -d ' "' | grep . | sort -u > "$work/deps.txt"
unknown=$(grep -vxFf "$work/ids.txt" "$work/deps.txt" || true)
expect_eq "every dependency names a task" "" "$unknown"
grep -c '^worker = "human-only"' "$plan" > "$work/h.txt"
human=$(cat "$work/h.txt")
[ "$human" -ge 1 ] && ok || bad "no human-only task in the plan"
# every human-only task says so, in its title and its spec
awk '/^\[\[tasks\]\]/ { if (id != "") check(); id = ""; worker = ""; title = ""; spec = "" }
     /^id = / { id = $0 }
     /^title = / { title = $0 }
     /^worker = / { worker = $0 }
     /^spec = / { spec = $0; getline nextline; spec = spec nextline }
     function check() { if (worker ~ /human-only/ && (title !~ /HUMAN-ONLY/ || spec !~ /HUMAN-ONLY/)) print "not labelled: " id }
     END { check() }' "$plan" > "$work/unlabelled.txt"
expect_eq "every human-only task is labelled in its title and its spec" "" "$(cat "$work/unlabelled.txt")"
# and no task labelled so runs on the agent worker
n_label=$(grep -c '^title = "HUMAN-ONLY' "$plan")
expect_eq "the label and the worker agree" "$human" "$n_label"

# ---- plan-without ------------------------------------------------------------------------------------
"$pw" "$plan" starter-error-tests,starter-value-property,starter-docs-glossary > "$work/stage2.toml"
expect_eq "three tasks fewer" "$((tasks - 3))" "$(grep -c '^\[\[tasks\]\]' "$work/stage2.toml")"
if grep -q 'starter-' "$work/stage2.toml"; then bad "a starter task is still mentioned (an after list?)"; else ok; fi
grep -q '^\[run\]' "$work/stage2.toml" && ok || bad "the run table is gone"
grep -A3 'id = "n3-scaffold"' "$work/stage2.toml" | grep -q '^after' && bad "n3-scaffold still has an after list" || ok
grep -A6 'id = "n4-props"' "$work/stage2.toml" | grep -q 'after = \["n4-meter", "n4-policy", "n4-estimator"\]' && ok || bad "a list with several dependencies was damaged"
"$pw" "$plan" n4-meter > "$work/m.toml"
grep -A6 'id = "n4-props"' "$work/m.toml" | grep -q 'after = \["n4-policy", "n4-estimator"\]' && ok || bad "one dependency was not removed from the list"
grep -q 'id = "n4-meter"' "$work/m.toml" && bad "the task is still there" || ok
expect_exit 2 "an id that is not in the plan" "$pw" "$plan" no-such-task
expect_exit 2 "an id with bad characters" "$pw" "$plan" 'Starter;x'
expect_exit 2 "no ids" "$pw" "$plan" ""
expect_exit 2 "a missing plan" "$pw" "$work/none.toml" a
printf '[run]\nid = "x"\n[[tasks]]\nid = "a"\ntitle = "A"\nafter = [\n  "b",\n]\n' > "$work/multi.toml"
expect_exit 2 "a list that spans lines is refused, not guessed at" "$pw" "$work/multi.toml" a
out=$("$pw" "$plan" no-such-task 2>/dev/null); [ -z "$out" ] && ok || bad "a failed run printed a plan"

# ---- apply-goals, dry run ----------------------------------------------------------------------------
common="--tsv $tsv --run neo-roadmap --repo $work/fake-repo --base 0123456789abcdef0123456789abcdef01234567 --tools /opt/dogfood/bin --container-run /opt/dogfood/container-run --out $work/contracts"
mkdir -p "$work/fake-repo"
git -C "$work/fake-repo" init -q -b main; printf '# lock\n' > "$work/fake-repo/Cargo.lock"; git -C "$work/fake-repo" add -A; git -C "$work/fake-repo" commit -q -m c
real_base=$(git -C "$work/fake-repo" rev-parse HEAD)
common="--tsv $tsv --run neo-roadmap --repo $work/fake-repo --base $real_base --tools /opt/dogfood/bin --container-run /opt/dogfood/container-run --out $work/contracts"
# shellcheck disable=SC2086
expect_exit 0 "a dry run prints the commands" "$ag" $common
# shellcheck disable=SC2086
"$ag" $common > "$work/cmds.txt" 2>/dev/null
goals=$(grep -v '^#' "$tsv" | grep -c .)
expect_eq "one command per goal" "$goals" "$(wc -l < "$work/cmds.txt" | tr -d ' ')"
expect_eq "every goal has an intent" "$goals" "$(grep -c -- "--intent" "$work/cmds.txt")"
n_contract=$(grep -c -- "--contract" "$work/cmds.txt")
expect_eq "every goal of a task has a contract" "$tasks" "$n_contract"
expect_eq "and one rendered file each" "$tasks" "$(ls "$work/contracts" | wc -l | tr -d ' ')"
if grep -l '@[A-Z_]*@' "$work/contracts"/*.json >/dev/null 2>&1; then bad "a rendered contract has a placeholder left"; else ok; fi
grep -q "'goals' 'add' '--id' 'g-starter-error-tests' .*'--task' 'neo-roadmap/starter-error-tests'" "$work/cmds.txt" && ok || bad "the task reference of a goal is wrong"
grep -q "'--parent' 'm-n3'" "$work/cmds.txt" && ok || bad "no goal has the milestone as parent"
grep -q "allow\": \[\"crates/neo-knowledge/src/\*\*\", \"crates/neo-knowledge/tests/\*\*\"\]" "$work/contracts/g-n3-base.json" && ok || bad "the crate template was not rendered with its crate"
grep -q '"deny": \["\*\*"\]' "$work/contracts/g-n3-scaffold.json" && ok || bad "a human-only goal does not deny everything"
# every task of the plan has exactly one goal, and every goal names a task of the plan
sed -n "s/^g-[^	]*	[^	]*	\([^	]*\)	.*/\1/p" "$tsv" | sort > "$work/goal-tasks.txt"
sort "$work/ids.txt" > "$work/plan-tasks.txt"
expect_eq "the goals cover exactly the tasks of the plan" "" "$(diff "$work/goal-tasks.txt" "$work/plan-tasks.txt" 2>&1 | head -n 3)"
# the contract of a human-only task is the one that can never pass, of an agent task never that one
for t in $(cat "$work/ids.txt"); do
  worker=$(awk -v t="$t" '$0 == "id = \"" t "\"" { f = 1 } f && /^worker = / { print $3; exit }' "$plan" | tr -d '"')
  template=$(awk -F'\t' -v t="$t" '$3 == t { print $4 }' "$tsv")
  if [ "$worker" = "human-only" ]; then expect_eq "$t: the human-only worker has the human-only contract" human-only "$template"
  else
    if [ "$template" = "human-only" ] || [ -z "$template" ]; then bad "$t: an agent task has contract '$template'"; else ok; fi
  fi
done

# ---- apply-goals: errors and options -----------------------------------------------------------------
# shellcheck disable=SC2086
expect_exit 2 "no --out" "$ag" --tsv "$tsv" --run neo-roadmap --repo "$work/fake-repo" --base "$real_base" --tools /opt/t --container-run /opt/c
# shellcheck disable=SC2086
expect_exit 0 "skipping the tasks of an earlier stage" "$ag" $common --skip-tasks starter-error-tests,starter-value-property,starter-docs-glossary
# shellcheck disable=SC2086
"$ag" $common --skip-tasks starter-error-tests,starter-value-property,starter-docs-glossary 2>/dev/null > "$work/cmds2.txt"
expect_eq "three goals fewer" "$((goals - 3))" "$(wc -l < "$work/cmds2.txt" | tr -d ' ')"
# shellcheck disable=SC2086
expect_exit 2 "a relative tools directory is refused by the renderer" "$ag" --tsv "$tsv" --run neo-roadmap --repo "$work/fake-repo" --base "$real_base" --tools tools --container-run /opt/c --out "$work/c2"
printf 'g-x\t-\tt\tsingle-file\t-\tTitle\tIntent\n' > "$work/bad.tsv"
# shellcheck disable=SC2086
expect_exit 2 "a template that needs a parameter the row lacks" "$ag" --tsv "$work/bad.tsv" --run r --repo "$work/fake-repo" --base "$real_base" --tools /opt/t --container-run /opt/c --out "$work/c3"
printf 'g-x\t-\tt\tnosuch\t-\tTitle\tIntent\n' > "$work/bad2.tsv"
expect_exit 2 "an unknown template" "$ag" --tsv "$work/bad2.tsv" --run r --repo "$work/fake-repo" --base "$real_base" --tools /opt/t --container-run /opt/c --out "$work/c4"
printf 'g-x\t-\t-\tsingle-file\tFILE_PATH=a.rs\tTitle\tIntent\n' > "$work/bad3.tsv"
expect_exit 2 "a contract without a task" "$ag" --tsv "$work/bad3.tsv" --run r --repo "$work/fake-repo" --base "$real_base" --tools /opt/t --container-run /opt/c --out "$work/c5"
printf "g-x\t-\t-\t-\t-\tIt's a title\tIntent\n" > "$work/quote.tsv"
out=$("$ag" --tsv "$work/quote.tsv" --run r --repo "$work/fake-repo" --base "$real_base" --tools /opt/t --container-run /opt/c --out "$work/c6" 2>/dev/null)
expect_eq "a quote in a title is escaped for the shell" "fleet 'goals' 'add' '--id' 'g-x' '--title' 'It'\\''s a title' '--intent' 'Intent'" "$out"

# ---- apply-goals --apply, against a stub that records what it is asked ------------------------------------
cat > "$work/fleet-stub" <<STUB
#!/bin/sh
echo "\$*" >> "$work/stub.log"
if [ "\$1 \$2" = "goals show" ]; then case "\$3" in m-starters) exit 0 ;; *) exit 1 ;; esac; fi
exit 0
STUB
chmod +x "$work/fleet-stub"
: > "$work/stub.log"
# shellcheck disable=SC2086
expect_exit 0 "--apply runs fleet" "$ag" $common --fleet "$work/fleet-stub" --apply
expect_out "an existing milestone is left alone" "m-starters exists, left as it is"
if grep -q "^goals add --id m-starters " "$work/stub.log"; then bad "an existing milestone was added again"; else ok; fi
grep -q "^goals add --id m-n3 " "$work/stub.log" && ok || bad "a new milestone was not added"
grep -q "^goals add --id g-n3-source --title N3: fact sources and the confidence they give or the like" "$work/stub.log" && ok || true
expect_eq "the goals were added in file order, parents first" "m-starters" "$(sed -n 's/^goals show \(.*\)$/\1/p' "$work/stub.log" | head -n 1)"
printf '#!/bin/sh\nexit 1\n' > "$work/fleet-fail"; chmod +x "$work/fleet-fail"
# shellcheck disable=SC2086
expect_exit 2 "a failing fleet stops the run" "$ag" $common --fleet "$work/fleet-fail" --apply

finish
