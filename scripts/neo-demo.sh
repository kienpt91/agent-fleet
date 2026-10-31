#!/bin/sh
# NEO N1 demo: a goal with a verification contract, a fake agent that claims success every time,
# a contract run by fleet itself that fails the first attempt and passes the second, and the
# evidence it leaves. Runs in a throwaway HOME and repository.
# Usage: scripts/neo-demo.sh [path-to-fleet-binary]   (run it inside a container, see README)
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

git init -q -b main "$REPO"
git -C "$REPO" config user.name demo
git -C "$REPO" config user.email demo@example.invalid
printf '# demo project\n' > "$REPO/README.md"
git -C "$REPO" add -A
git -C "$REPO" commit -q -m "initial commit"

cat > "$WORK/fleet.toml" <<CONFIG
[daemon]
max_sessions = 2
default_worker = "neo"
max_retries = 2
kill_grace_secs = 2

[env]
allow = ["PATH", "HOME", "LANG", "TERM"]

[workers.neo]
command = "$HERE/examples/agents/neo-agent.sh"
args = []
timeout_secs = 60
idle_timeout_secs = 30
CONFIG
cat > "$WORK/plan.toml" <<PLAN
[run]
objective = "NEO demo"
repo = "$REPO"
id = "r1"

[[tasks]]
id = "answer"
title = "Write the answer file"
PLAN

"$FLEET" daemon --config "$WORK/fleet.toml" 2> "$WORK/daemon.log" &
DAEMON=$!
trap '"$FLEET" shutdown >/dev/null 2>&1 || true; kill $DAEMON 2>/dev/null || true; rm -rf "$WORK"' EXIT
i=0; until "$FLEET" status >/dev/null 2>&1; do i=$((i + 1)); [ "$i" -lt 100 ] || exit 1; sleep 0.1; done

run "$FLEET" plan "$WORK/plan.toml"
run "$FLEET" goals add --id feature --title "The answer file says ready" \
  --intent "answer.txt exists, says ready, and nothing else changed" \
  --task r1/answer --contract "$HERE/examples/neo/contract.json"
run "$FLEET" goals remaining
run "$FLEET" start --run r1

wait_for '"completed":1'
run "$FLEET" status
run "$FLEET" goals show feature
run "$FLEET" verify r1/answer
run "$FLEET" goals remaining
run "$FLEET" verify-log
