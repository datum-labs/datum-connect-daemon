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

mod addon_page;
mod auth;
mod exclusive;
mod ingress;
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
    /// Its key is new on every start, so anything that finds or creates a
    /// Connector for it (`update_active`, enabling) would make a new one
    /// per restart. Repoint an endpoint through `retarget_active` instead.
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
    /// tunnel id, started at profile-create time (not tied to start/stop),
    /// or by a start that names a target the tunnel has no inspector for.
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
    /// Parsed `Args::waf_exempt_matches`, handed to every per-tunnel
    /// `TunnelService` so its enable path writes the same rules as
    /// `control` does (otherwise each start would undo the split).
    waf_exempt_matches: Vec<connect_lib::datum_apis::http_proxy::HTTPRouteMatch>,
    /// `Args::edge_policies`, handed to every per-tunnel `TunnelService`
    /// for the same reason: the enable path is where the policies are
    /// (re)ensured.
    edge_policies: bool,
    /// When this daemon process started, for the dashboard's uptime.
    started_at_unix_ms: u128,
    /// `Args::exclusive_label`: the one tunnel the Home Assistant add-on
    /// owns. See `exclusive.rs`.
    exclusive_label: Option<String>,
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

/// A tunnel's real local target, as a caller gave it, after the one
/// normalisation `create_tunnel` has always applied: a bare `host:port`
/// means plain HTTP. `normalized` is what gets persisted, `uri` is what the
/// inspector forwards to.
struct RealTarget {
    normalized: String,
    uri: axum::http::Uri,
}

fn parse_real_target(raw: &str) -> Result<RealTarget, axum::http::uri::InvalidUri> {
    let normalized = if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let uri = normalized.parse()?;
    Ok(RealTarget { normalized, uri })
}

/// Whether a tunnel must get a fresh inspector before it can serve `wanted`:
/// it has none in this process (its local state was lost, e.g. the Home
/// Assistant add-on reinstalled and adopted a tunnel a previous install
/// created), or the one it has forwards somewhere else. A trailing slash is
/// not a different target.
fn needs_repoint(has_inspector: bool, persisted: Option<&str>, wanted: &str) -> bool {
    !has_inspector || persisted.map(|p| p.trim_end_matches('/')) != Some(wanted.trim_end_matches('/'))
}

/// Starts an inspector forwarding to `target`, returning it with the
/// endpoint the tunnel's HTTPProxy should point at instead of the target.
async fn spawn_inspector(target: &RealTarget) -> n0_error::Result<(InspectorHandle, String)> {
    let handle = inspector::start(target.uri.clone()).await?;
    let endpoint = format!("http://{}", handle.local_addr);
    Ok((handle, endpoint))
}

/// Makes `handle` the tunnel's inspector: persists the REAL target (not the
/// inspector's ephemeral local address) so a daemon restart can find it
/// again — see `connect_dir` and the reconciliation pass in `run()` — and
/// replaces any previous inspector, whose task aborts as it drops. Only
/// call this once the HTTPProxy points at `handle`, or a failed repoint
/// leaves the persisted target describing an inspector nothing uses.
async fn install_inspector(
    connect_dir: &std::path::Path,
    inspectors: &Mutex<HashMap<String, InspectorHandle>>,
    id: &str,
    target: &str,
    handle: InspectorHandle,
) {
    if let Err(e) = save_inspector_target(connect_dir, id, target).await {
        tracing::warn!(tunnel = %id, "failed to persist real target, won't survive a daemon restart: {e:#}");
    }
    inspectors.lock().await.insert(id.to_string(), handle);
}

/// Free-text note a human attaches to a tunnel via the CLI (`tunnel api note
/// set/clear`) so it's still obvious what a tunnel is for once there are
/// 10+ of them running. Deliberately not part of `TunnelSummary`/`state.yml`
/// — every field there round-trips through Datum Cloud's own
/// `HTTPProxy`/`Connector` CRDs, and a note has nothing to do with that
/// resource. Stored the same way as `daemon_inspector_targets/<id>.txt`
/// above: one small flat file per tunnel id, no JSON wrapper needed for a
/// single free-text field.
const NOTE_MAX_BYTES: usize = 2000;

fn note_dir(base: &std::path::Path) -> std::path::PathBuf {
    base.join("daemon_notes")
}

fn note_path(base: &std::path::Path, id: &str) -> std::path::PathBuf {
    note_dir(base).join(format!("{id}.txt"))
}

async fn save_note(base: &std::path::Path, id: &str, note: &str) -> std::io::Result<()> {
    if note.is_empty() {
        return remove_note(base, id).await.map(|_| ());
    }
    let dir = note_dir(base);
    tokio::fs::create_dir_all(&dir).await?;
    tokio::fs::write(note_path(base, id), note).await
}

async fn load_note(base: &std::path::Path, id: &str) -> Option<String> {
    tokio::fs::read_to_string(note_path(base, id)).await.ok()
}

