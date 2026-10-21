#!/bin/sh
set -eu
: "${PRIVY_APP_ID:?PRIVY_APP_ID is required}"
: "${PRIVY_APP_SECRET:?PRIVY_APP_SECRET is required}"
export ATLAS_BALANCE_BIND="0.0.0.0:${PORT:-10000}"
export PRIVY_BRIDGE_URL="http://127.0.0.1:3101"
# The bridge's JavaScript heap is capped well inside the server's 512 MB, so it clears its garbage
# before the engine runs short; if it ever stops (a crash, or past that cap) it starts again on its
# own while the engine keeps answering.
(
  while :; do
    node --max-old-space-size=192 /app/privy-bridge/server.mjs || true
    echo "Privy bridge stopped; starting it again" >&2
    sleep 1
  done
) &
bridge_pid=$!
# Start answering only once the bridge is listening: on a cold start its SDKs take a few seconds to
# load, and every sign-in check goes through it.
tries=0
until node -e "require('net').connect(3101,'127.0.0.1').on('connect',()=>process.exit(0)).on('error',()=>process.exit(1))"; do
  if ! kill -0 "$bridge_pid" 2>/dev/null; then
    echo "Privy verification bridge failed to start" >&2
    exit 1
  fi
  tries=$((tries + 1))
  if [ "$tries" -ge 30 ]; then
    echo "Privy verification bridge did not start listening" >&2
    exit 1
  fi
  sleep 1
done
exec /usr/local/bin/engine-service
