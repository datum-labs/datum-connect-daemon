//! App-to-app (peer-to-peer) tunnels: connect two instances of this daemon
//! directly over iroh, with no Datum Cloud involvement at any layer — no
//! project, no `DatumCloudClient` call, no HTTPProxy/Connector resource,
//! and (deliberately) no Datum relay servers either. See NOTES.md's
//! "App-to-app (peer-to-peer) tunnels" section for the full design
//! rationale; this is the implementation of that scope.
//!
//! Trust model, chosen deliberately rather than defaulted into: possessing
//! a valid ticket for a still-enabled advertisement is the entire
//! credential — the same model iroh's own ticket types use elsewhere, and
//! comparable to a Syncthing device-ID exchange or a Tailscale share link.
//! There is no separate per-peer allowlist. Revocation already works via
//! the underlying `ListenNode::remove_proxy` (removing a `ProxyState`
//! immediately makes its target unreachable to anyone still holding a
//! ticket for it).
//!
//! Everything here is a thin wrapper over machinery that already exists in
//! `connect-lib` and is already proven in `app/cli`'s dev tooling
//! (`Commands::Serve`/`Commands::Connect`) — this module is what's
//! genuinely new: a stable identity for this purpose, an HTTP API in front
//! of it, and a relay mode that never touches Datum's infrastructure.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use connect_lib::{Advertisment, AdvertismentTicket, ConnectNode, ListenNode, OutboundProxyHandle, ProxyState, Repo, TcpProxyData};
use iroh::endpoint::ConnectionType;
use iroh::Watcher as _;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{Mutex, OnceCell, RwLock};

use crate::{err_response, not_found, ApiResult};

fn advertised_dir(base: &FsPath) -> PathBuf {
    base.join("daemon_peers")
}

fn advertised_path(base: &FsPath) -> PathBuf {
    advertised_dir(base).join("advertised.json")
}

async fn load_advertised(base: &FsPath) -> std::io::Result<HashSet<String>> {
    match tokio::fs::read(advertised_path(base)).await {
        Ok(bytes) => Ok(serde_json::from_slice::<Vec<String>>(&bytes).unwrap_or_default().into_iter().collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashSet::new()),
        Err(e) => Err(e),
    }
}

/// An outbound peer connection plus how many times its `conn_type` (direct
/// vs relay) has changed since it was established — see
/// `watch_connection_transitions`. The count, not the transitions
/// themselves, is what's cheap to expose live via `GET /v1/peers`; the
/// transitions themselves are recorded in the audit log.
struct TrackedConnection {
    handle: OutboundProxyHandle,
    transitions: Arc<AtomicU32>,
}

pub struct PeerState {
    repo: Repo,
    listen_node: OnceCell<ListenNode>,
    connect_node: OnceCell<ConnectNode>,
    connections: Mutex<HashMap<String, TrackedConnection>>,
    /// Resource ids genuinely created via `advertise`, persisted under
    /// `daemon_peers/advertised.json`. **Why this exists**: despite
    /// `Repo::peer_listen_key`'s doc comment claiming peer advertisements
    /// never share a `ProxyState` list with a Datum-routed tunnel's own
    /// node, they actually do — every `ListenNode` built from the same
    /// `Repo`/connect_dir (the shared tunnel-control node, each per-tunnel
    /// node, and this peer-listen node alike) loads and saves the *same*
    /// on-disk proxy list via `Repo::load_state`, since that call isn't
    /// scoped by which iroh identity is asking. Found 2026-09-07 (see
    /// NOTES.md) — without this set, `list`/`revoke` would surface (and
    /// `revoke` would be able to delete) other features' proxies just
    /// because they live in the same underlying list. Fixing the real
    /// isolation belongs in `connect-lib`'s shared plumbing; this is the
    /// smallest correct fix at this layer.
    advertised: RwLock<HashSet<String>>,
    /// Serializes mutate+persist of `advertised` end to end (including the
    /// disk write) so two concurrent advertise/revoke calls can't race and
    /// silently drop one another's update to `advertised.json` — found in
    /// review 2026-09-07: the previous version read-modified-wrote the set
    /// without holding anything across the `await`, so a losing writer's
    /// stale snapshot could overwrite a winner's newer one.
    advertised_lock: Mutex<()>,
    connect_dir: PathBuf,
}

