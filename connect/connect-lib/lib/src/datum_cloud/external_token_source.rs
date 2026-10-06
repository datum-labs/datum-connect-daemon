use std::env;
use std::process::Command;

use arc_swap::ArcSwap;
use base64::Engine;
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use super::service_account::{self, ServiceAccount};

/// Errors that can occur when constructing an [`ExternalTokenSource`] from environment.
#[derive(Debug, thiserror::Error)]
pub enum ExternalTokenError {
    #[error("DATUM_CREDENTIALS_HELPER environment variable not set")]
    MissingHelper,
    #[error("DATUM_SESSION not set and no session argument provided")]
    MissingSession,
    #[error("credentials helper exec failed: {0}")]
    HelperExecError(String),
    #[error("invalid JWT token: {0}")]
    InvalidToken(String),
    #[error("failed to parse JWT payload: {0}")]
    JwtParse(#[source] serde_json::Error),
    #[error("service account: {0}")]
    ServiceAccount(String),
}

/// Points the daemon at a Datum service account key file. When set, tokens
/// are minted in-process from the key instead of by a credentials helper.
pub const SA_KEY_FILE_ENV: &str = "DATUM_SA_KEY_FILE";
/// The IdP the service account's assertion is exchanged with.
pub const AUTH_ISSUER_ENV: &str = "DATUM_AUTH_ISSUER";

/// Where a fresh token comes from, for the startup fetch and every refresh.
#[derive(Clone)]
enum TokenFetcher {
    /// `DATUM_CREDENTIALS_HELPER auth get-token --session <session>`.
    Helper { helper: String, session: String },
    /// A JWT-bearer exchange with a service account key. Every call is a new
    /// exchange, so a forced refresh always gets a genuinely new token.
    ServiceAccount(std::sync::Arc<ServiceAccount>),
}

impl TokenFetcher {
    async fn fetch(&self) -> Result<String, ExternalTokenError> {
        match self {
            TokenFetcher::Helper { helper, session } => {
                ExternalTokenSource::exec_helper(helper, session)
            }
            TokenFetcher::ServiceAccount(sa) => sa.mint().await,
        }
    }
}

/// How many consecutive no-op forced refreshes before the credential is treated
/// as rejected-and-unrenewable. Small on purpose: each one is a round trip to a
/// subprocess prompted by a real 401, and once two in a row have changed
/// nothing, a third is not going to either.
const INEFFECTIVE_FORCED_LIMIT: u32 = 3;

/// Manages a bearer token provided from an external source (credentials helper + refresh loop).
///
/// Used in plugin mode. The token is obtained at startup by executing the
/// credentials helper (`DATUM_CREDENTIALS_HELPER auth get-token --session <session>`)
/// and refreshed periodically before JWT expiry or on demand via [`force_refresh()`](Self::force_refresh).
#[derive(Clone)]
pub struct ExternalTokenSource {
    token: std::sync::Arc<ArcSwap<SecretString>>,
    token_tx: std::sync::Arc<watch::Sender<String>>,
    refresh_trigger: std::sync::Arc<watch::Sender<u64>>,
    /// Consecutive forced refreshes that returned the *same* token.
    ///
    /// A forced refresh happens because something observed a 401. If the
    /// helper then hands back a byte-identical token, the credential cannot
    /// be renewed by asking again: the helper caches on expiry, and expiry is
    /// not why the token was rejected (revocation, session invalidation,
    /// server-side rotation, an audience change). Retrying it against an
    /// endpoint that just refused it cannot succeed, so this counter exists
    /// to recognise that state instead of looping on it forever.
    ineffective_forced: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl std::fmt::Debug for ExternalTokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalTokenSource")
            .finish_non_exhaustive()
    }
}

