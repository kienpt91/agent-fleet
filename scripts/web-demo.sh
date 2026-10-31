#!/bin/sh
# Scripted demo of the web dashboard: a daemon started with --web in a throwaway HOME and repository,
# the same four-task DAG and scripted agents as scripts/demo.sh, driven over HTTP with curl (login by
# token, session cookie, CSRF header, a decision gate, a merge that needs an approval bound to its
# digest), and, when a headless Chromium is installed, the real page loaded in that browser.
#
# Usage: scripts/web-demo.sh [path-to-fleet-binary]   (run it inside a container with curl; the
# image of docker/Dockerfile.web has curl and Chromium). Set WEB_DEMO_SHOTS=<dir> to keep screenshots.
# Every 64-character hex string in the output (the token, session ids, digests) is shown as <64 hex>.
set -eu
FLEET=${1:-/target/release/fleet}
HERE=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d)
export HOME="$WORK/home"
export XDG_STATE_HOME="$WORK/state"
mkdir -p "$HOME"
REPO="$WORK/repo"

mask() { sed -E 's/[0-9a-f]{64}/<64 hex>/g'; }
say() { printf '\n$ %s\n' "$*" | mask; }
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

# The daemon's stderr is the operator's terminal: the login URL (with the token) is printed there once.
say "fleet daemon --config fleet.toml --web 127.0.0.1:0"
"$FLEET" daemon --config "$WORK/fleet.toml" --web 127.0.0.1:0 2> "$WORK/terminal.txt" &
DAEMON=$!
cleanup() { "$FLEET" shutdown >/dev/null 2>&1 || true; kill $DAEMON 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT
i=0; until "$FLEET" status >/dev/null 2>&1; do i=$((i + 1)); [ "$i" -lt 100 ] || exit 1; sleep 0.1; done
i=0; until grep -q 'login URL' "$WORK/terminal.txt"; do i=$((i + 1)); [ "$i" -lt 100 ] || exit 1; sleep 0.1; done
mask < "$WORK/terminal.txt" | sed -E 's|#token=<64 hex>|#token=<64 hex>|'
LOGIN=$(sed -n 's/.*login URL.*: //p' "$WORK/terminal.txt")
BASE=${LOGIN%%/#*}           # http://127.0.0.1:<port>
HOSTPORT=${BASE#http://}
TOKEN=${LOGIN##*#token=}

"$FLEET" plan "$WORK/plan.toml" >/dev/null
"$FLEET" start --run r1 --max-parallel 2 >/dev/null
wait_for '"kind":"question"'

web() { say "curl $*"; curl -sS -i --max-time 20 "$@" | mask; echo; }

say "# no credentials"
curl -sS -i --max-time 20 "$BASE/api/status" | sed -n '1p;$p' | mask; echo
say "# a Host header that is not the daemon's address (DNS rebinding)"
curl -sS -i --max-time 20 -H 'Host: evil.example' "$BASE/" | sed -n '1p;$p' | mask; echo
say "# the page: static, hardened headers, no inline code"
curl -sS -i --max-time 20 "$BASE/" | sed -n '1,12p' | mask
say "# login: the page exchanges the fragment token for an HttpOnly SameSite=Strict cookie"
curl -sS -i --max-time 20 -c "$WORK/jar" -H "Origin: $BASE" -H 'Content-Type: application/json' \
  -d "{\"token\":\"$TOKEN\"}" "$BASE/api/login" | sed -n '1p;/^Set-Cookie/p;$p' | mask; echo
CSRF=$(curl -sS --max-time 20 -b "$WORK/jar" "$BASE/api/session" | sed -n 's/.*"csrf":"\([0-9a-f]*\)".*/\1/p')

say "# a state-changing request without the CSRF token, then with a foreign Origin"
curl -sS -i --max-time 20 -b "$WORK/jar" -H "Origin: $BASE" -H 'Content-Type: application/json' \
  -d '{"id":"g1","answer":"axum"}' "$BASE/api/gate/resolve" | sed -n '1p;$p' | mask; echo
curl -sS -i --max-time 20 -b "$WORK/jar" -H 'Origin: http://evil.example' -H "X-CSRF-Token: $CSRF" \
  -H 'Content-Type: application/json' -d '{"id":"g1","answer":"axum"}' "$BASE/api/gate/resolve" | sed -n '1p;$p' | mask; echo

if command -v chromium >/dev/null 2>&1 || command -v chromium-browser >/dev/null 2>&1; then
  BROWSER=$(command -v chromium || command -v chromium-browser)
  PROFILE="$WORK/profile"
  shoot() { # name route [extra checks follow as patterns]
    name=$1; route=$2; shift 2
    url="$BASE/$route"
    extra=""
    [ -z "${WEB_DEMO_SHOTS:-}" ] || extra="--screenshot=$WEB_DEMO_SHOTS/$name.png --window-size=1200,800"
    # shellcheck disable=SC2086
    "$BROWSER" --headless=new --no-sandbox --disable-gpu --disable-dev-shm-usage --user-data-dir="$PROFILE" \
      --virtual-time-budget=12000 --dump-dom $extra "$url" > "$WORK/dom-$name.html" 2> "$WORK/chromium-$name.err" || true
    for pattern in "$@"; do
      if grep -qF -- "$pattern" "$WORK/dom-$name.html"; then echo "  [$name] found: $pattern"; else echo "  [$name] MISSING: $pattern"; fi
    done
  }
  say "# the real page in headless Chromium (profile reused, so the cookie from the first load is used afterwards)"
  shoot login "#token=$TOKEN" 'Add a users table, an API and docs' 'Runs'
  shoot dag "#/run/r1" 'Build the users API' 'st-blocked'
  shoot decisions "#/decisions" 'Which HTTP framework' 'Decisions'
  grep -c 'fleet_session' "$WORK/dom-login.html" | sed 's/^/  cookie value visible to the page script (must be 0): /'
fi

post() { # path json
  curl -sS -i --max-time 20 -b "$WORK/jar" -H "Origin: $BASE" -H "X-CSRF-Token: $CSRF" \
    -H 'Content-Type: application/json' -d "$2" "$BASE$1"
}
say "# answer the worker's question through the dashboard API"
post /api/gate/resolve '{"id":"g1","answer":"axum"}' | sed -n '1p;$p' | mask; echo
wait_for '"completed":4'

say "# merge from the web: refused until a person approves the exact action"
post /api/merge '{"task":"schema"}' | sed -n '1p;$p' | mask; echo
APPROVAL=$(curl -sS --max-time 20 -b "$WORK/jar" "$BASE/api/status" | sed 's/},{/}\n{/g' | grep '"purpose":"merge"' | head -n 1)
AID=$(printf '%s' "$APPROVAL" | sed -n 's/.*"id":"\(a[0-9]*\)".*/\1/p')
DIGEST=$(printf '%s' "$APPROVAL" | sed -n 's/.*"digest":"\([0-9a-f]*\)".*/\1/p')
say "# the approval as the page shows it (exact action and digest)"
printf '%s\n' "$APPROVAL" | mask
if [ -n "${BROWSER:-}" ]; then
  say "# the approval queue in the browser: the exact action, every argument, the digest and the buttons"
  shoot approvals "#/decisions" 'fleet/r1/schema' 'Approve' 'Deny'
fi
say "# approving with a wrong digest is refused, the right digest is accepted"
post /api/approval/decide "{\"id\":\"$AID\",\"approve\":true,\"digest\":\"$(printf '0%.0s' $(seq 1 64))\"}" | sed -n '1p;$p' | mask; echo
post /api/approval/decide "{\"id\":\"$AID\",\"approve\":true,\"digest\":\"$DIGEST\"}" | sed -n '1p;$p' | mask; echo
post /api/merge '{"task":"schema"}' | sed -n '1p;$p' | mask; echo

web -H "Authorization: Bearer $TOKEN" "$BASE/api/log"
say "# a worker's view: the task output through the stream endpoint (server-sent events)"
curl -sS -N --max-time 5 -b "$WORK/jar" "$BASE/api/stream?task=r1/docs" | sed -n '1,12p' | mask || true
echo
if [ -n "${BROWSER:-}" ]; then
  say "# the page after the merge (event log with its chain status, health panel)"
  shoot events "#/events" 'Events' 'merged'
  shoot health "#/health" 'event log' 'hash chain intact'
fi
say "git log --oneline --graph fleet/r1/integration"
git -C "$REPO" log --oneline --graph fleet/r1/integration
