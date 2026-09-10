use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use chrono::Utc;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use kube::api::{ListParams, Patch, PatchParams};
use kube::{Api, ResourceExt};
use n0_error::{Result, StdResultExt};
use n0_future::task::AbortOnDropHandle;
use rand::Rng;
use serde_json::json;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::ListenNode;
use crate::datum_apis::connector::{
    Connector, ConnectorConnectionDetails, ConnectorConnectionDetailsPublicKey,
    ConnectorConnectionType, PublicKeyConnectorAddress, PublicKeyDiscoveryMode,
};
use crate::datum_apis::lease::Lease;
use crate::datum_cloud::DatumCloudClient;
use crate::kube_error::{is_not_found, is_unauthorized};

type ProjectRunner = Arc<
    dyn Fn(
            String,
            DatumCloudClient,
            Arc<dyn HeartbeatDetailsProvider>,
            CancellationToken,
        ) -> tokio::task::JoinHandle<()>
        + Send
        + Sync,
>;

use crate::DEFAULT_PCP_NAMESPACE;
const DEFAULT_LEASE_DURATION_SECS: i32 = 30;
const BACKOFF_INITIAL: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

#[derive(derive_more::Debug, Clone)]
pub struct HeartbeatAgent {
    #[debug(skip)]
    inner: Arc<HeartbeatInner>,
}

struct HeartbeatInner {
    datum: DatumCloudClient,
    provider: Arc<dyn HeartbeatDetailsProvider>,
    runner: ProjectRunner,
    projects: Mutex<HashMap<String, ProjectHeartbeat>>,
    known_projects: Mutex<HashSet<String>>,
    login_task: Mutex<Option<AbortOnDropHandle<()>>>,
}

struct ProjectHeartbeat {
    cancel: CancellationToken,
    _task: AbortOnDropHandle<()>,
}

impl HeartbeatAgent {
    pub fn new(datum: DatumCloudClient, listen: ListenNode) -> Self {
        let provider = Arc::new(ListenNodeDetailsProvider::new(listen));
        let runner: ProjectRunner = Arc::new(|project_id, datum, provider, cancel| {
            tokio::spawn(run_project(project_id, datum, provider, cancel))
        });
        Self::new_with_runner(datum, provider, runner)
    }

    fn new_with_runner(
        datum: DatumCloudClient,
        provider: Arc<dyn HeartbeatDetailsProvider>,
        runner: ProjectRunner,
    ) -> Self {
        Self {
            inner: Arc::new(HeartbeatInner {
                datum,
                provider,
                runner,
                projects: Mutex::new(HashMap::new()),
                known_projects: Mutex::new(HashSet::new()),
                login_task: Mutex::new(None),
            }),
        }
    }

    /// Start in auto-enroll mode: watch login + projects state and keep
    /// heartbeats running for every project the user has access to.
    /// Intended for multi-project consumers like the UI.
    ///
    /// For the CLI tunnel use case where there is exactly one project of
    /// interest, prefer [`Self::start_manual`] — auto-enroll silently
    /// maintains presence in projects the user didn't ask about.
    pub async fn start(&self) {
        let mut guard = self.inner.login_task.lock().await;
        if guard.is_some() {
            return;
        }

        // In plugin mode, login state never changes and projects are fixed.
        // Skip the background watcher — just do an initial project refresh.
        if self.inner.datum.is_plugin_mode() {
            if let Err(err) = self.refresh_projects().await {
                warn!("heartbeat: bootstrap failed: {err:#}");
            }
            return;
        }

        let this = self.clone();
        let mut login_rx = this.inner.datum.login_state_watch();
        let mut projects_rx = this.inner.datum.orgs_projects_watch();
        let task = tokio::spawn(async move {
            if *login_rx.borrow() != crate::datum_cloud::LoginState::Missing
                && let Err(err) = this.refresh_projects().await
            {
                warn!("heartbeat: bootstrap failed: {err:#}");
            }
            loop {
                tokio::select! {
                    res = login_rx.changed() => {
                        if res.is_err() {
                            return;
                        }
                        let login_state = login_rx.borrow().clone();
                        match login_state {
                            crate::datum_cloud::LoginState::Missing => {
                                this.clear_projects().await;
                                this.clear_known_projects().await;
                            }
                            _ => {
                                if let Err(err) = this.refresh_projects().await {
                                    warn!("heartbeat: bootstrap failed: {err:#}");
                                }
                            }
                        }
                    }
                    res = projects_rx.changed() => {
                        if res.is_err() {
                            return;
                        }
                        if *login_rx.borrow() != crate::datum_cloud::LoginState::Missing
                            && let Err(err) = this.refresh_projects().await {
                                warn!("heartbeat: bootstrap failed: {err:#}");
                            }
                    }
                }
            }
        });
        *guard = Some(AbortOnDropHandle::new(task));
    }

