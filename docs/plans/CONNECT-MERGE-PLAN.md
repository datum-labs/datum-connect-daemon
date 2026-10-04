# Plan: merge Scot's networking core with Brett's feature layer

Drafted 2026-10-02, updated 2026-10-04. Status: **Scot agreed to the split and to 2 of the 3 open decisions (see "Decisions with Scot").** No code has been written yet.

## The decision being implemented

Going forward there are two projects:

| Project | Owner | Owns |
|---|---|---|
| **datum-cloud/connect** (`feat/connect-daemon`) | Scot | Core networking: transport (MASQUE / CONNECT-TCP / CONNECT-UDP / CONNECT-IP over iroh), IP adapters (TUN/utun/Wintun), control-plane enrollment and peer policy, the daemon runtime, and the local API's auth primitives |
| **datum-labs/datum-connect-daemon** | Brett | Features, dashboards, integrations: dashboard and network map, L7 inspector and replay, log tail, tunnel notes, visible-signal and audit presentation, the HA add-on, Android, Daytona and SSH demos, and the user-facing CLI commands for those features |

Brett's project consumes Scot's as a **pinned library dependency**. It no longer contains its own networking code.

## Decisions with Scot (2026-10-04)

| # | Decision | Outcome |
|---|---|---|
| 1 | Integration style | **Option C (embedded `connect-runtime`) is agreed, on two conditions:** (1) it results in **one canonical daemon/runtime**, and (2) **policy, identity, state and reconciliation stay enforced inside that runtime**. Our extension hooks can add features but can't bypass or re-implement any of that. **Option B (separate process over his API) is fine as the interim path** until the runtime split lands |
| 2 | Plugin | **One `datumctl connect`.** Scot prefers folding our dashboard and inspector surfaces into his plugin over shipping two plugins. Our feature commands go there as PRs Scot reviews |
| 3 | Ticket-style cross-project sharing | **Still open** |

Other updates from Scot:
- **VPC gateway API.** Scot now has an API endpoint that deploys a gateway inside a VPC, to support VPN (CONNECT-IP) tunnels. That's the platform half of GVPC landing, which we'd been planning separately.
- **Security.** Scot confirmed the findings we sent him privately and is tracking the fixes in core.

## What the evaluation found

### Where the two repos stand

- **There is no shared git history, but there is a shared ancestor.** Our 2026-09-10 "initial public release" is a copy of Scot's commit `84a902e` (2026-08-02) plus our daemon. 69 of our 98 files are byte-identical to that commit.
- **Since that fork, Scot rebuilt the networking:** +50k/−7.7k lines, mostly in one commit `106ed3f` on 2026-10-01. That commit adds new `transport`, `ip-adapter` and `daemon` crates, the `successor` control plane, iroh 0.95 → 1.0, and a rewritten plugin.
- **Since that fork, we built the feature layer:** +4.1k/−0.9k lines under `connect/`, covering the daemon features, React dashboard, network map, notes, inspector fixes and HA add-on.
- **Neither side has the other's work.** Our dashboard, inspector, notes and map have no equivalent in his tree. His transport, CONNECT-IP and successor control plane have no equivalent in ours.
- **Some concepts overlap but share no code:** a token-tier local API, an audit log, and a crate named `datum-connect-daemon` (written independently on each side).
- **This is a port onto his core, not a 3-way merge.**

### Why his networking is better (confirmed in the code, not just asserted)

