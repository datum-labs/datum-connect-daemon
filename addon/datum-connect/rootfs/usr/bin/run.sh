#!/usr/bin/with-contenv bashio
# shellcheck shell=bash
#
# Entrypoint for the Datum Connect add-on.
#
# Derived from deploy/daytona-poc/entrypoint.sh, with the sandbox-specific
# parts removed and the credential story swapped from a fake JWT to a real
# service account.
set -euo pipefail

PORT=47780
CONNECT_DIR=/data/connect
CREDENTIALS_HELPER=/usr/bin/sa-credentials-helper.sh

PROJECT="$(bashio::config 'project')"
KEY_FILE="$(bashio::config 'service_account_key_file')"
TARGET="$(bashio::config 'target')"
LABEL="$(bashio::config 'tunnel_label')"
LOG_LEVEL="$(bashio::config 'log_level')"

if bashio::var.is_empty "${PROJECT}"; then
    bashio::exit.nok "No 'project' configured. Set the Datum project this tunnel belongs to in the add-on configuration."
fi
if [ ! -s "${KEY_FILE}" ]; then
    bashio::exit.nok "No service account key at ${KEY_FILE}. Download one from Datum and place it there — the Samba or File Editor add-on can put it in /share."
fi

# Checked here rather than left for the daemon to fail on later: a wrong file
# in the right place is a confusing failure, and this is cheap.
if ! jq -e 'select(.type == "datum_service_account") | .client_id, .private_key, .scope' "${KEY_FILE}" >/dev/null 2>&1; then
    bashio::exit.nok "${KEY_FILE} is not a Datum service account key (expected a JSON file with type, client_id, private_key and scope). A personal login token will not work here."
fi

mkdir -p "${CONNECT_DIR}"
export DATUM_SA_KEY_FILE="${KEY_FILE}"

export DATUM_PLUGIN_MODE=1
export DATUM_PROJECT="${PROJECT}"
export DATUM_CONNECT_DIR="${CONNECT_DIR}"
export DATUM_SESSION="service-account@${PROJECT}"
export DATUM_CREDENTIALS_HELPER="${CREDENTIALS_HELPER}"
export RUST_LOG="datum_connect_daemon=${LOG_LEVEL},connect_lib=${LOG_LEVEL}"

# The daemon force-stops any tunnel left on for 24h. That backstop exists for
# tunnels a person opens and forgets; this add-on's entire job is a tunnel that
# stays on, and a daily silent outage is the failure it would produce here.
# Turning the add-on off in the UI is the off switch.
export DATUM_TUNNEL_MAX_HOURS=87600

if bashio::var.is_empty "${LABEL}" || [ "${LABEL}" = "null" ]; then
    LABEL="home-assistant"
fi

bashio::log.info "Starting Datum Connect daemon (project ${PROJECT}, target ${TARGET})"

/usr/bin/datum-connect-daemon --port "${PORT}" &
DAEMON_PID=$!

# Hand the daemon the terminal signal so the Supervisor can stop it cleanly,
# including while the tunnel setup below is still running.
trap 'kill -TERM "${DAEMON_PID}" 2>/dev/null || true' TERM INT

# The daemon answers HTTP slightly before it finishes writing setup.token, so
# waiting on the port alone races and yields a 401 on the first call. Wait for
# the token file itself — this is documented in deploy/daytona-poc.
TOKEN_FILE="${CONNECT_DIR}/daemon_auth/setup.token"
for _ in $(seq 1 30); do
    [ -s "${TOKEN_FILE}" ] && break
    if ! kill -0 "${DAEMON_PID}" 2>/dev/null; then
        bashio::exit.nok "Daemon exited during startup. Check the log above."
    fi
    sleep 1
done

if [ ! -s "${TOKEN_FILE}" ]; then
    bashio::exit.nok "Daemon never finished starting (no setup token after 30s)."
fi

bashio::log.info "Daemon up (pid ${DAEMON_PID})"

API="http://127.0.0.1:${PORT}/v1"
AUTH="Authorization: Bearer $(cat "${TOKEN_FILE}")"

# Ensure the tunnel exists and is on. Without this the add-on starts a daemon
# with nothing to serve, and installing it would still need someone with a
# shell on the box — which HAOS does not offer.
#
# Idempotent across restarts: the tunnel is found again by its label, and the
# daemon persists it, so a restart re-uses the same public hostname rather than
# minting a new one each boot.
if ! TUNNELS=$(curl -sf "${API}/tunnels" -H "${AUTH}"); then
    bashio::exit.nok "Could not list tunnels. The daemon is up but cannot reach Datum Cloud — usually the service account key or project is wrong. Check the log above."
fi
TUNNEL_ID=$(jq -r --arg l "${LABEL}" 'map(select(.label == $l)) | first | .id // empty' <<<"${TUNNELS}")

if [ -z "${TUNNEL_ID}" ]; then
    bashio::log.info "Creating tunnel '${LABEL}' → ${TARGET}"
    BODY=$(jq -n --arg l "${LABEL}" --arg e "${TARGET}" '{label: $l, endpoint: $e}')
    if ! CREATED=$(curl -sf -X POST "${API}/tunnels" -H "${AUTH}" -H "Content-Type: application/json" -d "${BODY}"); then
        bashio::exit.nok "Could not create the tunnel. Check that the service account can create tunnels in project ${PROJECT}."
    fi
    TUNNEL_ID=$(jq -r '.id' <<<"${CREATED}")
else
    EXISTING_TARGET=$(jq -r --arg id "${TUNNEL_ID}" '.[] | select(.id == $id) | .endpoint' <<<"${TUNNELS}")
    bashio::log.info "Using existing tunnel '${LABEL}' (${TUNNEL_ID}) → ${EXISTING_TARGET}"
    if [ "${EXISTING_TARGET%/}" != "${TARGET%/}" ]; then
        bashio::log.warning "The 'target' option is ${TARGET}, but tunnel '${LABEL}' already points at ${EXISTING_TARGET}. Change 'tunnel_label' to create a new tunnel for the new target."
    fi
fi

if ! curl -sf -X POST "${API}/tunnels/${TUNNEL_ID}/start" -H "${AUTH}" >/dev/null; then
    bashio::exit.nok "Could not start tunnel ${TUNNEL_ID}. Check the log above."
fi

# Hostname assignment and DNS publication take a little while after start.
# Report it when it lands; not finding it in time is worth a warning, not a
# failure, since the tunnel keeps converging on its own.
HOSTNAME=""
for _ in $(seq 1 60); do
    HOSTNAME=$(curl -sf "${API}/tunnels/${TUNNEL_ID}/progress" -H "${AUTH}" | jq -r '.hostnames[0] // empty' || true)
    [ -n "${HOSTNAME}" ] && break
    sleep 2
done

if [ -n "${HOSTNAME}" ]; then
    bashio::log.info "Home Assistant is reachable at https://${HOSTNAME}"
else
    bashio::log.warning "Tunnel started, but no public hostname was reported within 2 minutes. It may still be provisioning; restart the add-on to check again."
fi

# Surface the daemon's exit status as the container's.
wait "${DAEMON_PID}"
