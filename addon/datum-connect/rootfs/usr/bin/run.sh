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
TARGET="$(bashio::config 'target')"
LABEL="$(bashio::config 'tunnel_label')"
LOG_LEVEL="$(bashio::config 'log_level')"

if bashio::var.is_empty "${PROJECT}"; then
    bashio::exit.nok "No 'project' configured. Set the Datum project this tunnel belongs to in the add-on configuration."
fi
if ! bashio::config.has_value 'service_account_key'; then
    bashio::exit.nok "No 'service_account_key' configured. This add-on needs a service account credential — a personal login token will expire and take the tunnel down with it."
fi

# Written to /data, which persists across add-on restarts and Home Assistant
# updates but is not part of an add-on config export.
mkdir -p "${CONNECT_DIR}"
install -m 600 /dev/null /data/service-account.key
bashio::config 'service_account_key' > /data/service-account.key

export DATUM_PLUGIN_MODE=1
export DATUM_PROJECT="${PROJECT}"
export DATUM_CONNECT_DIR="${CONNECT_DIR}"
export DATUM_SESSION="service-account@${PROJECT}"
export DATUM_CREDENTIALS_HELPER="${CREDENTIALS_HELPER}"
export RUST_LOG="datum_connect_daemon=${LOG_LEVEL},connect_lib=${LOG_LEVEL}"

bashio::log.info "Starting Datum Connect daemon (project ${PROJECT}, target ${TARGET})"

/usr/bin/datum-connect-daemon --port "${PORT}" &
DAEMON_PID=$!

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

# Hand the daemon the terminal signal so the Supervisor can stop it cleanly,
# and surface its exit status as the container's.
trap 'kill -TERM "${DAEMON_PID}" 2>/dev/null || true' TERM INT
wait "${DAEMON_PID}"
