#!/bin/sh
# Fake credentials helper baked into the immutable sandbox image.
#
# The daemon requires *some* DATUM_CREDENTIALS_HELPER to boot at all, even
# for peer-only use that never touches Datum Cloud (see NOTES.md's "App-to-
# app tunnels" section for the technical reason -- ExternalTokenSource::
# from_env() unconditionally fetches a token at startup). This image only
# ever does peer tunneling, so a locally-generated, well-formed-but-fake
# JWT is deliberately used here instead of a real Datum project credential
# -- there is no real Datum project this image could authenticate to
# anyway, by design (that's the point: it's Datum-agnostic and carries no
# secrets). Real ("lazy auth") fix stays separately tracked, not attempted
# here. Ignores whatever args datumctl passes (auth get-token --session X)
# and always returns the same shape of token.
#
# Far-future expiry so the daemon's own refresh-loop never has a reason to
# re-invoke this mid-run.
exp=4102444800  # 2100-01-01, arbitrary "never" for a short-lived sandbox

b64url() {
  # standard base64, then convert to the URL-safe alphabet and strip padding
  base64 | tr -d '=' | tr '/+' '_-' | tr -d '\n'
}

header=$(printf '{"alg":"HS256","typ":"JWT"}' | b64url)
payload=$(printf '{"exp":%d,"sub":"daytona-sandbox"}' "$exp" | b64url)

printf '%s.%s.fake_sig\n' "$header" "$payload"
