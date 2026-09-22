# Datum Connect Daemon — local API reference

This documents the local HTTP API exposed by `datum-connect-daemon` (`connect/connect-lib/daemon`). It's a loopback-only API (`127.0.0.1`, never bound to a public interface) fronting one Datum Cloud project per daemon instance. The `datumctl connect` plugin (`tunnel api ...` / `tunnel daemon ...`) is the reference client; anything else (a browser client, an MCP server) talks to the same endpoints directly over HTTP.

Base URL: `http://127.0.0.1:<port>` (default port `47780`, `--port`/`DATUM_TUNNEL_DAEMON_PORT`).

## Daemon info

### `GET /v1/info` — unauthenticated
Non-sensitive metadata a client needs before a token is even entered — e.g. to build a cloud-portal deep link, or to learn the daemon's configured log-tail line cap. Knowing this grants no capability by itself; every real action still goes through the normal auth gate below.
```json
{ "project_id": "my-project-1a2b3c", "portal_base_url": "https://cloud.datum.net", "log_tail_max_lines": 1000 }
```

## Auth

Every other endpoint requires `Authorization: Bearer <token>`. There are three tiers:

| Tier | Credential | Can do |
|---|---|---|
| **Setup** | The daemon's single setup token, at `$DATUM_CONNECT_DIR/daemon_auth/setup.token` (0600). Read automatically by the CLI for interactive use. | Everything. |
| **Operate** | A minted, per-tunnel token, shaped `<token_id>.<secret>` (see `POST /v1/tunnels/:id/tokens`). | Start/stop/read-progress/read-metrics on the *one* tunnel it was minted for. Nothing else. |
| **Viewer** | The daemon's single global viewer token, at `$DATUM_CONNECT_DIR/daemon_auth/viewer.token` — only exists once deliberately minted (`POST /v1/viewer-token`), unlike the setup token. | Read-only, across every tunnel: list/get, progress, metrics, traffic, replay. Also read-only across every *registered log source* (list, tail — see "Log tail sources" below). Never create/delete/start/stop/mint-any-token/audit/register-a-log-source. Built for the browser dashboard. **Scope note**: this includes captured request/response *bodies* (the traffic tab) and the full content of every registered log's tail, not just status — known-sensitive headers (`Authorization`, `Cookie`, etc.) are redacted before capture regardless of who reads them back, but neither traffic body content nor log content is or can reliably be redacted, so treat this token as sensitive to whatever your tunnels and registered logs actually contain, not as a harmless "read-only" credential. |

Error responses are always `{"error": "<message>"}`. Auth-specific status codes:
- **401** — missing, malformed, unknown, revoked, or expired credential (or a valid operate token that just isn't valid *for this tunnel*).
- **403** — a real, valid credential of the wrong tier (e.g. a viewer token hitting a setup-only or start/stop route).

Other status codes used throughout: **404** (tunnel/token/exchange not found), **409** (tunnel is mid-start or mid-stop, retry shortly), **500** (everything else, `error` holds the underlying message).

## Tunnels

### `GET /v1/tunnels` — setup or viewer
List every tunnel profile in the daemon's project. Returns `TunnelSummary[]`, each with two extra fields: `last_start_actor` — the credential that last started this tunnel (`"setup"`, `"operate:<token_id>"`, or `null` if it's never been started) — so an agent-started tunnel is always distinguishable from a human-started one, not just "running: true"; and `note` — a free-text note set via the CLI (see `POST /v1/tunnels/:id/note` below), or `null` if none has been set.

### `POST /v1/tunnels` — setup only
Create a tunnel profile. Does **not** start it.
```json
// request
{ "label": "my-app", "endpoint": "127.0.0.1:3000" }
```
Returns a `TunnelSummary`. Starts the L7 traffic inspector for this tunnel immediately (see below) and persists its real target so a daemon restart can recover it.

### `GET /v1/tunnels/:id` — setup or viewer
Returns one `TunnelSummary`, with the same `last_start_actor` field as the list endpoint above.

### `DELETE /v1/tunnels/:id` — setup only
Deletes the tunnel server-side first; only tears down local state (inspector, traffic history, operate tokens) once that succeeds. Returns `TunnelDeleteOutcome`.

