#!/bin/sh
# Template of the owner's container wrapper. Copy it OUTSIDE the repository (a contract program may
# not live inside the repository, the worktree or the export), edit the marked lines, make it
# executable and not writable by group or others, and give its absolute path to
# render-contract.sh / apply-goals.sh as CONTAINER_RUN / --container-run.
#
#   container-run.sh '<shell command>'                  checks: offline, nothing writable but the target volume
#   DOGFOOD_MODE=online container-run.sh '<command>'    YOU only: network on, cargo registry and the directory writable
#                                                       (`cargo fetch --locked`, `cargo update` in deps-refresh.sh)
#
# Verification checks run in an export of a commit (a private directory without .git) and call this
# with one argument, a shell command. The wrapper runs it in the development container:
#   - the CURRENT DIRECTORY (the export) is mounted read-only at /work: a check cannot change the
#     tree it judges, and nothing of yours is mounted;
#   - NO network, all capabilities dropped, no-new-privileges, your uid, HOME=/tmp/home inside;
#   - the cargo registry (a docker volume) is mounted read-only and filled only by `cargo fetch --locked`
#     in online mode, so a check can build offline from the locked crates and cannot add to the cache;
#   - the cargo target directory is a second, fixed volume, so every export builds on a warm cache.
#     (Two checks may run at the same time; cargo serializes them on the target directory's lock.)
# Checks are started with an environment of PATH, HOME, USER, LANG and TERM only: DOGFOOD_MODE can never
# reach them. No secret is passed into the container. This is not a boundary against a hostile test
# (README, Limitations): it keeps a check from the network, from your files and from its own tree.
#
# THE TWO THINGS YOU SET (both below):
#   docker   the absolute path of your docker client (`command -v docker`; Homebrew on Apple silicon: /opt/homebrew/bin/docker)
#   image    the development image, built once: docker build -f docker/Dockerfile.dev -t fleet-dev docker/
# and, if you like, the names of the two volumes. Create them once (RUNBOOK.md, section 2):
#   docker volume create fleet-dogfood-cargo; docker volume create fleet-dogfood-target
#   docker run --rm -u 0 -v fleet-dogfood-cargo:/cargo -v fleet-dogfood-target:/target fleet-dev chown "$(id -u):$(id -g)" /cargo /target
#   DOGFOOD_MODE=online container-run.sh 'cargo fetch --locked'      # in a clone of the repository, once and after every lock change
set -eu
[ $# -eq 1 ] || { echo "usage: container-run.sh '<shell command>'" >&2; exit 2; }
docker=/usr/local/bin/docker                  # EDIT: the absolute path of your docker client
image=fleet-dev                               # EDIT only if you use another derived image; NOT fleet-ssh (RUNBOOK.md, section 1: the `remote` layer is not part of a contract)
cargo_volume=fleet-dogfood-cargo
target_volume=fleet-dogfood-target
repo=$(pwd)
case ${DOGFOOD_MODE:-offline} in
  offline) net=none;   cargo_mount="$cargo_volume:/cargo:ro"; repo_mount="$repo:/work:ro" ;;
  online)  net=bridge; cargo_mount="$cargo_volume:/cargo";    repo_mount="$repo:/work" ;;
  *) echo "container-run: DOGFOOD_MODE must be offline or online" >&2; exit 2 ;;
esac
exec "$docker" run --rm \
  --user "$(id -u):$(id -g)" \
  --cap-drop ALL --security-opt no-new-privileges --network "$net" \
  -e HOME=/tmp/home -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target -e CARGO_TERM_COLOR=never \
  -v "$repo_mount" -v "$cargo_mount" -v "$target_volume:/target" -w /work \
  "$image" sh -c "$1"
