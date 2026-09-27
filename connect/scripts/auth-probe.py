"""Auth probe: answers "is the daemon's credential path healthy?" without a
tunnel, an inspector or the edge anywhere in the picture.

Three stages, reported separately, because they fail for different reasons and
the fix differs each time:

  1. HELPER  — can the credentials helper mint a token at all?
               (invoked exactly as connect-lib's exec_helper does)
  2. TOKEN   — is the token it returned actually fresh?
  3. API     — does the control plane accept it?

A 401 at stage 3 with a fresh token at stage 2 means the token is being
rejected, not stale. A stale token at stage 2 with a healthy helper at stage 1
means the refresh loop isn't swapping it in. Those are different bugs, and a
503 on a public hostname looks identical for both.
"""
import base64
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

HELPER = os.environ.get("DATUM_CREDENTIALS_HELPER", r"C:\datum\datumctl\datumctl.exe")
SESSION = os.environ.get("DATUM_SESSION", "brett.mertens@gmail.com@api.datum.net")
PROJECT = os.environ.get("DATUM_PROJECT", "demos-md21mk")
API = os.environ.get("DATUM_API_URL", "https://api.datum.net")

OK, FAIL = "PASS", "FAIL"
results = []


def report(stage, status, detail):
    results.append((stage, status, detail))
    print(f"[{status}] {stage}: {detail}")


def stage_helper():
    """Invoke the helper the same way connect-lib's exec_helper does."""
    try:
        p = subprocess.run(
            [HELPER, "auth", "get-token", "--session", SESSION],
            capture_output=True, text=True, timeout=60,
        )
    except FileNotFoundError:
        report("HELPER", FAIL, f"not found at {HELPER}")
        return None
    except subprocess.TimeoutExpired:
        report("HELPER", FAIL, "timed out after 60s")
        return None

    if p.returncode != 0:
        report("HELPER", FAIL, f"exit {p.returncode}: {p.stderr.strip()[:300]}")
        return None
    token = p.stdout.strip()
    if not token:
        report("HELPER", FAIL, "exit 0 but returned an empty token")
        return None
    report("HELPER", OK, f"minted a token ({len(token)} chars)")
    return token


def stage_token(token):
    """Decode the JWT's exp/iat. A token minted long ago that is already past
    expiry means the refresh path is not doing its job."""
    try:
        payload = token.split(".")[1]
        payload += "=" * (-len(payload) % 4)
        claims = json.loads(base64.urlsafe_b64decode(payload))
    except Exception as e:
        report("TOKEN", FAIL, f"could not decode JWT payload: {e}")
        return None

    now = int(time.time())
    exp, iat = claims.get("exp"), claims.get("iat")
    if exp is None:
        report("TOKEN", FAIL, "no exp claim")
        return claims
    remaining = exp - now
    age = f", minted {now - iat}s ago" if iat else ""
    if remaining <= 0:
        report("TOKEN", FAIL, f"ALREADY EXPIRED {-remaining}s ago{age}")
    else:
        report("TOKEN", OK, f"valid for {remaining}s ({remaining // 60}m){age}")
    return claims


def stage_api(token):
    """One authenticated call against the project control plane."""
    url = (
        f"{API}/apis/resourcemanager.miloapis.com/v1alpha1"
        f"/projects/{PROJECT}/control-plane/version"
    )
    req = urllib.request.Request(url, headers={"Authorization": f"Bearer {token}"})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            report("API", OK, f"HTTP {r.status} from the control plane")
            return
    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", "replace")[:300].replace("\n", " ")
        hint = {
            401: "token rejected — auth problem",
            403: "authenticated but not permitted — authz, not auth",
            404: "endpoint/project not found — check the project id",
        }.get(e.code, "")
        report("API", FAIL, f"HTTP {e.code} {hint}. Body: {body}")
    except Exception as e:
        report("API", FAIL, f"request failed (network/DNS/TLS, not auth): {e}")


print(f"helper  : {HELPER}")
print(f"session : {SESSION}")
print(f"project : {PROJECT}")
print(f"api     : {API}\n")

token = stage_helper()
if token:
    stage_token(token)
    stage_api(token)

print()
failed = [s for s, st, _ in results if st == FAIL]
if not failed:
    print("VERDICT: the credential path is healthy. A tunnel 503 is NOT auth.")
    sys.exit(0)
print(f"VERDICT: auth is broken at: {', '.join(failed)}")
sys.exit(1)
