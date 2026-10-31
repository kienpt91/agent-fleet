#!/bin/sh
# A scripted stand-in for an agent that talks for a while: one line per second, so a live view of the
# output (the terminal dashboard, `fleet tail --follow`, the web dashboard's event stream) has something
# to follow. Used by scripts/web-demo.sh.
set -eu
echo "slow agent: starting ${FLEET_TASK:-task}"
i=1
while [ "$i" -le "${SLOW_AGENT_SECS:-30}" ]; do
  echo "tick $i"
  i=$((i + 1))
  sleep 1
done
printf '{"version":1,"status":"done","summary":"talked for a while"}\n' > .fleet/result.json
echo "slow agent: finished"