async fn remove_note(base: &std::path::Path, id: &str) -> std::io::Result<()> {
    match tokio::fs::remove_file(note_path(base, id)).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
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
    /// JSON array of HTTPRoute matches that get their own `streams` rule on
    /// every tunnel's HTTPProxy, ahead of a `protected` rule for the rest,
    /// so a WAF scoped to `protected` skips them. Set by the Home Assistant
    /// add-on for HA's streaming endpoints, which Datum's WAF holds back
    /// (infra#6677). Unset (the default) leaves the rules exactly as before.
    /// Invalid JSON stops the daemon at startup: silently ignoring it would
    /// leave a `protected`-scoped WAF matching nothing.
    #[clap(long, env = "DATUM_TUNNEL_WAF_EXEMPT_MATCHES")]
    waf_exempt_matches: Option<String>,
    /// On create and on every enable, also ensure the tunnel's edge
    /// policies: a WAF scoped to the `protected` rule and a 1h request
    /// timeout (see `connect_lib::edge_policies`). Set by the Home Assistant
    /// add-on, alongside `DATUM_TUNNEL_WAF_EXEMPT_MATCHES`. Off (the
    /// default) touches no policy. Failures are logged, never fatal.
    #[clap(
        long,
        env = "DATUM_TUNNEL_EDGE_POLICIES",
        default_value_t = false,
        action = clap::ArgAction::Set,
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    edge_policies: bool,
    /// The Home Assistant add-on's `tunnel_label`: the add-on owns exactly
    /// one tunnel, the first with this label. Every other tunnel this daemon
    /// has local state for is then an older one: not resumed at start,
    /// stopped if on, and offered for removal on the add-on's page. Tunnels
    /// it has no local state for (other machines in a shared project) are
    /// never touched. Unset (the default), every tunnel is as before. See
    /// `exclusive.rs`.
    #[clap(long, env = "DATUM_TUNNEL_EXCLUSIVE_LABEL")]
    exclusive_label: Option<String>,
    /// Serve the Home Assistant add-on's page (ingress) on this port: the
    /// tunnel's status, Re-pair and Unpair for the daemon; the "Connect to
    /// Datum" flow for `setup`. Set by the add-on. Unset, there is no page.
    /// See `ingress.rs`.
    #[clap(long, env = "DATUM_INGRESS_PORT", global = true)]
    ingress_port: Option<u16>,
    /// Where the page listens: `auto` is the address the Home Assistant
    /// Supervisor's ingress proxy connects to, asked of the Supervisor
    /// (loopback outside an add-on), or an IP. Never every address: with
    /// host networking that is the LAN.
    #[clap(long, env = "DATUM_INGRESS_BIND", default_value = "auto", global = true)]
    ingress_bind: String,
    /// Whom the page answers: `auto` is the Supervisor's own address (the
    /// ingress proxy), or a comma-separated list of IPs. Everyone else is
    /// refused.
    #[clap(long, env = "DATUM_INGRESS_ALLOW", default_value = "auto", global = true)]
    ingress_allow: String,
    /// Where pairing saves its key. The page's Re-pair and Unpair only ever
    /// delete a key at this path, and only when the daemon runs on it.
    #[clap(long, env = "DATUM_PAIRED_KEY_FILE", global = true)]
    paired_key_file: Option<std::path::PathBuf>,
    /// Without a subcommand, runs the daemon.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Get a service account key by approving this device in a browser,
    /// instead of making one in the portal. Prints a link and a code, waits
    /// for approval, creates a service account with `editor` on the project
    /// and a key for it, saves the key, and exits 0. Used by the Home
    /// Assistant add-on when no key is configured; see
    /// `connect_lib::datum_cloud::pairing`.
    Pair {
        /// The project to pair with. Empty or omitted means the only
        /// project the approving login can see.
        #[clap(long)]
        project: Option<String>,
        /// Where to write the key file. Must not exist yet.
        #[clap(long)]
        key_out: std::path::PathBuf,
        /// Accept (and at once close) connections on 127.0.0.1:<port> while
        /// pairing waits, so a watchdog that checks the daemon's port by TCP
        /// connect does not restart the process mid-approval. Released
        /// before `pair` exits, so the daemon can bind it straight after.
        #[clap(long)]
        hold_port: Option<u16>,
        /// Where to read a project chosen while pairing waits, when the
        /// approving login can see several and none is set (or not the one
        /// set): `supervisor` (the add-on's saved options, read from the
        /// Home Assistant Supervisor), `file:<path>` (a JSON file with a
        /// `project` field, re-read every poll), or `none` (stop and list
        /// them, as before). `auto` is `supervisor` inside an add-on, where
        /// SUPERVISOR_TOKEN is set, and `none` elsewhere.
        #[clap(long, env = "DATUM_PAIRING_OPTIONS_SOURCE", default_value = "auto")]
        options_source: OptionsSource,
        /// Keep the approving login in this file (0600) while pairing waits
        /// for a project, so that a restart in the meantime continues
        /// without a new approval. Saving the add-on's options offers a
        /// restart, so the add-on passes /data/pairing-session.json. The
        /// file is deleted as soon as pairing ends. Omitted, the login is
        /// kept in memory only.
        #[clap(long, env = "DATUM_PAIRING_SESSION_FILE")]
        session_file: Option<std::path::PathBuf>,
    },
    /// `pair`, from a page: serve the add-on's page on `--ingress-port`,
    /// start pairing when "Connect to Datum" is clicked there, let the
    /// project be chosen from a list, and exit 0 once the key is saved.
    /// The link and code are still logged and shown as a notification, and
    /// `project` saved on the Configuration tab still counts. If the page
    /// cannot be served, this is `pair`. Used by the Home Assistant add-on
    /// since 0.3.0.
    Setup {
        /// Preselected on the page; the person still confirms it.
        #[clap(long)]
        project: Option<String>,
        #[clap(long)]
        key_out: std::path::PathBuf,
        /// As for `pair`.
        #[clap(long)]
        hold_port: Option<u16>,
        /// As for `pair`: where else a chosen project may come from.
        #[clap(long, env = "DATUM_PAIRING_OPTIONS_SOURCE", default_value = "auto")]
        options_source: OptionsSource,
        /// As for `pair`.
        #[clap(long, env = "DATUM_PAIRING_SESSION_FILE")]
        session_file: Option<std::path::PathBuf>,
    },
}

/// How long `setup` keeps offering new codes after "Connect to Datum": a
/// person clicked and is looking, so a code nobody approves within this
/// ends in "Get a new code" rather than rotating for an hour.
const SETUP_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(20 * 60);
/// How long `setup` keeps the page up after the key is saved, so that the
/// page shows "Done" before the daemon's own page replaces it.
const SETUP_HANDOVER: std::time::Duration = std::time::Duration::from_secs(3);

/// `pair`'s exit status when the add-on is stopped while pairing waits with
/// its login saved: not a failure, since the next start carries on.
/// EX_TEMPFAIL. run.sh checks for it.
const PAIR_PAUSED_EXIT: i32 = 75;

/// `pair --options-source`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum OptionsSource {
    Auto,
    Supervisor,
    File(std::path::PathBuf),
    None,
}

impl std::str::FromStr for OptionsSource {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(Self::Auto),
            "supervisor" => Ok(Self::Supervisor),
            "none" => Ok(Self::None),
            _ => match s.strip_prefix("file:") {
                Some(path) if !path.is_empty() => Ok(Self::File(path.into())),
                _ => Err(format!("expected auto, supervisor, none or file:<path>, not {s:?}")),
            },
        }
    }
}

impl OptionsSource {
    /// What pairing waits on, if anything. Asking for the Supervisor
    /// without one only costs the wait, so it is a warning, not an error.
    fn resolve(
        &self,
        supervisor: Option<&connect_lib::datum_cloud::ha_supervisor::Supervisor>,
    ) -> Option<Arc<dyn connect_lib::datum_cloud::pairing::ProjectSource>> {
        use connect_lib::datum_cloud::ha_supervisor::OptionsFile;
        match self {
            Self::Auto => supervisor.map(|s| Arc::new(s.clone()) as _),
            Self::Supervisor => {
                if supervisor.is_none() {
                    tracing::warn!(
                        "pair: --options-source supervisor, but SUPERVISOR_TOKEN is not set; with several projects, pairing stops instead of waiting"
                    );
                }
                supervisor.map(|s| Arc::new(s.clone()) as _)
            }
            Self::File(path) => Some(Arc::new(OptionsFile(path.clone()))),
            Self::None => None,
        }
    }
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
    /// See `save_note`/`load_note` — CLI-only, never set from the dashboard.
    note: Option<String>,
}