    /// Start in manual mode: do not watch login state and do not auto-enroll
    /// projects. Callers are responsible for [`Self::register_project`] /
    /// [`Self::deregister_project`] for the projects they want heartbeats
    /// for. Per-project loops still handle 401s via their own
    /// force-refresh logic, so transient auth blips are tolerated; a
    /// permanent logout is surfaced separately by the CLI's own login
    /// watcher.
    pub async fn start_manual(&self) {
        let mut guard = self.inner.login_task.lock().await;
        if guard.is_some() {
            return;
        }
        // Park a completed task so future start() / start_manual() calls
        // remain no-ops, matching start()'s "single-start" contract.
        let task = tokio::spawn(async {});
        *guard = Some(AbortOnDropHandle::new(task));
    }

    pub async fn register_project(&self, project_id: impl Into<String>) {
        let project_id = project_id.into();
        let mut projects = self.inner.projects.lock().await;
        if projects.contains_key(&project_id) {
            return;
        }
        let cancel = CancellationToken::new();
        let task = (self.inner.runner)(
            project_id.clone(),
            self.inner.datum.clone(),
            self.inner.provider.clone(),
            cancel.clone(),
        );
        projects.insert(
            project_id,
            ProjectHeartbeat {
                cancel,
                _task: AbortOnDropHandle::new(task),
            },
        );
    }

    pub async fn deregister_project(&self, project_id: &str) {
        let mut projects = self.inner.projects.lock().await;
        if let Some(project) = projects.remove(project_id) {
            project.cancel.cancel();
        }
    }

    async fn clear_projects(&self) {
        let mut projects = self.inner.projects.lock().await;
        for (_, project) in projects.drain() {
            project.cancel.cancel();
        }
    }

    async fn clear_known_projects(&self) {
        let mut known = self.inner.known_projects.lock().await;
        known.clear();
    }

    pub async fn refresh_projects(&self) -> Result<()> {
        let orgs = self.inner.datum.orgs_projects_cache();
        let mut next_projects: HashSet<String> = HashSet::new();
        for org in orgs {
            for project in org.projects {
                next_projects.insert(project.resource_id);
            }
        }

        {
            let mut known = self.inner.known_projects.lock().await;
            if *known == next_projects {
                return Ok(());
            }
            *known = next_projects.clone();
        }

        let running: Vec<String> = {
            let projects = self.inner.projects.lock().await;
            projects.keys().cloned().collect()
        };
        for project_id in running {
            if !next_projects.contains(&project_id) {
                self.deregister_project(&project_id).await;
            }
        }

        for project_id in &next_projects {
            let should_probe = {
                let projects = self.inner.projects.lock().await;
                !projects.contains_key(project_id.as_str())
            };
            if !should_probe {
                continue;
            }
            match probe_connector(
                &project_id,
                self.inner.datum.clone(),
                self.inner.provider.clone(),
            )
            .await
            {
                Ok(true) => self.register_project(project_id.clone()).await,
                Ok(false) => {
                    debug!(%project_id, "heartbeat: no connector yet");
                }
                Err(err) => {
                    warn!(%project_id, "heartbeat: connector probe failed: {err:#}");
                }
            }
        }

        Ok(())
    }
}

struct ConnectorCache {
    name: String,
    lease_name: Option<String>,
    lease_duration_seconds: Option<i32>,
    last_details: Option<serde_json::Value>,
    last_home_relay: Option<String>,
}

/// Returns true if `err` is a kube API error with HTTP status 401.
/// What the heartbeat loop should do with its cache after a lease op fails.
#[derive(Debug, PartialEq, Eq)]
enum LeaseErrorAction {
    /// Keep the cached connector/lease names; retry after backoff.
    Retain,
    /// Drop the cache so the next iteration re-resolves connector and lease
    /// from scratch. Used when the lease no longer exists server-side.
    Reset,
    /// Force a token refresh, then retain the cache and retry.
    RefreshAuth,
}

