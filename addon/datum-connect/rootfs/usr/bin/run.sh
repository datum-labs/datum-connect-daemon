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
# The Datum Connect page (ingress). The Supervisor says which port it
# connects to; 47781 is what config.yaml asks for.
INGRESS_PORT="$(bashio::addon.ingress_port 2>/dev/null || true)"
if bashio::var.is_empty "${INGRESS_PORT}" || [ "${INGRESS_PORT}" = "null" ] || [ "${INGRESS_PORT}" = "0" ]; then
    INGRESS_PORT=47781
fi

PROJECT="$(bashio::config 'project')"
PASTED_KEY="$(bashio::config 'service_account_key')"
KEY_FILE="$(bashio::config 'service_account_key_file')"
TARGET="$(bashio::config 'target')"
LABEL="$(bashio::config 'tunnel_label')"
LOG_LEVEL="$(bashio::config 'log_level')"

# A project id as typed or pasted, in the one form it is compared and passed
# on in: surrounding whitespace and one pair of surrounding quotes removed,
# and lowercased (Datum project ids are DNS-1123 names). Mirrors
# normalize_project in the daemon's pairing.rs, so that what pairing accepts
# and what the key's project is compared against below always agree.
normalize_project() {
    local p=$1 q
    p="${p#"${p%%[![:space:]]*}"}"
    p="${p%"${p##*[![:space:]]}"}"
    for q in '"' "'" '`'; do
        if [ "${#p}" -ge 2 ] && [ "${p:0:1}" = "${q}" ] && [ "${p: -1}" = "${q}" ]; then
            p="${p:1:${#p}-2}"
            p="${p#"${p%%[![:space:]]*}"}"
            p="${p%"${p##*[![:space:]]}"}"
            break
        fi
    done
    printf '%s' "${p}" | LC_ALL=C tr '[:upper:]' '[:lower:]'
}

if bashio::var.is_empty "${PROJECT}" || [ "${PROJECT}" = "null" ]; then
    PROJECT=""
fi
PROJECT=$(normalize_project "${PROJECT}")

# Where each kind of key lives in the add-on's private /data, readable by
# this add-on only. Kept apart so that clearing a pasted key falls back to
# the paired one, never to a leftover copy of the paste.
PASTED_KEY_FILE=/data/pasted-service-account.json
PAIRED_KEY_FILE=/data/service-account.json
# Left by a pairing that stopped at the access grant, so that the next one
# reuses its service account instead of making another. Names only.
PAIRING_PROGRESS="${PAIRED_KEY_FILE}.pending"
# The approving person's login, kept by pairing while it waits for a project
# so that a restart then (Home Assistant offers one on every save of the
# options) needs no new approval. Pairing deletes it when it ends.
PAIRING_SESSION=/data/pairing-session.json
# What `setup` exits with when the add-on is stopped before pairing
# finished, with or without a login kept for the next start: a pause, not a
# failure.
PAIR_PAUSED=75

# Read by both `setup` and the daemon: the page's port, and which key file
# the page's Re-pair and Unpair may delete. The page's address is asked of
# the Supervisor (ingress.rs).
export DATUM_INGRESS_PORT="${INGRESS_PORT}"
export DATUM_PAIRED_KEY_FILE="${PAIRED_KEY_FILE}"

# Which key, in order: a pasted key, a key file placed by hand, the key an
# earlier pairing saved, and otherwise pair now.
#
# A pasted key is written to a file because the daemon reads the key from
# one. The browser strips the line breaks from a pasted multi-line file,
# which is harmless: whitespace between JSON tokens is insignificant, and the
# PEM key inside is stored with escaped \n, not real line breaks.
if ! bashio::var.is_empty "${PASTED_KEY}" && [ "${PASTED_KEY}" != "null" ]; then
    # Before 0.2.0 the paste was copied to the path a paired key now uses.
    # Remove that copy, so that clearing the paste later does not pass it off
    # as a paired key. A real paired key differs from the paste and is kept.
    if [ -f "${PAIRED_KEY_FILE}" ] && [ "$(cat "${PAIRED_KEY_FILE}")" = "${PASTED_KEY}" ]; then
        rm -f "${PAIRED_KEY_FILE}"
    fi
    KEY_FILE="${PASTED_KEY_FILE}"
    (umask 077 && printf '%s' "${PASTED_KEY}" > "${KEY_FILE}")
    KEY_SOURCE="the pasted service_account_key"
    if bashio::config.true 'repair'; then
        bashio::log.warning "'repair' is on, but the pasted service_account_key is in use, so there is nothing to re-pair. Turn 'repair' off."
    fi
else
    rm -f "${PASTED_KEY_FILE}"
    if bashio::var.is_empty "${KEY_FILE}" || [ "${KEY_FILE}" = "null" ]; then
        KEY_FILE=/share/datum-service-account.json
    fi

    # Re-pairing: forget the paired key so that a new one is made below. The
    # old service account is not deleted: the add-on holds no login that
    # could. Name it, so it can be deleted by hand.
    if bashio::config.true 'repair'; then
        if [ -s "${PAIRED_KEY_FILE}" ]; then
            OLD_ACCOUNT=$(jq -r '.client_email // "an unnamed service account"' "${PAIRED_KEY_FILE}" 2>/dev/null || echo "an unnamed service account")
            rm -f "${PAIRED_KEY_FILE}"
            bashio::log.warning "'repair' is on: deleted the paired key for ${OLD_ACCOUNT}. That service account still exists in Datum. Once pairing again has worked, delete it in the portal, under the project's Service accounts."
        fi
        rm -f "${PAIRING_PROGRESS}"
        bashio::log.warning "'repair' is on. Turn it off on the Configuration tab once pairing has worked, or every restart pairs again."
    fi

    if [ -s "${KEY_FILE}" ]; then
        KEY_SOURCE="${KEY_FILE}"
    else
        if [ ! -s "${PAIRED_KEY_FILE}" ]; then
            bashio::log.info "No service account key yet. Open Datum Connect in the sidebar (or Open Web UI on the Info tab) and click Connect to Datum."
            # `setup` serves the Datum Connect page on the ingress port and
            # pairs when Connect is clicked there; the link and code are
            # still logged and shown as a notification. If the page cannot
            # be served, it pairs from the log, as `pair` did before 0.3.0.
            # --hold-port: the daemon's port answers while setup waits, as
            # it did for `pair`.
            # --options-source supervisor: 'project' saved on the
            # Configuration tab still chooses the project, read from the
            # Supervisor, since /data/options.json only changes on a restart.
            # --session-file: saving the options makes Home Assistant offer
            # a restart, which would otherwise mean a second approval.
            PAIR_ARGS=(setup --key-out "${PAIRED_KEY_FILE}" --hold-port "${PORT}" --options-source supervisor --session-file "${PAIRING_SESSION}")
            if [ -n "${PROJECT}" ]; then
                PAIR_ARGS+=(--project "${PROJECT}")
            fi
            PAIR_STATUS=0
            /usr/bin/datum-connect-daemon "${PAIR_ARGS[@]}" || PAIR_STATUS=$?
            if [ "${PAIR_STATUS}" -eq "${PAIR_PAUSED}" ]; then
                # Stopped before pairing finished; setup has said what
                # happens on the next start.
                exit 0
            elif [ "${PAIR_STATUS}" -ne 0 ]; then
                bashio::exit.nok "Pairing with Datum did not finish; the reason is above. Restart the add-on to try again, or use your own service account key (see the Documentation tab)."
            fi
            # The project may have been chosen while pairing waited, after
            # PROJECT was read from the stale options file. The key belongs
            # to whichever project was used, so take it from the key below.
            PROJECT=""
        fi
        KEY_FILE="${PAIRED_KEY_FILE}"
        KEY_SOURCE="the key saved by pairing"
    fi
fi
unset PASTED_KEY
# Not pairing now, or pairing is done: a login left from a pairing that was
# stopped and never resumed has no further use, so it does not stay on disk.
rm -f "${PAIRING_SESSION}"

# Checked here rather than left for the daemon to fail on later: a wrong file
# in the right place is a confusing failure, and this is cheap.
if ! jq -e '.type == "datum_service_account" and (.client_id and .private_key_id and .private_key and .scope)' "${KEY_FILE}" >/dev/null 2>&1; then
    bashio::exit.nok "${KEY_SOURCE} is not a Datum service account key (expected JSON with type, client_id, private_key_id, private_key and scope). Paste the whole file, braces included. A personal login token will not work here."
fi

# A service account's client_email is <name>@<project>.identity.miloapis.com,
# so the project it belongs to can be read off the key rather than typed in.
KEY_PROJECT=$(jq -r '.client_email // empty' "${KEY_FILE}" | sed -n 's/^[^@]*@\([^.]*\)\.identity\..*$/\1/p')
# A paired key was granted access to the one project it was made in.
if [ "${KEY_FILE}" = "${PAIRED_KEY_FILE}" ] && [ -n "${PROJECT}" ] && [ -n "${KEY_PROJECT}" ] && [ "${KEY_PROJECT}" != "${PROJECT}" ]; then
    bashio::exit.nok "'project' is ${PROJECT}, but this add-on was paired with project ${KEY_PROJECT}. Clear 'project' to keep using ${KEY_PROJECT}, or turn on 'repair' to pair with ${PROJECT}."
fi
if [ -z "${PROJECT}" ]; then
    if [ -z "${KEY_PROJECT}" ]; then
        bashio::exit.nok "Could not tell which project the service account key belongs to. Set 'project' on the Configuration tab."
    fi
    PROJECT="${KEY_PROJECT}"
    bashio::log.info "Using project ${PROJECT} from the service account key"
elif [ -n "${KEY_PROJECT}" ] && [ "${KEY_PROJECT}" != "${PROJECT}" ]; then
    bashio::log.warning "'project' is ${PROJECT}, but the service account key belongs to ${KEY_PROJECT}. If tunnel calls are refused, clear 'project' to use the key's."
fi

mkdir -p "${CONNECT_DIR}"
# The daemon signs a short-lived assertion with this key and exchanges it for
# an access token, a fresh one on every refresh, with no helper process.
export DATUM_SA_KEY_FILE="${KEY_FILE}"

export DATUM_PLUGIN_MODE=1
export DATUM_PROJECT="${PROJECT}"
export DATUM_CONNECT_DIR="${CONNECT_DIR}"
export DATUM_SESSION="service-account@${PROJECT}"
export RUST_LOG="datum_connect_daemon=${LOG_LEVEL},connect_lib=${LOG_LEVEL}"

# The daemon force-stops any tunnel left on for 24h. That backstop exists for
# tunnels a person opens and forgets; this add-on's entire job is a tunnel that
# stays on, and a daily silent outage is the failure it would produce here.
# Turning the add-on off in the UI is the off switch.
export DATUM_TUNNEL_MAX_HOURS=87600

# Home Assistant's streaming endpoints get their own rule on the tunnel's
# HTTPProxy, named "streams", ahead of the main rule, named "protected", and
# the WAF below is scoped to "protected" only. With a WAF attached, Datum's
# edge holds back streamed responses (datum-cloud/infra#6677), so without
# this split HA's live views never load. The daemon writes the split itself
# on every start; a split made from outside would be undone on the next one.
#
# The paths: Supervisor and add-on live logs (the follow variants of HA
# core's NO_TIMEOUT list in homeassistant/components/hassio/http.py), the
# event stream /api/stream, and MJPEG camera streams. They still need a Home
# Assistant login; only the WAF is skipped. A near miss, such as .../logs
# without /follow, falls through to "protected". The daemon refuses to start
# if this is not valid JSON.
DATUM_TUNNEL_WAF_EXEMPT_MATCHES=$(jq -c . <<'EOF'
[
  {"path": {"type": "RegularExpression", "value": "^/api/hassio/(?:(?:audio|cli|core|dns|host|multicast|observer|supervisor)|addons/[^/]+)/logs/(?:boots/-?[0-9]+/)?follow$"}},
  {"path": {"type": "Exact", "value": "/api/stream"}},
  {"path": {"type": "PathPrefix", "value": "/api/camera_proxy_stream/"}}
]
EOF
)
export DATUM_TUNNEL_WAF_EXEMPT_MATCHES

# Have the daemon put Datum's WAF in front of the tunnel, on (Enforce) for
# everything but the streams above, by scoping it to the "protected" rule,
# and raise the edge's request timeout to 1h, since Envoy's 15s default cuts
# HA's live views. Both policies are created only when missing, so changes
# made in the portal survive. The daemon checks them every time it starts
# the tunnel, so a failure is retried on the next add-on start. See DOCS.md.
export DATUM_TUNNEL_EDGE_POLICIES=1

if bashio::var.is_empty "${LABEL}" || [ "${LABEL}" = "null" ]; then
    LABEL="home-assistant"
fi

# The add-on owns exactly one tunnel: the first with this label, as found
# below. Changing the label makes a new tunnel, and without this the daemon
# would also bring the old one back on every start (its target and key are
# still in /data/connect) and resume it, so both stayed publicly online. With
# it, the daemon stops any other tunnel it has local state for, never
# resumes it, and the Datum Connect page offers to remove it. Tunnels it has
# no local state for (another machine in a shared project) are left alone.
# Passed to the daemon rather than handled here, because the daemon resumes
# tunnels before its API answers.
export DATUM_TUNNEL_EXCLUSIVE_LABEL="${LABEL}"

# No target set means "this Home Assistant". Ask the Supervisor which port it
# serves on rather than assuming 8123: the first real install was on port 80,
# and the hard-coded default produced a 502 at the public hostname.
if bashio::var.is_empty "${TARGET}" || [ "${TARGET}" = "null" ]; then
    HA_PORT=$(bashio::core.port 2>/dev/null || true)
    HA_SSL=$(bashio::core.ssl 2>/dev/null || true)
    if bashio::var.is_empty "${HA_PORT}" || [ "${HA_PORT}" = "null" ]; then
        HA_PORT=8123
        bashio::log.warning "Could not ask the Supervisor for Home Assistant's port; assuming ${HA_PORT}. Set 'target' if that is wrong."
    fi
    # The daemon forwards plain HTTP only. Home Assistant serving TLS itself
    # (ssl_certificate set) would fail every request with a 502 that says
    # nothing about why, so stop here and say it instead.
    if bashio::var.true "${HA_SSL}"; then
        bashio::exit.nok "Home Assistant serves HTTPS directly on port ${HA_PORT}, and this add-on can only forward plain HTTP. Set 'target' to a plain-HTTP address for Home Assistant, or remove its own certificate settings. Datum's edge provides HTTPS either way."
    fi
    TARGET="http://127.0.0.1:${HA_PORT}"
    bashio::log.info "Detected Home Assistant on port ${HA_PORT}"
fi

bashio::log.info "Starting Datum Connect daemon (project ${PROJECT}, target ${TARGET})"

/usr/bin/datum-connect-daemon --port "${PORT}" &
DAEMON_PID=$!

# Hand the daemon the terminal signal so the Supervisor can stop it cleanly,
# including while the tunnel setup below is still running.
trap 'kill -TERM "${DAEMON_PID}" 2>/dev/null || true' TERM INT

# Wait for both the setup token and the API itself. Neither one alone is
# enough, and which arrives first varies: deploy/daytona-poc saw the port
# answer before the token was written, while on the Green the token was
# written first and the listener only bound after the daemon finished
# reconciling existing tunnels against Datum Cloud — ~0.5s with ten tunnels,
# longer with more. GET /v1/info needs no auth, so it is a clean readiness
# probe.
API="http://127.0.0.1:${PORT}/v1"
TOKEN_FILE="${CONNECT_DIR}/daemon_auth/setup.token"
READY=false
for _ in $(seq 1 60); do
    if [ -s "${TOKEN_FILE}" ] && curl -sf -o /dev/null "${API}/info"; then
        READY=true
        break
    fi
    if ! kill -0 "${DAEMON_PID}" 2>/dev/null; then
        bashio::exit.nok "Daemon exited during startup. Check the log above."
    fi
    sleep 1
done

if [ "${READY}" != true ]; then
    bashio::exit.nok "Daemon never finished starting (API not answering after 60s). Check the log above."
fi

bashio::log.info "Daemon up (pid ${DAEMON_PID})"

AUTH="Authorization: Bearer $(cat "${TOKEN_FILE}")"

# Ensure the tunnel exists and is on. Without this the add-on starts a daemon
# with nothing to serve, and installing it would still need someone with a
# shell on the box — which HAOS does not offer.
#
# Idempotent across restarts: the tunnel is found again by its label, and the
# daemon persists it, so a restart re-uses the same public hostname rather than
# minting a new one each boot. The tunnel found may predate this install (an
# uninstall wipes /data, the tunnel lives on in Datum Cloud), so the start
# below always passes the target: the daemon then gives the tunnel a local
# inspector for it if it has none, and repoints it if 'target' changed.
# Keep the status and body apart: a 401/403 here means the service account is
# not allowed into the project, anything else is not a credential problem, and
# telling the user to recheck their key for a non-credential failure sends
# them the wrong way.
TUNNELS_FILE=$(mktemp)
STATUS=$(curl -s -o "${TUNNELS_FILE}" -w '%{http_code}' "${API}/tunnels" -H "${AUTH}" || true)
TUNNELS=$(cat "${TUNNELS_FILE}")
rm -f "${TUNNELS_FILE}"
case "${STATUS}" in
    200) ;;
    401|403)
        bashio::exit.nok "Datum Cloud refused the service account for project ${PROJECT} (HTTP ${STATUS}). Check that the key belongs to this project. Response: ${TUNNELS}" ;;
    *)
        bashio::exit.nok "Could not list tunnels (HTTP ${STATUS:-no response}). Response: ${TUNNELS}" ;;
