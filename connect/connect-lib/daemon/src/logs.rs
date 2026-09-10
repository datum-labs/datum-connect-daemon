//! Registered log-file "tail" sources for the dashboard — see
//! `LOG-TAIL-PLAN.md` for the full design rationale.
//!
//! Registering a source (deciding *which* file becomes readable at all) is
//! setup-only, the same trust tier as `tunnel create` / `peer advertise` —
//! it's the action that determines what's exposed, not just viewing it.
//! Reading a registered source's tail is setup-or-viewer, same as tunnel
//! traffic, so the browser dashboard can show it holding only a viewer
//! token. That means a viewer token can read the content of every
//! registered log — same shape of exposure API-REFERENCE.md already
//! documents for captured traffic bodies, extended here to cover log
//! content too rather than inventing a third tier.
//!
//! Deliberately does **not** accept an arbitrary path in the tail request
//! itself — only a pre-registered source name. See LOG-TAIL-PLAN.md's
//! "Shape: two separate concerns, two separate tiers" section for why.

use std::collections::HashMap;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::{Mutex, RwLock};

use crate::{err_response, ApiResult};

const READ_CHUNK_SIZE: u64 = 64 * 1024;
/// Hard ceiling on total bytes read while tailing, regardless of how many
/// newlines have been found — without this, a registered file with very few
/// (or zero) newlines would read the entire file into memory despite this
/// function's whole point being to avoid that. Found in review 2026-09-07.
const MAX_TAIL_READ_BYTES: u64 = 8 * 1024 * 1024;

fn sources_path(base: &Path) -> PathBuf {
    base.join("daemon_logs").join("sources.json")
}

fn source_not_found(name: &str) -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::NOT_FOUND, Json(json!({ "error": format!("log source '{name}' not found") })))
}

/// The registry itself. Loaded once at startup, mutated only by the two
/// setup-only endpoints below (`register`/`remove`), persisted to
/// `daemon_logs/sources.json` under the daemon's connect dir so it survives
/// a restart — same persistence style as `daemon_auth/`.
pub struct LogSources {
    sources: RwLock<HashMap<String, PathBuf>>,
    /// Serializes mutate+persist end to end (including the disk write) so
    /// two concurrent `log add`/`log remove` calls can't race and silently
    /// drop one another's update to `sources.json` — same class of bug (and
    /// fix) as `peer.rs`'s `PeerState::advertised_lock`, found in review
    /// 2026-09-07.
    persist_lock: Mutex<()>,
}

impl LogSources {
    pub async fn load(base: &Path) -> std::io::Result<Self> {
        let map = match tokio::fs::read(sources_path(base)).await {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e),
        };
        Ok(Self { sources: RwLock::new(map), persist_lock: Mutex::new(()) })
    }

    async fn save(&self, base: &Path) -> std::io::Result<()> {
        tokio::fs::create_dir_all(base.join("daemon_logs")).await?;
        let snapshot = self.sources.read().await.clone();
        crate::auth::write_json_atomic(&sources_path(base), &snapshot).await
    }

    pub async fn list(&self) -> Vec<(String, PathBuf)> {
        self.sources.read().await.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    pub async fn get(&self, name: &str) -> Option<PathBuf> {
        self.sources.read().await.get(name).cloned()
    }

    /// Registers (or re-registers, to change the path) a source and
    /// persists it — serialized against concurrent registrations/removals,
    /// rolling back the in-memory change if the write fails so it can't
    /// silently drift from what's actually on disk.
    pub async fn register(&self, base: &Path, name: String, path: PathBuf) -> std::io::Result<()> {
        let _guard = self.persist_lock.lock().await;
        let previous = self.sources.write().await.insert(name.clone(), path);
        if let Err(e) = self.save(base).await {
            let mut sources = self.sources.write().await;
            match previous {
                Some(p) => { sources.insert(name, p); }
                None => { sources.remove(&name); }
            }
            return Err(e);
        }
        Ok(())
    }

    /// Unregisters a source and persists it, same serialization/rollback as
    /// `register`. Returns whether a source with this name existed.
    pub async fn unregister(&self, base: &Path, name: &str) -> std::io::Result<bool> {
        let _guard = self.persist_lock.lock().await;
        let removed = self.sources.write().await.remove(name);
        let existed = removed.is_some();
        if let Err(e) = self.save(base).await {
            if let Some(path) = removed {
                self.sources.write().await.insert(name.to_string(), path);
            }
            return Err(e);
        }
        Ok(existed)
    }
}

#[derive(Deserialize)]
pub struct RegisterLogSourceRequest {
    pub name: String,
    pub path: String,
}

#[derive(Serialize)]
pub struct LogSourceSummary {
    pub name: String,
    pub path: String,
    /// Registering a not-yet-existing path is allowed (e.g. the app that
    /// will write it hasn't started yet) — this is how a caller tells the
    /// two cases apart rather than getting a confusing empty tail.
    pub exists: bool,
    pub size_bytes: Option<u64>,
}

