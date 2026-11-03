#!/bin/sh
# Template of the owner's container wrapper. Copy it OUTSIDE the repository (a contract program may
# not live inside the repository, the worktree or the export), edit the marked lines, make it
# executable and not writable by group or others, and give its absolute path to
# render-contract.sh / apply-goals.sh as CONTAINER_RUN / --container-run.
#
#   container-run.sh '<shell command>'                  checks: offline, a FRESH scratch target seeded from the warm one
#   DOGFOOD_MODE=online container-run.sh '<command>'    YOU only: network on, cargo registry and the directory writable
#                                                       (`cargo fetch --locked`, `cargo update` in deps-refresh.sh)
#   DOGFOOD_MODE=warm container-run.sh '<command>'      YOU only: offline, the WARM target writable, run in a clone of a
#                                                       commit you have reviewed (never in the export of a task), to
#                                                       refresh the cache that checks start from
#
# Verification checks run in an export of a commit (a private directory without .git) and call this
# with one argument, a shell command. The wrapper runs it in the development container:
#   - the CURRENT DIRECTORY (the export) is mounted read-only at /work: a check cannot change the
#     tree it judges, and nothing of yours is mounted;
#   - NO network, all capabilities dropped, no-new-privileges, your uid, HOME=/tmp/home inside;
#   - the cargo registry (a docker volume) is mounted read-only and filled only by `cargo fetch --locked`
#     in online mode, so a check can build offline from the locked crates and cannot add to the cache;
#   - the cargo target directory is a FRESH volume for this one check, removed when the check ends: it is
#     filled from the warm target volume, which the check sees read-only at /warm-target. A check can build
#     on a warm cache and can leave nothing behind for the next task: a hostile test of one task cannot
#     poison the build artifacts that another task's checks reuse. The warm target is written only in `warm`
#     mode, by you, on a commit you reviewed. Cost: a copy of the warm target (a few GB) per check.
#     (Two checks may run at the same time: each has a target of its own.)
# Checks are started with an environment of PATH, HOME, USER, LANG and TERM only: DOGFOOD_MODE can never
# reach them. No secret is passed into the container. This is not a boundary against a hostile test
# (README, Limitations): it keeps a check from the network, from your files and from the checks of other
# tasks. A scratch volume that outlives a killed wrapper is removed by `reset-cache.sh --scratch`.
#
# THE TWO THINGS YOU SET (both below):
#   docker   the absolute path of your docker client (`command -v docker`; Homebrew on Apple silicon: /opt/homebrew/bin/docker)
#   image    the development image, built once: docker build -f docker/Dockerfile.dev -t fleet-dev docker/
# and, if you like, the names of the two volumes. Create them once (RUNBOOK.md, section 2):
#   docker volume create fleet-dogfood-cargo; docker volume create fleet-dogfood-target
#   docker run --rm -u 0 -v fleet-dogfood-cargo:/cargo -v fleet-dogfood-target:/target fleet-dev chown "$(id -u):$(id -g)" /cargo /target
#   DOGFOOD_MODE=online container-run.sh 'cargo fetch --locked'      # in a clone of the repository, once and after every lock change
#   DOGFOOD_MODE=warm container-run.sh 'cargo clippy --workspace --all-targets --locked --offline && cargo test --workspace --no-run --locked --offline'
set -eu
[ $# -eq 1 ] || { echo "usage: container-run.sh '<shell command>'" >&2; exit 2; }
docker=/usr/local/bin/docker                  # EDIT: the absolute path of your docker client
image=fleet-dev                               # EDIT only if you use another derived image; NOT fleet-ssh (RUNBOOK.md, section 1: the `remote` layer is not part of a contract)
cargo_volume=fleet-dogfood-cargo
warm_volume=fleet-dogfood-target
scratch_prefix=fleet-dogfood-scratch          # reset-cache.sh --scratch removes volumes of this name
repo=$(pwd)
uid=$(id -u)
gid=$(id -g)
run_env="-e HOME=/tmp/home -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target -e CARGO_TERM_COLOR=never"
safety="--cap-drop ALL --security-opt no-new-privileges"
case ${DOGFOOD_MODE:-offline} in
  offline)
    scratch=$scratch_prefix-$$-$(date +%s)
    "$docker" volume create "$scratch" >/dev/null
    cleanup() { "$docker" volume rm -f "$scratch" >/dev/null 2>&1 || true; }
    trap cleanup EXIT
    trap 'exit 143' HUP INT TERM
    # a new volume belongs to root: hand its top directory to the check's user, with nothing else
    "$docker" run --rm -u 0 --network none --cap-drop ALL --cap-add CHOWN --security-opt no-new-privileges \
      -v "$scratch:/target" "$image" chown "$uid:$gid" /target
    # shellcheck disable=SC2086
    "$docker" run --rm --user "$uid:$gid" $safety --network none $run_env \
      -v "$repo:/work:ro" -v "$cargo_volume:/cargo:ro" -v "$warm_volume:/warm-target:ro" -v "$scratch:/target" -w /work \
      "$image" sh -c 'cp -a /warm-target/. /target/ 2>/dev/null || true; exec sh -c "$1"' sh "$1"
    ;;
  warm)
    # shellcheck disable=SC2086
    exec "$docker" run --rm --user "$uid:$gid" $safety --network none $run_env \
      -v "$repo:/work:ro" -v "$cargo_volume:/cargo:ro" -v "$warm_volume:/target" -w /work \
      "$image" sh -c "$1"
    ;;
  online)
    # shellcheck disable=SC2086
    exec "$docker" run --rm --user "$uid:$gid" $safety --network bridge $run_env \
      -v "$repo:/work" -v "$cargo_volume:/cargo" -w /work \
      "$image" sh -c "$1"
    ;;
  *) echo "container-run: DOGFOOD_MODE must be offline, warm or online" >&2; exit 2 ;;
esac
