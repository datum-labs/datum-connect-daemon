# 3-Tunnel Daytona Demo

A live, runnable demo built on top of `datum-connect-daemon`: an ephemeral
[Daytona](https://daytona.io) sandbox hosts a web page, its own dashboard,
and SSH, all three with **zero inbound firewall rules** — two are reached
through a Datum Cloud tunnel, one through a direct peer-to-peer (P2P)
tunnel. Unlike [`demos/ec2-zero-trust-ssh`](../ec2-zero-trust-ssh/), this is
the actual code — clone it, run two scripts, and it stands the whole thing
up for you.

## The story, in one line

A sandbox with no public IP and no open inbound ports serves a web page, a
management dashboard, and SSH — all three reachable from anywhere, relayed
out through a laptop that never hosts any of the content itself.

## What's running

- A Daytona sandbox running `datum-connect-daemon` with **fake, peer-only
  credentials** (see `sandbox/entrypoint.sh` and the daemon's
  `DATUM_PLUGIN_MODE=1` / `DATUM_CREDENTIALS_HELPER` env vars) — peer (P2P)
  tunnels don't need a real Datum Cloud project, so the sandbox image never
  holds one. It hosts `sshd` (:22), a static web page (`python3 -m
  http.server`, :8899), and its own daemon's browser dashboard (:47780),
  and advertises all three as peer tunnels over iroh.
- Your own machine's daemon — a real `datum-connect-daemon` instance
  configured with a real Datum Cloud project — acting as a pure **relay**:
  it `peer connect`s to all three sandbox endpoints, then opens a Cloud
  tunnel for the web page and the dashboard so each gets a public
  `*.datumproxy.net` hostname. SSH skips the Cloud tunnel entirely and
  stays a direct, hole-punched P2P connection from your machine to the
  sandbox.

Nothing here runs on your machine except the relay daemon itself — the web
page and dashboard you see in a browser are being served from inside the
sandbox the whole time.

## Setting it up

### Prerequisites

1. Build `datum-connect-daemon` and `datumctl-connect` per the main repo's
   [SETUP.md](../../SETUP.md) — you need **two** builds:
   - A native build for your own OS, to run `datumctl-connect` from
     `setup_demo.py`/`teardown_demo.py` below.
   - A Linux build of both binaries, copied into `deploy/daytona-poc/`
     (`../../deploy/daytona-poc/datum-connect-daemon` and
     `.../datumctl-connect`) — `../../deploy/daytona-poc/build.sh` shows the
     expected convention (cross-compile, then copy the two binaries there).
     `setup_demo.py` reads them from that path and bakes them into the
     sandbox image.
2. Start a home daemon instance against a real Datum Cloud project, with
   its connect-dir at `deploy/daytona-poc/.home-connect` (the same location
   [`deploy/daytona-poc`](../../deploy/daytona-poc/) already uses) — this is
   the relay daemon. It needs to already be running before either script
   below will work.
3. Get a [Daytona](https://daytona.io) API key and save it to
   `deploy/daytona-poc/.daytona-api-key` (plain text, no trailing newline
   needed).
4. Install the Daytona Python SDK: `pip install daytona`.
5. Make sure `ssh-keygen` is on your `PATH` (`setup_demo.py` generates a
   fresh ed25519 keypair for each run).

### Running it

```
python setup_demo.py
```

This builds the sandbox image, creates the sandbox, starts `sshd` + the web
server + the dashboard inside it, connects your relay daemon to all three
over P2P, opens the two Cloud tunnels, and prints a summary:

```
web page:  https://<random>.datumproxy.net
dashboard: https://<random>.datumproxy.net
ssh:       ssh -i <keyfile> -p <port> demo@127.0.0.1
```

Open both hostnames in a browser and try the SSH command — all three are
being served live from inside the sandbox. State is written to
`.demo-state.json` as the script progresses, so a mid-run failure still
leaves enough behind for teardown to clean up.

When you're done:

```
python teardown_demo.py
```

This revokes all three peer advertisements, deletes the sandbox, and
stops+deletes both Cloud tunnels.

## Things worth knowing before you run this live

- A Cloud tunnel's `hostnames` field is populated at **create** time, before
  the tunnel is actually live — a tunnel whose `start` never really landed
  can still have a hostname, serving bare 404s. Wait for the tunnel's
  `connector_ready` step instead of trusting `hostnames` alone.
- A single `connector_ready: ready` read can be a flicker, not a stable
  state — `setup_demo.py` requires 5 consecutive stable reads before
  trusting it, after seeing a one-off `ready` read followed by the hostname
  timing out for 90+ seconds.
- A tunnel left over from a previous run that didn't get torn down cleanly
  can still exist under the same label with a stale `--endpoint` — bound
  local ports are ephemeral and differ every run, and there's no `tunnel
  api update` to patch one in place. `setup_demo.py` deletes and recreates
  any same-label tunnel whose endpoint doesn't match what it wants, rather
  than reusing it as-is.
- A fresh `*.datumproxy.net` hostname can take up to a minute or so to
  resolve via DNS even once the tunnel itself is fully healthy
  (`connector_ready: true` and the local relay port already answering) —
  don't assume a `000`/connection-failed response on the first few tries
  means something is actually broken.
- Daytona's own auto-stop (15 minutes idle, by default) watches its own
  API/toolbox activity, which never sees this demo's traffic — all of it is
  P2P/relay traffic the Daytona control plane doesn't observe.
  `setup_demo.py` disables auto-stop on the sandbox it creates
  (`auto_stop_interval=0`) so it doesn't quietly stop itself mid-demo.
