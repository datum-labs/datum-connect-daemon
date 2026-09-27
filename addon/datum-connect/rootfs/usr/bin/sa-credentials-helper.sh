#!/usr/bin/env bash
# Credentials helper for service-account auth.
#
# The daemon shells out to DATUM_CREDENTIALS_HELPER with
# `auth get-token --session <session>` and reads a bearer token from stdout.
# That contract is defined by ExternalTokenSource::exec_helper in connect-lib.
#
# IMPORTANT, and the reason this is a thin script rather than inline in run.sh:
# the helper is re-invoked whenever the daemon observes a 401, so it must be
# able to return a *fresh* token rather than a cached one. A helper that always
# returns the same string produces an unrecoverable retry loop — see issue #12,
# where exactly that behaviour kept a tunnel down for 36 hours while every
# local signal reported healthy.
set -euo pipefail

KEY_FILE=/data/service-account.key

if [ ! -s "${KEY_FILE}" ]; then
    echo "sa-credentials-helper: no service account key at ${KEY_FILE}" >&2
    exit 1
fi

# TODO(1.4): exchange the service account credential for a short-lived access
# token against Datum's token endpoint, and print only the token on stdout.
# The exchange endpoint and request shape are the remaining unknown here; the
# rest of the add-on does not depend on how it is implemented, only that this
# script prints a currently-valid token and exits 0.
#
# Until that is wired, a pre-issued token in the key file is passed through so
# the rest of the add-on can be exercised end to end on the device.
cat "${KEY_FILE}"
