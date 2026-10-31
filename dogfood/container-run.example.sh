#!/bin/sh
# Template of the owner's container wrapper. Copy it OUTSIDE the repository (a contract program may
# not live inside the repository, the worktree or the export), edit the two marked lines, make it
# executable and not writable by group or others, and give its absolute path to
# render-contract.sh / apply-goals.sh as CONTAINER_RUN / --container-run.
#
#   container-run.sh '<shell command>'      runs the command in the development container with the
#                                           current directory mounted at /work
#
# A verification check runs in an export of the commit (a private directory with no .git) with no
# network wish of its own: this wrapper starts the container with NO network unless
# DOGFOOD_NET=bridge is set in the environment of whoever runs it (dogfood/deps-refresh.sh does that
# for `cargo update` only; a check never gets the variable). The conventions are those of the
# Development section of the README: image `fleet-dev` (docker build -f docker/Dockerfile.dev -t
# fleet-dev docker/), cargo registry and target directory in the docker volumes fleet-cargo and
# fleet-target, populated once with `cargo fetch --locked` (README, Development).
# No secret is passed in: HOME inside the container is /tmp/home and nothing of yours is mounted.
set -eu
[ $# -eq 1 ] || { echo "usage: container-run.sh '<shell command>'" >&2; exit 2; }
docker=/usr/local/bin/docker             # EDIT: the absolute path of your docker client
image=${DOGFOOD_IMAGE:-fleet-dev}        # EDIT only if you use another derived image
exec "$docker" run --rm --user "$(id -u):$(id -g)" --cap-drop ALL --security-opt no-new-privileges \
  --network "${DOGFOOD_NET:-none}" \
  -e HOME=/tmp/home -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target -e CARGO_TERM_COLOR=never \
  -v "$PWD:/work" -v fleet-cargo:/cargo -v fleet-target:/target -w /work "$image" sh -c "$1"