### `GET /v1/tunnels/:id/progress` — setup, operate (scoped), or viewer
Setup-pipeline status: HTTPProxy acceptance/programming, connector readiness, DNS publication.
```json
{
  "hostnames": ["foo-bar-12345.datumproxy.net"],
  "steps": [
    { "kind": "proxy_accepted", "status": "ready", "reason": "Accepted", "message": "...", "resource": "HTTPProxy/tunnel-xyz" },
    { "kind": "connector_ready", "status": "ready", "reason": "ConnectorReady", "message": "...", "resource": "Connector/datum-connect-xyz" }
  ]
}
```

### `GET /v1/tunnels/:id/metrics` — setup, operate (scoped), or viewer
Real network-level stats for a *running* tunnel, straight from iroh's own per-target proxy metrics (the same source the desktop app's bandwidth chart reads). `null` if the tunnel isn't currently running.
```json
{
  "bytes_to_origin": 2560, "bytes_from_origin": 2355,
  "accepted_requests": 8, "denied_requests": 0, "failed_requests": 0, "active_requests": 0,
  "active_iroh_connections": 0, "total_iroh_connections": 2
}
```

### `POST /v1/tunnels/:id/start` — setup or operate (scoped)
Turns the tunnel on: mints/rewires its listen key if needed, starts the heartbeat, enables it server-side. Returns the current `TunnelSummary`. Idempotent if already running. `409` if another start/stop for this id is already in flight.

### `POST /v1/tunnels/:id/stop` — setup or operate (scoped)
Turns the tunnel off. Returns the current `TunnelSummary`. Idempotent if already stopped. Also invoked internally by the auto-expiry sweep (see below) — that path logs as `auto_expired`, not `stop`, in the audit log.

### `POST /v1/tunnels/:id/note` — setup only
Sets (or clears, with an empty string) a free-text note on a tunnel — CLI-only (`tunnel api note set/clear`), so it's still obvious what a tunnel is for once there are many running. Shown read-only in the dashboard and returned by `GET /v1/tunnels`/`GET /v1/tunnels/:id`; there's no dashboard input for it. Capped at 2000 bytes.
```json
// request
{ "note": "spun up to test the notes feature" }
// response
{ "id": "tunnel-z9p5k", "note": "spun up to test the notes feature" }
```

## Traffic (L7 inspector) — setup or viewer for all of these

Each tunnel has its own reverse-proxy inspector sitting between the tunnel and the real local target, capturing request/response headers and bodies (capped at 64KB each; full traffic still streams through uncapped, only the *recorded copy* is capped, with `body_truncated: true` past the cap). Known-sensitive headers (`Authorization`, `Proxy-Authorization`, `Cookie`, `Set-Cookie`, `X-Api-Key`, `X-Auth-Token`) are redacted to `[redacted]` **at capture time**, before they ever enter the stored exchange — the real value still reaches the local target unchanged, only the recorded/displayable copy is redacted. Body content is not redacted (not reliably possible — a secret in a JSON field looks like any other string), so treat captured traffic as sensitive to whatever your own tunneled app actually sends.

### `GET /v1/tunnels/:id/traffic`
List captured exchanges, most recent last. Returns `ExchangeSummary[]`: `{ id, timestamp_unix_ms, method, path, response_status }`.

### `GET /v1/tunnels/:id/traffic/:exchange_id`
Full detail for one exchange, including headers and (possibly truncated) bodies for both request and response.

### `POST /v1/tunnels/:id/traffic/:exchange_id/replay`
Re-sends the captured request to the tunnel's real local target right now (a live side-effecting call, not a replay-only sandbox) and returns the fresh response. Refuses to replay an exchange whose captured body was truncated (would send an incomplete/mismatched-length body). 30s timeout against the real target.

*Viewer, not operate*: an operate token is scoped to acting on one tunnel it already knows the id of, not to browsing traffic — so these are setup-or-viewer, same as list/get, not setup-or-operate.

## Viewer token — setup only for all of these

### `POST /v1/viewer-token`
Mint (or rotate — calling this again immediately invalidates the old one) the single global viewer token.
```json
{ "token": "RaT6oV3YHUlYUAxsvISK_naUljh-BDzwTv5VzvOlxHg" }
```
Shown exactly once, like an operate token's secret half.

### `GET /v1/viewer-token`
`{ "exists": true|false }` — whether a viewer token currently exists. Never returns the value.

