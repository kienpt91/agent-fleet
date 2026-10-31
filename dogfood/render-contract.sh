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
# Values may contain only letters, digits and . _ / - (BASE also ^ and ~); nothing else is
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
    *) case $v in *[!A-Za-z0-9_./-]*) fail "$k may contain only letters, digits and . _ / -" ;; esac ;;
  esac
  case $k in
    TOOLS|CONTAINER_RUN|REPO) absolute "$k" "$v"; no_dotdot "$k" "$v" ;;
    RUN|TASK|CRATE) case $v in [a-z0-9]*) ;; *) fail "$k must start with a lowercase letter or digit" ;; esac
                    case $v in *[!a-z0-9_-]*) fail "$k may contain only a-z 0-9 _ -" ;; esac ;;
    FILE_PATH) case $v in /*) fail "FILE_PATH must be relative" ;; esac; no_dotdot FILE_PATH "$v" ;;
    LOCK_SHA256) case $v in *[!0-9a-f]*) fail "LOCK_SHA256 must be lowercase hex" ;; esac
                 [ ${#v} -eq 64 ] || fail "LOCK_SHA256 must be 64 hex digits" ;;
  esac
  eval "val_$k=\$v"
done

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
  v=$(get "$n")
  [ -n "$v" ] || fail "the placeholder @$n@ has no value (give $n=...)"
  sed "s|@$n@|$v|g" "$tmp" > "$tmp.next" && mv "$tmp.next" "$tmp"
done
if grep -q '@[A-Z_][A-Z0-9_]*@' "$tmp"; then fail "a placeholder is left in the output"; fi
mv "$tmp" "$out"
chmod 0644 "$out"
printf 'render-contract: wrote %s (%s)\n' "$out" "$(printf '%s' "$names" | tr '\n' ' ')"
