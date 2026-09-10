#!/bin/bash
# End-to-end smoke test for the local tunnel API daemon (skunkworks slice).
#
# Run from inside WSL (or any Linux env with rustc/cargo/go on PATH):
#   bash connect/scripts/e2e-smoke-test.sh
#
# What it does: builds datum-connect-daemon + the datumctl-connect plugin,
# starts the daemon, creates a tunnel to a throwaway local HTTP server,
# starts it, waits for the setup pipeline to finish (including DNS
# publication), curls the public hostname to prove traffic actually flows,
# then tears everything down.
#
# Auth: reuses whatever Datum Cloud session `datumctl` already has active
# via DATUM_CREDENTIALS_HELPER — no separate login needed. Defaults to a
# native Linux datumctl (a real Datum Cloud service account, logged in via
# `datumctl login --credentials <sa-creds.json> --hostname auth.datum.net`
# — see NOTES.md). Override the vars below if your setup differs.
set -uo pipefail
source "$HOME/.cargo/env" 2>/dev/null || true

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONNECT_LIB="$REPO_ROOT/connect-lib"
CONNECT_PLUGIN="$REPO_ROOT/connect-plugin"
DAEMON_BIN="$CONNECT_LIB/target/debug/datum-connect-daemon"
CLI="$CONNECT_PLUGIN/datumctl-connect"
PORT="${DATUM_TUNNEL_DAEMON_PORT:-47780}"
LOCAL_PORT="${SMOKE_TEST_LOCAL_PORT:-8765}"

: "${DATUM_SESSION:?Set DATUM_SESSION to your datumctl session id, e.g. user@example.com@api.datum.net (see ~/.datumctl/config on the box datumctl is logged in on)}"
: "${DATUM_CREDENTIALS_HELPER:=$HOME/.local/bin/datumctl}"
: "${DATUM_PROJECT:?Set DATUM_PROJECT to the test project id}"
export DATUM_PLUGIN_MODE=1
export DATUM_CREDENTIALS_HELPER
export DATUM_CONNECT_DIR="${DATUM_CONNECT_DIR:-$HOME/.datumctl/connect}"

echo "=== building daemon + CLI ==="
(cd "$CONNECT_LIB" && cargo build -p datum-connect-daemon)
(cd "$CONNECT_PLUGIN" && go build -o "$CLI" .)

echo "=== starting a local test server on :$LOCAL_PORT ==="
python3 -m http.server "$LOCAL_PORT" --directory /tmp >/tmp/pyserver.log 2>&1 &
PYPID=$!
sleep 1

echo "=== starting the tunnel daemon on :$PORT ==="
RUST_LOG=info "$DAEMON_BIN" --port "$PORT" >/tmp/daemon.log 2>&1 &
DAEMONPID=$!
for i in $(seq 1 20); do
  curl -s -o /dev/null "http://127.0.0.1:$PORT/v1/tunnels" && { echo "daemon up after ${i}s"; break; }
  sleep 1
done

cleanup() {
  [ -n "${TUNNEL_ID:-}" ] && "$CLI" --port "$PORT" tunnel api stop "$TUNNEL_ID" >/dev/null 2>&1
  [ -n "${TUNNEL_ID:-}" ] && "$CLI" --port "$PORT" tunnel api delete "$TUNNEL_ID" >/dev/null 2>&1
  kill "$DAEMONPID" "$PYPID" 2>/dev/null
}
trap cleanup EXIT

echo "=== create + start ==="
CREATE_OUT=$("$CLI" --port "$PORT" tunnel api create --label smoke-test --endpoint "127.0.0.1:$LOCAL_PORT")
TUNNEL_ID=$(echo "$CREATE_OUT" | grep -o '"id": *"[^"]*"' | head -1 | sed 's/.*"\([^"]*\)"$/\1/')
[ -z "$TUNNEL_ID" ] && { echo "FAILED to create tunnel:"; echo "$CREATE_OUT"; cat /tmp/daemon.log; exit 1; }
echo "tunnel id: $TUNNEL_ID"
"$CLI" --port "$PORT" tunnel api start "$TUNNEL_ID" >/dev/null

echo "=== waiting for setup pipeline (up to ~3min) ==="
HOSTNAME=""
for i in $(seq 1 60); do
  PROG=$("$CLI" --port "$PORT" tunnel api progress "$TUNNEL_ID" 2>&1 | tr -d '\n')
  HOSTNAME=$(echo "$PROG" | grep -o '"hostnames": *\[[^]]*\]' | grep -o '"[a-zA-Z0-9.-]*\.[a-zA-Z]*"' | head -1 | tr -d '"')
  # connector_metadata_programmed can legitimately stay "unknown" in
  # extension-server mode (see ProgressStepKind::ConnectorMetadataProgrammed
  # / TunnelProgress::all_ready in connect-lib) — don't count that as blocking.
  NOT_READY=$(echo "$PROG" | grep -o '"kind": *"[a-z_]*"[^}]*"status": *"[a-z]*"' \
    | grep -v 'connector_metadata_programmed' | grep -vc '"status": *"ready"')
  echo "[$i] not-ready=$NOT_READY hostname=$HOSTNAME"
  [ "$NOT_READY" = "0" ] && [ -n "$HOSTNAME" ] && { echo "READY"; break; }
  sleep 3
done

[ -z "$HOSTNAME" ] && { echo "FAILED: never got a hostname"; "$CLI" --port "$PORT" tunnel api progress "$TUNNEL_ID"; exit 1; }

echo "=== curling https://$HOSTNAME ==="
for i in $(seq 1 15); do
  CODE=$(curl -s -o /tmp/smoke_curl_body.txt -w "%{http_code}" "https://$HOSTNAME" --max-time 10)
  echo "[$i] http_code=$CODE"
  if [ "$CODE" = "200" ]; then
    echo "SUCCESS:"
    head -c 200 /tmp/smoke_curl_body.txt
    echo ""
    exit 0
  fi
  sleep 5
done

echo "FAILED: never got a 200 from the tunnel"
exit 1
