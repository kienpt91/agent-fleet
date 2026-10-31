#!/bin/sh
# NEO N2 demo: a run with `context = "compiled"`, a fake agent that fails its first attempt, the
# compiled context packs of both attempts, `fleet context explain|show|diff|verify` and a handoff.
# Runs in a throwaway HOME and repository.
# Usage: scripts/neo-n2-demo.sh [path-to-fleet-binary]   (run it inside a container, see README)
set -eu
FLEET=${1:-/target/release/fleet}
HERE=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d)
export HOME="$WORK/home"
export XDG_STATE_HOME="$WORK/state"
mkdir -p "$HOME"
REPO="$WORK/repo"

say() { printf '\n$ %s\n' "$*"; }
run() { say "$*"; "$@"; }
# waits (up to 60 s) until `fleet status --json` contains the given text
wait_for() {
  i=0
  until "$FLEET" status --json | grep -q "$1"; do
    i=$((i + 1)); [ "$i" -lt 600 ] || { echo "timed out waiting for $1" >&2; exit 1; }
    sleep 0.1
  done
}
# prints one layer (header, body, end marker) of a rendered pack read on stdin
layer() { awk -v n="$1" '$0 ~ ("^@@neo-layer " n " ") { p = 1 } p { print } $0 ~ ("^@@neo-end " n " ") { p = 0 }'; }

git init -q -b main "$REPO"
git -C "$REPO" config user.name demo
git -C "$REPO" config user.email demo@example.invalid
printf '# demo project\n' > "$REPO/README.md"
git -C "$REPO" add -A
git -C "$REPO" commit -q -m "initial commit"

cat > "$WORK/fleet.toml" <<CONFIG
[daemon]
max_sessions = 2
default_worker = "n2"
max_retries = 2
kill_grace_secs = 2

[env]
allow = ["PATH", "HOME", "LANG", "TERM"]

[context]
constraints = ["Change only answer.txt", "Do not add dependencies"]

[workers.n2]
command = "$HERE/examples/agents/n2-agent.sh"
args = []
timeout_secs = 60
idle_timeout_secs = 30
CONFIG
cat > "$WORK/plan.toml" <<PLAN
[run]
objective = "NEO N2 demo: answer.txt must say ready"
repo = "$REPO"
id = "r1"
context = "compiled"

[[tasks]]
id = "answer"
title = "Write the answer file"
spec = "Create answer.txt with the single word ready."
PLAN

"$FLEET" daemon --config "$WORK/fleet.toml" 2> "$WORK/daemon.log" &
DAEMON=$!
trap '"$FLEET" shutdown >/dev/null 2>&1 || true; kill $DAEMON 2>/dev/null || true; rm -rf "$WORK"' EXIT
i=0; until "$FLEET" status >/dev/null 2>&1; do i=$((i + 1)); [ "$i" -lt 100 ] || exit 1; sleep 0.1; done

run "$FLEET" plan "$WORK/plan.toml"
run "$FLEET" goals add --id feature --title "The answer file says ready" \
  --intent "answer.txt exists, says ready, and nothing else changed" \
  --task r1/answer --contract "$HERE/examples/neo/contract.json"
run "$FLEET" context explain r1/answer
run "$FLEET" start --run r1

wait_for '"completed":1'
run "$FLEET" status

LOG=$(find "$WORK/state" -name events.log | head -n 1)
D1=$(grep -o '"pack_digest":"[0-9a-f]*"' "$LOG" | sed -n 1p | cut -d'"' -f4)
D2=$(grep -o '"pack_digest":"[0-9a-f]*"' "$LOG" | sed -n 2p | cut -d'"' -f4)

say "fleet context show r1/answer --digest $(printf %.12s "$D2")   # the retry's pack: header and layer table"
"$FLEET" context show r1/answer --digest "$D2" | sed -n '1,14p'
say "... layer 4 of the retry's pack (what to do, how it is checked, what fleet's own runs said)"
"$FLEET" context show r1/answer --digest "$D2" | layer 4
say "... layer 7 of the retry's pack (events, and everything agents wrote, labelled UNVERIFIED)"
"$FLEET" context show r1/answer --digest "$D2" | layer 7
run "$FLEET" context diff r1/answer "$(printf %.12s "$D1")" "$(printf %.12s "$D2")"
say "$FLEET context verify r1/answer   # the plan has moved since the last pack was made: exit status 6"
if "$FLEET" context verify r1/answer; then echo "exit status 0"; else echo "exit status $?"; fi
run "$FLEET" handoff r1/answer
run "$FLEET" verify-log
