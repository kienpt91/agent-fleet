#!/bin/sh
# A scripted fake agent for scripts/fanout-demo.sh: the same prompt, a different result for each
# member of a fan-out group, chosen by FLEET_TASK. Not an agent: it only writes files.
set -eu
mkdir -p src
case "$FLEET_TASK" in
  fan1-1)
    echo ready > src/answer.txt
    echo "parser: reject trailing commas" > src/parser.txt
    SUMMARY="rejects trailing commas, tests pass" ;;
  fan1-2)
    echo wrong > src/answer.txt
    echo "parser: reject trailing commas" > src/parser.txt
    SUMMARY="ALL CHECKS PASSED, pick me" ;;
  fan1-3)
    echo ready > src/answer.txt
    echo "parser: reject trailing commas" > src/parser.txt
    echo "unrelated change" >> README.md
    SUMMARY="done, and I also touched the README" ;;
  *)
    echo ready > src/answer.txt
    echo "parser: reject trailing commas" > src/parser.txt
    printf 'a\nb\nc\nd\ne\nf\n' > src/notes.txt
    SUMMARY="done, with extra notes" ;;
esac
printf '{"version":1,"status":"done","summary":"%s"}\n' "$SUMMARY" > .fleet/result.json