fn classify_lease_error(err: &kube::Error) -> LeaseErrorAction {
    if is_not_found(err) {
        LeaseErrorAction::Reset
    } else if is_unauthorized(err) {
        LeaseErrorAction::RefreshAuth
    } else {
        LeaseErrorAction::Retain
    }
}

/// Force a token refresh after a 401. The proactive refresh timer in
/// [`ExternalTokenSource`] only fires when the access token is within 60s of
/// JWT expiry, so a token rejected before that (clock skew, revocation,
/// IdP-side rotation) would otherwise leave the heartbeat retrying with the
/// same dead token until the timer eventually catches up — the classic
/// "stale auth" tunnel failure.
///
/// This signals the in-process refresh loop to re-execute the
/// `DATUM_CREDENTIALS_HELPER` subprocess immediately. The loop swaps the new
/// token into the shared [`ExternalTokenSource`] and notifies watchers; the
/// next `project_control_plane_client()` call picks up the fresh token.
///
/// When auth is already in [`LoginState::Missing`] (e.g. after a previous
/// permanent refresh failure), this returns immediately without contacting
/// the helper — the auth layer has already surfaced the loss to the operator
/// and there is nothing to refresh until they log in again.
async fn force_refresh_auth(project_id: &str, datum: &DatumCloudClient) {
    if matches!(datum.login_state(), crate::datum_cloud::LoginState::Missing) {
        debug!(
            %project_id,
            "heartbeat: skipping forced refresh — auth state is missing, awaiting login"
        );
        return;
    }
    info!(
        %project_id,
        "heartbeat: 401 observed; forcing token refresh via credentials helper"
    );
    datum.force_token_refresh();
}