impl ExternalTokenSource {
    /// Creates an `ExternalTokenSource` by executing the credentials helper
    /// at startup to obtain the initial token.
    ///
    /// `session` is the session name to pass to `auth get-token --session <session>`.
    /// If `None`, falls back to `DATUM_SESSION` env var.
    pub fn from_env(session: Option<String>) -> Result<Self, ExternalTokenError> {
        let helper =
            env::var("DATUM_CREDENTIALS_HELPER").map_err(|_| ExternalTokenError::MissingHelper)?;

        let session = match session {
            Some(s) => s,
            None => env::var("DATUM_SESSION").map_err(|_| ExternalTokenError::MissingSession)?,
        };

        let token = Self::exec_helper(&helper, &session)?;

        let exp = parse_jwt_expiry(&token).map_err(|e| {
            ExternalTokenError::InvalidToken(format!("failed to extract expiry: {e}"))
        })?;

        debug!(
            token_len = token.len(),
            exp = ?exp,
            "ExternalTokenSource::from_env — token loaded from helper"
        );

        Ok(Self::with_token(token))
    }

    /// Picks the token source from the environment and starts its refresh
    /// loop. Must be called from within a tokio runtime.
    ///
    /// With `DATUM_SA_KEY_FILE` set, tokens are minted in-process from that
    /// service account key ([`Self::from_service_account_key_file`]).
    /// Otherwise this is exactly what the daemon and CLI always did:
    /// [`Self::from_env`], then a helper refresh loop when both a session and
    /// `DATUM_CREDENTIALS_HELPER` are set.
    pub async fn from_env_with_refresh(session: Option<String>) -> Result<Self, ExternalTokenError> {
        if let Some(path) = env::var_os(SA_KEY_FILE_ENV).filter(|p| !p.is_empty()) {
            let issuer = env::var(AUTH_ISSUER_ENV)
                .ok()
                .filter(|i| !i.is_empty())
                .unwrap_or_else(|| service_account::DEFAULT_ISSUER.to_string());
            return Self::from_service_account_key_file(std::path::Path::new(&path), &issuer).await;
        }
        let source = Self::from_env(session.clone())?;
        if let Some(session) = session
            && let Ok(helper) = env::var("DATUM_CREDENTIALS_HELPER")
        {
            source.start_refresh(helper, session);
        }
        Ok(source)
    }

    /// Mints the first token from a service account key file, then keeps it
    /// fresh with the same refresh loop the helper uses: proactively before
    /// expiry, and immediately on [`force_refresh()`](Self::force_refresh).
    /// Must be called from within a tokio runtime.
    pub async fn from_service_account_key_file(
        path: &std::path::Path,
        issuer: &str,
    ) -> Result<Self, ExternalTokenError> {
        let sa = ServiceAccount::from_file(path, issuer)?;
        info!(
            client_id = sa.client_id(),
            issuer,
            "Datum auth: minting tokens from the service account key at {}",
            path.display()
        );
        let fetcher = TokenFetcher::ServiceAccount(std::sync::Arc::new(sa));
        let token = fetcher.fetch().await?;
        let source = Self::with_token(token);
        source.spawn_refresh(fetcher);
        Ok(source)
    }

    fn with_token(token: String) -> Self {
        let (token_tx, _) = watch::channel(token.clone());
        let (refresh_tx, _) = watch::channel(0u64);
        Self {
            token: std::sync::Arc::new(ArcSwap::from_pointee(SecretString::new(token.into()))),
            token_tx: std::sync::Arc::new(token_tx),
            refresh_trigger: std::sync::Arc::new(refresh_tx),
            ineffective_forced: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        }
    }

    /// Returns the current token as a plain `String`.
    pub fn token(&self) -> String {
        self.token.load_full().expose_secret().to_string()
    }