impl PeerState {
    pub async fn new(repo: Repo, connect_dir: PathBuf) -> std::io::Result<Self> {
        let advertised = load_advertised(&connect_dir).await?;
        let had_prior_advertisements = !advertised.is_empty();
        let state = Self {
            repo,
            listen_node: OnceCell::new(),
            connect_node: OnceCell::new(),
            connections: Mutex::new(HashMap::new()),
            advertised: RwLock::new(advertised),
            advertised_lock: Mutex::new(()),
            connect_dir,
        };
        if had_prior_advertisements {
            // Previously-advertised targets exist on disk from before this
            // restart. `list`/`revoke` only ever look at the *lazily*
            // created listen node (listen_node_if_present), so without this,
            // a restarted daemon would report zero advertisements — even
            // though they're still real — until the next `advertise` call,
            // which would mint a brand-new resource id/ticket rather than
            // resuming the existing one (found in review 2026-09-07).
            // Warn-and-continue on failure, same tone as the tunnel
            // inspector reconciliation pass in main.rs.
            if let Err(e) = state.listen_node().await {
                tracing::warn!("failed to restore peer-listen identity for previously-advertised targets: {e:#}");
            }
        }
        Ok(state)
    }

    /// Lazily created on first advertise — a daemon that never advertises
    /// anything never spins up a second iroh identity at all.
    async fn listen_node(&self) -> n0_error::Result<&ListenNode> {
        self.listen_node
            .get_or_try_init(|| async {
                let key = self.repo.peer_listen_key().await?;
                ListenNode::new_with_key_and_relay_mode(
                    self.repo.clone(),
                    key,
                    iroh::endpoint::default_relay_mode(),
                )
                .await
            })
            .await
    }

    /// Non-forcing accessor for listing — a pure "list advertisements" call
    /// must not have the side effect of creating an iroh identity that
    /// wasn't already there.
    fn listen_node_if_present(&self) -> Option<&ListenNode> {
        self.listen_node.get()
    }

    /// Non-forcing accessor, same reasoning as `listen_node_if_present` —
    /// reporting connect-side metrics must not itself create the iroh
    /// identity used for dialing out.
    fn connect_node_if_present(&self) -> Option<&ConnectNode> {
        self.connect_node.get()
    }

    async fn connect_node(&self) -> n0_error::Result<&ConnectNode> {
        self.connect_node
            .get_or_try_init(|| {
                ConnectNode::new_with_relay_mode(self.repo.clone(), iroh::endpoint::default_relay_mode())
            })
            .await
    }

    async fn is_advertised(&self, resource_id: &str) -> bool {
        self.advertised.read().await.contains(resource_id)
    }

    async fn mark_advertised(&self, resource_id: String) -> std::io::Result<()> {
        let _guard = self.advertised_lock.lock().await;
        self.advertised.write().await.insert(resource_id.clone());
        if let Err(e) = self.save_advertised().await {
            // Don't let in-memory state silently drift from what's actually
            // on disk within this process's lifetime.
            self.advertised.write().await.remove(&resource_id);
            return Err(e);
        }
        Ok(())
    }

    async fn unmark_advertised(&self, resource_id: &str) -> std::io::Result<()> {
        let _guard = self.advertised_lock.lock().await;
        let was_present = self.advertised.write().await.remove(resource_id);
        if let Err(e) = self.save_advertised().await {
            if was_present {
                self.advertised.write().await.insert(resource_id.to_string());
            }
            return Err(e);
        }
        Ok(())
    }

    async fn save_advertised(&self) -> std::io::Result<()> {
        tokio::fs::create_dir_all(advertised_dir(&self.connect_dir)).await?;
        let snapshot: Vec<String> = self.advertised.read().await.iter().cloned().collect();
        crate::auth::write_json_atomic(&advertised_path(&self.connect_dir), &snapshot).await
    }
}

fn random_id() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 9];
    rand::rng().fill_bytes(&mut buf);
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

