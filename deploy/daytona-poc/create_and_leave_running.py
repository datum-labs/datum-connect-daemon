#!/usr/bin/env python3
"""Creates a sandbox from the same immutable image, injects a fresh ticket,
and *does not* delete it -- for interactive inspection via SSH or the
Daytona dashboard. See NOTES.md's "Immutable sandbox image POC" section.
"""
import sys
from pathlib import Path

from daytona import CreateSandboxFromImageParams, Daytona, DaytonaConfig, Image

HERE = Path(__file__).resolve().parent
API_KEY_FILE = HERE / ".daytona-api-key"

if len(sys.argv) != 2:
    print("usage: create_and_leave_running.py <ticket>", file=sys.stderr)
    sys.exit(1)
ticket = sys.argv[1]

api_key = API_KEY_FILE.read_text().strip()
daytona = Daytona(DaytonaConfig(api_key=api_key))

print("=== building image from Dockerfile ===")
image = Image.from_dockerfile(str(HERE / "Dockerfile"))

print("=== creating sandbox (left running -- not deleted) ===")
sandbox = daytona.create(
    CreateSandboxFromImageParams(image=image, env_vars={"SANDBOX_TICKET": ticket}),
    timeout=180,
    on_snapshot_create_logs=lambda log: print(f"[image build] {log}"),
)

print(f"sandbox id: {sandbox.id}")
print(f"sandbox state: {sandbox.state}")

print("=== generating SSH access (4 hour expiry) ===")
ssh_access = sandbox.create_ssh_access(expires_in_minutes=240)
print(f"SSH command: {ssh_access.ssh_command}")
print(f"expires_at: {ssh_access.expires_at}")

print()
print("Entrypoint NOT run automatically -- run it yourself over SSH to watch it live:")
print("  SANDBOX_TICKET=<the ticket> /opt/datum/entrypoint.sh")
print(f"(the ticket that was injected as this sandbox's env var: {ticket})")
