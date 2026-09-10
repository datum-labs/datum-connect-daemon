#!/usr/bin/env python3
"""Orchestrates the Daytona immutable-image POC end to end.

See NOTES.md's "Immutable sandbox image POC" section for the full design.
Run this from the same machine that hosts the home daemon and broker (the
AWS instance, for now) so it can mint tickets locally via the CLI and
inspect the broker's own data log directly, without needing to SSH back
to itself.

Every API shape used here (DaytonaConfig, CreateSandboxFromImageParams,
Image.from_dockerfile, Sandbox.process.exec) was verified against the
actual installed `daytona` package (0.207.1), not assumed from docs.
"""
import json
import subprocess
import sys
import time
from pathlib import Path

from daytona import CreateSandboxFromImageParams, Daytona, DaytonaConfig, Image

HERE = Path(__file__).resolve().parent
DAEMON_CONNECT_DIR = Path.home() / ".datumctl-connect"
CLI = Path.home() / "datum-connect-daemon" / "connect" / "connect-plugin" / "datumctl-connect"
BROKER_PORT = 18081
DATA_LOG = HERE / "sandbox_data_log.jsonl"
API_KEY_FILE = HERE / ".daytona-api-key"


def setup_token() -> str:
    return (DAEMON_CONNECT_DIR / "daemon_auth" / "setup.token").read_text().strip()


def cli_json(*args: str) -> dict:
    result = subprocess.run(
        [str(CLI), "--port", "47780", "--token", setup_token(), *args],
        capture_output=True,
        text=True,
        check=True,
    )
    # Some subcommands (peer advertise, viewer-token create) print a
    # friendly trailing note after the JSON block -- raw_decode parses
    # just the leading JSON value and ignores whatever follows it.
    return json.JSONDecoder().raw_decode(result.stdout)[0]


def advertise_fresh(label: str) -> dict:
    # A fresh `peer advertise` call mints a genuinely new resource_id/ticket
    # every time -- this is what makes "one credential per sandbox run" real
    # rather than one shared ticket reused across every launch.
    return cli_json(
        "tunnel", "api", "peer", "advertise",
        "--endpoint", f"127.0.0.1:{BROKER_PORT}",
        "--label", label,
    )


def revoke(resource_id: str) -> None:
    cli_json("tunnel", "api", "peer", "revoke", resource_id)


def data_log_line_count() -> int:
    if not DATA_LOG.exists():
        return 0
    return sum(1 for _ in DATA_LOG.open())


def main() -> None:
    run_label = f"sandbox-run-{int(time.time())}"
    print(f"=== [1/6] advertising a fresh, single-use ticket: {run_label} ===")
    ad = advertise_fresh(run_label)
    resource_id = ad["resource_id"]
    ticket = ad["ticket"]
    print(f"resource_id={resource_id}")

    lines_before = data_log_line_count()

    api_key = API_KEY_FILE.read_text().strip()
    daytona = Daytona(DaytonaConfig(api_key=api_key))

    print("=== [2/6] building the sandbox image from our Dockerfile ===")
    image = Image.from_dockerfile(str(HERE / "Dockerfile"))

    print("=== [3/6] creating the sandbox -- this runs on Daytona's infrastructure, not here ===")
    sandbox = daytona.create(
        CreateSandboxFromImageParams(image=image, env_vars={"SANDBOX_TICKET": ticket}),
        timeout=180,
        on_snapshot_create_logs=lambda log: print(f"[image build] {log}"),
    )

    try:
        print("=== [4/6] running the entrypoint inside the sandbox ===")
        response = sandbox.process.exec(
            "/opt/datum/entrypoint.sh",
            env={"SANDBOX_TICKET": ticket},
            # Generous headroom over entrypoint.sh's own worst case (15s
            # daemon-startup wait + up to 10 retries * (5s timeout + 2s
            # sleep) for the config fetch) -- a tighter number here caused
            # a real 408 PROCESS_EXECUTION_TIMEOUT the first time this ran
            # for real against Daytona's cross-network P2P handshake.
            timeout=180,
        )
        print(f"--- sandbox stdout (exit code {response.exit_code}) ---")
        print(response.result)

        if response.exit_code != 0:
            print("sandbox entrypoint failed", file=sys.stderr)
            sys.exit(1)
    finally:
        print("=== [5/6] destroying the sandbox ===")
        sandbox.delete()

    # The real proof, not the sandbox's own say-so: did home's own log grow.
    lines_after = data_log_line_count()
    print(f"=== [6/6] independent verification: home data log grew from {lines_before} to {lines_after} lines ===")
    if lines_after <= lines_before:
        print("no new data landed on the home side -- treat this run as unverified despite exit code 0", file=sys.stderr)
        sys.exit(1)

    last_line = json.loads(DATA_LOG.read_text().splitlines()[-1])
    print("last record:", json.dumps(last_line, indent=2))

    print(f"revoking {resource_id} now that this run is done")
    revoke(resource_id)
    print("done.")


if __name__ == "__main__":
    main()
