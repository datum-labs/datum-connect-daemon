use std::{fmt::Debug, net::SocketAddr, str::FromStr, sync::Arc, time::Duration};

use iroh::{
    Endpoint, EndpointId, SecretKey, discovery::dns::DnsDiscovery, endpoint::default_relay_mode,
    protocol::Router,
};
use iroh_base::RelayUrl;
use iroh_n0des::ApiSecret;
use iroh_proxy_utils::upstream::{TargetMetrics, UpstreamMetrics};
use iroh_proxy_utils::{
    ALPN as IROH_HTTP_CONNECT_ALPN, Authority, HttpProxyRequest, HttpProxyRequestKind,
};
use iroh_proxy_utils::{
    downstream::{DownstreamMetrics, DownstreamProxy, EndpointAuthority, ProxyMode},
    upstream::{AuthError, AuthHandler, UpstreamProxy},
};
use iroh_relay::dns::{DnsProtocol, DnsResolver};
use iroh_relay::{RelayConfig, RelayMap};
use n0_error::{Result, StackResultExt, StdResultExt};
use tokio::{
    net::TcpListener,
    sync::futures::Notified,
    task::{JoinHandle, JoinSet},
};
use tracing::{Instrument, debug, error_span, info, instrument, warn};

use crate::{Repo, StateWrapper, TcpProxyData, config::Config, state::ProxyState};

#[derive(Debug, Clone, Copy, Default)]
pub struct MetricsUpdate {
    pub send: u64,
    pub recv: u64,
}

#[derive(Debug, Clone)]
pub struct ListenNode {
    router: Router,
    state: StateWrapper,
    repo: Repo,
    metrics: Arc<UpstreamMetrics>,
    _n0des: Option<Arc<iroh_n0des::Client>>,
}

impl ListenNode {
    pub async fn new(repo: Repo) -> Result<Self> {
        let n0des_api_secret = n0des_api_secret_from_env()?;
        Self::build(repo, n0des_api_secret, None).await
    }

    /// Construct a listen node using a project-scoped iroh identity. The CLI
    /// Tunnel command takes this path so each project's Connector has a
    /// distinct iroh public key — see [`Repo::listen_key_for_project`] for
    /// why that matters.
    pub async fn new_for_project(repo: Repo, project_id: &str) -> Result<Self> {
        let n0des_api_secret = n0des_api_secret_from_env()?;
        Self::build(repo, n0des_api_secret, Some(project_id)).await
    }

    /// Construct a listen node using a pre-generated iroh identity.
    ///
    /// The key is NOT read from disk — it is used directly. Useful when the
    /// key is generated in memory (e.g., new tunnel creation) and needs to be
    /// passed through without a round-trip to disk.
    pub async fn new_with_key(repo: Repo, secret_key: SecretKey) -> Result<Self> {
        let n0des_api_secret = n0des_api_secret_from_env()?;
        Self::build_with_key(repo, n0des_api_secret, secret_key).await
    }

    /// Construct a listen node using a pre-generated iroh identity and an
    /// explicit relay mode, bypassing `DATUM_CONNECT_RELAY_URLS`/the
    /// built-in Datum relay shortlist entirely. For peers that must never
    /// route through Datum's own infrastructure at any layer (see
    /// `ConnectNode::new_with_relay_mode` and NOTES.md's app-to-app
    /// tunnels section) — e.g. `iroh::endpoint::default_relay_mode()` for
    /// iroh's own public relays.
    pub async fn new_with_key_and_relay_mode(
        repo: Repo,
        secret_key: SecretKey,
        relay_mode: iroh::endpoint::RelayMode,
    ) -> Result<Self> {
        let n0des_api_secret = n0des_api_secret_from_env()?;
        let config = repo.config().await?;
        let endpoint = build_endpoint_with_relay_mode(secret_key, &config, relay_mode).await?;
        let n0des = build_n0des_client_opt(&endpoint, n0des_api_secret).await;
        let state = repo.load_state().await?;

        let upstream_proxy = UpstreamProxy::new(state.clone())?;
        let metrics = upstream_proxy.metrics();

        let router = Router::builder(endpoint)
            .accept(IROH_HTTP_CONNECT_ALPN, upstream_proxy)
            .spawn();

        Ok(Self {
            repo,
            router,
            state,
            metrics,
            _n0des: n0des,
        })
    }