#[derive(Deserialize)]
pub struct AdvertiseRequest {
    /// Local target to advertise, "host:port" (no scheme — this is a raw
    /// TCP forward, not an HTTP proxy like the public-tunnel `endpoint`).
    pub endpoint: String,
    pub label: Option<String>,
}

#[derive(Serialize)]
pub struct AdvertiseResponse {
    pub resource_id: String,
    pub label: String,
    pub endpoint_id: String,
    /// Portable, paste-able string — hand this to the peer you want to
    /// reach this target. Possessing it is the entire credential; there is
    /// no separate approval step on connect.
    pub ticket: String,
}

pub async fn advertise(
    State(state): State<Arc<crate::AppState>>,
    Json(req): Json<AdvertiseRequest>,
) -> ApiResult<AdvertiseResponse> {
    let data = TcpProxyData::from_host_port_str(&req.endpoint)
        .map_err(|e| err_response(format!("invalid endpoint '{}': {e}", req.endpoint)))?;
    let advertisment = Advertisment::new(data, req.label);

    let node = state.peer.listen_node().await.map_err(err_response)?;
    node.set_proxy(ProxyState::new(advertisment.clone()))
        .await
        .map_err(err_response)?;
    state.peer.mark_advertised(advertisment.id().to_string()).await.map_err(err_response)?;

    let ticket = advertisment.ticket(node.endpoint_id()).to_ticket_string();
    Ok(Json(AdvertiseResponse {
        resource_id: advertisment.id().to_string(),
        label: advertisment.label().to_string(),
        endpoint_id: node.endpoint_id().to_string(),
        ticket,
    }))
}

#[derive(Deserialize)]
pub struct ConnectRequest {
    pub ticket: String,
    /// Local address to bind and forward from, e.g. "127.0.0.1:0" for an
    /// ephemeral port.
    pub bind: String,
}

#[derive(Serialize)]
pub struct ConnectResponse {
    pub id: String,
    pub bound_addr: String,
    pub remote_endpoint_id: String,
    pub target: String,
}

pub async fn connect(
    State(state): State<Arc<crate::AppState>>,
    Json(req): Json<ConnectRequest>,
) -> ApiResult<ConnectResponse> {
    let ticket: AdvertismentTicket = req
        .ticket
        .parse()
        .map_err(|e| err_response(format!("invalid ticket: {e}")))?;
    let bind_addr: SocketAddr = req
        .bind
        .parse()
        .map_err(|e| err_response(format!("invalid bind address '{}': {e}", req.bind)))?;

    let node = state.peer.connect_node().await.map_err(err_response)?;
    let handle = node
        .connect_and_bind_local(ticket.endpoint, ticket.service(), bind_addr)
        .await
        .map_err(err_response)?;

    let id = random_id();
    let remote_id = handle.remote_id();
    let response = ConnectResponse {
        id: id.clone(),
        bound_addr: handle.bound_addr().to_string(),
        remote_endpoint_id: remote_id.to_string(),
        target: handle.advertisment().address(),
    };
    let transitions = Arc::new(AtomicU32::new(0));
    state.peer.connections.lock().await.insert(
        id.clone(),
        TrackedConnection { handle, transitions: transitions.clone() },
    );
    tokio::spawn(watch_connection_transitions(state.clone(), id, remote_id, transitions));
    Ok(Json(response))
}

