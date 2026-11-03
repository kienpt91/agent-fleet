#!/bin/sh
# Copies the check scripts and the protected list from a REVIEWED commit of the repository to a
# directory outside the repository. Verification contracts run these copies (a contract program
# may not live inside the repository, the worktree or the export), so a task can neither edit
# the checker it is judged by nor have it replaced by what is in its own branch.
#
#   install-tools.sh --repo <repo> --ref <ref> --dest <dir>
#
# --ref is the commit you reviewed (a hash, or the branch you merge human-reviewed work into).
# The files are read from git objects, never from a working tree. <dir> must be an absolute path
# outside the repository, owned by you and not writable by group or others; it is created (0755)
# when missing. Prints the SHA-256 of each file so you can record what you installed.
# Run it again after every merged change to dogfood/ (and compare the hashes).
set -u
set -f

fail() { printf 'install-tools: %s\n' "$1" >&2; exit 2; }

repo=; ref=; dest=
while [ $# -gt 0 ]; do
  case $1 in
    --repo) [ $# -ge 2 ] || fail "--repo needs a value"; repo=$2; shift 2 ;;
    --ref) [ $# -ge 2 ] || fail "--ref needs a value"; ref=$2; shift 2 ;;
    --dest) [ $# -ge 2 ] || fail "--dest needs a value"; dest=$2; shift 2 ;;
    *) fail "unknown argument $1" ;;
  esac
done
[ -n "$repo" ] && [ -n "$ref" ] && [ -n "$dest" ] || fail "usage: install-tools.sh --repo <repo> --ref <ref> --dest <dir>"
case $ref in -*|*[!A-Za-z0-9._/@^~-]*) fail "the ref has characters a ref does not" ;; esac
case $dest in /*) ;; *) fail "--dest must be an absolute path" ;; esac
case "/${dest#/}/" in */../*|*/./*|*//*) fail "--dest must not contain .. or . segments" ;; esac
[ -d "$repo" ] || fail "not a directory: $repo"
repo_real=$(CDPATH= cd -- "$repo" && pwd -P) || fail "cannot enter $repo"

[ ! -L "$dest" ] || fail "$dest is a symbolic link"
if [ ! -e "$dest" ]; then
  parent=$(dirname -- "$dest")
  [ -d "$parent" ] || fail "the parent of $dest does not exist"
  mkdir -m 0755 "$dest" || fail "cannot create $dest"
fi
[ -d "$dest" ] || fail "$dest is not a directory"
dest_real=$(CDPATH= cd -- "$dest" && pwd -P) || fail "cannot enter $dest"
case $dest_real/ in
  "$repo_real"/*) fail "$dest is inside the repository; it must be outside it" ;;
esac
case $repo_real/ in
  "$dest_real"/*) fail "the repository is inside $dest" ;;
esac
[ -O "$dest_real" ] || fail "$dest is not owned by you"
if [ -n "$(find "$dest_real" -maxdepth 0 \( -perm -020 -o -perm -002 \) -print)" ]; then
  fail "$dest is writable by group or others"
fi

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  else fail "neither sha256sum nor shasum is available"; fi
}

commit=$(GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_NOSYSTEM=1 git -C "$repo_real" rev-parse --verify --quiet "$ref^{commit}") \
  || fail "$ref is not a commit of $repo"
printf 'install-tools: reading dogfood/ at %s\n' "$commit"

install_one() { # name mode
  tmp=$(mktemp "$dest_real/.install.XXXXXX") || fail "cannot create a file in $dest_real"
  if ! GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_NOSYSTEM=1 \
       git -C "$repo_real" cat-file blob "$commit:dogfood/$1" > "$tmp" 2>/dev/null; then
    rm -f "$tmp"; fail "dogfood/$1 does not exist at $commit"
  fi
  [ -s "$tmp" ] || { rm -f "$tmp"; fail "dogfood/$1 is empty at $commit"; }
  chmod "$2" "$tmp" && mv -f "$tmp" "$dest_real/$1" || { rm -f "$tmp"; fail "cannot install $1"; }
  printf '  %s  %s\n' "$(sha256_of "$dest_real/$1")" "$1"
}
install_one check-protected.sh 0555
install_one check-docs-claims.sh 0555
install_one check-secret-tree.sh 0555
install_one check-conflict-markers.sh 0555
install_one check-test-only.sh 0555
install_one protected-paths.json 0444
printf 'install-tools: installed into %s\n' "$dest_real"
