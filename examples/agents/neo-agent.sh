#!/bin/sh
# A fake coding agent for the NEO demo. It always reports success. Its first attempt writes the
# wrong answer; when the brief of a retry quotes a failed verification, it writes the right one.
set -eu
brief="${FLEET_BRIEF_FILE:?FLEET_BRIEF_FILE is set by fleet}"
echo "agent: working on ${FLEET_TASK:?}"
if grep -q "verify feature/answer" "$brief"; then
  echo "agent: the verification complained, fixing the answer"
  echo "ready" > answer.txt
else
  echo "wrong" > answer.txt
fi
printf '{"version":1,"status":"done","summary":"done, every check passes"}\n' > .fleet/result.json
echo "agent: finished"
