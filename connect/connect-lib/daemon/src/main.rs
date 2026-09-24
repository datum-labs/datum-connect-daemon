//! Minimal local HTTP daemon wrapping `connect-lib`'s `TunnelService`.
//!
//! Skunkworks proof-of-concept: proves that a persistent process (rather
//! than the one-tunnel-per-process `datum-connect listen` model) can create,
//! start, stop, and report on tunnels over a small HTTP API, as the first
//! step toward a shared API for the desktop app, an MCP server, and
//! `datumctl`. See `D:\code\datum-desktop-tunnels\API-PLAN.md`.
//!
//! No auth/token-tier model yet (loopback-only for now), single project per
//! daemon instance, no appliance mode. `datumctl`'s `connect-plugin` is the
//! first client (`tunnel daemon` to manage this process, `tunnel api` to
//! call it).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use connect_lib::datum_cloud::env::ApiEnv;
use connect_lib::datum_cloud::external_token_source::ExternalTokenSource;
use connect_lib::datum_cloud::DatumCloudClient;
use connect_lib::{HeartbeatAgent, ListenNode, Repo, SelectedContext, TunnelService};
use iroh::SecretKey;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{Mutex, RwLock};
use tracing_subscriber::prelude::*;

mod auth;
mod inspector;
mod logs;
mod peer;
use auth::Actor;
use inspector::InspectorHandle;

pub(crate) type ApiResult<T> = Result<Json<T>, (StatusCode, Json<serde_json::Value>)>;

/// A tunnel that's currently "on" — its live iroh identity and heartbeat
/// have to stay running for as long as the tunnel is enabled, unlike the
/// stateless read/create/delete operations below which reuse one shared
/// throwaway identity (`AppState::control`), matching how the existing
/// `datum-connect` CLI's `list`/`update`/`delete` commands already work.
struct RunningTunnel {
    // Also the source of live per-tunnel network metrics — see get_metrics.
    node: ListenNode,
    heartbeat: HeartbeatAgent,
    service: TunnelService,
    /// `Instant` drives the auto-expiry sweep (immune to wall-clock
    /// adjustment); `_unix_ms` is only for future audit-log enrichment,
    /// since `Instant` isn't meaningful to a human reading `audit.jsonl`.
    started_at: std::time::Instant,
    #[allow(dead_code)]
    started_at_unix_ms: u64,
}

struct AppState {
    datum: DatumCloudClient,
    repo: Repo,
    project_id: String,
    /// Shared throwaway identity for operations that don't need a live
    /// connection: list, get, progress, create (profile only), delete.
    control: TunnelService,
    running: Mutex<HashMap<String, RunningTunnel>>,
    /// Tunnel ids currently mid-start or mid-stop, reserved atomically
    /// before any await point — closes the race where two concurrent
    /// /start calls could both see "not running" and both mint/rewire a
    /// listen key, and (since the auto-expiry sweep introduced a second
    /// caller of "stop this tunnel") the race where a /start could begin
    /// while a not-yet-finished teardown is still in flight. Plain std
    /// Mutex (not tokio's) so it can be locked synchronously from Drop for
    /// cleanup.
    busy: StdMutex<HashSet<String>>,
    /// One traffic inspector per tunnel — see `inspector.rs`. Keyed by
    /// tunnel id, started at profile-create time (not tied to start/stop).
    inspectors: Mutex<HashMap<String, InspectorHandle>>,
    /// Base directory for persisting each tunnel's real (non-inspector)
    /// target across restarts — see `save_inspector_target` and the
    /// reconciliation pass in `run()`. The daemon's own server-side
    /// `endpoint` for a tunnel is always the inspector's ephemeral local
    /// port, which dies with the process, so the real target has to be
    /// recorded somewhere else to survive a restart. Also the base for
    /// `daemon_auth/` (setup/operate tokens, audit log — see `auth.rs`).
    connect_dir: std::path::PathBuf,
    /// Loaded once at startup, never re-read per request — see
    /// `auth::load_or_create_setup_token`.
    setup_token: String,
    /// Global, read-only — see `auth.rs`'s module doc comment. `None` until
    /// someone deliberately mints one; unlike `setup_token` this can change
    /// at runtime (mint/revoke), hence the lock.
    viewer_token: RwLock<Option<String>>,
    /// Max time a tunnel may stay enabled before the auto-expiry sweep
    /// force-stops it, regardless of who started it.
    max_tunnel_runtime: std::time::Duration,
    /// Serializes audit-log appends so concurrent writers (request handlers
    /// racing the sweep task) can't interleave partial lines.
    audit_lock: Mutex<()>,
    /// App-to-app (peer-to-peer) tunnels — see `peer.rs`. Entirely separate
    /// from `datum`/`control`/`repo` above: no Datum Cloud project, client,
    /// or resource is ever touched by this feature.
    peer: peer::PeerState,
    /// Registered log-tail sources for the dashboard — see `logs.rs` and
    /// `LOG-TAIL-PLAN.md`.
    log_sources: logs::LogSources,
    /// Operator-configurable hard cap on `/v1/logs/:name/tail` — see
    /// `Args::log_tail_max_lines`.
    log_tail_max_lines: usize,
    /// When this daemon process started, for the dashboard's uptime.
    started_at_unix_ms: u128,
}

fn inspector_target_dir(base: &std::path::Path) -> std::path::PathBuf {
    base.join("daemon_inspector_targets")
}

fn inspector_target_path(base: &std::path::Path, id: &str) -> std::path::PathBuf {
    inspector_target_dir(base).join(format!("{id}.txt"))
}

