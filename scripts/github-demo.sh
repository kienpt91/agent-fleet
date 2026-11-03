#!/bin/sh
# GitHub integration demo: an issue becomes a task, a worker does it, and a pull request is opened
# only after an approval bound to its exact content. `gh` is a stand-in script that replays canned
# answers (examples/github), the remote is a local bare repository: nothing here touches a network.
# Usage: scripts/github-demo.sh [path-to-fleet-binary]   (run it inside a container, see README)
set -eu
FLEET=${1:-/target/release/fleet}
HERE=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d)
export HOME="$WORK/home"
export XDG_STATE_HOME="$WORK/state"
mkdir -p "$HOME" "$WORK/bin" "$WORK/gh-log"
REPO="$WORK/repo"
REMOTE="$WORK/remote.git"

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
git init -q --bare -b main "$REMOTE"
git -C "$REPO" remote add origin "$REMOTE"

# the stand-in for gh: logs its arguments, replays a canned answer by endpoint
cat > "$WORK/bin/gh" <<GH
#!/bin/sh
LOG="$WORK/gh-log"; EX="$HERE/examples/github"
{ printf 'gh'; for a in "\$@"; do printf ' [%s]' "\$a"; done; printf '\n'; } >> "\$LOG/calls"
for last; do :; done
case "\$last" in
  repos/o/r/issues\?*) f=issues_list.json ;;
  repos/o/r/issues/7) f=issue_7.json ;;
  repos/o/r/pulls) f=pull_created.json ;;
  repos/o/r/pulls/12) f=pull_created.json ;;
  repos/o/r/commits/*/check-runs*) f=check_runs.json ;;
  repos/o/r/commits/*/status) f=commit_status.json ;;
  repos/o/r/pulls/12/reviews*) f=reviews.json ;;
  repos/o/r) f=repo.json ;;
  *) echo "gh: Not Found (HTTP 404)" >&2; printf 'HTTP/2.0 404 Not Found\r\n\r\n{"message":"Not Found"}'; exit 1 ;;
esac
head=\$(git -C "$REPO" rev-parse --verify -q refs/heads/fleet/r1/fix || echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)
printf 'HTTP/2.0 200 OK\r\nx-ratelimit-remaining: 4990\r\nx-ratelimit-reset: 9999999999\r\n\r\n'
sed "s/@@HEAD@@/\$head/" "\$EX/\$f"
GH
chmod +x "$WORK/bin/gh"

cat > "$WORK/worker.sh" <<'W'
#!/bin/sh
set -eu
echo "fixed" > fix.txt
printf '{"version":1,"status":"done","summary":"fixed the crash"}\n' > .fleet/result.json
W
chmod +x "$WORK/worker.sh"

cat > "$WORK/fleet.toml" <<CONFIG
[daemon]
max_sessions = 2
default_worker = "agent"
kill_grace_secs = 2

[env]
allow = ["PATH", "HOME", "LANG", "TERM"]

[workers.agent]
command = "$WORK/worker.sh"
args = []
timeout_secs = 60

[github]
enabled = true
repo = "o/r"
allow_local_remote = true
CONFIG

PATH="$WORK/bin:$PATH" "$FLEET" daemon --config "$WORK/fleet.toml" 2> "$WORK/daemon.log" &
DAEMON=$!
trap '"$FLEET" shutdown >/dev/null 2>&1 || true; kill $DAEMON 2>/dev/null || true; rm -rf "$WORK"' EXIT
i=0; until "$FLEET" status >/dev/null 2>&1; do i=$((i + 1)); [ "$i" -lt 100 ] || exit 1; sleep 0.1; done

run "$FLEET" run create --objective "Fix the crash" --repo "$REPO" --id r1
run "$FLEET" github status
run "$FLEET" issue list
run "$FLEET" issue show 7
run "$FLEET" issue to-task 7 --run r1 --id fix
run "$FLEET" start --run r1
wait_for '"completed":1'
say "$FLEET pr create --task r1/fix   # exits 5: nothing is pushed before a human approves exactly this"
"$FLEET" pr create --task r1/fix || echo "(exit code $?)"
run "$FLEET" approval list
ID=$("$FLEET" approval list --json | sed -n 's/.*"id":"\(a[0-9]*\)".*/\1/p' | head -n 1)
run "$FLEET" approve "$ID"
say "$FLEET pr create --task r1/fix --title 'A different title'   # any change needs a new approval"
"$FLEET" pr create --task r1/fix --title "A different title" || echo "(exit code $?)"
run "$FLEET" pr create --task r1/fix
say "git -C remote.git for-each-ref   # exactly the approved commit on exactly one ref"
git -C "$REMOTE" for-each-ref
say "$FLEET pr create --task r1/fix   # the same commit is never published twice"
"$FLEET" pr create --task r1/fix || echo "(exit code $?)"
run "$FLEET" pr status 12
say "the calls gh received (fixed argv, no token anywhere)"
cat "$WORK/gh-log/calls"
run "$FLEET" verify-log
