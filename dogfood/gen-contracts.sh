#!/bin/sh
# Regenerates the contract templates of contracts/ from protected-paths.json. The templates share
# their first ten checks (the reusable fragment, also written to contracts/common-checks.json) and
# differ in what follows: the scope of the task (`@ALSO@` stands for the scopes of the tasks it depends on:
# fleet merges finished dependencies into a dependent's branch, and the diff check of a contract reads the
# whole diff since the run's base, so a dependent's scope must allow what its dependencies changed). One template, flaky-test.json, is a REVIEWED
# EXCEPTION: it replaces the glob deny list by the script check with `--except <file>` and adds the
# checks that make a test-only change in a protected file credible (see contracts/README.md).
# Run it after every change of protected-paths.json or of the fragment below, and commit the result:
# crates/fleet-infra/tests/dogfood.rs fails when a template and the list disagree.
#
#   sh dogfood/gen-contracts.sh
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$here"
deny=$(sed -n 's/.*"path": *"\([^"]*\)".*/        "\1"/p' protected-paths.json | sed '$!s/$/,/')
[ -n "$deny" ] || { echo "gen-contracts: no entries in protected-paths.json" >&2; exit 2; }

# The shared checks, as JSON objects separated by commas (no brackets). With the argument
# `exception` the glob deny check is left out and the script check gets `--except @FILE_PATH@`.
common() {
  if [ "${1:-}" != exception ]; then
    cat <<EOC
    {
      "id": "protected-diff",
      "kind": "diff",
      "deny": [
$deny
      ]
    },
EOC
    protected_args='["--repo", "@REPO@", "@BASE@..fleet/@RUN@/@TASK@"]'
  else
    protected_args='["--repo", "@REPO@", "--except", "@FILE_PATH@", "@BASE@..fleet/@RUN@/@TASK@"]'
  fi
  cat <<EOC
    {
      "id": "protected-script",
      "kind": "command",
      "program": "@TOOLS@/check-protected.sh",
      "args": $protected_args,
      "timeout_ms": 60000
    },
    {
      "id": "lock-unchanged",
      "kind": "file",
      "path": "Cargo.lock",
      "assert": "sha256",
      "value": "@LOCK_SHA256@"
    },
    {
      "id": "docs-claims",
      "kind": "command",
      "program": "@TOOLS@/check-docs-claims.sh",
      "args": ["--repo", "@REPO@", "@BASE@..fleet/@RUN@/@TASK@"],
      "timeout_ms": 60000
    },
    {
      "id": "conflict-markers",
      "kind": "command",
      "program": "@TOOLS@/check-conflict-markers.sh",
      "args": ["--tree"],
      "timeout_ms": 60000
    },
    {
      "id": "secret-scan",
      "kind": "command",
      "program": "@TOOLS@/check-secret-tree.sh",
      "timeout_ms": 120000
    },
    {
      "id": "fmt",
      "kind": "command",
      "program": "@CONTAINER_RUN@",
      "args": ["cargo fmt --all -- --check"],
      "timeout_ms": 300000
    },
    {
      "id": "clippy",
      "kind": "command",
      "program": "@CONTAINER_RUN@",
      "args": ["cargo clippy --workspace --all-targets --locked --offline -- -D warnings"],
      "timeout_ms": 1200000
    },
    {
      "id": "architecture",
      "kind": "command",
      "program": "@CONTAINER_RUN@",
      "args": ["cargo test --locked --offline -p fleet-cli --test architecture"],
      "timeout_ms": 1200000
    },
    {
      "id": "layers",
      "kind": "command",
      "program": "@CONTAINER_RUN@",
      "args": ["sh scripts/test-layers.sh"],
      "timeout_ms": 1800000
    }
EOC
}

template() { # name, the checks that follow the fragment (JSON objects, comma separated)
  {
    printf '{\n  "version": 1,\n  "checks": [\n'
    common
    printf ',\n%s\n  ]\n}\n' "$2"
  } > "contracts/$1.json"
}

template single-file '    {
      "id": "scope",
      "kind": "diff",
      "allow": ["@FILE_PATH@"@ALSO@]
    }'
template docs-only '    {
      "id": "scope",
      "kind": "diff",
      "allow": ["README.md", "docs/**"@ALSO@]
    }'
template crate-src '    {
      "id": "scope",
      "kind": "diff",
      "allow": ["crates/@CRATE@/src/**", "crates/@CRATE@/tests/**"@ALSO@]
    }'

# The reviewed exception for a flaky test inside one protected Rust file: the file is the only thing
# that may change, only below its test module (check-test-only.sh), and the named test must still be
# there and survive a loop under CPU load (the stress check: eight busy loops, the test 25 times in a
# row, a failure of any one fails the check). `--lib` means a unit test of the crate's library.
{
  printf '{\n  "version": 1,\n  "checks": [\n'
  common exception
  cat <<'EOT'
,
    {
      "id": "scope",
      "kind": "diff",
      "allow": ["@FILE_PATH@"]
    },
    {
      "id": "kept",
      "kind": "file",
      "path": "@FILE_PATH@",
      "assert": "contains",
      "value": "fn @TEST_NAME@"
    },
    {
      "id": "test-only",
      "kind": "command",
      "program": "@TOOLS@/check-test-only.sh",
      "args": ["--repo", "@REPO@", "--file", "@FILE_PATH@", "@BASE@..fleet/@RUN@/@TASK@"],
      "timeout_ms": 60000
    },
    {
      "id": "stress",
      "kind": "command",
      "program": "@CONTAINER_RUN@",
      "args": ["cargo test -p @PACKAGE@ --lib --no-run --offline --locked -q && for j in 1 2 3 4 5 6 7 8; do (timeout 280 sh -c \"while :; do :; done\" &) ; done; i=0; while [ $i -lt 25 ]; do cargo test -p @PACKAGE@ --lib @TEST_FILTER@ --offline --locked -q 2>&1 | grep -E \"test result\" | grep -q \"1 passed\" || { echo FAILED-AT-$i; exit 1; }; i=$((i+1)); done; echo stress-ok"],
      "timeout_ms": 900000
    }
  ]
}
EOT
} > contracts/flaky-test.json

{ printf '[\n'; common; printf '\n]\n'; } > contracts/common-checks.json
cat > contracts/human-only.json <<'EOF'
{
  "version": 1,
  "checks": [
    {
      "id": "no-change-at-all",
      "kind": "diff",
      "deny": ["**"]
    },
    {
      "id": "never-exists",
      "kind": "file",
      "path": "dogfood/.human-only-this-file-never-exists",
      "assert": "exists"
    }
  ]
}
EOF
echo "gen-contracts: wrote single-file, docs-only, crate-src, flaky-test, human-only and common-checks in $here/contracts"
