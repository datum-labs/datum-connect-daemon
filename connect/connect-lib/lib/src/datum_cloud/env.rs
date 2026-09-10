use std::{borrow::Cow, env};

use serde::{Deserialize, Serialize};

const STAGING_API_URL: &str = "https://api.staging.env.datum.net";
const PROD_API_URL: &str = "https://api.datum.net";
const STAGING_WEB_URL: &str = "https://cloud.staging.env.datum.net";
const PROD_WEB_URL: &str = "https://cloud.datum.net";

/// Environment for Datum API. Use [`ApiEnv::default()`] to respect `DATUM_API_HOST` first,
/// then `DATUM_API_ENV`. Use [`ApiEnv::from_env_with_host_override()`] for explicit host override.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ApiEnv {
    Staging,
    Production,
    /// Custom API host (plugin mode override).
    Custom { api_url: String },
}

impl ApiEnv {
    /// Uses `DATUM_API_ENV`: `staging` → Staging, anything else (including unset) → Production.
    fn from_env() -> Self {
        match env::var("DATUM_API_ENV").as_deref() {
            Ok("staging") => ApiEnv::Staging,
            _ => ApiEnv::Production,
        }
    }

    /// Checks `DATUM_API_HOST` first, falls back to `from_env()`.
    ///
    /// In plugin mode, the Go plugin sets `DATUM_API_HOST` to point at a
    /// specific API host. This method honors that override.
    ///
    /// If the host value does not have a scheme (`http://` or `https://`),
    /// `https://` is prepended automatically so that downstream URL
    /// construction always produces valid absolute URLs.
    ///
    /// An empty `DATUM_API_HOST` (set to `""`) is treated as unset — the
    /// function falls through to `from_env()`.
    pub fn from_env_with_host_override() -> Self {
        if let Ok(host) = env::var("DATUM_API_HOST") {
            if !host.is_empty() {
                let api_url = if host.starts_with("http://") || host.starts_with("https://") {
                    host
                } else {
                    format!("https://{}", host)
                };
                return ApiEnv::Custom { api_url };
            }
        }
        Self::from_env()
    }

    pub fn api_url(&self) -> Cow<'static, str> {
        match self {
            ApiEnv::Staging => Cow::Borrowed(STAGING_API_URL),
            ApiEnv::Production => Cow::Borrowed(PROD_API_URL),
            ApiEnv::Custom { api_url } => Cow::Owned(api_url.clone()),
        }
    }

    pub fn web_url(&self) -> Cow<'static, str> {
        match self {
            ApiEnv::Staging => Cow::Borrowed(STAGING_WEB_URL),
            ApiEnv::Production => Cow::Borrowed(PROD_WEB_URL),
            ApiEnv::Custom { api_url } => Cow::Owned(
                api_url
                    .replace("api.", "app.")
                    .replace("//api.", "//app."),
            ),
        }
    }
}

impl Default for ApiEnv {
    fn default() -> Self {
        Self::from_env_with_host_override()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cleanup_env() {
        unsafe {
            std::env::remove_var("DATUM_API_ENV");
            std::env::remove_var("DATUM_API_HOST");
        }
    }

    #[test]
    fn default_respects_datum_api_env_when_no_host() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        cleanup_env();
        assert!(matches!(ApiEnv::default(), ApiEnv::Production));
        unsafe { std::env::set_var("DATUM_API_ENV", "staging"); }
        assert!(matches!(ApiEnv::default(), ApiEnv::Staging));
    }

    #[test]
    fn from_env_with_host_override_uses_datum_api_host() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        cleanup_env();
        unsafe { std::env::set_var("DATUM_API_HOST", "https://custom.example.com"); }
        let env = ApiEnv::from_env_with_host_override();
        assert!(matches!(&env, ApiEnv::Custom { api_url } if api_url == "https://custom.example.com"));
    }

    #[test]
    fn api_url_custom_returns_host() {
        let env = ApiEnv::Custom { api_url: "https://my.api.com".to_string() };
        assert_eq!(env.api_url(), "https://my.api.com");
    }

    #[test]
    fn from_env_with_host_override_adds_https_scheme_when_missing() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        cleanup_env();
        unsafe { std::env::set_var("DATUM_API_HOST", "api.datum.net"); }
        let env = ApiEnv::from_env_with_host_override();
        assert!(matches!(&env, ApiEnv::Custom { api_url } if api_url == "https://api.datum.net"));
    }

    #[test]
    fn from_env_with_host_override_preserves_existing_scheme() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        cleanup_env();
        unsafe { std::env::set_var("DATUM_API_HOST", "http://internal.api.datum.net"); }
        let env = ApiEnv::from_env_with_host_override();
        assert!(matches!(&env, ApiEnv::Custom { api_url } if api_url == "http://internal.api.datum.net"));
    }

    #[test]
    fn from_env_with_host_override_empty_falls_back_to_production() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        cleanup_env();
        unsafe { std::env::set_var("DATUM_API_HOST", ""); }
        let env = ApiEnv::from_env_with_host_override();
        assert!(matches!(env, ApiEnv::Production));
    }

    #[test]
    fn api_url_staging_returns_staging_url() {
        assert_eq!(ApiEnv::Staging.api_url(), STAGING_API_URL);
    }

    #[test]
    fn api_url_production_returns_prod_url() {
        assert_eq!(ApiEnv::Production.api_url(), PROD_API_URL);
    }
}
