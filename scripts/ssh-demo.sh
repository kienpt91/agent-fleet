#!/bin/sh
# Scripted demo of SSH remote worktrees against a throwaway OpenSSH server that this script starts
# itself (non-root, loopback, keys generated now and deleted at the end). It shows the operator's
# steps (add, list, trust, doctor), a remote task that comes back verified, and what happens when the
# host's key changes. Run it in the fleet-ssh image (docker/Dockerfile.ssh) after scripts/ssh-env.sh:
#
#   sh scripts/ssh-env.sh scripts/ssh-demo.sh /target/release/fleet
set -eu
FLEET=${1:-/target/release/fleet}
WORK=$(mktemp -d)
export HOME="$WORK/home"
export XDG_STATE_HOME="$WORK/state"
mkdir -p "$HOME"
REPO="$WORK/repo"
SSHD="$WORK/sshd"
mkdir -p "$SSHD"
PORT=$((20000 + $$ % 20000))
USER_NAME=$(id -un)
CONFIG="$WORK/fleet.toml"

say() { printf '\n$ %s\n' "$*"; }
run() { say "$*"; "$@"; }
wait_for() {
  i=0
  until "$FLEET" status --json | grep -q "$1"; do
    i=$((i + 1)); [ "$i" -lt 600 ] || { echo "timed out waiting for $1" >&2; exit 1; }
    sleep 0.1
  done
}

# ---- the "remote" machine: an sshd of our own -------------------------------------------------------
ssh-keygen -q -t ed25519 -N '' -C demo -f "$SSHD/host_key"
ssh-keygen -q -t ed25519 -N '' -C demo -f "$SSHD/client_key"
cp "$SSHD/client_key.pub" "$SSHD/authorized_keys"
cat > "$SSHD/sshd_config" <<CONFIG
Port $PORT
ListenAddress 127.0.0.1
HostKey $SSHD/host_key
AuthorizedKeysFile $SSHD/authorized_keys
AllowUsers $USER_NAME
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
StrictModes no
AllowTcpForwarding yes
AllowAgentForwarding yes
X11Forwarding yes
LogLevel ERROR
CONFIG
start_sshd() {
  /usr/sbin/sshd -D -e -f "$SSHD/sshd_config" 2> "$SSHD/sshd.log" &
  echo $! > "$SSHD/sshd.pid"
  i=0; until ssh-keyscan -T 1 -p "$PORT" 127.0.0.1 >/dev/null 2>&1; do
    i=$((i + 1)); [ "$i" -lt 100 ] || { cat "$SSHD/sshd.log" >&2; exit 1; }; sleep 0.1
  done
}
stop_sshd() { kill "$(cat "$SSHD/sshd.pid")" 2>/dev/null || true; rm -f "$SSHD/sshd.pid"; sleep 0.3; }
start_sshd
trap '"$FLEET" shutdown >/dev/null 2>&1 || true; stop_sshd; kill ${DAEMON:-0} 2>/dev/null || true; rm -rf "$WORK"' EXIT

# ---- the project and the fleet ------------------------------------------------------------------------
git init -q -b main "$REPO"
git -C "$REPO" config user.name demo
git -C "$REPO" config user.email demo@example.invalid
printf '# demo project\n' > "$REPO/README.md"
git -C "$REPO" add -A
git -C "$REPO" commit -q -m "initial commit"

cat > "$CONFIG" <<CONFIG
[daemon]
max_sessions = 4
default_worker = "remote-agent"
max_retries = 0
kill_grace_secs = 2

[env]
allow = ["PATH", "HOME", "LANG", "TERM"]

[workers.remote-agent]
command = "sh"
args = ["-c", "echo \"hello from \$(uname -s), written for task \$FLEET_TASK in \$FLEET_WORKTREE\" > from-the-host.txt; printf '{{\"version\":1,\"status\":\"done\",\"summary\":\"wrote a file on the host\"}}' > .fleet/result.json"]
timeout_secs = 60
idle_timeout_secs = 30
CONFIG

echo "The operator compares this fingerprint with the one on the host's console:"
ssh-keygen -l -f "$SSHD/host_key.pub"

run "$FLEET" host add build --config "$CONFIG" --address 127.0.0.1 --port "$PORT" --user "$USER_NAME" \
  --identity "$SSHD/client_key" --base-dir "$WORK/remote-work" --worker sh
run "$FLEET" host list --config "$CONFIG"
say "$FLEET host doctor build --config $CONFIG   # nothing is trusted yet: no connection is made"
"$FLEET" host doctor build --config "$CONFIG" || echo "(exit code $?)"

say "$FLEET host trust build --config $CONFIG   # needs a terminal; here one is made with script(1) and \"yes\" is typed"
printf 'yes\n' | script -qec "$FLEET host trust build --config $CONFIG" /dev/null | tr -d '\r'
run "$FLEET" host list --config "$CONFIG"
run "$FLEET" host doctor build --config "$CONFIG"

"$FLEET" daemon --config "$CONFIG" 2> "$WORK/daemon.log" &
DAEMON=$!
i=0; until "$FLEET" status >/dev/null 2>&1; do i=$((i + 1)); [ "$i" -lt 100 ] || exit 1; sleep 0.1; done

cat > "$WORK/plan.toml" <<PLAN
[run]
id = "r1"
objective = "Try a worker on another machine"
repo = "$REPO"
max_parallel = 2

[[tasks]]
id = "remote"
title = "Write a file on the build host"
host = "build"
PLAN
run "$FLEET" plan "$WORK/plan.toml"
run "$FLEET" start --run r1
wait_for '"completed":1'
run "$FLEET" status
say "git -C \$REPO show fleet/r1/remote:from-the-host.txt   # the file was written on the host and came back as a commit of the local repository"
git -C "$REPO" show fleet/r1/remote:from-the-host.txt
say "git -C \$REPO log --format='%an <%ae>: %s' fleet/r1/remote   # one commit of fleet's own on top of the base"
git -C "$REPO" log --format='%an <%ae>: %s' fleet/r1/remote
say "ls \$WORK/remote-work/r1   # nothing is left on the host"
ls -A "$WORK/remote-work/r1" | wc -l
say "fleet tail remote   # what fleet recorded about the attempt"
"$FLEET" tail remote --lines 8

echo
echo "--- the host is reinstalled: its key changes ---"
stop_sshd
rm -f "$SSHD/host_key" "$SSHD/host_key.pub"
ssh-keygen -q -t ed25519 -N '' -C demo -f "$SSHD/host_key"
start_sshd
ssh-keygen -l -f "$SSHD/host_key.pub"
run "$FLEET" task add --run r1 --title "Another file" --id again --host build
run "$FLEET" start --run r1
wait_for '"failed":1'
run "$FLEET" events --limit 5
say "ls \$WORK/remote-work/r1   # still nothing: the changed key was refused before anything was sent"
ls -A "$WORK/remote-work/r1" | wc -l
