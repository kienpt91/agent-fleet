#!/bin/sh
# Scripted demo of mediated agent-to-agent messaging: two scripted workers exchange a question and
# an answer through a channel the operator declared in the plan, a third message carries a
# secret-shaped string and is refused, and the operator reads everything with `fleet msg`.
# Usage: scripts/msg-demo.sh [path-to-fleet-binary]   (run it inside a container, see README)
set -eu
FLEET=${1:-/target/release/fleet}
WORK=$(mktemp -d)
export HOME="$WORK/home"
export XDG_STATE_HOME="$WORK/state"
mkdir -p "$HOME"
REPO="$WORK/repo"

say() { printf '\n$ %s\n' "$*"; }
run() { say "$*"; "$@"; }
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

# The worker is a shell script. It sends with a temporary name and a rename, and reads its inbox.
cat > "$WORK/agent.sh" <<'AGENT'
#!/bin/sh
send() { printf '%s' "$2" > .fleet/outbox/$1.tmp && mv .fleet/outbox/$1.tmp .fleet/outbox/$1.json; }
wait_for() { i=0; while [ ! -f "$1" ]; do sleep 0.2; i=$((i + 1)); [ "$i" -lt 200 ] || exit 3; done; }
case "$FLEET_TASK" in
  api)
    send 1 '{"version":1,"channel":"design","to":"docs","kind":"question","subject":"Which port does the API use?","body":"Docs: I am going to listen on 8080. Is that what you will document?"}'
    wait_for .fleet/inbox/m2.json
    echo "api received: $(cat .fleet/inbox/m2.json)"
    ;;
  docs)
    wait_for .fleet/inbox/m1.json
    echo "docs received: $(cat .fleet/inbox/m1.json)"
    send 1 '{"version":1,"channel":"design","to":"api","kind":"result","subject":"Yes, 8080","body":"Documenting 8080.","in_reply_to":"m1"}'
    K="ghp_$(printf 'a%.0s' $(seq 1 36))"
    send 2 "{\"version\":1,\"channel\":\"design\",\"to\":\"api\",\"kind\":\"info\",\"subject\":\"token\",\"body\":\"my token is $K\"}"
    ;;
esac
echo '{"version":1,"status":"done","summary":"finished"}' > .fleet/result.json
AGENT
chmod +x "$WORK/agent.sh"

cat > "$WORK/fleet.toml" <<CONFIG
[daemon]
max_sessions = 4
default_worker = "scripted"
kill_grace_secs = 2

[env]
allow = ["PATH", "HOME", "LANG", "TERM"]

[workers.scripted]
command = "$WORK/agent.sh"
timeout_secs = 60
CONFIG

cat > "$WORK/plan.toml" <<PLAN
[run]
objective = "Build the API and document it"
repo = "$REPO"
id = "r1"

[[tasks]]
id = "api"
title = "Build the API"

[[tasks]]
id = "docs"
title = "Document the API"

# declared by the operator; a task in no channel can neither send nor receive
[[channels]]
name = "design"
members = ["task:api", "task:docs"]
mode = "direct"
PLAN

"$FLEET" daemon --config "$WORK/fleet.toml" 2> "$WORK/daemon.log" &
DAEMON=$!
trap '"$FLEET" shutdown >/dev/null 2>&1 || true; kill $DAEMON 2>/dev/null || true; rm -rf "$WORK"' EXIT
i=0; until "$FLEET" status >/dev/null 2>&1; do i=$((i + 1)); [ "$i" -lt 100 ] || exit 1; sleep 0.1; done

run "$FLEET" plan "$WORK/plan.toml"
run "$FLEET" start --run r1 --max-parallel 2
wait_for '"completed":2'
run "$FLEET" msg list
run "$FLEET" msg show m1
run "$FLEET" msg show m3
run "$FLEET" events --limit 12
