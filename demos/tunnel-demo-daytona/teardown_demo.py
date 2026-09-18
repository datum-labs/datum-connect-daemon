#!/usr/bin/env python3
"""Tears down everything setup_demo.py stood up: revokes all three peer
advertisements (ssh, web, dashboard), deletes the Daytona sandbox, and
stops+deletes both Cloud tunnels. Reads .demo-state.json (written by
setup_demo.py) for the ids to act on.

Best-effort throughout -- one failed step (e.g. a sandbox already deleted
by hand) shouldn't stop the rest of cleanup from running.
"""
import json
import subprocess
import sys
from pathlib import Path

from daytona import Daytona, DaytonaConfig

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent.parent
DAYTONA_POC_DIR = REPO_ROOT / "deploy" / "daytona-poc"
DAEMON_CONNECT_DIR = DAYTONA_POC_DIR / ".home-connect"
CLI = REPO_ROOT / "connect" / "connect-plugin" / (
    "datumctl-connect.exe" if sys.platform == "win32" else "datumctl-connect"
)
API_KEY_FILE = DAYTONA_POC_DIR / ".daytona-api-key"
STATE_FILE = HERE / ".demo-state.json"


def setup_token() -> str:
    return (DAEMON_CONNECT_DIR / "daemon_auth" / "setup.token").read_text().strip()


def cli_json(*args: str) -> dict:
    result = subprocess.run(
        [str(CLI), "--port", "47780", "--token", setup_token(), *args],
        capture_output=True,
        text=True,
        check=True,
    )
    return json.JSONDecoder().raw_decode(result.stdout)[0]


def best_effort(description: str, fn) -> None:
    print(f"=== {description} ===")
    try:
        fn()
    except Exception as exc:  # noqa: BLE001 -- cleanup must keep going regardless
        print(f"  failed ({exc}) -- may need manual cleanup", file=sys.stderr)


def main() -> None:
    if not STATE_FILE.exists():
        sys.exit(f"no {STATE_FILE.name} found -- nothing to tear down (or already torn down)")
    state = json.loads(STATE_FILE.read_text())

    for name in ("ssh", "web", "dashboard"):
        connection_id = state.get(f"{name}_connection_id")
        if connection_id:
            best_effort(
                f"disconnecting the {name} peer connection",
                lambda cid=connection_id: cli_json("tunnel", "api", "peer", "disconnect", cid),
            )

        resource_id = state.get(f"{name}_resource_id")
        if resource_id:
            best_effort(
                f"revoking the {name} peer advertisement",
                lambda rid=resource_id: cli_json("tunnel", "api", "peer", "revoke", rid),
            )

    if state.get("sandbox_id"):
        def delete_sandbox() -> None:
            api_key = API_KEY_FILE.read_text().strip()
            daytona = Daytona(DaytonaConfig(api_key=api_key))
            daytona.get(state["sandbox_id"]).delete()

        best_effort("deleting the Daytona sandbox", delete_sandbox)

    for key, description in (
        ("web_tunnel_id", "stopping+deleting the web page Cloud tunnel"),
        ("dashboard_tunnel_id", "stopping+deleting the dashboard Cloud tunnel"),
    ):
        tunnel_id = state.get(key)
        if not tunnel_id:
            continue

        def stop_and_delete(tid: str = tunnel_id) -> None:
            cli_json("tunnel", "api", "stop", tid)
            cli_json("tunnel", "api", "delete", tid)

        best_effort(description, stop_and_delete)

    STATE_FILE.unlink()
    print()
    print("done -- verify with `tunnel api list` / `tunnel api peer list` / the Daytona console if in doubt.")


if __name__ == "__main__":
    main()
