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
PASTED_KEY="$(bashio::config 'service_account_key')"
KEY_FILE="$(bashio::config 'service_account_key_file')"
TARGET="$(bashio::config 'target')"
LABEL="$(bashio::config 'tunnel_label')"
LOG_LEVEL="$(bashio::config 'log_level')"

# A pasted key wins over a file. It is written to the add-on's private /data,
# readable by this add-on only, because the credentials helper reads a file.
# The browser strips the line breaks from a pasted multi-line file, which is
# harmless: whitespace between JSON tokens is insignificant, and the PEM key
# inside is stored with escaped \n, not real line breaks.
if ! bashio::var.is_empty "${PASTED_KEY}" && [ "${PASTED_KEY}" != "null" ]; then
    KEY_FILE=/data/service-account.json
    (umask 077 && printf '%s' "${PASTED_KEY}" > "${KEY_FILE}")
    KEY_SOURCE="the pasted service_account_key"
else
    if bashio::var.is_empty "${KEY_FILE}" || [ "${KEY_FILE}" = "null" ]; then
        KEY_FILE=/share/datum-service-account.json
    fi
    if [ ! -s "${KEY_FILE}" ]; then
        bashio::exit.nok "No service account key. Paste the key file's contents into 'service_account_key' on the Configuration tab (or place the file at ${KEY_FILE})."
    fi
    KEY_SOURCE="${KEY_FILE}"
fi
unset PASTED_KEY

# Checked here rather than left for the daemon to fail on later: a wrong file
# in the right place is a confusing failure, and this is cheap.
if ! jq -e 'select(.type == "datum_service_account") | .client_id, .private_key, .scope' "${KEY_FILE}" >/dev/null 2>&1; then
    bashio::exit.nok "${KEY_SOURCE} is not a Datum service account key (expected JSON with type, client_id, private_key and scope). Paste the whole file, braces included. A personal login token will not work here."
fi

# A service account's client_email is <name>@<project>.identity.miloapis.com,
# so the project it belongs to can be read off the key rather than typed in.
KEY_PROJECT=$(jq -r '.client_email // empty' "${KEY_FILE}" | sed -n 's/^[^@]*@\([^.]*\)\.identity\..*$/\1/p')
if bashio::var.is_empty "${PROJECT}" || [ "${PROJECT}" = "null" ]; then
    if [ -z "${KEY_PROJECT}" ]; then
        bashio::exit.nok "Could not tell which project the service account key belongs to. Set 'project' on the Configuration tab."
    fi
    PROJECT="${KEY_PROJECT}"
    bashio::log.info "Using project ${PROJECT} from the service account key"
elif [ -n "${KEY_PROJECT}" ] && [ "${KEY_PROJECT}" != "${PROJECT}" ]; then
    bashio::log.warning "'project' is ${PROJECT}, but the service account key belongs to ${KEY_PROJECT}. If tunnel calls are refused, clear 'project' to use the key's."
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

if bashio::var.is_empty "${LABEL}" || [ "${LABEL}" = "null" ]; then
    LABEL="home-assistant"
fi

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
# minting a new one each boot.
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

