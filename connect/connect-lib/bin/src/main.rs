//! Plugin-mode tunnel agent (`datum-connect`). The Go-side `datumctl connect`
//! plugin spawns this binary as a subprocess and communicates over stdout
//! (line-delimited JSON when `--json`, human text otherwise).
//!
//! # JSON EVENT CONTRACT (emitted by this binary's main handler)
//!
//! See `progress.rs` for setup-phase events (`tunnel_progress`,
//! `tunnel_verifying`, `tunnel_verified`).
//!
//! | Event type                | When                                       | Fields                                                                            |
//! |---------------------------|--------------------------------------------|-----------------------------------------------------------------------------------|
//! | `tunnel_created`          | new HTTPProxy created                      | `id`                                                                              |
//! | `tunnel_updated`          | label/endpoint changed                     | `id`, `label`, `endpoint`, `hostnames`                                            |
//! | `tunnel_ready`            | setup complete AND proxy reachable (non-5xx) | `id`, `label`, `endpoint`, `hostnames`, `endpoint_id`, `status`, `elapsed_secs` |
//! | `tunnel_login_lost`       | LoginState::Missing observed mid-run       | `id`, `message`                                                                   |
//! | `tunnel_terminal_failure` | progress.terminal_failure() Some mid-run   | `id`, `message`                                                                   |
//! | `tunnel_deleted_upstream` | get_active_progress -> None mid-run        | `id`, `message`                                                                   |
//! | `tunnel_disabled`         | cleanup before exit                        | `id`                                                                              |
//! | `tunnel_deleted`          | `delete` subcommand only                   | `id`, `deleted: true`                                                             |
//!
//! `tunnel_ready` is the single event that drives the Go supervisor's
//! `gotReady` handshake (`connect/tunnel/listen/main.go:160-176`, established
//! in commit `1bb9552`). It MUST NOT be removed, renamed, or have its emission
//! site moved without coordinating the Go side.

use std::io::Write;
use std::sync::OnceLock;

use clap::{Parser, Subcommand};
use n0_error::StdResultExt;
use tracing_subscriber::{
    filter::EnvFilter,
    layer::SubscriberExt,
    reload::{self, Handle},
    util::SubscriberInitExt,
    Registry,
};

use connect_lib::datum_cloud::env::ApiEnv;
use connect_lib::datum_cloud::external_token_source::ExternalTokenSource;
use connect_lib::datum_cloud::DatumCloudClient;
use connect_lib::{HeartbeatAgent, ListenNode, Repo, SelectedContext, TunnelService};
use iroh::SecretKey;

mod progress;

type ReloadHandle = Handle<EnvFilter, Registry>;
static RELOAD_HANDLE: OnceLock<ReloadHandle> = OnceLock::new();
/// The filter string that `init_tracing()` actually installed.
/// Used by `current_filter_string()` to restore the original filter later.
static INSTALLED_FILTER: OnceLock<String> = OnceLock::new();

fn init_tracing() {
    let debug = debug_enabled();
    let default_directive = if debug {
        "datum_connect=debug,connect_lib=debug"
    } else {
        "datum_connect=info"
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_directive));
    let filter_string = filter.to_string();
    let (filter_layer, handle) = reload::Layer::new(filter);
    // Best-effort: if a subscriber is already installed (e.g. duplicate call in tests),
    // skip without panicking.
    let _ = tracing_subscriber::registry()
        .with(filter_layer)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .try_init();
    let _ = RELOAD_HANDLE.set(handle);
    let _ = INSTALLED_FILTER.set(filter_string);
    if debug {
        eprintln!("[datum-connect] debug logging enabled (token refresh, heartbeat, etc.)");
    }
}

