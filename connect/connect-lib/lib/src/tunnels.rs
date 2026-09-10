use std::collections::BTreeMap;

use iroh_proxy_utils::Authority;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams};
use kube::{Api, ResourceExt};
use n0_error::{Result, StdResultExt, StackResultExt};
use serde::Serialize;
use serde_json::json;
use tracing::{debug, warn};

use crate::datum_apis::connector::{
    CONNECTOR_CONDITION_IROH_DNS_PUBLISHED, CONNECTOR_CONDITION_READY,
    CONNECTOR_REASON_DEFERRED_TO_OWNER, Connector, ConnectorConnectionDetails,
    ConnectorConnectionDetailsPublicKey, ConnectorConnectionType, ConnectorSpec,
    PublicKeyConnectorAddress, PublicKeyDiscoveryMode,
};
use crate::datum_apis::connector_class::ConnectorClass;
use crate::datum_apis::connector_advertisement::{
    ConnectorAdvertisement, ConnectorAdvertisementLayer4, ConnectorAdvertisementLayer4Service,
    ConnectorAdvertisementSpec, Layer4ServiceAddress, Layer4ServicePort, Protocol,
};
use crate::datum_apis::http_proxy::{
    ConnectorReference, HTTP_PROXY_CONDITION_ACCEPTED, HTTP_PROXY_CONDITION_CERTIFICATES_READY,
    HTTP_PROXY_CONDITION_CONNECTOR_METADATA_PROGRAMMED, HTTP_PROXY_CONDITION_PROGRAMMED, HTTPProxy,
    HTTPProxyRule, HTTPProxyRuleBackend, HTTPProxySpec, HTTPRouteMatch,
    HTTPRouteRulesFiltersType, HTTPRouteRulesMatchesHeaders, HTTPRouteRulesMatchesHeadersType,
    HTTPRouteRulesMatchesPath, HTTPRouteRulesMatchesPathType,
};
use crate::datum_apis::traffic_protection_policy::{
    LocalPolicyTargetReferenceWithSectionName, OWASPCRS, ParanoiaLevels, TrafficProtectionPolicy,
    TrafficProtectionPolicyMode, TrafficProtectionPolicyRuleSet,
    TrafficProtectionPolicyRuleSetType, TrafficProtectionPolicySpec,
};
use crate::datum_cloud::DatumCloudClient;
use crate::kube_error::is_quota_check_timeout;
use crate::{DEFAULT_PCP_NAMESPACE, Advertisment, ListenNode, TcpProxyData, state::ProxyState};
const DEFAULT_CONNECTOR_CLASS_NAME: &str = "datum-connect";
const CONNECTOR_SELECTOR_FIELD: &str = "status.connectionDetails.publicKey.id";
const ADVERTISEMENT_CONNECTOR_FIELD: &str = "spec.connectorRef.name";
const DISPLAY_NAME_ANNOTATION: &str = "app.kubernetes.io/name";

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TunnelSummary {
    pub id: String,
    pub label: String,
    pub endpoint: String,
    pub hostnames: Vec<String>,
    pub enabled: bool,
    pub accepted: bool,
    pub programmed: bool,
    pub connector_metadata_programmed: bool,
    /// True when the backing Connector's `Ready` condition is `True`.
    /// False means the connector lease has expired and the tunnel agent is
    /// offline (traffic will be dropped).
    pub connector_ready: bool,
    /// The name of the Connector resource backing this tunnel.
    pub connector_name: Option<String>,
    /// Device name from the Connector's `datum.net/device-name` annotation.
    pub connector_device: Option<String>,
}

/// A Connector that exists in the project but is not referenced by any tunnel.
/// These are typically left over from a previous tunnel that was deleted without
/// a clean shutdown (e.g. the agent was killed before it could run cleanup).
#[derive(Debug, Clone, PartialEq)]
pub struct OrphanedConnector {
    pub name: String,
    /// True when the connector's `Ready` condition is `True`.
    pub ready: bool,
    /// Device name from the Connector's `datum.net/device-name` annotation.
    pub device: Option<String>,
}

impl TunnelSummary {
    pub fn origin_authority(&self) -> Option<Authority> {
        TcpProxyData::from_host_port_str(&strip_scheme(&self.endpoint))
            .ok()
            .map(Authority::from)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TunnelDeleteOutcome {
    pub project_id: String,
    pub http_proxy: Option<String>,
    pub connector_ad: Option<String>,
    pub traffic_protection_policy: Option<String>,
    pub connector: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TunnelService {
    datum: DatumCloudClient,
    listen: ListenNode,
    publish_tickets: bool,
    create_traffic_protection_policies: bool,
}

fn proxy_state_from_summary(
    tunnel_id: &str,
    endpoint: &str,
    label: &str,
    enabled: bool,
) -> Result<ProxyState> {
    let data = TcpProxyData::from_host_port_str(&strip_scheme(endpoint))?;
    let info = Advertisment::with_id(tunnel_id.to_string(), data, Some(label.to_string()));
    Ok(ProxyState { info, enabled })
}

/// Returns whether the named condition is present with status "True".
/// If `require_true` is false, returns true when the condition is absent
/// (used for ConnectorMetadataProgrammed, which is intentionally unset in
/// extension-server mode — absence means the extension server manages
/// xDS injection directly).
fn condition_status(
    conditions: Option<&[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition]>,
    kind: &str,
    require_true: bool,
) -> bool {
    conditions
        .unwrap_or_default()
        .iter()
        .find(|c| c.type_ == kind)
        .map(|c| c.status == "True")
        .unwrap_or(!require_true)
}

/// One checkpoint in the tunnel setup pipeline. Maps 1:1 to a controller
/// condition; the order roughly tracks how a healthy setup progresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressStepKind {
    /// HTTPProxy `Accepted` — control plane accepted the resource.
    ProxyAccepted,
    /// HTTPProxy `CertificatesReady` — TLS certs issued for the hostname.
    CertificatesReady,
    /// Connector `Ready` — agent is online and renewing its lease.
    ConnectorReady,
    /// Connector `IrohDNSPublished` — iroh DNS record published. The
    /// failure-with-`DeferredToOwner` case is the silent-tunnel failure
    /// that signals cross-project iroh-key collision.
    IrohDnsPublished,
    /// HTTPProxy `Programmed` — edge actually programmed the route.
    ProxyProgrammed,
    /// HTTPProxy `ConnectorMetadataProgrammed` — Envoy has the iroh metadata
    /// it needs to dial the connector.
    ConnectorMetadataProgrammed,
}

impl ProgressStepKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::ProxyAccepted => "tunnel accepted",
            Self::CertificatesReady => "TLS certificate issued",
            Self::ConnectorReady => "connector ready",
            Self::IrohDnsPublished => "iroh DNS published",
            Self::ProxyProgrammed => "route programmed",
            Self::ConnectorMetadataProgrammed => "envoy metadata propagated",
        }
    }

    pub fn all() -> &'static [ProgressStepKind] {
        &[
            Self::ProxyAccepted,
            Self::CertificatesReady,
            Self::ConnectorReady,
            Self::IrohDnsPublished,
            Self::ProxyProgrammed,
            Self::ConnectorMetadataProgrammed,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Controller hasn't reported on this condition yet.
    Unknown,
    /// Condition exists with status False — still waiting (or failing).
    Pending,
    /// Condition is True.
    Ready,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProgressStep {
    pub kind: ProgressStepKind,
    pub status: StepStatus,
    pub reason: Option<String>,
    pub message: Option<String>,
    /// Pre-formatted "Kind/name" of the underlying Kubernetes resource
    /// (`HTTPProxy/<tunnel_id>` or `Connector/<connector_name>`). The CLI
    /// renders this alongside each step so the user can pivot to
    /// `datumctl describe ...` on the exact resource that's stuck or
    /// reporting a stale Ready. `None` only when the resource doesn't
    /// exist server-side (e.g. probing for a tunnel id that's not there).
    pub resource: Option<String>,
}

impl ProgressStepKind {
    /// The Kubernetes resource kind whose conditions back this step.
    pub fn resource_kind(&self) -> &'static str {
        match self {
            Self::ConnectorReady | Self::IrohDnsPublished => "Connector",
            Self::ProxyAccepted
            | Self::CertificatesReady
            | Self::ProxyProgrammed
            | Self::ConnectorMetadataProgrammed => "HTTPProxy",
        }
    }
}