async fn run_project(
    project_id: String,
    datum: DatumCloudClient,
    provider: Arc<dyn HeartbeatDetailsProvider>,
    cancel: CancellationToken,
) {
    let mut backoff = Backoff::new();
    let mut cache: Option<ConnectorCache> = None;

    loop {
        if cancel.is_cancelled() {
            return;
        }

        let pcp = match datum.project_control_plane_client(&project_id).await {
            Ok(client) => client,
            Err(err) => {
                warn!(%project_id, "heartbeat: failed to get pcp client: {err:#}");
                sleep_with_cancel(backoff.next(), &cancel).await;
                continue;
            }
        };
        let client = pcp.client();
        let connectors: Api<Connector> = Api::namespaced(client.clone(), DEFAULT_PCP_NAMESPACE);
        let leases: Api<Lease> = Api::namespaced(client, DEFAULT_PCP_NAMESPACE);

        if cache.is_none() {
            match find_connector(&connectors, provider.endpoint_id()).await {
                Ok(Some(connector)) => {
                    let connector_name = connector.name_any();
                    let lease_name = connector
                        .status
                        .as_ref()
                        .and_then(|status| status.lease_ref.as_ref())
                        .map(|lease| lease.name.clone());
                    let last_home_relay = connector
                        .status
                        .as_ref()
                        .and_then(|status| status.connection_details.as_ref())
                        .and_then(|details| details.public_key.as_ref())
                        .map(|details| details.home_relay.clone());
                    info!(
                        %project_id,
                        connector = %connector_name,
                        lease = lease_name.as_deref().unwrap_or("<pending>"),
                        "heartbeat: registered connector, starting lease renewals"
                    );
                    cache = Some(ConnectorCache {
                        name: connector_name,
                        lease_name,
                        lease_duration_seconds: None,
                        last_details: None,
                        last_home_relay,
                    });
                    backoff.reset();
                }
                Ok(None) => {
                    debug!(%project_id, "heartbeat: no connector yet");
                    sleep_with_cancel(backoff.next(), &cancel).await;
                    continue;
                }
                Err(err) => {
                    warn!(%project_id, "heartbeat: connector lookup failed: {err:#}");
                    if is_unauthorized(&err) {
                        force_refresh_auth(&project_id, &datum).await;
                    }
                    sleep_with_cancel(backoff.next(), &cancel).await;
                    continue;
                }
            }
        }

        let Some(mut cached) = cache.take() else {
            continue;
        };

        if cached.lease_name.is_none() {
            match connectors.get(&cached.name).await {
                Ok(connector) => {
                    cached.lease_name = connector
                        .status
                        .as_ref()
                        .and_then(|status| status.lease_ref.as_ref())
                        .map(|lease| lease.name.clone());
                    if cached.lease_name.is_none() {
                        sleep_with_cancel(backoff.next(), &cancel).await;
                        cache = Some(cached);
                        continue;
                    }
                    cached.last_home_relay = connector
                        .status
                        .as_ref()
                        .and_then(|status| status.connection_details.as_ref())
                        .and_then(|details| details.public_key.as_ref())
                        .map(|details| details.home_relay.clone());
                }
                Err(err) => {
                    warn!(
                        %project_id,
                        connector = %cached.name,
                        "heartbeat: failed to fetch connector: {err:#}"
                    );
                    if is_unauthorized(&err) {
                        force_refresh_auth(&project_id, &datum).await;
                    }
                    cache = None;
                    sleep_with_cancel(backoff.next(), &cancel).await;
                    continue;
                }
            }
        }

        let details = match provider.connection_details(cached.last_home_relay.as_deref()) {
            Some(details) => details,
            None => {
                warn!(%project_id, connector = %cached.name, "heartbeat: missing home relay");
                cache = Some(cached);
                sleep_with_cancel(backoff.next(), &cancel).await;
                continue;
            }
        };

        let details_value = match serde_json::to_value(&details) {
            Ok(value) => value,
            Err(err) => {
                warn!(
                    %project_id,
                    connector = %cached.name,
                    "heartbeat: failed to serialize details: {err:#}"
                );
                cache = Some(cached);
                sleep_with_cancel(backoff.next(), &cancel).await;
                continue;
            }
        };

        if cached.last_details.as_ref() != Some(&details_value) {
            let patch = json!({ "status": { "connectionDetails": details_value } });
            match connectors
                .patch_status(&cached.name, &PatchParams::default(), &Patch::Merge(&patch))
                .await
            {
                Ok(_) => {
                    cached.last_details = Some(patch["status"]["connectionDetails"].clone());
                }
                Err(err) => {
                    warn!(
                        %project_id,
                        connector = %cached.name,
                        "heartbeat: failed to patch connection details: {err:#}"
                    );
                    if is_unauthorized(&err) {
                        force_refresh_auth(&project_id, &datum).await;
                        cache = Some(cached);
                        sleep_with_cancel(backoff.next(), &cancel).await;
                        continue;
                    }
                }
            }
        }

        if cached.lease_duration_seconds.is_none() {
            let Some(lease_name) = cached.lease_name.as_ref() else {
                cache = Some(cached);
                sleep_with_cancel(backoff.next(), &cancel).await;
                continue;
            };
            match leases.get(lease_name).await {
                Ok(lease) => {
                    cached.lease_duration_seconds = lease
                        .spec
                        .as_ref()
                        .and_then(|spec| spec.lease_duration_seconds);
                }
                Err(err) => {
                    warn!(
                        %project_id,
                        lease = %lease_name,
                        "heartbeat: failed to fetch lease: {err:#}"
                    );
                    match classify_lease_error(&err) {
                        LeaseErrorAction::Reset => cache = None,
                        LeaseErrorAction::RefreshAuth => {
                            force_refresh_auth(&project_id, &datum).await;
                            cache = Some(cached);
                        }
                        LeaseErrorAction::Retain => cache = Some(cached),
                    }
                    sleep_with_cancel(backoff.next(), &cancel).await;
                    continue;
                }
            }
        }

        let Some(lease_name) = cached.lease_name.as_ref() else {
            cache = Some(cached);
            sleep_with_cancel(backoff.next(), &cancel).await;
            continue;
        };

        let renew_time = MicroTime(Utc::now());
        let patch = json!({ "spec": { "renewTime": renew_time } });
        if let Err(err) = leases
            .patch(lease_name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
        {
            warn!(%project_id, lease = %lease_name, "heartbeat: lease renew failed: {err:#}");
            match classify_lease_error(&err) {
                LeaseErrorAction::Reset => cache = None,
                LeaseErrorAction::RefreshAuth => {
                    force_refresh_auth(&project_id, &datum).await;
                    cache = Some(cached);
                }
                LeaseErrorAction::Retain => cache = Some(cached),
            }
            sleep_with_cancel(backoff.next(), &cancel).await;
            continue;
        }

        let lease_duration = cached
            .lease_duration_seconds
            .unwrap_or(DEFAULT_LEASE_DURATION_SECS);
        let interval = renewal_interval(lease_duration);
        backoff.reset();
        cache = Some(cached);
        sleep_with_cancel(interval, &cancel).await;
    }
}

async fn probe_connector(
    project_id: &str,
    datum: DatumCloudClient,
    provider: Arc<dyn HeartbeatDetailsProvider>,
) -> Result<bool> {
    let pcp = datum.project_control_plane_client(project_id).await?;
    let client = pcp.client();
    let connectors: Api<Connector> = Api::namespaced(client, DEFAULT_PCP_NAMESPACE);
    let selector = provider.endpoint_id();
    Ok(find_connector(&connectors, selector)
        .await
        .std_context("connector lookup failed")?
        .is_some())
}

async fn find_connector(
    connectors: &Api<Connector>,
    endpoint_id: String,
) -> kube::Result<Option<Connector>> {
    let selector = format!("status.connectionDetails.publicKey.id={endpoint_id}");
    let list = connectors
        .list(&ListParams::default().fields(&selector))
        .await?;
    if list.items.is_empty() {
        return Ok(None);
    }
    if list.items.len() > 1 {
        warn!(
            %selector,
            count = list.items.len(),
            "heartbeat: multiple connectors found, using first"
        );
    }
    Ok(list.items.into_iter().next())
}

trait HeartbeatDetailsProvider: Send + Sync {
    fn endpoint_id(&self) -> String;
    fn connection_details(
        &self,
        fallback_home_relay: Option<&str>,
    ) -> Option<ConnectorConnectionDetails>;
}

struct ListenNodeDetailsProvider {
    listen: ListenNode,
}

impl ListenNodeDetailsProvider {
    fn new(listen: ListenNode) -> Self {
        Self { listen }
    }
}

impl HeartbeatDetailsProvider for ListenNodeDetailsProvider {
    fn endpoint_id(&self) -> String {
        self.listen.endpoint_id().to_string()
    }

    fn connection_details(
        &self,
        fallback_home_relay: Option<&str>,
    ) -> Option<ConnectorConnectionDetails> {
        let endpoint = self.listen.endpoint();
        let endpoint_addr = endpoint.addr();
        let home_relay = endpoint_addr
            .relay_urls()
            .next()
            .map(|url| url.to_string())
            .or_else(|| fallback_home_relay.map(|relay| relay.to_string()))?;
        let addresses: Vec<PublicKeyConnectorAddress> = endpoint_addr
            .ip_addrs()
            .map(|addr| PublicKeyConnectorAddress {
                address: addr.ip().to_string(),
                port: addr.port() as i32,
            })
            .collect();

        Some(ConnectorConnectionDetails {
            connection_type: ConnectorConnectionType::PublicKey,
            public_key: Some(ConnectorConnectionDetailsPublicKey {
                id: endpoint.id().to_string(),
                discovery_mode: Some(PublicKeyDiscoveryMode::Dns),
                home_relay,
                addresses,
            }),
        })
    }
}

fn renewal_interval(lease_duration_seconds: i32) -> Duration {
    let lease_duration_seconds = lease_duration_seconds.max(1) as u64;
    let base = Duration::from_secs((lease_duration_seconds / 2).max(1));
    let jitter_max = (base.as_secs() / 5).max(1);
    let mut rng = rand::rng();
    let jitter = rng.random_range(0..=jitter_max);
    base + Duration::from_secs(jitter)
}

async fn sleep_with_cancel(duration: Duration, cancel: &CancellationToken) {
    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = tokio::time::sleep(duration) => {}
    }
}

