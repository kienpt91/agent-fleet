#!/bin/sh
# Scripted end-to-end demo: a four-task DAG run by scripted fake agents inside a throwaway
# HOME and repository. It shows a decision gate (a question), a failing quality gate with a retry,
# approvals for merges and the integration branch.
# Usage: scripts/demo.sh [path-to-fleet-binary]   (run it inside a container, see README)
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
max_sessions = 4
default_worker = "scripted"
max_retries = 2
kill_grace_secs = 2

[env]
allow = ["PATH", "HOME", "LANG", "TERM"]

[workers.scripted]
command = "$HERE/examples/agents/fake-agent.sh"
args = ["{task}"]
timeout_secs = 60
idle_timeout_secs = 30

[[gates]]
name = "changelog"
command = "$HERE/examples/agents/check-changelog.sh"
timeout_secs = 30
CONFIG
sed "s|repo = \".\"|repo = \"$REPO\"|" "$HERE/examples/plan.example.toml" > "$WORK/plan.toml"

"$FLEET" daemon --config "$WORK/fleet.toml" 2> "$WORK/daemon.log" &
DAEMON=$!
trap '"$FLEET" shutdown >/dev/null 2>&1 || true; kill $DAEMON 2>/dev/null || true; rm -rf "$WORK"' EXIT
i=0; until "$FLEET" status >/dev/null 2>&1; do i=$((i + 1)); [ "$i" -lt 100 ] || exit 1; sleep 0.1; done

run "$FLEET" plan "$WORK/plan.toml"
run "$FLEET" start --run r1 --max-parallel 2

wait_for '"kind":"question"'
run "$FLEET" status
run "$FLEET" gate list
run "$FLEET" gate resolve g1 --answer axum

wait_for '"completed":4'
run "$FLEET" status
run "$FLEET" tail release

for task in schema docs api release; do
  say "$FLEET merge $task"
  "$FLEET" merge "$task" || true
done
run "$FLEET" approval list
for id in a1 a2 a3 a4; do
  run "$FLEET" approve "$id"
done
for task in schema docs api release; do
  run "$FLEET" merge "$task"
done

say "git log --oneline --graph fleet/r1/integration"
git -C "$REPO" log --oneline --graph fleet/r1/integration
say "git status --short   # the working tree and branch of the repository are untouched"
git -C "$REPO" status --short
git -C "$REPO" branch --show-current
run "$FLEET" status
run "$FLEET" events --limit 12
