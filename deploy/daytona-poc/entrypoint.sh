#!/bin/sh
# Immutable sandbox image entrypoint -- "call home" over a Datum peer
# tunnel, fetch config, do (simulated) work, send results home. See
# NOTES.md's "Immutable sandbox image POC" section for the full design.
#
# Nothing here is baked-in per-run config: the only thing that varies
# between sandbox instances is $SANDBOX_TICKET, injected fresh by the
# orchestration script at `daytona.create(..., env_vars=...)` time. The
# image itself is identical across every run.
set -e

echo "=== [1/5] starting local daemon (peer-only, no real Datum project needed) ==="
export DATUM_PLUGIN_MODE=1
export DATUM_SESSION=sandbox-session
export DATUM_CREDENTIALS_HELPER=/opt/datum/fake-credentials-helper.sh
export DATUM_PROJECT=sandbox-poc
export DATUM_CONNECT_DIR=/tmp/datum-connect
export RUST_LOG=info,iroh=debug,connect_lib=debug
rm -rf "$DATUM_CONNECT_DIR"

echo "--- basic outbound connectivity check (not iroh-specific) ---"
if curl -sf --max-time 5 -o /dev/null https://example.com; then
  echo "general internet egress: OK"
else
  echo "general internet egress: FAILED (exit $?) -- if this fails too, it's a sandbox network policy, not an iroh/QUIC-specific block"
fi

/opt/datum/datum-connect-daemon --port 47780 > /tmp/daemon.log 2>&1 &
DAEMON_PID=$!

TOKEN_FILE="$DATUM_CONNECT_DIR/daemon_auth/setup.token"
for i in $(seq 1 30); do
  # The daemon answers HTTP (the unauthenticated dashboard page) slightly
  # before it finishes writing setup.token -- wait for the token file
  # itself, not just an HTTP response, or the next step races it.
  if [ -f "$TOKEN_FILE" ]; then
    break
  fi
  sleep 0.5
done
if [ ! -f "$TOKEN_FILE" ]; then
  echo "daemon never finished starting up (no setup token after 15s):" >&2
  cat /tmp/daemon.log >&2
  exit 1
fi
echo "daemon up (pid $DAEMON_PID)"

if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
  echo "daemon process $DAEMON_PID is already gone right after startup -- full log:" >&2
  cat /tmp/daemon.log >&2
  exit 1
fi

echo "=== [2/5] connecting home over the peer tunnel ==="
if [ -z "$SANDBOX_TICKET" ]; then
  echo "SANDBOX_TICKET not set -- this image is meant to be launched with a per-instance ticket injected as an env var" >&2
  exit 1
fi

SETUP_TOKEN=$(cat "$DATUM_CONNECT_DIR/daemon_auth/setup.token")
CLI=/opt/datum/datumctl-connect

if ! CONNECT_RESULT=$($CLI --port 47780 --token "$SETUP_TOKEN" tunnel api peer connect --ticket "$SANDBOX_TICKET" --bind 127.0.0.1:0); then
  echo "peer connect failed -- daemon alive? $(kill -0 "$DAEMON_PID" 2>/dev/null && echo yes || echo no) -- full daemon log:" >&2
  cat /tmp/daemon.log >&2
  exit 1
fi
echo "$CONNECT_RESULT"
BOUND_ADDR=$(echo "$CONNECT_RESULT" | jq -r '.bound_addr')
if [ -z "$BOUND_ADDR" ] || [ "$BOUND_ADDR" = "null" ]; then
  echo "failed to establish peer connection home" >&2
  exit 1
fi
echo "connected home, local bound address: $BOUND_ADDR"

echo "=== [3/5] fetching config from home ==="
# The peer connection binding a local port doesn't mean the underlying
# iroh handshake (real NAT traversal/relay negotiation across two genuinely
# different networks -- Daytona's infrastructure and wherever home runs)
# has actually completed yet -- retry briefly rather than assuming it's
# instant the way a same-host test would be.
CONFIG=""
for i in $(seq 1 10); do
  if CONFIG=$(curl -sf --max-time 5 "http://$BOUND_ADDR/config"); then
    break
  fi
  echo "(config fetch attempt $i failed, retrying...)" >&2
  CONFIG=""
  sleep 2
done
if [ -z "$CONFIG" ]; then
  echo "never got config from home after retrying -- last 150 lines of daemon log (RUST_LOG=info,iroh=debug):" >&2
  tail -150 /tmp/daemon.log >&2
  exit 1
fi
echo "$CONFIG" | tee /tmp/fetched-config.json
CONFIG_INSTANCE_ID=$(echo "$CONFIG" | jq -r '.broker_instance_id')

echo "=== [4/5] doing the sandbox's actual work (simulated) ==="
# Stand-in for whatever real task this sandbox exists to run -- the point
# being demonstrated is the config/data round trip, not the task itself.
RESULT_HASH=$(echo "$CONFIG" | sha256sum | cut -d' ' -f1)
echo "computed result hash: $RESULT_HASH"

echo "=== [5/5] sending results home ==="
PAYLOAD=$(jq -n \
  --arg config_instance_id "$CONFIG_INSTANCE_ID" \
  --arg result_hash "$RESULT_HASH" \
  --arg ran_at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  '{config_instance_id: $config_instance_id, result_hash: $result_hash, ran_at: $ran_at}')
curl -sf -X POST -H "Content-Type: application/json" -d "$PAYLOAD" "http://$BOUND_ADDR/data"
echo ""
echo "=== done: config fetched from a service with no public IP, results sent back the same way ==="