| Area | Ours (iroh 0.95) | Scot's (iroh 1.0) |
|---|---|---|
| Wire protocol | `iroh-http-proxy/1`, HTTP/1.1 CONNECT inside the external crate `iroh-proxy-utils` (pinned git rev). We own no wire code | HTTP/3 over iroh: CONNECT-TCP, CONNECT-UDP with HTTP datagrams, and RFC 9484-shaped CONNECT-IP with capsules and MTU checks. All of it is in his clean `connect-transport` crate |
| UDP | None | Yes, but no fragmentation, so payloads over 1100 bytes are dropped |
| Whole-network / VPN | None (GVPC was only a plan) | CONNECT-IP plus native TUN on Linux, macOS and Windows |
| Peer authorization | Coarse. It's per advertisement, not per peer key, and there's a known authorization gap (tracked separately) | Per-destination, per-peer-key policy, with **live revocation** (sessions that lose authorization are cancelled) |
| Identity | A fresh control key is minted on every start, and key storage needs hardening | One key per project, written atomically with private permissions, plus a Windows DACL |
| Control plane | kube client, `generateName` connectors, heartbeat that retries auth failures forever (issue #12) | Raw REST with UID pinning, ownership checks and idempotent create; fails closed on errors |
| Cancellation / timeouts | Ad hoc | `CancellationToken` everywhere, with bounded setup timeouts |
| Tests | About 94 Rust unit tests, **and no CI runs `cargo test`** (our workflows sit in a subdirectory GitHub ignores) | Real two-endpoint loopback tests for transport, a daemon API test suite, and a Python e2e against a fake control plane |

Adopting his core also **closes our known peer-authorization and key-storage gaps** at no extra cost.

### What his core still lacks (to raise with Scot, not to fix ourselves)

1. **One QUIC connection per TCP flow.** Every local socket does a fresh handshake, hole-punch and relay hop. That's fine for SSH and painful for browsers and dashboards that open 6+ connections at once.
2. **The policy refresh is a 30 s poll.** Granting or revoking takes up to 30 s. Worse, **any control-plane error calls `fail_closed`**, which drops every live session, so one API 5xx is an outage.
3. **No event stream and an untyped status API.** `transport` and `networks` in `/v1/status` are `serde_json::Value`, and everything has to be polled.
4. **Peer diagnostics cover outbound dials only.** For inbound peers there is no path, RTT or byte counts, which the network map needs.
5. **`DestinationId` is the port only** (`tcp-443`), so two services on the same port but different hosts collide.
6. **Admission control.** Add per-peer connection limits and an earlier policy check (details shared with Scot directly).
7. **Prod relays are n0's.** Datum relays are used only on staging. (Ours has `datumconnect.net` relays plus a startup latency probe.)
8. **The default "private" access is the whole project.** Per-device, least-privilege identity isn't done yet.
9. **Windows hasn't been validated on real hardware.** OIDC login isn't supported on Windows (credentials file only). Releases aren't signed.
10. **The daemon is a binary, not a library.** `Transport::bind` hard-codes the n0 preset, and there's no hook for a feature layer to add routes or sit in the data path.
11. **About 10k lines of legacy library code** (old `tunnels.rs`, `heartbeat.rs`, `iroh-proxy-utils`) are still in his tree.

Items 3, 4 and 10 block the feature layer. The rest are core-quality issues for Scot's backlog.

## Hard constraint: the platform gateway

**The platform gateway still speaks only the old protocol.** Our public tunnels today go `<name>.datumproxy.net` → Datum gateway → our connector over `iroh-http-proxy/1`. Scot's README says `serve --public` needs the platform to implement the new MASQUE gateway contract, and that hasn't shipped.

**Consequence:** we can't delete the `iroh-proxy-utils` path until the platform gateway speaks `datum-connect/masque-v1`. The merge has to run **both paths side by side** for a while:
- the legacy path for public HTTPS tunnels,
- MASQUE for peer-to-peer, private and VPC traffic.

We need to find out from the platform/NSO team whether that gateway work is scheduled, and when.

## Target architecture

```
datum-labs/datum-connect-daemon (Brett)                datum-cloud/connect (Scot)
┌───────────────────────────────────────────┐          ┌──────────────────────────────┐
│ dashboard (React/datum-ui)                │          │ connect-transport            │
│ feature routes: inspector, replay, logs,  │  depends │ connect-ip-adapter           │
│   notes, map data, visible-signal         │ ───────► │ connect-control (successor)  │
│ integrations: HA add-on, Android, demos   │  (pinned │ connect-runtime  ◄── new lib │
│ legacy-gateway adapter (until platform)   │   tag)   │   (reconcile, policy, auth,  │
│ binary: datum-connect-daemon              │          │    status, events)           │
└───────────────────────────────────────────┘          │ bin: headless daemon + plugin│
                                                       └──────────────────────────────┘
```

**Agreed integration style: embed his runtime as a library (Option C below). Option B is the interim path.**
- Scot splits his `daemon` crate into a `connect-runtime` library plus a thin binary.
- Our daemon builds his runtime and mounts his `/v1` router under its own routes.
- Our feature routes and the dashboard are added on top of that.
- The result is one process, one install and one port.
- The boundary is a Rust API plus his `/v1` HTTP API, both versioned by tag.

| Option | How | For | Against |
|---|---|---|---|
| A. Library pick-and-mix | We depend on `connect-transport` and `successor` only, and keep our own daemon runtime | Least work for Scot | We'd re-implement his reconcile, policy and auth, so the two projects end up with two networking runtimes again. That defeats the split |
| B. Two processes | His daemon runs unchanged; ours becomes a companion app using his HTTP API | Cleanest ownership, no Rust coupling | Two installs and two services. The inspector can't hook the data path except as a loopback hop. Everything polls. HA/Android packaging gets harder |
| **C. Embedded runtime** | His runtime as a library with a router and an extension API; ours is the binary users run | One process; clear boundary; features get events and diagnostics in-process | Scot has to carve out a library API (backlog item 10) |

**Interim: B, until `connect-runtime` exists.** His daemon does MASQUE serve/dial/networks. Ours keeps running the legacy gateway path and the feature layer, and the dashboard reads both APIs. This needs only items 3 and 4 from his backlog, and it lets us start before the runtime split.

## Feature-by-feature mapping

| Our feature | Today | After the merge |
|---|---|---|
| Public HTTPS tunnels (HTTPProxy + gateway) | `ListenNode` + `iroh-proxy-utils` | **Kept on the legacy adapter**, isolated in one module, until the platform MASQUE gateway ships. Then it moves to his `serve --public` |
| Peer tunnels (tickets) | `peer.rs`, n0 relays, no Datum | His `serve` + `dial` with private policy or an explicit `allow` list. **Open question: should we keep "anyone with a ticket, cross-project, no Datum account"?** His model needs an enrolled Connector in the same project. Options: drop it, or add a ticket → allow-key mechanism in core |
| Dialing Scot's/others' connectors | Not possible | Native, via his `POST /v1/dials`. The earlier Scot-connector dial plan (Phases 1–2) folds into this plan |
| CONNECT-IP / GVPC landing | Plan only | Native, via his `POST /v1/networks` plus ip-adapter. **Scot already has an API endpoint that deploys a VPC gateway for VPN tunnels**, so the client side can target it directly. The GVPC edge landing plan gets rebased on it. The network map shows VPC gateways as their own line |
| Token tiers and audit | Ours: setup / per-tunnel operate / global viewer | **Use his** (`Setup/Operate/Viewer`, scoped `project\|service\|dial`, expiry, revocation). Port our viewer-token UX and `last_actor` visible-signal on top. Our `/v1/tunnels/:id/tokens` becomes a thin alias, then is retired |
| L7 inspector and replay | HTTPProxy backend → inspector port → target | **Keep, as an opt-in loopback hop.** The serve target points at the inspector, and the inspector forwards to the real target. That makes it protocol-agnostic in core and HTTP-only only when enabled. It needs a core hook to rewrite a service target, or we register the service with the inspector address |
| Metrics panel | `iroh_proxy_utils::UpstreamMetrics` | His `TransportStats` + `PeerDiagnostics`. **Needs per-service and inbound stats from core** (item 4) |
| Network map ("subway map") | Derived from `/v1/tunnels` + `/v1/peers` `conn_type` | Derived from his status: services, dials, networks, per-peer `path` and `rtt_ms`. VPC networks finally appear as their own line. Better with an event stream (item 3) |
| Tunnel notes, log tail, auto-expiry, restart auto-resume | Ours, independent | Kept as is. Notes are keyed by his service/dial ids instead of tunnel ids |
| Dashboard | Polls our API every 3 s | Same, against the merged API. Switch to SSE when core adds events |
| datumctl plugin | Ours: `tunnel …` and `tunnel api …` | **Decided: one `datumctl connect`, Scot's.** Our dashboard and inspector surfaces (and the notes, logs and token UX) fold into it as PRs Scot reviews. Our plugin is retired once that's done |
| HA add-on (aarch64, GHCR) | Builds our daemon | Rebuilt on the merged binary. Our glibc ≤ 2.36 gate and native arm64 build carry over. A CONNECT-IP/TUN add-on would need `NET_ADMIN` |
| Android POC | Our daemon as `lib*.so` | Rebuild on iroh 1.0. CONNECT-IP would need Android `VpnService`, which is out of scope for now |
| Daytona and SSH demos | Use peer/gateway tunnels | Re-scripted on serve/dial after Phase 3 |
| Relay selection and probe | Ours: Datum relays + latency probe | **Offer it to core** (fixes item 7) |
| Windows DNS resolver override | Ours | Offer it to core. We're the team with Windows hardware, so we should own Windows validation (item 9) |

## Phases

### Phase 0: agreement and contract (about 1 week, mostly conversation)

1. Decisions with Scot:
   - ~~(a) integration option C vs B~~: C agreed, B as the interim path.
   - ~~(b) plugin naming and ownership~~: one `datumctl connect`, Scot's.
   - (c) whether ticket-style cross-project sharing lives in core: **still open**.
2. Agree the core backlog items the feature layer needs first: **10** (runtime as a library), **3** (events and typed status), **4** (inbound and per-service diagnostics).
3. **Versioning.** He tags releases; we pin a tag via a Cargo git dependency. Breaking API changes come with a note.
4. Ask the platform/NSO team when the MASQUE gateway is scheduled. This sets when the legacy adapter can be retired.
5. Fix our CI so `cargo test` actually runs. Move the workflows to `.github/workflows/`.

### Phase 1: spike, our workspace on his core (2–3 days)

1. Branch `feat/core-on-connect` from `labs/main`. Add his crates as a pinned git dependency and bump our workspace to iroh 1.0.
2. Get it to build, with the legacy `iroh-proxy-utils` path still compiling. Check that the iroh 0.95-era proxy crate works on iroh 1.0. If it doesn't, the legacy adapter needs `iroh-proxy-utils` 0.3, which his lib already uses.
3. **Proof:** dial Scot's live connector `connect-e913…` `tcp-443` from our binary, so `curl https://google.com --connect-to google.com:443:localhost:8443` works. This is the old SCOT-CONNECTOR-PLAN Phase 1.
4. **Exit criteria:** a list of the exact core API gaps we hit. It feeds Scot's backlog.

### Phase 2: adopt the core (about 1–2 weeks)

1. **Interim (B):** his daemon runs alongside ours. The dashboard reads his `/v1/status` and `/v1/audit` for MASQUE services, dials and networks, and reads ours for legacy public tunnels and features.
   **Final (C):** once `connect-runtime` lands, our binary embeds it and mounts his `/v1` router. Then there's one daemon again. Every feature hook goes through his runtime's policy and identity; none of them bypass it.
2. Remove our `peer.rs`, the per-tunnel `ListenNode` and `HeartbeatAgent` from the MASQUE path. Move the legacy gateway path into `legacy_gateway.rs`, with nothing else touching iroh internals.
3. Converge local auth and audit on his model. Migrate existing `daemon_auth/` tokens, or force a one-time re-mint (simpler; it's a preview).
4. **Exit criteria:** serve, dial and network all work from our binary, existing public tunnels still work through the legacy adapter, and his test suite plus ours pass in CI.

### Phase 3: port the features (about 1–2 weeks)

1. Inspector as an opt-in loopback hop. Replay works for both legacy and MASQUE services.
2. Metrics, network map and visible-signal on his status and diagnostics schema. Add VPC networks to the map.
3. Notes, log tail and auto-expiry re-keyed to service/dial ids.
4. Dashboard updated to the merged API. Plugin feature commands moved to the agreed home.
5. **Exit criteria:** every dashboard panel works against the merged daemon, and `datumctl connect …` covers the old `tunnel api …` feature commands.

### Phase 4: integrations (about 1 week)

1. HA add-on image rebuilt and released, with a migration note: existing users re-enroll because connector naming changed from `datum-connect-*` to `connect-*`.
2. Daytona and SSH demos re-scripted. Android rebuild is best-effort.
3. Windows validation of his service, Wintun and DACL code on Brett's machine. Bugs found go upstream to Scot.

### Phase 5: retire the legacy path (when the platform ships)

When the platform gateway speaks `datum-connect/masque-v1`:
- move public tunnels to `serve --public`,
- delete `legacy_gateway.rs` and the `iroh-proxy-utils` dependency,
- ask Scot to delete his ~10k lines of legacy library code at the same time.

## Ongoing rules between the two projects

- **One canonical runtime.** Policy, identity, state and reconciliation are enforced only in Scot's runtime. Feature code may observe and extend it, but never re-implements or bypasses it.

- **Networking changes go upstream.** If a feature needs a change in transport, policy or enrollment, it's a PR or issue on Scot's repo, never a patch in ours. (Exception: the legacy adapter, until Phase 5.)
- **We never push to datum-cloud/connect directly** without Scot's say-so. Contributions go through PRs he reviews.
- **Pin tags, not branches.** We upgrade deliberately, and the dashboard's e2e run gates each bump.
- **Connector naming.** Everything new uses his `connect-<key40>` / masque-v1 Connector class. Our `datum-connect` class is legacy-only.

## Risks

| Risk | Mitigation |
|---|---|
| Platform MASQUE gateway slips, so we carry two stacks for a long time | Keep the legacy adapter small and isolated. Don't add features to it |
| His core is a "local preview" that landed in one large commit; churn will be high | Pin tags, and keep the Phase 1 gap list as a shared backlog |
| One-connection-per-flow makes dashboard/browser traffic over MASQUE slow | Measure in Phase 1 (fits the tunnel benchmark plan). Raise multiplexing early |
| `fail_closed` on any control-plane error drops all live tunnels | Top core backlog item. Demos are fragile until it's fixed |
| Losing ticket-based sharing breaks the Daytona and peer demos | Decide in Phase 0. If it's dropped, re-script the demos with same-project enrollment |
| Users must re-enroll (new connector names, keys and API) | It's a preview; document it in release notes |

## Questions for Scot

1. ~~Runtime split?~~ **Yes (2026-10-04),** as long as there's one canonical runtime that keeps the enforcement. B is fine as the interim path. *Follow-up: what does the extension hook look like? We need inspector target rewrite and event subscription.*
2. ~~Plugin?~~ **One `datumctl connect`, with our dashboard and inspector folded in** (2026-10-04).
3. Should ticket-style, cross-project, no-account sharing exist in core?
4. Priorities: events/typed status, inbound diagnostics, connection reuse, and making `fail_closed` per-service instead of global. Which do you already plan to do?
5. Release cadence and tag scheme we can pin to.
6. Do you know the platform MASQUE gateway timeline?
7. Will you take PRs for Datum prod relays + relay probing, and for Windows fixes from our Windows testing?
8. *New:* What's the API for the VPC gateway endpoint, and is it on staging? We'd like the network map and the GVPC client path to target it.
