# Datum Connect Daemon

A headless daemon that puts Datum Cloud tunnels — and direct peer-to-peer tunnels — behind one local HTTP API, so a desktop UI, the `datumctl` CLI, a browser dashboard, and AI agents can all be thin clients of the same running process instead of each reimplementing tunnel logic. Same shape as Tailscale's `tailscaled` + CLI + GUI.

> **Labs-tier, not for production.** This is an early, actively-evolving project out of [Datum Labs](https://github.com/datum-labs) — demos and tools for Datum Cloud, not hardened or supported the way the core platform is. Expect rough edges; see [Status](#status) for what's actually proven versus still aspirational.

## What it does

- **One daemon, many clients.** The daemon owns tunnel state and exposes it over a loopback-only HTTP+JSON API (`127.0.0.1`, never bound to a public interface). The `datumctl connect` CLI plugin and the built-in browser dashboard are both just clients of that API — anything else (an MCP server, a script) can be too.
- **Two kinds of tunnel.** A tunnel can go through Datum Cloud (an `HTTPProxy`/`Connector` pair gets you a real public hostname), or stay strictly peer-to-peer over [iroh](https://iroh.computer) with no Datum Cloud involvement at all.
- **Tiered local auth.** Three token tiers — setup, operate, viewer — so a human, an automated agent, and a read-only dashboard viewer each get exactly the access they need. See [API-REFERENCE.md](./API-REFERENCE.md).
- **Attributable agent actions.** Every tunnel records who last started it (which credential, not just "yes it's running"), so an agent-driven tunnel is always visible as such in the dashboard and the audit log — not a silent background action.
- **An L7 traffic inspector.** Captured request/response headers and bodies per tunnel, with known-sensitive headers redacted at capture time, plus one-click replay against the real local target.

## Status

**Working end-to-end**, live-verified against real Datum Cloud infrastructure:
- Full tunnel lifecycle (create/start/stop/delete) — through the local API directly and through the real `datumctl connect ...` CLI dispatch path.
- Setup/operate/viewer auth tiers, an append-only audit log, and per-tunnel auto-expiry.
- The L7 traffic inspector: capture, replay, sensitive-header redaction.
- Peer-to-peer (app-to-app) tunnels with zero Datum Cloud involvement.
- Crash/reboot recovery — the daemon auto-resumes previously-enabled tunnels — plus a systemd unit for Linux boot persistence.
- Native Windows build and run, alongside the original WSL2 path.
- The browser dashboard: tunnels, peers, traffic, and a live log tail.

**Not yet built:**
- An MCP server client (any HTTP client can already act as one against the API today — this is just the packaged version).
- Whole-subnet/VPC routing — the main gap versus Tailscale, Cloudflare, and ngrok.
- A lazy-auth path for peer-only use — today the daemon requires a working Datum Cloud credential to start at all, even if a given session never touches Datum Cloud. See [SETUP.md](./SETUP.md#auth).
- Broad automated test coverage — today it's unit tests plus manual verification scripts, not a standing integration suite.
- Proof beyond AWS — the systemd/cloud-deploy path has been verified end-to-end on AWS; GCP and Azure haven't been tried yet.

**Known limitation:** downloads through a Datum-proxied tunnel currently hit a connection cutoff around ~70MB. A chunked-resume workaround is suspected but not yet verified.

## Architecture

```
   datumctl CLI  ─┐
  browser dashboard├──► local HTTP API (loopback only) ──► datum-connect-daemon ──► iroh (QUIC)
   (future) MCP  ──┘                                              │
                                                                   ├─► Datum Cloud (HTTPProxy/Connector) → public hostname
                                                                   └─► direct peer connection → another daemon
```

- **`connect/connect-lib`** (Rust) — the daemon itself (`daemon/`), the shared tunnel/auth/dashboard logic (`lib/`), and the older single-shot CLI binary this project builds on top of (`bin/`).
- **`connect/connect-plugin`** (Go) — the `datumctl connect` plugin: process supervision, PID tracking, and the `tunnel api ...` / `tunnel daemon ...` command surface.
- **`docker/`** — a small demo web app (a property-viewing booking system) used purely as "something real to point a tunnel at." Not part of the daemon itself — see [docker/README.md](./docker/README.md).
- **`deploy/`** — a systemd unit for running the daemon as a Linux service, and a proof-of-concept for running it inside a [Daytona](https://daytona.io) sandbox for peer-only, Datum-agnostic use.

## Quickstart

See [SETUP.md](./SETUP.md) for full build/install instructions (WSL2, native Windows, and Linux). The short version, once built:

```bash
# start the daemon (auto-generates a setup token on first run)
./connect/connect-lib/target/debug/datum-connect-daemon --port 47780 &
TOKEN=$(cat "$DATUM_CONNECT_DIR/daemon_auth/setup.token")

# create, start, and hit a tunnel
CLI=./connect/connect-plugin/datumctl-connect
$CLI --port 47780 --token "$TOKEN" tunnel api create --label test --endpoint 127.0.0.1:8000
$CLI --port 47780 --token "$TOKEN" tunnel api start <id-from-above>
$CLI --port 47780 --token "$TOKEN" tunnel api progress <id>   # wait for a hostname to appear
curl https://<hostname-from-progress>/
```

Or open `http://127.0.0.1:47780/` for the browser dashboard and paste in the same setup token.

## Try it against something real

The `docker/` demo app is a disposable target to tunnel to instead of a bare `python -m http.server`. `docker compose up --build` in `docker/`, then point a tunnel at `127.0.0.1:3000`.

## License

AGPLv3 — see [LICENSE](./LICENSE). This repository also vendors a copy of [`datum-cloud/connect`](https://github.com/datum-cloud/connect) (also AGPLv3) under `connect/` — see [`connect/LICENSE`](./connect/LICENSE).