    #[instrument("listen-node", skip_all)]
    pub async fn with_n0des_api_secret(
        repo: Repo,
        n0des_api_secret: Option<ApiSecret>,
    ) -> Result<Self> {
        Self::build(repo, n0des_api_secret, None).await
    }

    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    #[instrument("listen-node", skip(repo, n0des_api_secret))]
    async fn build(
        repo: Repo,
        n0des_api_secret: Option<ApiSecret>,
        project_id: Option<&str>,
    ) -> Result<Self> {
        let config = repo.config().await?;
        let secret_key = match project_id {
            Some(pid) => repo.listen_key_for_project(pid).await?,
            None => repo.listen_key(project_id).await?,
        };
        let endpoint = build_endpoint(secret_key, &config).await?;
        let n0des = build_n0des_client_opt(&endpoint, n0des_api_secret).await;
        let state = repo.load_state().await?;

        let upstream_proxy = UpstreamProxy::new(state.clone())?;
        let metrics = upstream_proxy.metrics();

        let router = Router::builder(endpoint)
            .accept(IROH_HTTP_CONNECT_ALPN, upstream_proxy)
            .spawn();

        let this = Self {
            repo,
            router,
            state,
            metrics,
            _n0des: n0des,
        };
        Ok(this)
    }

    #[instrument("listen-node", skip(repo, n0des_api_secret, secret_key))]
    async fn build_with_key(
        repo: Repo,
        n0des_api_secret: Option<ApiSecret>,
        secret_key: SecretKey,
    ) -> Result<Self> {
        let config = repo.config().await?;
        let endpoint = build_endpoint(secret_key, &config).await?;
        let n0des = build_n0des_client_opt(&endpoint, n0des_api_secret).await;
        let state = repo.load_state().await?;

        let upstream_proxy = UpstreamProxy::new(state.clone())?;
        let metrics = upstream_proxy.metrics();

        let router = Router::builder(endpoint)
            .accept(IROH_HTTP_CONNECT_ALPN, upstream_proxy)
            .spawn();

        Ok(Self {
            repo,
            router,
            state,
            metrics,
            _n0des: n0des,
        })
    }

    pub fn state_updated(&self) -> Notified<'_> {
        self.state.updated()
    }

    pub fn state(&self) -> &StateWrapper {
        &self.state
    }

    pub fn metrics(&self) -> &Arc<UpstreamMetrics> {
        &self.metrics
    }

    /// Per-target byte/request metrics for one advertised target, keyed the
    /// same way the underlying proxy already keys them internally — callers
    /// (e.g. the peer-tunnel API) never need to know `Authority` exists.
    /// `None` if nothing has been recorded for this target yet.
    pub fn metrics_for(&self, data: &TcpProxyData) -> Option<Arc<TargetMetrics>> {
        self.metrics.get(&Authority::from(data.clone()))
    }

    pub fn proxies(&self) -> Vec<ProxyState> {
        self.state.get().proxies.to_vec()
    }

    pub fn proxy_by_id(&self, id: &str) -> Option<ProxyState> {
        self.state
            .get()
            .proxies
            .iter()
            .find(|p| p.id() == id)
            .cloned()
    }

    pub async fn set_proxy(&self, proxy: ProxyState) -> Result<()> {
        self.state
            .update(&self.repo, |state| state.set_proxy(proxy.clone()))
            .await?;
        Ok(())
    }

    pub async fn set_proxy_state(&self, proxy: ProxyState) -> Result<()> {
        self.state
            .update(&self.repo, |state| state.set_proxy(proxy))
            .await?;
        Ok(())
    }

    pub async fn remove_proxy(&self, resource_id: &str) -> Result<Option<ProxyState>> {
        debug!(%resource_id, "removing proxy {resource_id}");
        let res = self
            .state
            .update(&self.repo, move |state| state.remove_proxy(resource_id))
            .await;
        debug!(%resource_id, "removed {res:?}");
        res
    }

    pub async fn remove_proxy_state(&self, resource_id: &str) -> Result<Option<ProxyState>> {
        debug!(%resource_id, "removing proxy state {resource_id}");
        let res = self
            .state
            .update(&self.repo, move |state| state.remove_proxy(resource_id))
            .await;
        debug!(%resource_id, "removed {res:?}");
        res
    }

    pub fn endpoint(&self) -> &Endpoint {
        self.router.endpoint()
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.router.endpoint().id()
    }
}

