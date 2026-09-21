#!/bin/sh
# Sandbox entrypoint for the runnable ec2-zero-trust-ssh Daytona demo.
# Identical wiring to ../../tunnel-demo-daytona/sandbox/entrypoint.sh --
# starts a local peer-only daemon, installs the caller's public key, starts
# sshd, starts a static web server for the subway-map page, then advertises
# all three (sshd, web server, this daemon's own dashboard) over Datum peer
# tunnels so the laptop can `peer connect` to each of them. Everything keeps
# running in the background after this script exits -- Daytona sandboxes
# are long-running, not batch jobs.
set -e

echo "=== [1/5] starting local daemon (peer-only, no real Datum project needed) ==="
export DATUM_PLUGIN_MODE=1
export DATUM_SESSION=sandbox-session
export DATUM_CREDENTIALS_HELPER=/opt/datum/fake-credentials-helper.sh
export DATUM_PROJECT=sandbox-poc
export DATUM_CONNECT_DIR=/tmp/datum-connect
export RUST_LOG=info,iroh=debug,connect_lib=debug
rm -rf "$DATUM_CONNECT_DIR"

/opt/datum/datum-connect-daemon --port 47780 > /tmp/daemon.log 2>&1 &
DAEMON_PID=$!

TOKEN_FILE="$DATUM_CONNECT_DIR/daemon_auth/setup.token"
for i in $(seq 1 30); do
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

echo "=== [2/5] installing the caller's SSH public key ==="
if [ -z "$SSH_PUBKEY" ]; then
  echo "SSH_PUBKEY not set -- this image is meant to be launched with a per-instance public key injected as an env var" >&2
  exit 1
fi
echo "$SSH_PUBKEY" > /home/demo/.ssh/authorized_keys
chmod 600 /home/demo/.ssh/authorized_keys
chown demo:demo /home/demo/.ssh/authorized_keys

echo "=== [3/5] starting sshd ==="
/usr/sbin/sshd
for i in $(seq 1 10); do
  if pgrep sshd > /dev/null 2>&1; then
    break
  fi
  sleep 0.5
done
if ! pgrep sshd > /dev/null 2>&1; then
  echo "sshd never came up" >&2
  exit 1
fi
echo "sshd up"

echo "=== [4/5] starting the subway-map web page ==="
python3 -m http.server 8899 --bind 127.0.0.1 --directory /opt/datum/web > /tmp/web.log 2>&1 &
WEB_PID=$!
for i in $(seq 1 20); do
  if curl -sf -o /dev/null http://127.0.0.1:8899/; then
    break
  fi
  sleep 0.5
done
if ! curl -sf -o /dev/null http://127.0.0.1:8899/; then
  echo "web server never came up -- full log:" >&2
  cat /tmp/web.log >&2
  exit 1
fi
echo "web server up (pid $WEB_PID)"

echo "=== [5/5] advertising sshd, the web server, and this daemon's own dashboard over peer tunnels ==="
SETUP_TOKEN=$(cat "$TOKEN_FILE")
CLI=/opt/datum/datumctl-connect

advertise() {
  # $1 = endpoint, $2 = label, $3 = name (used in the printed marker line)
  endpoint="$1"; label="$2"; name="$3"
  result=""
  for i in $(seq 1 10); do
    if result=$($CLI --port 47780 --token "$SETUP_TOKEN" tunnel api peer advertise --endpoint "$endpoint" --label "$label" 2>&1); then
      break
    fi
    echo "(peer advertise for $name attempt $i failed, retrying...)" >&2
    result=""
    sleep 1
  done
  if [ -z "$result" ]; then
    echo "peer advertise for $name failed -- daemon alive? $(kill -0 "$DAEMON_PID" 2>/dev/null && echo yes || echo no) -- full daemon log:" >&2
    cat /tmp/daemon.log >&2
    exit 1
  fi
  # `peer advertise` prints the JSON block followed by a plain-language note
  # ("Send the ticket value above...") -- jq needs just the JSON object, so
  # trim to the first top-level {...} block before parsing it.
  ad_json=$(echo "$result" | sed -n '/^{/,/^}/p')
  resource_id=$(echo "$ad_json" | jq -r '.resource_id')
  ticket=$(echo "$ad_json" | jq -r '.ticket')
  if [ -z "$resource_id" ] || [ "$resource_id" = "null" ] || [ -z "$ticket" ] || [ "$ticket" = "null" ]; then
    echo "failed to parse resource_id/ticket out of the advertise response for $name" >&2
    exit 1
  fi
  echo "TUNNEL_DEMO_TICKET_JSON: $(jq -nc --arg n "$name" --arg r "$resource_id" --arg t "$ticket" '{name:$n,resource_id:$r,ticket:$t}')"
}

advertise 127.0.0.1:22 ssh-ec2demo ssh
advertise 127.0.0.1:8899 web-ec2demo web
advertise 127.0.0.1:47780 dashboard-ec2demo dashboard

echo "=== done: sandbox is advertising all three, waiting to be connected to ==="