async fn save_inspector_target(base: &std::path::Path, id: &str, target: &str) -> std::io::Result<()> {
    let dir = inspector_target_dir(base);
    tokio::fs::create_dir_all(&dir).await?;
    tokio::fs::write(inspector_target_path(base, id), target).await
}

async fn load_inspector_target(base: &std::path::Path, id: &str) -> Option<String> {
    tokio::fs::read_to_string(inspector_target_path(base, id)).await.ok()
}

async fn remove_inspector_target(base: &std::path::Path, id: &str) {
    let _ = tokio::fs::remove_file(inspector_target_path(base, id)).await;
}

#[derive(Parser, Debug)]
#[command(name = "datum-connect-daemon", about = "Local HTTP daemon for Datum Connect tunnels (plugin mode)")]
struct Args {
    #[clap(long, env = "DATUM_CONNECT_DIR")]
    repo: Option<std::path::PathBuf>,
    #[clap(long, env = "DATUM_PROJECT")]
    project: Option<String>,
    #[clap(long, env = "DATUM_TUNNEL_DAEMON_PORT", default_value_t = 47780)]
    port: u16,
    /// Auto-expiry backstop: a tunnel left enabled longer than this gets
    /// force-stopped regardless of who started it. See NOTES.md's security
    /// model guardrail #1.
    #[clap(long, env = "DATUM_TUNNEL_MAX_HOURS", default_value_t = 24)]
    max_tunnel_hours: u64,
    /// Hard cap on lines a single `/v1/logs/:name/tail` request can return,
    /// regardless of what it asks for — see LOG-TAIL-PLAN.md. Also the
    /// default when `lines` is omitted. Surfaced via `GET /v1/info` so the
    /// dashboard can show it.
    #[clap(long, env = "DATUM_LOG_TAIL_MAX_LINES", default_value_t = 1000)]
    log_tail_max_lines: usize,
}

#[derive(Deserialize)]
struct CreateTunnelRequest {
    label: String,
    endpoint: String,
}

pub(crate) fn err_response(e: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": e.to_string() })),
    )
}

pub(crate) fn not_found(id: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": format!("tunnel '{id}' not found") })),
    )
}

/// Same missing-key handling as `datum-connect`'s CLI `Listen` command: a
/// freshly `create`d tunnel has no persisted per-tunnel key yet, so the
/// first `start` generates one. `should_rewire = true` means the connector
/// created at profile-creation time (under the daemon's shared throwaway
/// identity) needs to be replaced to match this new identity.
async fn resolve_listen_key(
    repo: &Repo,
    project_id: &str,
    tunnel_id: &str,
) -> n0_error::Result<(SecretKey, bool)> {
    match repo.listen_key_for_tunnel(project_id, tunnel_id).await {
        Ok(k) => Ok((k, false)),
        Err(e) if e.to_string().contains("KEY_NOT_FOUND") => {
            tracing::info!(tunnel = %tunnel_id, "no listen key on file — generating one (connector will be (re)created)");
            Ok((SecretKey::generate(&mut rand::rng()), true))
        }
        Err(e) => Err(e),
    }
}

/// Human-readable OS details for the dashboard's Device page — e.g.
/// "Mac OS" / "15.5.0", or "Ubuntu" / "24.04" / "noble". Detecting them
/// shells out on some platforms (`sw_vers` on macOS), so it's done once
/// rather than per `/v1/info` request.
struct OsDetails {
    name: String,
    version: String,
    codename: Option<String>,
    edition: Option<String>,
}

static OS_INFO: std::sync::LazyLock<OsDetails> = std::sync::LazyLock::new(|| {
    let info = os_info::get();
    OsDetails {
        name: info.os_type().to_string(),
        version: info.version().to_string(),
        codename: info.codename().map(str::to_string),
        edition: info.edition().map(str::to_string),
    }
});

/// The dashboard is a React + datum-ui app under `daemon/dashboard/`, built
/// by Vite into one self-contained file (JS, CSS and fonts inlined). The
/// built file is committed so building this crate never needs Node/Bun —
/// rebuild it with `task build:dashboard` after changing the dashboard.
async fn dashboard_page() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("../dashboard/dist/index.html"))
}

/// Non-sensitive daemon metadata the dashboard needs before a token is even
/// entered — e.g. to build a cloud-portal deep link for a tunnel
/// (`{portal_base_url}/project/{project_id}/edge/{tunnel_id}/overview`,
/// same pattern as `app/ui/src/util.rs`'s `tunnel_edge_portal_url`). Knowing
/// the project id or portal URL grants no capability by itself — every
/// real action still goes through the normal auth gate — so this is
/// unauthenticated, same as the dashboard shell itself.
///
/// The device/daemon fields feed the dashboard's Device page. Keep this
/// endpoint to facts that are harmless to any local process: nothing
/// path-like (the connect dir embeds the username), no relay config, no
/// token state — those belong behind the auth gate if they're ever needed.
async fn get_info(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({
        "project_id": state.project_id,
        "portal_base_url": "https://cloud.datum.net",
        "log_tail_max_lines": state.log_tail_max_lines,
        "device_name": connect_lib::friendly_device_name(),
        "hostname": gethostname::gethostname().to_string_lossy(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "os_name": OS_INFO.name,
        "os_version": OS_INFO.version,
        "os_codename": OS_INFO.codename,
        "os_edition": OS_INFO.edition,
        "daemon_version": env!("CARGO_PKG_VERSION"),
        "started_at_unix_ms": state.started_at_unix_ms,
        "max_tunnel_hours": state.max_tunnel_runtime.as_secs() / 3600,
    }))
}

