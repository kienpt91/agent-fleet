#!/bin/sh
# Runs the test suites layer by layer and prints a summary. Run it inside the development
# container with the repository mounted at /work (see README), after `cargo fetch --locked`.
#
#   domain       pure unit and property tests of entities, aggregates, events, sanitizer, redactor, hash chain
#                (fleet-domain) and of the NEO crates: verification contracts, glob matcher, judging,
#                evidence (neo-verify) and the goal ledger with its status machine (neo-goals)
#   application  the orchestrator driven through commands with in-memory fakes for every port,
#                and the port contract suites against those fakes
#   adapters     unit tests of every real adapter, the same contract suites against the real
#                file adapters in temp dirs, the web adapter over real loopback sockets and the origin rule
#                with real child processes
#   soak         bounded memory with chatty workers (FLEET_SOAK_SECS, default 20)
#   end-to-end   the real `fleet` binary and daemon (also with --web), CLI, TUI rendering and architecture rules
set -eu
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/target}
OPTS="--locked --offline"

layer() {
  name=$1; shift
  printf '\n== %s ==\n' "$name"
  out=$("$@" 2>&1) || { printf '%s\n' "$out"; echo "FAILED: $name" >&2; exit 1; }
  printf '%s\n' "$out" | grep -E '^test result' | awk -v n="$name" '{p+=$4; f+=$6} END {printf "%s: %d passed, %d failed\n", n, p, f}'
}

layer domain       cargo test $OPTS -p fleet-domain -p neo-verify -p neo-goals -p neo-context
layer application  cargo test $OPTS -p fleet-app --features testing
layer adapters     cargo test $OPTS -p fleet-infra --lib --test contracts --test repo_read --test web_http --test origin --test golden_pack
layer soak         cargo test $OPTS -p fleet-infra --test soak
layer end-to-end   cargo test $OPTS -p fleet-cli