# Put Datum's WAF in front of the tunnel, on (Enforce) for everything except
# Home Assistant's streaming endpoints. The policy targets the HTTPProxy's
# "protected" rule by sectionName, and the streams have their own rule (see
# DATUM_TUNNEL_WAF_EXEMPT_MATCHES above), because a WAF on a stream makes
# Datum's edge hold the response back (datum-cloud/infra#6677).
#
# This must run after the daemon has applied those rules, which it does
# during the tunnel start above. Before that, sectionName "protected" would
# resolve to nothing, which is harmless (the policy applies to nothing) but
# means no WAF. The same goes for good if the daemon ever lacks the rule:
# the WAF is silently off, not on everything. check_protected_rule warns.
#
# Created only when missing, never overwritten: a policy that already exists
# may have been tuned in the portal, and a restart must not undo that. The
# one exception is the switched-off policy add-on 0.1.5 or 0.1.6 created, recognised
# strictly (see is_addon_015_waf) and moved to this setup once. Every policy
# the add-on writes carries the managed-by annotation; one without it is
# never changed.
#
# Paranoia level 1 with rule 920420 excluded is the setting validated on a
# real device when enforced (remote login, and saving an automation with a template
# condition, over cellular). 920420 has to go at every level: Home
# Assistant's login page POSTs JSON as text/plain, which CRS v4 rejects, so
# login fails. Attacks in text/plain bodies are still caught by the other
# rules. Level 2 also blocks any {{ }} template in a REST body, which breaks
# template automations, so it stays off until it has targeted exclusions.
#
# A failure here is a warning, not an exit: the tunnel already works, and
# taking it down because its WAF could not be set up helps no one.
DATUM_API=${DATUM_API_URL:-https://api.datum.net}
CONTROL_PLANE="${DATUM_API}/apis/resourcemanager.miloapis.com/v1alpha1/projects/${PROJECT}/control-plane"
WAF_NAME="${TUNNEL_ID}-waf"
WAF_URL="${CONTROL_PLANE}/apis/networking.datumapis.com/v1alpha/namespaces/default/trafficprotectionpolicies"
PROXY_URL="${CONTROL_PLANE}/apis/networking.datumapis.com/v1alpha/namespaces/default/httpproxies/${TUNNEL_ID}"
PORTAL_LINK="https://cloud.datum.net (project ${PROJECT}, policy ${WAF_NAME})"
MANAGED_BY_KEY="connect.datum.net/managed-by"
MANAGED_BY_VALUE="datum-connect-addon"
PROTECTED_RULE="protected"

# The policy's spec, shared by a fresh create and the 0.1.5/0.1.6 upgrade.
WAF_SPEC=$(jq -n --arg route "${TUNNEL_ID}" --arg section "${PROTECTED_RULE}" '{
    mode: "Enforce",
    samplingPercentage: 100,
    ruleSets: [{
        type: "OWASPCoreRuleSet",
        owaspCoreRuleSet: {
            paranoiaLevels: {blocking: 1, detection: 1},
            scoreThresholds: {inbound: 5, outbound: 4},
            ruleExclusions: {ids: [920420]}
        }
    }],
    targetRefs: [{group: "gateway.networking.k8s.io", kind: "HTTPRoute", name: $route, sectionName: $section}]
}')

# The start of a failed response, on one line. Error bodies can be whole HTML
# pages, and one dumped in full once buried the rest of the log.
excerpt() {
    local text
    text=$(tr -s '\r\n\t' '   ' < "$1")
    text="${text% }"
    if [ "${#text}" -gt 200 ]; then
        text="${text:0:200}..."
    fi
    printf '%s' "${text}"
}

# The WAF follows the "protected" rule by name, so a proxy without one has
# no WAF at all: if the daemon ran without DATUM_TUNNEL_WAF_EXEMPT_MATCHES,
# or the rules were edited by hand. Nothing else would show it.
check_protected_rule() {
    local token=$1 status body
    body=$(mktemp)
    status=$(curl -s -m 30 -o "${body}" -w '%{http_code}' "${PROXY_URL}" \
        -H "Authorization: Bearer ${token}" || true)
    if [ "${status}" != 200 ]; then
        bashio::log.warning "Could not check tunnel ${TUNNEL_ID}'s rules for the WAF (HTTP ${status:-no response}). Response: $(excerpt "${body}")"
    elif ! jq -e --arg rule "${PROTECTED_RULE}" 'any(.spec.rules[]?; .name == $rule)' "${body}" >/dev/null; then
        bashio::log.warning "!!! Edge protection is OFF: tunnel ${TUNNEL_ID} has no rule named '${PROTECTED_RULE}', so its WAF matches nothing. Restart the add-on; if this persists, report it."
    fi
    rm -f "${body}"
}

# True only for the exact policy add-on 0.1.5 or 0.1.6 created: switched off, aimed at
# the whole route, without our annotation, and with that rule set
# unchanged. Anything else may have been chosen in the portal.
is_addon_015_waf() {
    jq -e --arg route "${TUNNEL_ID}" --arg key "${MANAGED_BY_KEY}" '
        .spec.mode == "Disabled"
        and ((.metadata.annotations // {}) | has($key) | not)
        and (.spec.samplingPercentage // 100) == 100
        and (.spec.targetRefs | length) == 1
        and .spec.targetRefs[0].kind == "HTTPRoute"
        and .spec.targetRefs[0].name == $route
        and (.spec.targetRefs[0].sectionName // "") == ""
        and (.spec.ruleSets | length) == 1
        and .spec.ruleSets[0].type == "OWASPCoreRuleSet"
        and (.spec.ruleSets[0].owaspCoreRuleSet
            | .paranoiaLevels == {blocking: 1, detection: 1}
              and .scoreThresholds == {inbound: 5, outbound: 4}
              and .ruleExclusions == {ids: [920420]})
    ' "$1" >/dev/null
}

# Moves that switched-off policy to the current spec in place. The PUT carries the
# resourceVersion just read, so an edit made in between wins over this (409).
upgrade_015_waf() {
    local token=$1 current=$2 status body policy
    policy=$(jq --arg key "${MANAGED_BY_KEY}" --arg value "${MANAGED_BY_VALUE}" --argjson spec "${WAF_SPEC}" \
        'del(.status) | .spec = $spec | .metadata.annotations[$key] = $value' "${current}")
    body=$(mktemp)
    status=$(curl -s -m 30 -o "${body}" -w '%{http_code}' -X PUT "${WAF_URL}/${WAF_NAME}" \
        -H "Authorization: Bearer ${token}" -H "Content-Type: application/json" \
        -d "${policy}" || true)
    case "${status}" in
        200|201)
            bashio::log.info "Edge protection policy from add-on 0.1.5/0.1.6 switched ON: Datum WAF now enforces on everything except Home Assistant's streaming endpoints. Manage it in the Datum portal: ${PORTAL_LINK}" ;;
        409)
            bashio::log.warning "Edge protection still OFF: policy ${WAF_NAME} changed while being upgraded, so it was left as is. Restart the add-on to try again." ;;
        401|403)
            bashio::log.warning "Edge protection still OFF: the service account may not update WAF policies in project ${PROJECT} (HTTP ${status}). Give it that permission, then restart the add-on." ;;
        *)
            bashio::log.warning "Edge protection still OFF: upgrading policy ${WAF_NAME} failed (HTTP ${status:-no response}). Response: $(excerpt "${body}")" ;;
    esac
    rm -f "${body}"
}

ensure_waf() {
    local token=$1 status body policy
    check_protected_rule "${token}"
    body=$(mktemp)
    status=$(curl -s -m 30 -o "${body}" -w '%{http_code}' "${WAF_URL}/${WAF_NAME}" \
        -H "Authorization: Bearer ${token}" || true)
    case "${status}" in
        200)
            if jq -e --arg key "${MANAGED_BY_KEY}" '(.metadata.annotations // {}) | has($key)' "${body}" >/dev/null; then
                bashio::log.info "Edge protection policy found (Datum WAF, set up by this add-on, kept as set). Manage it in the Datum portal: ${PORTAL_LINK}"
            elif is_addon_015_waf "${body}"; then
                upgrade_015_waf "${token}" "${body}"
            else
                bashio::log.info "Edge protection policy found, not set up by this add-on, so kept as set. Unless it targets rule '${PROTECTED_RULE}', an enforcing WAF there also holds back live views. Manage it in the Datum portal: ${PORTAL_LINK}"
            fi
            rm -f "${body}"
            return ;;
        404) ;;
        *)
            bashio::log.warning "Edge protection NOT set up: could not check for policy ${WAF_NAME} (HTTP ${status:-no response}). Response: $(excerpt "${body}")"
            rm -f "${body}"
            return ;;
    esac

    policy=$(jq -n --arg name "${WAF_NAME}" --arg display "${LABEL}" \
        --arg key "${MANAGED_BY_KEY}" --arg value "${MANAGED_BY_VALUE}" --argjson spec "${WAF_SPEC}" '{
        apiVersion: "networking.datumapis.com/v1alpha",
        kind: "TrafficProtectionPolicy",
        metadata: {
            name: $name,
            namespace: "default",
            annotations: {"networking.datumapis.com/display-name": $display, ($key): $value}
        },
        spec: $spec
    }')
    status=$(curl -s -m 30 -o "${body}" -w '%{http_code}' -X POST "${WAF_URL}" \
        -H "Authorization: Bearer ${token}" -H "Content-Type: application/json" \
        -d "${policy}" || true)
    case "${status}" in
        200|201)
            bashio::log.info "Edge protection policy created: Datum WAF on (Enforce) for everything except Home Assistant's streaming endpoints. Manage it in the Datum portal: ${PORTAL_LINK}" ;;
        401|403)
            bashio::log.warning "Edge protection NOT set up: the service account may not create WAF policies in project ${PROJECT} (HTTP ${status}). Give it that permission, then restart the add-on." ;;
        *)
            bashio::log.warning "Edge protection NOT set up: creating policy ${WAF_NAME} failed (HTTP ${status:-no response}). Response: $(excerpt "${body}")" ;;
    esac
    rm -f "${body}"
}

