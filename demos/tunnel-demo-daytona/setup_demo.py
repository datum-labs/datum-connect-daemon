#!/usr/bin/env python3
"""Stands up the 3-tunnel demo -- a placeholder web page, this daemon's own
dashboard, and SSH -- all served *from a Daytona sandbox*, relayed out
through this laptop: a Datum Cloud tunnel for the web page, a Datum Cloud
tunnel for the dashboard, and a P2P peer tunnel for SSH.

The sandbox hosts everything itself and advertises all three over Datum
peer (P2P) tunnels using its own peer-only, fake-credentialed daemon (see
sandbox/fake-credentials-helper.sh -- peer tunnels are project-agnostic, so
this never needs a real Datum project). This laptop's home daemon is the
only thing that holds real Datum project credentials (whatever real Datum
Cloud project it's configured with); it's a pure relay here: `peer connect`
to all three sandbox endpoints, then Cloud-tunnel the web + dashboard legs
out from their locally-bound ports. SSH skips the Cloud tunnel and stays
direct P2P, same as before.

Unlike ../daytona-poc/run_demo.py, this does NOT tear anything down when it
exits -- it's meant to leave a live, browsable/SSH-able demo running. Run
teardown_demo.py when you're done to revoke all three peer advertisements,
delete the sandbox, and stop both Cloud tunnels.

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
    # Observed live: `tunnel api create` against this shared/multi-tenant
    # daemon has exited nonzero at least once despite the tunnel actually
    # having been created server-side (confirmed via `tunnel api list`
    # immediately after) -- looks like a transient race, not a real
    # rejection. Retry callers (create_cloud_tunnel below) rather than
    # trusting exit code alone; always print stderr on failure so a genuine
    # rejection is visible instead of a bare traceback.
    last_error: subprocess.CalledProcessError | None = None
    for attempt in range(1, attempts + 1):
        result = subprocess.run(
            [str(CLI), "--port", "47780", "--token", setup_token(), *args],
            capture_output=True,
            text=True,
        )
        if result.returncode == 0:
            # Some subcommands (peer advertise) print a trailing
            # plain-language note after the JSON block -- raw_decode parses
            # just the leading JSON value.
            return json.JSONDecoder().raw_decode(result.stdout)[0]
        print(f"cli_json {args} attempt {attempt}/{attempts} failed (exit {result.returncode}): {result.stderr.strip()}", file=sys.stderr)
        last_error = subprocess.CalledProcessError(result.returncode, args, result.stdout, result.stderr)
        if attempt < attempts:
            time.sleep(2)
    raise last_error


def save_state(state: dict) -> None:
    # Written incrementally (after every resource is created, not just once
    # at the end) so a mid-run crash -- e.g. the `tunnel api start` timeout
    # hit live -- leaves enough behind for teardown_demo.py to clean up
    # instead of orphaning sandboxes/peers/tunnels with nothing tracking them.
    STATE_FILE.write_text(json.dumps(state, indent=2))


def find_tunnel_by_label(label: str) -> dict | None:
    for tunnel in cli_json("tunnel", "api", "list"):
        if tunnel["label"] == label:
            return tunnel
    return None


def create_cloud_tunnel(label: str, endpoint: str) -> tuple[str, str]:
    # `tunnel api create` has exited nonzero at least once against this
    # shared daemon despite the tunnel actually landing server-side (seen
    # live, confirmed via `tunnel api list` right after) -- so on failure,
    # check whether it landed anyway before treating this as a real error.
    # Never blindly retry the raw create call: that's what produced two
    # duplicate "tunnel-demo-dashboard" tunnels the first time this ran.
    #
    # Also seen live: a tunnel from a *previous run that never got torn
    # down* can still exist under this same label with a stale --endpoint --
    # bound ports are ephemeral and differ every run. There's no `tunnel api
    # update`, so a stale endpoint can't be patched in place; reusing it
    # as-is silently points the public hostname at a port nothing is
    # listening on anymore (confirmed live: both web + dashboard hostnames
    # came back 502/reset because this exact thing happened). Only reuse a
    # same-label tunnel whose endpoint already matches what we want;
    # otherwise delete it and create fresh.
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
            tunnel = cli_json("tunnel", "api", "create", "--label", label, "--endpoint", endpoint)
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
            # Seen live: the CLI call itself timed out ("context deadline
            # exceeded") against this shared daemon. Sometimes the start
            # goes through server-side anyway despite the failed round-trip
            # (confirmed via `tunnel api list` showing programmed:true right
            # after) -- but also seen live, in the SAME failure mode, a case
            # where it genuinely never landed at all (confirmed via the
            # daemon's own audit log: a `create` event with no matching
            # `start` event). Can't tell which happened from the CLI's exit
            # code alone -- fall through and let the progress poll below
            # decide, re-issuing start if it doesn't come up.
            print(f"tunnel api start for {tunnel_id} reported failure ({exc}) -- checking progress anyway", file=sys.stderr)

    try_start()
    STABLE_READS_REQUIRED = 5
    consecutive_ready = 0
    for i in range(90):
        progress = cli_json("tunnel", "api", "progress", tunnel_id)
        hostnames = progress.get("hostnames") or []
        # `hostnames` is assigned at *create* time, before the tunnel is
        # ever started -- confirmed live: a tunnel whose `start` genuinely
        # never landed server-side (no `start` event in the audit log at
        # all) still had a hostname here, serving bare 404s, while its
        # `connector_ready` step stayed "pending" / "Connector lease has
        # expired. Agent may be offline." `connector_ready: ready` is the
        # actual signal that traffic will be forwarded.
        connector_ready = any(
            step.get("kind") == "connector_ready" and step.get("status") == "ready"
            for step in progress.get("steps", [])
        )
        # A single `connector_ready: ready` read can be a flicker, not a
        # stable state -- confirmed live: a poll loop like this one (without
        # this consecutive-reads check) returned on one `ready` read, yet a
        # `tunnel api get` moments later showed `connector_ready: false` and
        # the public hostname timed out for 90+s. Manually re-starting and
        # requiring ~20 consecutive stable `true` reads over ~40s was what
        # actually fixed it, so require several in a row here too before
        # trusting it.
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
        ["ssh-keygen", "-t", "ed25519", "-N", "", "-f", str(key_path), "-C", "tunnel-demo"],
        capture_output=True,
        text=True,
        check=True,
    )
    return key_path, (key_path.with_suffix(".pub")).read_text().strip()


def connection_is_alive(bound_addr: str, attempts: int = 6, delay: float = 2.0) -> bool:
    # A raw TCP probe, not just "did the socket connect" -- observed live:
    # a `peer connect` can report success with a bound_addr that accepts
    # TCP connections but resets them the instant real traffic flows (the
    # underlying iroh session behind it never actually came up). Sending a
    # harmless HTTP request works as a generic liveness check across all 3
    # targets here: sshd sends its banner unprompted regardless of what we
    # send, and the two HTTP targets (web page, dashboard) answer a HEAD.
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
    # Seen live: `peer connect` against this shared daemon can report success
    # (valid id/bound_addr) for a connection that's actually dead-on-arrival
    # -- e.g. resets on first real traffic -- while a brand-new `peer
    # connect` call against the very same ticket, given a few seconds to
    # settle, comes up fine. So don't just trust the CLI's success response:
    # verify the connection actually carries traffic, and if it doesn't,
    # throw it away and get a fresh one rather than retrying the same dead
    # connection.
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


def main() -> None:
    state: dict = {}

    print("=== [1/6] checking the home daemon is up ===")
    setup_token()  # exits with a clear error if not

    print("=== [2/6] generating an ephemeral SSH keypair for this run ===")
    key_path, pubkey = generate_ssh_keypair()
    state["ssh_key_path"] = str(key_path)

    print("=== [3/6] building and creating the sandbox (hosts SSH, the web page, and its own dashboard) ===")
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
            # Daytona's default auto-stop (15 min idle) watches its own
            # toolbox/API activity, which sees none of this demo's traffic --
            # sshd, the web server, and the peer-tunnel daemon are all reached
            # directly over P2P, never through the Daytona API. Without this,
            # the sandbox silently stops itself mid-demo (confirmed live: hit
            # SANDBOX_NOT_RUNNING partway through leaving it up for a browse).
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
        # Generous headroom, matching ../daytona-poc/run_demo.py's proven
        # number -- a tighter timeout there caused a real 408
        # PROCESS_EXECUTION_TIMEOUT against Daytona's cross-network P2P
        # handshake, and this entrypoint does the same daemon-startup wait
        # plus its own sshd + web server + 3x peer-advertise retries on top.
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

    print("=== [6/6] relaying the web page + dashboard out over Cloud tunnels ===")
    web_tunnel_id, web_hostname = create_cloud_tunnel("tunnel-demo-web", f"127.0.0.1:{bound_ports['web']}")
    state["web_tunnel_id"] = web_tunnel_id
    save_state(state)
    dashboard_tunnel_id, dashboard_hostname = create_cloud_tunnel("tunnel-demo-dashboard", f"127.0.0.1:{bound_ports['dashboard']}")
    state["dashboard_tunnel_id"] = dashboard_tunnel_id
    ssh_port = bound_ports["ssh"]

    save_state(state)

    print()
    print("=== demo is live -- everything below is served from the Daytona sandbox ===")
    print(f"web page:  https://{web_hostname}")
    print(f"dashboard: https://{dashboard_hostname}")
    print(f"ssh:       ssh -i {key_path} -p {ssh_port} demo@127.0.0.1")
    print()
    print(f"run teardown_demo.py when you're done (state saved to {STATE_FILE.name}).")


if __name__ == "__main__":
    main()
