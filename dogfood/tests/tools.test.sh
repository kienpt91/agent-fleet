#!/bin/sh
# Tests of render-contract.sh and install-tools.sh.
. "$(dirname "$0")/lib.sh"
rc="$dogfood/render-contract.sh"
it="$dogfood/install-tools.sh"
sha() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }

tpl=$dogfood/contracts/single-file.json
repo=$(new_repo rc-repo src/a.rs)
printf '# lock\n' > "$repo/Cargo.lock"; commit_all "$repo" lock
lock=$(sha "$repo/Cargo.lock")
base=$(git -C "$repo" rev-parse HEAD)
good="TOOLS=/opt/dogfood/bin CONTAINER_RUN=/opt/dogfood/container-run REPO=$repo BASE=$base RUN=neo-roadmap TASK=starter-x FILE_PATH=docs/domain.md"

# ---- rendering ---------------------------------------------------------------------------------------
# shellcheck disable=SC2086
expect_exit 0 "a template renders" "$rc" "$tpl" "$work/out.json" $good
expect_out "and says what it filled in" "wrote $work/out.json"
if grep -q '@[A-Z_]*@' "$work/out.json"; then bad "a placeholder is left"; else ok; fi
grep -q '"program": "/opt/dogfood/bin/check-protected.sh"' "$work/out.json" && ok || bad "the tools directory was not substituted"
grep -q "\"args\": \[\"--repo\", \"$repo\", \"$base..fleet/neo-roadmap/starter-x\"\]" "$work/out.json" && ok || bad "the range of the task branch was not substituted"
grep -q '"allow": \["docs/domain.md"\]' "$work/out.json" && ok || bad "the file path was not substituted"
grep -q "\"value\": \"$lock\"" "$work/out.json" && ok || bad "the sha256 of Cargo.lock at the base was not derived"
grep -q '"program": "/opt/dogfood/container-run"' "$work/out.json" && ok || bad "the container wrapper was not substituted"
[ "$(wc -c < "$work/out.json")" -lt 32768 ] && ok || bad "a rendered contract must stay under the 32 KiB cap"

# the lock hash comes from the base commit, not from the working tree
printf '# changed later\n' > "$repo/Cargo.lock"
# shellcheck disable=SC2086
"$rc" "$tpl" "$work/out2.json" $good >/dev/null
grep -q "\"value\": \"$lock\"" "$work/out2.json" && ok || bad "the lock hash followed the working tree"
# an explicit hash is taken as given
# shellcheck disable=SC2086
"$rc" "$tpl" "$work/out3.json" $good LOCK_SHA256=$(printf 'a%.0s' $(seq 1 64)) >/dev/null
grep -q '"value": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"' "$work/out3.json" && ok || bad "an explicit hash was ignored"

# ---- refusals ----------------------------------------------------------------------------------------
without() { # NAME: $good minus that setting
  printf '%s\n' "$good" | tr ' ' '\n' | grep -v "^$1=" | tr '\n' ' '
}
# shellcheck disable=SC2046
expect_exit 2 "a placeholder without a value" "$rc" "$tpl" "$work/x.json" $(without TASK)
# shellcheck disable=SC2086
expect_exit 2 "a value no template uses (a typo must not pass)" "$rc" "$tpl" "$work/x.json" $good CRATE=neo-x
for bad_arg in 'FILE_PATH=a b' 'FILE_PATH=a"b' 'FILE_PATH=a|b' 'FILE_PATH=a\b' 'FILE_PATH=../etc/passwd' 'FILE_PATH=/etc/passwd' 'FILE_PATH=a/../b' 'FILE_PATH=a//b' 'TOOLS=relative/bin' 'TOOLS=/opt/../bin' 'CONTAINER_RUN=run' 'REPO=repo' 'RUN=Neo' 'RUN=-x' 'RUN=a.b' 'TASK=a/b' 'TASK=' 'BASE=-x' 'BASE=a;b' 'BASE=a b' 'LOCK_SHA256=xyz' 'LOCK_SHA256=ABCDEF'; do
  # shellcheck disable=SC2046
  expect_exit 2 "refused: $bad_arg" "$rc" "$tpl" "$work/x.json" $(without "${bad_arg%%=*}") "$bad_arg"
done
expect_exit 2 "an argument that is not NAME=VALUE" "$rc" "$tpl" "$work/x.json" nope
expect_exit 2 "a lowercase name" "$rc" "$tpl" "$work/x.json" tools=/x
expect_exit 2 "a missing template" "$rc" "$work/none.json" "$work/x.json"
printf '{"version": 1, "checks": []}\n' > "$work/plain.json"
expect_exit 2 "a value for a template that has no placeholder" "$rc" "$work/plain.json" "$work/x.json" FILE_PATH=a
mkdir "$work/adir"
# shellcheck disable=SC2086
expect_exit 2 "an output that is a directory" "$rc" "$tpl" "$work/adir" $good
# shellcheck disable=SC2046
expect_exit 2 "LOCK_SHA256 cannot be worked out without a base that has Cargo.lock" "$rc" "$tpl" "$work/x.json" $(without BASE) BASE=0000000000000000000000000000000000000000
[ ! -e "$work/x.json" ] && ok || bad "a failed render left an output"
# nothing from the value can add JSON: every value is made of safe characters only
# shellcheck disable=SC2086
expect_exit 0 "the human-only contract has no placeholder and is copied as it is" "$rc" "$dogfood/contracts/human-only.json" "$work/h.json"
cmp -s "$dogfood/contracts/human-only.json" "$work/h.json" && ok || bad "the copy differs"

