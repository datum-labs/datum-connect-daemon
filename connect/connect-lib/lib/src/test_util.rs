//! Shared test utilities for connect-lib test modules.
//!
//! This module consolidates duplicated helper functions that were previously
//! defined inline in multiple test modules (`project_control_plane.rs`,
//! `datum_cloud/mod.rs`, `external_token_source.rs`, `heartbeat.rs`).

use crate::ExternalTokenSource;
use base64::Engine;
use kube::core::ErrorResponse;

/// A temporary directory that cleans up on drop.
///
/// The `prefix` parameter is used to create distinct temp directory names
/// to avoid collisions when multiple tests run concurrently.
pub struct TempDir {
    path: std::path::PathBuf,
}

impl TempDir {
    /// Create a new temporary directory with the given prefix.
    pub fn new(prefix: &str) -> Self {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("connect-test-{prefix}-{ts}"));
        std::fs::create_dir_all(&path).expect("should create temp dir");
        TempDir { path }
    }

    /// Returns the path to the temporary directory.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Helper: create a JWT-like string with a given `exp` claim.
///
/// The `sub` claim is set to `"test-user"` and the signature is `"fake_sig"`.
pub fn make_jwt_with_exp(exp: u64) -> String {
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::json!({"alg":"HS256","typ":"JWT"}).to_string().as_bytes(),
    );
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::json!({"exp": exp, "sub":"test-user"}).to_string().as_bytes(),
    );
    format!("{header}.{payload}.fake_sig")
}

/// Create a temporary helper script that outputs a fake JWT, set env vars,
/// and return a configured [`ExternalTokenSource`].
///
/// The returned `TempDir` keeps the script alive for the test scope.
///
/// # Panics
///
/// Panics if the temp directory cannot be created, the helper script cannot
/// be written, or the [`ExternalTokenSource`] cannot be constructed.
pub fn setup_plugin_env() -> (TempDir, ExternalTokenSource) {
    let _lock = crate::ENV_LOCK.lock().unwrap();
    let dir = TempDir::new("plugin");
    let helper_path = dir.path().join("fake-helper.sh");
    let jwt = make_jwt_with_exp(9999999999);
    std::fs::write(&helper_path, format!("#!/bin/sh\necho '{}'\n", jwt))
        .expect("should write helper script");
    #[cfg(unix)]
    std::fs::set_permissions(
        &helper_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .expect("should set executable permission");
    let helper_str = helper_path.to_string_lossy().to_string();

    unsafe {
        std::env::set_var("DATUM_CREDENTIALS_HELPER", &helper_str);
        std::env::set_var("DATUM_SESSION", "test-session");
    }

    let source =
        ExternalTokenSource::from_env(Some("test-session".to_string())).expect("should create token source");
    (dir, source)
}

/// Create a [`kube::Error::Api`] with the given HTTP status code and reason.
///
/// The `message` field is set to `"test"` — this is suitable for tests that
/// only check the error code and reason (e.g. `classify_lease_error`).
/// For tests that inspect the message content, use the module-local
/// `api_error` in `tunnels.rs` instead.
pub fn api_error(code: u16, reason: &str) -> kube::Error {
    kube::Error::Api(ErrorResponse {
        status: "Failure".into(),
        message: "test".into(),
        reason: reason.into(),
        code,
    })
}

/// A Kubernetes API server on loopback, just enough of one for the delete
/// paths: objects are stored by their item path
/// (`/apis/<group>/<version>/namespaces/<ns>/<plural>/<name>`), GET answers
/// an item or lists a collection, DELETE removes one, and every request is
/// recorded. Field selectors are ignored, so a filtered list returns the
/// whole collection: callers must not rely on the filter alone.
#[derive(Default)]
pub struct FakeKube {
    pub objects: std::collections::BTreeMap<String, serde_json::Value>,
    /// `(method, path)` of every request, query string dropped.
    pub requests: Vec<(String, String)>,
    /// Paths answered with this status instead (any method).
    pub fail: std::collections::HashMap<String, u16>,
}

impl FakeKube {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `object` at its item path, from its own apiVersion, kind's
    /// plural and name. `plural` is given because the fake has no discovery.
    pub fn put(&mut self, plural: &str, object: serde_json::Value) {
        let api_version = object["apiVersion"].as_str().expect("apiVersion");
        let name = object["metadata"]["name"].as_str().expect("metadata.name");
        let path = format!("/apis/{api_version}/namespaces/default/{plural}/{name}");
        self.objects.insert(path, object);
    }

    pub fn has(&self, plural: &str, name: &str) -> bool {
        self.objects.keys().any(|k| k.ends_with(&format!("/{plural}/{name}")))
    }

    pub fn deleted(&self) -> Vec<String> {
        self.requests
            .iter()
            .filter(|(m, _)| m == "DELETE")
            .map(|(_, p)| p.rsplit('/').next().unwrap_or_default().to_string())
            .collect()
    }

    fn handle(&mut self, method: &str, path: &str) -> (u16, serde_json::Value) {
        use serde_json::json;
        self.requests.push((method.to_string(), path.to_string()));
        if let Some(code) = self.fail.get(path) {
            return (*code, status_body(*code, "Injected", "injected failure"));
        }
        // apis/<g>/<v>/namespaces/<ns>/<plural>[/<name>]
        let segments = path.trim_matches('/').split('/').count();
        let is_item = segments == 7;
        match method {
            "GET" if is_item => match self.objects.get(path) {
                Some(o) => (200, o.clone()),
                None => (404, status_body(404, "NotFound", "not found")),
            },
            "GET" => {
                let prefix = format!("{path}/");
                let items: Vec<serde_json::Value> = self
                    .objects
                    .iter()
                    .filter(|(k, _)| k.starts_with(&prefix) && !k[prefix.len()..].contains('/'))
                    .map(|(_, v)| v.clone())
                    .collect();
                (200, json!({"apiVersion": "v1", "kind": "List", "metadata": {}, "items": items}))
            }
            "DELETE" => match self.objects.remove(path) {
                Some(o) => (200, o),
                None => (404, status_body(404, "NotFound", "not found")),
            },
            _ => (405, status_body(405, "MethodNotAllowed", "not in the fake")),
        }
    }
}

fn status_body(code: u16, reason: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "v1", "kind": "Status", "metadata": {},
        "status": "Failure", "message": message, "reason": reason, "code": code,
    })
}

/// Serves `fake` on loopback and returns a client for it.
pub async fn serve_fake_kube(
    fake: FakeKube,
) -> (kube::Client, std::sync::Arc<std::sync::Mutex<FakeKube>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Whichever test gets here first installs it; the rest find it there.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fake = std::sync::Arc::new(std::sync::Mutex::new(fake));
    let shared = fake.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let fake = shared.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let head = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                    let len = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_string()))
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + len {
                        break head;
                    }
                };
                let mut first = head.lines().next().unwrap_or_default().split(' ');
                let method = first.next().unwrap_or_default().to_string();
                let target = first.next().unwrap_or_default();
                let path = target.split('?').next().unwrap_or_default().to_string();
                let (status, body) = fake.lock().unwrap().handle(&method, &path);
                let body = body.to_string();
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    let config = kube::Config::new(format!("http://{addr}").parse().unwrap());
    (kube::Client::try_from(config).unwrap(), fake)
}
