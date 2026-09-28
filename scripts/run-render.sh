#!/bin/sh
set -eu
: "${PRIVY_APP_ID:?PRIVY_APP_ID is required}"
: "${PRIVY_APP_SECRET:?PRIVY_APP_SECRET is required}"
export ATLAS_BALANCE_BIND="0.0.0.0:${PORT:-10000}"
export PRIVY_BRIDGE_URL="http://127.0.0.1:3101"
node /app/privy-bridge/server.mjs &
bridge_pid=$!
sleep 1
if ! kill -0 "$bridge_pid" 2>/dev/null; then
  echo "Privy verification bridge failed to start" >&2
  exit 1
fi
exec /usr/local/bin/engine-service