    /// How many consecutive forced refreshes have returned an unchanged token.
    ///
    /// Non-zero means something is rejecting the current credential while the
    /// helper keeps handing back the same one. Callers should treat a sustained
    /// non-zero value as "credential rejected and not renewable by retrying",
    /// which is operator-actionable, rather than as a transient blip.
    pub fn ineffective_forced_refreshes(&self) -> u32 {
        self.ineffective_forced.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Returns a watch channel subscriber for token updates.
    pub fn watch(&self) -> watch::Receiver<String> {
        self.token_tx.subscribe()
    }

    /// Atomically swaps the token and notifies watch subscribers.
    pub fn swap_token(&self, new_token: String) {
        debug!(
            new_token_len = new_token.len(),
            "ExternalTokenSource::swap_token"
        );
        self.token.store(std::sync::Arc::new(SecretString::new(
            new_token.clone().into(),
        )));
        let _ = self.token_tx.send(new_token);
    }

    /// Start the background refresh loop. Must be called from within a tokio runtime.
    ///
    /// The loop periodically re-executes the credentials helper before the current
    /// token expires, calls [`swap_token()`](Self::swap_token) with the result,
    /// and responds to [`force_refresh()`](Self::force_refresh) signals.
    pub fn start_refresh(&self, helper: String, session: String) {
        self.spawn_refresh(TokenFetcher::Helper { helper, session });
    }

    fn spawn_refresh(&self, fetcher: TokenFetcher) {
        let this = self.clone();
        let mut refresh_rx = self.refresh_trigger.subscribe();
        let initial_exp = match parse_jwt_expiry(&self.token()) {
            Ok(exp) => exp,
            Err(_) => None,
        };
        tokio::spawn(async move {
            this.run_refresh_loop(fetcher, &mut refresh_rx, initial_exp)
                .await;
        });
    }

    /// Triggers an immediate token refresh.
    ///
    /// Call this when a 401 response is observed from the API.
    /// The refresh loop wakes up early, re-executes the credentials helper,
    /// and calls [`swap_token()`](Self::swap_token) with the result.
    pub fn force_refresh(&self) {
        let current = *self.refresh_trigger.borrow();
        info!(
            trigger_count = current.wrapping_add(1),
            "token refresh: forced refresh requested (401 or stale auth observed)"
        );
        let _ = self.refresh_trigger.send(current.wrapping_add(1));
    }

    fn exec_helper(helper: &str, session: &str) -> Result<String, ExternalTokenError> {
        let output = Command::new(helper)
            .args(["auth", "get-token", "--session", session])
            .output()
            .map_err(|e| ExternalTokenError::HelperExecError(format!("exec failed: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ExternalTokenError::HelperExecError(format!(
                "exit code {}: {}",
                output.status,
                stderr.trim()
            )));
        }
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if token.is_empty() {
            return Err(ExternalTokenError::HelperExecError(
                "empty token returned".into(),
            ));
        }
        Ok(token)
    }

    async fn run_refresh_loop(
        self,
        fetcher: TokenFetcher,
        refresh_rx: &mut watch::Receiver<u64>,
        initial_exp: Option<u64>,
    ) {
        // Compute the next refresh time: 60s before JWT expiry, or 1h from now if no expiry.
        let mut next_refresh: std::time::SystemTime = initial_exp
            .and_then(|exp| {
                std::time::UNIX_EPOCH
                    .checked_add(std::time::Duration::from_secs(exp.saturating_sub(60)))
            })
            .unwrap_or_else(|| {
                std::time::SystemTime::now() + std::time::Duration::from_secs(3600)
            });

        if let Some(exp) = initial_exp {
            debug!(
                exp = exp,
                next_refresh_in_secs = next_refresh
                    .duration_since(std::time::SystemTime::now())
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                "token refresh loop started; proactive refresh scheduled 60s before JWT expiry"
            );
        } else {
            debug!(
                "token refresh loop started; no JWT expiry claim, defaulting to 1h refresh interval"
            );
        }

        let mut backoff = std::time::Duration::from_secs(5);
        const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);

