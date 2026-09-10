use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use arc_swap::ArcSwap;
use chrono::Utc;
use n0_error::Result;
use tokio::sync::watch;


use crate::{ProjectControlPlaneClient, Repo, SelectedContext};

pub mod env;
pub mod external_token_source;

pub use self::{
    env::ApiEnv,
};

use self::external_token_source::ExternalTokenSource;

/// Inline replacement for `openidconnect::AccessToken` — removed to avoid dependency.
#[derive(Debug, Clone)]
pub struct AccessToken(String);

impl AccessToken {
    pub fn new(token: String) -> Self {
        Self(token)
    }

    pub fn secret(&self) -> &str {
        &self.0
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub(crate) mod auth {
    use chrono::Utc;
    use std::sync::Arc;
    use std::time::Duration as StdDuration;

    use arc_swap::ArcSwap;

    use super::AccessToken;

    #[derive(Debug, Clone)]
    pub struct AuthTokens {
        pub access_token: AccessToken,
        pub refresh_token: Option<String>,
        pub issued_at: chrono::DateTime<Utc>,
        pub expires_in: StdDuration,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum LoginState {
        Missing,
        Valid,
        Refreshing,
    }

    impl Default for LoginState {
        fn default() -> Self {
            LoginState::Missing
        }
    }

    #[derive(Debug, Clone)]
    pub struct UserProfile {
        pub user_id: String,
        pub email: String,
        pub first_name: Option<String>,
        pub last_name: Option<String>,
        pub avatar_url: Option<String>,
        pub registration_approval: Option<String>,
    }

    #[derive(Debug, Clone)]
    pub struct AuthState {
        pub tokens: AuthTokens,
        pub profile: UserProfile,
    }

    #[derive(Debug)]
    pub struct MaybeAuth(ArcSwap<AuthState>);

    impl Clone for MaybeAuth {
        fn clone(&self) -> Self {
            Self(ArcSwap::from(self.0.load_full()))
        }
    }

    impl MaybeAuth {
        pub fn new(state: AuthState) -> Self {
            Self(ArcSwap::from_pointee(state))
        }

        pub fn dummy(state: AuthState) -> Self {
            Self(ArcSwap::from_pointee(state))
        }

        pub fn load(&self) -> Arc<AuthState> {
            self.0.load_full()
        }

        pub fn get(&self) -> Result<Arc<AuthState>, ()> {
            Ok(self.0.load_full())
        }
    }

    impl AuthState {
        pub fn get(&self) -> Result<&AuthState, ()> {
            Ok(self)
        }
    }

    /// Outcome classification for a token refresh attempt.
    ///
    /// The distinction matters for the heartbeat loop and the listener: a
    /// `Transient` failure should keep auth state intact and let the next retry
    /// recover, while a `Permanent` failure means the OAuth provider has
    /// definitively rejected our credentials and only a fresh interactive login
    /// can recover. Treating every refresh failure as permanent — which the
    /// previous implementation did — meant a 30-second IdP wobble would log a
    /// long-running tunnel out.
    ///
    /// Connect-lib note: this fork operates in plugin mode where token refresh
    /// is performed by the parent process (datumctl) via the
    /// DATUM_CREDENTIALS_HELPER subprocess. The OIDC machinery that produced
    /// these errors upstream does not exist in connect-lib. The enum is
    /// nevertheless exposed at `pub(crate)` so heartbeat/binary callers can
    /// classify any future in-process refresh attempts (Phase 12 work) and so
    /// the surface matches upstream for downstream merge compatibility.
    #[derive(Debug)]
    #[allow(dead_code)]
    pub enum RefreshError {
        /// The IdP definitively rejected the refresh (typically `invalid_grant`,
        /// `invalid_client`, etc.). Auth state has been cleared; the operator
        /// must log in again.
        Permanent(n0_error::AnyError),
        /// Transient failure (network, IdP 5xx, parse error, ID-token claim
        /// verification). Auth state is preserved; the caller should retry with
        /// backoff.
        Transient(n0_error::AnyError),
    }

    impl std::fmt::Display for RefreshError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Permanent(e) => write!(f, "refresh permanently rejected by IdP: {e:#}"),
                Self::Transient(e) => write!(f, "transient refresh failure: {e:#}"),
            }
        }
    }

    impl std::error::Error for RefreshError {}
}

#[allow(unused_imports)]
pub(crate) use self::auth::RefreshError;

pub use self::auth::{AuthState, AuthTokens, LoginState, MaybeAuth, UserProfile};

#[derive(derive_more::Debug, Clone)]
pub struct DatumCloudClient {
    env: ApiEnv,
    token_source: Arc<ExternalTokenSource>,
    session: SessionStateWrapper,
    login_state_tx: watch::Sender<LoginState>,
}