/// Polls `conn_type` for one outbound connection every 2s (well within the
/// dashboard's own 3s refresh, so a change reads as "live" without needing
/// iroh's async watcher-stream API and its own crate dependency) and
/// records each change — direct-vs-relay flips are the one genuinely
/// iroh-specific signal worth surfacing as an event, not just a snapshot
/// (project owner's "what does iroh give us" / "live upgrade/degradation
/// events" asks, 2026-09-07/08). Exits on its own once `connection_id` is
/// no longer in `state.peer.connections` (disconnected, or the daemon is
/// shutting down) — no separate cancellation handle needed.
async fn watch_connection_transitions(
    state: Arc<crate::AppState>,
    connection_id: String,
    remote_id: iroh::EndpointId,
    transitions: Arc<AtomicU32>,
) {
    let mut last: Option<ConnectionType> = None;
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        interval.tick().await;
        if !state.peer.connections.lock().await.contains_key(&connection_id) {
            return; // disconnected — stop watching
        }
        let Some(node) = state.peer.connect_node_if_present() else { continue };
        let Some(mut watcher) = node.endpoint().conn_type(remote_id) else { continue };
        let current = watcher.get();
        if let Some(prev) = &last {
            if *prev != current {
                transitions.fetch_add(1, Ordering::Relaxed);
                let (label, detail) = describe_conn_type(&current);
                tracing::info!(
                    connection = %connection_id, from = %describe_conn_type(prev).0, to = %label, detail = ?detail,
                    "peer connection type changed"
                );
                crate::auth::append_audit(
                    &state.connect_dir,
                    &state.audit_lock,
                    &format!("peer_connection_{label}"),
                    &connection_id,
                    "system",
                )
                .await;
            }
        }
        last = Some(current);
    }
}

#[derive(Serialize)]
pub struct AdvertisementSummary {
    pub resource_id: String,
    pub label: String,
    pub endpoint: String,
    pub enabled: bool,
    /// Per-target, genuinely scoped to this one advertisement (not shared
    /// with any other) — see `ConnectMetrics` for why the connect side
    /// can't offer the same precision.
    pub bytes_to_origin: u64,
    pub bytes_from_origin: u64,
}

#[derive(Serialize)]
pub struct ConnectionSummary {
    pub id: String,
    pub bound_addr: String,
    pub remote_endpoint_id: String,
    pub target: String,
    /// "direct" (real peer-to-peer, hole-punched), "relay" (bounced through
    /// a relay server), "mixed" (have a direct address but it's unconfirmed,
    /// still relaying), or "unknown" (no verified connection state yet) —
    /// see `iroh::endpoint::ConnectionType`. Added 2026-09-07 per the
    /// project owner's ask for "what does iroh give us" — this is the one
    /// genuinely iroh-specific thing worth surfacing over generic byte
    /// counts.
    pub conn_type: String,
    /// The direct socket address and/or relay URL behind `conn_type`, when
    /// known — e.g. which relay is actually being used.
    pub conn_detail: Option<String>,
    /// Round-trip time to this peer in milliseconds, when known.
    pub latency_ms: Option<u64>,
    /// How many times `conn_type` has changed since this connection was
    /// established (e.g. relay -> direct once hole-punching succeeds, or
    /// direct -> relay on degradation) — see `watch_connection_transitions`.
    /// Each change is also recorded in the audit log as
    /// `peer_connection_<type>`, actor `system`; this count is what's cheap
    /// to show live in the dashboard on every poll.
    pub transition_count: u32,
}

/// Maps iroh's `ConnectionType` to a stable, UI-friendly label plus a
/// human-readable detail string — decouples the API's wire format from
/// iroh's own `Display` formatting (`"direct(1.2.3.4:5678)"` etc.), which
/// isn't a contract we want to depend on verbatim.
fn describe_conn_type(ct: &ConnectionType) -> (&'static str, Option<String>) {
    match ct {
        ConnectionType::Direct(addr) => ("direct", Some(addr.to_string())),
        ConnectionType::Relay(url) => ("relay", Some(url.to_string())),
        ConnectionType::Mixed(addr, url) => ("mixed", Some(format!("{addr} via {url}"))),
        ConnectionType::None => ("unknown", None),
    }
}

#[derive(Serialize)]
pub struct ConnectMetrics {
    /// Aggregate across every outbound peer connection from this daemon,
    /// not per-connection — the underlying proxy pool only tracks bytes in
    /// total for the connect/dial side, unlike the advertise side (which is
    /// genuinely per-target). With one active connection these numbers
    /// describe it exactly; with several, they describe all of them combined.
    pub bytes_to_upstream: u64,
    pub bytes_from_upstream: u64,
    pub active_iroh_connections: u64,
    pub total_iroh_connections: u64,
}

#[derive(Serialize)]
pub struct PeerListResponse {
    pub endpoint_id: Option<String>,
    pub advertisements: Vec<AdvertisementSummary>,
    pub connections: Vec<ConnectionSummary>,
    /// `None` until this daemon has ever connected out to a peer — reporting
    /// metrics must not itself spin up the connect-side iroh identity.
    pub connect_metrics: Option<ConnectMetrics>,
}