/// Wraps `connect_lib::TunnelSummary` (flattened, so the wire shape is
/// unchanged for every existing field) with who most recently started it —
/// `setup` (a human), `operate:<token_id>` (an agent/script holding a
/// scoped token), or `None` if it's never been started within the retained
/// audit window. This is the data half of the "visible signal" the
/// original security model called for: the dashboard badges/toasts on
/// `operate:` actors so "an agent turned this on" is never silent. See
/// NOTES.md's "agent-friendly, human stays in control" discussion,
/// 2026-09-08.
#[derive(Serialize)]
struct TunnelSummaryWithActor {
    #[serde(flatten)]
    tunnel: connect_lib::TunnelSummary,
    last_start_actor: Option<String>,
}

async fn with_last_start_actor(state: &AppState, tunnel: connect_lib::TunnelSummary) -> TunnelSummaryWithActor {
    let last_start_actor = auth::last_actor_for_event(&state.connect_dir, &tunnel.id, "start").await;
    TunnelSummaryWithActor { tunnel, last_start_actor }
}

async fn list_tunnels(State(state): State<Arc<AppState>>) -> ApiResult<Vec<TunnelSummaryWithActor>> {
    let tunnels = state.control.list_active().await.map_err(err_response)?;
    let mut out = Vec::with_capacity(tunnels.len());
    for t in tunnels {
        out.push(with_last_start_actor(&state, t).await);
    }
    Ok(Json(out))
}