impl DatumCloudClient {
    /// Constructs a `DatumCloudClient` using an `ExternalTokenSource` (plugin mode).
    pub fn with_external_token_source(env: ApiEnv, token_source: ExternalTokenSource) -> Self {
        let (login_state_tx, _) = watch::channel(LoginState::Valid);
        Self {
            env,
            token_source: Arc::new(token_source),
            session: SessionStateWrapper::empty(),
            login_state_tx,
        }
    }

    pub fn login_state(&self) -> LoginState {
        LoginState::Valid
    }

    pub fn is_plugin_mode(&self) -> bool {
        true
    }

    pub fn token(&self) -> String {
        self.token_source.token()
    }

    /// Force an immediate token refresh by signalling the background refresh
    /// loop in [`ExternalTokenSource`] to re-execute the credentials helper
    /// now, ahead of its proactive schedule.
    ///
    /// Call this when a 401 response is observed from the API: the proactive
    /// refresh timer only fires near JWT expiry, so a token rejected early
    /// (clock skew, revocation, IdP-side rotation) would otherwise leave
    /// callers retrying with the same dead token until the timer catches up.
    /// The refresh loop swaps the new token into the shared [`ExternalTokenSource`]
    /// and notifies watchers; the next [`Self::project_control_plane_client`]
    /// call (and any `ProjectControlPlaneClient` rebuilt via its token watch)
    /// picks up the fresh token.
    pub fn force_token_refresh(&self) {
        self.token_source.force_refresh();
    }

    pub fn api_url(&self) -> Cow<'static, str> {
        self.env.api_url()
    }

    pub fn web_url(&self) -> Cow<'static, str> {
        self.env.web_url()
    }

    pub fn auth_update_watch(&self) -> watch::Receiver<u64> {
        let (_, rx) = watch::channel(0u64);
        rx
    }

    /// Returns a watch receiver for login state changes.
    pub fn login_state_watch(&self) -> watch::Receiver<LoginState> {
        self.login_state_tx.subscribe()
    }

    pub fn auth_state(&self) -> Arc<MaybeAuth> {
        Arc::new(MaybeAuth::dummy(AuthState {
            tokens: AuthTokens {
                access_token: AccessToken::new(self.token_source.token()),
                refresh_token: None,
                issued_at: Utc::now(),
                expires_in: StdDuration::from_secs(3600),
            },
            profile: UserProfile {
                user_id: "external".to_string(),
                email: "external@plugin".to_string(),
                first_name: None,
                last_name: None,
                avatar_url: None,
                registration_approval: None,
            },
        }))
    }

    pub async fn is_authenticated(&self) -> Result<bool> {
        Ok(true)
    }

    pub async fn login(&self) -> Result<()> {
        Ok(())
    }

    pub async fn logout(&self) -> Result<()> {
        Ok(())
    }

    pub fn selected_context(&self) -> Option<SelectedContext> {
        self.session.selected_context()
    }

    pub fn selected_context_watch(&self) -> watch::Receiver<Option<SelectedContext>> {
        self.session.selected_context_watch()
    }

    pub async fn set_selected_context(
        &self,
        selected_context: Option<SelectedContext>,
    ) -> Result<()> {
        self.session.set_selected_context(selected_context).await
    }

    fn project_control_plane_url(&self, project_id: &str) -> String {
        format!(
            "{}/apis/resourcemanager.miloapis.com/v1alpha1/projects/{project_id}/control-plane",
            self.api_url()
        )
    }

    pub async fn project_control_plane_client(
        &self,
        project_id: &str,
    ) -> Result<ProjectControlPlaneClient> {
        let token = self.token_source.token();
        self.project_control_plane_client_with_token(project_id, &token)
    }

    pub async fn project_control_plane_client_active(
        &self,
    ) -> Result<Option<ProjectControlPlaneClient>> {
        let Some(selected) = self.selected_context() else {
            return Ok(None);
        };
        Ok(Some(
            self.project_control_plane_client(&selected.project_id)
                .await?,
        ))
    }

    pub fn orgs_projects_cache(&self) -> Vec<OrganizationWithProjects> {
        self.session.orgs_projects()
    }

    pub fn orgs_projects_watch(&self) -> watch::Receiver<Vec<OrganizationWithProjects>> {
        self.session.orgs_projects_watch()
    }

    fn project_control_plane_client_with_token(
        &self,
        project_id: &str,
        access_token: &str,
    ) -> Result<ProjectControlPlaneClient> {
        let server_url = self.project_control_plane_url(project_id);
        ProjectControlPlaneClient::new(
            project_id.to_string(),
            server_url,
            access_token.to_string(),
            self.clone(),
        )
    }
}

#[derive(Debug, Clone, Default)]
struct SessionStateWrapper {
    selected_context: Arc<ArcSwap<Option<SelectedContext>>>,
    selected_context_tx: watch::Sender<Option<SelectedContext>>,
    orgs_projects: Arc<ArcSwap<Vec<OrganizationWithProjects>>>,
    orgs_projects_tx: watch::Sender<Vec<OrganizationWithProjects>>,
    repo: Option<Repo>,
}