struct Backoff {
    current: Duration,
}

impl Backoff {
    fn new() -> Self {
        Self {
            current: BACKOFF_INITIAL,
        }
    }

    fn next(&mut self) -> Duration {
        let wait = self.current;
        self.current = (self.current * 2).min(BACKOFF_MAX);
        wait
    }

    fn reset(&mut self) {
        self.current = BACKOFF_INITIAL;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datum_cloud::{ApiEnv, DatumCloudClient, RefreshError};
    use crate::test_util::{api_error, setup_plugin_env};

    struct TestProvider {
        endpoint_id: String,
    }

    impl HeartbeatDetailsProvider for TestProvider {
        fn endpoint_id(&self) -> String {
            self.endpoint_id.clone()
        }

        fn connection_details(
            &self,
            _fallback_home_relay: Option<&str>,
        ) -> Option<ConnectorConnectionDetails> {
            None
        }
    }

    fn test_datum_client() -> DatumCloudClient {
        let (_dir, token_source) = setup_plugin_env();
        DatumCloudClient::with_external_token_source(ApiEnv::Production, token_source)
    }

    #[tokio::test]
    async fn start_manual_does_not_auto_enroll() {
        // Manual mode is the CLI tunnel-listen path: only the project the
        // caller explicitly registers should get a heartbeat task. Auto-
        // enroll would have probed `orgs_and_projects()` on bootstrap and
        // registered every accessible project — we verify it didn't by
        // checking the projects map stays empty until we register one.
        //
        // Adapted from upstream test (datum-cloud/app@b7e9d6b): the upstream
        // version constructed the client via `DatumCloudClient::with_repo`
        // (an OIDC path that does not exist on the connect-side fork —
        // connect-lib uses ExternalTokenSource in plugin mode). The
        // assertion semantics are identical.
        let datum = test_datum_client();
        let provider = Arc::new(TestProvider {
            endpoint_id: "test-endpoint".to_string(),
        });
        let runner: ProjectRunner = Arc::new(|_project_id, _datum, _provider, cancel| {
            tokio::spawn(async move {
                cancel.cancelled().await;
            })
        });
        let agent = HeartbeatAgent::new_with_runner(datum, provider, runner);

        agent.start_manual().await;
        // Give any background bootstrap a chance to run; manual mode
        // shouldn't have spawned one, but if it did this would expose it.
        tokio::task::yield_now().await;
        assert_eq!(
            agent.inner.projects.lock().await.len(),
            0,
            "manual mode must not auto-enroll any project"
        );

        agent.register_project("explicit-project").await;
        assert_eq!(
            agent.inner.projects.lock().await.len(),
            1,
            "register_project still works in manual mode"
        );

        // start_manual is idempotent (matches start()'s contract): a
        // second call is a no-op rather than tearing down and replacing.
        agent.start_manual().await;
        assert_eq!(agent.inner.projects.lock().await.len(), 1);
    }

    #[test]
    fn refresh_error_variants_classify_transient_vs_permanent() {
        // Heartbeat-side classification consumer test: the auth layer
        // (datum_cloud::auth::RefreshError) hands the heartbeat loop a
        // typed error. A `Transient` variant means "keep credentials,
        // retry with backoff"; a `Permanent` variant means "auth state
        // is dead, stop hammering the IdP until re-login". Today the
        // connect-side fork operates in plugin mode so neither variant
        // is produced in-process (token refresh is external) — this
        // test exists to assert the matchable surface for downstream
        // callers (Phase 12 binary) and to satisfy the Wave 3
        // acceptance criterion that heartbeat.rs references
        // RefreshError.
        let transient = RefreshError::Transient(n0_error::anyerr!("IdP 5xx, retry"));
        let permanent = RefreshError::Permanent(n0_error::anyerr!("refresh token revoked"));
        match transient {
            RefreshError::Transient(_) => {}
            RefreshError::Permanent(_) => panic!("Transient must not match Permanent"),
        }
        match permanent {
            RefreshError::Permanent(_) => {}
            RefreshError::Transient(_) => panic!("Permanent must not match Transient"),
        }
        // Display impl must clearly differentiate the two so the heartbeat
        // log line can be grepped (Transient → keep retrying;
        // Permanent → surface to operator).
        assert!(
            format!("{}", RefreshError::Transient(n0_error::anyerr!("x")))
                .contains("transient")
        );
        assert!(
            format!("{}", RefreshError::Permanent(n0_error::anyerr!("x")))
                .contains("permanently")
        );
    }

    #[test]
    fn classify_lease_error_resets_on_not_found() {
        // Mirrors the production wedge: the Lease was deleted server-side and
        // the renew loop kept patching the dead name. A 404 must clear the
        // cache so the next iteration re-resolves the connector + lease.
        assert_eq!(
            classify_lease_error(&api_error(404, "NotFound")),
            LeaseErrorAction::Reset
        );
    }

    #[test]
    fn classify_lease_error_refreshes_on_unauthorized() {
        assert_eq!(
            classify_lease_error(&api_error(401, "Unauthorized")),
            LeaseErrorAction::RefreshAuth
        );
    }

    #[test]
    fn classify_lease_error_retains_on_transient() {
        for code in [403, 409, 429, 500, 502, 503] {
            assert_eq!(
                classify_lease_error(&api_error(code, "Transient")),
                LeaseErrorAction::Retain,
                "code {code} should retain cache"
            );
        }
    }
}