### `DELETE /v1/viewer-token`
Revoke it immediately. `{ "revoked": true|false }`.

## Operate tokens — setup only for all of these

### `POST /v1/tunnels/:id/tokens`
Mint a new operate token scoped to this tunnel.
```json
// request
{ "ttl_seconds": 86400 }   // omit or null = never expires
```
```json
// response — the "bearer" value is shown exactly once, never retrievable again
{
  "token_id": "Lh60O0UX3rDD",
  "bearer": "Lh60O0UX3rDD.tzKX2aH8nrAZnqF71aB80mwrK-XjNUkTmMSJV7qG1es",
  "tunnel_id": "tunnel-tpmrf",
  "created_at_unix_ms": 1787938116274,
  "expires_at_unix_ms": 1787941716274
}
```
Only a salted hash of the secret half is ever persisted to disk.

### `GET /v1/tunnels/:id/tokens`
List this tunnel's tokens, metadata only (`token_id`, `created_at_unix_ms`, `expires_at_unix_ms`, `revoked`) — the secret is never shown again after creation.

### `DELETE /v1/tunnels/:id/tokens/:token_id`
Revoke a token immediately — the human kill switch. Any in-flight or future use of that token gets `401` from the moment this call succeeds.

## Audit log

### `GET /v1/audit` — setup only
Returns the last ~500 entries from the append-only audit log (`daemon_auth/audit.jsonl`). No edit/delete endpoint exists anywhere — that's what makes it immutable.
```json
[
  { "ts_unix_ms": 1787938115828, "event": "create", "tunnel_id": "tunnel-tpmrf", "actor": "setup" },
  { "ts_unix_ms": 1787938119708, "event": "start", "tunnel_id": "tunnel-tpmrf", "actor": "operate:Lh60O0UX3rDD" },
  { "ts_unix_ms": 1787939000000, "event": "auto_expired", "tunnel_id": "tunnel-tpmrf", "actor": "system" }
]
```
`event` is one of: `create`, `delete`, `start`, `stop`, `auto_expired`, `token_created`, `token_revoked`, `viewer_token_created`, `viewer_token_revoked`, `log_source_added`, `log_source_removed`. `actor` is `setup`, `operate:<token_id>`, `viewer`, or `system` (the auto-expiry sweep) — log-source events are always `setup`, since registering one is a setup-only action.

## App-to-app (peer) tunnels — setup only, except `GET /v1/peers` (setup or viewer)

Direct daemon-to-daemon tunnels over iroh, with no Datum Cloud involvement at any layer — no project, no HTTPProxy/Connector, and pinned to iroh's own public relays rather than Datum's. Shown in the dashboard's "Peer tunnels" section.

### `POST /v1/peers/advertise`
Advertise a local target for direct peer access.
```json
// request
{ "endpoint": "127.0.0.1:8890", "label": "demo-target" }
```
```json
{
  "resource_id": "proxy-66eqbo0nm1wq", "label": "demo-target",
  "endpoint_id": "cb0c76bc2a9bd7ff...",
  "ticket": "datumcjyhe33ypewtmntfofrg6mdonuyxo4ibbnsgk3lpfv2gc4thmv2asmjsg4x..."
}
```
The `ticket` is the entire credential — hand it to whoever should be able to reach this target. No separate allowlist.

### `POST /v1/peers/connect`
Consume a ticket and bind a local port that forwards directly to the advertising peer.
```json
// request
{ "ticket": "datum...", "bind": "127.0.0.1:0" }
```
```json
{
  "id": "41NYOeSkCrkK", "bound_addr": "127.0.0.1:46707", "remote_endpoint_id": "cb0c76bc...", "target": "127.0.0.1:8890",
  "conn_type": "direct", "conn_detail": "203.0.113.10:41641", "latency_ms": 12, "transition_count": 0
}
```
`conn_type` is one of `"direct"`, `"relay"`, `"mixed"`, or `"unknown"`, polled every 2s from iroh's own connection state; `conn_detail` is the socket address or relay URL behind it. `transition_count` counts how many times `conn_type` has changed since the connection was established — a connection that falls back from direct to relay mid-session (or recovers) is visible here, not just its current state.