# Let a request through the edge run for up to an hour. Datum's edge (Envoy)
# otherwise ends every response 15s after the request, its default route
# timeout. In Home Assistant that cuts streams, such as the add-on log view
# (/api/hassio/addons/<slug>/logs/follow), after ~15-20s, and any download
# that takes longer. An hour is the most the platform allows.
#
# Same rule as the WAF: created only when missing, never overwritten, so a
# value tuned since is kept across restarts.
#
# A failure is a warning, not an exit: the tunnel works without it, only
# long requests are cut short.
TIMEOUT_NAME="${TUNNEL_ID}-timeout"
TIMEOUT_URL="${CONTROL_PLANE}/apis/gateway.envoyproxy.io/v1alpha1/namespaces/default/backendtrafficpolicies"

ensure_request_timeout() {
    local token=$1 status body policy
    body=$(mktemp)
    status=$(curl -s -m 30 -o "${body}" -w '%{http_code}' "${TIMEOUT_URL}/${TIMEOUT_NAME}" \
        -H "Authorization: Bearer ${token}" || true)
    case "${status}" in
        200)
            bashio::log.info "Edge request timeout set (existing policy ${TIMEOUT_NAME} kept)"
            rm -f "${body}"
            return ;;
        404) ;;
        *)
            bashio::log.warning "Edge request timeout NOT raised: could not check for policy ${TIMEOUT_NAME} (HTTP ${status:-no response}). Response: $(excerpt "${body}")"
            rm -f "${body}"
            return ;;
    esac

    policy=$(jq -n --arg name "${TIMEOUT_NAME}" --arg route "${TUNNEL_ID}" '{
        apiVersion: "gateway.envoyproxy.io/v1alpha1",
        kind: "BackendTrafficPolicy",
        metadata: {name: $name, namespace: "default"},
        spec: {
            targetRefs: [{group: "gateway.networking.k8s.io", kind: "HTTPRoute", name: $route}],
            timeout: {http: {requestTimeout: "1h"}}
        }
    }')
    status=$(curl -s -m 30 -o "${body}" -w '%{http_code}' -X POST "${TIMEOUT_URL}" \
        -H "Authorization: Bearer ${token}" -H "Content-Type: application/json" \
        -d "${policy}" || true)
    case "${status}" in
        200|201)
            bashio::log.info "Edge request timeout raised to 1h (policy ${TIMEOUT_NAME})" ;;
        401|403)
            bashio::log.warning "Edge request timeout NOT raised: the service account may not create traffic policies in project ${PROJECT} (HTTP ${status}). Give it that permission, then restart the add-on." ;;
        *)
            bashio::log.warning "Edge request timeout NOT raised: creating policy ${TIMEOUT_NAME} failed (HTTP ${status:-no response}). Response: $(excerpt "${body}")" ;;
    esac
    rm -f "${body}"
}

# One token covers every call below; it outlives them by a wide margin.
if DATUM_TOKEN=$("${CREDENTIALS_HELPER}" auth get-token --session "${DATUM_SESSION}"); then
    ensure_waf "${DATUM_TOKEN}"
    ensure_request_timeout "${DATUM_TOKEN}"
else
    bashio::log.warning "Edge protection and request timeout NOT set up: could not get a token from the service account."
fi
unset DATUM_TOKEN

# Surface the daemon's exit status as the container's.
wait "${DAEMON_PID}"