# ---- the flaky-test template (a reviewed exception) -----------------------------------------------------------
ftpl=$dogfood/contracts/flaky-test.json
fgood="TOOLS=/opt/dogfood/bin CONTAINER_RUN=/opt/dogfood/container-run REPO=$repo BASE=$base RUN=neo-roadmap TASK=flaky-x FILE_PATH=crates/fleet-infra/src/server.rs PACKAGE=fleet-infra TEST_NAME=a_reply_is_delivered TEST_FILTER=server::tests::a_reply_is_delivered"
# shellcheck disable=SC2086
expect_exit 0 "the flaky-test template renders" "$rc" "$ftpl" "$work/f.json" $fgood
grep -q '"args": \["--repo", "'"$repo"'", "--except", "crates/fleet-infra/src/server.rs", "'"$base"'..fleet/neo-roadmap/flaky-x"\]' "$work/f.json" && ok || bad "the exception was not named in the protected script check"
grep -q '"value": "fn a_reply_is_delivered"' "$work/f.json" && ok || bad "the kept check does not name the test"
grep -q 'cargo test -p fleet-infra --lib server::tests::a_reply_is_delivered --offline --locked -q' "$work/f.json" && ok || bad "the stress check does not run the test"
grep -q '"id": "protected-diff"' "$work/f.json" && bad "the exception template still has the glob deny check" || ok
grep -q '"id": "test-only"' "$work/f.json" && ok || bad "no test-only check"
for bad_arg in 'FILE_PATH=crates/fleet-infra/Cargo.toml' 'FILE_PATH=crates/fleet-infra/build.rs' 'FILE_PATH=crates/fleet-cli/tests/architecture.rs' 'FILE_PATH=SECURITY.md' 'FILE_PATH=dogfood/check-protected.sh' 'FILE_PATH=crates/a/src/x.md' 'TEST_FILTER=a b' 'TEST_FILTER=a;b' 'TEST_FILTER=Server::t' 'TEST_FILTER=-x' 'TEST_NAME=A' 'TEST_NAME=a:b' 'PACKAGE=Fleet' 'PACKAGE=-x' 'PACKAGE=a/b'; do
  key=${bad_arg%%=*}
  # shellcheck disable=SC2046
  expect_exit 2 "flaky-test: refused $bad_arg" "$rc" "$ftpl" "$work/x.json" $(printf '%s\n' "$fgood" | tr ' ' '\n' | grep -v "^$key=" | tr '\n' ' ') "$bad_arg"
done
# shellcheck disable=SC2086
expect_exit 2 "a template without TEST_NAME takes no PACKAGE" "$rc" "$tpl" "$work/x.json" $good PACKAGE=fleet-infra

# ---- install-tools -----------------------------------------------------------------------------------
src=$work/src-repo
mkdir -p "$src/dogfood"
git -C "$src" init -q -b main
for f in check-protected.sh check-docs-claims.sh check-secret-tree.sh check-conflict-markers.sh check-test-only.sh protected-paths.json; do cp "$dogfood/$f" "$src/dogfood/$f"; done
git -C "$src" add -A; git -C "$src" commit -q -m tools
echo '# edited but not committed' >> "$src/dogfood/check-protected.sh"
dest=$work/tools
expect_exit 0 "install from a commit" "$it" --repo "$src" --ref main --dest "$dest"
expect_out "it names the commit it read" "reading dogfood/ at $(git -C "$src" rev-parse main)"
for f in check-protected.sh check-docs-claims.sh check-secret-tree.sh check-conflict-markers.sh check-test-only.sh protected-paths.json; do
  committed=$(git -C "$src" cat-file blob "main:dogfood/$f" | { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi; } | cut -d' ' -f1)
  expect_eq "$f is the committed file" "$committed" "$(sha "$dest/$f")"
  expect_out "its hash is printed" "$committed"
done
[ ! -w "$dest/check-protected.sh" ] && ok || bad "the installed script is writable"
[ -x "$dest/check-protected.sh" ] && ok || bad "the installed script is not executable"
[ ! -x "$dest/protected-paths.json" ] && ok || bad "the list is executable"
expect_exit 0 "installing again over the first install" "$it" --repo "$src" --ref main --dest "$dest"
expect_exit 2 "a destination inside the repository" "$it" --repo "$src" --ref main --dest "$src/inside"
expect_exit 2 "a destination that is the repository" "$it" --repo "$src" --ref main --dest "$src"
expect_exit 2 "a relative destination" "$it" --repo "$src" --ref main --dest tools
expect_exit 2 "a destination with .." "$it" --repo "$src" --ref main --dest "$work/a/../tools"
mkdir "$work/loose"; chmod 0777 "$work/loose"
expect_exit 2 "a destination everybody can write" "$it" --repo "$src" --ref main --dest "$work/loose"
mkdir "$work/grp"; chmod 0775 "$work/grp"
expect_exit 2 "a destination the group can write" "$it" --repo "$src" --ref main --dest "$work/grp"
ln -s "$dest" "$work/link"
expect_exit 2 "a destination that is a symbolic link" "$it" --repo "$src" --ref main --dest "$work/link"
expect_exit 2 "an unknown ref" "$it" --repo "$src" --ref nonexistent --dest "$work/t2"
expect_exit 2 "a ref that is an option" "$it" --repo "$src" --ref --output=x --dest "$work/t2"
expect_exit 2 "a missing parent directory" "$it" --repo "$src" --ref main --dest "$work/no/such/dir"
expect_exit 2 "missing arguments" "$it" --repo "$src"
git -C "$src" rm -q dogfood/check-docs-claims.sh; git -C "$src" commit -q -m drop
expect_exit 2 "a file that is missing at the ref" "$it" --repo "$src" --ref main --dest "$work/t3"
expect_exit 0 "but present at the earlier commit" "$it" --repo "$src" --ref main~1 --dest "$work/t4"

finish
