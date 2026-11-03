#!/bin/sh
# Turns a contract template of dogfood/contracts/ into a contract that `fleet goals add --contract`
# accepts, by filling in the @NAME@ placeholders.
#
#   render-contract.sh <template.json> <out.json> NAME=VALUE...
#
# Placeholders and what they must be:
#   TOOLS          absolute directory with the installed check scripts (install-tools.sh), outside the repository
#   CONTAINER_RUN  absolute path of the owner's container wrapper (container-run.example.sh), outside the repository
#   REPO           absolute path of the repository the run belongs to
#   BASE           the run's base commit or ref (a commit hash is best: the ref must not move during the run)
#   RUN, TASK      the run id and task id: the task's branch is fleet/<RUN>/<TASK>
#   LOCK_SHA256    SHA-256 of Cargo.lock at BASE; worked out from REPO and BASE when not given
#   FILE_PATH      the one file a task may change (relative, no wildcards)
#   CRATE          the name of a crate below crates/ that a task may change
#   PACKAGE        a cargo package name (`fleet-infra`): the package of a flaky test
#   TEST_NAME      the name of that test function (`a_reply_that_is_delivered`): it must still be in FILE_PATH
#   TEST_FILTER    the filter that selects exactly that test for `cargo test` (`server::tests::a_reply_that_is_delivered`)
#   ALSO           optional, comma separated paths or globs added to the `allow` list of the scope check: the scopes of the
#                  tasks this one depends on (fleet merges finished dependencies into a dependent's branch and the diff
#                  check reads the whole diff since the run's base). apply-goals.sh works it out from the plan.
#   (a template with TEST_NAME is a reviewed exception: FILE_PATH must then be crates/<crate>/src/**.rs or crates/<crate>/tests/**.rs)
# Values may contain only letters, digits and . _ / - (BASE also ^ and ~, TEST_FILTER also :); nothing else is
# substituted, so a value can never add a field to the JSON. A placeholder left unfilled, a value
# no template uses and a malformed value are errors. An unrendered template is never accepted
# by fleet: `@TOOLS@/...` is not an absolute program path.
set -u
set -f

fail() { printf 'render-contract: %s\n' "$1" >&2; exit 2; }