async fn get_tunnel(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<TunnelSummaryWithActor> {
    match state.control.get_active(&id).await {
        Ok(Some(t)) => Ok(Json(with_last_start_actor(&state, t).await)),
        Ok(None) => Err(not_found(&id)),
        Err(e) => Err(err_response(e)),
    }
}

/// Adds an explicit yes/no on top of the raw step list, so callers (the CI
/// action and, eventually, a Kubernetes controller's `Ready` condition) don't
/// have to re-implement step-matching themselves the way
/// `e2e-smoke-test.sh` historically did.
#[derive(Serialize)]
struct TunnelProgressWithStatus {
    #[serde(flatten)]
    progress: connect_lib::TunnelProgress,
    ready: bool,
    terminal_failure: bool,
}

async fn get_progress(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<TunnelProgressWithStatus> {
    match state.control.get_active_progress(&id).await {
        Ok(Some(p)) => {
            let ready = p.all_ready();
            let terminal_failure = p.terminal_failure().is_some();
            Ok(Json(TunnelProgressWithStatus { progress: p, ready, terminal_failure }))
        }
        Ok(None) => Err(not_found(&id)),
        Err(e) => Err(err_response(e)),
    }
}

#[cfg(test)]
mod progress_status_tests {
    use super::*;
    use connect_lib::{ProgressStep, ProgressStepKind, StepStatus};

    fn step(kind: ProgressStepKind, status: StepStatus, reason: Option<&str>) -> ProgressStep {
        ProgressStep {
            kind,
            status,
            reason: reason.map(str::to_string),
            message: None,
            resource: None,
        }
    }

    /// `#[serde(flatten)]` on `progress` must keep `hostnames`/`steps` at the
    /// top level alongside the new `ready`/`terminal_failure` fields, exactly
    /// as documented in API-REFERENCE.md — not nested under a `"progress"` key.
    #[test]
    fn wire_shape_flattens_progress_alongside_status_fields() {
        let progress = connect_lib::TunnelProgress {
            hostnames: vec!["foo-bar-12345.datumproxy.net".to_string()],
            steps: vec![step(ProgressStepKind::ProxyAccepted, StepStatus::Ready, None)],
        };
        let wrapped = TunnelProgressWithStatus {
            ready: progress.all_ready(),
            terminal_failure: progress.terminal_failure().is_some(),
            progress,
        };
        let value = serde_json::to_value(&wrapped).unwrap();
        assert!(value.get("progress").is_none(), "progress should be flattened, not nested");
        assert!(value.get("hostnames").is_some());
        assert!(value.get("steps").is_some());
        assert_eq!(value["ready"], serde_json::json!(true));
        assert_eq!(value["terminal_failure"], serde_json::json!(false));
    }

    #[test]
    fn terminal_failure_true_when_iroh_dns_owner_collision() {
        let progress = connect_lib::TunnelProgress {
            hostnames: vec![],
            steps: vec![step(
                ProgressStepKind::IrohDnsPublished,
                StepStatus::Pending,
                Some("DeferredToOwner"),
            )],
        };
        let wrapped = TunnelProgressWithStatus {
            ready: progress.all_ready(),
            terminal_failure: progress.terminal_failure().is_some(),
            progress,
        };
        let value = serde_json::to_value(&wrapped).unwrap();
        assert_eq!(value["ready"], serde_json::json!(false));
        assert_eq!(value["terminal_failure"], serde_json::json!(true));
    }
}

/// Real network-level stats for a running tunnel, straight from iroh's own
/// per-target proxy metrics (`iroh_proxy_utils::upstream::UpstreamMetrics`,
/// the same source the desktop app's bandwidth view already reads via
/// `ListenNode::metrics()`). `None` if the tunnel isn't currently running —
/// there's no live `ListenNode` to read from otherwise.
#[derive(Serialize)]
struct TunnelMetrics {
    bytes_to_origin: u64,
    bytes_from_origin: u64,
    accepted_requests: u64,
    denied_requests: u64,
    failed_requests: u64,
    active_requests: u64,
    active_iroh_connections: u64,
    total_iroh_connections: u64,
}

async fn get_metrics(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Option<TunnelMetrics>> {
    let tunnel = match state.control.get_active(&id).await {
        Ok(Some(t)) => t,
        Ok(None) => return Err(not_found(&id)),
        Err(e) => return Err(err_response(e)),
    };
    let running = state.running.lock().await;
    let Some(handle) = running.get(&id) else {
        return Ok(Json(None));
    };
    let Some(authority) = tunnel.origin_authority() else {
        return Ok(Json(None));
    };
    let node_metrics = handle.node.metrics();
    let target = node_metrics.get(&authority);
    Ok(Json(Some(TunnelMetrics {
        bytes_to_origin: target.as_ref().map(|t| t.bytes_to_origin()).unwrap_or(0),
        bytes_from_origin: target.as_ref().map(|t| t.bytes_from_origin()).unwrap_or(0),
        accepted_requests: target.as_ref().map(|t| t.accepted_requests()).unwrap_or(0),
        denied_requests: target.as_ref().map(|t| t.denied_requests()).unwrap_or(0),
        failed_requests: target.as_ref().map(|t| t.failed_requests()).unwrap_or(0),
        active_requests: target.as_ref().map(|t| t.active_requests()).unwrap_or(0),
        active_iroh_connections: node_metrics.active_iroh_connections(),
        total_iroh_connections: node_metrics.total_iroh_connections(),
    })))
}

async fn list_traffic(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Vec<inspector::ExchangeSummary>> {
    match state.inspectors.lock().await.get(&id) {
        Some(handle) => Ok(Json(handle.list())),
        None => Err(not_found(&id)),
    }
}

async fn get_traffic_exchange(
    State(state): State<Arc<AppState>>,
    Path((id, exchange_id)): Path<(String, String)>,
) -> ApiResult<inspector::CapturedExchange> {
    match state.inspectors.lock().await.get(&id) {
        Some(handle) => match handle.get(&exchange_id) {
            Some(exchange) => Ok(Json(exchange)),
            None => Err(not_found(&exchange_id)),
        },
        None => Err(not_found(&id)),
    }
}

async fn replay_traffic_exchange(
    State(state): State<Arc<AppState>>,
    Path((id, exchange_id)): Path<(String, String)>,
) -> ApiResult<inspector::ReplayResult> {
    let (real_target, exchange) = {
        let inspectors = state.inspectors.lock().await;
        let handle = inspectors.get(&id).ok_or_else(|| not_found(&id))?;
        let exchange = handle.get(&exchange_id).ok_or_else(|| not_found(&exchange_id))?;
        (handle.real_target().clone(), exchange)
    };

    if exchange.request.body_truncated {
        return Err(err_response(format!(
            "cannot replay '{exchange_id}': its captured request body was truncated at the {}KB capture cap, so replaying it would send an incomplete body under a mismatched or misleading length",
            inspector::CAPTURE_CAP_BYTES / 1024
        )));
    }

    let path_and_query: axum::http::uri::PathAndQuery = exchange
        .path
        .parse()
        .map_err(|e| err_response(format!("captured path '{}' is not replayable: {e}", exchange.path)))?;
    let mut target_parts = real_target.into_parts();
    target_parts.path_and_query = Some(path_and_query);
    let target_authority = target_parts.authority.as_ref().map(|a| a.as_str().to_string()).unwrap_or_default();
    let target_uri = axum::http::Uri::from_parts(target_parts)
        .map_err(|e| err_response(format!("bad replay target: {e}")))?;

    let mut builder = axum::http::Request::builder()
        .method(exchange.method.as_str())
        .uri(target_uri);
    for (k, v) in &exchange.request.headers {
        // Skip length/framing headers from the captured request: the
        // replay body below is reconstructed from captured bytes, and a
        // stale Content-Length copied verbatim from the original request
        // would mismatch it. Let the client compute the correct one from
        // the actual body being sent.
        if k.eq_ignore_ascii_case("content-length") || k.eq_ignore_ascii_case("transfer-encoding") {
            continue;
        }
        builder = builder.header(k, v);
    }
    let body = http_body_util::Full::new(axum::body::Bytes::from(exchange.request.body.clone()));
    let mut req = builder.body(body).map_err(|e| err_response(format!("bad replay request: {e}")))?;
    // The captured headers still carry the tunnel's public hostname in
    // Host, not the real target's — same fix as the live proxy path
    // (inspector::set_host_header), otherwise Host-validating targets
    // (Vite, Next.js, ...) reject the replay even though the identical
    // live request succeeds.
    inspector::set_host_header(req.headers_mut(), &target_authority);

    let client: hyper_util::client::legacy::Client<
        hyper_util::client::legacy::connect::HttpConnector,
        http_body_util::Full<axum::body::Bytes>,
    > = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build_http();

    let resp = tokio::time::timeout(std::time::Duration::from_secs(30), client.request(req))
        .await
        .map_err(|_| err_response("replay timed out after 30s — the real target may be unresponsive"))?
        .map_err(|e| err_response(format!("replay failed: {e}")))?;
    let status = resp.status();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("<binary>").to_string()))
        .collect();
    let body_bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .map_err(|e| err_response(format!("failed reading replay response: {e}")))?
        .to_bytes();

    Ok(Json(inspector::ReplayResult {
        status: status.as_u16(),
        headers,
        body: String::from_utf8_lossy(&body_bytes).into_owned(),
    }))
}

async fn create_tunnel(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateTunnelRequest>,
) -> ApiResult<connect_lib::TunnelSummary> {
    let normalized = if req.endpoint.starts_with("http://") || req.endpoint.starts_with("https://") {
        req.endpoint.clone()
    } else {
        format!("http://{}", req.endpoint)
    };
    let target_uri: axum::http::Uri = normalized
        .parse()
        .map_err(|e| err_response(format!("invalid endpoint '{}': {e}", req.endpoint)))?;

    let handle = inspector::start(target_uri).await.map_err(err_response)?;
    let inspector_endpoint = format!("http://{}", handle.local_addr);

    match state.control.create_active(&req.label, &inspector_endpoint).await {
        Ok(tunnel) => {
            // Persist the REAL target (not the inspector's ephemeral local
            // address) so a daemon restart can find it again — see
            // `connect_dir` and the reconciliation pass in `run()`.
            if let Err(e) = save_inspector_target(&state.connect_dir, &tunnel.id, &normalized).await {
                tracing::warn!(tunnel = %tunnel.id, "failed to persist real target, won't survive a daemon restart: {e:#}");
            }
            state.inspectors.lock().await.insert(tunnel.id.clone(), handle);
            auth::append_audit(&state.connect_dir, &state.audit_lock, "create", &tunnel.id, "setup").await;
            Ok(Json(tunnel))
        }
        Err(e) => Err(err_response(e)), // `handle` drops here, aborting the inspector task
    }
}

async fn delete_tunnel(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<connect_lib::TunnelDeleteOutcome> {
    // Delete server-side FIRST. The inspector holds the only record of this
    // tunnel's real target and its traffic history, with no way to recover
    // either once it's dropped — so it (and the running/heartbeat entry)
    // must only be torn down once we know the delete actually succeeded,
    // not optimistically beforehand. A transient delete_active failure now
    // leaves the tunnel fully intact and retryable instead of silently
    // losing local state for a tunnel that's still live server-side.
    let outcome = state.control.delete_active(&id).await.map_err(err_response)?;

    if let Some(running) = state.running.lock().await.remove(&id) {
        running.heartbeat.deregister_project(&state.project_id).await;
    }
    state.inspectors.lock().await.remove(&id); // dropped here, aborting its task
    remove_inspector_target(&state.connect_dir, &id).await;
    auth::delete_tokens_for_tunnel(&state.connect_dir, &id).await;
    auth::append_audit(&state.connect_dir, &state.audit_lock, "delete", &id, "setup").await;

    Ok(Json(outcome))
}

/// Releases a tunnel's `busy` reservation when dropped — covers every exit
/// path out of `start_tunnel`/`stop_tunnel_internal` (success,
/// `?`-propagated errors, panics unwinding), so a failed or completed
/// start/stop never leaves the id stuck reserved.
struct BusyGuard {
    state: Arc<AppState>,
    id: String,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.state.busy.lock().unwrap().remove(&self.id);
    }
}

/// Atomically claims the right to start/stop this tunnel id before any
/// await point. `HashSet::insert` returns false if already present, so
/// under concurrent calls exactly one wins this race; the rest get a 409
/// rather than racing ahead (e.g. two concurrent starts minting/rewiring
/// conflicting listen keys, or a start racing a not-yet-finished teardown).
fn claim_busy(state: &Arc<AppState>, id: &str) -> Result<BusyGuard, (StatusCode, Json<serde_json::Value>)> {
    if !state.busy.lock().unwrap().insert(id.to_string()) {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("tunnel '{id}' is busy (starting or stopping) — try again shortly") })),
        ));
    }
    Ok(BusyGuard { state: state.clone(), id: id.to_string() })
}

