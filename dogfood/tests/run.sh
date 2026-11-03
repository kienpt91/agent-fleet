#!/bin/sh
# Runs every dogfood shell test and prints a total. Needs git and a POSIX sh; run it in the
# development container: DRUST_NET=none drust <repo> 'sh dogfood/tests/run.sh'
cd "$(dirname "$0")" || exit 2
status=0
total_pass=0
for t in *.test.sh; do
  out=$(sh "./$t" 2>&1) || status=1
  printf '%s\n' "$out"
  n=$(printf '%s\n' "$out" | sed -n 's/^.*: \([0-9][0-9]*\) passed, .*/\1/p' | tail -n 1)
  total_pass=$((total_pass + ${n:-0}))
done
printf 'dogfood shell tests: %d checks passed%s\n' "$total_pass" "$([ "$status" -eq 0 ] || printf ', SOME FAILED')"
exit "$status"