impl ProgressStep {
    /// True if this step is in a terminal failure mode that won't self-heal
    /// without user action. The canonical case is the iroh DNS owner
    /// collision: another Connector with the same iroh key owns the record,
    /// and waiting longer won't change that.
    pub fn is_terminal_failure(&self) -> bool {
        matches!(self.kind, ProgressStepKind::IrohDnsPublished)
            && self.status == StepStatus::Pending
            && self.reason.as_deref() == Some(CONNECTOR_REASON_DEFERRED_TO_OWNER)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TunnelProgress {
    pub hostnames: Vec<String>,
    pub steps: Vec<ProgressStep>,
}

impl TunnelProgress {
    pub fn all_ready(&self) -> bool {
        self.steps.iter().all(|s| {
            // ConnectorMetadataProgrammed is absent in extension-server mode
            // (EPP emission disabled). Treat Unknown as Ready for this step
            // only — the extension server handles xDS injection directly and
            // does not report back via a condition.
            if s.kind == ProgressStepKind::ConnectorMetadataProgrammed
                && s.status == StepStatus::Unknown
            {
                return true;
            }
            s.status == StepStatus::Ready
        })
    }

    pub fn step(&self, kind: ProgressStepKind) -> Option<&ProgressStep> {
        self.steps.iter().find(|s| s.kind == kind)
    }

    pub fn terminal_failure(&self) -> Option<&ProgressStep> {
        self.steps.iter().find(|s| s.is_terminal_failure())
    }

    fn from_resources(proxy: &HTTPProxy, connector: Option<&Connector>) -> Self {
        let proxy_conds = proxy.status.as_ref().and_then(|s| s.conditions.as_deref());
        let proxy_gen = proxy.metadata.generation.unwrap_or(0);
        let proxy_resource = proxy
            .metadata
            .name
            .as_deref()
            .map(|n| format!("HTTPProxy/{n}"));
        let conn_conds = connector
            .and_then(|c| c.status.as_ref())
            .and_then(|s| s.conditions.as_deref());
        let conn_gen = connector.and_then(|c| c.metadata.generation).unwrap_or(0);
        let connector_resource = connector
            .and_then(|c| c.metadata.name.as_deref())
            .map(|n| format!("Connector/{n}"));

        // A condition is Ready only if its observedGeneration has caught up
        // with the resource's current generation. After we PATCH the spec
        // (e.g. `tunnel listen --id` re-points the backend, bumping
        // generation 1→2), the controller's prior True conditions still
        // show observedGeneration=1 until it re-reconciles. Treating those
        // as Ready makes the CLI claim "Tunnel ready" while the data plane
        // is still serving 503s from stale Envoy config.
        let make_step = |kind: ProgressStepKind,
                         conds: Option<&[k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition]>,
                         type_: &str,
                         current_gen: i64,
                         resource: Option<String>|
         -> ProgressStep {
            let cond = conds.unwrap_or_default().iter().find(|c| c.type_ == type_);
            let observed = cond.and_then(|c| c.observed_generation).unwrap_or(0);
            let fresh = observed >= current_gen;
            let status = match cond {
                Some(c) if c.status == "True" && fresh => StepStatus::Ready,
                Some(_) => StepStatus::Pending,
                None => StepStatus::Unknown,
            };
            ProgressStep {
                kind,
                status,
                reason: cond.map(|c| c.reason.clone()),
                message: cond.map(|c| c.message.clone()),
                resource,
            }
        };

        let steps = vec![
            make_step(
                ProgressStepKind::ProxyAccepted,
                proxy_conds,
                HTTP_PROXY_CONDITION_ACCEPTED,
                proxy_gen,
                proxy_resource.clone(),
            ),
            make_step(
                ProgressStepKind::CertificatesReady,
                proxy_conds,
                HTTP_PROXY_CONDITION_CERTIFICATES_READY,
                proxy_gen,
                proxy_resource.clone(),
            ),
            make_step(
                ProgressStepKind::ConnectorReady,
                conn_conds,
                CONNECTOR_CONDITION_READY,
                conn_gen,
                connector_resource.clone(),
            ),
            make_step(
                ProgressStepKind::IrohDnsPublished,
                conn_conds,
                CONNECTOR_CONDITION_IROH_DNS_PUBLISHED,
                conn_gen,
                connector_resource.clone(),
            ),
            make_step(
                ProgressStepKind::ProxyProgrammed,
                proxy_conds,
                HTTP_PROXY_CONDITION_PROGRAMMED,
                proxy_gen,
                proxy_resource.clone(),
            ),
            make_step(
                ProgressStepKind::ConnectorMetadataProgrammed,
                proxy_conds,
                HTTP_PROXY_CONDITION_CONNECTOR_METADATA_PROGRAMMED,
                proxy_gen,
                proxy_resource,
            ),
        ];

        Self {
            hostnames: proxy_hostnames(proxy),
            steps,
        }
    }
}

impl TunnelService {
    pub fn new(datum: DatumCloudClient, listen: ListenNode) -> Self {
        Self {
            datum,
            listen,
            publish_tickets: publish_tickets_enabled(),
            create_traffic_protection_policies: create_traffic_protection_policies_enabled(),
        }
    }

    pub async fn list_active(&self) -> Result<Vec<TunnelSummary>> {
        let Some(selected) = self.datum.selected_context() else {
            return Ok(Vec::new());
        };
        self.list_project(&selected.project_id).await
    }

    pub async fn get_active(&self, tunnel_id: &str) -> Result<Option<TunnelSummary>> {
        let tunnels = self.list_active().await?;
        Ok(tunnels.into_iter().find(|tunnel| tunnel.id == tunnel_id))
    }

    pub async fn get_active_by_endpoint(&self, endpoint: &str) -> Result<Option<TunnelSummary>> {
        let tunnels = self.list_active().await?;
        let normalized = normalize_endpoint(endpoint);
        Ok(tunnels.into_iter().find(|tunnel| tunnel.endpoint == normalized))
    }

    /// Fetch the rich progress view for a tunnel: every checkpoint condition
    /// from both the HTTPProxy and its referenced Connector. Returns `None`
    /// if the proxy doesn't exist (matches `get_active`).
    pub async fn get_active_progress(
        &self,
        tunnel_id: &str,
    ) -> Result<Option<TunnelProgress>> {
        let Some(selected) = self.datum.selected_context() else {
            return Ok(None);
        };
        let pcp = self
            .datum
            .project_control_plane_client(&selected.project_id)
            .await?;
        let client = pcp.client();
        let proxies: Api<HTTPProxy> = Api::namespaced(client.clone(), DEFAULT_PCP_NAMESPACE);
        let Some(proxy) = proxies
            .get_opt(tunnel_id)
            .await
            .std_context("Failed to fetch HTTPProxy")?
        else {
            return Ok(None);
        };

        let connector = if let Some(name) = proxy_connector_name(&proxy) {
            let connectors: Api<Connector> = Api::namespaced(client, DEFAULT_PCP_NAMESPACE);
            connectors
                .get_opt(&name)
                .await
                .std_context("Failed to fetch Connector")?
        } else {
            None
        };

        Ok(Some(TunnelProgress::from_resources(&proxy, connector.as_ref())))
    }

    /// Re-patch the connector's connectionDetails after it becomes Ready.
    ///
    /// The replicator mirrors the upstream-status annotation to the downstream
    /// connector on every spec change. When the connector first becomes
    /// Ready:True, the connector controller touches the downstream gateway
    /// annotation to trigger an Envoy Gateway re-translation — but if the
    /// annotation captured Ready:False before the lease renewed, that touch
    /// never fires. Re-patching connectionDetails triggers a spec change on
    /// the upstream connector, which causes the replicator to re-mirror the
    /// annotation with the current (Ready:True) status, and EG re-translates.
    pub async fn refresh_connection_details(&self) -> Result<()> {
        let Some(selected) = self.datum.selected_context() else {
            return Ok(());
        };
        let project_id = &selected.project_id;
        let Some(connector) = self.find_connector(project_id).await? else {
            return Ok(());
        };
        let pcp = self.datum.project_control_plane_client(project_id).await?;
        let connectors: Api<Connector> =
            Api::namespaced(pcp.client(), DEFAULT_PCP_NAMESPACE);
        let name = connector.name_any();
        if let Some(details) = build_connection_details(&self.listen) {
            let details_value = serde_json::to_value(details)
                .std_context("Failed to serialize connection details")?;
            let patch = json!({ "status": { "connectionDetails": details_value } });
            if let Err(err) = connectors
                .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
                .await
            {
                warn!(%name, "Failed to refresh connector connectionDetails: {err:#}");
            } else {
                debug!(%name, "refreshed connector connectionDetails to trigger replicator");
            }
        }
        Ok(())
    }

    pub async fn create_active(&self, label: &str, endpoint: &str) -> Result<TunnelSummary> {
        let Some(selected) = self.datum.selected_context() else {
            n0_error::bail_any!("No project selected");
        };
        self.create_project(&selected.project_id, label, endpoint)
            .await
    }

    pub async fn update_active(
        &self,
        tunnel_id: &str,
        label: &str,
        endpoint: &str,
    ) -> Result<TunnelSummary> {
        let Some(selected) = self.datum.selected_context() else {
            n0_error::bail_any!("No project selected");
        };
        self.update_project(&selected.project_id, tunnel_id, label, endpoint)
            .await
    }

    /// Delete connectors in the active project that are not referenced by any
    /// HTTPProxy. Returns the names of deleted connectors.
    pub async fn cleanup_orphaned_connectors(&self) -> Result<Vec<String>> {
        let Some(selected) = self.datum.selected_context() else {
            return Ok(Vec::new());
        };
        self.cleanup_orphaned_connectors_project(&selected.project_id)
            .await
    }

    async fn cleanup_orphaned_connectors_project(
        &self,
        project_id: &str,
    ) -> Result<Vec<String>> {
        let pcp = self.datum.project_control_plane_client(project_id).await?;
        let client = pcp.client();
        let proxies: Api<HTTPProxy> = Api::namespaced(client.clone(), DEFAULT_PCP_NAMESPACE);
        let connectors: Api<Connector> = Api::namespaced(client.clone(), DEFAULT_PCP_NAMESPACE);
        let ads: Api<ConnectorAdvertisement> =
            Api::namespaced(client, DEFAULT_PCP_NAMESPACE);

        let proxy_list = proxies
            .list(&ListParams::default())
            .await
            .std_context("Failed to list HTTPProxy objects")?;

        // Collect connector names referenced by non-deleting proxies.
        let referenced: std::collections::HashSet<String> = proxy_list
            .items
            .iter()
            .filter(|p| p.metadata.deletion_timestamp.is_none())
            .filter_map(|p| proxy_connector_name(p))
            .collect();

        let connector_list = connectors
            .list(&ListParams::default())
            .await
            .std_context("Failed to list Connector objects")?;

        let mut deleted = Vec::new();
        for c in connector_list.items {
            let Some(name) = c.metadata.name.clone() else { continue };
            if referenced.contains(&name) {
                continue;
            }
            // Delete leftover ConnectorAdvertisements for this connector.
            let ad_selector = format!("{ADVERTISEMENT_CONNECTOR_FIELD}={name}");
            if let Ok(ad_list) = ads
                .list(&ListParams::default().fields(&ad_selector))
                .await
            {
                for ad in ad_list.items {
                    if let Some(ad_name) = ad.metadata.name.clone()
                        && let Err(err) = ads.delete(&ad_name, &DeleteParams::default()).await
                    {
                        warn!(%ad_name, "Failed to delete orphaned connector advertisement: {err:#}");
                    }
                }
            }
            // Delete the connector.
            if let Err(err) = connectors.delete(&name, &DeleteParams::default()).await {
                warn!(%name, "Failed to delete orphaned connector: {err:#}");
            } else {
                debug!(%name, "Deleted orphaned connector");
                deleted.push(name);
            }
        }
        Ok(deleted)
    }

    pub async fn set_enabled_active(
        &self,
        tunnel_id: &str,
        enabled: bool,
    ) -> Result<TunnelSummary> {
        let Some(selected) = self.datum.selected_context() else {
            n0_error::bail_any!("No project selected");
        };
        self.set_enabled_project(&selected.project_id, tunnel_id, enabled)
            .await
    }

    pub async fn delete_active(&self, tunnel_id: &str) -> Result<TunnelDeleteOutcome> {
        let Some(selected) = self.datum.selected_context() else {
            n0_error::bail_any!("No project selected");
        };
        self.delete_project(&selected.project_id, tunnel_id).await
    }

    pub async fn list_project(&self, project_id: &str) -> Result<Vec<TunnelSummary>> {
        let (tunnels, _) = self.list_project_with_orphans(project_id).await?;
        Ok(tunnels)
    }

    /// Like [`list_project`] but also returns any [`OrphanedConnector`]s found
    /// in the project — connectors that exist but are not referenced by any
    /// tunnel's HTTPProxy. These are typically left over from a previous tunnel
    /// that exited uncleanly.
    pub async fn list_project_with_orphans(
        &self,
        project_id: &str,
    ) -> Result<(Vec<TunnelSummary>, Vec<OrphanedConnector>)> {
        let pcp = self.datum.project_control_plane_client(project_id).await?;
        let client = pcp.client();
        let proxies: Api<HTTPProxy> = Api::namespaced(client.clone(), DEFAULT_PCP_NAMESPACE);
        let ads: Api<ConnectorAdvertisement> = Api::namespaced(client.clone(), DEFAULT_PCP_NAMESPACE);
        let connectors_api: Api<Connector> = Api::namespaced(client, DEFAULT_PCP_NAMESPACE);

        let proxy_list = proxies
            .list(&ListParams::default())
            .await
            .std_context("Failed to list HTTPProxy objects")?;

        let ad_list = ads
            .list(&ListParams::default())
            .await
            .std_context("Failed to list ConnectorAdvertisement objects")?;
        let enabled_by_name: std::collections::HashMap<String, ConnectorAdvertisement> = ad_list
            .items
            .into_iter()
            .filter_map(|item| item.metadata.name.clone().map(|name| (name, item)))
            .collect();

        // Fetch all connectors so we can check their Ready condition and detect orphans.
        let connector_list = connectors_api
            .list(&ListParams::default())
            .await
            .std_context("Failed to list Connector objects")?;
        let connector_ready_by_name: std::collections::HashMap<String, bool> = connector_list
            .items
            .iter()
            .filter_map(|c| {
                let name = c.metadata.name.clone()?;
                let ready = condition_status(
                    c.status
                        .as_ref()
                        .and_then(|s| s.conditions.as_deref()),
                    CONNECTOR_CONDITION_READY,
                    true,
                );
                Some((name, ready))
            })
            .collect();

        let connector_device_by_name: std::collections::HashMap<String, String> =
            connector_list
                .items
                .iter()
                .filter_map(|c| {
                    let name = c.metadata.name.clone()?;
                    let device = c
                        .metadata
                        .annotations
                        .as_ref()
                        .and_then(|a| a.get(DEVICE_NAME_ANNOTATION))?
                        .clone();
                    Some((name, device))
                })
                .collect();

        let mut tunnels = Vec::new();
        let mut referenced_connector_names = std::collections::HashSet::new();

        for proxy in proxy_list.items {
            let Some(name) = proxy.metadata.name.clone() else {
                continue;
            };
            if !name.starts_with("tunnel-") {
                continue;
            }
            let label = proxy
                .metadata
                .annotations
                .as_ref()
                .and_then(|labels| labels.get(DISPLAY_NAME_ANNOTATION))
                .cloned()
                .unwrap_or_else(|| name.clone());
            let endpoint = normalize_endpoint(&proxy_backend_endpoint(&proxy).unwrap_or_default());
            let hostnames = proxy_hostnames(&proxy);
            let accepted = condition_status(
                proxy
                    .status
                    .as_ref()
                    .and_then(|status| status.conditions.as_deref()),
                HTTP_PROXY_CONDITION_ACCEPTED,
                true,
            );
            let programmed = condition_status(
                proxy
                    .status
                    .as_ref()
                    .and_then(|status| status.conditions.as_deref()),
                HTTP_PROXY_CONDITION_PROGRAMMED,
                true,
            );
            let connector_metadata_programmed = condition_status(
                proxy
                    .status
                    .as_ref()
                    .and_then(|status| status.conditions.as_deref()),
                HTTP_PROXY_CONDITION_CONNECTOR_METADATA_PROGRAMMED,
                false,
            );
            let enabled = enabled_by_name.contains_key(&name);
            let connector_name = proxy_connector_name(&proxy);
            let connector_ready = connector_name
                .as_deref()
                .and_then(|cn| connector_ready_by_name.get(cn).copied())
                .unwrap_or(false);
            if let Some(cn) = &connector_name {
                referenced_connector_names.insert(cn.clone());
            }
            tunnels.push(TunnelSummary {
                id: name,
                label,
                endpoint,
                hostnames,
                enabled,
                accepted,
                programmed,
                connector_metadata_programmed,
                connector_ready,
                connector_name: connector_name.clone(),
                connector_device: connector_name
                    .as_deref()
                    .and_then(|cn| connector_device_by_name.get(cn).cloned()),
            });
        }

        // Any connector not referenced by a tunnel is orphaned.
        let orphans: Vec<OrphanedConnector> = connector_list
            .items
            .into_iter()
            .filter_map(|c| {
                let name = c.metadata.name?;
                if referenced_connector_names.contains(&name) {
                    return None;
                }
                let ready = *connector_ready_by_name.get(&name).unwrap_or(&false);
                let device = c
                    .metadata
                    .annotations
                    .as_ref()
                    .and_then(|a| a.get(DEVICE_NAME_ANNOTATION))
                    .cloned();
                Some(OrphanedConnector { name, ready, device })
            })
            .collect();

        Ok((tunnels, orphans))
    }

    /// Like [`list_active`] but also returns orphaned connectors.
    /// Used by the `list` subcommand to show stale connector warnings.
    pub async fn list_active_with_orphans(
        &self,
    ) -> Result<(Vec<TunnelSummary>, Vec<OrphanedConnector>)> {
        let Some(selected) = self.datum.selected_context() else {
            return Ok((Vec::new(), Vec::new()));
        };
        self.list_project_with_orphans(&selected.project_id).await
    }

    pub async fn create_project(
        &self,
        project_id: &str,
        label: &str,
        endpoint: &str,
    ) -> Result<TunnelSummary> {
        let endpoint = normalize_endpoint(endpoint);
        let target = parse_target(&endpoint)?;
        let connector = self.ensure_connector(project_id).await?;
        let connector_name = connector.name_any();

        let pcp = self.datum.project_control_plane_client(project_id).await?;
        let client = pcp.client();
        let proxies: Api<HTTPProxy> = Api::namespaced(client.clone(), DEFAULT_PCP_NAMESPACE);
        let ads: Api<ConnectorAdvertisement> =
            Api::namespaced(client.clone(), DEFAULT_PCP_NAMESPACE);

        debug!(
            %project_id,
            connector = %connector_name,
            endpoint = %endpoint,
            "creating HTTPProxy"
        );
        let mut proxy = HTTPProxy {
            metadata: ObjectMeta {
                generate_name: Some("tunnel-".to_string()),
                annotations: Some(BTreeMap::from([(
                    DISPLAY_NAME_ANNOTATION.to_string(),
                    label.to_string(),
                )])),
                ..Default::default()
            },
            spec: HTTPProxySpec {
                hostnames: None,
                rules: vec![
                    https_redirect_rule(),
                    proxy_rule(&endpoint, &connector_name),
                ],
            },
            status: None,
        };
        let post_params = PostParams::default();
        proxy = with_quota_check_retry("HTTPProxy create", || {
            proxies.create(&post_params, &proxy)
        })
        .await
        .map_err(|err| {
            warn!(
                %project_id,
                connector = %connector_name,
                endpoint = %endpoint,
                "HTTPProxy create failed: {err:#}"
            );
            format_quota_error(&err, "HTTPProxy")
                .unwrap_or_else(|| format!("Failed to create HTTPProxy: {err}"))
        })
        .map_err(|err| n0_error::anyerr!(err))?;
        let proxy_name = proxy.name_any();
        debug!(
            %project_id,
            proxy = %proxy_name,
            connector = %connector_name,
            "created HTTPProxy"
        );

        let ad_spec = advertisement_spec(&connector_name, target);
        debug!(
            %project_id,
            proxy = %proxy_name,
            connector = %connector_name,
            "creating ConnectorAdvertisement"
        );
        let ad = ConnectorAdvertisement {
            metadata: ObjectMeta {
                name: Some(proxy_name.clone()),
                ..Default::default()
            },
            spec: ad_spec,
            status: None,
        };
        let ad_post = PostParams::default();
        with_quota_check_retry("ConnectorAdvertisement create", || {
            ads.create(&ad_post, &ad)
        })
        .await
        .map_err(|err| {
            warn!(
                %project_id,
                proxy = %proxy_name,
                connector = %connector_name,
                "ConnectorAdvertisement create failed: {err:#}"
            );
            format_quota_error(&err, "ConnectorAdvertisement")
                .unwrap_or_else(|| format!("Failed to create ConnectorAdvertisement: {err}"))
        })
        .map_err(|err| n0_error::anyerr!(err))?;
        debug!(
            %project_id,
            proxy = %proxy_name,
            connector = %connector_name,
            "created ConnectorAdvertisement"
        );

        if self.create_traffic_protection_policies {
            let tpps: Api<TrafficProtectionPolicy> =
                Api::namespaced(client.clone(), DEFAULT_PCP_NAMESPACE);
            debug!(
                %project_id,
                proxy = %proxy_name,
                "creating TrafficProtectionPolicy"
            );
            let tpp = TrafficProtectionPolicy {
                metadata: ObjectMeta {
                    name: Some(proxy_name.clone()),
                    ..Default::default()
                },
                spec: TrafficProtectionPolicySpec {
                    target_refs: vec![LocalPolicyTargetReferenceWithSectionName {
                        group: "gateway.networking.k8s.io".to_string(),
                        kind: "Gateway".to_string(),
                        name: proxy_name.clone(),
                        section_name: None,
                    }],
                    mode: Some(TrafficProtectionPolicyMode::Enforce),
                    sampling_percentage: None,
                    rule_sets: Some(vec![TrafficProtectionPolicyRuleSet {
                        rule_set_type: TrafficProtectionPolicyRuleSetType::OWASPCoreRuleSet,
                        owasp_core_rule_set: Some(OWASPCRS {
                            paranoia_levels: Some(ParanoiaLevels {
                                blocking: Some(1),
                                detection: Some(1),
                            }),
                            score_thresholds: None,
                            rule_exclusions: None,
                        }),
                    }]),
                },
                status: None,
            };
            let tpp_post = PostParams::default();
            with_quota_check_retry("TrafficProtectionPolicy create", || {
                tpps.create(&tpp_post, &tpp)
            })
                .await
                .map_err(|err| {
                    warn!(
                        %project_id,
                        proxy = %proxy_name,
                        "TrafficProtectionPolicy create failed: {err:#}"
                    );
                    format_quota_error(&err, "TrafficProtectionPolicy").unwrap_or_else(|| {
                        format!("Failed to create TrafficProtectionPolicy: {err}")
                    })
                })
                .map_err(|err| n0_error::anyerr!(err))?;
            debug!(
                %project_id,
                proxy = %proxy_name,
                "created TrafficProtectionPolicy"
            );
        } else {
            debug!(
                %project_id,
                proxy = %proxy_name,
                "skipping TrafficProtectionPolicy creation (env disabled)"
            );
        }

        let proxy_state = proxy_state_from_summary(&proxy_name, &endpoint, label, true)?;
        if self.publish_tickets {
            debug!(%proxy_name, "publishing ticket for tunnel");
            if let Err(err) = self.listen.set_proxy(proxy_state).await {
                warn!(%proxy_name, "Failed to publish ticket: {err:#}");
            }
        } else if let Err(err) = self.listen.set_proxy_state(proxy_state).await {
            warn!(%proxy_name, "Failed to store proxy state: {err:#}");
        }

        Ok(TunnelSummary {
            id: proxy_name,
            label: label.to_string(),
            endpoint,
            hostnames: proxy_hostnames(&proxy),
            enabled: true,
            accepted: condition_status(
                proxy
                    .status
                    .as_ref()
                    .and_then(|status| status.conditions.as_deref()),
                HTTP_PROXY_CONDITION_ACCEPTED,
                true,
            ),
            programmed: condition_status(
                proxy
                    .status
                    .as_ref()
                    .and_then(|status| status.conditions.as_deref()),
                HTTP_PROXY_CONDITION_PROGRAMMED,
                true,
            ),
            connector_metadata_programmed: condition_status(
                proxy
                    .status
                    .as_ref()
                    .and_then(|status| status.conditions.as_deref()),
                HTTP_PROXY_CONDITION_CONNECTOR_METADATA_PROGRAMMED,
                false,
            ),
            // connector_ready is not checked at creation time; the heartbeat
            // agent will establish the lease shortly after.
            connector_ready: false,
            connector_name: Some(connector_name.clone()),
            connector_device: Some(friendly_device_name()),
        })
    }

    pub async fn update_project(
        &self,
        project_id: &str,
        tunnel_id: &str,
        label: &str,
        endpoint: &str,
    ) -> Result<TunnelSummary> {
        let endpoint = normalize_endpoint(endpoint);
        let target = parse_target(&endpoint)?;
        let connector = self.ensure_connector(project_id).await?;
        let connector_name = connector.name_any();

        let pcp = self.datum.project_control_plane_client(project_id).await?;
        let client = pcp.client();
        let proxies: Api<HTTPProxy> = Api::namespaced(client.clone(), DEFAULT_PCP_NAMESPACE);
        let ads: Api<ConnectorAdvertisement> = Api::namespaced(client, DEFAULT_PCP_NAMESPACE);

        let existing = proxies
            .get(tunnel_id)
            .await
            .std_context("Failed to fetch HTTPProxy")?;
        let hostnames = existing.spec.hostnames.clone().unwrap_or_default();
        let desired_rules = vec![https_redirect_rule(), proxy_rule(&endpoint, &connector_name)];

        // Skip the PATCH when the existing spec already matches what we'd
        // write. A no-op patch still bumps metadata.generation on some API
        // servers, which triggers a downstream Envoy re-reconcile and a
        // window where the data plane returns 5xx — exactly the resume-
        // induced churn the UI doesn't suffer because its enable path
        // never touches HTTPProxy.spec. Making this verb idempotent at the
        // lib boundary means every caller (CLI, UI Edit dialog, future
        // datumctl plugin) gets the no-churn behavior for free.
        if http_proxy_spec_matches(&existing, label, &desired_rules) {
            debug!(
                %project_id,
                proxy = %tunnel_id,
                "HTTPProxy spec already matches desired state; skipping patch"
            );
        } else {
            let patch = json!({
                "metadata": {
                    "annotations": {
                        DISPLAY_NAME_ANNOTATION: label,
                    }
                },
                "spec": {
                    "hostnames": hostnames,
                    "rules": desired_rules,
                }
            });
            proxies
                .patch(tunnel_id, &PatchParams::default(), &Patch::Merge(&patch))
                .await
                .std_context("Failed to update HTTPProxy")?;
        }

        let existing_ad = ads
            .get_opt(tunnel_id)
            .await
            .std_context("Failed to fetch ConnectorAdvertisement")?;
        if let Some(existing_ad) = existing_ad.as_ref() {
            let desired_ad_spec = advertisement_spec(&connector_name, target);
            if advertisement_spec_matches(existing_ad, &desired_ad_spec) {
                debug!(
                    %project_id,
                    advertisement = %tunnel_id,
                    "ConnectorAdvertisement spec already matches; skipping patch"
                );
            } else {
                let ad_patch = json!({ "spec": desired_ad_spec });
                ads.patch(tunnel_id, &PatchParams::default(), &Patch::Merge(&ad_patch))
                    .await
                    .std_context("Failed to update ConnectorAdvertisement")?;
            }
        }

        let enabled = existing_ad.is_some();
        let connector_name = proxy_connector_name(&existing);

        let summary = TunnelSummary {
            id: tunnel_id.to_string(),
            label: label.to_string(),
            endpoint,
            hostnames: proxy_hostnames(&existing),
            enabled,
            accepted: condition_status(
                existing
                    .status
                    .as_ref()
                    .and_then(|status| status.conditions.as_deref()),
                HTTP_PROXY_CONDITION_ACCEPTED,
                true,
            ),
            programmed: condition_status(
                existing
                    .status
                    .as_ref()
                    .and_then(|status| status.conditions.as_deref()),
                HTTP_PROXY_CONDITION_PROGRAMMED,
                true,
            ),
            connector_metadata_programmed: condition_status(
                existing
                    .status
                    .as_ref()
                    .and_then(|status| status.conditions.as_deref()),
                HTTP_PROXY_CONDITION_CONNECTOR_METADATA_PROGRAMMED,
                false,
            ),
            connector_ready: false,
            connector_name,
            connector_device: None,
        };

        if !self.publish_tickets
            && let Ok(proxy_state) = proxy_state_from_summary(
                &summary.id,
                &summary.endpoint,
                &summary.label,
                summary.enabled,
            )
            && let Err(err) = self.listen.set_proxy_state(proxy_state).await
        {
            warn!(tunnel_id = %summary.id, "Failed to store proxy state: {err:#}");
        }

        Ok(summary)
    }

    pub async fn set_enabled_project(
        &self,
        project_id: &str,
        tunnel_id: &str,
        enabled: bool,
    ) -> Result<TunnelSummary> {
        let connector = self.ensure_connector(project_id).await?;
        let connector_name = connector.name_any();

        let pcp = self.datum.project_control_plane_client(project_id).await?;
        let client = pcp.client();
        let proxies: Api<HTTPProxy> = Api::namespaced(client.clone(), DEFAULT_PCP_NAMESPACE);
        let ads: Api<ConnectorAdvertisement> = Api::namespaced(client, DEFAULT_PCP_NAMESPACE);

        let proxy = proxies
            .get(tunnel_id)
            .await
            .std_context("Failed to fetch HTTPProxy")?;
        let endpoint = normalize_endpoint(&proxy_backend_endpoint(&proxy).unwrap_or_default());
        let label = proxy
            .metadata
            .annotations
            .as_ref()
            .and_then(|labels| labels.get(DISPLAY_NAME_ANNOTATION))
            .cloned()
            .unwrap_or_else(|| tunnel_id.to_string());

        // Always patch the proxy's connector backend to reference the fresh
        // connector. The previous connector was deleted by ensure_connector;
        // if we don't update the proxy here the operator watches a connector
        // that no longer exists and the Ready/IrohDNSPublished conditions
        // never become True.
        {
            let target = parse_target(&endpoint)?;
            let desired_rules = vec![https_redirect_rule(), proxy_rule(&endpoint, &connector_name)];
            if !http_proxy_spec_matches(&proxy, &label, &desired_rules) {
                let hostnames = proxy.spec.hostnames.clone().unwrap_or_default();
                let patch = json!({
                    "metadata": { "annotations": { DISPLAY_NAME_ANNOTATION: &label } },
                    "spec": { "hostnames": hostnames, "rules": desired_rules },
                });
                proxies
                    .patch(tunnel_id, &PatchParams::default(), &Patch::Merge(&patch))
                    .await
                    .std_context("Failed to patch HTTPProxy connector reference")?;
            }
            let _ = target; // used above
        }

        if enabled {
            let target = parse_target(&endpoint)?;
            let ad_spec = advertisement_spec(&connector_name, target);
            match ads
                .get_opt(tunnel_id)
                .await
                .std_context("Failed to load ConnectorAdvertisement")?
            {
                Some(_) => {
                    let ad_patch = json!({ "spec": ad_spec });
                    ads.patch(tunnel_id, &PatchParams::default(), &Patch::Merge(&ad_patch))
                        .await
                        .std_context("Failed to update ConnectorAdvertisement")?;
                }
                None => {
                    let ad = ConnectorAdvertisement {
                        metadata: ObjectMeta {
                            name: Some(tunnel_id.to_string()),
                            ..Default::default()
                        },
                        spec: ad_spec,
                        status: None,
                    };
                    let ad_post = PostParams::default();
                    with_quota_check_retry("ConnectorAdvertisement create", || {
                        ads.create(&ad_post, &ad)
                    })
                    .await
                    .std_context("Failed to create ConnectorAdvertisement")?;
                }
            }
        } else if ads
            .get_opt(tunnel_id)
            .await
            .std_context("Failed to load ConnectorAdvertisement")?
            .is_some()
        {
            ads.delete(tunnel_id, &DeleteParams::default())
                .await
                .std_context("Failed to delete ConnectorAdvertisement")?;
        }

        let connector_name = proxy_connector_name(&proxy);

        let summary = TunnelSummary {
            id: tunnel_id.to_string(),
            label,
            endpoint,
            hostnames: proxy_hostnames(&proxy),
            enabled,
            accepted: condition_status(
                proxy
                    .status
                    .as_ref()
                    .and_then(|status| status.conditions.as_deref()),
                HTTP_PROXY_CONDITION_ACCEPTED,
                true,
            ),
            programmed: condition_status(
                proxy
                    .status
                    .as_ref()
                    .and_then(|status| status.conditions.as_deref()),
                HTTP_PROXY_CONDITION_PROGRAMMED,
                true,
            ),
            connector_metadata_programmed: condition_status(
                proxy
                    .status
                    .as_ref()
                    .and_then(|status| status.conditions.as_deref()),
                HTTP_PROXY_CONDITION_CONNECTOR_METADATA_PROGRAMMED,
                false,
            ),
            connector_ready: false,
            connector_name,
            connector_device: None,
        };

        if !self.publish_tickets
            && let Ok(proxy_state) = proxy_state_from_summary(
                &summary.id,
                &summary.endpoint,
                &summary.label,
                summary.enabled,
            )
            && let Err(err) = self.listen.set_proxy_state(proxy_state).await
        {
            warn!(tunnel_id = %summary.id, "Failed to store proxy state: {err:#}");
        }

        Ok(summary)
    }

    pub async fn delete_project(
        &self,
        project_id: &str,
        tunnel_id: &str,
    ) -> Result<TunnelDeleteOutcome> {
        let connector = self.find_connector(project_id).await?;
        let connector_name = connector.as_ref().map(|c| c.name_any());

        let pcp = self.datum.project_control_plane_client(project_id).await?;
        let client = pcp.client();
        let proxies: Api<HTTPProxy> = Api::namespaced(client.clone(), DEFAULT_PCP_NAMESPACE);
        let ads: Api<ConnectorAdvertisement> =
            Api::namespaced(client.clone(), DEFAULT_PCP_NAMESPACE);
        let connectors: Api<Connector> = Api::namespaced(client.clone(), DEFAULT_PCP_NAMESPACE);

        let mut http_proxy_name: Option<String> = None;
        if proxies
            .get_opt(tunnel_id)
            .await
            .std_context("Failed to load HTTPProxy")?
            .is_some()
        {
            proxies
                .delete(tunnel_id, &DeleteParams::default())
                .await
                .std_context("Failed to delete HTTPProxy")?;
            http_proxy_name = Some(tunnel_id.to_string());
        }

        let mut connector_ad_name: Option<String> = None;
        if ads
            .get_opt(tunnel_id)
            .await
            .std_context("Failed to load ConnectorAdvertisement")?
            .is_some()
        {
            ads.delete(tunnel_id, &DeleteParams::default())
                .await
                .std_context("Failed to delete ConnectorAdvertisement")?;
            connector_ad_name = Some(tunnel_id.to_string());
        }

        let mut tpp_name: Option<String> = None;
        let tpps: Api<TrafficProtectionPolicy> =
            Api::namespaced(client.clone(), DEFAULT_PCP_NAMESPACE);
        if tpps
            .get_opt(tunnel_id)
            .await
            .std_context("Failed to load TrafficProtectionPolicy")?
            .is_some()
        {
            tpps.delete(tunnel_id, &DeleteParams::default())
                .await
                .std_context("Failed to delete TrafficProtectionPolicy")?;
            tpp_name = Some(tunnel_id.to_string());
        }

        if self.publish_tickets {
            debug!(%tunnel_id, "unpublishing ticket for tunnel");
            if let Err(err) = self.listen.remove_proxy(tunnel_id).await {
                warn!(%tunnel_id, "Failed to unpublish ticket: {err:#}");
            }
        } else if let Err(err) = self.listen.remove_proxy_state(tunnel_id).await {
            warn!(%tunnel_id, "Failed to remove proxy state: {err:#}");
        }

        let mut connector_name_out: Option<String> = None;
        if let Some(connector_name) = connector_name {
            let remaining = proxies
                .list(&ListParams::default())
                .await
                .std_context("Failed to list remaining HTTPProxy objects")?;
            let mut remaining_for_connector = remaining
                .items
                .into_iter()
                .filter(|proxy| {
                    // Skip proxies already marked for deletion — the API
                    // server returns them in list responses until
                    // finalizers complete, but they won't keep using the
                    // connector.
                    proxy.metadata.deletion_timestamp.is_none()
                        && proxy_connector_name(proxy).as_deref() == Some(connector_name.as_str())
                })
                .peekable();
            if remaining_for_connector.peek().is_none() {
                let ad_selector = format!("{ADVERTISEMENT_CONNECTOR_FIELD}={connector_name}");
                let ads_list = ads
                    .list(&ListParams::default().fields(&ad_selector))
                    .await
                    .std_context("Failed to list remaining ConnectorAdvertisements")?;
                for ad in ads_list.items {
                    if let Some(name) = ad.metadata.name.clone()
                        && let Err(err) = ads.delete(&name, &DeleteParams::default()).await
                    {
                        warn!(%name, "Failed to delete connector advertisement: {err:#}");
                    }
                }

                if connectors
                    .get_opt(&connector_name)
                    .await
                    .std_context("Failed to load Connector")?
                    .is_some()
                {
                    connectors
                        .delete(&connector_name, &DeleteParams::default())
                        .await
                        .std_context("Failed to delete Connector")?;
                    connector_name_out = Some(connector_name);
                }
            }
        }

        if let Err(err) = self.listen.repo().delete_tunnel_dir(project_id, tunnel_id).await {
            warn!(%tunnel_id, "Failed to delete tunnel local state: {err:#}");
        }

        Ok(TunnelDeleteOutcome {
            project_id: project_id.to_string(),
            http_proxy: http_proxy_name,
            connector_ad: connector_ad_name,
            traffic_protection_policy: tpp_name,
            connector: connector_name_out,
        })
    }

    async fn find_connector(&self, project_id: &str) -> Result<Option<Connector>> {
        let pcp = self.datum.project_control_plane_client(project_id).await?;
        let client = pcp.client();
        let connectors: Api<Connector> = Api::namespaced(client, DEFAULT_PCP_NAMESPACE);
        let endpoint_id = self.listen.endpoint_id().to_string();
        let selector = format!("{CONNECTOR_SELECTOR_FIELD}={endpoint_id}");
        let list = match connectors
            .list(&ListParams::default().fields(&selector))
            .await
        {
            Ok(list) => list,
            Err(kube::Error::Api(e)) if e.code == 403 => {
                n0_error::bail_any!(
                    "Permission denied listing connectors in project {project_id}. \
                     Switch your datumctl context to this project first: \
                     'datumctl ctx switch {project_id}'"
                );
            }
            Err(kube::Error::Api(e)) if e.code == 401 => {
                n0_error::bail_any!(
                    "Authentication failed for project {project_id}. \
                     Switch your datumctl context to this project first: \
                     'datumctl ctx switch {project_id}'"
                );
            }
            Err(err) => {
                return Err(err).std_context("Failed to list connectors");
            }
        };
        if list.items.is_empty() {
            return Ok(None);
        }
        if list.items.len() > 1 {
            debug!(
                %selector,
                count = list.items.len(),
                "Multiple connectors found for endpoint, using first"
            );
        }
        let mut connector = list.items.into_iter().next().unwrap();
        patch_device_annotations(&connectors, &mut connector).await;
        Ok(Some(connector))
    }

    async fn resolve_connector_class(client: kube::Client) -> Result<String> {
        let classes: Api<ConnectorClass> = Api::all(client);
        match classes.list(&ListParams::default()).await {
            Ok(class_list) if !class_list.items.is_empty() => {
                for c in &class_list.items {
                    if c.name_any() == DEFAULT_CONNECTOR_CLASS_NAME {
                        return Ok(DEFAULT_CONNECTOR_CLASS_NAME.to_string());
                    }
                }
                let fallback = class_list
                    .items
                    .first()
                    .map(|c| c.name_any())
                    .context("No ConnectorClass available")?;
                warn!(
                    %fallback,
                    "ConnectorClass '{DEFAULT_CONNECTOR_CLASS_NAME}' not found, using '{fallback}'"
                );
                Ok(fallback)
            }
            Ok(_) => {
                warn!("No ConnectorClass found in cluster; using default '{DEFAULT_CONNECTOR_CLASS_NAME}'");
                Ok(DEFAULT_CONNECTOR_CLASS_NAME.to_string())
            }
            Err(e) => {
                warn!("Failed to list ConnectorClasses (using default '{DEFAULT_CONNECTOR_CLASS_NAME}'): {e:#}");
                Ok(DEFAULT_CONNECTOR_CLASS_NAME.to_string())
            }
        }
    }

    async fn ensure_connector(&self, project_id: &str) -> Result<Connector> {
        let pcp = self.datum.project_control_plane_client(project_id).await?;
        let client = pcp.client();
        let connectors: Api<Connector> = Api::namespaced(client.clone(), DEFAULT_PCP_NAMESPACE);

        // Reuse an existing connector rather than deleting and recreating it.
        // Delete-and-recreate causes a new generation-1 object; the replicator
        // mirrors the status annotation once (at Ready:False while the lease
        // hasn't renewed yet) and then never re-mirrors when Ready flips to
        // True, so the extension server permanently sees the connector as
        // offline and returns 503. Patching in-place keeps the same generation
        // and the replicator re-mirrors on spec changes, avoiding the race.
        if let Some(connector) = self.find_connector(project_id).await? {
            let name = connector.name_any();
            debug!(%name, "reusing existing connector, patching connectionDetails");
            if let Some(details) = build_connection_details(&self.listen) {
                let details_value = serde_json::to_value(details)
                    .std_context("Failed to serialize connection details")?;
                let patch = json!({ "status": { "connectionDetails": details_value } });
                if let Err(err) = connectors
                    .patch_status(&name, &PatchParams::default(), &Patch::Merge(&patch))
                    .await
                {
                    warn!(%name, "Failed to patch connector connectionDetails: {err:#}");
                }
            } else {
                warn!(%name, "Missing connection details for connector status patch");
            }
            return Ok(connector);
        }

        let class_name = Self::resolve_connector_class(client).await?;

        let mut connector = Connector {
            metadata: ObjectMeta {
                generate_name: Some("datum-connect-".to_string()),
                annotations: Some(device_annotations()),
                ..Default::default()
            },
            spec: ConnectorSpec {
                connector_class_name: class_name,
                capabilities: None,
            },
            status: None,
        };
        let conn_post = PostParams::default();
        connector = with_quota_check_retry("Connector create", || {
            connectors.create(&conn_post, &connector)
        })
        .await
        .std_context("Failed to create Connector")?;

        if let Some(details) = build_connection_details(&self.listen) {
            let details_value = serde_json::to_value(details)
                .std_context("Failed to serialize connection details")?;
            let patch = json!({ "status": { "connectionDetails": details_value } });
            if let Err(err) = connectors
                .patch_status(
                    &connector.name_any(),
                    &PatchParams::default(),
                    &Patch::Merge(&patch),
                )
                .await
            {
                warn!(connector = %connector.name_any(), "Failed to patch connector status: {err:#}");
            }
        } else {
            warn!(connector = %connector.name_any(), "Missing connection details for connector status");
        }

        Ok(connector)
    }
}

#[derive(Debug, Clone)]
struct ParsedTarget {
    address: String,
    port: u16,
}

fn parse_target(target: &str) -> Result<ParsedTarget> {
    let target = target.trim();
    if let Ok(url) = url::Url::parse(target) {
        let host = url.host_str().context("missing host")?;
        let port = url.port().context("missing port")?;
        return Ok(ParsedTarget {
            address: host.to_string(),
            port,
        });
    }

    let (host, port_str) = if target.starts_with('[') {
        let end = target.find(']').context("invalid IPv6 address")?;
        let host = &target[1..end];
        let port = target
            .get(end + 1..)
            .and_then(|rest| rest.strip_prefix(':'))
            .context("missing port")?;
        (host, port)
    } else {
        let (host, port) = target.rsplit_once(':').context("missing port")?;
        (host, port)
    };
    let port: u16 = port_str.parse().std_context("invalid port")?;
    Ok(ParsedTarget {
        address: host.to_string(),
        port,
    })
}

fn build_connection_details(listen: &ListenNode) -> Option<ConnectorConnectionDetails> {
    let endpoint = listen.endpoint();
    let endpoint_addr = endpoint.addr();
    let home_relay = endpoint_addr.relay_urls().next()?.to_string();
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

/// Normalizes an endpoint string by ensuring it has an `http://` scheme prefix.
/// Returns the input unchanged if it already contains `://`.
pub fn normalize_endpoint(endpoint: &str) -> String {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return endpoint.to_string();
    }
    if endpoint.contains("://") {
        return endpoint.to_string();
    }
    format!("http://{endpoint}")
}

fn strip_scheme(endpoint: &str) -> String {
    if let Ok(url) = url::Url::parse(endpoint)
        && let Some(host) = url.host_str()
        && let Some(port) = url.port()
    {
        return format!("{host}:{port}");
    }
    endpoint.to_string()
}

fn proxy_hostnames(proxy: &HTTPProxy) -> Vec<String> {
    proxy
        .status
        .as_ref()
        .and_then(|status| status.hostnames.clone())
        .or_else(|| proxy.spec.hostnames.clone())
        .unwrap_or_default()
}

/// True when the HTTPProxy's display label annotation and rules already
/// match what `update_project` would write. Used to short-circuit the
/// PATCH so a no-op update doesn't bump `metadata.generation` and trigger
/// a downstream Envoy re-reconcile (see the resume-induced 5xx window).
fn http_proxy_spec_matches(
    existing: &HTTPProxy,
    desired_label: &str,
    desired_rules: &[HTTPProxyRule],
) -> bool {
    let existing_label = existing
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(DISPLAY_NAME_ANNOTATION))
        .map(String::as_str);
    if existing_label != Some(desired_label) {
        return false;
    }
    // Compare via serde Value rather than structural equality on the Rust
    // types so we get a stable representation that doesn't drift when
    // Option<...> fields with serde defaults serialize differently.
    let Ok(existing_rules_value) = serde_json::to_value(&existing.spec.rules) else {
        return false;
    };
    let Ok(desired_rules_value) = serde_json::to_value(desired_rules) else {
        return false;
    };
    existing_rules_value == desired_rules_value
}

/// True when the ConnectorAdvertisement's spec already matches what
/// `update_project` would write. Same idempotency motivation as
/// `http_proxy_spec_matches`.
fn advertisement_spec_matches(
    existing: &ConnectorAdvertisement,
    desired: &ConnectorAdvertisementSpec,
) -> bool {
    let Ok(existing_value) = serde_json::to_value(&existing.spec) else {
        return false;
    };
    let Ok(desired_value) = serde_json::to_value(desired) else {
        return false;
    };
    existing_value == desired_value
}

/// Extract the connector name from the first backend that references one.
fn proxy_connector_name(proxy: &HTTPProxy) -> Option<String> {
    proxy
        .spec
        .rules
        .iter()
        .flat_map(|rule| rule.backends.iter().flatten())
        .find_map(|backend| backend.connector.as_ref().map(|c| c.name.clone()))
}

/// Rule that matches requests with x-forwarded-proto: http and redirects to HTTPS (301).
/// Evaluated first so HTTP traffic is upgraded before hitting the backend rule.
fn https_redirect_rule() -> HTTPProxyRule {
    HTTPProxyRule {
        name: None,
        matches: vec![HTTPRouteMatch {
            path: Some(HTTPRouteRulesMatchesPath {
                r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
                value: Some("/".to_string()),
            }),
            headers: Some(vec![HTTPRouteRulesMatchesHeaders {
                name: "x-forwarded-proto".to_string(),
                r#type: Some(HTTPRouteRulesMatchesHeadersType::Exact),
                value: "http".to_string(),
            }]),
            ..Default::default()
        }],
        filters: Some(vec![crate::datum_apis::http_proxy::HTTPRouteRulesFilters {
            request_redirect: Some(crate::datum_apis::http_proxy::HTTPRouteRulesFiltersRequestRedirect {
                scheme: Some("https".to_string()),
                status_code: Some(301),
                hostname: None,
                path: None,
                port: None,
            }),
            r#type: HTTPRouteRulesFiltersType::RequestRedirect,
            extension_ref: None,
            request_header_modifier: None,
            request_mirror: None,
            response_header_modifier: None,
            url_rewrite: None,
        }]),
        backends: None,
    }
}

fn proxy_rule(endpoint: &str, connector_name: &str) -> HTTPProxyRule {
    HTTPProxyRule {
        name: None,
        matches: vec![default_match()],
        filters: None,
        backends: Some(vec![HTTPProxyRuleBackend {
            endpoint: endpoint.to_string(),
            connector: Some(ConnectorReference {
                name: connector_name.to_string(),
            }),
            filters: None,
        }]),
    }
}

fn proxy_backend_endpoint(proxy: &HTTPProxy) -> Option<String> {
    proxy
        .spec
        .rules
        .iter()
        .find_map(|rule| rule.backends.as_ref().and_then(|b| b.first()))
        .map(|backend| backend.endpoint.clone())
}

fn advertisement_spec(connector_name: &str, target: ParsedTarget) -> ConnectorAdvertisementSpec {
    let port_name = format!("tcp-{}", target.port);
    ConnectorAdvertisementSpec {
        connector_ref: crate::datum_apis::connector::LocalConnectorReference {
            name: connector_name.to_string(),
        },
        layer4: Some(vec![ConnectorAdvertisementLayer4 {
            name: "default".to_string(),
            services: vec![ConnectorAdvertisementLayer4Service {
                address: Layer4ServiceAddress(target.address),
                ports: vec![Layer4ServicePort {
                    name: port_name,
                    port: target.port as i32,
                    protocol: Protocol::Tcp,
                }],
            }],
        }]),
    }
}

fn default_match() -> HTTPRouteMatch {
    HTTPRouteMatch {
        path: Some(HTTPRouteRulesMatchesPath {
            r#type: Some(HTTPRouteRulesMatchesPathType::PathPrefix),
            value: Some("/".to_string()),
        }),
        ..Default::default()
    }
}

pub fn friendly_device_name() -> String {
    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = std::process::Command::new("scutil")
            .arg("--get")
            .arg("ComputerName")
            .output()
        {
            if output.status.success() {
                let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !name.is_empty() {
                    return name;
                }
            }
        }
    }
    let hostname = gethostname::gethostname().to_string_lossy().into_owned();
    hostname
        .strip_suffix(".local")
        .unwrap_or(&hostname)
        .to_string()
}