        loop {
            let now = std::time::SystemTime::now();
            let wait = if next_refresh > now {
                next_refresh
                    .duration_since(now)
                    .unwrap_or(std::time::Duration::ZERO)
            } else {
                std::time::Duration::ZERO
            };

            // Wait either for the timer or a force_refresh signal
            let forced = tokio::select! {
                _ = tokio::time::sleep(wait) => {
                    debug!("token refresh: proactive timer fired");
                    false
                }
                _ = refresh_rx.changed() => {
                    info!("token refresh: forced refresh signalled (401 or stale auth)");
                    true
                }
            };

            // Once the credential is known to be unrenewable, stop re-running
            // the helper for every 401. It would return the same token it
            // already returned, which is what produced the endless retry loop
            // this guard exists to break. The proactive timer is deliberately
            // still honoured: the operator may log in again at any point, and
            // that path is how the daemon notices.
            if forced && self.ineffective_forced_refreshes() >= INEFFECTIVE_FORCED_LIMIT {
                debug!(
                    "token refresh: ignoring forced refresh — credential is rejected and                      unrenewable; waiting for the proactive timer or a new login"
                );
                continue;
            }

            // Run the helper, or exchange the key, for a fresh token
            match fetcher.fetch().await {
                Ok(new_token) => {
                    let previous = self.token();
                    let prev_exp = parse_jwt_expiry(&previous).ok().flatten();
                    let new_exp = parse_jwt_expiry(&new_token).ok().flatten();

                    // A forced refresh that changes nothing is the signature of
                    // a credential rejected for a reason other than expiry: the
                    // helper sees a token with life left and returns its cached
                    // copy, so asking again is futile. Count those; any genuinely
                    // new token clears the count.
                    if new_token == previous {
                        if forced {
                            let seen = self
                                .ineffective_forced
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                + 1;
                            if seen == INEFFECTIVE_FORCED_LIMIT {
                                error!(
                                    consecutive = seen,
                                    "token refresh: the credentials helper keeps returning the                                      same token after a 401 — the credential is rejected and                                      cannot be renewed by retrying. Tunnels will stay down until                                      the operator logs in again."
                                );
                            } else {
                                warn!(
                                    consecutive = seen,
                                    "token refresh: forced refresh returned an unchanged token"
                                );
                            }
                        }
                    } else {
                        self.ineffective_forced
                            .store(0, std::sync::atomic::Ordering::Relaxed);
                    }

                    self.swap_token(new_token.clone());
                    backoff = std::time::Duration::from_secs(5); // Reset backoff

                    info!(
                        forced,
                        new_exp = ?new_exp,
                        prev_exp = ?prev_exp,
                        "token refresh: succeeded; token swapped and watchers notified"
                    );

                    // Parse new expiry for next refresh
                    next_refresh = match new_exp {
                        Some(exp) => std::time::UNIX_EPOCH
                            + std::time::Duration::from_secs(exp.saturating_sub(60)),
                        None => {
                            std::time::SystemTime::now() + std::time::Duration::from_secs(3600)
                        }
                    };
                }
                Err(e) => {
                    warn!(forced, "token refresh failed: {e}; retrying in {:?}", backoff);
                    // Retry with backoff
                    next_refresh = std::time::SystemTime::now() + backoff;
                    backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
                }
            }
        }
    }
}

/// Parse the `exp` (expiry) claim from the middle segment of a JWT.
///
/// Returns `None` if the claim is missing (caller may default to 1 h).
fn parse_jwt_expiry(token: &str) -> Result<Option<u64>, JwtParseError> {
    let parts: Vec<&str> = token.splitn(3, '.').collect();
    if parts.len() < 2 {
        return Err(JwtParseError::InvalidToken(
            "JWT must have at least 2 segments (header.payload[.signature])".into(),
        ));
    }

    let payload_b64 = parts[1];

    // Base64url decode: replace URL-safe chars with standard base64 chars, then pad.
    let mut standard_b64 = payload_b64.replace('-', "+").replace('_', "/");
    let pad = 4 - standard_b64.len() % 4;
    if pad != 4 {
        standard_b64.extend((0..pad).map(|_| '='));
    }

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&standard_b64)
        .map_err(|e| JwtParseError::InvalidBase64(e.to_string()))?;

    let payload_str =
        String::from_utf8(decoded).map_err(|e| JwtParseError::InvalidUtf8(e.to_string()))?;

    let value: serde_json::Value =
        serde_json::from_str(&payload_str).map_err(JwtParseError::Json)?;

    Ok(value.get("exp").and_then(|v| v.as_u64()))
}

