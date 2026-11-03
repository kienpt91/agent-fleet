#!/bin/sh
# Line coverage per layer with rustc's source-based instrumentation (`-C instrument-coverage`) and
# the llvm-profdata / llvm-cov tools of the official `llvm-tools` rustup component. No third-party
# coverage crate. Run inside the development container (docker/Dockerfile.dev) with the repository
# mounted at /work and dependencies fetched:
#
#   docker run --rm -v "$PWD:/work" -v fleet-target:/target -w /work fleet-dev scripts/coverage.sh
#
# What is counted: the library and binary code of each crate. Not counted: files under tests/,
# examples, the in-memory fakes (crates/fleet-app/src/testing*), test helpers (testutil.rs) and everything
# after the first `#[cfg(test)]` of a source file (the unit tests themselves, which would otherwise
# inflate every number). Processes killed with SIGKILL by the recovery tests write no profile, so
# their coverage is lost; coverage of the daemon comes from the runs that stop it normally.
set -eu
cd "$(dirname "$0")/.."
SYSROOT=$(rustc --print sysroot)
HOST=$(rustc -vV | sed -n 's/^host: //p')
BIN="$SYSROOT/lib/rustlib/$HOST/bin"
COV=${COV_DIR:-/target/cov}
rm -rf "$COV"
mkdir -p "$COV/raw"
export RUSTFLAGS="-C instrument-coverage"
export CARGO_TARGET_DIR="$COV/build"
export LLVM_PROFILE_FILE="$COV/raw/p-%p-%m.profraw"
export FLEET_SOAK_SECS=${FLEET_SOAK_SECS:-8}

cargo test --workspace --locked --offline --no-fail-fast
objects=$(cargo test --workspace --locked --offline --no-run --message-format=json 2>/dev/null \
  | grep -o '"executable":"[^"]*"' | cut -d'"' -f4 | sort -u)
[ -x "$CARGO_TARGET_DIR/debug/fleet" ] && objects="$objects $CARGO_TARGET_DIR/debug/fleet"

"$BIN/llvm-profdata" merge -sparse "$COV"/raw/*.profraw -o "$COV/all.profdata"
first=""; rest=""
for o in $objects; do
  if [ -z "$first" ]; then first=$o; else rest="$rest -object=$o"; fi
done
# shellcheck disable=SC2086
"$BIN/llvm-cov" export --format=lcov --instr-profile="$COV/all.profdata" \
  --ignore-filename-regex='(/\.cargo/|/cargo/registry/|/rustc/|/tests/|/examples/|/testing|testutil\.rs)' \
  "$first" $rest > "$COV/lcov.info"

awk '
  /^SF:/ {
    file = substr($0, 4); cutoff = 1000000000
    cmd = "grep -n -m1 \"#\\[cfg(test)\\]\" \"" file "\" 2>/dev/null"
    if ((cmd | getline hit) > 0) { split(hit, parts, ":"); cutoff = parts[1] }
    close(cmd)
    crate = "other"
    if (file ~ /crates\/(fleet-domain|neo-verify|neo-goals|neo-context)\//) crate = "domain"
    else if (file ~ /crates\/fleet-app\//) crate = "application"
    else if (file ~ /crates\/fleet-infra\//) crate = "infrastructure"
    else if (file ~ /crates\/fleet-cli\//) crate = "cli"
    next
  }
  /^DA:/ {
    split(substr($0, 4), da, ",")
    if (da[1] + 0 < cutoff) { total[crate]++; if (da[2] + 0 > 0) covered[crate]++ }
  }
  END {
    printf "%-16s %8s %8s %8s\n", "layer", "lines", "covered", "percent"
    n = split("domain application infrastructure cli other", order, " ")
    for (i = 1; i <= n; i++) {
      c = order[i]
      if (total[c] == 0) continue
      printf "%-16s %8d %8d %7.1f%%\n", c, total[c], covered[c], 100 * covered[c] / total[c]
      all += total[c]; allc += covered[c]
    }
    printf "%-16s %8d %8d %7.1f%%\n", "overall", all, allc, 100 * allc / all
  }
' "$COV/lcov.info" | tee "$COV/summary.txt"