const DEVICE_NAME_ANNOTATION: &str = "datum.net/device-name";
const DEVICE_OS_ANNOTATION: &str = "datum.net/device-os";

fn device_annotations() -> BTreeMap<String, String> {
    BTreeMap::from([
        (DEVICE_NAME_ANNOTATION.to_string(), friendly_device_name()),
        (
            DEVICE_OS_ANNOTATION.to_string(),
            std::env::consts::OS.to_string(),
        ),
    ])
}

async fn patch_device_annotations(api: &Api<Connector>, connector: &mut Connector) {
    let expected = device_annotations();
    let current = connector.metadata.annotations.as_ref();
    let needs_patch = expected.iter().any(|(k, v)| {
        current
            .and_then(|a| a.get(k))
            .map(|cv| cv != v)
            .unwrap_or(true)
    });
    if !needs_patch {
        return;
    }
    let patch = json!({ "metadata": { "annotations": expected } });
    match api
        .patch(
            &connector.name_any(),
            &PatchParams::default(),
            &Patch::Merge(&patch),
        )
        .await
    {
        Ok(patched) => *connector = patched,
        Err(err) => {
            warn!(
                connector = %connector.name_any(),
                "Failed to patch device annotations: {err:#}"
            );
        }
    }
}

fn format_quota_error(err: &dyn std::error::Error, resource_type: &str) -> Option<String> {
    let err_msg = err.to_string();
    // Transient quota-check timeout — the error literally says "Please try
    // again in a moment". Don't relabel it as "exceeded"; with the retry
    // wrapper applied at creation sites we'll usually never get here, and
    // when we do the original message is the most accurate signal.
    if err_msg.contains("took too long to be checked against your quota") {
        return None;
    }
    if err_msg.contains("quota") || err_msg.contains("Insufficient quota") {
        return Some(format!(
            "Quota limit exceeded for {resource_type} resources.\n\n\
            You've reached the limit for creating {resource_type} resources in this project.\n\n\
            To fix this, you can:\n  \
            - Delete unused tunnels to free up capacity\n  \
            - Contact support to request a higher quota limit\n\n\
            Run 'tunnel list' to see existing tunnels."
        ));
    }
    None
}