[ $# -ge 2 ] || fail "usage: render-contract.sh <template.json> <out.json> NAME=VALUE..."
template=$1; out=$2; shift 2
[ -f "$template" ] && [ -r "$template" ] || fail "cannot read the template $template"
[ ! -e "$out" ] || [ -f "$out" ] || fail "$out exists and is not a file"

names=$(grep -o '@[A-Z_][A-Z0-9_]*@' "$template" | sort -u | tr -d '@')
# A template without a placeholder (the human-only contract) is copied as it is.

get() { # NAME -> value given on the command line
  eval "printf '%s' \"\${val_$1:-}\""
}
absolute() { case $2 in /*) ;; *) fail "$1 must be an absolute path" ;; esac; }
no_dotdot() { case "/${2#/}/" in */../*|*//*|*/./*) fail "$1 must not contain .. or empty or . segments" ;; esac; }

for arg in "$@"; do
  case $arg in
    [A-Z]*=*) ;;
    *) fail "not a NAME=VALUE argument: $arg" ;;
  esac
  k=${arg%%=*}; v=${arg#*=}
  case $k in *[!A-Z0-9_]*|"") fail "bad placeholder name: $k" ;; esac
  printf '%s\n' "$names" | grep -qx "$k" || fail "the template has no placeholder @$k@"
  [ -n "$v" ] || fail "$k is empty"
  case $k in
    BASE) case $v in *[!A-Za-z0-9_./^~-]*|-*) fail "BASE has characters a ref does not" ;; esac ;;
    TEST_FILTER) case $v in *[!A-Za-z0-9_:]*|[!a-z_]*) fail "TEST_FILTER may contain only letters, digits, _ and :, and starts with a lowercase letter or _" ;; esac ;;
    ALSO) case $v in *[!A-Za-z0-9_./*,-]*) fail "ALSO may contain only letters, digits and . _ / * - , (a comma separated list of paths or globs)" ;; esac ;;
    *) case $v in *[!A-Za-z0-9_./-]*) fail "$k may contain only letters, digits and . _ / -" ;; esac ;;
  esac
  case $k in
    TOOLS|CONTAINER_RUN|REPO) absolute "$k" "$v"; no_dotdot "$k" "$v" ;;
    RUN|TASK|CRATE) case $v in [a-z0-9]*) ;; *) fail "$k must start with a lowercase letter or digit" ;; esac
                    case $v in *[!a-z0-9_-]*) fail "$k may contain only a-z 0-9 _ -" ;; esac ;;
    FILE_PATH) case $v in /*) fail "FILE_PATH must be relative" ;; esac; no_dotdot FILE_PATH "$v" ;;
    PACKAGE) case $v in [a-z]*) ;; *) fail "PACKAGE must start with a lowercase letter" ;; esac
             case $v in *[!a-z0-9_-]*) fail "PACKAGE may contain only a-z 0-9 _ -" ;; esac ;;
    TEST_NAME) case $v in [a-z_]*) ;; *) fail "TEST_NAME must start with a lowercase letter or _" ;; esac
               case $v in *[!a-z0-9_]*) fail "TEST_NAME may contain only a-z 0-9 _" ;; esac ;;
    LOCK_SHA256) case $v in *[!0-9a-f]*) fail "LOCK_SHA256 must be lowercase hex" ;; esac
                 [ ${#v} -eq 64 ] || fail "LOCK_SHA256 must be 64 hex digits" ;;
    ALSO) also_json=; also_n=0
          case $v in ,*|*,|*,,*) fail "ALSO has an empty entry" ;; esac
          oldifs=$IFS; IFS=,
          for item in $v; do
            IFS=$oldifs
            [ -n "$item" ] || fail "ALSO has an empty entry"
            case $item in /*) fail "ALSO entries must be relative" ;; esac
            no_dotdot ALSO "$item"
            case $item in *'***'*) fail "ALSO: a run of three stars in $item" ;; esac
            [ ${#item} -le 120 ] || fail "ALSO: an entry is longer than 120 bytes"
            also_n=$((also_n + 1)); [ "$also_n" -le 40 ] || fail "ALSO has more than 40 entries"
            also_json="$also_json, \"$item\""
            IFS=,
          done
          IFS=$oldifs ;;
  esac
  eval "val_$k=\$v"
done

# a reviewed exception may name only a Rust source or test file of a crate
if printf '%s\n' "$names" | grep -qx TEST_NAME; then
  fp=$(get FILE_PATH)
  case $fp in
    */build.rs|crates/fleet-cli/tests/architecture.rs) fail "FILE_PATH $fp can never be a reviewed exception" ;;
    crates/*/src/*.rs|crates/*/tests/*.rs) ;;
    *) fail "FILE_PATH of a reviewed exception must be crates/<crate>/src/**.rs or crates/<crate>/tests/**.rs" ;;
  esac
fi

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 | cut -d' ' -f1
  else fail "neither sha256sum nor shasum is available"; fi
}

# LOCK_SHA256 is derived, never guessed: from Cargo.lock at BASE in REPO.
if printf '%s\n' "$names" | grep -qx LOCK_SHA256 && [ -z "$(get LOCK_SHA256)" ]; then
  repo=$(get REPO); base=$(get BASE)
  [ -n "$repo" ] && [ -n "$base" ] || fail "LOCK_SHA256 is worked out from REPO and BASE: give both, or give LOCK_SHA256"
  gitc() { GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_NOSYSTEM=1 git -C "$repo" "$@"; }
  gitc cat-file -e "$base:Cargo.lock" 2>/dev/null || fail "cannot read Cargo.lock at $base in $repo"
  lock=$(gitc show "$base:Cargo.lock" 2>/dev/null | sha256_of) || fail "cannot hash Cargo.lock at $base in $repo"
  [ ${#lock} -eq 64 ] || fail "cannot hash Cargo.lock at $base in $repo"
  val_LOCK_SHA256=$lock
fi

tmp=$(mktemp "${TMPDIR:-/tmp}/render-contract.XXXXXX") || fail "cannot create a temporary file"
trap 'rm -f "$tmp" "$tmp.next"' EXIT HUP INT TERM
cp "$template" "$tmp"
for n in $names; do
  if [ "$n" = ALSO ]; then
    v=${also_json:-}   # `, "a", "b"` built above from validated entries, or nothing
  else
    v=$(get "$n")
    [ -n "$v" ] || fail "the placeholder @$n@ has no value (give $n=...)"
  fi
  sed "s|@$n@|$(printf '%s' "$v" | sed 's/[&|\\]/\\&/g')|g" "$tmp" > "$tmp.next" && mv "$tmp.next" "$tmp"
done
if grep -q '@[A-Z_][A-Z0-9_]*@' "$tmp"; then fail "a placeholder is left in the output"; fi
mv "$tmp" "$out"
chmod 0644 "$out"
printf 'render-contract: wrote %s (%s)\n' "$out" "$(printf '%s' "$names" | tr '\n' ' ')"
