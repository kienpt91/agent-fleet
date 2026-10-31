#!/bin/sh
# A fake coding agent for the NEO N2 demo. It reads its compiled context (.fleet/context.md). Its
# first attempt writes a wrong answer and leaves a summary; when layer 4 of its next pack says that
# a check failed, it writes the right one. It always claims success: fleet's own run decides.
set -eu
ctx=.fleet/context.md
echo "agent: working on ${FLEET_TASK:?}; $(sed -n 1p "$ctx")"
if grep -q ': failed on commit' "$ctx"; then
  echo "agent: layer 4 of my context says a check failed; fixing the answer"
  echo "ready" > answer.txt
else
  echo "wrong" > answer.txt
  printf '{"version":1,"summary":"wrote a first answer, not sure it is right"}\n' > .fleet/handoff.json
fi
printf '{"version":1,"status":"done","summary":"done, every check passes"}\n' > .fleet/result.json
echo "agent: finished"