async fn start_tunnel(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Extension(actor): Extension<Actor>,
) -> ApiResult<connect_lib::TunnelSummary> {
    start_tunnel_internal(&state, &id, &actor.audit_label()).await.map(Json)
}

/// Shared by the `/start` handler and the startup reconciliation pass
/// (which auto-resumes any tunnel that was enabled before the daemon
/// last stopped — see the "recover a previously-enabled tunnel after a
/// restart/reboot" note in `run()`), so both go through the identical
/// mint-key/heartbeat/enable sequence rather than two implementations
/// drifting apart.
async fn start_tunnel_internal(
    state: &Arc<AppState>,
    id: &str,
    actor_label: &str,
) -> Result<connect_lib::TunnelSummary, (StatusCode, Json<serde_json::Value>)> {
    {
        let running = state.running.lock().await;
        if running.contains_key(id) {
            drop(running);
            return state
                .control
                .get_active(id)
                .await
                .map_err(err_response)?
                .ok_or_else(|| not_found(id));
        }
    }

    let _busy_guard = claim_busy(state, id)?;

    let tunnel = match state.control.get_active(id).await {
        Ok(Some(t)) => t,
        Ok(None) => return Err(not_found(id)),
        Err(e) => return Err(err_response(e)),
    };

    let (key, should_rewire) = resolve_listen_key(&state.repo, &state.project_id, id)
        .await
        .map_err(err_response)?;
    let node = ListenNode::new_with_key(state.repo.clone(), key.clone())
        .await
        .map_err(err_response)?;
    if should_rewire {
        state
            .repo
            .save_listen_key_for_tunnel(&state.project_id, id, &key)
            .await
            .map_err(err_response)?;
    }
    let service = TunnelService::new(state.datum.clone(), node.clone());

    // Heartbeat first so the relay/connection details are populated before
    // enabling — same ordering as `datum-connect listen` and for the same
    // reason (see that binary's comment on this).
    let heartbeat = HeartbeatAgent::new(state.datum.clone(), node.clone());
    heartbeat.start().await;
    heartbeat.register_project(&state.project_id).await;
    for _ in 0..40 {
        if node.endpoint().addr().relay_urls().next().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    if should_rewire {
        service
            .update_active(id, &tunnel.label, &tunnel.endpoint)
            .await
            .map_err(err_response)?;
        if let Err(e) = service.cleanup_orphaned_connectors().await {
            tracing::warn!("cleanup_orphaned_connectors after rewire failed: {e:#}");
        }
    }

    service.set_enabled_active(id, true).await.map_err(err_response)?;

    let current = service.get_active(id).await.map_err(err_response)?;

    state.running.lock().await.insert(
        id.to_string(),
        RunningTunnel {
            node,
            heartbeat,
            service,
            started_at: std::time::Instant::now(),
            started_at_unix_ms: current_unix_ms(),
        },
    );
    auth::append_audit(&state.connect_dir, &state.audit_lock, "start", id, actor_label).await;

    current.ok_or_else(|| not_found(id))
}

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

enum StopReason {
    Manual,
    AutoExpired,
}

impl StopReason {
    fn audit_event(&self) -> &'static str {
        match self {
            StopReason::Manual => "stop",
            StopReason::AutoExpired => "auto_expired",
        }
    }
}

