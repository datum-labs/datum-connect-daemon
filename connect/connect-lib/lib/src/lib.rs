pub mod config;
pub mod datum_apis;
pub mod datum_cloud;
pub mod heartbeat;
pub mod http_user_agent;
pub mod kube_error;
pub mod node;
pub mod project_control_plane;
pub mod repo;
pub mod state;
pub mod tunnels;

#[cfg(test)]
pub mod test_util;

pub use config::{Config, DiscoveryMode};
pub use datum_cloud::external_token_source::{ExternalTokenError, ExternalTokenSource};
pub use datum_cloud::{ApiEnv, AuthState, AuthTokens, LoginState, MaybeAuth, UserProfile};
pub use heartbeat::HeartbeatAgent;
pub use http_user_agent::datum_http_user_agent;
pub use node::{build_endpoint, ConnectNode, ListenNode, OutboundProxyHandle};
pub use project_control_plane::ProjectControlPlaneClient;
pub use repo::{MissingConnectDir, Repo};
pub use state::{
    Advertisment, AdvertismentTicket, ProxyState, SelectedContext, State, StateWrapper, TcpProxyData,
};
pub use tunnels::{
    friendly_device_name, normalize_endpoint, OrphanedConnector, ProgressStep, ProgressStepKind,
    StepStatus, TunnelDeleteOutcome, TunnelProgress, TunnelService, TunnelSummary,
};

/// The root domain for datum connect URLs to subdomain from. A proxy URL will
/// be a three-word-codename subdomain off this URL. eg: "https://vast-gold-mine.iroh.datum.net"
pub const DATUM_CONNECT_GATEWAY_DOMAIN_NAME: &str = "iroh.datum.net";

/// Kubernetes namespace used for Connector, HTTPProxy, and related CRDs.
pub const DEFAULT_PCP_NAMESPACE: &str = "default";

/// Serializes env-dependent tests (std::env::set_var is not thread-safe).
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
