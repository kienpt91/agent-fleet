#!/bin/sh
# Tests of container-run.example.sh and reset-cache.sh with a stand-in for docker that records its calls: what
# each mode mounts, which network it uses and that the scratch target is created and removed around a check.
. "$(dirname "$0")/lib.sh"
crun_src="$dogfood/container-run.example.sh"
rc="$dogfood/reset-cache.sh"

log="$work/docker.log"
cat > "$work/docker" <<STUB
#!/bin/sh
echo "\$*" >> "$log"
case \$1 in
  volume) case \$2 in ls) [ -z "\${FAKE_VOLUMES:-}" ] || printf '%s\n' \$FAKE_VOLUMES ;; rm) case \$3 in *busy*) exit 1 ;; esac ;; esac ;;
  run) [ -z "\${FAKE_RUN_EXIT:-}" ] || exit "\$FAKE_RUN_EXIT" ;;
esac
exit 0
STUB
chmod 755 "$work/docker"
# the wrapper as an owner would edit it: the stand-in for the docker client
sed "s#^docker=.*#docker=$work/docker#" "$crun_src" > "$work/crun"
chmod 755 "$work/crun"
mkdir -p "$work/export"

calls() { cat "$log" 2>/dev/null; }
run_line() { grep '^run ' "$log" | grep -- '-w /work' | head -n 1; }

# ---- the checks' mode: a fresh scratch target ----------------------------------------------------------------
: > "$log"
( cd "$work/export" && expect_exit 0 "a check runs" "$work/crun" 'cargo test' )
expect_eq "one scratch volume is created" 1 "$(grep -c '^volume create fleet-dogfood-scratch-' "$log")"
expect_eq "and removed afterwards" 1 "$(grep -c '^volume rm -f fleet-dogfood-scratch-' "$log")"
line=$(run_line)
case $line in *"--network none"*) ok ;; *) bad "a check has no network" "$line" ;; esac
case $line in *"--cap-drop ALL"*) ok ;; *) bad "a check drops every capability" "$line" ;; esac
case $line in *"export:/work:ro"*) ok ;; *) bad "the export is read-only" "$line" ;; esac
case $line in *"fleet-dogfood-cargo:/cargo:ro"*) ok ;; *) bad "the cargo registry is read-only" "$line" ;; esac
case $line in *"fleet-dogfood-target:/warm-target:ro"*) ok ;; *) bad "the warm target is read-only" "$line" ;; esac
case $line in *"fleet-dogfood-target:/target"*) bad "the warm target must never be the writable target of a check" "$line" ;; *) ok ;; esac
case $line in *"fleet-dogfood-scratch-"*":/target"*) ok ;; *) bad "the target of a check is its own scratch volume" "$line" ;; esac
case $line in *"cp -a /warm-target/. /target/"*) ok ;; *) bad "the scratch target is seeded from the warm one" "$line" ;; esac
# the volume is handed to the check's user with one capability and nothing else
chown_line=$(grep '^run ' "$log" | grep chown | head -n 1)
case $chown_line in *"--cap-drop ALL --cap-add CHOWN"*"--network none"*|*"--network none"*"--cap-drop ALL --cap-add CHOWN"*) ok ;; *) bad "the setup container is as small as it can be" "$chown_line" ;; esac

# a failing check leaves no volume behind and its status is the wrapper's
: > "$log"
( cd "$work/export" && FAKE_RUN_EXIT=7 expect_exit 7 "the status of the check is the wrapper's" "$work/crun" 'false' )
expect_eq "a failed check's scratch volume is removed too" 1 "$(grep -c '^volume rm -f fleet-dogfood-scratch-' "$log")"

# two checks never share a volume
: > "$log"
( cd "$work/export" && "$work/crun" a && sleep 1 && "$work/crun" b ) >/dev/null 2>&1
n=$(grep '^volume create' "$log" | sort -u | wc -l | tr -d ' ')
expect_eq "every check has a volume of its own" 2 "$n"

# ---- the operator's modes ---------------------------------------------------------------------------------------
: > "$log"
( cd "$work/export" && DOGFOOD_MODE=warm expect_exit 0 "warm mode" "$work/crun" 'cargo build' )
line=$(run_line)
case $line in *"--network none"*) ok ;; *) bad "warming needs no network" "$line" ;; esac
case $line in *"fleet-dogfood-target:/target"*) ok ;; *) bad "warm mode writes the warm target" "$line" ;; esac
case $line in *"export:/work:ro"*) ok ;; *) bad "warm mode keeps the tree read-only" "$line" ;; esac
expect_eq "warm mode creates no scratch volume" 0 "$(grep -c '^volume create' "$log")"

: > "$log"
( cd "$work/export" && DOGFOOD_MODE=online expect_exit 0 "online mode" "$work/crun" 'cargo fetch --locked' )
line=$(run_line)
case $line in *"--network bridge"*) ok ;; *) bad "online mode has the network" "$line" ;; esac
case $line in *"fleet-dogfood-cargo:/cargo "*|*"fleet-dogfood-cargo:/cargo") ok ;; *) bad "online mode fills the registry" "$line" ;; esac
( cd "$work/export" && DOGFOOD_MODE=bogus expect_exit 2 "an unknown mode" "$work/crun" 'x' )
( cd "$work/export" && expect_exit 2 "no command" "$work/crun" )

# ---- reset-cache.sh ----------------------------------------------------------------------------------------------
: > "$log"
FAKE_VOLUMES="fleet-dogfood-scratch-1-2 fleet-dogfood-scratch-busy-3 other-volume fleet-dogfood-target" \
  expect_exit 0 "removing leftover scratch volumes" "$rc" --scratch --docker "$work/docker"
expect_out "it says what it removed" "removed 1 scratch volume(s), left 1"
expect_out "and what it left" "fleet-dogfood-scratch-busy-3 is in use"
expect_eq "only scratch volumes were asked to be removed" 2 "$(grep -c '^volume rm ' "$log")"
expect_eq "never the warm one or another" 0 "$(grep '^volume rm ' "$log" | grep -c 'target\|other')"
: > "$log"
expect_exit 0 "emptying the warm target" "$rc" --warm --docker "$work/docker"
expect_eq "the volume is removed and made again" "volume rm fleet-dogfood-target
volume create fleet-dogfood-target" "$(grep '^volume' "$log")"
expect_eq "and handed to the user" 1 "$(grep -c 'run --rm -u 0 --network none --cap-drop ALL --cap-add CHOWN' "$log")"
expect_exit 2 "no action" "$rc"
expect_exit 2 "both actions" "$rc" --scratch --warm
expect_exit 2 "an odd prefix" "$rc" --scratch --prefix 'a b'
expect_exit 2 "an odd volume" "$rc" --warm --volume 'x;y'

finish
