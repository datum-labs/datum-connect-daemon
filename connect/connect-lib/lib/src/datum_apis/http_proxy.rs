use k8s_openapi::apimachinery::pkg::apis::meta::v1 as metav1;
use serde::{Deserialize, Serialize};

pub type Hostname = String;
pub type SectionName = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayStatusAddress {
    pub ip: Option<String>,
    pub hostname: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HTTPRouteRulesMatchesHeaders {
    pub name: String,
    #[serde(rename = "type")]
    pub r#type: Option<HTTPRouteRulesMatchesHeadersType>,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum HTTPRouteRulesMatchesHeadersType {
    Exact,
   RegularExpression,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HTTPRouteRulesMatchesPath {
    #[serde(rename = "type")]
    pub r#type: Option<HTTPRouteRulesMatchesPathType>,
    pub value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum HTTPRouteRulesMatchesPathType {
    PathPrefix,
    Exact,
    RegularExpression,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HTTPRouteMatch {
    pub path: Option<HTTPRouteRulesMatchesPath>,
    pub headers: Option<Vec<HTTPRouteRulesMatchesHeaders>>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub query_params: Option<Vec<HTTPRouteRulesMatchesQueryParams>>,
    #[serde(default)]
    pub time_of_day: Option<Vec<HTTPRouteRulesMatchesTimeOfDay>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HTTPRouteRulesMatchesQueryParams {
    pub name: String,
    #[serde(rename = "type")]
    pub r#type: Option<HTTPRouteRulesMatchesQueryParamsType>,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum HTTPRouteRulesMatchesQueryParamsType {
    Exact,
    RegularExpression,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HTTPRouteRulesMatchesTimeOfDay {
    pub time: String,
    pub modifier: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HTTPRouteRulesFiltersRequestRedirect {
    pub scheme: Option<String>,
    pub status_code: Option<u16>,
    pub hostname: Option<String>,
    pub path: Option<String>,
    pub port: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HTTPRouteRulesFilters {
    pub request_redirect: Option<HTTPRouteRulesFiltersRequestRedirect>,
    #[serde(rename = "type")]
    pub r#type: HTTPRouteRulesFiltersType,
    pub extension_ref: Option<serde_json::Value>,
    pub request_header_modifier: Option<serde_json::Value>,
    pub request_mirror: Option<serde_json::Value>,
    pub response_header_modifier: Option<serde_json::Value>,
    pub url_rewrite: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum HTTPRouteRulesFiltersType {
    RequestRedirect,
    RequestHeaderModifier,
    ResponseHeaderModifier,
    URLRewrite,
    RequestMirror,
    ExtensionRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorReference {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HTTPProxyRuleBackend {
    pub endpoint: String,
    pub connector: Option<ConnectorReference>,
    pub filters: Option<Vec<HTTPRouteRulesFilters>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HTTPProxyRule {
    pub name: Option<SectionName>,
    pub matches: Vec<HTTPRouteMatch>,
    pub filters: Option<Vec<HTTPRouteRulesFilters>>,
    pub backends: Option<Vec<HTTPProxyRuleBackend>>,
}

#[derive(kube::CustomResource, Debug, Clone, Serialize, Deserialize)]
#[kube(
    group = "networking.datumapis.com",
    version = "v1alpha",
    kind = "HTTPProxy",
    plural = "httpproxies",
    namespaced,
    status = "HTTPProxyStatus",
    schema = "disabled"
)]
#[serde(rename_all = "camelCase")]
pub struct HTTPProxySpec {
    pub hostnames: Option<Vec<Hostname>>,
    pub rules: Vec<HTTPProxyRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HTTPProxyStatus {
    pub addresses: Option<Vec<GatewayStatusAddress>>,
    pub hostnames: Option<Vec<Hostname>>,
    pub conditions: Option<Vec<metav1::Condition>>,
}

pub const HTTP_PROXY_CONDITION_ACCEPTED: &str = "Accepted";
pub const HTTP_PROXY_CONDITION_PROGRAMMED: &str = "Programmed";
pub const HTTP_PROXY_CONDITION_HOSTNAMES_VERIFIED: &str = "HostnamesVerified";
pub const HTTP_PROXY_CONDITION_HOSTNAMES_IN_USE: &str = "HostnamesInUse";
pub const HTTP_PROXY_CONDITION_CERTIFICATES_READY: &str = "CertificatesReady";
pub const HTTP_PROXY_CONDITION_CONNECTOR_METADATA_PROGRAMMED: &str = "ConnectorMetadataProgrammed";

pub const HTTP_PROXY_REASON_ACCEPTED: &str = "Accepted";
pub const HTTP_PROXY_REASON_PROGRAMMED: &str = "Programmed";
pub const HTTP_PROXY_REASON_CONFLICT: &str = "Conflict";
pub const HTTP_PROXY_REASON_PENDING: &str = "Pending";
pub const HTTP_PROXY_REASON_HOSTNAMES_VERIFIED: &str = "HostnamesVerified";
pub const HTTP_PROXY_REASON_UNVERIFIED_HOSTNAMES_PRESENT: &str = "UnverifiedHostnamesPresent";
pub const HTTP_PROXY_REASON_HOSTNAME_IN_USE: &str = "HostnameInUse";
