#!/usr/bin/env python3
"""Stands up the runnable Daytona version of ../ec2-zero-trust-ssh/ -- the
"subway map" site, this daemon's own dashboard, and SSH, all served *from a
Daytona sandbox*, relayed out through this laptop: a Datum Cloud tunnel for
the site, a Datum Cloud tunnel for the dashboard, and a P2P peer tunnel for
SSH. Mechanically identical to ../tunnel-demo-daytona/setup_demo.py (same
daemon, same three legs) -- the only real differences are the page served
(the subway-map diagram instead of a placeholder) and the tunnel labels
(ec2demo-web/ec2demo-dashboard instead of tunnel-demo-web/tunnel-demo-
dashboard), so this can run *alongside* a live tunnel-demo-daytona sandbox
without either one stealing the other's Cloud tunnels.

The sandbox hosts everything itself and advertises all three over Datum
peer (P2P) tunnels using its own peer-only, fake-credentialed daemon (see
sandbox/fake-credentials-helper.sh -- peer tunnels are project-agnostic, so
this never needs a real Datum project). This laptop's home daemon is the
only thing that holds real Datum project credentials; it's a pure relay
here: `peer connect` to all three sandbox endpoints, then Cloud-tunnel the
site + dashboard legs out from their locally-bound ports. SSH skips the
Cloud tunnel and stays direct P2P.

Once both Cloud tunnels are up, this also live-edits the sandbox's page
(via the Daytona SDK's exec, which runs as root inside the sandbox) --
swapping the two SITE_HOSTNAME_PLACEHOLDER/DASHBOARD_HOSTNAME_PLACEHOLDER
strings in web/index.html for the real hostnames -- the same "prove real
administrative access, no restart needed" beat the static demo's own
README walks through by hand.

Does NOT tear anything down when it exits -- it's meant to leave a live,
browsable/SSH-able demo running. Run teardown_demo.py when done.

Reuses the already-running home daemon from ../daytona-poc/.home-connect
(configured with a real Datum Cloud project) for every step here. Start it
first (see ../daytona-poc/NOTES.md) if it isn't already running.
"""
import json
import socket
import subprocess
import sys
import time
from pathlib import Path

from daytona import CreateSandboxFromImageParams, Daytona, DaytonaConfig, Image

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent.parent
DAYTONA_POC_DIR = REPO_ROOT / "deploy" / "daytona-poc"
DAEMON_CONNECT_DIR = DAYTONA_POC_DIR / ".home-connect"
CLI = REPO_ROOT / "connect" / "connect-plugin" / (
    "datumctl-connect.exe" if sys.platform == "win32" else "datumctl-connect"
)
API_KEY_FILE = DAYTONA_POC_DIR / ".daytona-api-key"
STATE_FILE = HERE / ".demo-state.json"
SSH_KEY_DIR = HERE / ".demo-ssh"

WEB_TUNNEL_LABEL = "ec2demo-web"
DASHBOARD_TUNNEL_LABEL = "ec2demo-dashboard"


def setup_token() -> str:
    token_file = DAEMON_CONNECT_DIR / "daemon_auth" / "setup.token"
    if not token_file.exists():
        sys.exit(
            f"home daemon doesn't look like it's running -- {token_file} not found. "
            "Start it against .home-connect (configured with a real Datum Cloud "
            "project) first."
        )
    return token_file.read_text().strip()


def cli_json(*args: str, attempts: int = 1) -> dict:
    # Observed live (tunnel-demo-daytona): `tunnel api create` against this
    # shared/multi-tenant daemon has exited nonzero at least once despite the
    # tunnel actually having been created server-side -- looks like a
    # transient race, not a real rejection. Retry callers (create_cloud_tunnel
    # below) rather than trusting exit code alone; always print stderr on
    # failure so a genuine rejection is visible instead of a bare traceback.
    last_error: subprocess.CalledProcessError | None = None
    for attempt in range(1, attempts + 1):
        result = subprocess.run(
            [str(CLI), "--port", "47780", "--token", setup_token(), *args],
            capture_output=True,
            text=True,
        )
        if result.returncode == 0:
            return json.JSONDecoder().raw_decode(result.stdout)[0]
        print(f"cli_json {args} attempt {attempt}/{attempts} failed (exit {result.returncode}): {result.stderr.strip()}", file=sys.stderr)
        last_error = subprocess.CalledProcessError(result.returncode, args, result.stdout, result.stderr)
        if attempt < attempts:
            time.sleep(2)
    raise last_error


