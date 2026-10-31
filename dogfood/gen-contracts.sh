#!/bin/sh
# Regenerates the contract templates of contracts/ from protected-paths.json. The templates share
# their first nine checks (the reusable fragment, also written to contracts/common-checks.json) and
# differ in the last, the scope of the task. Run it after every change of protected-paths.json or of
# the fragment below, and commit the result: crates/fleet-infra/tests/dogfood.rs fails when a
# template and the list disagree.
#
#   sh dogfood/gen-contracts.sh
set -eu
here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$here"
deny=$(sed -n 's/.*"path": *"\([^"]*\)".*/        "\1"/p' protected-paths.json | sed '$!s/$/,/')
[ -n "$deny" ] || { echo "gen-contracts: no entries in protected-paths.json" >&2; exit 2; }

common() {
  cat <<EOC
    {
      "id": "protected-diff",
      "kind": "diff",
      "deny": [
$deny
      ]
    },
    {
      "id": "protected-script",
      "kind": "command",
      "program": "@TOOLS@/check-protected.sh",
      "args": ["--repo", "@REPO@", "@BASE@..fleet/@RUN@/@TASK@"],
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
      "timeout_ms": 120000
    },
    {
      "id": "clippy",
      "kind": "command",
      "program": "@CONTAINER_RUN@",
      "args": ["cargo clippy --workspace --all-targets --locked --offline -- -D warnings"],
      "timeout_ms": 900000
    },
    {
      "id": "architecture",
      "kind": "command",
      "program": "@CONTAINER_RUN@",
      "args": ["cargo test --locked --offline -p fleet-cli --test architecture"],
      "timeout_ms": 900000
    },
    {
      "id": "layers",
      "kind": "command",
      "program": "@CONTAINER_RUN@",
      "args": ["sh scripts/test-layers.sh"],
      "timeout_ms": 1500000
    }
EOC
}

template() { # name, the scope check (JSON text)
  {
    printf '{\n  "version": 1,\n  "checks": [\n'
    common
    printf ',\n%s\n  ]\n}\n' "$2"
  } > "contracts/$1.json"
}

template single-file '    {
      "id": "scope",
      "kind": "diff",
      "allow": ["@FILE_PATH@"]
    }'
template docs-only '    {
      "id": "scope",
      "kind": "diff",
      "allow": ["README.md", "docs/**"]
    }'
template crate-src '    {
      "id": "scope",
      "kind": "diff",
      "allow": ["crates/@CRATE@/src/**", "crates/@CRATE@/tests/**"]
    }'
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
echo "gen-contracts: wrote single-file, docs-only, crate-src, human-only and common-checks in $here/contracts"
