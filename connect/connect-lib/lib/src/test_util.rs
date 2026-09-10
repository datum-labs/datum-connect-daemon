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