async fn with_last_start_actor(state: &AppState, tunnel: connect_lib::TunnelSummary) -> TunnelSummaryWithActor {
    let last_start_actor = auth::last_actor_for_event(&state.connect_dir, &tunnel.id, "start").await;
    let note = load_note(&state.connect_dir, &tunnel.id).await;
    TunnelSummaryWithActor { tunnel, last_start_actor, note }
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
mod args_tests {
    use super::*;

    /// Unset means off, so the desktop app and datumctl, which never set
    /// it, keep their behaviour. The add-on's `1` and the usual spellings
    /// turn it on.
    #[test]
    fn edge_policies_default_off_and_accept_boolish_values() {
        if std::env::var_os("DATUM_TUNNEL_EDGE_POLICIES").is_none() {
            let args = Args::try_parse_from(["datum-connect-daemon"]).unwrap();
            assert!(!args.edge_policies);
        }
        for (raw, want) in [("1", true), ("true", true), ("yes", true), ("0", false), ("false", false)] {
            let args =
                Args::try_parse_from(["datum-connect-daemon", "--edge-policies", raw]).unwrap();
            assert_eq!(args.edge_policies, want, "{raw}");
        }
    }

    #[test]
    fn exclusive_label_parses_and_defaults_off() {
        if std::env::var_os("DATUM_TUNNEL_EXCLUSIVE_LABEL").is_none() {
            assert_eq!(Args::try_parse_from(["datum-connect-daemon"]).unwrap().exclusive_label, None);
        }
        let args = Args::try_parse_from(["datum-connect-daemon", "--exclusive-label", "home-assistant"]).unwrap();
        assert_eq!(args.exclusive_label.as_deref(), Some("home-assistant"));
    }

    #[test]
    fn the_proxy_hint_only_when_something_is_missing() {
        use connect_lib::datum_cloud::ha_core::ProxySetup;
        assert_eq!(proxy_hint(None), None);
        assert_eq!(proxy_hint(Some(&ProxySetup::Ready)), None);
        let needed = ProxySetup::Needed { config: serde_json::json!({}) };
        assert!(proxy_hint(Some(&needed)).unwrap().contains("click Allow"));
        assert!(proxy_hint(Some(&ProxySetup::Pending)).unwrap().contains("Settings > System > Network"));
    }

    /// The add-on runs `pair --project "$PROJECT" --key-out <path>`, with
    /// an empty project when none is set.
    #[test]
    fn pair_subcommand_parses() {
        let args = Args::try_parse_from([
            "datum-connect-daemon", "pair", "--project", "p-1", "--key-out", "/data/k.json",
        ])
        .unwrap();
        match args.command {
            Some(Command::Pair { project, key_out, hold_port: None, options_source: OptionsSource::Auto, session_file: None }) => {
                assert_eq!(project.as_deref(), Some("p-1"));
                assert_eq!(key_out, std::path::PathBuf::from("/data/k.json"));
            }
            other => panic!("{other:?}"),
        }
        let args =
            Args::try_parse_from(["datum-connect-daemon", "pair", "--key-out", "k.json"]).unwrap();
        assert!(matches!(args.command, Some(Command::Pair { project: None, .. })));
        assert!(Args::try_parse_from(["datum-connect-daemon", "pair"]).is_err(), "--key-out is required");
        // No subcommand still means "run the daemon".
        let args = Args::try_parse_from(["datum-connect-daemon", "--port", "1"]).unwrap();
        assert!(args.command.is_none());
    }

    /// The watchdog's view: while held, a connect succeeds; once released,
    /// the daemon can bind the same port.
    #[tokio::test]
    async fn held_port_answers_and_is_free_after_release() {
        let holder = PortHolder::bind(0).await.unwrap();
        let addr = holder.addr;
        for _ in 0..3 {
            tokio::net::TcpStream::connect(addr).await.expect("watchdog connect");
        }
        holder.release().await;
        let rebound = tokio::net::TcpListener::bind(addr).await.expect("port free after release");
        drop(rebound);
    }

    #[tokio::test]
    async fn a_busy_port_cannot_be_held() {
        let busy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = busy.local_addr().unwrap().port();
        assert!(PortHolder::bind(port).await.is_err());
    }

    #[test]
    fn options_source_parses() {
        let parse = |extra: &[&str]| {
            let mut argv = vec!["datum-connect-daemon", "pair", "--key-out", "k.json"];
            argv.extend_from_slice(extra);
            match Args::try_parse_from(argv).map(|a| a.command) {
                Ok(Some(Command::Pair { options_source, .. })) => Ok(options_source),
                Ok(other) => panic!("{other:?}"),
                Err(e) => Err(e.to_string()),
            }
        };
        assert_eq!(parse(&["--options-source", "supervisor"]).unwrap(), OptionsSource::Supervisor);
        assert_eq!(parse(&["--options-source", "none"]).unwrap(), OptionsSource::None);
        assert_eq!(
            parse(&["--options-source", "file:/tmp/o.json"]).unwrap(),
            OptionsSource::File("/tmp/o.json".into())
        );
        assert!(parse(&["--options-source", "file:"]).is_err());
        assert!(parse(&["--options-source", "bogus"]).is_err());
    }

    /// Outside an add-on there is no SUPERVISOR_TOKEN, so `auto` neither
    /// waits nor notifies: the log is all there is, as before.
    #[test]
    fn options_source_without_a_supervisor() {
        use connect_lib::datum_cloud::ha_supervisor::Supervisor;
        assert!(OptionsSource::Auto.resolve(None).is_none());
        assert!(OptionsSource::Supervisor.resolve(None).is_none());
        assert!(OptionsSource::None.resolve(None).is_none());
        assert!(OptionsSource::File("o.json".into()).resolve(None).is_some());
        let sup = Supervisor::new("http://127.0.0.1:9", "t".to_string().into()).unwrap();
        assert!(OptionsSource::Auto.resolve(Some(&sup)).is_some());
        assert!(OptionsSource::None.resolve(Some(&sup)).is_none());
    }

    #[test]
    fn choose_project_log_lists_every_project() {
        use connect_lib::datum_cloud::pairing::ProjectChoice;
        let projects = vec![
            ProjectChoice { id: "p-1".into(), display_name: "Home".into(), organization: "o-1".into() },
            ProjectChoice { id: "p-2".into(), display_name: "Garage".into(), organization: "o-2".into() },
        ];
        let first = choose_project_line(&projects, None, std::time::Duration::from_secs(1800));
        assert_eq!(
            first,
            "Your Datum login can see 2 projects. Set 'project' on the add-on's Configuration tab to one of these ids and click Save. Home Assistant offers to restart the add-on when you save; either way, pairing continues without a new login (waiting up to 30 minutes):\n  p-1 (Home, organization o-1)\n  p-2 (Garage, organization o-2)"
        );
        let again = choose_project_line(&projects, Some("nope"), std::time::Duration::from_secs(600));
        assert!(again.starts_with("'nope' isn't one of your projects. Your Datum login can see 2 projects."), "{again}");
        assert!(again.contains("(waiting up to 10 minutes)"), "{again}");
    }

    /// The add-on runs `setup --key-out <path> --hold-port 47780
    /// --options-source supervisor --session-file <path> --ingress-port N`.
    #[test]
    fn setup_subcommand_parses_with_the_page_flags() {
        let args = Args::try_parse_from([
            "datum-connect-daemon", "setup", "--key-out", "/data/k.json", "--hold-port", "47780",
            "--options-source", "supervisor", "--session-file", "/data/s.json", "--project", "p-1",
            "--ingress-port", "47781",
        ])
        .unwrap();
        assert_eq!(args.ingress_port, Some(47781));
        match args.command {
            Some(Command::Setup { project, key_out, hold_port: Some(47780), options_source: OptionsSource::Supervisor, session_file: Some(s) }) => {
                assert_eq!(project.as_deref(), Some("p-1"));
                assert_eq!(key_out, std::path::PathBuf::from("/data/k.json"));
                assert_eq!(s, std::path::PathBuf::from("/data/s.json"));
            }
            other => panic!("{other:?}"),
        }
        if std::env::var_os("DATUM_INGRESS_BIND").is_none() && std::env::var_os("DATUM_INGRESS_ALLOW").is_none() {
            assert_eq!((args.ingress_bind.as_str(), args.ingress_allow.as_str()), ("auto", "auto"));
        }
        // The daemon takes them too, before or without a subcommand.
        let args = Args::try_parse_from([
            "datum-connect-daemon", "--port", "1", "--ingress-port", "2", "--ingress-bind", "172.30.32.1",
            "--paired-key-file", "/data/service-account.json",
        ])
        .unwrap();
        assert!(args.command.is_none());
        assert_eq!(args.ingress_port, Some(2));
        assert_eq!(args.ingress_bind, "172.30.32.1");
        assert_eq!(args.paired_key_file, Some(std::path::PathBuf::from("/data/service-account.json")));
    }

    #[test]
    fn session_file_parses() {
        let args = Args::try_parse_from([
            "datum-connect-daemon", "pair", "--key-out", "k.json", "--session-file", "/data/pairing-session.json",
        ])
        .unwrap();
        match args.command {
            Some(Command::Pair { session_file: Some(p), .. }) => {
                assert_eq!(p, std::path::PathBuf::from("/data/pairing-session.json"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hold_port_parses() {
        let args = Args::try_parse_from([
            "datum-connect-daemon", "pair", "--key-out", "k.json", "--hold-port", "47780",
        ])
        .unwrap();
        assert!(matches!(args.command, Some(Command::Pair { hold_port: Some(47780), .. })));
    }

    #[test]
    fn expiry_reads_naturally() {
        assert_eq!(minutes(std::time::Duration::from_secs(300)), "5 minutes");
        assert_eq!(minutes(std::time::Duration::from_secs(60)), "1 minute");
        assert_eq!(minutes(std::time::Duration::from_secs(90)), "2 minutes");
        assert_eq!(minutes(std::time::Duration::from_secs(45)), "45 seconds");
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

#[cfg(test)]
mod start_request_tests {
    use super::*;

    /// Every body an existing client might send must still mean a plain
    /// start: none at all, whitespace, JSON null, and an empty object.
    #[test]
    fn absent_or_empty_body_is_a_plain_start() {
        for body in ["", "  \n", "null", "{}"] {
            assert_eq!(parse_start_request(body.as_bytes()).unwrap(), StartTunnelRequest::default(), "{body:?}");
        }
    }

    #[test]
    fn target_is_read_from_body() {
        let req = parse_start_request(br#"{"target": "http://127.0.0.1:80"}"#).unwrap();
        assert_eq!(req.target.as_deref(), Some("http://127.0.0.1:80"));
    }

    /// Ignoring a broken body would start the tunnel at its old target,
    /// the exact failure the field exists to prevent.
    #[test]
    fn malformed_body_is_an_error() {
        assert!(parse_start_request(b"{\"target\":").is_err());
        assert!(parse_start_request(br#"{"target": 80}"#).is_err());
    }

    #[test]
    fn real_target_defaults_to_http_like_create() {
        let t = parse_real_target("127.0.0.1:8123").unwrap();
        assert_eq!(t.normalized, "http://127.0.0.1:8123");
        assert_eq!(t.uri.authority().map(|a| a.as_str()), Some("127.0.0.1:8123"));
        assert_eq!(parse_real_target("https://example.test").unwrap().normalized, "https://example.test");
        assert!(parse_real_target("http://bad host").is_err());
    }

    #[test]
    fn repoint_only_when_inspector_missing_or_target_differs() {
        let want = "http://127.0.0.1:80";
        // Adopted after a reinstall: no inspector, nothing persisted.
        assert!(needs_repoint(false, None, want));
        // Persisted target survived but its inspector did not start.
        assert!(needs_repoint(false, Some(want), want));
        // Target changed in the add-on's configuration.
        assert!(needs_repoint(true, Some("http://127.0.0.1:8123"), want));
        assert!(needs_repoint(true, None, want));
        // Unchanged, so a running tunnel keeps today's early return.
        assert!(!needs_repoint(true, Some(want), want));
        assert!(!needs_repoint(true, Some("http://127.0.0.1:80/"), want));
    }

    /// The installed inspector must be the one handed in, and its target
    /// must be on disk where the restart reconciliation looks for it.
    #[tokio::test]
    async fn install_inspector_persists_target_and_replaces_previous() {
        let dir = std::env::temp_dir().join(format!("dcd-install-inspector-{}", std::process::id()));
        let inspectors = Mutex::new(HashMap::new());

        let first = parse_real_target("http://127.0.0.1:8123").unwrap();
        let (handle, _) = spawn_inspector(&first).await.unwrap();
        install_inspector(&dir, &inspectors, "tunnel-a", &first.normalized, handle).await;

        let second = parse_real_target("127.0.0.1:80").unwrap();
        let (handle, endpoint) = spawn_inspector(&second).await.unwrap();
        let addr = handle.local_addr;
        install_inspector(&dir, &inspectors, "tunnel-a", &second.normalized, handle).await;

        assert_eq!(endpoint, format!("http://{addr}"));
        let map = inspectors.lock().await;
        assert_eq!(map.len(), 1);
        assert_eq!(map["tunnel-a"].local_addr, addr);
        assert_eq!(map["tunnel-a"].real_target().to_string(), "http://127.0.0.1:80/");
        assert_eq!(load_inspector_target(&dir, "tunnel-a").await.as_deref(), Some("http://127.0.0.1:80"));
        drop(map);
        let _ = tokio::fs::remove_dir_all(&dir).await;
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
    let target = parse_real_target(&req.endpoint)
        .map_err(|e| err_response(format!("invalid endpoint '{}': {e}", req.endpoint)))?;

    let (handle, inspector_endpoint) = spawn_inspector(&target).await.map_err(err_response)?;

    // This makes a Connector for `control`'s throwaway key, which never goes
    // Ready. The tunnel's first start replaces it with one for the tunnel's
    // own key (see `resolve_listen_key`) and cleans it up. Creating under
    // the tunnel's key instead isn't cheap: the key is stored by tunnel id,
    // which the API server only assigns on create, and a connector is found
    // again by the key in its connectionDetails, which only a node that has
    // reached its relay fills in. Get either wrong and the first start
    // makes a second connector with nothing to clean it up.
    match state.control.create_active(&req.label, &inspector_endpoint).await {
        Ok(tunnel) => {
            install_inspector(&state.connect_dir, &state.inspectors, &tunnel.id, &target.normalized, handle).await;
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
    delete_tunnel_internal(&state, &id, "setup").await.map(Json)
}

/// Shared by `DELETE /v1/tunnels/:id` and the add-on page's Remove.
pub(crate) async fn delete_tunnel_internal(
    state: &Arc<AppState>,
    id: &str,
    actor_label: &str,
) -> Result<connect_lib::TunnelDeleteOutcome, (StatusCode, Json<serde_json::Value>)> {
    let id = id.to_string();
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
    if let Err(e) = remove_note(&state.connect_dir, &id).await {
        tracing::warn!(tunnel = %id, "failed to remove note file on delete: {e:#}");
    }
    auth::delete_tokens_for_tunnel(&state.connect_dir, &id).await;
    auth::append_audit(&state.connect_dir, &state.audit_lock, "delete", &id, actor_label).await;

    Ok(outcome)
}

#[derive(Deserialize)]
struct SetNoteRequest {
    note: String,
}

/// CLI-only (see `note` field's doc comment on `TunnelSummaryWithActor`) —
/// posting an empty string clears the note rather than needing a separate
/// DELETE route. 404s if the tunnel itself doesn't exist, same check
/// `revoke`/other id-scoped mutations already use elsewhere in this file.
async fn set_tunnel_note(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<SetNoteRequest>,
) -> ApiResult<serde_json::Value> {
    if state.control.get_active(&id).await.map_err(err_response)?.is_none() {
        return Err(not_found(&id));
    }
    if req.note.len() > NOTE_MAX_BYTES {
        return Err(err_response(format!("note too long ({} bytes, max {NOTE_MAX_BYTES})", req.note.len())));
    }
    save_note(&state.connect_dir, &id, &req.note).await.map_err(err_response)?;
    Ok(Json(json!({ "id": id, "note": req.note })))
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

/// Optional body of `POST /v1/tunnels/:id/start`. No body, an empty one,
/// `null` and `{}` all mean a plain start, exactly as before the body
/// existed, so existing clients (datumctl, the dashboard) are unaffected.
#[derive(Deserialize, Default, Debug, PartialEq)]
struct StartTunnelRequest {
    /// The real local target this tunnel should forward to. Lets a client
    /// adopt a tunnel whose local state is gone — the Home Assistant add-on
    /// finds its tunnel again by label after a reinstall wiped `/data`, and
    /// with it the persisted target, so nothing would otherwise create the
    /// inspector the tunnel's HTTPProxy needs. Also how such a client
    /// changes an existing tunnel's target.
    #[serde(default)]
    target: Option<String>,
}

/// Parsed by hand rather than with axum's `Option<Json<_>>`, which turns a
/// malformed body into `None` and would silently start the tunnel at its
/// old target.
fn parse_start_request(body: &[u8]) -> Result<StartTunnelRequest, String> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(StartTunnelRequest::default());
    }
    serde_json::from_slice::<Option<StartTunnelRequest>>(body)
        .map(Option::unwrap_or_default)
        .map_err(|e| format!("invalid request body: {e}"))
}

async fn start_tunnel(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Extension(actor): Extension<Actor>,
    body: axum::body::Bytes,
) -> ApiResult<connect_lib::TunnelSummary> {
    let result = start_tunnel_with_body(&state, &id, &actor, &body).await;
    // Logged here as well as returned: a client that drops the response
    // body (the add-on's `curl -f` did) otherwise leaves no trace of why.
    if let Err((status, body)) = &result {
        tracing::warn!(tunnel = %id, %status, error = %body.0, "start failed");
    }
    result.map(Json)
}

async fn start_tunnel_with_body(
    state: &Arc<AppState>,
    id: &str,
    actor: &Actor,
    body: &[u8],
) -> Result<connect_lib::TunnelSummary, (StatusCode, Json<serde_json::Value>)> {
    let bad_request = |msg: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": msg })));
    let req = parse_start_request(body).map_err(bad_request)?;
    let target = match req.target.as_deref() {
        None => None,
        // Choosing what a tunnel exposes is a create-level decision, which
        // is setup-only: an operate token may turn its one tunnel on and
        // off, not aim it at another local port.
        Some(_) if !matches!(actor, Actor::Setup) => {
            return Err((
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "setting a tunnel's target needs the setup token" })),
            ));
        }
        Some(raw) => Some(
            parse_real_target(raw).map_err(|e| bad_request(format!("invalid target '{raw}': {e}")))?,
        ),
    };
    start_tunnel_internal(state, id, &actor.audit_label(), target.as_ref()).await
}

/// Shared by the `/start` handler and the startup reconciliation pass
/// (which auto-resumes any tunnel that was enabled before the daemon
/// last stopped — see the "recover a previously-enabled tunnel after a
/// restart/reboot" note in `run()`), so both go through the identical
/// mint-key/heartbeat/enable sequence rather than two implementations
/// drifting apart.
///
/// With a `target`, the tunnel is first given an inspector for it, unless
/// it already has one forwarding there — see `needs_repoint`. That works
/// whether or not the tunnel is running; the reconciliation pass passes
/// `None`, since the inspector it has just started is already right.
async fn start_tunnel_internal(
    state: &Arc<AppState>,
    id: &str,
    actor_label: &str,
    target: Option<&RealTarget>,
) -> Result<connect_lib::TunnelSummary, (StatusCode, Json<serde_json::Value>)> {
    let repoint = match target {
        Some(t) => {
            let has_inspector = state.inspectors.lock().await.contains_key(id);
            let persisted = load_inspector_target(&state.connect_dir, id).await;
            needs_repoint(has_inspector, persisted.as_deref(), &t.normalized).then_some(t)
        }
        None => None,
    };

    if repoint.is_none() && state.running.lock().await.contains_key(id) {
        return state
            .control
            .get_active(id)
            .await
            .map_err(err_response)?
            .ok_or_else(|| not_found(id));
    }

    let _busy_guard = claim_busy(state, id)?;

    let tunnel = match state.control.get_active(id).await {
        Ok(Some(t)) => t,
        Ok(None) => return Err(not_found(id)),
        Err(e) => return Err(err_response(e)),
    };

    // Already on, but its target is changing: on the add-on, the startup
    // reconciliation pass resumes the tunnel at its old target before the
    // add-on gets to ask for the new one. Only the endpoint changes, so
    // the HTTPProxy keeps the connector it already references.
    let running_service = state.running.lock().await.get(id).map(|r| r.service.clone());
    if let Some(service) = running_service {
        // A concurrent start may have finished between the check above and
        // the claim; with nothing to repoint, that's the early return.
        let Some(t) = repoint else {
            return Ok(tunnel);
        };
        let (handle, endpoint) = spawn_inspector(t).await.map_err(err_response)?;
        let updated = service.retarget_active(id, &endpoint).await.map_err(err_response)?;
        install_inspector(&state.connect_dir, &state.inspectors, id, &t.normalized, handle).await;
        tracing::info!(tunnel = %id, target = %t.normalized, "repointed running tunnel at a new inspector");
        auth::append_audit(&state.connect_dir, &state.audit_lock, "retarget", id, actor_label).await;
        return Ok(updated);
    }

    let inspector = match repoint {
        Some(t) => Some((t, spawn_inspector(t).await.map_err(err_response)?)),
        None => None,
    };
    let endpoint = match &inspector {
        Some((_, (_, inspector_endpoint))) => inspector_endpoint.clone(),
        None => tunnel.endpoint.clone(),
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
    let service = TunnelService::new(state.datum.clone(), node.clone())
        .with_waf_exempt_matches(state.waf_exempt_matches.clone())
        .with_edge_policies(state.edge_policies);

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

    // A new key means a new connector, which the HTTPProxy must move to.
    // A new inspector alone means only a new endpoint, so the connector
    // stays as it is; enabling below confirms it.
    if should_rewire {
        service
            .update_active(id, &tunnel.label, &endpoint)
            .await
            .map_err(err_response)?;
        if let Err(e) = service.cleanup_orphaned_connectors().await {
            tracing::warn!("cleanup_orphaned_connectors after rewire failed: {e:#}");
        }
    } else if inspector.is_some() {
        service.retarget_active(id, &endpoint).await.map_err(err_response)?;
    }
    if let Some((t, (handle, _))) = inspector {
        install_inspector(&state.connect_dir, &state.inspectors, id, &t.normalized, handle).await;
        tracing::info!(tunnel = %id, target = %t.normalized, "pointed tunnel at a new inspector");
        auth::append_audit(&state.connect_dir, &state.audit_lock, "retarget", id, actor_label).await;
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

/// `datum-connect-daemon pair`. Its stdout is what a person reads (the Home
/// Assistant add-on's log), so it says what happens in plain words; tracing
/// only carries retries, on stderr.
async fn pair(
    project: Option<String>,
    key_out: std::path::PathBuf,
    hold_port: Option<u16>,
    options_source: OptionsSource,
    session_file: Option<std::path::PathBuf>,
) -> n0_error::Result<()> {
    use connect_lib::datum_cloud::ha_supervisor::{PairingNotifier, Supervisor};
    use connect_lib::datum_cloud::pairing::{self, PairingConfig, PairingEvent, ProjectWait};

    let mut cfg = PairingConfig::from_env(project, key_out);
    cfg.session_file = session_file;
    // Inside a Home Assistant add-on: the link as a clickable notification,
    // and a project chosen on the Configuration tab without a restart. The
    // log lines below are printed either way, as the fallback.
    let supervisor = Supervisor::from_env();
    cfg.project_wait = options_source.resolve(supervisor.as_ref()).map(ProjectWait::new);
    let notifier = supervisor.map(PairingNotifier::spawn);
    let mut say = |event: PairingEvent| {
        if let Some(n) = &notifier {
            n.event(&event);
        }
        say_line(&pairing_line(&event));
    };
    // A port that cannot be held only matters if a watchdog is watching,
    // so it is worth a warning, never a failed pairing.
    let holder = match hold_port {
        Some(port) => match PortHolder::bind(port).await {
            Ok(h) => {
                tracing::debug!(addr = %h.addr, "pair: holding the daemon's port while pairing");
                Some(h)
            }
            Err(e) => {
                tracing::warn!("pair: cannot hold port {port} for the watchdog, pairing anyway: {e}");
                None
            }
        },
        None => None,
    };
    // The add-on's stop is a SIGTERM. Stop pairing on it, like on Ctrl-C,
    // so the port is released on that path too.
    let outcome = tokio::select! {
        r = pairing::pair(&cfg, &mut say) => Some(r),
        _ = shutdown_signal() => None,
    };
    // Stopped while waiting for a project, with the login saved: the next
    // start carries on from here, so this is a pause, not a failure.
    let paused = outcome.is_none() && cfg.session_file.as_deref().is_some_and(std::path::Path::exists);
    if let Some(n) = notifier {
        match &outcome {
            Some(r) => n.outcome(r),
            None if paused => n.paused(),
            // Stopped with the add-on: its code is no use any more.
            None => n.dismiss(),
        }
        n.finish().await;
    }
    if let Some(holder) = holder {
        holder.release().await;
    }
    let paired = match outcome {
        Some(r) => r.map_err(|e| n0_error::anyerr!("Pairing with Datum failed: {e}"))?,
        None if paused => {
            say_line("Pairing paused; it continues after the restart without a new login.");
            std::process::exit(PAIR_PAUSED_EXIT);
        }
        None => return Err(n0_error::anyerr!("Pairing with Datum stopped before it finished.")),
    };
    println!(
        "Paired: this device now uses service account {} in project {}. To revoke it, delete that service account in the Datum portal under the project's Service accounts.",
        paired.service_account_email, paired.project
    );
    Ok(())
}

/// What `pair` and `setup` print for each event. Their stdout is what a
/// person reads (the add-on's log), so it says what happens in plain words.
fn pairing_line(event: &connect_lib::datum_cloud::pairing::PairingEvent) -> String {
    use connect_lib::datum_cloud::pairing::PairingEvent;
    match event {
        PairingEvent::Code { url, user_code, expires_in } => format!(
            "To connect this Home Assistant to Datum, open {url} and enter code {user_code} (expires in {})",
            minutes(*expires_in)
        ),
        PairingEvent::CodeExpired => "That code expired before it was approved. Here is a new one.".into(),
        PairingEvent::Approved { email } => format!("Approved as {email}"),
        PairingEvent::Resumed { email } => format!("Continuing pairing as {email} (no new login needed)"),
        PairingEvent::SessionDropped { reason } => {
            format!("Could not continue the earlier pairing: {reason}. Starting a new login.")
        }
        PairingEvent::ChooseProject { projects, rejected, wait } => {
            choose_project_line(projects, rejected.as_deref(), *wait)
        }
        PairingEvent::ProjectSelected { project, organization } => {
            format!("Using project {project} (organization {organization})")
        }
        PairingEvent::ServiceAccountCreated { email, project } => {
            format!("Created service account {email} in project {project}")
        }
        PairingEvent::ServiceAccountReused { email, project } => {
            format!("Using service account {email} in project {project}, created by the previous attempt")
        }
        PairingEvent::AccessGranted => "Granted access".into(),
        PairingEvent::AccessNotConfirmed { email, project } => format!(
            "Could not grant access again; carrying on in case an owner has granted it. If tunnel calls are refused, ask an organization owner or editor to grant role 'editor' to service account {email} on project {project}."
        ),
        PairingEvent::KeySaved { .. } => "Saved key".into(),
    }
}

fn say_line(line: &str) {
    use std::io::Write;
    println!("{line}");
    let _ = std::io::stdout().flush();
}

/// `setup`'s log and notification. The notification points at the page,
/// with the link and code as the fallback; the log keeps every line `pair`
/// prints, so a person with only the log can still pair.
struct SetupLog {
    notifier: std::sync::Mutex<Option<connect_lib::datum_cloud::ha_supervisor::PairingNotifier>>,
    panel: Option<String>,
}

impl SetupLog {
    fn notify(&self, f: impl FnOnce(&connect_lib::datum_cloud::ha_supervisor::PairingNotifier)) {
        if let Some(n) = self.notifier.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            f(n);
        }
    }
}

impl connect_lib::datum_cloud::pairing_setup::SetupObserver for SetupLog {
    fn event(&self, event: &connect_lib::datum_cloud::pairing::PairingEvent) {
        use connect_lib::datum_cloud::ha_supervisor::{setup_choose_message, setup_code_message};
        use connect_lib::datum_cloud::pairing::PairingEvent;
        let panel = self.panel.as_deref();
        match event {
            PairingEvent::ChooseProject { projects, .. } => {
                let mut line = format!(
                    "Signed in. Choose the project on the Datum Connect page, or set 'project' on the add-on's Configuration tab to one of these {} ids and click Save:",
                    projects.len()
                );
                for p in projects {
                    line.push_str(&format!("\n  {} ({}, organization {})", p.id, p.display_name, p.organization));
                }
                say_line(&line);
                self.notify(|n| n.show(setup_choose_message(panel, projects.len())));
            }
            PairingEvent::Code { url, user_code, expires_in } => {
                say_line(&pairing_line(event));
                self.notify(|n| n.show(setup_code_message(panel, url, user_code, *expires_in)));
            }
            other => say_line(&pairing_line(other)),
        }
    }

    fn restarted(&self) {
        say_line("New code requested from the Datum Connect page.");
    }

    fn outcome(&self, result: &Result<connect_lib::datum_cloud::pairing::PairedKey, connect_lib::datum_cloud::pairing::PairingError>) {
        use connect_lib::datum_cloud::ha_supervisor::setup_failure_message;
        use connect_lib::datum_cloud::pairing_setup::error_view;
        match result {
            Ok(_) => self.notify(|n| n.dismiss()),
            Err(e) => {
                let message = error_view(e).message;
                say_line(&format!("Pairing with Datum did not finish: {message} Open the Datum Connect page to try again."));
                self.notify(|n| n.show(setup_failure_message(self.panel.as_deref(), &message)));
            }
        }
    }
}

/// `datum-connect-daemon setup`. See `Command::Setup`.
#[allow(clippy::too_many_arguments)]
async fn setup(
    project: Option<String>,
    key_out: std::path::PathBuf,
    hold_port: Option<u16>,
    options_source: OptionsSource,
    session_file: Option<std::path::PathBuf>,
    ingress_port: Option<u16>,
    ingress_bind: &str,
    ingress_allow: &str,
) -> n0_error::Result<()> {
    use connect_lib::datum_cloud::ha_supervisor::{PairingNotifier, Supervisor, panel_path, setup_waiting_message};
    use connect_lib::datum_cloud::pairing::PairingConfig;
    use connect_lib::datum_cloud::pairing_setup::SetupController;

    let supervisor = Supervisor::from_env();
    // Without a page, pairing from the log and notification is still
    // pairing: fall back to `pair` rather than fail.
    let Some(port) = ingress_port else {
        tracing::warn!("setup: no --ingress-port, so no page; pairing from the log instead");
        return pair(project, key_out, hold_port, options_source, session_file).await;
    };
    let (bind, allowed) = match ingress::resolve_addresses(ingress_bind, ingress_allow, supervisor.as_ref()).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("setup: {e}; pairing from the log instead");
            return pair(project, key_out, hold_port, options_source, session_file).await;
        }
    };
    let listener = match ingress::bind(bind, port).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(
                "setup: cannot serve the Datum Connect page on {bind}:{port} ({e}); pairing from the log and notification instead"
            );
            return pair(project, key_out, hold_port, options_source, session_file).await;
        }
    };

    let mut cfg = PairingConfig::from_env(project, key_out);
    cfg.session_file = session_file;
    cfg.max_wait = SETUP_MAX_WAIT;
    let fallback = options_source.resolve(supervisor.as_ref());
    let panel = match &supervisor {
        Some(s) => match s.self_info().await {
            Ok(info) => panel_path(&info.slug),
            Err(e) => {
                tracing::warn!("setup: cannot ask the Supervisor for this add-on's slug, so the notification cannot link to the page: {e}");
                None
            }
        },
        None => None,
    };
    let log = Arc::new(SetupLog {
        notifier: std::sync::Mutex::new(supervisor.clone().map(PairingNotifier::spawn)),
        panel: panel.clone(),
    });
    let ctl = SetupController::new(cfg, fallback, Some(log.clone()));
    let server = ingress::serve_on(listener, ingress::router(ingress::Backend::Setup(ctl.clone()), allowed.clone()));
    tracing::info!(%bind, port, ?allowed, "serving the Datum Connect page");

    let holder = match hold_port {
        Some(p) => match PortHolder::bind(p).await {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::warn!("setup: cannot hold port {p} for the watchdog, carrying on: {e}");
                None
            }
        },
        None => None,
    };

    say_line(
        "This Home Assistant isn't connected to Datum yet. Open Datum Connect (in the sidebar, or Settings > Apps > Datum Connect > Open Web UI) and click Connect to Datum.",
    );
    log.notify(|n| n.show(setup_waiting_message(panel.as_deref())));
    // A login kept by a run stopped while choosing a project: carry on
    // with it, so the page opens on the project list.
    if ctl.has_saved_session() {
        ctl.start();
    }

    let outcome = tokio::select! {
        key = ctl.paired() => Some(key),
        _ = shutdown_signal() => None,
    };
    let paused = match &outcome {
        Some(key) => {
            say_line(&format!(
                "Paired: this device now uses service account {} in project {}. To revoke it, delete that service account in the Datum portal under the project's Service accounts.",
                key.service_account_email, key.project
            ));
            // The run's own outcome has already asked for this; asked again
            // here, last in the queue, so nothing shown after it brings the
            // notification back. The daemon asks once more as it starts.
            log.notify(|n| n.dismiss());
            tokio::time::sleep(SETUP_HANDOVER).await;
            false
        }
        None => {
            ctl.stop().await;
            let paused = ctl.has_saved_session();
            log.notify(|n| if paused { n.paused() } else { n.dismiss() });
            paused
        }
    };
    server.abort();
    let _ = server.await;
    let notifier = log.notifier.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(n) = notifier {
        n.finish().await;
    }
    if let Some(holder) = holder {
        holder.release().await;
    }
    if outcome.is_none() {
        say_line(if paused {
            "Pairing paused; it continues after the restart without a new login."
        } else {
            "Stopped before connecting to Datum; the page offers it again on the next start."
        });
        // Not a failure either way: the add-on was stopped.
        std::process::exit(PAIR_PAUSED_EXIT);
    }
    Ok(())
}

/// Keeps a TCP port answering while `pair` runs, in place of the daemon
/// that will listen there afterwards. Connections are accepted and dropped:
/// the Home Assistant Supervisor's `tcp://` watchdog only checks that a
/// connect succeeds. Same address as the daemon's own listener.
struct PortHolder {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl PortHolder {
    async fn bind(port: u16) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
        let addr = listener.local_addr()?;
        let task = tokio::spawn(async move {
            loop {
                if let Ok((socket, _)) = listener.accept().await {
                    drop(socket);
                }
            }
        });
        Ok(Self { addr, task })
    }

    /// Returns once the listener is closed, so the port is free to bind.
    async fn release(self) {
        self.task.abort();
        // Awaiting the aborted task is what guarantees it, and the listener
        // it owns, has been dropped.
        let _ = self.task.await;
    }
}

/// Ctrl-C, or SIGTERM where there is one.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// "5 minutes", "1 minute", "45 seconds".
fn minutes(d: std::time::Duration) -> String {
    connect_lib::datum_cloud::ha_supervisor::duration_words(d)
}

/// The log's version of the "choose a project" notification.
fn choose_project_line(
    projects: &[connect_lib::datum_cloud::pairing::ProjectChoice],
    rejected: Option<&str>,
    wait: std::time::Duration,
) -> String {
    let mut line = String::new();
    if let Some(r) = rejected {
        line.push_str(&format!("'{r}' isn't one of your projects. "));
    }
    line.push_str(&format!(
        "Your Datum login can see {} projects. Set 'project' on the add-on's Configuration tab to one of these ids and click Save. Home Assistant offers to restart the add-on when you save; either way, pairing continues without a new login (waiting up to {}):",
        projects.len(),
        minutes(wait)
    ));
    for p in projects {
        line.push_str(&format!("\n  {} ({}, organization {})", p.id, p.display_name, p.organization));
    }
    line
}

/// `pair` and `setup` log to stderr only, and quietly: their stdout is
/// what a person reads.
fn init_pairing_tracing() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("datum_connect_daemon=warn,connect_lib=warn")),
        )
        .init();
}

/// The start-up hint about Home Assistant's trusted proxies, if one is due.
fn proxy_hint(setup: Option<&connect_lib::datum_cloud::ha_core::ProxySetup>) -> Option<&'static str> {
    use connect_lib::datum_cloud::ha_core::ProxySetup;
    match setup? {
        ProxySetup::Ready => None,
        ProxySetup::Needed { .. } => Some(
            "Home Assistant does not accept connections through Datum yet, so its public address answers 400: Bad Request. Open Datum Connect in the sidebar and click Allow, or see the Documentation tab.",
        ),
        ProxySetup::Pending => Some(
            "Home Assistant has a network settings change waiting for confirmation. Finish it in Settings > System > Network, then check the Datum Connect page.",
        ),
    }
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

    match args.command {
        Some(Command::Pair { project, key_out, hold_port, options_source, session_file }) => {
            init_pairing_tracing();
            return pair(project, key_out, hold_port, options_source, session_file).await;
        }
        Some(Command::Setup { project, key_out, hold_port, options_source, session_file }) => {
            init_pairing_tracing();
            return setup(
                project,
                key_out,
                hold_port,
                options_source,
                session_file,
                args.ingress_port,
                &args.ingress_bind,
                &args.ingress_allow,
            )
            .await;
        }
        None => {}
    }

    let session = std::env::var("DATUM_SESSION").ok();
    if session.is_none() && std::env::var("DATUM_PLUGIN_MODE").map(|v| v != "1").unwrap_or(true) {
        return Err(n0_error::anyerr!(
            "neither DATUM_SESSION nor DATUM_PLUGIN_MODE=1 set — this daemon runs in plugin mode only"
        ));
    }

    // A service account key (DATUM_SA_KEY_FILE, the Home Assistant add-on)
    // mints tokens in-process; otherwise the credentials helper, as before.
    let token_source = ExternalTokenSource::from_env_with_refresh(session.clone())
        .await
        .map_err(|e| n0_error::anyerr!("failed to create token source: {e}"))?;
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
    let waf_exempt_matches = match args.waf_exempt_matches.as_deref() {
        Some(raw) if !raw.trim().is_empty() => connect_lib::parse_waf_exempt_matches(raw)
            .map_err(|e| n0_error::anyerr!("invalid DATUM_TUNNEL_WAF_EXEMPT_MATCHES: {e}"))?,
        _ => Vec::new(),
    };
    if !waf_exempt_matches.is_empty() {
        tracing::info!(
            count = waf_exempt_matches.len(),
            "WAF exemptions configured: tunnels get a `streams` rule ahead of `protected`"
        );
    }
    if args.edge_policies {
        tracing::info!(
            "Edge policies on: each tunnel's WAF and 1h request timeout are ensured on create and on every start"
        );
    }
    let control = TunnelService::new(datum.clone(), control_node)
        .with_waf_exempt_matches(waf_exempt_matches.clone())
        .with_edge_policies(args.edge_policies);

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
        waf_exempt_matches,
        edge_policies: args.edge_policies,
        started_at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        exclusive_label: args.exclusive_label.clone().filter(|l| !l.trim().is_empty()),
    });
    if let Some(label) = &state.exclusive_label {
        tracing::info!(%label, "the add-on owns one tunnel: older tunnels from this device are stopped and not resumed");
    }

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

    // The add-on's page, up before reconciling, so that it answers while
    // the tunnels are brought back. Never fatal: the tunnel matters more.
    if let Some(port) = args.ingress_port {
        let supervisor = connect_lib::datum_cloud::ha_supervisor::Supervisor::from_env();
        match ingress::resolve_addresses(&args.ingress_bind, &args.ingress_allow, supervisor.as_ref()).await {
            Ok((bind, allowed)) => match ingress::bind(bind, port).await {
                Ok(listener) => {
                    let view = addon_page::DaemonPaired::new(
                        state.clone(),
                        std::env::var_os("DATUM_SA_KEY_FILE").map(std::path::PathBuf::from),
                        args.paired_key_file.clone(),
                        supervisor,
                    );
                    let router = ingress::router(ingress::Backend::Paired(Arc::new(view)), allowed.clone());
                    // Detached: it serves for as long as the daemon runs.
                    drop(ingress::serve_on(listener, router));
                    tracing::info!(%bind, port, ?allowed, "serving the Datum Connect page");
                }
                Err(e) => tracing::warn!("cannot serve the Datum Connect page on {bind}:{port}: {e}"),
            },
            Err(e) => tracing::warn!("not serving the Datum Connect page: {e}"),
        }
    }

    // Inside the add-on: pairing is over (there is a key), so its
    // notification goes, and a hint if Home Assistant does not accept
    // requests through Datum yet. In the background: neither may hold the
    // tunnel up.
    if let Some(sup) = connect_lib::datum_cloud::ha_supervisor::Supervisor::from_env()
        && std::env::var_os("DATUM_SA_KEY_FILE")
            .map(std::path::PathBuf::from)
            .is_some_and(|k| k.is_file())
    {
        tokio::spawn(async move {
            let check =
                connect_lib::datum_cloud::ha_core::startup_check(&sup, std::time::Duration::from_secs(20)).await;
            if let Err(e) = check.dismissed {
                tracing::debug!("could not dismiss a leftover pairing notification (best-effort): {e}");
            }
            if let Some(line) = proxy_hint(check.proxies.as_ref()) {
                tracing::info!("{line}");
            }
        });
    }

    // Reconcile inspectors for any tunnel that was already enabled before
    // this daemon (re)started. Its old inspector process died with the
    // previous process, but the real target was persisted to disk in
    // `create_tunnel` — without this, every pre-existing tunnel's traffic
    // history and real-target mapping would be permanently unrecoverable
    // after a routine restart.
    match state.control.list_active().await {
        Ok(tunnels) => {
            // The add-on owns one tunnel; older ones from this device stay
            // off. See `exclusive.rs`.
            let older: HashSet<String> = match &state.exclusive_label {
                Some(label) => {
                    let part = exclusive::partition(tunnels.clone(), label, |id| {
                        exclusive::is_locally_known(&state.connect_dir, &state.project_id, id)
                    });
                    part.older.into_iter().map(|t| t.id).collect()
                }
                None => HashSet::new(),
            };
            let (older_tunnels, tunnels): (Vec<_>, Vec<_>) =
                tunnels.into_iter().partition(|t| older.contains(&t.id));
            for t in older_tunnels {
                if !t.enabled {
                    tracing::info!("{}", exclusive::already_off_line(&t));
                    continue;
                }
                match stop_tunnel_internal(&state, &t.id, StopReason::Manual, "system").await {
                    Ok(_) => tracing::info!("{}", exclusive::stopped_line(&t)),
                    Err((status, body)) => tracing::warn!(
                        tunnel = %t.id,
                        %status,
                        error = %body.0,
                        "Could not stop older tunnel '{}'; it is not resumed, but may still answer until it is stopped or removed",
                        t.label
                    ),
                }
            }
            // Every existing tunnel gets its inspector reconciled here,
            // regardless of enabled/disabled state — the inspector's
            // lifecycle is tied to the tunnel profile existing at all, not
            // to whether it's currently turned on (mirrors create_tunnel,
            // which starts one unconditionally). A disabled tunnel skipped
            // here would keep a persisted endpoint pointing at a dead
            // inspector forever, since a plain start (no `target` in its
            // body) doesn't create or repoint one itself — it relies on
            // one already existing in state.inspectors by the time it runs.
            for t in tunnels {
                let Some(target) = load_inspector_target(&state.connect_dir, &t.id).await else {
                    tracing::warn!(tunnel = %t.id, "no persisted real target found, cannot reconcile inspector (a start that gives a target will create one)");
                    continue;
                };
                let real_target = match parse_real_target(&target) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(tunnel = %t.id, %target, "persisted target is not a valid URI, skipping: {e}");
                        continue;
                    }
                };
                let was_enabled = t.enabled;
                let tunnel_id = t.id.clone();
                match spawn_inspector(&real_target).await {
                    Ok((handle, inspector_endpoint)) => {
                        // Endpoint only: `update_active` here made a new
                        // Connector for `control`'s fresh key on every
                        // restart, pointed the tunnel at it until the
                        // auto-resume below pointed it back, and left it
                        // behind, never Ready.
                        if let Err(e) = state.control.retarget_active(&t.id, &inspector_endpoint).await {
                            tracing::warn!(tunnel = %t.id, "failed to repoint tunnel at reconciled inspector: {e:#}");
                            continue;
                        }
                        tracing::info!(tunnel = %t.id, %target, "reconciled inspector after restart");
                        install_inspector(&state.connect_dir, &state.inspectors, &t.id, &real_target.normalized, handle)
                            .await;

                        // Recover a tunnel that was live (enabled) before
                        // the daemon last stopped — whether from a crash,
                        // a manual restart, or a full machine reboot.
                        // Without this, reconciliation only brings the
                        // inspector back; the tunnel would report
                        // "enabled" in its profile but silently serve no
                        // traffic until someone noticed and called
                        // /start by hand.
                        if was_enabled {
                            match start_tunnel_internal(&state, &tunnel_id, "system", None).await {
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
        .route("/v1/tunnels/:id/note", post(set_tunnel_note))
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