async fn summarize(name: &str, path: &Path) -> LogSourceSummary {
    let meta = tokio::fs::metadata(path).await.ok();
    LogSourceSummary {
        name: name.to_string(),
        path: path.display().to_string(),
        exists: meta.is_some(),
        size_bytes: meta.map(|m| m.len()),
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `POST /v1/logs` — setup only. Registers (or re-registers, to change the
/// path) a named log source. Does not require the file to exist yet.
pub async fn register(
    State(state): State<Arc<crate::AppState>>,
    Json(req): Json<RegisterLogSourceRequest>,
) -> ApiResult<LogSourceSummary> {
    if !valid_name(&req.name) {
        return Err(err_response("name must be non-empty and contain only letters, digits, '-', '_' (it appears in the URL path)"));
    }
    let requested = PathBuf::from(&req.path);
    // Canonicalize when possible so the registry always records a real,
    // resolved location rather than a relative path whose meaning would
    // depend on some future process's own working directory. Falls back to
    // the as-given path when the file doesn't exist yet.
    let resolved = tokio::fs::canonicalize(&requested).await.unwrap_or(requested);

    state.log_sources.register(&state.connect_dir, req.name.clone(), resolved.clone()).await.map_err(err_response)?;
    crate::auth::append_audit(&state.connect_dir, &state.audit_lock, "log_source_added", &req.name, "setup").await;

    Ok(Json(summarize(&req.name, &resolved).await))
}

/// `GET /v1/logs` — setup or viewer. Metadata only, never content.
pub async fn list(State(state): State<Arc<crate::AppState>>) -> ApiResult<Vec<LogSourceSummary>> {
    let mut sources = state.log_sources.list().await;
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::with_capacity(sources.len());
    for (name, path) in sources {
        out.push(summarize(&name, &path).await);
    }
    Ok(Json(out))
}

/// `DELETE /v1/logs/:name` — setup only. Unregisters the source; never
/// touches the underlying file.
pub async fn remove(
    State(state): State<Arc<crate::AppState>>,
    AxumPath(name): AxumPath<String>,
) -> ApiResult<serde_json::Value> {
    if !state.log_sources.unregister(&state.connect_dir, &name).await.map_err(err_response)? {
        return Err(source_not_found(&name));
    }
    crate::auth::append_audit(&state.connect_dir, &state.audit_lock, "log_source_removed", &name, "setup").await;
    Ok(Json(json!({ "removed": true, "name": name })))
}

#[derive(Deserialize)]
pub struct TailQuery {
    pub lines: Option<usize>,
}

#[derive(Serialize)]
pub struct TailResponse {
    pub name: String,
    pub lines: Vec<String>,
}

/// `GET /v1/logs/:name/tail?lines=N` — setup or viewer. `lines` defaults to
/// (and is hard-capped at) `--log-tail-max-lines`/`DATUM_LOG_TAIL_MAX_LINES`
/// — operator-configurable, not a compile-time constant, per
/// LOG-TAIL-PLAN.md. Also surfaced via `GET /v1/info` so the dashboard can
/// show it.
pub async fn tail(
    State(state): State<Arc<crate::AppState>>,
    AxumPath(name): AxumPath<String>,
    Query(q): Query<TailQuery>,
) -> ApiResult<TailResponse> {
    let path = state.log_sources.get(&name).await.ok_or_else(|| source_not_found(&name))?;
    let cap = state.log_tail_max_lines.max(1);
    let requested = q.lines.unwrap_or(cap).clamp(1, cap);

    let lines = match tail_file(&path, requested).await {
        Ok(lines) => lines,
        // Not-yet-existing (or momentarily rotated-away) file reads as "no
        // lines yet", not an error — see `register`'s doc comment.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(err_response(format!("failed to read '{}': {e}", path.display()))),
    };
    Ok(Json(TailResponse { name, lines }))
}

/// Reads the last `n` lines of `path` by seeking backward in fixed-size
/// chunks from EOF, rather than loading the whole file — a multi-GB log
/// tailed for 100 lines shouldn't cost a multi-GB read. Tolerates the file
/// shrinking/rotating out from under a concurrent read by returning
/// whatever was read so far instead of erroring, since this is a live log
/// that can legitimately be rotated while being tailed (see
/// LOG-TAIL-PLAN.md's rotation caveat).
async fn tail_file(path: &Path, n: usize) -> std::io::Result<Vec<String>> {
    let mut file = tokio::fs::File::open(path).await?;
    let file_len = file.metadata().await?.len();

    let mut pos = file_len;
    let mut newline_count = 0usize;
    let mut buf: Vec<u8> = Vec::new();
    let mut total_read: u64 = 0;

    // The third condition bounds the worst case for a file with very few
    // (or zero) newlines — e.g. one huge line — which would otherwise never
    // satisfy `newline_count <= n` before reaching byte 0.
    while pos > 0 && newline_count <= n && total_read < MAX_TAIL_READ_BYTES {
        let chunk_len = READ_CHUNK_SIZE.min(pos);
        let chunk_start = pos - chunk_len;
        if file.seek(SeekFrom::Start(chunk_start)).await.is_err() {
            break;
        }
        let mut chunk = vec![0u8; chunk_len as usize];
        if file.read_exact(&mut chunk).await.is_err() {
            break; // file shrank/rotated concurrently — return what we have
        }
        pos = chunk_start;
        total_read += chunk_len;
        newline_count += chunk.iter().filter(|&&b| b == b'\n').count();
        chunk.extend_from_slice(&buf);
        buf = chunk;
    }

    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    // `pos > 0` means we stopped before the true start of the file, so the
    // very first "line" in `buf` is actually a fragment cut off mid-line by
    // our chunk boundary, not a real line — drop it. When `pos == 0` we
    // read all the way back to byte 0, so the first line is real.
    if pos > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    let start = lines.len().saturating_sub(n);
    Ok(lines[start..].iter().map(|s| strip_ansi(s)).collect())
}

/// Strips ANSI CSI escape sequences (`ESC [ ... <final byte>`) from a line.
/// Needed because `tracing_subscriber`'s file layer still emits color codes
/// around span-context fields even with `with_ansi(false)` set (a known
/// quirk, not fully gated by that flag) — and more generally, any
/// registered source could be a colorized log (e.g. a dev server's own
/// output) that would otherwise show up as garbled bracket-notation text in
/// the dashboard's plain-text `<pre>` block.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next(); // consume '['
            for c2 in chars.by_ref() {
                if ('@'..='~').contains(&c2) {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}
