#!/bin/sh
# Runs a command in the `fleet-ssh` test container with what OpenSSH needs: a passwd entry for the
# (arbitrary) uid the container runs as. The image makes /etc/passwd writable for this; nothing else
# is changed and nothing persists outside the throwaway container.
#
#   sh scripts/ssh-env.sh cargo test -p fleet-infra --test ssh_remote
set -eu
uid=$(id -u)
gid=$(id -g)
if ! getent passwd "$uid" >/dev/null 2>&1; then
  if [ ! -w /etc/passwd ]; then
    echo "ssh-env: /etc/passwd is not writable; run this in the fleet-ssh image (docker/Dockerfile.ssh)" >&2
    exit 1
  fi
  echo "fleetssh:x:$uid:$gid:fleet ssh test user:${HOME:-/tmp}:/bin/sh" >> /etc/passwd
fi
mkdir -p "${HOME:-/tmp}"
export FLEET_SSH_TESTS=1
exec "$@"