#[derive(Debug, thiserror::Error)]
enum JwtParseError {
    #[error("invalid JWT format: {0}")]
    InvalidToken(String),
    #[error("invalid base64url encoding: {0}")]
    InvalidBase64(String),
    #[error("invalid UTF-8 in JWT payload: {0}")]
    InvalidUtf8(String),
    #[error("failed to parse JWT payload as JSON: {0}")]
    Json(#[source] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{make_jwt_with_exp, setup_plugin_env, TempDir};

    #[test]
    fn parse_jwt_expiry_extracts_exp() {
        let token = make_jwt_with_exp(1700000000);
        let exp = parse_jwt_expiry(&token).unwrap().unwrap();
        assert_eq!(exp, 1700000000);
    }

    #[test]
    fn parse_jwt_expiry_returns_none_when_missing() {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::json!({"sub":"test-user"}).to_string().as_bytes(),
        );
        let token = format!("{header}.{payload}.sig");
        let exp = parse_jwt_expiry(&token).unwrap();
        assert!(exp.is_none());
    }

    #[test]
    fn parse_jwt_expiry_rejects_too_short() {
        let result = parse_jwt_expiry("not-a-jwt");
        assert!(result.is_err());
    }

    #[test]
    fn parse_jwt_expiry_rejects_invalid_base64() {
        let token = format!("header.!!!.sig");
        let result = parse_jwt_expiry(&token);
        assert!(result.is_err());
    }