### `GET /v1/peers` — setup or viewer
This daemon's own advertisements and active outbound connections, plus real network metrics. Per-advertisement byte counts are genuinely scoped to that one target; `connect_metrics` is an aggregate across *every* outbound peer connection combined, not per-connection — the underlying proxy pool only tracks connect-side bytes in total, unlike the advertise side.
```json
{
  "endpoint_id": "cb0c76bc2a9bd7ff...",
  "advertisements": [{
    "resource_id": "proxy-66eqbo0nm1wq", "label": "demo-target", "endpoint": "127.0.0.1:8890", "enabled": true,
    "bytes_to_origin": 552, "bytes_from_origin": 580
  }],
  "connections": [{
    "id": "41NYOeSkCrkK", "bound_addr": "127.0.0.1:46707", "remote_endpoint_id": "cb0c76bc...", "target": "127.0.0.1:8890",
    "conn_type": "direct", "conn_detail": "203.0.113.10:41641", "latency_ms": 12, "transition_count": 0
  }],
  "connect_metrics": { "bytes_to_upstream": 316, "bytes_from_upstream": 580, "active_iroh_connections": 0, "total_iroh_connections": 1 }
}
```
`connect_metrics` is `null` until this daemon has ever connected out to a peer (reporting metrics must not itself spin up the connect-side iroh identity).

### `DELETE /v1/peers/advertise/:resource_id`
Revoke an advertisement — every ticket issued for it stops working immediately, including already-connected peers.

### `DELETE /v1/peers/connections/:id`
Stop an active outbound connection from `peer connect`.

## Log tail sources

Lets the dashboard (and `tunnel api log ...`) show a tail of any registered log file — the daemon's own log, or any other file the operator wants visible (e.g. a local dev server's log). **Registering a source is setup only** — it's the action that decides which file becomes readable at all, same trust level as tunnel create/peer advertise. **Reading is setup or viewer.** There is no endpoint that accepts an arbitrary path at read time — only a pre-registered name.

The daemon auto-registers its own log as a source named `daemon` (`$DATUM_CONNECT_DIR/daemon.log`) on every startup.

### `POST /v1/logs` — setup only
Register (or re-register, to change the path) a named log source. Does not require the file to exist yet.
```json
// request
{ "name": "dev-server", "path": "/home/you/myapp/server.log" }
// response
{ "name": "dev-server", "path": "/home/you/myapp/server.log", "exists": true, "size_bytes": 48213 }
```

### `GET /v1/logs` — setup or viewer
List registered sources. Metadata only, never content — `LogSourceSummary[]` (`name`, `path`, `exists`, `size_bytes`).

### `DELETE /v1/logs/:name` — setup only
Unregister a source. Never touches or deletes the underlying file.

### `GET /v1/logs/:name/tail?lines=N` — setup or viewer
Last `N` lines, oldest-first within the window — same order `tail -n N` prints, not reversed. Reads backward from EOF in chunks rather than loading the whole file. Omitting `lines=` or asking for more than the daemon's configured cap (`--log-tail-max-lines`/`DATUM_LOG_TAIL_MAX_LINES`, default 1000, surfaced via `GET /v1/info`) falls back to that same cap. A not-yet-existing file (or one that's momentarily rotated away) returns an empty `lines: []`, not an error.
```json
{ "name": "dev-server", "lines": ["...", "..."] }
```
**Known limitation**: if the tailed file rotates mid-session (logrotate, or the app's own rotation), the next tail just reads whatever's at that path now — no inode-tracking to keep following the pre-rotation file.

## Guardrails that aren't separate endpoints

- **Auto-expiry backstop**: `--max-tunnel-hours`/`DATUM_TUNNEL_MAX_HOURS` (default 24). A background sweep force-stops any tunnel that's overrun this, regardless of who started it — logged as `auto_expired`/`system`.
- **Loopback-only binding**: hardcoded to `127.0.0.1`, not configurable — the daemon is unreachable from the network under any configuration.
- **Log tail line cap**: operator-configured (`--log-tail-max-lines`/`DATUM_LOG_TAIL_MAX_LINES`, default 1000), to bound response size against a huge registered file. The same value is both the default and the ceiling.

## Not yet built

See the top-level [README.md](./README.md#status) for the current list of what's proven versus still aspirational — notably, the daemon's Datum-auth bootstrap isn't lazy yet, so it's required at startup even for peer-only usage that never touches Datum Cloud.