def save_state(state: dict) -> None:
    STATE_FILE.write_text(json.dumps(state, indent=2))


def find_tunnel_by_label(label: str) -> dict | None:
    for tunnel in cli_json("tunnel", "api", "list"):
        if tunnel["label"] == label:
            return tunnel
    return None


def create_cloud_tunnel(label: str, endpoint: str) -> tuple[str, str]:
    # Same stale-endpoint/duplicate-avoidance dance as
    # ../tunnel-demo-daytona/setup_demo.py's create_cloud_tunnel -- see that
    # file's comments for the full reasoning (bound ports are ephemeral, a
    # same-label tunnel from a prior run can have a stale --origin, etc).
    wanted = f"http://{endpoint}"
    tunnel = find_tunnel_by_label(label)
    if tunnel is not None and tunnel.get("endpoint") != wanted:
        print(f"existing tunnel for {label!r} ({tunnel['id']}) points at stale endpoint {tunnel.get('endpoint')!r} (want {wanted!r}) -- deleting and recreating")
        for cmd in (("tunnel", "api", "stop", tunnel["id"]), ("tunnel", "api", "delete", tunnel["id"])):
            try:
                cli_json(*cmd)
            except subprocess.CalledProcessError:
                pass
        tunnel = None
    if tunnel is not None:
        print(f"reusing existing tunnel for {label!r} ({tunnel['id']}) from a prior run")
    else:
        try:
            tunnel = cli_json("tunnel", "api", "create", "--label", label, "--origin", endpoint)
        except subprocess.CalledProcessError:
            tunnel = find_tunnel_by_label(label)
            if tunnel is None or tunnel.get("endpoint") != wanted:
                raise
            print(f"create for {label!r} reported failure but the tunnel exists ({tunnel['id']}) -- continuing")
    tunnel_id = tunnel["id"]

    def try_start() -> None:
        try:
            cli_json("tunnel", "api", "start", tunnel_id)
        except subprocess.CalledProcessError as exc:
            print(f"tunnel api start for {tunnel_id} reported failure ({exc}) -- checking progress anyway", file=sys.stderr)

    try_start()
    STABLE_READS_REQUIRED = 5
    consecutive_ready = 0
    for i in range(90):
        progress = cli_json("tunnel", "api", "progress", tunnel_id)
        hostnames = progress.get("hostnames") or []
        connector_ready = any(
            step.get("kind") == "connector_ready" and step.get("status") == "ready"
            for step in progress.get("steps", [])
        )
        if hostnames and connector_ready:
            consecutive_ready += 1
            if consecutive_ready >= STABLE_READS_REQUIRED:
                return tunnel_id, hostnames[0]
        else:
            consecutive_ready = 0
        if i == 20 and consecutive_ready == 0:
            print(f"tunnel {tunnel_id} ({label}) still not connector_ready after 20s -- re-issuing start", file=sys.stderr)
            try_start()
        time.sleep(1)
    sys.exit(f"tunnel {tunnel_id} ({label}) never became stably connector_ready")


def build_image_with_retry(dockerfile: Path, attempts: int = 3):
    last_error: Exception | None = None
    for attempt in range(1, attempts + 1):
        try:
            return Image.from_dockerfile(str(dockerfile))
        except Exception as exc:  # noqa: BLE001 -- genuinely want to retry any build failure
            last_error = exc
            print(f"image build attempt {attempt}/{attempts} failed: {exc}", file=sys.stderr)
            if attempt < attempts:
                time.sleep(5)
    raise RuntimeError(f"image build failed after {attempts} attempts") from last_error


