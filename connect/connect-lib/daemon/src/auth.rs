//! Local API auth: setup vs. operate token tiers.
//!
//! Two tiers, filesystem-trust as the human root of trust (deliberately not
//! cryptographic proof-of-humanity — the daemon already relies on "same OS
//! user, same machine" via loopback-only binding, this just narrows that
//! down further for callers that shouldn't get full control):
//!
//! - **Setup token**: one per daemon instance, full access. Generated once
//!   on first startup, stored plaintext (it has to be — the CLI reads back
//!   the literal file to send as a bearer value) at
//!   `connect_dir/daemon_auth/setup.token`, 0600 on unix.
//! - **Operate tokens**: minted per-tunnel, scoped to exactly one tunnel id.
//!   Presented as a self-addressing bearer value `<token_id>.<secret>` so
//!   lookup is a direct file open (`operate_tokens/<tunnel_id>/<token_id>.json`),
//!   never a directory scan. Only a salted hash of the secret is ever
//!   written to disk (`ring::digest::SHA256`, already a transitive
//!   dependency via `rustls` — this is a one-line `Cargo.toml` addition, not
//!   new supply-chain surface), compared via `ring::constant_time`.
//! - **Viewer token**: one per daemon instance, like the setup token, but
//!   read-only and global — list/get tunnels, progress, metrics, traffic,
//!   and replay. Never create/delete a tunnel, start/stop one, mint or
//!   revoke any token (viewer or operate), or read the audit log. Built
//!   specifically for the browser dashboard, which previously had no
//!   choice but to hold the full setup token even though its own UI never
//!   exposes any configuration action — a real, if low-risk, credential
//!   mismatch that this closes. Unlike the setup token, **not created
//!   automatically at startup** — it only exists once someone deliberately
//!   mints one (`POST /v1/viewer-token`, setup-tier only), so a fresh
//!   daemon has no standing read-only credential floating around until
//!   asked for one.

use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use crate::AppState;

const SETUP_TOKEN_BYTES: usize = 32;
const VIEWER_TOKEN_BYTES: usize = 32;
const OPERATE_SECRET_BYTES: usize = 32;
const OPERATE_TOKEN_ID_BYTES: usize = 9;
const SALT_BYTES: usize = 16;
const AUDIT_TAIL_LINES: usize = 500;

/// Caller identity attached to the request by whichever tier middleware ran,
/// read back by handlers that need to know who's acting (currently
/// start/stop, for the audit log's `actor` field) via the
/// `Extension<Actor>` extractor.
#[derive(Clone, Debug)]
pub enum Actor {
    Setup,
    /// The tunnel id isn't carried here — every handler that needs it
    /// already has it from its own `Path` extraction, since an operate
    /// token is only ever valid for the exact `:id` in the URL it was
    /// checked against.
    Operate { token_id: String },
    /// Global, read-only — see the module doc comment's "Viewer token"
    /// section.
    Viewer,
}

impl Actor {
    pub fn audit_label(&self) -> String {
        match self {
            Actor::Setup => "setup".to_string(),
            Actor::Operate { token_id } => format!("operate:{token_id}"),
            Actor::Viewer => "viewer".to_string(),
        }
    }
}

fn auth_dir(base: &FsPath) -> PathBuf {
    base.join("daemon_auth")
}

fn setup_token_path(base: &FsPath) -> PathBuf {
    auth_dir(base).join("setup.token")
}

fn viewer_token_path(base: &FsPath) -> PathBuf {
    auth_dir(base).join("viewer.token")
}

fn operate_tokens_dir(base: &FsPath, tunnel_id: &str) -> PathBuf {
    auth_dir(base).join("operate_tokens").join(tunnel_id)
}

fn operate_token_path(base: &FsPath, tunnel_id: &str, token_id: &str) -> PathBuf {
    operate_tokens_dir(base, tunnel_id).join(format!("{token_id}.json"))
}

fn audit_log_path(base: &FsPath) -> PathBuf {
    auth_dir(base).join("audit.jsonl")
}

