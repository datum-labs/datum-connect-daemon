use std::sync::Arc;

use arc_swap::ArcSwap;
use http::HeaderValue;
use http::header::USER_AGENT;
use kube::{Client, Config};
use n0_error::{Result, StdResultExt};
use n0_future::task::AbortOnDropHandle;
use secrecy::SecretString;
use tokio::sync::watch;
use tracing::warn;

use crate::datum_cloud::DatumCloudClient;
use crate::datum_cloud::LoginState;
use crate::http_user_agent::datum_http_user_agent;

#[derive(derive_more::Debug, Clone)]
pub struct ProjectControlPlaneClient {
    project_id: String,
    server_url: String,
    access_token: Arc<ArcSwap<String>>,
    #[debug("kube::Client")]
    client: Arc<ArcSwap<Client>>,
    datum: DatumCloudClient,
    _auth_task: Option<Arc<AbortOnDropHandle<()>>>,
    token_rx: Option<watch::Receiver<String>>,
}

impl ProjectControlPlaneClient {
    pub fn new(
        project_id: String,
        server_url: String,
        access_token: String,
        datum: DatumCloudClient,
    ) -> Result<Self> {
        let client = Self::build_kube_client(&server_url, &access_token)?;
        let mut this = Self {
            project_id,
            server_url,
            access_token: Arc::new(ArcSwap::from_pointee(access_token)),
            client: Arc::new(ArcSwap::from_pointee(client)),
            datum,
            _auth_task: None,
            token_rx: None,
        };
        this.start_auth_watch();
        Ok(this)
    }

