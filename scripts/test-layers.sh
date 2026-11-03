#!/bin/sh
# Runs the test suites layer by layer and prints a summary. Run it inside the development
# container with the repository mounted at /work (see README), after `cargo fetch --locked`.
#
#   domain       pure unit and property tests of entities, aggregates, events, sanitizer, redactor, hash chain
#                (fleet-domain), of the fan-out crate (group state machine, comparison, ranking rule) and of the NEO crates: verification contracts, glob matcher, judging,
#                evidence (neo-verify), the goal ledger with its status machine (neo-goals) and the forge model:
#                strict schemas, untrusted issue text, the pull request digest, the rate gate (fleet-forge)
#   application  the orchestrator driven through commands with in-memory fakes for every port,
#                and the port contract suites against those fakes
#   adapters     unit tests of every real adapter, the same contract suites against the real
#                file adapters in temp dirs, the web adapter over real loopback sockets and the origin rule
#                with real child processes, the `gh` adapter against a fake `gh`, the git push against a
#                local bare repository and the consistency of the dogfood/ data (the protected list, the
#                contracts, the plan)
#   soak         bounded memory with chatty workers (FLEET_SOAK_SECS, default 20)
#   dogfood      the shell tests of the checker scripts of dogfood/ (dogfood/tests/run.sh): check-protected.sh and the
#                others are the second judge in every self-development contract, and the contract renderer, the plan
#                and goal scripts and the dependency refresh are exercised with stand-ins (needs git and a POSIX sh)
#   end-to-end   the real `fleet` binary and daemon (also with --web), CLI, TUI rendering and architecture rules
#   remote       remote worktrees against a real OpenSSH server (host key pinning, quoting through a real login
#                shell, forwarding, kill escalation, verification of what a host returns); runs when the image has
#                sshd (docker/Dockerfile.ssh), which also lets the end-to-end layer run its remote tests
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

# With an OpenSSH server in the image the remote tests run (and must not be skipped).
RUN=""
if [ -x /usr/sbin/sshd ]; then RUN="sh scripts/ssh-env.sh"; fi

# The shell tests print one total of their own instead of cargo's `test result` lines.
shell_layer() {
  name=$1; shift
  printf '\n== %s ==\n' "$name"
  out=$("$@" 2>&1) || { printf '%s\n' "$out"; echo "FAILED: $name" >&2; exit 1; }
  printf '%s\n' "$out" | sed -n "s/^dogfood shell tests: \([0-9][0-9]*\) checks passed.*/$name: \1 passed, 0 failed/p"
}

layer domain       cargo test $OPTS -p fleet-domain -p neo-verify -p neo-goals -p neo-context -p fleet-forge -p fleet-fanout
layer application  cargo test $OPTS -p fleet-app --features testing
layer adapters     cargo test $OPTS -p fleet-infra --lib --test contracts --test repo_read --test git_diff --test web_http --test origin --test golden_pack --test forge_gh --test forge_push --test dogfood
layer soak         cargo test $OPTS -p fleet-infra --test soak
shell_layer dogfood sh dogfood/tests/run.sh
if [ -n "$RUN" ]; then
  layer remote     $RUN cargo test $OPTS -p fleet-infra --test ssh_remote
else
  printf '\n== remote ==\nskipped: no OpenSSH server in this image (build docker/Dockerfile.ssh)\n'
fi
layer end-to-end   $RUN cargo test $OPTS -p fleet-cli