fn now_unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn b64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn b64_decode(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s)
}

fn random_b64(n: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; n];
    rand::rng().fill_bytes(&mut buf);
    b64_encode(&buf)
}

fn hash_secret(salt: &[u8], secret: &str) -> Vec<u8> {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    ctx.update(salt);
    ctx.update(secret.as_bytes());
    ctx.finish().as_ref().to_vec()
}

/// Plain manual constant-time comparison (XOR-fold over every byte,
/// regardless of where a mismatch occurs) rather than ring's
/// `constant_time::verify_slices_are_equal`, which is deprecated upstream
/// ("internal function not intended for external use"). This is the
/// standard textbook pattern and doesn't depend on ring for it.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(unix)]
async fn set_owner_only_perms(path: &FsPath) {
    use std::os::unix::fs::PermissionsExt;
    let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await;
}

// Windows has no equivalent of Unix mode bits — this strips inherited ACL
// entries and grants full control to the current user only, via `icacls`
// rather than hand-rolled SECURITY_DESCRIPTOR/DACL FFI (a shelled-out,
// well-tested system tool is far easier to get right, and to audit, than
// unsafe Win32 calls for something security-load-bearing like this).
// Without this, token files previously inherited whatever ACL their parent
// folder happened to have — on a shared machine, that could mean any other
// local account could read the setup token and take over the daemon.
#[cfg(windows)]
async fn set_owner_only_perms(path: &FsPath) {
    let Ok(username) = std::env::var("USERNAME") else {
        tracing::warn!(
            path = %path.display(),
            "could not determine current user (USERNAME unset); leaving default ACL in place"
        );
        return;
    };
    let identity = match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => format!("{domain}\\{username}"),
        _ => username,
    };

    let result = tokio::process::Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r") // drop inherited entries from the parent folder
        .arg("/grant:r")
        .arg(format!("{identity}:F")) // grant only this user full control
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => {}
        Ok(output) => tracing::warn!(
            path = %path.display(),
            stderr = %String::from_utf8_lossy(&output.stderr),
            "icacls failed to restrict permissions on token file"
        ),
        Err(e) => tracing::warn!(
            path = %path.display(),
            error = %e,
            "failed to invoke icacls to restrict permissions on token file"
        ),
    }
}
#[cfg(not(any(unix, windows)))]
async fn set_owner_only_perms(_path: &FsPath) {}

/// Write-temp-then-rename so a concurrent reader (a validating request, or
/// another mint/revoke racing on a *different* token file) never sees a
/// torn/partial JSON body — plain in-place writes don't give that guarantee.
pub(crate) async fn write_json_atomic<T: Serialize>(path: &FsPath, value: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).expect("serialize auth record");
    write_atomic_raw(path, &bytes).await
}