def generate_ssh_keypair() -> tuple[Path, str]:
    SSH_KEY_DIR.mkdir(exist_ok=True)
    key_path = SSH_KEY_DIR / f"demo-{int(time.time())}"
    subprocess.run(
        ["ssh-keygen", "-t", "ed25519", "-N", "", "-f", str(key_path), "-C", "ec2demo-tunnel-demo"],
        capture_output=True,
        text=True,
        check=True,
    )
    return key_path, (key_path.with_suffix(".pub")).read_text().strip()


def connection_is_alive(bound_addr: str, attempts: int = 6, delay: float = 2.0) -> bool:
    host, port_str = bound_addr.rsplit(":", 1)
    port = int(port_str)
    for _ in range(attempts):
        time.sleep(delay)
        try:
            with socket.create_connection((host, port), timeout=3) as sock:
                sock.settimeout(3)
                sock.sendall(b"HEAD / HTTP/1.0\r\n\r\n")
                if sock.recv(1):
                    return True
        except OSError:
            continue
    return False


def peer_connect(ticket: str, name: str) -> dict:
    for outer in range(5):
        result: dict | None = None
        for i in range(10):
            try:
                result = cli_json(
                    "tunnel", "api", "peer", "connect",
                    "--ticket", ticket,
                    "--bind", "127.0.0.1:0",
                )
                break
            except subprocess.CalledProcessError:
                print(f"({name}: peer connect attempt {i + 1} failed, retrying...)", file=sys.stderr)
                time.sleep(1)
        if result is None:
            sys.exit(f"peer connect for {name} never succeeded")
        if connection_is_alive(result["bound_addr"]):
            return result
        print(f"({name}: connection {result['id']} came back dead -- disconnecting and trying a fresh one ({outer + 1}/5)...)", file=sys.stderr)
        try:
            cli_json("tunnel", "api", "peer", "disconnect", result["id"])
        except subprocess.CalledProcessError:
            pass
    sys.exit(f"peer connect for {name} never produced a live connection after several fresh attempts")


def live_edit_hostnames(sandbox, web_hostname: str, dashboard_hostname: str) -> None:
    # The demo's own "prove real administrative access" beat: reach into the
    # sandbox and swap the two placeholder strings in the already-live page
    # for the real hostnames -- no restart needed, it's a plain static file
    # server. Uses the Daytona SDK's exec (runs as root inside the sandbox,
    # same as entrypoint.sh itself) rather than the demo@ SSH login used
    # elsewhere in this script: /opt/datum/web/ is root-owned from the image
    # build (see sandbox/Dockerfile), so an in-place `sed -i` as the
    # unprivileged `demo` SSH user fails with "Permission denied" trying to
    # write its temp file there (confirmed live on the first run of this
    # script) -- the SSH connection itself is fine, it's just the wrong user
    # for this one file.
    remote_cmd = (
        f"sed -i "
        f"-e 's/SITE_HOSTNAME_PLACEHOLDER/{web_hostname}/' "
        f"-e 's/DASHBOARD_HOSTNAME_PLACEHOLDER/{dashboard_hostname}/' "
        f"/opt/datum/web/index.html"
    )
    response = sandbox.process.exec(remote_cmd, timeout=15)
    if response.exit_code != 0:
        print(f"live-edit of the sandbox page failed (exit {response.exit_code}): {response.result} -- the page will still show placeholder hostnames", file=sys.stderr)
    else:
        print("live-edited the sandbox's page with the real hostnames -- no restart needed")