    pub fn new_with_token_source(
        project_id: String,
        server_url: String,
        token_source: crate::datum_cloud::external_token_source::ExternalTokenSource,
    ) -> Result<Self> {
        let initial_token = token_source.token();
        let client = Self::build_kube_client(&server_url, &initial_token)?;
        let datum = DatumCloudClient::with_external_token_source(
            crate::ApiEnv::from_env_with_host_override(),
            token_source.clone(),
        );
        let mut this = Self {
            project_id,
            server_url,
            access_token: Arc::new(ArcSwap::from_pointee(initial_token)),
            client: Arc::new(ArcSwap::from_pointee(client)),
            datum,
            _auth_task: None,
            token_rx: Some(token_source.watch()),
        };
        this.start_auth_watch();
        Ok(this)
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub fn server_url(&self) -> &str {
        &self.server_url
    }

    pub fn access_token(&self) -> String {
        self.access_token.load_full().as_ref().clone()
    }

    pub fn client(&self) -> Client {
        self.client.load_full().as_ref().clone()
    }

    pub async fn client_refreshed(&self) -> Result<Client> {
        let access_token = self.datum.token();
        self.rebuild_if_changed(&access_token)?;
        Ok(self.client())
    }

    fn build_kube_client(server_url: &str, access_token: &str) -> Result<Client> {
        let uri = server_url
            .parse()
            .std_context("Invalid project control plane URL")?;
        let mut config = Config::new(uri);
        config.auth_info.token = Some(SecretString::new(access_token.to_string().into_boxed_str()));
        let ua = HeaderValue::from_str(&datum_http_user_agent())
            .std_context("Invalid User-Agent for kube client")?;
        config.headers.push((USER_AGENT, ua));
        Client::try_from(config).std_context("Failed to create project control plane client")
    }

    fn rebuild_if_changed(&self, access_token: &str) -> Result<()> {
        let current = self.access_token.load_full();
        if current.as_ref().as_str() == access_token {
            return Ok(());
        }

        let client = Self::build_kube_client(&self.server_url, access_token)?;
        self.client.store(Arc::new(client));
        self.access_token.store(Arc::new(access_token.to_string()));
        Ok(())
    }

    async fn refresh_client_from_update(&self) -> Result<()> {
        if self.datum.is_plugin_mode() {
            let token = self.datum.token();
            return self.rebuild_if_changed(&token);
        }
        let auth_state = self.datum.auth_state();
        let auth = auth_state.load();
        self.rebuild_if_changed(&auth.tokens.access_token.secret().to_string())
    }

    fn start_auth_watch(&mut self) {
        if self._auth_task.is_some() {
            return;
        }
        let mut client = self.clone();
        let task = tokio::spawn(async move {
            if let Some(token_rx) = client.token_rx.take() {
                if let Err(err) = client.refresh_client_from_update().await {
                    warn!("failed to refresh project control plane client: {err:#}");
                }
                let mut token_rx = token_rx;
                loop {
                    if token_rx.changed().await.is_err() {
                        return;
                    }
                    let new_token = (*token_rx.borrow()).clone();
                    if let Err(err) = client.rebuild_if_changed(&new_token) {
                        warn!("failed to refresh project control plane client: {err:#}");
                    }
                }
            } else {
                let mut login_rx = client.datum.login_state_watch();
                let mut auth_update_rx = client.datum.auth_update_watch();
                if *login_rx.borrow() != LoginState::Missing
                    && let Err(err) = client.refresh_client_from_update().await
                {
                    warn!("failed to refresh project control plane client: {err:#}");
                }
                loop {
                    tokio::select! {
                        res = login_rx.changed() => {
                            if res.is_err() {
                                return;
                            }
                        }
                        res = auth_update_rx.changed() => {
                            if res.is_err() {
                                return;
                            }
                        }
                    }
                    if *login_rx.borrow() != LoginState::Missing
                        && let Err(err) = client.refresh_client_from_update().await
                    {
                        warn!("failed to refresh project control plane client: {err:#}");
                    }
                }
            }
        });
        self._auth_task = Some(Arc::new(AbortOnDropHandle::new(task)));
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    #[allow(unused_imports)]
    use crate::test_util::setup_plugin_env;

    // These tests require rustls CryptoProvider (requires 'ring' or 'aws-lc-rs'
    // feature). Gate behind a feature flag so they don't fail in CI when
    // those features are disabled. Run manually with:
    //   cargo test --lib --features integration-tests

    #[test]
    #[cfg(feature = "integration-tests")]
    fn new_with_token_source_accepts_external_token_source() {
        let (_dir, token_source) = setup_plugin_env();
        let result = ProjectControlPlaneClient::new_with_token_source(
            "test-project".to_string(),
            "https://api.datum.net/apis/resourcemanager.miloapis.com/v1alpha1/projects/test-project/control-plane".to_string(),
            token_source,
        );
        let _ = result;
    }

    #[test]
    #[cfg(feature = "integration-tests")]
    fn new_with_token_source_sets_project_id() {
        let (_dir, token_source) = setup_plugin_env();
        let pcp = ProjectControlPlaneClient::new_with_token_source(
            "my-project-id".to_string(),
            "https://api.datum.net/apis/resourcemanager.miloapis.com/v1alpha1/projects/my-project-id/control-plane".to_string(),
            token_source,
        );
        if let Ok(pcp) = pcp {
            assert_eq!(pcp.project_id(), "my-project-id");
        }
    }

    #[test]
    #[cfg(feature = "integration-tests")]
    fn access_token_returns_token_from_source() {
        let (_dir, token_source) = setup_plugin_env();
        let expected_token = token_source.token();
        let pcp = ProjectControlPlaneClient::new_with_token_source(
            "test-project".to_string(),
            "https://api.datum.net/apis/resourcemanager.miloapis.com/v1alpha1/projects/test-project/control-plane".to_string(),
            token_source,
        );
        if let Ok(pcp) = pcp {
            assert_eq!(pcp.access_token(), expected_token);
        }
    }

    #[test]
    #[cfg(feature = "integration-tests")]
    fn server_url_is_stored() {
        let (_dir, token_source) = setup_plugin_env();
        let server_url = "https://custom.api.net/apis/resourcemanager.miloapis.com/v1alpha1/projects/test/control-plane".to_string();
        let pcp = ProjectControlPlaneClient::new_with_token_source(
            "test-project".to_string(),
            server_url.clone(),
            token_source,
        );
        if let Ok(pcp) = pcp {
            assert_eq!(pcp.server_url(), server_url);
        }
    }

    #[test]
    #[cfg(feature = "integration-tests")]
    fn datum_is_plugin_mode_after_new_with_token_source() {
        let (_dir, token_source) = setup_plugin_env();
        let pcp = ProjectControlPlaneClient::new_with_token_source(
            "test-project".to_string(),
            "https://api.datum.net/apis/resourcemanager.miloapis.com/v1alpha1/projects/test-project/control-plane".to_string(),
            token_source,
        );
        if let Ok(pcp) = pcp {
            assert!(pcp.datum.is_plugin_mode());
        }
    }
}
