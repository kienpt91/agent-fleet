#!/bin/sh
# Fan-out demo: one prompt to four fake agents, one passing the contract, one failing it, one editing
# outside the allowed paths, one passing with a bigger change; the comparison; the pick through the
# approval; the superseded members; the cleanup. Runs in a throwaway HOME and repository.
# Usage: scripts/fanout-demo.sh [path-to-fleet-binary]   (run it inside a container, see README)
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
wait_for() {
  i=0
  until "$FLEET" groups show fan1 | grep -q "$1"; do
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
default_worker = "fake"
max_retries = 0
kill_grace_secs = 2

[compare]
rank = ["completed", "verified", "in-scope", "diff", "time"]

[env]
allow = ["PATH", "HOME", "LANG", "TERM"]

[workers.fake]
command = "$HERE/examples/agents/fanout-agent.sh"
args = []
timeout_secs = 60
idle_timeout_secs = 30
CONFIG

"$FLEET" daemon --config "$WORK/fleet.toml" 2> "$WORK/daemon.log" &
DAEMON=$!
trap '"$FLEET" shutdown >/dev/null 2>&1 || true; kill $DAEMON 2>/dev/null || true; rm -rf "$WORK"' EXIT
i=0; until "$FLEET" status >/dev/null 2>&1; do i=$((i + 1)); [ "$i" -lt 100 ] || exit 1; sleep 0.1; done

run "$FLEET" run create --id r1 --objective "parser work" --repo "$REPO"
run "$FLEET" fanout --run r1 --title "Reject trailing commas" \
  --spec "Make the parser reject trailing commas and keep the error text." \
  --workers fake --count 4 --contract "$HERE/examples/fanout-contract.json"
run "$FLEET" start --run r1
wait_for finished
run "$FLEET" groups list
run "$FLEET" compare fan1

say "$FLEET pick fan1 fan1-3   (it failed its contract: refused)"
"$FLEET" pick fan1 fan1-3 || echo "(exit $?)"
say "$FLEET merge fan1-1   (a plain merge of a member is refused)"
"$FLEET" merge fan1-1 || echo "(exit $?)"
say "$FLEET pick fan1 fan1-1   (asks for the approval)"
"$FLEET" pick fan1 fan1-1 || echo "(exit $?)"
run "$FLEET" approval list
ID=$("$FLEET" approval list --json | sed -n 's/.*"id":"\(a[0-9]*\)".*/\1/p' | head -n 1)
run "$FLEET" approve "$ID"
run "$FLEET" pick fan1 fan1-1
run "$FLEET" groups show fan1
run git -C "$REPO" ls-tree -r --name-only fleet/r1/integration
run git -C "$REPO" branch --list 'fleet/*'
run "$FLEET" cleanup --branches
run "$FLEET" groups show fan1
run git -C "$REPO" branch --list 'fleet/*'
run "$FLEET" verify-log