async fn write_atomic_raw(path: &FsPath, bytes: &[u8]) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp: PathBuf = tmp.into();
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Loaded once at daemon startup into `AppState`, not re-read per request.
/// Never regenerates an existing token — `create_new` fails closed if the
/// file's already there, so a daemon restart doesn't silently invalidate
/// every client's stored credential.
pub async fn load_or_create_setup_token(base: &FsPath) -> std::io::Result<String> {
    tokio::fs::create_dir_all(auth_dir(base)).await?;
    let path = setup_token_path(base);
    match tokio::fs::OpenOptions::new().create_new(true).write(true).open(&path).await {
        Ok(mut f) => {
            use tokio::io::AsyncWriteExt;
            let token = random_b64(SETUP_TOKEN_BYTES);
            f.write_all(token.as_bytes()).await?;
            drop(f);
            set_owner_only_perms(&path).await;
            Ok(token)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => tokio::fs::read_to_string(&path).await,
        Err(e) => Err(e),
    }
}

/// Loads an existing viewer token from disk at startup, if one was minted
/// in a previous run — unlike the setup token, never creates one. Returns
/// `Ok(None)` (not an error) when no viewer token has ever been minted.
pub async fn load_viewer_token(base: &FsPath) -> std::io::Result<Option<String>> {
    match tokio::fs::read_to_string(viewer_token_path(base)).await {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Mints a fresh viewer token, overwriting any existing one (there is only
/// ever one — minting again is how you rotate it without a separate
/// "revoke then create" step).
async fn create_viewer_token_on_disk(base: &FsPath) -> std::io::Result<String> {
    tokio::fs::create_dir_all(auth_dir(base)).await?;
    let path = viewer_token_path(base);
    let token = random_b64(VIEWER_TOKEN_BYTES);
    write_atomic_raw(&path, token.as_bytes()).await?;
    set_owner_only_perms(&path).await;
    Ok(token)
}

async fn revoke_viewer_token_on_disk(base: &FsPath) -> std::io::Result<bool> {
    match tokio::fs::remove_file(viewer_token_path(base)).await {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct OperateTokenRecord {
    token_id: String,
    tunnel_id: String,
    salt_b64: String,
    hash_b64: String,
    created_at_unix_ms: u64,
    expires_at_unix_ms: Option<u64>,
    revoked: bool,
}

#[derive(Serialize)]
pub struct MintedToken {
    pub token_id: String,
    /// `<token_id>.<secret>` — the actual bearer value. Shown exactly once;
    /// only a salted hash of the secret half is ever persisted.
    pub bearer: String,
    pub tunnel_id: String,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: Option<u64>,
}

#[derive(Serialize)]
pub struct TokenMeta {
    pub token_id: String,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: Option<u64>,
    pub revoked: bool,
}

pub enum RevokeOutcome {
    Revoked,
    NotFound,
}

pub async fn mint_operate_token(
    base: &FsPath,
    tunnel_id: &str,
    ttl_seconds: Option<u64>,
) -> std::io::Result<MintedToken> {
    tokio::fs::create_dir_all(operate_tokens_dir(base, tunnel_id)).await?;
    let token_id = random_b64(OPERATE_TOKEN_ID_BYTES);
    let secret = random_b64(OPERATE_SECRET_BYTES);
    let mut salt = vec![0u8; SALT_BYTES];
    {
        use rand::RngCore;
        rand::rng().fill_bytes(&mut salt);
    }
    let hash = hash_secret(&salt, &secret);
    let now = now_unix_ms();
    let expires_at_unix_ms = ttl_seconds.map(|s| now + s.saturating_mul(1000));
    let record = OperateTokenRecord {
        token_id: token_id.clone(),
        tunnel_id: tunnel_id.to_string(),
        salt_b64: b64_encode(&salt),
        hash_b64: b64_encode(&hash),
        created_at_unix_ms: now,
        expires_at_unix_ms,
        revoked: false,
    };
    write_json_atomic(&operate_token_path(base, tunnel_id, &token_id), &record).await?;
    Ok(MintedToken {
        token_id: token_id.clone(),
        bearer: format!("{token_id}.{secret}"),
        tunnel_id: tunnel_id.to_string(),
        created_at_unix_ms: now,
        expires_at_unix_ms,
    })
}

pub async fn list_operate_tokens(base: &FsPath, tunnel_id: &str) -> std::io::Result<Vec<TokenMeta>> {
    let dir = operate_tokens_dir(base, tunnel_id);
    let mut out = Vec::new();
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    while let Some(entry) = entries.next_entry().await? {
        let Ok(content) = tokio::fs::read(entry.path()).await else { continue };
        if let Ok(r) = serde_json::from_slice::<OperateTokenRecord>(&content) {
            out.push(TokenMeta {
                token_id: r.token_id,
                created_at_unix_ms: r.created_at_unix_ms,
                expires_at_unix_ms: r.expires_at_unix_ms,
                revoked: r.revoked,
            });
        }
    }
    out.sort_by_key(|t| t.created_at_unix_ms);
    Ok(out)
}

pub async fn revoke_operate_token(base: &FsPath, tunnel_id: &str, token_id: &str) -> std::io::Result<RevokeOutcome> {
    let path = operate_token_path(base, tunnel_id, token_id);
    let content = match tokio::fs::read(&path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(RevokeOutcome::NotFound),
        Err(e) => return Err(e),
    };
    let mut record: OperateTokenRecord = serde_json::from_slice(&content)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    record.revoked = true;
    write_json_atomic(&path, &record).await?;
    Ok(RevokeOutcome::Revoked)
}

/// Best-effort cascade on profile delete — closes the (extremely unlikely,
/// since tunnel ids are random) stale-authorization gap where a deleted
/// tunnel's old operate tokens could otherwise validate against a future
/// tunnel that happened to reuse the same id.
pub async fn delete_tokens_for_tunnel(base: &FsPath, tunnel_id: &str) {
    let _ = tokio::fs::remove_dir_all(operate_tokens_dir(base, tunnel_id)).await;
}

async fn validate_operate_secret(base: &FsPath, tunnel_id: &str, token_id: &str, secret: &str) -> bool {
    let Ok(content) = tokio::fs::read(operate_token_path(base, tunnel_id, token_id)).await else {
        return false;
    };
    let Ok(record) = serde_json::from_slice::<OperateTokenRecord>(&content) else {
        return false;
    };
    if record.tunnel_id != tunnel_id || record.revoked {
        return false;
    }
    if let Some(exp) = record.expires_at_unix_ms {
        if now_unix_ms() > exp {
            return false;
        }
    }
    let (Ok(salt), Ok(expected)) = (b64_decode(&record.salt_b64), b64_decode(&record.hash_b64)) else {
        return false;
    };
    constant_time_eq(&hash_secret(&salt, secret), &expected)
}

fn unauthorized(msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": msg })))
}

fn forbidden(msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::FORBIDDEN, Json(json!({ "error": msg })))
}

fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    v.strip_prefix("Bearer ").map(|s| s.trim().to_string())
}

/// Gate for setup-only routes. A well-formed operate token (parses as
/// `<id>.<secret>`) is a real credential of the wrong tier -> 403; anything
/// else not matching the setup token -> 401.
pub async fn require_setup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    mut req: Request,
    next: Next,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    let Some(bearer) = extract_bearer(&headers) else {
        return Err(unauthorized("missing Authorization: Bearer header"));
    };
    if constant_time_eq(bearer.as_bytes(), state.setup_token.as_bytes()) {
        req.extensions_mut().insert(Actor::Setup);
        return Ok(next.run(req).await);
    }
    if bearer.contains('.') || matches_viewer_token(&state, &bearer).await {
        return Err(forbidden("this endpoint requires setup-tier access"));
    }
    Err(unauthorized("invalid credential"))
}

/// Gate for start/stop/progress: setup token, or an operate token scoped to
/// exactly this route's `:id`. A mismatched/unknown/revoked/expired operate
/// token collapses to 401 here rather than distinguishing "wrong scope" from
/// "invalid" — telling them apart would require scanning every tunnel's
/// token directory instead of the one O(1) lookup this path is built
/// around, for a distinction with little practical value to the caller.
pub async fn require_operate_or_setup(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    mut req: Request,
    next: Next,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    let Some(bearer) = extract_bearer(&headers) else {
        return Err(unauthorized("missing Authorization: Bearer header"));
    };
    if constant_time_eq(bearer.as_bytes(), state.setup_token.as_bytes()) {
        req.extensions_mut().insert(Actor::Setup);
        return Ok(next.run(req).await);
    }
    if matches_viewer_token(&state, &bearer).await {
        return Err(forbidden("this endpoint requires setup or operate access, not viewer (read-only)"));
    }
    let Some((token_id, secret)) = bearer.split_once('.') else {
        return Err(unauthorized("invalid credential"));
    };
    if validate_operate_secret(&state.connect_dir, &id, token_id, secret).await {
        req.extensions_mut().insert(Actor::Operate { token_id: token_id.to_string() });
        Ok(next.run(req).await)
    } else {
        Err(unauthorized("invalid, expired, or revoked credential for this tunnel"))
    }
}

async fn matches_viewer_token(state: &AppState, bearer: &str) -> bool {
    match state.viewer_token.read().await.as_deref() {
        Some(token) => constant_time_eq(bearer.as_bytes(), token.as_bytes()),
        None => false,
    }
}

/// Gate for global, tunnel-agnostic reads (list, get, traffic, replay):
/// setup token or the global viewer token. No operate access here — an
/// operate token is scoped to acting on one specific tunnel it already
/// knows the id of, not to browsing every tunnel on the daemon.
pub async fn require_setup_or_viewer(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    mut req: Request,
    next: Next,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    let Some(bearer) = extract_bearer(&headers) else {
        return Err(unauthorized("missing Authorization: Bearer header"));
    };
    if constant_time_eq(bearer.as_bytes(), state.setup_token.as_bytes()) {
        req.extensions_mut().insert(Actor::Setup);
        return Ok(next.run(req).await);
    }
    if matches_viewer_token(&state, &bearer).await {
        req.extensions_mut().insert(Actor::Viewer);
        return Ok(next.run(req).await);
    }
    if bearer.contains('.') {
        return Err(forbidden("this endpoint requires setup or viewer access"));
    }
    Err(unauthorized("invalid credential"))
}

/// Gate for per-tunnel reads (progress, metrics): setup, the global viewer
/// token, or an operate token scoped to exactly this route's `:id`.
pub async fn require_setup_or_operate_or_viewer(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    mut req: Request,
    next: Next,
) -> Result<Response, (StatusCode, Json<serde_json::Value>)> {
    let Some(bearer) = extract_bearer(&headers) else {
        return Err(unauthorized("missing Authorization: Bearer header"));
    };
    if constant_time_eq(bearer.as_bytes(), state.setup_token.as_bytes()) {
        req.extensions_mut().insert(Actor::Setup);
        return Ok(next.run(req).await);
    }
    if matches_viewer_token(&state, &bearer).await {
        req.extensions_mut().insert(Actor::Viewer);
        return Ok(next.run(req).await);
    }
    let Some((token_id, secret)) = bearer.split_once('.') else {
        return Err(unauthorized("invalid credential"));
    };
    if validate_operate_secret(&state.connect_dir, &id, token_id, secret).await {
        req.extensions_mut().insert(Actor::Operate { token_id: token_id.to_string() });
        Ok(next.run(req).await)
    } else {
        Err(unauthorized("invalid, expired, or revoked credential for this tunnel"))
    }
}

pub async fn append_audit(base: &FsPath, lock: &Mutex<()>, event: &str, tunnel_id: &str, actor: &str) {
    let _guard = lock.lock().await;
    if let Err(e) = tokio::fs::create_dir_all(auth_dir(base)).await {
        tracing::warn!("failed to create audit dir: {e:#}");
        return;
    }
    let mut line = json!({
        "ts_unix_ms": now_unix_ms(),
        "event": event,
        "tunnel_id": tunnel_id,
        "actor": actor,
    })
    .to_string();
    line.push('\n');
    use tokio::io::AsyncWriteExt;
    match tokio::fs::OpenOptions::new().create(true).append(true).open(audit_log_path(base)).await {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()).await {
                tracing::warn!("failed to append audit log: {e:#}");
            }
        }
        Err(e) => tracing::warn!("failed to open audit log: {e:#}"),
    }
}

pub async fn read_audit_tail(base: &FsPath) -> Vec<serde_json::Value> {
    let content = match tokio::fs::read_to_string(audit_log_path(base)).await {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(AUDIT_TAIL_LINES);
    lines[start..].iter().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// Who most recently caused `event` for `tunnel_id`, from the retained
/// audit tail (`setup`, `operate:<token_id>`, or `system`) — `None` if it's
/// never happened within that window. Built for the dashboard's "visible
/// signal" badge/toast when an agent (an operate token) turns a tunnel on,
/// rather than adding new dedicated state: the audit log already durably
/// records exactly this, so this just answers a more specific question
/// against it instead of returning the whole log.
pub async fn last_actor_for_event(base: &FsPath, tunnel_id: &str, event: &str) -> Option<String> {
    read_audit_tail(base).await.into_iter().rev().find_map(|e| {
        if e.get("tunnel_id")?.as_str()? == tunnel_id && e.get("event")?.as_str()? == event {
            e.get("actor")?.as_str().map(str::to_string)
        } else {
            None
        }
    })
}

// --- HTTP handlers ---

#[derive(Deserialize)]
pub struct CreateTokenRequest {
    pub ttl_seconds: Option<u64>,
}

pub async fn create_token(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<CreateTokenRequest>,
) -> crate::ApiResult<MintedToken> {
    match state.control.get_active(&id).await {
        Ok(Some(_)) => {}
        Ok(None) => return Err(crate::not_found(&id)),
        Err(e) => return Err(crate::err_response(e)),
    }
    let minted = mint_operate_token(&state.connect_dir, &id, req.ttl_seconds)
        .await
        .map_err(crate::err_response)?;
    append_audit(&state.connect_dir, &state.audit_lock, "token_created", &id, "setup").await;
    Ok(Json(minted))
}

pub async fn list_tokens(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> crate::ApiResult<Vec<TokenMeta>> {
    list_operate_tokens(&state.connect_dir, &id).await.map(Json).map_err(crate::err_response)
}

pub async fn revoke_token(
    State(state): State<Arc<AppState>>,
    Path((id, token_id)): Path<(String, String)>,
) -> crate::ApiResult<serde_json::Value> {
    match revoke_operate_token(&state.connect_dir, &id, &token_id).await {
        Ok(RevokeOutcome::Revoked) => {
            append_audit(&state.connect_dir, &state.audit_lock, "token_revoked", &id, "setup").await;
            Ok(Json(json!({ "revoked": true, "token_id": token_id })))
        }
        Ok(RevokeOutcome::NotFound) => Err(crate::not_found(&token_id)),
        Err(e) => Err(crate::err_response(e)),
    }
}

pub async fn get_audit(State(state): State<Arc<AppState>>) -> crate::ApiResult<Vec<serde_json::Value>> {
    Ok(Json(read_audit_tail(&state.connect_dir).await))
}

#[derive(Serialize)]
pub struct ViewerTokenStatus {
    pub exists: bool,
}

pub async fn get_viewer_token_status(
    State(state): State<Arc<AppState>>,
) -> crate::ApiResult<ViewerTokenStatus> {
    Ok(Json(ViewerTokenStatus { exists: state.viewer_token.read().await.is_some() }))
}

#[derive(Serialize)]
pub struct MintedViewerToken {
    /// Shown once, like an operate token's secret — the daemon only ever
    /// keeps this in memory plus the plaintext file (it has to be
    /// plaintext for the same reason the setup token is: a real client has
    /// to be able to read it back and use it as a bearer value).
    pub token: String,
}

pub async fn create_viewer_token(
    State(state): State<Arc<AppState>>,
) -> crate::ApiResult<MintedViewerToken> {
    let token = create_viewer_token_on_disk(&state.connect_dir)
        .await
        .map_err(crate::err_response)?;
    *state.viewer_token.write().await = Some(token.clone());
    append_audit(&state.connect_dir, &state.audit_lock, "viewer_token_created", "-", "setup").await;
    Ok(Json(MintedViewerToken { token }))
}

pub async fn revoke_viewer_token(
    State(state): State<Arc<AppState>>,
) -> crate::ApiResult<serde_json::Value> {
    let existed = revoke_viewer_token_on_disk(&state.connect_dir).await.map_err(crate::err_response)?;
    *state.viewer_token.write().await = None;
    if existed {
        append_audit(&state.connect_dir, &state.audit_lock, "viewer_token_revoked", "-", "setup").await;
    }
    Ok(Json(json!({ "revoked": existed })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("datum-connect-daemon-auth-test-{name}-{}", now_unix_ms()));
        dir
    }

    #[tokio::test]
    async fn setup_token_is_created_once_and_stable_across_reloads() {
        let base = scratch_dir("setup-token");
        let first = load_or_create_setup_token(&base).await.unwrap();
        let second = load_or_create_setup_token(&base).await.unwrap();
        assert_eq!(first, second, "a restart must not invalidate the existing setup token");
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn operate_token_round_trip_validates() {
        let base = scratch_dir("operate-roundtrip");
        let minted = mint_operate_token(&base, "tunnel-a", Some(3600)).await.unwrap();
        let (token_id, secret) = minted.bearer.split_once('.').unwrap();
        assert!(validate_operate_secret(&base, "tunnel-a", token_id, secret).await);
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn operate_token_rejected_for_a_different_tunnel() {
        let base = scratch_dir("operate-wrong-scope");
        let minted = mint_operate_token(&base, "tunnel-a", None).await.unwrap();
        let (token_id, secret) = minted.bearer.split_once('.').unwrap();
        assert!(!validate_operate_secret(&base, "tunnel-b", token_id, secret).await);
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn operate_token_rejected_after_revoke() {
        let base = scratch_dir("operate-revoke");
        let minted = mint_operate_token(&base, "tunnel-a", None).await.unwrap();
        let (token_id, secret) = minted.bearer.split_once('.').unwrap();
        assert!(validate_operate_secret(&base, "tunnel-a", token_id, secret).await);
        matches!(
            revoke_operate_token(&base, "tunnel-a", token_id).await.unwrap(),
            RevokeOutcome::Revoked
        );
        assert!(!validate_operate_secret(&base, "tunnel-a", token_id, secret).await);
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn operate_token_rejected_once_expired() {
        let base = scratch_dir("operate-expired");
        // ttl_seconds = 0 -> expires_at is "now", so it's already in the past.
        let minted = mint_operate_token(&base, "tunnel-a", Some(0)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let (token_id, secret) = minted.bearer.split_once('.').unwrap();
        assert!(!validate_operate_secret(&base, "tunnel-a", token_id, secret).await);
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn wrong_secret_is_rejected() {
        let base = scratch_dir("operate-wrong-secret");
        let minted = mint_operate_token(&base, "tunnel-a", None).await.unwrap();
        let (token_id, _) = minted.bearer.split_once('.').unwrap();
        assert!(!validate_operate_secret(&base, "tunnel-a", token_id, "not-the-real-secret").await);
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn list_reflects_mint_and_revoke() {
        let base = scratch_dir("operate-list");
        let a = mint_operate_token(&base, "tunnel-a", None).await.unwrap();
        let _b = mint_operate_token(&base, "tunnel-a", None).await.unwrap();
        let listed = list_operate_tokens(&base, "tunnel-a").await.unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|t| !t.revoked));

        revoke_operate_token(&base, "tunnel-a", &a.token_id).await.unwrap();
        let listed = list_operate_tokens(&base, "tunnel-a").await.unwrap();
        let revoked_one = listed.iter().find(|t| t.token_id == a.token_id).unwrap();
        assert!(revoked_one.revoked);
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn cascade_delete_removes_all_tokens_for_a_tunnel() {
        let base = scratch_dir("operate-cascade");
        mint_operate_token(&base, "tunnel-a", None).await.unwrap();
        mint_operate_token(&base, "tunnel-a", None).await.unwrap();
        delete_tokens_for_tunnel(&base, "tunnel-a").await;
        let listed = list_operate_tokens(&base, "tunnel-a").await.unwrap();
        assert!(listed.is_empty());
        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn audit_log_appends_are_readable_in_order() {
        let base = scratch_dir("audit-log");
        let lock = Mutex::new(());
        append_audit(&base, &lock, "create", "tunnel-a", "setup").await;
        append_audit(&base, &lock, "start", "tunnel-a", "operate:abc").await;
        append_audit(&base, &lock, "auto_expired", "tunnel-a", "system").await;
        let entries = read_audit_tail(&base).await;
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["event"], "create");
        assert_eq!(entries[1]["actor"], "operate:abc");
        assert_eq!(entries[2]["event"], "auto_expired");
        let _ = tokio::fs::remove_dir_all(&base).await;
    }
}