impl StateWrapper {
    fn tcp_proxy_exists(&self, host: &str, port: u16) -> bool {
        let normalized_host = normalize_loopback(strip_host_scheme(host));
        let exists = self.get().proxies.iter().any(|a| {
            a.enabled
                && normalize_loopback(&a.info.service().host) == normalized_host
                && a.info.service().port == port
        });
        if !exists {
            debug!(
                requested_host = host,
                normalized_host, port, "tcp_proxy_exists: no matching proxy found"
            );
        }
        exists
    }
}

fn strip_host_scheme(host: &str) -> &str {
    host.strip_prefix("http://")
        .or_else(|| host.strip_prefix("https://"))
        .unwrap_or(host)
}

fn normalize_loopback(host: &str) -> &str {
    match host {
        "localhost" | "::1" => "127.0.0.1",
        _ => host,
    }
}

impl AuthHandler for StateWrapper {
    async fn authorize<'a>(
        &'a self,
        _remote_id: EndpointId,
        req: &'a HttpProxyRequest,
    ) -> Result<(), AuthError> {
        match &req.kind {
            HttpProxyRequestKind::Tunnel { target } => {
                if self.tcp_proxy_exists(&target.host, target.port) {
                    Ok(())
                } else {
                    Err(AuthError::Forbidden)
                }
            }
            HttpProxyRequestKind::Absolute { target, .. } => {
                if let Ok(authority) = Authority::from_absolute_uri(&target) {
                    if self.tcp_proxy_exists(&authority.host, authority.port) {
                        Ok(())
                    } else {
                        Err(AuthError::Forbidden)
                    }
                } else {
                    debug!(%target, "failed to parse host:port from absolute URL");
                    Err(AuthError::Forbidden)
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConnectNode {
    endpoint: Endpoint,
    proxy: DownstreamProxy,
    _n0des: Option<Arc<iroh_n0des::Client>>,
}

impl ConnectNode {
    pub async fn new(repo: Repo) -> Result<Self> {
        let n0des_api_secret = n0des_api_secret_from_env()?;
        Self::with_n0des_api_secret(repo, n0des_api_secret).await
    }

    #[instrument("connect-node", skip_all)]
    pub async fn with_n0des_api_secret(
        repo: Repo,
        n0des_api_secret: Option<ApiSecret>,
    ) -> Result<Self> {
        let config = repo.config().await?;
        let secret_key = repo.connect_key().await?;
        let endpoint = build_endpoint(secret_key, &config).await?;
        let n0des = build_n0des_client_opt(&endpoint, n0des_api_secret).await;
        let pool = DownstreamProxy::new(endpoint.clone(), Default::default());
        Ok(Self {
            endpoint,
            _n0des: n0des,
            proxy: pool,
        })
    }

    /// Construct a connect node with an explicit relay mode, bypassing
    /// `DATUM_CONNECT_RELAY_URLS`/the built-in Datum relay shortlist
    /// entirely — see `ListenNode::new_with_key_and_relay_mode`'s doc
    /// comment for why this exists (app-to-app peer tunnels must not
    /// touch Datum's infrastructure at any layer, including relay choice).
    pub async fn new_with_relay_mode(
        repo: Repo,
        relay_mode: iroh::endpoint::RelayMode,
    ) -> Result<Self> {
        let n0des_api_secret = n0des_api_secret_from_env()?;
        let config = repo.config().await?;
        let secret_key = repo.connect_key().await?;
        let endpoint = build_endpoint_with_relay_mode(secret_key, &config, relay_mode).await?;
        let n0des = build_n0des_client_opt(&endpoint, n0des_api_secret).await;
        let pool = DownstreamProxy::new(endpoint.clone(), Default::default());
        Ok(Self {
            endpoint,
            _n0des: n0des,
            proxy: pool,
        })
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// The raw iroh endpoint — gives callers access to `conn_type()`/
    /// `latency()` per remote peer (direct-vs-relay, RTT), mirroring
    /// `ListenNode::endpoint()`.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Aggregate byte/connection metrics across every outbound connection
    /// this node has ever made — the underlying proxy pool doesn't track
    /// bytes per individual connection, only in total, so this is the
    /// finest-grained view available for the connect/dial side (contrast
    /// with `ListenNode::metrics_for`, which genuinely is per-target).
    pub fn metrics(&self) -> &Arc<DownstreamMetrics> {
        self.proxy.metrics()
    }

    pub async fn connect_and_bind_local(
        &self,
        remote_id: EndpointId,
        advertisment: &TcpProxyData,
        bind_addr: SocketAddr,
    ) -> Result<OutboundProxyHandle> {
        let local_socket = TcpListener::bind(bind_addr).await?;
        let bound_addr = local_socket.local_addr()?;

        let upstream = EndpointAuthority::new(remote_id, advertisment.clone().into());
        let mode = ProxyMode::Tcp(upstream);

        let proxy = self.proxy.clone();
        let task = tokio::spawn(async move {
            info!("bound local socket on {bound_addr}");
            if let Err(err) = proxy.forward_tcp_listener(local_socket, mode).await {
                warn!("Forwarding local socket failed: {err:#}");
            }
        }.instrument(error_span!("forward-tcp", remote_id=%remote_id.fmt_short(), authority=%advertisment.address())));
        Ok(OutboundProxyHandle {
            remote_id,
            task,
            // The resolved address, not the input `bind_addr` — when the
            // caller passes an ephemeral-port wildcard (e.g. "127.0.0.1:0",
            // the default for both our daemon's peer-connect endpoint and
            // its CLI), `bind_addr` still has port 0 in it, which is not a
            // connectable address. `local_socket.local_addr()` (already
            // computed above as `bound_addr`) has the real assigned port.
            bound_addr,
            advertisment: advertisment.clone(),
        })
    }
}

pub struct OutboundProxyHandle {
    task: JoinHandle<()>,
    bound_addr: SocketAddr,
    remote_id: EndpointId,
    advertisment: TcpProxyData,
}

impl OutboundProxyHandle {
    pub fn abort(&self) {
        self.task.abort();
    }

    pub fn remote_id(&self) -> EndpointId {
        self.remote_id
    }

    pub fn bound_addr(&self) -> SocketAddr {
        self.bound_addr
    }

    pub fn advertisment(&self) -> &TcpProxyData {
        &self.advertisment
    }
}

pub async fn build_endpoint(secret_key: SecretKey, common: &Config) -> Result<Endpoint> {
    let relay_mode = relay_mode_from_env_or_build().await?;
    build_endpoint_with_relay_mode(secret_key, common, relay_mode).await
}

/// Same as `build_endpoint`, but with the relay mode passed in directly
/// instead of resolved from `DATUM_CONNECT_RELAY_URLS`/the built-in Datum
/// shortlist — for callers that must not route through Datum's relay
/// infrastructure regardless of environment configuration.
pub async fn build_endpoint_with_relay_mode(
    secret_key: SecretKey,
    common: &Config,
    relay_mode: iroh::endpoint::RelayMode,
) -> Result<Endpoint> {
    let mut builder = match common.discovery_mode {
        crate::config::DiscoveryMode::Dns => {
            Endpoint::empty_builder(relay_mode).secret_key(secret_key)
        }
        crate::config::DiscoveryMode::Default | crate::config::DiscoveryMode::Hybrid => {
            Endpoint::builder()
                .relay_mode(relay_mode)
                .secret_key(secret_key)
        }
    };
    if let Some(addr) = common.ipv4_addr {
        builder = builder.bind_addr_v4(addr);
    }
    if let Some(addr) = common.ipv6_addr {
        builder = builder.bind_addr_v6(addr);
    }
    // Applies regardless of discovery_mode — relay and discovery hostname
    // resolution both go through this resolver. Previously only wired up
    // for Dns/Hybrid mode, but the default n0des discovery path needs the
    // same override: on some hosts (seen on Windows) iroh's default
    // hickory-resolver auto-detected system config times out reaching
    // relay hosts and the default discovery host, even though the OS's own
    // resolver reaches them fine. Pointing at an explicit nameserver (e.g.
    // 1.1.1.1) works around that.
    if let Some(resolver_addr) = common.dns_resolver {
        let resolver = DnsResolver::builder()
            .with_nameserver(resolver_addr, DnsProtocol::Udp)
            .build();
        builder = builder.dns_resolver(resolver);
    }
    match common.discovery_mode {
        crate::config::DiscoveryMode::Default => {}
        crate::config::DiscoveryMode::Dns | crate::config::DiscoveryMode::Hybrid => {
            let origin = match &common.dns_origin {
                Some(origin) => origin.clone(),
                None => n0_error::bail_any!(
                    "dns_origin is required when discovery_mode is set to dns or hybrid"
                ),
            };
            builder = builder.discovery(DnsDiscovery::builder(origin));
        }
    }
    let endpoint = builder.bind().await?;
    info!(id = %endpoint.id(), "iroh endpoint bound");
    Ok(endpoint)
}

const DATUM_CONNECT_RELAY_URLS: &str = "DATUM_CONNECT_RELAY_URLS";
const BUILD_DATUM_CONNECT_RELAY_URLS: &str = "BUILD_DATUM_CONNECT_RELAY_URLS";
const STARTUP_RELAY_SELECTION_MAX: usize = 5;
const STARTUP_RELAY_PROBE_TIMEOUT: Duration = Duration::from_millis(800);

/// Built-in Datum relay shortlist. Used when neither the runtime env
/// `DATUM_CONNECT_RELAY_URLS` nor the compile-time env
/// `BUILD_DATUM_CONNECT_RELAY_URLS` is set. Ensures stock `cargo build` /
/// `nix run` / IDE builds reach a Datum-routable relay network instead of
/// silently falling through to the n0 public relays (which the Datum
/// gateway cannot route through).
const DEFAULT_DATUM_RELAY_URLS: &str =
    "iroh-relay.us-east-1.datumconnect.net,iroh-relay.us-west-1.datumconnect.net";

/// Resolve the iroh relay set with explicit precedence:
///   1. runtime env `DATUM_CONNECT_RELAY_URLS` (operator override)
///   2. compile-time env `BUILD_DATUM_CONNECT_RELAY_URLS` (CI-injected list)
///   3. built-in `DEFAULT_DATUM_RELAY_URLS` shortlist
///   4. iroh's `default_relay_mode()` — n0 public/canary relays. Reaching this
///      branch means the Datum gateway will not be able to dial this endpoint.
async fn relay_mode_from_env_or_build() -> Result<iroh::endpoint::RelayMode> {
    if let Ok(raw_urls) = std::env::var(DATUM_CONNECT_RELAY_URLS) {
        match parse_relay_urls(&raw_urls) {
            Ok(relays) => {
                let relays =
                    select_best_relays_for_startup(relays, STARTUP_RELAY_SELECTION_MAX).await;
                info!(
                    source = %DATUM_CONNECT_RELAY_URLS,
                    count = relays.len(),
                    "using custom iroh relay list from environment"
                );
                return Ok(iroh::endpoint::RelayMode::Custom(relays_to_map(relays)));
            }
            Err(err) => {
                warn!("invalid relay urls in {DATUM_CONNECT_RELAY_URLS}: {err:#}");
            }
        }
    }

    if let Some(raw_urls) = option_env!("BUILD_DATUM_CONNECT_RELAY_URLS") {
        match parse_relay_urls(raw_urls) {
            Ok(relays) => {
                let relays =
                    select_best_relays_for_startup(relays, STARTUP_RELAY_SELECTION_MAX).await;
                info!(
                    source = %BUILD_DATUM_CONNECT_RELAY_URLS,
                    count = relays.len(),
                    "using custom iroh relay list from build environment"
                );
                return Ok(iroh::endpoint::RelayMode::Custom(relays_to_map(relays)));
            }
            Err(err) => {
                warn!("invalid relay urls in {BUILD_DATUM_CONNECT_RELAY_URLS}: {err:#}");
            }
        }
    }

    match parse_relay_urls(DEFAULT_DATUM_RELAY_URLS) {
        Ok(relays) => {
            let relays = select_best_relays_for_startup(relays, STARTUP_RELAY_SELECTION_MAX).await;
            info!(
                source = "built-in",
                count = relays.len(),
                "using built-in Datum relay shortlist"
            );
            return Ok(iroh::endpoint::RelayMode::Custom(relays_to_map(relays)));
        }
        Err(err) => {
            warn!("invalid built-in DEFAULT_DATUM_RELAY_URLS, this is a bug: {err:#}");
        }
    }

    warn!(
        "Falling back to iroh's default public relays (n0). The Datum gateway \
         cannot route through this relay network — inbound connections to this \
         endpoint will fail. Set DATUM_CONNECT_RELAY_URLS or fix \
         DEFAULT_DATUM_RELAY_URLS."
    );
    Ok(default_relay_mode())
}

fn parse_relay_urls(raw: &str) -> Result<Vec<RelayUrl>> {
    let relays: Vec<RelayUrl> = raw
        .split(|c: char| c == ',' || c == ';' || c.is_ascii_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(normalize_relay_url)
        .map(|url| RelayUrl::from_str(&url))
        .collect::<std::result::Result<Vec<_>, _>>()
        .std_context(
            "Failed parsing relay URL list. Expected comma/space/newline separated URLs",
        )?;

    if relays.is_empty() {
        n0_error::bail_any!("Relay URL list was provided but empty after parsing");
    }

    let mut deduped = Vec::with_capacity(relays.len());
    for relay in relays {
        if !deduped.iter().any(|seen: &RelayUrl| seen == &relay) {
            deduped.push(relay);
        }
    }
    Ok(deduped)
}

fn normalize_relay_url(raw: &str) -> String {
    if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    }
}

async fn select_best_relays_for_startup(relays: Vec<RelayUrl>, max_relays: usize) -> Vec<RelayUrl> {
    let total_candidates = relays.len();
    if relays.len() <= max_relays {
        return relays;
    }

    let client = match reqwest::Client::builder()
        .timeout(STARTUP_RELAY_PROBE_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            warn!("relay probe setup failed, using first {max_relays} relays: {err:#}");
            return relays.into_iter().take(max_relays).collect();
        }
    };

    let mut joinset = JoinSet::new();
    for relay in relays.iter().cloned() {
        let client = client.clone();
        joinset.spawn(async move {
            let latency = probe_relay_latency(&client, &relay).await;
            (relay, latency)
        });
    }

    let mut successful = Vec::new();
    let mut failed = Vec::new();
    while let Some(joined) = joinset.join_next().await {
        match joined {
            Ok((relay, Ok(latency))) => successful.push((relay, latency)),
            Ok((relay, Err(reason))) => failed.push((relay, reason)),
            Err(err) => {
                debug!("relay probe task join error: {err:#}");
            }
        }
    }

    successful.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.as_str().cmp(b.0.as_str())));
    let mut selected: Vec<RelayUrl> = successful
        .iter()
        .take(max_relays)
        .map(|(relay, _)| relay.clone())
        .collect();

    if selected.len() < max_relays {
        failed.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        for (relay, _) in &failed {
            if selected.len() == max_relays {
                break;
            }
            if !selected.iter().any(|r| r == relay) {
                selected.push(relay.clone());
            }
        }
    }

    if selected.len() < max_relays {
        for relay in relays {
            if selected.len() == max_relays {
                break;
            }
            if !selected.iter().any(|r| r == &relay) {
                selected.push(relay);
            }
        }
    }

    if !failed.is_empty() {
        let failure_samples: Vec<String> = failed
            .iter()
            .take(5)
            .map(|(relay, reason)| format!("{relay} -> {reason}"))
            .collect();
        warn!(
            failed = failed.len(),
            samples = ?failure_samples,
            "relay ping probe failures observed"
        );
    }
    info!(
        total = total_candidates,
        successful = successful.len(),
        selected = selected.len(),
        selected_relays = ?selected,
        "selected startup relay shortlist"
    );
    selected
}

async fn probe_relay_latency(
    client: &reqwest::Client,
    relay: &RelayUrl,
) -> std::result::Result<Duration, String> {
    let host = relay
        .host_str()
        .ok_or_else(|| "missing host in relay url".to_string())?
        .trim_end_matches('.');
    let mut https_url = reqwest::Url::parse(&format!("https://{host}/ping"))
        .map_err(|err| format!("url parse: {err}"))?;
    https_url.set_query(None);
    debug!(
        relay = %relay,
        url = %https_url,
        timeout_ms = STARTUP_RELAY_PROBE_TIMEOUT.as_millis(),
        "starting relay ping probe"
    );
    let start = tokio::time::Instant::now();
    match client.get(https_url.clone()).send().await {
        Ok(resp) if resp.status().is_success() => {
            let elapsed = start.elapsed();
            debug!(
                relay = %relay,
                url = %https_url,
                status = %resp.status(),
                elapsed_ms = elapsed.as_millis(),
                "relay ping probe succeeded"
            );
            Ok(elapsed)
        }
        Ok(resp) => {
            debug!(
                relay = %relay,
                url = %https_url,
                status = %resp.status(),
                elapsed_ms = start.elapsed().as_millis(),
                "relay ping probe got non-success response"
            );
            Err(format!("status {}", resp.status()))
        }
        Err(err) => {
            debug!(
                relay = %relay,
                url = %https_url,
                elapsed_ms = start.elapsed().as_millis(),
                "relay ping probe request failed: {err:#}"
            );
            Err(format!("{err:#}"))
        }
    }
}

fn relays_to_map(relays: Vec<RelayUrl>) -> RelayMap {
    RelayMap::from_iter(relays.into_iter().map(RelayConfig::from))
}

pub(crate) fn n0des_api_secret_from_env() -> Result<Option<ApiSecret>> {
    let api_secret_str = match std::env::var("N0DES_API_SECRET") {
        Ok(s) => s,
        Err(_) => match option_env!("BUILD_N0DES_API_SECRET") {
            None => return Ok(None),
            Some(s) => s.to_string(),
        },
    };
    let api_secret = ApiSecret::from_str(&api_secret_str)
        .context("Failed to parse n0des API secret from env variable N0DES_API_SECRET")?;
    Ok(Some(api_secret))
}

pub(crate) async fn build_n0des_client_opt(
    endpoint: &Endpoint,
    api_secret: Option<ApiSecret>,
) -> Option<Arc<iroh_n0des::Client>> {
    match api_secret {
        None => {
            info!("Disabling metrics collection: N0DES_API_SECRET is not set");
            None
        }
        Some(n0des_api_secret) => {
            let remote_id = n0des_api_secret.remote.id;
            debug!(remote = %remote_id.fmt_short(), "connecting to n0des endpoint");
            let builder = match iroh_n0des::Client::builder(endpoint)
                .api_secret(n0des_api_secret)
            {
                Ok(b) => b,
                Err(err) => {
                    warn!("Disabling metrics collection: Failed to build n0des client: {err:#}");
                    return None;
                }
            };
            match builder.build().await.std_context("Failed to connect to n0des endpoint") {
                Ok(client) => {
                    info!(remote = %remote_id.fmt_short(), "Connected to n0des endpoint for metrics collection");
                    Some(Arc::new(client))
                }
                Err(err) => {
                    warn!("Disabling metrics collection: Failed to connect to n0des: {err:#}");
                    None
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_default_relay_list_parses() {
        let parsed = parse_relay_urls(DEFAULT_DATUM_RELAY_URLS)
            .expect("DEFAULT_DATUM_RELAY_URLS must parse — guards the runtime fallback path");
        assert!(
            !parsed.is_empty(),
            "DEFAULT_DATUM_RELAY_URLS must yield at least one relay"
        );
        for relay in &parsed {
            assert_eq!(relay.scheme(), "https");
        }
    }

    /// End-to-end app-to-app (peer-to-peer) tunnel: two independent
    /// identities, no Datum Cloud client, no project, no HTTPProxy/
    /// Connector resource, and no Datum relay servers (explicit
    /// `default_relay_mode()` — iroh's own public relays only). This is
    /// the mechanism NOTES.md's "App-to-app tunnels" section documents as
    /// already existing in this crate; this test proves it, not just that
    /// it compiles.
    #[tokio::test]
    async fn app_to_app_tunnel_forwards_real_tcp_traffic() {
        use crate::state::{Advertisment, ProxyState, TcpProxyData};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A real local target: a one-shot TCP echo server on an ephemeral port.
        let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target_listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = target_listener.accept().await.unwrap();
            let mut buf = [0u8; 5];
            sock.read_exact(&mut buf).await.unwrap();
            sock.write_all(&buf).await.unwrap();
        });

        // "Server" side: advertise the target under a stable, freshly
        // generated identity — no disk-persisted key needed for this test.
        let server_repo = Repo::open_or_create(std::env::temp_dir().join(format!(
            "app-to-app-server-{}",
            uuid::Uuid::new_v4()
        )))
        .await
        .unwrap();
        let server_key = SecretKey::generate(&mut rand::rng());
        let server_node = ListenNode::new_with_key_and_relay_mode(
            server_repo,
            server_key,
            iroh::endpoint::default_relay_mode(),
        )
        .await
        .unwrap();

        // Give the server's endpoint a moment to register with a relay
        // before anyone tries to dial it — same wait `start_tunnel` in the
        // daemon already does for exactly this reason.
        for _ in 0..40 {
            if server_node.endpoint().addr().relay_urls().next().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }

        let data = TcpProxyData {
            host: target_addr.ip().to_string(),
            port: target_addr.port(),
        };
        let advertisment = Advertisment::new(data, Some("test-target".to_string()));
        server_node
            .set_proxy(ProxyState::new(advertisment.clone()))
            .await
            .unwrap();
        let ticket = advertisment.ticket(server_node.endpoint_id());

        // Ticket round-trips through its portable string form exactly like
        // it would over our daemon's HTTP API / a pasted CLI argument.
        let ticket_string = ticket.to_ticket_string();
        let ticket: crate::state::AdvertismentTicket = ticket_string.parse().unwrap();

        // "Client" side: a completely independent identity/repo, dialing
        // the server's EndpointId directly.
        let client_repo = Repo::open_or_create(std::env::temp_dir().join(format!(
            "app-to-app-client-{}",
            uuid::Uuid::new_v4()
        )))
        .await
        .unwrap();
        let client_node =
            ConnectNode::new_with_relay_mode(client_repo, iroh::endpoint::default_relay_mode())
                .await
                .unwrap();

        let handle = client_node
            .connect_and_bind_local(
                ticket.endpoint,
                ticket.service(),
                "127.0.0.1:0".parse().unwrap(),
            )
            .await
            .unwrap();

        // Real TCP traffic through the whole path: local socket -> iroh ->
        // server's ListenNode -> real local target -> echoed back.
        let mut client_sock = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tokio::net::TcpStream::connect(handle.bound_addr()),
        )
        .await
        .expect("connecting to the forwarded local port timed out")
        .unwrap();
        client_sock.write_all(b"hello").await.unwrap();
        let mut echoed = [0u8; 5];
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            client_sock.read_exact(&mut echoed),
        )
        .await
        .expect("reading the echo back through the peer tunnel timed out")
        .unwrap();
        assert_eq!(&echoed, b"hello");

        handle.abort();
    }

    #[tokio::test]
    async fn new_with_key_uses_provided_key_without_disk_read() {
        let tmp = std::env::temp_dir();
        let dir = tmp.join(format!("node-test-{}", uuid::Uuid::new_v4()));
        let repo = Repo::open_or_create(&dir).await.unwrap();

        // Generate a key in memory.
        let key = SecretKey::generate(&mut rand::rng());
        // Derive expected EndpointId by creating a temporary endpoint.
        let expected_id = {
            let ep = iroh::Endpoint::builder()
                .relay_mode(iroh::endpoint::RelayMode::Default)
                .secret_key(key.clone())
                .bind()
                .await
                .unwrap();
            ep.id()
        };

        // new_with_key should use the key directly (no disk read needed).
        let node = ListenNode::new_with_key(repo, key).await.unwrap();

        // The endpoint ID must match the provided key's derived ID.
        assert_eq!(
            node.endpoint_id(),
            expected_id,
            "endpoint ID must match the provided key"
        );
    }
}