def main() -> None:
    state: dict = {}

    print("=== [1/6] checking the home daemon is up ===")
    setup_token()

    print("=== [2/6] generating an ephemeral SSH keypair for this run ===")
    key_path, pubkey = generate_ssh_keypair()
    state["ssh_key_path"] = str(key_path)

    print("=== [3/6] building and creating the sandbox (hosts SSH, the subway-map page, and its own dashboard) ===")
    for binary in ("datum-connect-daemon", "datumctl-connect", "fake-credentials-helper.sh"):
        src = DAYTONA_POC_DIR / binary
        if not src.exists():
            sys.exit(f"missing {src} -- build it the same way ../daytona-poc/build.sh does")
        (HERE / "sandbox" / binary).write_bytes(src.read_bytes())
    sandbox_web_dir = HERE / "sandbox" / "web"
    sandbox_web_dir.mkdir(exist_ok=True)
    (sandbox_web_dir / "index.html").write_bytes((HERE / "web" / "index.html").read_bytes())

    api_key = API_KEY_FILE.read_text().strip()
    daytona = Daytona(DaytonaConfig(api_key=api_key))
    image = build_image_with_retry(HERE / "sandbox" / "Dockerfile")
    sandbox = daytona.create(
        CreateSandboxFromImageParams(
            image=image,
            env_vars={"SSH_PUBKEY": pubkey},
            auto_stop_interval=0,
        ),
        timeout=180,
        on_snapshot_create_logs=lambda log: print(f"[image build] {log}"),
    )
    state["sandbox_id"] = sandbox.id
    save_state(state)
    sandbox.update_network_settings(network_block_all=False)

    print("=== [4/6] running the entrypoint (starts daemon, sshd, web server, advertises all 3 over P2P) ===")
    response = sandbox.process.exec(
        "/opt/datum/entrypoint.sh",
        env={"SSH_PUBKEY": pubkey},
        timeout=180,
    )
    print(f"--- sandbox stdout (exit code {response.exit_code}) ---")
    print(response.result)
    if response.exit_code != 0:
        sys.exit("sandbox entrypoint failed -- see output above")

    marker = "TUNNEL_DEMO_TICKET_JSON: "
    ads: dict[str, dict] = {}
    for line in response.result.splitlines():
        if line.startswith(marker):
            ad = json.loads(line[len(marker):])
            ads[ad["name"]] = ad
    missing = {"ssh", "web", "dashboard"} - ads.keys()
    if missing:
        sys.exit(f"sandbox never advertised: {', '.join(sorted(missing))}")

    print("=== [5/6] connecting from here to all 3 sandbox endpoints over P2P ===")
    bound_ports: dict[str, str] = {}
    for name, ad in ads.items():
        state[f"{name}_resource_id"] = ad["resource_id"]
        save_state(state)
        connect_result = peer_connect(ad["ticket"], name)
        state[f"{name}_connection_id"] = connect_result["id"]
        bound_ports[name] = connect_result["bound_addr"].split(":")[-1]
        save_state(state)

    print("=== [6/6] relaying the site + dashboard out over Cloud tunnels ===")
    web_tunnel_id, web_hostname = create_cloud_tunnel(WEB_TUNNEL_LABEL, f"127.0.0.1:{bound_ports['web']}")
    state["web_tunnel_id"] = web_tunnel_id
    save_state(state)
    dashboard_tunnel_id, dashboard_hostname = create_cloud_tunnel(DASHBOARD_TUNNEL_LABEL, f"127.0.0.1:{bound_ports['dashboard']}")
    state["dashboard_tunnel_id"] = dashboard_tunnel_id
    ssh_port = bound_ports["ssh"]

    save_state(state)

    live_edit_hostnames(sandbox, web_hostname, dashboard_hostname)

    print()
    print("=== demo is live -- everything below is served from the Daytona sandbox ===")
    print(f"site:      https://{web_hostname}")
    print(f"dashboard: https://{dashboard_hostname}")
    print(f"ssh:       ssh -i {key_path} -p {ssh_port} demo@127.0.0.1")
    print()
    print(f"run teardown_demo.py when you're done (state saved to {STATE_FILE.name}).")


if __name__ == "__main__":
    main()