    #[test]
    fn parse_jwt_expiry_rejects_invalid_json() {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"not-json");
        let token = format!("{header}.{payload}.sig");
        let result = parse_jwt_expiry(&token);
        assert!(result.is_err());
    }

    #[test]
    fn parse_jwt_expiry_handles_url_safe_chars() {
        let payload_json = serde_json::json!({"exp": 9999999999u64, "sub": "test"});
        let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            payload_json.to_string().as_bytes(),
        );
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
        let token = format!("{header}.{payload_b64}.sig");
        let exp = parse_jwt_expiry(&token).unwrap().unwrap();
        assert_eq!(exp, 9999999999);
    }

    #[test]
    fn from_env_requires_helper() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("DATUM_CREDENTIALS_HELPER");
            std::env::set_var("DATUM_SESSION", "test-session");
        }
        let result = ExternalTokenSource::from_env(Some("test-session".to_string()));
        assert!(matches!(result, Err(ExternalTokenError::MissingHelper)));
    }

    #[test]
    fn from_env_requires_session() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("DATUM_CREDENTIALS_HELPER", "/bin/echo");
            std::env::remove_var("DATUM_SESSION");
        }
        let result = ExternalTokenSource::from_env(None);
        assert!(matches!(result, Err(ExternalTokenError::MissingSession)));
    }

    #[test]
    fn from_env_succeeds_with_fake_helper() {
        let (_dir, source) = setup_plugin_env();
        assert!(source.token().starts_with("eyJ"));
    }

    #[test]
    fn from_env_requires_datum_credentials_helper() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("DATUM_CREDENTIALS_HELPER");
            std::env::set_var("DATUM_SESSION", "test-session");
        }
        let result = ExternalTokenSource::from_env(None);
        assert!(matches!(result, Err(ExternalTokenError::MissingHelper)));
    }

    /// The failure this guards against, reproduced in miniature: a token is
    /// rejected while still well inside its `exp`, so the helper — which
    /// caches on expiry — hands back the identical token every time it is
    /// asked. Before this counter existed that produced an unbounded retry
    /// loop (observed at 1089 consecutive attempts in the field) with nothing
    /// escalating and nothing surfaced.
    ///
    /// Built by hand rather than via `setup_plugin_env()` so it does not
    /// depend on the shell-script fake helper, which cannot execute on
    /// Windows.
    #[test]
    fn unchanged_forced_refreshes_latch_then_clear_on_recovery() {
        let initial = make_jwt_with_exp(9999999999);
        let (token_tx, _) = watch::channel(initial.clone());
        let (refresh_tx, _) = watch::channel(0u64);
        let source = ExternalTokenSource {
            token: std::sync::Arc::new(ArcSwap::from_pointee(SecretString::new(
                initial.clone().into(),
            ))),
            token_tx: std::sync::Arc::new(token_tx),
            refresh_trigger: std::sync::Arc::new(refresh_tx),
            ineffective_forced: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        };

        assert_eq!(source.ineffective_forced_refreshes(), 0, "starts clean");

        // Each forced refresh comes back with the token it already had.
        for expected in 1..=INEFFECTIVE_FORCED_LIMIT {
            source
                .ineffective_forced
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            assert_eq!(source.ineffective_forced_refreshes(), expected);
        }
        assert!(
            source.ineffective_forced_refreshes() >= INEFFECTIVE_FORCED_LIMIT,
            "at the limit the loop stops re-running the helper for every 401"
        );

        // A genuinely different token means the credential recovered — the
        // operator logged in again — and the latch must clear, or the daemon
        // would stay stuck in the failed state after auth came back.
        let fresh = make_jwt_with_exp(8888888888);
        assert_ne!(fresh, initial);
        source
            .ineffective_forced
            .store(0, std::sync::atomic::Ordering::Relaxed);
        source.swap_token(fresh.clone());

        assert_eq!(source.token(), fresh);
        assert_eq!(source.ineffective_forced_refreshes(), 0, "recovery clears the latch");
    }

    #[test]
    fn swap_token_updates_and_notifies_watch() {
        let (_dir, source) = setup_plugin_env();

        let rx = source.watch();
        let new_token = make_jwt_with_exp(8888888888);
        source.swap_token(new_token.clone());

        assert_eq!(source.token(), new_token);
        assert_eq!(*rx.borrow(), new_token);
    }

    #[test]
    fn swap_token_multiple_times() {
        let (_dir, source) = setup_plugin_env();

        for i in 1..=5 {
            let new_token = make_jwt_with_exp(7777777000 + i);
            source.swap_token(new_token.clone());
            assert_eq!(source.token(), new_token);
        }
    }

    #[test]
    fn watch_receiver_initial_value() {
        let (_dir, source) = setup_plugin_env();
        let rx = source.watch();
        assert_eq!(*rx.borrow(), source.token());
    }

    #[test]
    fn clone_preserves_state() {
        let (_dir, source) = setup_plugin_env();
        let cloned = source.clone();

        assert_eq!(source.token(), cloned.token());

        let new_token = make_jwt_with_exp(6666666000);
        source.swap_token(new_token.clone());
        assert_eq!(cloned.token(), new_token);
    }

    #[test]
    fn force_refresh_triggers_signal() {
        let (_dir, source) = setup_plugin_env();
        let rx = source.refresh_trigger.subscribe();
        // Initial value is 0
        assert_eq!(*rx.borrow(), 0);

        source.force_refresh();
        // After force_refresh, the value should have incremented
        // Since send happens synchronously, borrow() already shows the new value
        assert_eq!(*rx.borrow(), 1);

        source.force_refresh();
        assert_eq!(*rx.borrow(), 2);
    }

    /// Verifies the end-to-end refresh path: when `force_refresh()` is
    /// signalled (e.g. after a 401), the background loop re-executes the
    /// credentials helper and swaps in the new token, notifying watchers.
    ///
    /// This guards against the "stale auth" regression where the heartbeat
    /// observed a 401 but never actually triggered a refresh — the token
    /// stayed dead until the proactive timer eventually fired.
    #[tokio::test]
    async fn force_refresh_swaps_token_via_loop() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let dir = TempDir::new("ets-loop");

        // Helper that emits a distinct JWT on every invocation by reading
        // and incrementing a counter file. This lets the test observe that
        // the loop actually re-executed the helper (not just that the signal
        // was sent).
        let counter_path = dir.path().join("counter");
        std::fs::write(&counter_path, "0").expect("should write counter");
        let helper_path = dir.path().join("counter-helper.sh");
        let counter_str = counter_path.to_string_lossy().replace('\'', "'\\''");
        let script = format!(
            "#!/bin/sh\n\
             n=$(cat '{counter_str}')\n\
             n=$((n + 1))\n\
             echo \"$n\" > '{counter_str}'\n\
             exp=$((1700000000 + n))\n\
             header=$(printf '{{\"alg\":\"HS256\",\"typ\":\"JWT\"}}' | base64 | tr -d '=' | tr '/+' '_-')\n\
             payload=$(printf '{{\"exp\":%d,\"sub\":\"rotating\"}}' \"$exp\" | base64 | tr -d '=' | tr '/+' '_-')\n\
             printf '%s.%s.rotated\\n' \"$header\" \"$payload\"\n",
        );
        std::fs::write(&helper_path, script).expect("should write helper script");
        #[cfg(unix)]
        std::fs::set_permissions(
            &helper_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .expect("should set executable permission");

        unsafe {
            std::env::set_var("DATUM_CREDENTIALS_HELPER", helper_path.to_string_lossy().as_ref());
            std::env::set_var("DATUM_SESSION", "test-session");
        }

        // Use a token with a far-future expiry so the proactive timer does
        // not fire during the test — only the forced refresh should swap.
        let initial = make_jwt_with_exp(9999999999);
        std::fs::write(&counter_path, "0").expect("should reset counter");
        // Build the source by hand so from_env() doesn't consume the first
        // helper invocation (we want the *loop* to be the one rotating).
        let (token_tx, _) = watch::channel(initial.clone());
        let (refresh_tx, _) = watch::channel(0u64);
        let source = ExternalTokenSource {
            token: std::sync::Arc::new(ArcSwap::from_pointee(SecretString::new(
                initial.clone().into(),
            ))),
            token_tx: std::sync::Arc::new(token_tx),
            refresh_trigger: std::sync::Arc::new(refresh_tx),
            ineffective_forced: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
        };

        let rx = source.watch();
        assert_eq!(*rx.borrow(), initial, "watch initial value");

        source.start_refresh(
            helper_path.to_string_lossy().to_string(),
            "test-session".to_string(),
        );

        // Nothing should have rotated yet (proactive timer is far in the
        // future). Give the loop a moment to prove a negative.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(source.token(), initial, "no proactive refresh expected yet");

        // Force a refresh (as the heartbeat does on a 401) and wait for the
        // loop to re-exec the helper and swap the token.
        source.force_refresh();
        for _ in 0..40 {
            if source.token() != initial {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let new_token = source.token();
        assert_ne!(
            new_token, initial,
            "force_refresh must have rotated the token"
        );
        assert!(
            new_token.ends_with(".rotated"),
            "rotated token should come from the counter helper: {new_token}"
        );
        assert_eq!(*rx.borrow(), new_token, "watchers notified of new token");
    }

    // --- Service account (DATUM_SA_KEY_FILE) -----------------------------

    use crate::datum_cloud::service_account::tests::{
        pkcs1_key_json, test_key, verify_assertion,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A stand-in for the IdP's token endpoint, on loopback. Every POST gets
    /// a different JWT (far-future `exp`, so the proactive timer stays out
    /// of the way), and each request's form body is recorded.
    async fn fake_token_endpoint() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = bodies.clone();
        tokio::spawn(async move {
            let mut n = 0u64;
            while let Ok((mut sock, _)) = listener.accept().await {
                n += 1;
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let read = sock.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..read]);
                    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + len {
                        let body = String::from_utf8_lossy(&buf[end + 4..end + 4 + len]);
                        seen.lock().unwrap().push(body.into_owned());
                        break;
                    }
                }
                let body = serde_json::json!({
                    "access_token": make_jwt_with_exp(9_999_000_000 + n),
                    "token_type": "Bearer",
                    "expires_in": 43199,
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (issuer, bodies)
    }

    fn form(body: &str) -> std::collections::HashMap<String, String> {
        url::form_urlencoded::parse(body.as_bytes()).into_owned().collect()
    }

    /// The issue #12 lesson, for the native path: a forced refresh (a 401)
    /// must come back with a genuinely new token from a new exchange, never
    /// a cached copy of the one that was just refused.
    #[tokio::test]
    async fn service_account_mints_a_new_token_on_every_forced_refresh() {
        let (issuer, bodies) = fake_token_endpoint().await;
        let key = test_key();
        let dir = TempDir::new("sa-key");
        let key_path = dir.path().join("service-account.json");
        std::fs::write(&key_path, pkcs1_key_json(&key)).unwrap();

        let source = ExternalTokenSource::from_service_account_key_file(&key_path, &issuer)
            .await
            .expect("first token minted at startup");
        let first = source.token();
        assert!(first.starts_with("eyJ"));

        {
            let bodies = bodies.lock().unwrap();
            assert_eq!(bodies.len(), 1, "one exchange at startup");
            let sent = form(&bodies[0]);
            assert_eq!(sent["grant_type"], "urn:ietf:params:oauth:grant-type:jwt-bearer");
            assert_eq!(
                sent["scope"],
                "openid profile urn:zitadel:iam:org:project:id:zitadel:aud"
            );
            let (_, claims) = verify_assertion(&sent["assertion"], &key.public_der);
            assert_eq!(claims["aud"], issuer);
        }

        let rx = source.watch();
        for round in 1..=2 {
            let before = source.token();
            source.force_refresh();
            for _ in 0..100 {
                if source.token() != before {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            assert_ne!(source.token(), before, "round {round}: forced refresh must rotate");
            assert_eq!(*rx.borrow(), source.token(), "watchers notified");
            assert_eq!(bodies.lock().unwrap().len(), 1 + round, "one new exchange per refresh");
        }
        assert_eq!(source.ineffective_forced_refreshes(), 0);
    }

    #[tokio::test]
    async fn service_account_key_problems_fail_before_any_request() {
        let dir = TempDir::new("sa-bad-key");
        let missing = dir.path().join("nope.json");
        let err = ExternalTokenSource::from_service_account_key_file(&missing, "http://127.0.0.1:9")
            .await
            .unwrap_err();
        assert!(matches!(err, ExternalTokenError::ServiceAccount(_)), "{err}");

        let not_a_key = dir.path().join("token.json");
        std::fs::write(&not_a_key, "\"eyJhbGciOi.personal.token\"").unwrap();
        let err = ExternalTokenSource::from_service_account_key_file(&not_a_key, "http://127.0.0.1:9")
            .await
            .unwrap_err();
        assert!(matches!(err, ExternalTokenError::ServiceAccount(_)), "{err}");
    }

    /// Tolerates a lock poisoned by an unrelated failing test, which on
    /// Windows the shell-script helper tests reliably are.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[tokio::test]
    async fn key_file_env_selects_the_native_source() {
        let _lock = env_lock();
        let (issuer, bodies) = fake_token_endpoint().await;
        let dir = TempDir::new("sa-env");
        let key_path = dir.path().join("service-account.json");
        std::fs::write(&key_path, pkcs1_key_json(&test_key())).unwrap();
        unsafe {
            std::env::set_var(SA_KEY_FILE_ENV, &key_path);
            std::env::set_var(AUTH_ISSUER_ENV, &issuer);
            // Not needed, and would be ignored if it were set.
            std::env::remove_var("DATUM_CREDENTIALS_HELPER");
        }
        let result = ExternalTokenSource::from_env_with_refresh(Some("s".into())).await;
        unsafe {
            std::env::remove_var(SA_KEY_FILE_ENV);
            std::env::remove_var(AUTH_ISSUER_ENV);
        }
        assert!(result.expect("native source").token().starts_with("eyJ"));
        assert_eq!(bodies.lock().unwrap().len(), 1);
    }

    /// Without the key file variable (or with it empty) selection is what it
    /// always was: the credentials helper, which here is missing.
    #[tokio::test]
    async fn without_key_file_env_the_helper_path_is_unchanged() {
        let _lock = env_lock();
        for key_file in [None, Some("")] {
            unsafe {
                match key_file {
                    Some(v) => std::env::set_var(SA_KEY_FILE_ENV, v),
                    None => std::env::remove_var(SA_KEY_FILE_ENV),
                }
                std::env::remove_var("DATUM_CREDENTIALS_HELPER");
                std::env::set_var("DATUM_SESSION", "test-session");
            }
            let result = ExternalTokenSource::from_env_with_refresh(None).await;
            assert!(
                matches!(result, Err(ExternalTokenError::MissingHelper)),
                "{key_file:?}: {:?}",
                result.err()
            );
        }
        unsafe { std::env::remove_var(SA_KEY_FILE_ENV) };
    }
}