impl SessionStateWrapper {
    fn empty() -> Self {
        let (selected_context_tx, _) = watch::channel(None);
        let (orgs_projects_tx, _) = watch::channel(Vec::new());
        Self {
            selected_context: Arc::new(ArcSwap::from_pointee(None)),
            selected_context_tx,
            orgs_projects: Arc::new(ArcSwap::from_pointee(Vec::new())),
            orgs_projects_tx,
            repo: None,
        }
    }

    fn selected_context(&self) -> Option<SelectedContext> {
        self.selected_context.load_full().as_ref().clone()
    }

    fn selected_context_watch(&self) -> watch::Receiver<Option<SelectedContext>> {
        self.selected_context_tx.subscribe()
    }

    async fn set_selected_context(&self, selected_context: Option<SelectedContext>) -> Result<()> {
        let current = self.selected_context.load_full();
        if current.as_ref().as_ref() != selected_context.as_ref() {
            if let Some(repo) = self.repo.as_ref() {
                repo.write_selected_context(selected_context.as_ref())
                    .await?;
            }
            self.selected_context
                .store(Arc::new(selected_context.clone()));
        }
        let _ = self.selected_context_tx.send(selected_context);
        Ok(())
    }

    fn orgs_projects(&self) -> Vec<OrganizationWithProjects> {
        self.orgs_projects.load_full().as_ref().clone()
    }

    fn orgs_projects_watch(&self) -> watch::Receiver<Vec<OrganizationWithProjects>> {
        self.orgs_projects_tx.subscribe()
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Organization {
    pub resource_id: String,
    pub display_name: String,
    pub r#type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrganizationWithProjects {
    pub org: Organization,
    pub projects: Vec<Project>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub resource_id: String,
    pub display_name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::setup_plugin_env;

    #[test]
    fn with_external_token_source_creates_plugin_mode_client() {
        let (_dir, token_source) = setup_plugin_env();
        let client = DatumCloudClient::with_external_token_source(ApiEnv::Production, token_source);
        assert!(client.is_plugin_mode());
    }

    #[test]
    fn login_state_valid_in_plugin_mode() {
        let (_dir, token_source) = setup_plugin_env();
        let client = DatumCloudClient::with_external_token_source(ApiEnv::Production, token_source);
        assert_eq!(client.login_state(), LoginState::Valid);
    }

    #[test]
    fn token_returns_external_token() {
        let (_dir, token_source) = setup_plugin_env();
        let client = DatumCloudClient::with_external_token_source(ApiEnv::Production, token_source);
        // The fake helper returns a JWT with exp 9999999999
        assert!(client.token().starts_with("eyJ"));
    }

    #[test]
    fn auth_state_returns_dummy_in_plugin_mode() {
        let (_dir, token_source) = setup_plugin_env();
        let client = DatumCloudClient::with_external_token_source(ApiEnv::Production, token_source);
        let auth_state = client.auth_state();
        assert!(auth_state.get().is_ok());
        let auth = auth_state.get().unwrap();
        assert_eq!(auth.profile.user_id, "external");
        assert_eq!(auth.profile.email, "external@plugin");
    }

    #[test]
    fn api_url_from_env_in_plugin_mode() {
        let (_dir, token_source) = setup_plugin_env();
        let client = DatumCloudClient::with_external_token_source(ApiEnv::Production, token_source);
        assert!(client.api_url().contains("datum.net"));
    }

    #[test]
    fn web_url_from_env_in_plugin_mode() {
        let (_dir, token_source) = setup_plugin_env();
        let client = DatumCloudClient::with_external_token_source(ApiEnv::Production, token_source);
        assert!(client.web_url().contains("datum.net"));
    }

    #[test]
    fn datum_cloud_client_clone_in_plugin_mode() {
        let (_dir, token_source) = setup_plugin_env();
        let client = DatumCloudClient::with_external_token_source(ApiEnv::Production, token_source);
        let cloned = client.clone();
        assert!(cloned.is_plugin_mode());
        assert_eq!(cloned.token(), client.token());
    }

    #[test]
    fn auth_update_watch_returns_receiver_in_plugin_mode() {
        let (_dir, token_source) = setup_plugin_env();
        let client = DatumCloudClient::with_external_token_source(ApiEnv::Production, token_source);
        let rx = client.auth_update_watch();
        // Initial value should be 0
        assert_eq!(*rx.borrow(), 0);
    }

    #[test]
    fn selected_context_is_none_in_plugin_mode() {
        let (_dir, token_source) = setup_plugin_env();
        let client = DatumCloudClient::with_external_token_source(ApiEnv::Production, token_source);
        // In plugin mode, session state is empty (no OIDC repo)
        assert!(client.selected_context().is_none());
    }

    #[test]
    fn login_state_watch_returns_receiver_in_plugin_mode() {
        let (_dir, token_source) = setup_plugin_env();
        let client = DatumCloudClient::with_external_token_source(ApiEnv::Production, token_source);
        let rx = client.login_state_watch();
        assert_eq!(*rx.borrow(), LoginState::Valid);
    }
}
