# Zero-Trust SSH Demo

A template for a live demo built on top of `datum-connect-daemon`: a server
that's reachable and administrable with **zero inbound firewall rules** —
public HTTP traffic goes through Datum Cloud's own proxy, and SSH goes
through a direct peer-to-peer (P2P) tunnel. Neither path needs a hole
punched in the box's security group.

This is a generalized writeup with placeholders instead of any specific
box's real hostnames/IDs — see [index.html](./index.html) for the
accompanying "subway map" diagram that visualizes the same story on the demo
page itself.

## The story, in one line

Public traffic goes through Datum Cloud's proxy; SSH goes through a direct
P2P tunnel — both reach the box despite it having no open inbound ports at
all.

## What's running

- A server (any host — EC2 in the original run of this demo) running
  `datum-connect-daemon`.
- Two Cloud-proxied tunnels: one pointed at a small static demo page, one
  pointed at the daemon's own browser dashboard. Each gets a real public
  `*.datumproxy.net` hostname via an `HTTPProxy`/`Connector` pair — see the
  main [README](../../README.md#architecture) for how that works.
- One **peer (P2P) advertisement**, not a Cloud-proxied tunnel — the daemon
  advertising `127.0.0.1:22` (SSH) directly over iroh, no Datum Cloud
  involved. This is what shows up in the dashboard's "Peer tunnels" panel.

## Setting it up

1. Stand up `datum-connect-daemon` on your server, with two Cloud-proxied
   tunnels pointed at whatever you want the public demo to show (a site, the
   dashboard itself, etc.) — see [SETUP.md](../../SETUP.md).
2. Advertise SSH as a peer resource on the server's daemon:
   ```
   POST /v1/peers/advertise   { "endpoint": "127.0.0.1:22", "label": "demo-ssh" }
   ```
   (setup-token authenticated — see [API-REFERENCE.md](../../API-REFERENCE.md)).
3. On your own laptop, run a second daemon instance purely for the P2P side
   (`DATUM_PLUGIN_MODE=1`, its own `DATUM_CONNECT_DIR`), then consume the
   ticket from step 2:
   ```
   POST /v1/peers/connect   { "ticket": "<ticket from step 2>", "bind": "127.0.0.1:2222" }
   ```
4. `ssh -p 2222 you@127.0.0.1` now reaches the server over a direct,
   hole-punched connection. Confirm it in the dashboard's Peer Tunnels panel
   — you're looking for `"conn_type": "direct"`, not `"relay"`.
5. Once step 4 works, remove any temporary inbound SSH rule from the
   server's security group entirely. Direct `ssh <public-ip>` should now
   time out; `ssh -p 2222 you@127.0.0.1` should keep working unaffected.
   That contrast is the demo.

## Live script

1. **Show the public site** — open the demo site's `*.datumproxy.net` URL.
   No public IP, no port-forward, nothing internet-facing on the box itself
   for this traffic.
2. **Show the dashboard** — open the dashboard's `*.datumproxy.net` URL,
   same tunnel mechanism, but now it's the daemon's own management UI (the
   thing managing every tunnel here, including the one serving it). Fetch
   the setup token over the P2P tunnel to unlock it:
   ```
   ssh -p 2222 you@127.0.0.1 "cat ~/.datumctl-connect/daemon_auth/setup.token"
   ```
3. **Prove real administrative access** — live-edit the site over the P2P
   tunnel (e.g. `sed -i` a background color or headline in its `index.html`)
   and hard-refresh the public URL. No restart needed if it's served by a
   plain static file server.
4. **The reveal** — remove the inbound SSH rule from the security group
   entirely, live, in front of the audience.
5. **Prove it — direct SSH now fails**: `ssh <public-ip>` times out.
6. **Prove it — the P2P tunnel still works**: `ssh -p 2222 you@127.0.0.1`
   succeeds immediately, unaffected, because it never touched the security
   group in the first place.

## The webpage (`index.html`)

A static, hand-authored "subway map" diagram, meant to sit on whatever plain
page the public tunnel (step 1 above) points at. Two lines converge on a
single "EC2 instance" station: a green **P2P line** running straight to an
SSH stop with no intermediate stop, and a blue **Proxy line** running
through a "Datum Cloud Proxy" interchange before branching to the site and
dashboard stops. It's a direct visual echo of the "public traffic is gated,
admin traffic isn't" story above.

It's intentionally static — no calls to the daemon's API — so it's easy to
drop into any plain HTML page and hand-edit the two blocks marked
`EDIT ME` (the card copy, and the station labels/hostnames in the SVG) for
your own demo. A natural next step, once the visual design is settled for a
given demo, is swapping the hardcoded station data for a live fetch against
[`/v1/tunnels`](../../API-REFERENCE.md) and
[`/v1/peers`](../../API-REFERENCE.md) so the map reflects real-time state
instead of being hand-maintained.

## Things worth knowing before you run this live

- A **viewer token** can read full traffic/log body content across every
  tunnel on that daemon, not just status — by design, not a bug, but worth
  knowing before handing a viewer token to anyone during the demo (see
  [API-REFERENCE.md](../../API-REFERENCE.md)).
- Each tunnel has a **max-runtime auto-expiry** (`--max-tunnel-hours`,
  default 24h) — a daemon-wide setting, not per-tunnel. A tunnel that's
  meant to stay up through a multi-day demo will auto-stop unless this is
  raised, which also weakens the backstop for everything else on that
  daemon.
- Peer advertisements **do not survive the daemon process being replaced**
  (a rebuild/redeploy) — re-advertise after any such restart, and revoke
  stale ones (`DELETE /v1/peers/advertise/:resource_id`) so they don't pile
  up in the dashboard's Peer Tunnels panel.
- `advertise` mints a **new** resource on every call rather than upserting
  by label — safe to call repeatedly, but clean up duplicates afterward.
