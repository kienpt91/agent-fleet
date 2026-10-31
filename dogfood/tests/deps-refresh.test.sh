#!/bin/sh
# Tests of deps-refresh.sh with stand-ins for the container wrapper and for the age check.
. "$(dirname "$0")/lib.sh"
dr="$dogfood/deps-refresh.sh"

mkrepo() {
  r=$(new_repo "$1" src/a.rs)
  printf '# lock v1\n' > "$r/Cargo.lock"; commit_all "$r" lock
  printf '%s\n' "$r"
}
# a wrapper that "updates" the lock and records the network setting it was started with
cat > "$work/crun-ok" <<STUB
#!/bin/sh
echo "net=\${DOGFOOD_NET:-unset} cmd=\$1 cwd=\$(pwd)" >> "$work/crun.log"
echo '# lock v2' >> Cargo.lock
STUB
cat > "$work/crun-same" <<STUB
#!/bin/sh
exit 0
STUB
cat > "$work/crun-fail" <<STUB
#!/bin/sh
echo '# half written' >> Cargo.lock
exit 1
STUB
# node stand-ins: print what the real check prints
cat > "$work/node-clean" <<'STUB'
#!/bin/sh
echo "$2: 121 locked crates, cutoff $3, too new: 0"
exit 0
STUB
cat > "$work/node-new" <<'STUB'
#!/bin/sh
echo "$2: 121 locked crates, cutoff $3, too new: 2"
echo "  foo@1.2.3  2026-10-30T00:00:00Z"
exit 1
STUB
cat > "$work/node-garbage" <<'STUB'
#!/bin/sh
echo "network is down"
exit 0
STUB
cat > "$work/node-odd" <<'STUB'
#!/bin/sh
echo "$2: 121 locked crates, cutoff $3, too new: 0"
exit 3
STUB
chmod +x "$work"/crun-* "$work"/node-*
touch "$work/age.mjs"
run() { # repo wrapper node [extra]
  r=$1; c=$2; n=$3; shift 3
  "$dr" --repo "$r" --container-run "$work/$c" --node "$work/$n" --age-check "$work/age.mjs" "$@"
}

r=$(mkrepo ok)
expect_exit 0 "a clean refresh" run "$r" crun-ok node-clean --cutoff 2026-10-24T00:00:00Z
expect_out "the cutoff is passed on" "cutoff 2026-10-24T00:00:00Z"
expect_out "it tells you to commit yourself" "commit Cargo.lock yourself"
grep -q 'lock v2' "$r/Cargo.lock" && ok || bad "the new lock was not left in place"
[ -n "$(git -C "$r" status --porcelain)" ] && ok || bad "the refresh must leave the change uncommitted"
grep -q "net=bridge cmd=cargo update cwd=$r" "$work/crun.log" && ok || bad "cargo update did not get the network (or ran elsewhere): $(cat "$work/crun.log")"
[ "$(git -C "$r" rev-list --count HEAD)" -eq 2 ] && ok || bad "the script committed"

r=$(mkrepo toonew)
expect_exit 1 "a version that is too new" run "$r" crun-ok node-new --cutoff 2026-10-24T00:00:00Z
expect_out "is reported" "2 version(s) are newer"
expect_eq "and the lock is put back" "" "$(git -C "$r" status --porcelain)"

r=$(mkrepo garbage)
expect_exit 2 "an age check that says nothing clear is not a pass" run "$r" crun-ok node-garbage --cutoff 2026-10-24T00:00:00Z
expect_eq "the lock is put back" "" "$(git -C "$r" status --porcelain)"
r=$(mkrepo odd)
expect_exit 2 "an age check that exits non-zero without a finding is not a pass" run "$r" crun-ok node-odd --cutoff 2026-10-24T00:00:00Z
expect_eq "the lock is put back too" "" "$(git -C "$r" status --porcelain)"
r=$(mkrepo failing)
expect_exit 2 "a failing update" run "$r" crun-fail node-clean --cutoff 2026-10-24T00:00:00Z
expect_eq "puts the lock back" "" "$(git -C "$r" status --porcelain)"
r=$(mkrepo same)
expect_exit 0 "nothing to refresh" run "$r" crun-same node-clean --cutoff 2026-10-24T00:00:00Z
expect_out "is said" "nothing to refresh"
r=$(mkrepo dirty)
echo x >> "$r/src/a.rs"
expect_exit 2 "uncommitted changes" run "$r" crun-ok node-clean --cutoff 2026-10-24T00:00:00Z
expect_out "are named as the reason" "uncommitted changes"
r=$(mkrepo cutoffs)
expect_exit 2 "a cutoff that is not a timestamp" run "$r" crun-ok node-clean --cutoff yesterday
expect_exit 0 "a default cutoff (now minus 7 days)" run "$r" crun-ok node-clean
expect_out "is a timestamp" "cutoff 20"
expect_exit 2 "a relative wrapper path" "$dr" --repo "$r" --container-run crun --node "$work/node-clean" --age-check "$work/age.mjs"
expect_exit 2 "a missing wrapper" "$dr" --repo "$r" --container-run "$work/none" --node "$work/node-clean" --age-check "$work/age.mjs"
expect_exit 2 "not a repository" "$dr" --repo "$work" --container-run "$work/crun-ok" --node "$work/node-clean" --age-check "$work/age.mjs"
expect_exit 2 "an unknown argument" "$dr" --frob 1

finish