pub async fn list(State(state): State<Arc<crate::AppState>>) -> ApiResult<PeerListResponse> {
    let (endpoint_id, advertisements) = match state.peer.listen_node_if_present() {
        Some(node) => {
            // Filter to genuine peer advertisements — `node.proxies()` also
            // returns every other feature's proxies that happen to live in
            // the same shared on-disk list (see `PeerState::advertised`'s
            // doc comment).
            let mut ads = Vec::new();
            for p in node.proxies() {
                let resource_id = p.info.id().to_string();
                if !state.peer.is_advertised(&resource_id).await {
                    continue;
                }
                let target = node.metrics_for(p.info.service());
                ads.push(AdvertisementSummary {
                    resource_id,
                    label: p.info.label().to_string(),
                    endpoint: p.info.service().address(),
                    enabled: p.enabled,
                    bytes_to_origin: target.as_ref().map(|t| t.bytes_to_origin()).unwrap_or(0),
                    bytes_from_origin: target.as_ref().map(|t| t.bytes_from_origin()).unwrap_or(0),
                });
            }
            (Some(node.endpoint_id().to_string()), ads)
        }
        None => (None, Vec::new()),
    };

    let connect_node = state.peer.connect_node_if_present();
    let connections = state
        .peer
        .connections
        .lock()
        .await
        .iter()
        .map(|(id, tc)| {
            let handle = &tc.handle;
            let (conn_type, conn_detail) = connect_node
                .and_then(|node| node.endpoint().conn_type(handle.remote_id()))
                .map(|mut w| describe_conn_type(&w.get()))
                .map(|(label, detail)| (label.to_string(), detail))
                .unwrap_or_else(|| ("unknown".to_string(), None));
            let latency_ms = connect_node
                .and_then(|node| node.endpoint().latency(handle.remote_id()))
                .map(|d| d.as_millis() as u64);
            ConnectionSummary {
                id: id.clone(),
                bound_addr: handle.bound_addr().to_string(),
                remote_endpoint_id: handle.remote_id().to_string(),
                target: handle.advertisment().address(),
                conn_type,
                conn_detail,
                latency_ms,
                transition_count: tc.transitions.load(Ordering::Relaxed),
            }
        })
        .collect();

    let connect_metrics = state.peer.connect_node_if_present().map(|node| {
        let m = node.metrics();
        ConnectMetrics {
            bytes_to_upstream: m.bytes_to_upstream.get(),
            bytes_from_upstream: m.bytes_from_upstream.get(),
            active_iroh_connections: m.active_iroh_connections(),
            total_iroh_connections: m.total_iroh_connections(),
        }
    });

    Ok(Json(PeerListResponse { endpoint_id, advertisements, connections, connect_metrics }))
}

pub async fn revoke(
    State(state): State<Arc<crate::AppState>>,
    Path(resource_id): Path<String>,
) -> ApiResult<serde_json::Value> {
    // Also guards against deleting some *other* feature's proxy state (e.g.
    // a regular tunnel's) — before this check, this endpoint would happily
    // remove any id from the shared list, peer-created or not.
    if !state.peer.is_advertised(&resource_id).await {
        return Err(not_found(&resource_id));
    }
    let Some(node) = state.peer.listen_node_if_present() else {
        return Err(not_found(&resource_id));
    };
    match node.remove_proxy(&resource_id).await.map_err(err_response)? {
        Some(_) => {
            state.peer.unmark_advertised(&resource_id).await.map_err(err_response)?;
            Ok(Json(json!({ "revoked": true, "resource_id": resource_id })))
        }
        None => Err(not_found(&resource_id)),
    }
}

pub async fn disconnect(
    State(state): State<Arc<crate::AppState>>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    match state.peer.connections.lock().await.remove(&id) {
        Some(tc) => {
            tc.handle.abort(); // watch_connection_transitions notices this id is gone within 2s and exits on its own
            Ok(Json(json!({ "disconnected": true, "id": id })))
        }
        None => Err(not_found(&id)),
    }
}