esac
TUNNEL_ID=$(jq -r --arg l "${LABEL}" 'map(select(.label == $l)) | first | .id // empty' <<<"${TUNNELS}")

# POSTs a JSON body to the daemon. Sets RESPONSE to the response body and
# RESPONSE_STATUS to the HTTP status, or 000 when nothing answered.
post_json() {
    local url=$1 body=$2 file
    file=$(mktemp)
    RESPONSE_STATUS=$(curl -s -o "${file}" -w '%{http_code}' -X POST "${url}" -H "${AUTH}" -H "Content-Type: application/json" -d "${body}" || true)
    RESPONSE=$(cat "${file}")
    rm -f "${file}"
}

# The daemon's reason for a failed call, short enough for one log line: its
# .error field if the body is JSON, else the raw body, flattened and cut at
# 200 characters.
error_excerpt() {
    local msg
    msg=$(jq -r '.error // empty' <<<"${RESPONSE}" 2>/dev/null || true)
    [ -n "${msg}" ] || msg=${RESPONSE:-no response body}
    msg=$(printf '%s' "${msg}" | tr '\r\n\t' '   ')
    printf '%s' "${msg:0:200}"
}

if [ -z "${TUNNEL_ID}" ]; then
    TUNNEL_CREATED=true
    bashio::log.info "Creating tunnel '${LABEL}' → ${TARGET}"
    BODY=$(jq -n --arg l "${LABEL}" --arg e "${TARGET}" '{label: $l, endpoint: $e}')
    post_json "${API}/tunnels" "${BODY}"
    if [ "${RESPONSE_STATUS}" != 200 ]; then
        bashio::exit.nok "Could not create the tunnel (HTTP ${RESPONSE_STATUS:-000}): $(error_excerpt). Check that the service account can create tunnels in project ${PROJECT}."
    fi
    TUNNEL_ID=$(jq -r '.id' <<<"${RESPONSE}")