/// Retry a kube API call up to ~15 seconds while it keeps tripping the
/// operator's quota-check timeout. Other errors return immediately so
/// real failures still surface fast. Prints a one-line stderr notice on
/// the first retry so the user knows we're waiting on the server.
async fn with_quota_check_retry<T, F, Fut>(op_name: &str, mut f: F) -> kube::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = kube::Result<T>>,
{
    let delays = [
        std::time::Duration::from_secs(1),
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(4),
        std::time::Duration::from_secs(8),
    ];
    for (i, delay) in delays.iter().enumerate() {
        match f().await {
            Ok(v) => return Ok(v),
            Err(err) if is_quota_check_timeout(&err) => {
                if i == 0 {
                    eprintln!(
                        "  … quota check timed out for {op_name}; retrying for up to 15s"
                    );
                }
                warn!(
                    op = op_name,
                    attempt = i + 1,
                    next_delay_s = delay.as_secs(),
                    "quota check timed out; retrying"
                );
                tokio::time::sleep(*delay).await;
            }
            Err(err) => return Err(err),
        }
    }
    f().await
}

fn publish_tickets_enabled() -> bool {
    std::env::var("DATUM_CONNECT_PUBLISH_TICKETS")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn create_traffic_protection_policies_enabled() -> bool {
    std::env::var("DATUM_CONNECT_CREATE_TRAFFIC_PROTECTION_POLICIES")
        .ok()
        .or_else(|| {
            option_env!("BUILD_DATUM_CONNECT_CREATE_TRAFFIC_PROTECTION_POLICIES")
                .map(str::to_string)
        })
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datum_apis::connector::{ConnectorSpec, ConnectorStatus};
    use crate::datum_apis::http_proxy::{HTTPProxySpec, HTTPProxyStatus};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
    use kube::api::ObjectMeta;

    fn cond(type_: &str, status: &str, reason: &str, message: &str) -> Condition {
        Condition {
            type_: type_.to_string(),
            status: status.to_string(),
            reason: reason.to_string(),
            message: message.to_string(),
            last_transition_time: Time(chrono::DateTime::UNIX_EPOCH),
            observed_generation: None,
        }
    }

    fn proxy(conds: Vec<Condition>) -> HTTPProxy {
        let mut p = HTTPProxy::new(
            "tunnel-test",
            HTTPProxySpec {
                hostnames: None,
                rules: vec![],
            },
        );
        p.metadata = ObjectMeta {
            name: Some("tunnel-test".into()),
            ..Default::default()
        };
        p.status = Some(HTTPProxyStatus {
            addresses: None,
            hostnames: Some(vec!["ground-pearl.datumproxy.net".into()]),
            conditions: Some(conds),
        });
        p
    }

    fn connector(conds: Vec<Condition>) -> Connector {
        let mut c = Connector::new(
            "datum-connect-test",
            ConnectorSpec {
                connector_class_name: "datum-connect".into(),
                capabilities: None,
            },
        );
        c.status = Some(ConnectorStatus {
            capabilities: None,
            conditions: Some(conds),
            connection_details: None,
            lease_ref: None,
        });
        c
    }

    #[test]
    fn progress_unknown_when_controllers_silent() {
        let p = proxy(vec![]);
        let progress = TunnelProgress::from_resources(&p, None);
        assert_eq!(progress.steps.len(), 6);
        assert!(
            progress.steps.iter().all(|s| s.status == StepStatus::Unknown),
            "no conditions yet → every step Unknown"
        );
        assert!(!progress.all_ready());
        assert!(progress.terminal_failure().is_none());
    }

    #[test]
    fn progress_all_ready_when_every_condition_true() {
        let p = proxy(vec![
            cond(HTTP_PROXY_CONDITION_ACCEPTED, "True", "Accepted", ""),
            cond(HTTP_PROXY_CONDITION_CERTIFICATES_READY, "True", "AllCertificatesReady", ""),
            cond(HTTP_PROXY_CONDITION_PROGRAMMED, "True", "Programmed", ""),
            cond(
                HTTP_PROXY_CONDITION_CONNECTOR_METADATA_PROGRAMMED,
                "True",
                "ConnectorMetadataApplied",
                "",
            ),
        ]);
        let c = connector(vec![
            cond(CONNECTOR_CONDITION_READY, "True", "ConnectorReady", ""),
            cond(CONNECTOR_CONDITION_IROH_DNS_PUBLISHED, "True", "Owner", ""),
        ]);
        let progress = TunnelProgress::from_resources(&p, Some(&c));
        assert!(progress.all_ready());
        assert!(progress.terminal_failure().is_none());
    }

    #[test]
    fn progress_flags_deferred_to_owner_as_terminal() {
        // This is the silent-tunnel failure: the iroh DNS record is owned by
        // a different project's Connector. Waiting longer won't help — the
        // CLI must bail and surface the owner so the user can act.
        let p = proxy(vec![cond(HTTP_PROXY_CONDITION_ACCEPTED, "True", "Accepted", "")]);
        let owner_msg =
            "iroh DNS record is owned by Connector /other-project/default/datum-connect-xyz";
        let c = connector(vec![
            cond(CONNECTOR_CONDITION_READY, "True", "ConnectorReady", ""),
            cond(
                CONNECTOR_CONDITION_IROH_DNS_PUBLISHED,
                "False",
                CONNECTOR_REASON_DEFERRED_TO_OWNER,
                owner_msg,
            ),
        ]);
        let progress = TunnelProgress::from_resources(&p, Some(&c));
        let fail = progress.terminal_failure().expect("terminal failure detected");
        assert_eq!(fail.kind, ProgressStepKind::IrohDnsPublished);
        assert_eq!(fail.message.as_deref(), Some(owner_msg));
        assert!(!progress.all_ready());
    }

    #[test]
    fn progress_pending_for_false_but_non_terminal_reason() {
        // CertificatesReady=False with reason "Issuing" should stay Pending
        // (still progressing) — not Ready, not terminal.
        let p = proxy(vec![cond(
            HTTP_PROXY_CONDITION_CERTIFICATES_READY,
            "False",
            "Issuing",
            "Certificate request submitted",
        )]);
        let progress = TunnelProgress::from_resources(&p, None);
        let cert_step = progress
            .step(ProgressStepKind::CertificatesReady)
            .expect("step exists");
        assert_eq!(cert_step.status, StepStatus::Pending);
        assert!(progress.terminal_failure().is_none());
    }

    #[test]
    fn progress_step_carries_resource_label() {
        // Every step should know which Kubernetes resource backs it so the
        // CLI can render "[HTTPProxy/tunnel-test]" or
        // "[Connector/datum-connect-test]" alongside the line — that's
        // what the user copy-pastes into `datumctl describe`.
        let p = proxy(vec![]);
        let c = connector(vec![]);
        let progress = TunnelProgress::from_resources(&p, Some(&c));

        for step in &progress.steps {
            let resource = step.resource.as_deref().expect("resource label set");
            let expected_kind = step.kind.resource_kind();
            assert!(
                resource.starts_with(&format!("{expected_kind}/")),
                "step {:?} should be backed by {expected_kind}, got {resource}",
                step.kind,
            );
        }

        // Connector-backed steps fall back to None when no connector exists.
        let progress_no_conn = TunnelProgress::from_resources(&p, None);
        let iroh = progress_no_conn
            .step(ProgressStepKind::IrohDnsPublished)
            .unwrap();
        assert!(
            iroh.resource.is_none(),
            "connector-backed step has no resource when connector is missing"
        );
        let proxy_step = progress_no_conn
            .step(ProgressStepKind::ProxyAccepted)
            .unwrap();
        assert_eq!(
            proxy_step.resource.as_deref(),
            Some("HTTPProxy/tunnel-test")
        );
    }

    fn api_error(code: u16, message: &str) -> kube::Error {
        kube::Error::Api(kube::core::ErrorResponse {
            status: "Failure".into(),
            message: message.into(),
            reason: if code == 403 { "Forbidden".into() } else { "Unknown".into() },
            code,
        })
    }

    #[test]
    fn quota_check_timeout_classifier_matches_transient_403() {
        // The exact phrase the operator emits when the quota check itself
        // times out — distinct from real quota exhaustion. The error message
        // literally says "Please try again in a moment".
        let err = api_error(
            403,
            "connectoradvertisements.networking.datumapis.com \"tunnel-x\" is forbidden: \
             Your request took too long to be checked against your quota. Please try again \
             in a moment — if this keeps happening, contact support.",
        );
        assert!(is_quota_check_timeout(&err));

        // Real exhaustion shouldn't trigger retry.
        let exhausted = api_error(403, "Insufficient quota for ConnectorAdvertisement");
        assert!(!is_quota_check_timeout(&exhausted));

        // 401 with similar text shouldn't match — different failure class.
        let unauthorized = api_error(401, "took too long to be checked against your quota");
        assert!(!is_quota_check_timeout(&unauthorized));

        // format_quota_error should NOT mangle the timeout message into a
        // misleading "Quota limit exceeded" string.
        assert!(
            format_quota_error(&err, "ConnectorAdvertisement").is_none(),
            "transient timeout must propagate verbatim, not become 'exceeded'"
        );
        // It SHOULD format real exhaustion.
        assert!(format_quota_error(&exhausted, "ConnectorAdvertisement").is_some());
    }

    #[test]
    fn progress_pending_when_status_is_stale_for_current_generation() {
        // `tunnel listen --id` PATCHes the HTTPProxy spec to re-point the
        // backend at the current connector, bumping generation 1 → 2. The
        // controller's prior True conditions still carry observedGeneration=1
        // until it re-reconciles. Treating those as Ready was the bug
        // behind "Tunnel ready after 0 sec" while the edge served 503s
        // for minutes — Envoy was still on the previous-generation config.
        let mut stale = cond(
            HTTP_PROXY_CONDITION_PROGRAMMED,
            "True",
            "Programmed",
            "Stale from previous generation",
        );
        stale.observed_generation = Some(1);
        let mut p_stale = proxy(vec![stale]);
        p_stale.metadata.generation = Some(2);
        let progress_stale = TunnelProgress::from_resources(&p_stale, None);
        let step = progress_stale
            .step(ProgressStepKind::ProxyProgrammed)
            .expect("step exists");
        assert_eq!(
            step.status,
            StepStatus::Pending,
            "True condition with observedGeneration < generation must be Pending"
        );
        assert!(!progress_stale.all_ready());

        // Once the controller observes the new generation, status flips Ready.
        let mut fresh = cond(HTTP_PROXY_CONDITION_PROGRAMMED, "True", "Programmed", "");
        fresh.observed_generation = Some(2);
        let mut p_fresh = proxy(vec![fresh]);
        p_fresh.metadata.generation = Some(2);
        let progress_fresh = TunnelProgress::from_resources(&p_fresh, None);
        assert_eq!(
            progress_fresh
                .step(ProgressStepKind::ProxyProgrammed)
                .unwrap()
                .status,
            StepStatus::Ready,
            "matched observedGeneration must be Ready"
        );
    }

    fn proxy_with_backend(label: &str, endpoint: &str, connector_name: &str) -> HTTPProxy {
        let mut p = HTTPProxy::new(
            "tunnel-test",
            HTTPProxySpec {
                hostnames: Some(vec!["test.datumproxy.net".into()]),
                rules: vec![https_redirect_rule(), proxy_rule(endpoint, connector_name)],
            },
        );
        let mut ann = std::collections::BTreeMap::new();
        ann.insert(DISPLAY_NAME_ANNOTATION.to_string(), label.to_string());
        p.metadata = ObjectMeta {
            name: Some("tunnel-test".into()),
            annotations: Some(ann),
            ..Default::default()
        };
        p
    }

    #[test]
    fn http_proxy_spec_matches_skips_no_op_resume() {
        // The CLI resume path now goes through update_active which calls
        // update_project. When the existing tunnel already points at the
        // current connector with the same endpoint and label, the lib must
        // recognize that and skip the PATCH — sending one would bump
        // metadata.generation and trigger a downstream Envoy re-reconcile.
        let existing =
            proxy_with_backend("my-label", "http://127.0.0.1:11434", "datum-connect-mhxj5");
        let desired_rules = vec![
            https_redirect_rule(),
            proxy_rule("http://127.0.0.1:11434", "datum-connect-mhxj5"),
        ];
        assert!(http_proxy_spec_matches(
            &existing,
            "my-label",
            &desired_rules
        ));
    }

    #[test]
    fn http_proxy_spec_matches_detects_each_drift_axis() {
        let existing =
            proxy_with_backend("my-label", "http://127.0.0.1:11434", "datum-connect-mhxj5");

        // Different connector — adoption across identity change must patch.
        let rules_new_connector = vec![
            https_redirect_rule(),
            proxy_rule("http://127.0.0.1:11434", "datum-connect-NEW"),
        ];
        assert!(!http_proxy_spec_matches(
            &existing,
            "my-label",
            &rules_new_connector
        ));

        // Different endpoint — backend retarget must patch.
        let rules_new_endpoint = vec![
            https_redirect_rule(),
            proxy_rule("http://127.0.0.1:9999", "datum-connect-mhxj5"),
        ];
        assert!(!http_proxy_spec_matches(
            &existing,
            "my-label",
            &rules_new_endpoint
        ));

        // Different label — rename must patch.
        let rules_same = vec![
            https_redirect_rule(),
            proxy_rule("http://127.0.0.1:11434", "datum-connect-mhxj5"),
        ];
        assert!(!http_proxy_spec_matches(
            &existing,
            "different-label",
            &rules_same
        ));

        // No annotation at all — must patch.
        let mut bare = existing.clone();
        bare.metadata.annotations = None;
        assert!(!http_proxy_spec_matches(&bare, "my-label", &rules_same));
    }

    fn target(host: &str, port: u16) -> ParsedTarget {
        ParsedTarget {
            address: host.to_string(),
            port,
        }
    }

    fn advertisement_with_target(connector_name: &str, host: &str, port: u16) -> ConnectorAdvertisement {
        ConnectorAdvertisement {
            metadata: ObjectMeta {
                name: Some("tunnel-test".into()),
                ..Default::default()
            },
            spec: advertisement_spec(connector_name, target(host, port)),
            status: None,
        }
    }

    #[test]
    fn advertisement_spec_matches_skips_no_op() {
        let existing = advertisement_with_target("datum-connect-mhxj5", "127.0.0.1", 11434);
        let desired = advertisement_spec("datum-connect-mhxj5", target("127.0.0.1", 11434));
        assert!(advertisement_spec_matches(&existing, &desired));
    }

    #[test]
    fn advertisement_spec_matches_detects_drift() {
        let existing = advertisement_with_target("datum-connect-mhxj5", "127.0.0.1", 11434);
        let desired_new_port =
            advertisement_spec("datum-connect-mhxj5", target("127.0.0.1", 9999));
        assert!(!advertisement_spec_matches(&existing, &desired_new_port));

        let desired_new_conn =
            advertisement_spec("datum-connect-NEW", target("127.0.0.1", 11434));
        assert!(!advertisement_spec_matches(&existing, &desired_new_conn));
    }
}
