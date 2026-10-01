#!/usr/bin/env bash
# Credentials helper for Datum service-account auth.
#
# The daemon shells out to DATUM_CREDENTIALS_HELPER with
# `auth get-token --session <session>` and reads a bearer token from stdout.
# That contract is defined by ExternalTokenSource::exec_helper in connect-lib.
#
# Datum's IdP is Zitadel, which accepts the OAuth 2.0 JWT-bearer grant: sign a
# short-lived assertion with the service account's private key and exchange it
# for an access token. No browser and no refresh token, which is the whole
# reason an appliance uses this rather than a human session — a personal login
# was observed dying after two days, taking the tunnel with it.
#
# Every invocation mints a genuinely new token. That matters: the daemon
# re-runs this helper whenever it sees a 401, and a helper that returns a
# cached token instead produces an unrecoverable retry loop (issue #12). The
# JWT-bearer flow has no cache to get stuck in.
set -euo pipefail

KEY_FILE=${DATUM_SA_KEY_FILE:-/data/service-account.key}
ISSUER=${DATUM_AUTH_ISSUER:-https://auth.datum.net}
TOKEN_ENDPOINT="${ISSUER}/oauth/v2/token"

if [ ! -s "${KEY_FILE}" ]; then
    echo "sa-credentials-helper: no service account key at ${KEY_FILE}" >&2
    exit 1
fi

b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

kid=$(jq -re .private_key_id "${KEY_FILE}")
client_id=$(jq -re .client_id "${KEY_FILE}")
scope=$(jq -re .scope "${KEY_FILE}")

now=$(date +%s)
header=$(printf '{"alg":"RS256","kid":"%s","typ":"JWT"}' "${kid}" | b64url)
# Deliberately short-lived: the assertion only has to survive one round trip,
# and the access token it buys carries the real lifetime.
claims=$(printf '{"iss":"%s","sub":"%s","aud":"%s","iat":%d,"exp":%d}' \
    "${client_id}" "${client_id}" "${ISSUER}" "${now}" "$((now + 300))" | b64url)
signing_input="${header}.${claims}"

pem=$(mktemp)
trap 'rm -f "${pem}"' EXIT
(umask 077; jq -re .private_key "${KEY_FILE}" > "${pem}")

signature=$(printf '%s' "${signing_input}" | openssl dgst -sha256 -sign "${pem}" | b64url)
assertion="${signing_input}.${signature}"

response=$(curl -sS --fail-with-body -m 30 -X POST "${TOKEN_ENDPOINT}" \
    --data-urlencode "grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer" \
    --data-urlencode "assertion=${assertion}" \
    --data-urlencode "scope=${scope}" 2>&1) || {
    echo "sa-credentials-helper: token exchange failed: ${response}" >&2
    exit 1
}

token=$(printf '%s' "${response}" | jq -re .access_token 2>/dev/null) || {
    echo "sa-credentials-helper: no access_token in response" >&2
    exit 1
}

printf '%s' "${token}"
