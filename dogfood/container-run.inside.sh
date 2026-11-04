#!/bin/sh
# The container wrapper for when everything already runs INSIDE the development container: the
# maintainers' dry run (dryrun.sh) and CI-like runs, where no docker client is available. It gives the
# command what container-run.example.sh gives it: a clean environment (HOME, cargo variables), the tree
# at one fixed path (/tmp/work: cargo's cache is keyed by path) and no other state. Copies the current
# directory first, so the command cannot change the export, and runs ONE command at a time (two
# verification checks may start together; they share the one fixed path).
#
#   container-run.inside.sh '<shell command>'
#
# The tests run on 6 threads (RUST_TEST_THREADS, override it in the environment of the dry run): a few tests bound their wall-clock
# time for an idle machine (README, Tests) and fail when their own siblings and other jobs starve them, which on a loaded
# machine failed a task of the dry run eight times in a row. No test is changed.
#
# Not for real runs on a machine that has docker: there, container-run.example.sh adds the network
# and file isolation of a container, which this script does not.
set -eu
[ $# -eq 1 ] || { echo "usage: container-run.inside.sh '<shell command>'" >&2; exit 2; }
src=$(pwd)
lock=${DOGFOOD_INSIDE_LOCK:-/tmp/container-run-inside.lock}
exec 9>"$lock"
flock 9
work=/tmp/work
rm -rf "$work"
mkdir -p "$work" /tmp/home
cp -a "$src/." "$work/"
cd "$work"
exec env -i HOME=/tmp/home CARGO_HOME="${CARGO_HOME:-/cargo}" CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/target}" CARGO_TERM_COLOR=never \
  RUSTUP_HOME="${RUSTUP_HOME:-/usr/local/rustup}" PATH=/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin FLEET_SOAK_SECS="${FLEET_SOAK_SECS:-2}" RUST_TEST_THREADS="${RUST_TEST_THREADS:-6}" \
  sh -c "$1"