/// Shared by the `/stop` handler and the auto-expiry sweep, so both go
/// through the exact same atomic check-and-take: `HashMap::remove` under
/// the single `running` mutex either wins (proceeds to tear down) or finds
/// nothing (idempotent no-op) — there's no separate "check, then act" gap
/// for a second caller to race, which matters now that the sweep is a
/// second caller of "stop this tunnel" alongside the human/agent-facing one.
async fn stop_tunnel_internal(
    state: &Arc<AppState>,
    id: &str,
    reason: StopReason,
    actor_label: &str,
) -> ApiResult<connect_lib::TunnelSummary> {
    let _busy_guard = claim_busy(state, id)?;

    let running = state.running.lock().await.remove(id);
    let disable_result = match &running {
        Some(r) => r.service.set_enabled_active(id, false).await,
        None => state.control.set_enabled_active(id, false).await,
    };
    if let Some(r) = running {
        r.heartbeat.deregister_project(&state.project_id).await;
    }
    let result = disable_result.map_err(err_response)?;
    auth::append_audit(&state.connect_dir, &state.audit_lock, reason.audit_event(), id, actor_label).await;
    Ok(Json(result))
}

async fn stop_tunnel(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Extension(actor): Extension<Actor>,
) -> ApiResult<connect_lib::TunnelSummary> {
    stop_tunnel_internal(&state, &id, StopReason::Manual, &actor.audit_label()).await
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("{:#}", err);
        std::process::exit(1);
    }
}