/// Whether verbose debug logging is enabled. Toggled by the
/// `DATUM_CONNECT_DEBUG=1` env var. When enabled, the tracing filter is
/// bumped to `debug` for the `datum_connect` and `connect_lib` targets so that
/// token-refresh events (proactive + forced) and heartbeat 401 handling print
/// to the console (stderr).
fn debug_enabled() -> bool {
    std::env::var("DATUM_CONNECT_DEBUG")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

static DEBUG_FLAG: OnceLock<bool> = OnceLock::new();

fn silence_tracing() {
    if let Some(handle) = RELOAD_HANDLE.get() {
        let _ = handle.modify(|f| *f = EnvFilter::new("off"));
    }
}

fn restore_tracing(prev: &str) {
    if let Some(handle) = RELOAD_HANDLE.get() {
        let _ = handle.modify(|f| *f = EnvFilter::new(prev));
    }
}

fn current_filter_string() -> String {
    INSTALLED_FILTER
        .get()
        .cloned()
        .unwrap_or_else(|| "datum_connect=info".to_string())
}

/// Resolve the listen key for a tunnel, generating a new one if missing.
/// Returns `(key, force_rewire)` where `force_rewire` is true when a new
/// key was generated.
async fn resolve_listen_key(
    repo: &Repo,
    project_id: &str,
    tunnel_id: &str,
) -> n0_error::Result<(SecretKey, bool)> {
    match repo.listen_key_for_tunnel(project_id, tunnel_id).await {
        Ok(k) => Ok((k, false)),
        Err(e) if e.to_string().contains("KEY_NOT_FOUND") => {
            let _ = writeln!(
                std::io::stderr(),
                "  \u{26A0} No listen key for tunnel {tunnel_id} — generating new key. \
                 The hostname will stay the same but the old connector will be replaced."
            );
            let _ = std::io::stderr().flush();
            let new_key = SecretKey::generate(&mut rand::rng());
            Ok((new_key, true))
        }
        Err(e) => Err(e),
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "datum-connect",
    about = "Datum Connect tunnel agent (plugin mode)",
    version = concat!("0.1.0+", env!("GIT_COMMIT")),
)]
struct Args {
    #[clap(long, env = "DATUM_CONNECT_DIR")]
    repo: Option<std::path::PathBuf>,
    #[clap(long, env = "DATUM_PROJECT")]
    project: Option<String>,
    #[clap(long, global = true)]
    json: bool,
    /// Enable verbose debug logging on stderr (token refresh events, heartbeat
    /// 401 handling, etc.). Also enabled by `DATUM_CONNECT_DEBUG=1`.
    #[clap(long, global = true)]
    debug: bool,
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// List all tunnels in the current project.
    List,
    /// Start a tunnel exposing a local service.
    Listen {
        #[clap(long)]
        label: Option<String>,
        #[clap(long)]
        endpoint: Option<String>,
        #[clap(long)]
        id: Option<String>,
    },
    /// Update an existing tunnel.
    Update {
        #[clap(long)]
        id: String,
        #[clap(long)]
        label: Option<String>,
        #[clap(long)]
        endpoint: Option<String>,
    },
    /// Delete a tunnel.
    Delete {
        #[clap(long)]
        id: String,
    },
}

/// Why the Listen handler's runtime select-loop terminated. Drives the
/// final exit status: CtrlC = clean exit 0; TerminalFailure / DeletedUpstream
/// = exit 1 with an n0_error::anyerr! message.
enum ExitReason {
    CtrlC,
    TerminalFailure,
    DeletedUpstream,
}

fn resolve_project(project_id: &str) -> SelectedContext {
    SelectedContext {
        project_id: project_id.to_string(),
        project_name: project_id.to_string(),
        org_id: String::new(),
        org_name: String::new(),
        org_type: String::new(),
    }
}

#[tokio::main]
async fn main() {
    let result = run().await;
    if let Err(err) = result {
        eprintln!("{:#}", err);
        std::process::exit(1);
    }
}

async fn run() -> n0_error::Result<()> {
    let _ = rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| n0_error::anyerr!("failed to install ring crypto provider for rustls"))?;

    init_tracing();

    // Let clap handle --version/--help before the plugin-mode env check
    // so `datum-connect --version` works without DATUM_SESSION set.
    let args = Args::parse();

    let session: Option<String> = std::env::var("DATUM_SESSION").ok();
    if session.is_none() && std::env::var("DATUM_PLUGIN_MODE").map(|v| v != "1").unwrap_or(true) {
        return Err(n0_error::anyerr!(
            "neither DATUM_SESSION nor DATUM_PLUGIN_MODE=1 set — this binary runs in plugin mode only"
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

    // Honour the `--debug` CLI flag by bumping the tracing filter to debug.
    // init_tracing() runs before Args::parse(), so the env var path covered
    // the initial subscriber; this reloads the filter for the flag case.
    // `RUST_LOG` always takes precedence and is left untouched when set.
    let rust_log_set = std::env::var("RUST_LOG").is_ok();
    if args.debug && !rust_log_set {
        let _ = DEBUG_FLAG.set(true);
        if let Some(handle) = RELOAD_HANDLE.get() {
            let _ = handle.modify(|f| {
                *f = EnvFilter::new("datum_connect=debug,connect_lib=debug");
            });
            eprintln!("[datum-connect] debug logging enabled (token refresh, heartbeat, etc.)");
        }
    }

    let json = args.json;

    let project_id = match args.project {
        Some(ref pid) => pid.clone(),
        None => {
            let session = std::env::var("DATUM_SESSION")
                .ok()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    n0_error::anyerr!(
                        "no project set — pass --project or run 'datumctl config set project <name>'"
                    )
                })?;
            session
        }
    };

    let ctx = resolve_project(&project_id);
    datum.set_selected_context(Some(ctx)).await?;

    let repo_path = match args.repo {
        Some(p) => p,
        None => match Repo::default_location() {
            Ok(p) => p,
            Err(e) => {
                eprint!("{e}");
                std::process::exit(64);
            }
        },
    };
    let repo = Repo::open_or_create(repo_path).await?;

    match args.command {
        Commands::List => {
            let node = ListenNode::new(repo.clone()).await?;
            let service = TunnelService::new(datum.clone(), node.clone());
            let (tunnels, orphans) = service.list_active_with_orphans().await?;
            let mut output: Vec<serde_json::Value> = tunnels
                .iter()
                .map(|t| {
                    let status = if t.accepted && t.programmed && t.connector_metadata_programmed {
                        "ready"
                    } else if t.accepted {
                        "accepted"
                    } else {
                        "pending"
                    };
                    let connector = if t.connector_ready { "ok" } else { "stale" };
                    serde_json::json!({
                        "type": "tunnel",
                        "id": t.id,
                        "label": t.label,
                        "endpoint": t.endpoint,
                        "status": status,
                        "enabled": t.enabled,
                        "hostnames": t.hostnames,
                        "connector": connector,
                        "connector_name": t.connector_name,
                        "connector_device": t.connector_device
                    })
                })
                .collect();
            for o in &orphans {
                let connector = if o.ready { "ok" } else { "stale" };
                output.push(serde_json::json!({
                    "type": "orphaned_connector",
                    "id": o.name,
                    "label": "",
                    "endpoint": "",
                    "status": "orphaned",
                    "enabled": false,
                    "hostnames": [],
                    "connector": connector,
                    "connector_name": o.name,
                    "connector_device": o.device
                }));
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&output).anyerr()?);
            } else {
                if output.is_empty() {
                    println!("No tunnels found.");
                }
                for t in &output {
                    println!("{}", serde_json::to_string(t).anyerr()?);
                }
            }
        }
        Commands::Listen { label, endpoint, id } => {
            // Plan 12-02 resolution rules (replaces plan 12-01 stubs):
            //   --endpoint only        → generate key in memory, create tunnel
            //   --id only              → real resolution via TunnelService::get_active;
            //                            read per-tunnel key, inherit endpoint
            //   --id + --endpoint      → validate endpoint agreement, read per-tunnel key
            //   neither flag           → picker with auto-adopt on len==1, error on len==0
            //
            // Per-tunnel key layout (Phase 17):
            //   --endpoint generates a key in memory → new_with_key() → persist after creation
            //   --id / picker read per-tunnel key → new_with_key() → no persistence needed
            //
            // Informed by datum-cloud/app@ca4470f (tunnel listen --id pins existing
            // tunnel and preserves its hostname) and @a68d8ae (--id alone resumes
            // an existing tunnel; --id+--endpoint must agree).
            //
            // The id branches pre-build (node, service) so we can call
            // get_active(&id). They stash the result in `preresolved_ns` so the
            // downstream block reuses them instead of re-creating.

            // Optional in-memory key: Some(key) for --endpoint (generated),
            // None for --id/picker (key read from disk).
            let mut in_memory_key: Option<SecretKey> = None;
            // Set when --id resume generates a new key because the original
            // was missing. Forces update_active to rewire the HTTPProxy to
            // the new connector.
            let mut force_rewire = false;

            let preresolved_ns: Option<(ListenNode, TunnelService, connect_lib::TunnelSummary)>;
            let endpoint: String = match (endpoint, id) {
                (Some(ep), None) => {
                    // --endpoint only: generate key in memory, use new_with_key()
                    let secret_key = SecretKey::generate(&mut rand::rng());
                    in_memory_key = Some(secret_key.clone());
                    ListenNode::new_with_key(repo.clone(), secret_key).await?;
                    // No existing tunnel — preresolved_ns stays None so
                    // the downstream block falls through to create.
                    preresolved_ns = None;
                    ep
                }
                (None, Some(tunnel_id)) => {
                    let node = ListenNode::new(repo.clone()).await?;
                    let service = TunnelService::new(datum.clone(), node.clone());
                    let t = service.get_active(&tunnel_id).await?.ok_or_else(|| {
                        n0_error::anyerr!("Tunnel '{tunnel_id}' not found in project {project_id}")
                    })?;
                    // Read the per-tunnel key. If missing (e.g. key file was
                    // deleted or tunnel created on another machine), generate
                    // a new key and force a rewire so the HTTPProxy points to
                    // a new connector with the same hostname.
                    let (key, should_rewire) =
                        resolve_listen_key(&repo, &project_id, &t.id).await?;
                    if should_rewire {
                        in_memory_key = Some(key.clone());
                        force_rewire = true;
                    }
                    let node = ListenNode::new_with_key(repo.clone(), key).await?;
                    let service = TunnelService::new(datum.clone(), node.clone());
                    // Inherit endpoint from the existing tunnel.
                    let ep = t.endpoint.clone();
                    preresolved_ns = Some((node, service, t));
                    ep
                }
                (Some(endpoint_val), Some(id_val)) => {
                    let node = ListenNode::new(repo.clone()).await?;
                    let service = TunnelService::new(datum.clone(), node.clone());
                    let t = service.get_active(&id_val).await?.ok_or_else(|| {
                        n0_error::anyerr!("Tunnel '{id_val}' not found in project {project_id}")
                    })?;
                    if t.endpoint != endpoint_val {
                        return Err(n0_error::anyerr!(
                            "--id '{id_val}' references endpoint '{}' but --endpoint was '{endpoint_val}' — they must agree (or omit --endpoint to inherit from the tunnel)",
                            t.endpoint
                        ));
                    }
                    // Read the per-tunnel key (same missing-key handling as above).
                    let (key, should_rewire) =
                        resolve_listen_key(&repo, &project_id, &t.id).await?;
                    if should_rewire {
                        in_memory_key = Some(key.clone());
                        force_rewire = true;
                    }
                    let node = ListenNode::new_with_key(repo.clone(), key).await?;
                    let service = TunnelService::new(datum.clone(), node.clone());
                    preresolved_ns = Some((node, service, t));
                    endpoint_val
                }
                (None, None) => {
                    // Picker codepath needs a service to call list_active.
                    // Build a temporary node for listing, then rebuild with
                    // the per-tunnel key after the user picks.
                    let node = ListenNode::new(repo.clone()).await?;
                    let service = TunnelService::new(datum.clone(), node.clone());
                    let tunnels = service.list_active().await?;
                    if tunnels.is_empty() {
                        return Err(n0_error::anyerr!(
                            "No tunnels exist in project {project_id}. Pass --endpoint to create one."
                        ));
                    }
                    let picked = if tunnels.len() == 1 {
                        // Auto-adopt the only candidate without popping a picker
                        // (informed by datum-cloud/app@cff37e7).
                        tunnels.into_iter().next().unwrap()
                    } else {
                        // Multiple candidates: silence tracing, prompt with inquire,
                        // restore tracing. inquire is sync, so call from a
                        // blocking task to keep the tokio runtime healthy.
                        let prev_filter = current_filter_string();
                        silence_tracing();
                        let choices: Vec<String> = tunnels
                            .iter()
                            .map(|t| format!("{}  ({})  → {}", t.label, t.id, t.endpoint))
                            .collect();
                        let chosen_idx_res = tokio::task::spawn_blocking(move || {
                            inquire::Select::new("Select a tunnel:", choices)
                                .with_starting_cursor(0)
                                .raw_prompt()
                                .map(|item| item.index)
                        })
                        .await
                        .map_err(|e| n0_error::anyerr!("picker task join failed: {e}"))?;
                        restore_tracing(&prev_filter);
                        let idx = chosen_idx_res
                            .map_err(|e| n0_error::anyerr!("picker error: {e}"))?;
                        tunnels.into_iter().nth(idx).unwrap()
                    };
                    // Read the per-tunnel key using the picked tunnel's name.
                    let key = repo
                        .listen_key_for_tunnel(&project_id, &picked.id)
                        .await?;
                    let node = ListenNode::new_with_key(repo.clone(), key).await?;
                    let service = TunnelService::new(datum.clone(), node.clone());
                    let ep = picked.endpoint.clone();
                    preresolved_ns = Some((node, service, picked));
                    ep
                }
            };

            // Reuse the (node, service, existing-tunnel) tuple if one of the
            // resolution branches above already built it; otherwise build now
            // and look up the existing tunnel by endpoint.
            let (node, service, existing) = match preresolved_ns {
                Some((n, s, t)) => {
                    // For --endpoint, t.id is empty — treat as "no existing tunnel".
                    let existing = if t.id.is_empty() { None } else { Some(t) };
                    (n, s, existing)
                }
                None => {
                    let n = ListenNode::new(repo.clone()).await?;
                    let s = TunnelService::new(datum.clone(), n.clone());
                    let existing = s.get_active_by_endpoint(&endpoint).await?;
                    // `--endpoint` carries a local, non-unique address (e.g.
                    // `localhost:8888`), so matching by endpoint alone would
                    // adopt a tunnel created on a different machine and rewire
                    // its HTTPProxy to this process's fresh connector —
                    // orphaning the original machine's connector and stealing
                    // the hostname. Only adopt when the existing tunnel's
                    // connector belongs to this device; otherwise fall through
                    // to create a new tunnel.
                    // (If connector_device is None the match fails and we
                    // also fall through to create a new tunnel.)
                    let existing = existing.and_then(|t| {
                        let here = connect_lib::friendly_device_name();
                        if t.connector_device.as_deref() == Some(here.as_str()) {
                            Some(t)
                        } else {
                            tracing::info!(
                                endpoint = %endpoint,
                                tunnel = %t.id,
                                device = ?t.connector_device,
                                here = %here,
                                "existing tunnel belongs to a different device; creating a new tunnel"
                            );
                            None
                        }
                    });
                    (n, s, existing)
                }
            };
            let endpoint_id = node.endpoint_id();
            let _ = writeln!(std::io::stderr(), "  \u{25CB} Your endpoint ID: {}", endpoint_id.to_string());
            let _ = writeln!(std::io::stderr(), "  \u{25CB} Setting up tunnel...");
            let _ = std::io::stderr().flush();

            let setup_start = std::time::Instant::now();
            let step_started_at = std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::<
                    connect_lib::ProgressStepKind,
                    std::time::Instant,
                >::new(),
            ));

            // Start heartbeat BEFORE create_active/ensure_connector so the
            // iroh endpoint connects to the relay and populates its address
            // before build_connection_details() runs. Without this the
            // connector status patch has no relay URL and the operator never
            // sees connection details → Connector/Ready stays False forever.
            let heartbeat = HeartbeatAgent::new(datum.clone(), node.clone());
            heartbeat.start().await;
            heartbeat.register_project(&project_id).await;
            // Wait up to 10s for the relay URL to appear (iroh connects fast
            // in practice, usually <1s). This is a best-effort poll; if the
            // relay never appears, ensure_connector will warn and proceed.
            for _ in 0..40 {
                if node.endpoint().addr().relay_urls().next().is_some() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }

            let tunnel_id = if let Some(t) = existing {
                if force_rewire || label.as_ref().is_some_and(|l| l != &t.label) {
                    let label_val = label.clone().unwrap_or_else(|| t.label.clone());
                    let updated = service.update_active(&t.id, &label_val, &endpoint).await?;
                    // Persist the new key if one was generated.
                    if let Some(ref secret_key) = in_memory_key {
                        repo.save_listen_key_for_tunnel(&project_id, &t.id, secret_key)
                            .await?;
                    }
                    // Clean up orphaned connectors left from the old key.
                    let deleted = service.cleanup_orphaned_connectors().await?;
                    for name in &deleted {
                        tracing::info!(connector = %name, "Cleaned up orphaned connector");
                    }
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({"type": "tunnel_updated", "id": updated.id})
                        );
                    }
                    updated.id
                } else {
                    t.id
                }
            } else {
                let label = label.unwrap_or_else(|| endpoint.clone());
                let tunnel = service.create_active(&label, &endpoint).await?;
                // Persist the in-memory key to the per-tunnel directory.
                if let Some(ref secret_key) = in_memory_key {
                    let key_path = repo
                        .path()
                        .join(&project_id)
                        .join(&tunnel.id)
                        .join(Repo::LISTEN_KEY_FILE);
                    tokio::fs::create_dir_all(key_path.parent().unwrap()).await?;
                    tokio::fs::write(&key_path, secret_key.to_bytes()).await?;
                }
                if json {
                    println!(
                        "{}",
                        serde_json::json!({"type": "tunnel_created", "id": tunnel.id})
                    );
                }
                tunnel.id
            };

            let _ = service.set_enabled_active(&tunnel_id, true).await;

            // Mode (Text/Json) routes callback output:
            //   Text → stderr (one transition line per change, prefixed by resource)
            //   Json → stdout (one tunnel_progress / tunnel_verifying /
            //                  tunnel_verified event per transition)
            // The Go supervisor's 'default: skip' case in connect/tunnel/listen/main.go
            // ignores the new event types; only the final tunnel_ready event
            // unblocks its gotReady handshake.
            let mode = if json { progress::Mode::Json } else { progress::Mode::Text };

            // Now start progress monitoring — heartbeat is already connecting,
            // so the operator sees Pending before Ready.
            let mode_for_cb = mode;
            let step_started_at_for_cb = step_started_at.clone();
            let progress_cb = move |step: &connect_lib::ProgressStep,
                                    prev: connect_lib::StepStatus| {
                let elapsed = {
                    let mut map = step_started_at_for_cb.lock().unwrap();
                    let timer = map
                        .entry(step.kind.clone())
                        .or_insert_with(std::time::Instant::now);
                    timer.elapsed()
                };
                progress::render_progress_step(mode_for_cb, step, prev, elapsed);
            };

            let service_for_progress = service.clone();
            let tunnel_id_for_progress = tunnel_id.clone();
            let progress_handle = tokio::spawn(async move {
                progress::await_tunnel_progress(&service_for_progress, &tunnel_id_for_progress, &progress_cb).await
            });

            let mut final_progress = progress_handle.await.unwrap()?;

            // Re-patch connectionDetails now that the connector is Ready:True.
            // This triggers the replicator to re-mirror the upstream-status
            // annotation to the downstream cluster with the current Ready:True
            // state, which in turn triggers Envoy Gateway to re-translate xDS
            // so the extension server injects the iroh cluster config.
            // Without this, if the annotation was captured at Ready:False
            // (race between replicator and lease renewal), the extension
            // server serves 503 indefinitely.
            let _ = service.refresh_connection_details().await;

            // Hostnames are written by the gateway controller shortly after
            // Programmed=True. Poll until one appears (usually <1s).
            if final_progress.hostnames.is_empty() {
                for _ in 0..20 {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    if let Ok(Some(p)) = service.get_active_progress(&tunnel_id).await {
                        if !p.hostnames.is_empty() {
                            final_progress = p;
                            break;
                        }
                    }
                }
            }
            let hostname = final_progress
                .hostnames
                .first()
                .cloned()
                .ok_or_else(|| {
                    n0_error::anyerr!("Tunnel {tunnel_id} has no hostname after Ready")
                })?;

            // Resolve the proxy hostname via authoritative DNS as a visible
            // step. This fails fast if the hostname cannot be resolved, and
            // prevents the HTTP probes below from getting stuck on DNS.
            progress::resolve_hostname_dns(&hostname).await?;

            // Verify origin is up and poll the tunnel URL every 10 seconds
            // until it returns a successful (non-5xx) response. Only after
            // this do we declare the tunnel ready to the user / Go supervisor.
            let verify_mode = mode;
            let service_for_refresh = service.clone();
            progress::verify_endpoints(
                &endpoint,
                &hostname,
                std::time::Duration::from_secs(10),
                move |label, url, elapsed, status| {
                    progress::render_verify(verify_mode, label, url, elapsed, status);
                },
                || {
                    let svc = service_for_refresh.clone();
                    tokio::spawn(async move {
                        if let Err(e) = svc.refresh_connection_details().await {
                            tracing::debug!("refresh during probe failed: {e:#}");
                        }
                    });
                },
            )
            .await?;

            // Re-fetch the up-to-date TunnelSummary for the tunnel_ready
            // payload (existing contract — id, label, endpoint, hostnames,
            // endpoint_id, status, elapsed_secs).
            let tunnel = service
                .get_active(&tunnel_id)
                .await?
                .ok_or_else(|| n0_error::anyerr!("Tunnel {tunnel_id} not found after setup"))?;

            let elapsed = setup_start.elapsed().as_secs();
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "type": "tunnel_ready",
                        "id": tunnel.id,
                        "label": tunnel.label,
                        "endpoint": tunnel.endpoint,
                        "hostnames": tunnel.hostnames,
                        "endpoint_id": endpoint_id.to_string(),
                        "status": "ready",
                        "elapsed_secs": elapsed
                    })
                );
            } else {
                for hostname in &tunnel.hostnames {
                    println!("Tunnel ready: https://{}", hostname);
                }
            }

            // --- Mid-session watch loop (Plan 12-04) ---
            // After tunnel_ready, watch three signals concurrently:
            //   1. ctrl_c        — user-initiated clean shutdown (exit 0)
            //   2. login_state   — credential expiry/revocation guidance
            //                      (text or JSON; does NOT exit so user can read)
            //   3. 10s poll      — detect mid-session terminal failure
            //                      (e.g. iroh-DNS collision flips post-Ready)
            //                      or upstream deletion (HTTPProxy removed)
            //
            // Cleanup (set_enabled_active false + tunnel_disabled) runs for
            // ALL exit paths via the post-loop block. Informed by upstream
            // datum-cloud/app@6264818 (runtime select-loop precedent).
            let mut login_rx = datum.login_state_watch();
            let mut runtime_poll =
                tokio::time::interval(std::time::Duration::from_secs(10));
            // First tick fires immediately; consume it so the first real poll
            // happens 10s after tunnel_ready (not concurrently with it).
            runtime_poll.tick().await;

            let exit_reason: ExitReason = loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        break ExitReason::CtrlC;
                    }
                    res = login_rx.changed() => {
                        if res.is_err() {
                            // Sender dropped — treat as a transient error, continue.
                            continue;
                        }
                        let state = login_rx.borrow().clone();
                        if state == connect_lib::LoginState::Missing {
                            let guidance =
                                "Datum login has expired or been revoked. \
                                 Stop this command and run `datum login` to refresh credentials. \
                                 The tunnel will continue running on cached credentials until they expire.";
                            if json {
                                println!(
                                    "{}",
                                    serde_json::json!({
                                        "type": "tunnel_login_lost",
                                        "id": tunnel_id,
                                        "message": guidance
                                    })
                                );
                            } else {
                                eprintln!("{}", guidance);
                            }
                            // Do NOT break — keep the tunnel running so the user has time to read.
                        }
                    }
                    _ = runtime_poll.tick() => {
                        match service.get_active_progress(&tunnel_id).await {
                            Ok(Some(progress)) => {
                                if let Some(failed) = progress.terminal_failure() {
                                    let msg = progress::format_terminal_failure(failed);
                                    if json {
                                        println!(
                                            "{}",
                                            serde_json::json!({
                                                "type": "tunnel_terminal_failure",
                                                "id": tunnel_id,
                                                "message": msg
                                            })
                                        );
                                    } else {
                                        eprintln!("{}", msg);
                                    }
                                    break ExitReason::TerminalFailure;
                                }
                            }
                            Ok(None) => {
                                let msg = format!(
                                    "Tunnel {tunnel_id} no longer exists on the server"
                                );
                                if json {
                                    println!(
                                        "{}",
                                        serde_json::json!({
                                            "type": "tunnel_deleted_upstream",
                                            "id": tunnel_id,
                                            "message": &msg
                                        })
                                    );
                                } else {
                                    eprintln!("{}", msg);
                                }
                                break ExitReason::DeletedUpstream;
                            }
                            Err(e) => {
                                tracing::warn!("transient progress query error: {e}");
                            }
                        }
                    }
                }
            };

            // --- Cleanup (runs for all exit paths) ---
            let outcome = service.delete_active(&tunnel_id).await;
            match &outcome {
                Ok(o) => {
                    if json {
                        let mut resources = Vec::new();
                        if let Some(ref name) = o.http_proxy {
                            resources.push(serde_json::json!({"type": "HTTPProxy", "name": name}));
                        }
                        if let Some(ref name) = o.connector_ad {
                            resources.push(serde_json::json!({"type": "ConnectorAdvertisement", "name": name}));
                        }
                        if let Some(ref name) = o.traffic_protection_policy {
                            resources.push(serde_json::json!({"type": "TrafficProtectionPolicy", "name": name}));
                        }
                        if let Some(ref name) = o.connector {
                            resources.push(serde_json::json!({"type": "Connector", "name": name}));
                        }
                        println!(
                            "{}",
                            serde_json::json!({
                                "type": "tunnel_deleted",
                                "id": tunnel_id,
                                "deleted": true,
                                "resources": resources
                            })
                        );
                    } else {
                        println!("Deleted tunnel {}", tunnel_id);
                        if let Some(ref name) = o.http_proxy {
                            println!("  HTTPProxy {}", name);
                        }
                        if let Some(ref name) = o.connector_ad {
                            println!("  ConnectorAdvertisement {}", name);
                        }
                        if let Some(ref name) = o.traffic_protection_policy {
                            println!("  TrafficProtectionPolicy {}", name);
                        }
                        if let Some(ref name) = o.connector {
                            println!("  Connector {}", name);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("failed to delete tunnel on shutdown: {e}");
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({"type": "tunnel_deleted", "id": tunnel_id})
                        );
                    }
                }
            }

            // Non-zero exit for terminal failures.
            return match exit_reason {
                ExitReason::CtrlC => Ok(()),
                ExitReason::TerminalFailure => {
                    Err(n0_error::anyerr!("tunnel exited with terminal failure"))
                }
                ExitReason::DeletedUpstream => {
                    Err(n0_error::anyerr!("tunnel deleted upstream"))
                }
            };
        }
        Commands::Update { id, label, endpoint } => {
            let node = ListenNode::new(repo.clone()).await?;
            let service = TunnelService::new(datum.clone(), node.clone());
            let current = service
                .get_active(&id)
                .await?
                .ok_or_else(|| n0_error::anyerr!("Tunnel {} not found", id))?;
            let new_label = label.unwrap_or(current.label);
            let new_endpoint = endpoint.unwrap_or(current.endpoint);
            let tunnel = service.update_active(&id, &new_label, &new_endpoint).await?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "type": "tunnel_updated",
                        "id": tunnel.id,
                        "label": tunnel.label,
                        "endpoint": tunnel.endpoint,
                        "hostnames": tunnel.hostnames
                    })
                );
            } else {
                println!("Updated tunnel {}:", tunnel.id);
                println!("  label: {}", tunnel.label);
                println!("  endpoint: {}", tunnel.endpoint);
            }
        }
        Commands::Delete { id } => {
            let node = ListenNode::new(repo.clone()).await?;
            let service = TunnelService::new(datum.clone(), node.clone());
            let outcome = service.delete_active(&id).await?;
            if json {
                let mut resources = Vec::new();
                if let Some(ref name) = outcome.http_proxy {
                    resources.push(serde_json::json!({"type": "HTTPProxy", "name": name}));
                }
                if let Some(ref name) = outcome.connector_ad {
                    resources.push(serde_json::json!({"type": "ConnectorAdvertisement", "name": name}));
                }
                if let Some(ref name) = outcome.traffic_protection_policy {
                    resources.push(serde_json::json!({"type": "TrafficProtectionPolicy", "name": name}));
                }
                if let Some(ref name) = outcome.connector {
                    resources.push(serde_json::json!({"type": "Connector", "name": name}));
                }
                println!(
                    "{}",
                    serde_json::json!({
                        "type": "tunnel_deleted",
                        "id": id,
                        "deleted": true,
                        "resources": resources
                    })
                );
            } else {
                println!("Deleted tunnel {}", id);
                if let Some(name) = outcome.http_proxy {
                    println!("  HTTPProxy {}", name);
                }
                if let Some(name) = outcome.connector_ad {
                    println!("  ConnectorAdvertisement {}", name);
                }
                if let Some(name) = outcome.traffic_protection_policy {
                    println!("  TrafficProtectionPolicy {}", name);
                }
                if let Some(name) = outcome.connector {
                    println!("  Connector {}", name);
                }
            }
        }
    }
    Ok(())
}
