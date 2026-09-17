#!/usr/bin/env python3
"""Standalone test: does network_block_all=False actually open egress?

Unlike run_demo.py, this does NOT need to run on the AWS home instance --
it doesn't mint a CLI ticket and doesn't read the broker's local data log.
All it needs is a Daytona API key. It creates a bare sandbox, lifts the
default block-all rule, and curls out to iroh's relay/discovery hosts from
inside the sandbox to see whether the fix actually works, in isolation
from the rest of the call-home plumbing.

Usage: set DAYTONA_API_KEY env var (or drop the key in .daytona-api-key
next to this file), then `python test_egress.py`.
"""
import os
import sys
from pathlib import Path

from daytona import CreateSandboxFromImageParams, Daytona, DaytonaConfig, Image

HERE = Path(__file__).resolve().parent
API_KEY_FILE = HERE / ".daytona-api-key"

TARGETS = [
    "https://use1-1.relay.n0.iroh.link",
    "https://dns.iroh.link",
]


def api_key() -> str:
    if os.environ.get("DAYTONA_API_KEY"):
        return os.environ["DAYTONA_API_KEY"]
    if API_KEY_FILE.exists():
        return API_KEY_FILE.read_text().strip()
    sys.exit("no API key: set DAYTONA_API_KEY or create .daytona-api-key next to this script")


def main() -> None:
    daytona = Daytona(DaytonaConfig(api_key=api_key(), api_url="https://app.daytona.io/api"))

    print("=== [1/4] building a minimal debian+curl image ===")
    # python_version is pinned explicitly -- debian_slim() otherwise infers
    # it from the interpreter running this script, and Daytona's base images
    # only go up to 3.13 (this machine runs 3.14).
    image = Image.debian_slim(python_version="3.12").run_commands(
        "apt-get update", "apt-get install -y curl"
    )

    print("=== [2/4] creating the sandbox on Daytona's infrastructure ===")
    sandbox = daytona.create(
        CreateSandboxFromImageParams(image=image),
        timeout=180,
        on_snapshot_create_logs=lambda log: print(f"[image build] {log}"),
    )

    try:
        print("=== [3/4] lifting the default block-all outbound rule ===")
        sandbox.update_network_settings(network_block_all=False)

        print("=== [4/4] curling iroh's relay/discovery hosts from inside the sandbox ===")
        ok = True
        for url in TARGETS:
            cmd = f"curl -sS -m 10 -o /dev/null -w 'HTTP %{{http_code}} in %{{time_total}}s\\n' {url}"
            response = sandbox.process.exec(cmd, timeout=20)
            print(f"{url} -> exit={response.exit_code} {response.result.strip()}")
            if response.exit_code != 0:
                ok = False

        if not ok:
            print("at least one target was unreachable -- network_block_all=False did not fully open egress", file=sys.stderr)
            sys.exit(1)
        print("egress confirmed working")
    finally:
        print("destroying the sandbox")
        sandbox.delete()


if __name__ == "__main__":
    main()