async fn run() -> n0_error::Result<()> {
    let _ = rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| n0_error::anyerr!("failed to install ring crypto provider for rustls"))?;

    let args = Args::parse();

    let session = std::env::var("DATUM_SESSION").ok();
    if session.is_none() && std::env::var("DATUM_PLUGIN_MODE").map(|v| v != "1").unwrap_or(true) {
        return Err(n0_error::anyerr!(
            "neither DATUM_SESSION nor DATUM_PLUGIN_MODE=1 set — this daemon runs in plugin mode only"
        ));
    }

    let token_source = ExternalTokenSource::from_env(session.clone())
        .map_err(|e| n0_error::anyerr!("failed to create token source: {e}"))?;
    if let Some(ref s) = session {
        if let Ok(helper) = std::env::var("DATUM_CREDENTIALS_HELPER") {
            token_source.start_refresh(helper, s.clone());
        }
    }
    let datum = DatumCloudClient::with_external_token_source(ApiEnv::default(), token_source);

    let project_id = args
        .project
        .ok_or_else(|| n0_error::anyerr!("no project set — pass --project or set DATUM_PROJECT"))?;
    datum
        .set_selected_context(Some(SelectedContext {
            project_id: project_id.clone(),
            project_name: project_id.clone(),
            org_id: String::new(),
            org_name: String::new(),
            org_type: String::new(),
        }))
        .await?;

    let repo_path = match args.repo {
        Some(p) => p,
        None => Repo::default_location()
            .map_err(|e| n0_error::anyerr!("{e}"))?,
    };

    // Two destinations for the same log stream: stderr (unchanged for
    // anything already redirecting it, e.g. `_run_dashboard_demo.sh`) and
    // `daemon.log` under the connect dir, so the dashboard's "Logs" section
    // has something to tail out of the box — see LOG-TAIL-PLAN.md. Needs
    // `repo_path` resolved first, so this runs later than a typical
    // "first thing in main" logging setup would.
    let env_filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("datum_connect_daemon=info,connect_lib=info"))
    };
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(env_filter());
    let daemon_log_path = repo_path.join("daemon.log");
    let file_appender = tracing_appender::rolling::never(&repo_path, "daemon.log");
    let (file_writer, file_guard) = tracing_appender::non_blocking(file_appender);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false)
        .with_filter(env_filter());
    tracing_subscriber::registry().with(stderr_layer).with(file_layer).init();
    // Held for the daemon's whole lifetime — dropping it stops the
    // non-blocking writer's background flush thread.
    let _file_guard = file_guard;

    let repo = Repo::open_or_create(repo_path.clone()).await?;

    let control_node = ListenNode::new(repo.clone()).await?;
    let control = TunnelService::new(datum.clone(), control_node);

    let setup_token = auth::load_or_create_setup_token(&repo_path)
        .await
        .map_err(|e| n0_error::anyerr!("failed to load/create setup token: {e}"))?;
    let viewer_token = auth::load_viewer_token(&repo_path)
        .await
        .map_err(|e| n0_error::anyerr!("failed to load viewer token: {e}"))?;

    let peer_state = peer::PeerState::new(repo.clone(), repo_path.clone())
        .await
        .map_err(|e| n0_error::anyerr!("failed to load peer state: {e}"))?;
    let log_sources = logs::LogSources::load(&repo_path)
        .await
        .map_err(|e| n0_error::anyerr!("failed to load log sources: {e}"))?;

    let state = Arc::new(AppState {
        datum,
        repo,
        project_id,
        control,
        running: Mutex::new(HashMap::new()),
        busy: StdMutex::new(HashSet::new()),
        inspectors: Mutex::new(HashMap::new()),
        connect_dir: repo_path,
        setup_token,
        viewer_token: RwLock::new(viewer_token),
        max_tunnel_runtime: std::time::Duration::from_secs(args.max_tunnel_hours.saturating_mul(3600)),
        audit_lock: Mutex::new(()),
        peer: peer_state,
        log_sources,
        log_tail_max_lines: args.log_tail_max_lines,
        started_at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    });

    // Auto-register the daemon's own log file as a built-in tailable source
    // (see LOG-TAIL-PLAN.md) — reuses `logs::register` verbatim rather than
    // special-casing "self log" registration, the same path any user-added
    // source goes through. Skipped when already registered with the same
    // path so a restart doesn't spam the audit log with a repeat
    // `log_source_added` event every time.
    // `register` stores a canonicalized path (falling back to the as-given
    // path if that fails), so the comparison here has to canonicalize the
    // same way — otherwise this never matches on Windows, where
    // canonicalize adds a `\\?\` prefix, and every restart would re-add a
    // `log_source_added` audit entry despite pointing at the same file.
    let canonical_daemon_log_path =
        tokio::fs::canonicalize(&daemon_log_path).await.unwrap_or_else(|_| daemon_log_path.clone());
    let already_registered = state
        .log_sources
        .list()
        .await
        .iter()
        .any(|(name, path)| name == "daemon" && path == &canonical_daemon_log_path);
    if !already_registered {
        if let Err((_, Json(err))) = logs::register(
            State(state.clone()),
            Json(logs::RegisterLogSourceRequest {
                name: "daemon".to_string(),
                path: daemon_log_path.display().to_string(),
            }),
        )
        .await
        {
            tracing::warn!("failed to auto-register daemon log source: {err}");
        }
    }

    // Reconcile inspectors for any tunnel that was already enabled before
    // this daemon (re)started. Its old inspector process died with the
    // previous process, but the real target was persisted to disk in
    // `create_tunnel` — without this, every pre-existing tunnel's traffic
    // history and real-target mapping would be permanently unrecoverable
    // after a routine restart.
    match state.control.list_active().await {
        Ok(tunnels) => {
            // Every existing tunnel gets its inspector reconciled here,
            // regardless of enabled/disabled state — the inspector's
            // lifecycle is tied to the tunnel profile existing at all, not
            // to whether it's currently turned on (mirrors create_tunnel,
            // which starts one unconditionally). A disabled tunnel skipped
            // here would keep a persisted endpoint pointing at a dead
            // inspector forever, since start_tunnel doesn't create or
            // repoint one itself — it relies entirely on one already
            // existing in state.inspectors by the time it runs.
            for t in tunnels {
                let Some(target) = load_inspector_target(&state.connect_dir, &t.id).await else {
                    tracing::warn!(tunnel = %t.id, "no persisted real target found, cannot reconcile inspector");
                    continue;
                };
                let target_uri: axum::http::Uri = match target.parse() {
                    Ok(u) => u,
                    Err(e) => {
                        tracing::warn!(tunnel = %t.id, %target, "persisted target is not a valid URI, skipping: {e}");
                        continue;
                    }
                };
                let was_enabled = t.enabled;
                let tunnel_id = t.id.clone();
                match inspector::start(target_uri).await {
                    Ok(handle) => {
                        let inspector_endpoint = format!("http://{}", handle.local_addr);
                        if let Err(e) =
                            state.control.update_active(&t.id, &t.label, &inspector_endpoint).await
                        {
                            tracing::warn!(tunnel = %t.id, "failed to repoint tunnel at reconciled inspector: {e:#}");
                            continue;
                        }
                        tracing::info!(tunnel = %t.id, %target, "reconciled inspector after restart");
                        state.inspectors.lock().await.insert(t.id, handle);

                        // Recover a tunnel that was live (enabled) before
                        // the daemon last stopped — whether from a crash,
                        // a manual restart, or a full machine reboot.
                        // Without this, reconciliation only brings the
                        // inspector back; the tunnel would report
                        // "enabled" in its profile but silently serve no
                        // traffic until someone noticed and called
                        // /start by hand.
                        if was_enabled {
                            match start_tunnel_internal(&state, &tunnel_id, "system").await {
                                Ok(_) => tracing::info!(tunnel = %tunnel_id, "auto-resumed previously-enabled tunnel after restart"),
                                Err((status, body)) => tracing::warn!(
                                    tunnel = %tunnel_id,
                                    %status,
                                    error = %body.0,
                                    "failed to auto-resume previously-enabled tunnel after restart — it will stay reachable via its inspector reconciliation above, but won't serve live traffic until 'start' is called manually"
                                ),
                            }
                        }
                    }
                    Err(e) => tracing::warn!(tunnel = %t.id, "failed to start reconciled inspector: {e:#}"),
                }
            }
        }
        Err(e) => tracing::warn!("failed to list active tunnels for inspector reconciliation: {e:#}"),
    }

    // Auto-expiry backstop: force-stop any tunnel that's been enabled
    // longer than `max_tunnel_runtime`, regardless of who started it. This
    // is the second-ever caller of "stop this tunnel" — see
    // `stop_tunnel_internal`'s doc comment for why that required an
    // atomicity fix, not just a new call site.
    {
        let sweep_state = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                let expired: Vec<String> = {
                    let running = sweep_state.running.lock().await;
                    running
                        .iter()
                        .filter(|(_, r)| r.started_at.elapsed() > sweep_state.max_tunnel_runtime)
                        .map(|(id, _)| id.clone())
                        .collect()
                };
                for id in expired {
                    tracing::info!(tunnel = %id, "auto-expiry backstop firing, force-stopping");
                    if let Err(e) =
                        stop_tunnel_internal(&sweep_state, &id, StopReason::AutoExpired, "system").await
                    {
                        tracing::warn!(tunnel = %id, "auto-expiry stop failed: {e:?}");
                    }
                }
            }
        });
    }

    // Tier partitioning, not per-handler auth checks: which routes require
    // which credential is the shape of this route table, not something
    // buried in N handler bodies — see NOTES.md's "Local API auth" section
    // for why. `/traffic` and `/replay` are viewer-accessible, not
    // setup-only — see the `setup_or_viewer` block below for why, and
    // NOTES.md's security-review section for the redaction work that made
    // that an acceptable scope instead of a real exposure.
    let setup_only = Router::new()
        .route("/v1/tunnels", post(create_tunnel))
        .route("/v1/tunnels/:id", axum::routing::delete(delete_tunnel))
        .route("/v1/tunnels/:id/tokens", get(auth::list_tokens).post(auth::create_token))
        .route("/v1/tunnels/:id/tokens/:token_id", axum::routing::delete(auth::revoke_token))
        .route("/v1/viewer-token", get(auth::get_viewer_token_status).post(auth::create_viewer_token).delete(auth::revoke_viewer_token))
        .route("/v1/audit", get(auth::get_audit))
        .route("/v1/peers/advertise", post(peer::advertise))
        .route("/v1/peers/advertise/:resource_id", axum::routing::delete(peer::revoke))
        .route("/v1/peers/connect", post(peer::connect))
        .route("/v1/peers/connections/:id", axum::routing::delete(peer::disconnect))
        .route("/v1/logs", post(logs::register))
        .route("/v1/logs/:name", axum::routing::delete(logs::remove))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_setup));

    let setup_or_operate = Router::new()
        .route("/v1/tunnels/:id/start", post(start_tunnel))
        .route("/v1/tunnels/:id/stop", post(stop_tunnel))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_operate_or_setup));

    let setup_or_operate_or_viewer = Router::new()
        .route("/v1/tunnels/:id/progress", get(get_progress))
        .route("/v1/tunnels/:id/metrics", get(get_metrics))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_setup_or_operate_or_viewer,
        ));

    // `/traffic` and `/replay` are viewer-accessible (that's the whole
    // point of the viewer tier — the dashboard shows captured traffic),
    // but not operate — an operate token is scoped to acting on one
    // tunnel it already knows the id of, not to browsing. `/v1/peers`
    // (list, read-only) joins them for the same reason — the dashboard's
    // peer-tunnels section needs it — while advertise/connect/revoke/
    // disconnect stay setup-only, same as tunnel create: they're the
    // actions that determine what's reachable, not just viewing it. Same
    // split again for `/v1/logs`: registering a source (`logs::register`,
    // setup-only above) decides which file becomes readable at all;
    // listing sources and reading a tail here is just viewing what's
    // already been approved — see `logs.rs` and `LOG-TAIL-PLAN.md`.
    let setup_or_viewer = Router::new()
        .route("/v1/tunnels", get(list_tunnels))
        .route("/v1/tunnels/:id", get(get_tunnel))
        .route("/v1/tunnels/:id/traffic", get(list_traffic))
        .route("/v1/tunnels/:id/traffic/:exchange_id", get(get_traffic_exchange))
        .route("/v1/tunnels/:id/traffic/:exchange_id/replay", post(replay_traffic_exchange))
        .route("/v1/peers", get(peer::list))
        .route("/v1/logs", get(logs::list))
        .route("/v1/logs/:name/tail", get(logs::tail))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::require_setup_or_viewer));

    // The dashboard's HTML/JS shell carries no secrets of its own — the
    // viewer pastes a token into the page, which then hits the same
    // gated /v1/... routes as any other client. So the shell itself is
    // served unauthenticated; nothing behind it is.
    let public = Router::new()
        .route("/", get(dashboard_page))
        .route("/v1/info", get(get_info));

    let app = public
        .merge(setup_only)
        .merge(setup_or_operate)
        .merge(setup_or_operate_or_viewer)
        .merge(setup_or_viewer)
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    tracing::info!(%addr, "datum-connect-daemon listening");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| n0_error::anyerr!("failed to bind {addr}: {e}"))?;
    axum::serve(listener, app)
        .await
        .map_err(|e| n0_error::anyerr!("server error: {e}"))?;

    Ok(())
}