else
    bashio::log.info "Using existing tunnel '${LABEL}' (${TUNNEL_ID}); pointing it at ${TARGET}"
    # The daemon keeps each tunnel's iroh key at <connect dir>/<project>/<id>/
    # listen_key, and the tunnel's connector is registered under that key.
    # With no key here, this tunnel came from an earlier install (an uninstall
    # wipes /data), so the start below has to give it a new key and a new
    # connector. Checked now because the start writes the new key. A tunnel
    # adopted this way can show "Service offline" for a few minutes; on a
    # real device it came back within about 4 minutes. The line after the
    # start says what to do if it doesn't.
    if [ ! -s "${CONNECT_DIR}/${PROJECT}/${TUNNEL_ID}/listen_key" ]; then
        CONNECTOR_REPLACED=true
    fi
fi

BODY=$(jq -n --arg t "${TARGET}" '{target: $t}')
post_json "${API}/tunnels/${TUNNEL_ID}/start" "${BODY}"
if [ "${RESPONSE_STATUS}" != 200 ]; then
    bashio::exit.nok "Could not start tunnel ${TUNNEL_ID} (HTTP ${RESPONSE_STATUS:-000}): $(error_excerpt)"
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
    if [ "${TUNNEL_CREATED:-false}" = true ]; then
        # Seen on a real install: a brand-new address gave Firefox a 503
        # and Chrome "Unable to connect" for 15-20 minutes, while plain
        # HTTP/1.1 worked after about 8. Only said when the tunnel was just
        # created: a reused one's address is not new.
        bashio::log.info "Home Assistant is reachable at https://${HOSTNAME} (a new address can take up to 20 minutes to work everywhere)"
    else
        bashio::log.info "Home Assistant is reachable at https://${HOSTNAME}"
    fi
else
    bashio::log.warning "Tunnel started, but no public hostname was reported within 2 minutes. It may still be provisioning; restart the add-on to check again."
fi

if [ "${CONNECTOR_REPLACED:-false}" = true ]; then
    bashio::log.info "Re-using tunnel '${LABEL}' (${TUNNEL_ID}) from an earlier install: its connector was replaced, so https://${HOSTNAME:-<its hostname>} may show 'Service offline' for a few minutes. If it's still offline after 20 minutes, restart the add-on; if that doesn't help, set a new 'tunnel_label'."
fi

# Surface the daemon's exit status as the container's.
wait "${DAEMON_PID}"
