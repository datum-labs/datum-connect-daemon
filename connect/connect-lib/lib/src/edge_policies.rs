//! Edge policies for a tunnel that stays on: Datum's WAF, scoped to the
//! proxy's `protected` rule, and a 1h request timeout.
//!
//! Opt-in, via [`TunnelService::with_edge_policies`]. Only the Home Assistant
//! add-on turns it on (`DATUM_TUNNEL_EDGE_POLICIES`), together with
//! `DATUM_TUNNEL_WAF_EXEMPT_MATCHES`, which gives the proxy the `protected`
//! rule the WAF targets. This used to be curl and jq in the add-on's
//! `run.sh`; it lives here so it runs on every enable rather than once per
//! boot, which makes a transient API failure at boot self-healing.
//!
//! Both policies are created only when missing and never overwritten: one
//! that exists may have been tuned in the portal, and a restart must not undo
//! that. The one exception is the switched-off WAF policy add-on 0.1.5/0.1.6
//! created, recognised strictly by [`is_legacy_addon_waf`] and moved to the
//! current setup once.
//!
//! Every failure is a logged warning. The tunnel already works without these
//! policies, and taking it down because one could not be set up helps no one.
//!
//! The policies are handled as `DynamicObject`s rather than typed resources.
//! For the backend traffic policy that saves adding a CRD type for one
//! field; for the WAF it keeps every field the server returns, so the legacy
//! check compares whole objects exactly as the add-on's jq did, and the
//! upgrade round-trips anything this crate has no type for.
//!
//! [`TunnelService::with_edge_policies`]: crate::TunnelService::with_edge_policies

use std::future::Future;
use std::time::Duration;

use kube::api::{Api, ApiResource, DynamicObject, GroupVersionKind, PostParams};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::DEFAULT_PCP_NAMESPACE;
use crate::datum_apis::http_proxy::HTTPProxyRule;
use crate::tunnels::WAF_PROTECTED_RULE_NAME;

/// Marks a policy as written by this code. A WAF policy without it is never
/// changed (bar the one legacy upgrade). The value predates the move into the
/// daemon and must stay as is, or policies made by add-on 0.1.7 would read as
/// someone else's.
pub const MANAGED_BY_ANNOTATION: &str = "connect.datum.net/managed-by";
pub const MANAGED_BY_VALUE: &str = "datum-connect-addon";
const DISPLAY_NAME_ANNOTATION: &str = "networking.datumapis.com/display-name";
const HTTP_ROUTE_GROUP: &str = "gateway.networking.k8s.io";

/// The most the platform allows. Datum's edge (Envoy) otherwise ends every
/// response 15s after the request, which cuts Home Assistant's live views
/// and any long download.
pub const REQUEST_TIMEOUT: &str = "1h";

/// Per API call, matching the add-on's `curl -m 30`.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest response excerpt quoted in a log line. Error bodies can be whole
/// HTML pages, and one dumped in full once buried the rest of the log.
const EXCERPT_MAX_CHARS: usize = 200;

pub fn waf_policy_name(tunnel_id: &str) -> String {
    format!("{tunnel_id}-waf")
}

pub fn timeout_policy_name(tunnel_id: &str) -> String {
    format!("{tunnel_id}-timeout")
}

fn waf_resource() -> ApiResource {
    ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk("networking.datumapis.com", "v1alpha", "TrafficProtectionPolicy"),
        "trafficprotectionpolicies",
    )
}

fn timeout_resource() -> ApiResource {
    ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk("gateway.envoyproxy.io", "v1alpha1", "BackendTrafficPolicy"),
        "backendtrafficpolicies",
    )
}

/// The WAF policy's spec, shared by a fresh create and the legacy upgrade.
///
/// Paranoia level 1 with rule 920420 excluded is the setting validated on a
/// real device when enforced (remote login, and saving an automation with a
/// template condition, over cellular). 920420 has to go at every level: Home
/// Assistant's login page POSTs JSON as text/plain, which CRS v4 rejects, so
/// login fails. Attacks in text/plain bodies are still caught by the other
/// rules. Level 2 also blocks any {{ }} template in a REST body, which breaks
/// template automations, so it stays off until it has targeted exclusions.
///
/// Scoped by `sectionName` to the proxy's `protected` rule, so the `streams`
/// rule stays outside it: a WAF on a stream makes Datum's edge hold the
/// response back (datum-cloud/infra#6677).
pub fn desired_waf_spec(tunnel_id: &str) -> Value {
    json!({
        "mode": "Enforce",
        "samplingPercentage": 100,
        "ruleSets": [{
            "type": "OWASPCoreRuleSet",
            "owaspCoreRuleSet": {
                "paranoiaLevels": {"blocking": 1, "detection": 1},
                "scoreThresholds": {"inbound": 5, "outbound": 4},
                "ruleExclusions": {"ids": [920420]}
            }
        }],
        "targetRefs": [{
            "group": HTTP_ROUTE_GROUP,
            "kind": "HTTPRoute",
            "name": tunnel_id,
            "sectionName": WAF_PROTECTED_RULE_NAME
        }]
    })
}

/// The whole route, not just `protected`: the timeout matters most for the
/// streams.
pub fn desired_timeout_spec(tunnel_id: &str) -> Value {
    json!({
        "targetRefs": [{"group": HTTP_ROUTE_GROUP, "kind": "HTTPRoute", "name": tunnel_id}],
        "timeout": {"http": {"requestTimeout": REQUEST_TIMEOUT}}
    })
}

fn has_managed_by(policy: &Value) -> bool {
    policy
        .pointer("/metadata/annotations")
        .and_then(Value::as_object)
        .is_some_and(|a| a.contains_key(MANAGED_BY_ANNOTATION))
}

/// JSON equality with numbers compared by value, as jq does, so `1` and
/// `1.0` match.
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| json_eq(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| json_eq(v, w)))
        }
        _ => a == b,
    }
}

/// jq's `//`: a missing, null or false value falls back.
fn or_default<'a>(value: Option<&'a Value>, default: &'a Value) -> &'a Value {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => default,
        Some(v) => v,
    }
}

/// True only for the exact policy add-on 0.1.5 or 0.1.6 created: switched
/// off, aimed at the whole route, without the managed-by annotation, and
/// with that rule set unchanged. Anything else may have been chosen in the
/// portal. A straight port of the add-on's `is_addon_015_waf` jq filter.
pub fn is_legacy_addon_waf(policy: &Value, tunnel_id: &str) -> bool {
    let spec = &policy["spec"];
    let refs = spec["targetRefs"].as_array().map(Vec::as_slice).unwrap_or_default();
    let rule_sets = spec["ruleSets"].as_array().map(Vec::as_slice).unwrap_or_default();
    let crs = &spec["ruleSets"][0]["owaspCoreRuleSet"];
    spec["mode"] == "Disabled"
        && !has_managed_by(policy)
        && json_eq(or_default(spec.get("samplingPercentage"), &json!(100)), &json!(100))
        && refs.len() == 1
        && refs[0]["kind"] == "HTTPRoute"
        && refs[0]["name"] == tunnel_id
        && or_default(refs[0].get("sectionName"), &json!("")) == ""
        && rule_sets.len() == 1
        && rule_sets[0]["type"] == "OWASPCoreRuleSet"
        && json_eq(&crs["paranoiaLevels"], &json!({"blocking": 1, "detection": 1}))
        && json_eq(&crs["scoreThresholds"], &json!({"inbound": 5, "outbound": 4}))
        && json_eq(&crs["ruleExclusions"], &json!({"ids": [920420]}))
}

/// What to do with a WAF policy that already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingWaf {
    /// Set up by this code: kept as set, tuned or not.
    Managed,
    /// Add-on 0.1.5/0.1.6's switched-off policy: upgraded once.
    Legacy,
    /// Made by someone else: never touched.
    Foreign,
}

pub fn classify_existing_waf(policy: &Value, tunnel_id: &str) -> ExistingWaf {
    if has_managed_by(policy) {
        ExistingWaf::Managed
    } else if is_legacy_addon_waf(policy, tunnel_id) {
        ExistingWaf::Legacy
    } else {
        ExistingWaf::Foreign
    }
}

/// The WAF follows the `protected` rule by name, so a proxy without one has
/// no WAF at all.
pub fn has_protected_rule(rules: &[HTTPProxyRule]) -> bool {
    rules
        .iter()
        .any(|r| r.name.as_deref() == Some(WAF_PROTECTED_RULE_NAME))
}

fn new_waf_policy(tunnel_id: &str, label: &str) -> DynamicObject {
    let mut policy = DynamicObject::new(&waf_policy_name(tunnel_id), &waf_resource())
        .within(DEFAULT_PCP_NAMESPACE)
        .data(json!({ "spec": desired_waf_spec(tunnel_id) }));
    policy.metadata.annotations = Some(
        [
            (DISPLAY_NAME_ANNOTATION.to_string(), label.to_string()),
            (MANAGED_BY_ANNOTATION.to_string(), MANAGED_BY_VALUE.to_string()),
        ]
        .into(),
    );
    policy
}

/// The legacy policy moved to the current spec in place. It keeps the
/// `resourceVersion` it was read with, so the replace loses (409) to an edit
/// made in between rather than overwriting it.
pub fn upgraded_waf_policy(mut existing: DynamicObject, tunnel_id: &str) -> DynamicObject {
    if let Some(data) = existing.data.as_object_mut() {
        data.remove("status");
        data.insert("spec".to_string(), desired_waf_spec(tunnel_id));
    }
    existing
        .metadata
        .annotations
        .get_or_insert_with(Default::default)
        .insert(MANAGED_BY_ANNOTATION.to_string(), MANAGED_BY_VALUE.to_string());
    existing
}

fn new_timeout_policy(tunnel_id: &str) -> DynamicObject {
    DynamicObject::new(&timeout_policy_name(tunnel_id), &timeout_resource())
        .within(DEFAULT_PCP_NAMESPACE)
        .data(json!({ "spec": desired_timeout_spec(tunnel_id) }))
}

/// One tunnel's identity, as the policies and log lines need it.
#[derive(Debug, Clone)]
pub struct EdgeTarget {
    pub project_id: String,
    pub tunnel_id: String,
    pub label: String,
    /// The Datum portal, for the "manage it in the portal" hint.
    pub portal_url: String,
}

impl EdgeTarget {
    fn portal_link(&self) -> String {
        format!(
            "{} (project {}, policy {})",
            self.portal_url,
            self.project_id,
            waf_policy_name(&self.tunnel_id)
        )
    }
}

/// An API call that did not succeed, reduced to what a log line needs.
#[derive(Debug)]
enum CallError {
    Status(u16, String),
    NoResponse(String),
}

impl CallError {
    fn code(&self) -> Option<u16> {
        match self {
            CallError::Status(code, _) => Some(*code),
            CallError::NoResponse(_) => None,
        }
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Status(code, body) => write!(f, "HTTP {code}: {}", excerpt(body)),
            CallError::NoResponse(e) => write!(f, "no response: {}", excerpt(e)),
        }
    }
}

/// The start of a failed response, on one line.
fn excerpt(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(EXCERPT_MAX_CHARS) {
        Some((cut, _)) => format!("{}...", &text[..cut]),
        None => text,
    }
}

async fn call<T>(fut: impl Future<Output = kube::Result<T>>) -> Result<T, CallError> {
    match tokio::time::timeout(CALL_TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(kube::Error::Api(e))) => Err(CallError::Status(e.code, e.message)),
        Ok(Err(e)) => Err(CallError::NoResponse(e.to_string())),
        Err(_) => Err(CallError::NoResponse(format!("timed out after {CALL_TIMEOUT:?}"))),
    }
}

fn is_denied(err: &CallError) -> bool {
    matches!(err.code(), Some(401 | 403))
}

/// Ensures both policies for one tunnel. Never fails: every problem is
/// logged as a warning, and the next enable tries again.
///
/// `proxy_has_protected_rule` is about the rules actually on the proxy, as
/// just written or confirmed by the caller.
pub async fn ensure_edge_policies(
    client: kube::Client,
    target: EdgeTarget,
    proxy_has_protected_rule: bool,
) {
    ensure_request_timeout(&client, &target).await;
    ensure_waf(&client, &target, proxy_has_protected_rule).await;
}

async fn ensure_request_timeout(client: &kube::Client, t: &EdgeTarget) {
    let api: Api<DynamicObject> =
        Api::namespaced_with(client.clone(), DEFAULT_PCP_NAMESPACE, &timeout_resource());
    let name = timeout_policy_name(&t.tunnel_id);
    match call(api.get_opt(&name)).await {
        Ok(Some(_)) => {
            info!("Edge request timeout set (existing policy {name} kept)");
            return;
        }
        Ok(None) => {}
        Err(e) => {
            warn!("Edge request timeout NOT raised: could not check for policy {name} ({e})");
            return;
        }
    }
    match call(api.create(&PostParams::default(), &new_timeout_policy(&t.tunnel_id))).await {
        Ok(_) => info!("Edge request timeout raised to {REQUEST_TIMEOUT} (policy {name})"),
        // Created between the check and here, by a concurrent enable.
        Err(CallError::Status(409, _)) => {
            info!("Edge request timeout set (existing policy {name} kept)")
        }
        Err(e) if is_denied(&e) => warn!(
            "Edge request timeout NOT raised: the service account may not create traffic policies in project {} ({e}). Give it that permission, then restart the add-on.",
            t.project_id
        ),
        Err(e) => warn!("Edge request timeout NOT raised: creating policy {name} failed ({e})"),
    }
}

async fn ensure_waf(client: &kube::Client, t: &EdgeTarget, proxy_has_protected_rule: bool) {
    if !proxy_has_protected_rule {
        warn!(
            "!!! Edge protection is OFF: tunnel {} has no rule named '{WAF_PROTECTED_RULE_NAME}', so its WAF matches nothing. Restart the add-on; if this persists, report it.",
            t.tunnel_id
        );
    }

    let api: Api<DynamicObject> =
        Api::namespaced_with(client.clone(), DEFAULT_PCP_NAMESPACE, &waf_resource());
    let name = waf_policy_name(&t.tunnel_id);
    let portal = t.portal_link();
    let existing = match call(api.get_opt(&name)).await {
        Ok(existing) => existing,
        Err(e) => {
            warn!("Edge protection NOT set up: could not check for policy {name} ({e})");
            return;
        }
    };

    if let Some(existing) = existing {
        let as_json = serde_json::to_value(&existing).unwrap_or_default();
        match classify_existing_waf(&as_json, &t.tunnel_id) {
            ExistingWaf::Managed => info!(
                "Edge protection policy found (Datum WAF, set up by this add-on, kept as set). Manage it in the Datum portal: {portal}"
            ),
            ExistingWaf::Foreign => info!(
                "Edge protection policy found, not set up by this add-on, so kept as set. Unless it targets rule '{WAF_PROTECTED_RULE_NAME}', an enforcing WAF there also holds back live views. Manage it in the Datum portal: {portal}"
            ),
            ExistingWaf::Legacy => {
                let upgraded = upgraded_waf_policy(existing, &t.tunnel_id);
                match call(api.replace(&name, &PostParams::default(), &upgraded)).await {
                    Ok(_) => info!(
                        "Edge protection policy from add-on 0.1.5/0.1.6 switched ON: Datum WAF now enforces on everything except Home Assistant's streaming endpoints. Manage it in the Datum portal: {portal}"
                    ),
                    Err(CallError::Status(409, _)) => warn!(
                        "Edge protection still OFF: policy {name} changed while being upgraded, so it was left as is. Restart the add-on to try again."
                    ),
                    Err(e) if is_denied(&e) => warn!(
                        "Edge protection still OFF: the service account may not update WAF policies in project {} ({e}). Give it that permission, then restart the add-on.",
                        t.project_id
                    ),
                    Err(e) => warn!("Edge protection still OFF: upgrading policy {name} failed ({e})"),
                }
            }
        }
        return;
    }

    match call(api.create(&PostParams::default(), &new_waf_policy(&t.tunnel_id, &t.label))).await {
        Ok(_) => info!(
            "Edge protection policy created: Datum WAF on (Enforce) for everything except Home Assistant's streaming endpoints. Manage it in the Datum portal: {portal}"
        ),
        // Created between the check and here, most likely by a concurrent
        // enable. Whoever made it, it exists now and is kept.
        Err(CallError::Status(409, _)) => info!(
            "Edge protection policy found (created moments ago, kept as set). Manage it in the Datum portal: {portal}"
        ),
        Err(e) if is_denied(&e) => warn!(
            "Edge protection NOT set up: the service account may not create WAF policies in project {} ({e}). Give it that permission, then restart the add-on.",
            t.project_id
        ),
        Err(e) => warn!("Edge protection NOT set up: creating policy {name} failed ({e})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TUNNEL: &str = "tunnel-abc12";

    /// The policy add-on 0.1.5/0.1.6 created, as the server returns it.
    fn legacy_policy() -> Value {
        json!({
            "apiVersion": "networking.datumapis.com/v1alpha",
            "kind": "TrafficProtectionPolicy",
            "metadata": {
                "name": "tunnel-abc12-waf",
                "namespace": "default",
                "resourceVersion": "4242",
                "annotations": {"networking.datumapis.com/display-name": "home-assistant"}
            },
            "spec": {
                "mode": "Disabled",
                "samplingPercentage": 100,
                "ruleSets": [{
                    "type": "OWASPCoreRuleSet",
                    "owaspCoreRuleSet": {
                        "paranoiaLevels": {"blocking": 1, "detection": 1},
                        "scoreThresholds": {"inbound": 5, "outbound": 4},
                        "ruleExclusions": {"ids": [920420]}
                    }
                }],
                "targetRefs": [{"group": "gateway.networking.k8s.io", "kind": "HTTPRoute", "name": TUNNEL}]
            },
            "status": {"ancestors": []}
        })
    }

    fn tweaked(f: impl FnOnce(&mut Value)) -> Value {
        let mut p = legacy_policy();
        f(&mut p);
        p
    }

    #[test]
    fn waf_spec_targets_protected_rule_by_shared_constant() {
        let spec = desired_waf_spec(TUNNEL);
        assert_eq!(spec["mode"], "Enforce");
        assert_eq!(spec["samplingPercentage"], 100);
        assert_eq!(
            spec["targetRefs"],
            json!([{
                "group": "gateway.networking.k8s.io",
                "kind": "HTTPRoute",
                "name": TUNNEL,
                "sectionName": WAF_PROTECTED_RULE_NAME
            }])
        );
        let crs = &spec["ruleSets"][0];
        assert_eq!(crs["type"], "OWASPCoreRuleSet");
        assert_eq!(
            crs["owaspCoreRuleSet"],
            json!({
                "paranoiaLevels": {"blocking": 1, "detection": 1},
                "scoreThresholds": {"inbound": 5, "outbound": 4},
                "ruleExclusions": {"ids": [920420]}
            })
        );
    }

    #[test]
    fn new_waf_policy_carries_name_and_annotations() {
        let v = serde_json::to_value(new_waf_policy(TUNNEL, "home-assistant")).unwrap();
        assert_eq!(v["apiVersion"], "networking.datumapis.com/v1alpha");
        assert_eq!(v["kind"], "TrafficProtectionPolicy");
        assert_eq!(v["metadata"]["name"], "tunnel-abc12-waf");
        assert_eq!(v["metadata"]["namespace"], "default");
        assert_eq!(
            v["metadata"]["annotations"],
            json!({
                "networking.datumapis.com/display-name": "home-assistant",
                "connect.datum.net/managed-by": "datum-connect-addon"
            })
        );
        assert_eq!(v["spec"], desired_waf_spec(TUNNEL));
        assert_eq!(classify_existing_waf(&v, TUNNEL), ExistingWaf::Managed);
    }

    #[test]
    fn timeout_policy_targets_whole_route_for_an_hour() {
        let v = serde_json::to_value(new_timeout_policy(TUNNEL)).unwrap();
        assert_eq!(v["apiVersion"], "gateway.envoyproxy.io/v1alpha1");
        assert_eq!(v["kind"], "BackendTrafficPolicy");
        assert_eq!(v["metadata"]["name"], "tunnel-abc12-timeout");
        assert_eq!(v["metadata"]["namespace"], "default");
        assert_eq!(
            v["spec"],
            json!({
                "targetRefs": [{"group": "gateway.networking.k8s.io", "kind": "HTTPRoute", "name": TUNNEL}],
                "timeout": {"http": {"requestTimeout": "1h"}}
            })
        );
    }

    #[test]
    fn legacy_policy_is_upgraded() {
        assert!(is_legacy_addon_waf(&legacy_policy(), TUNNEL));
        assert_eq!(classify_existing_waf(&legacy_policy(), TUNNEL), ExistingWaf::Legacy);
        // Sampling left at its default, and an explicit empty sectionName,
        // are the same policy.
        assert!(is_legacy_addon_waf(
            &tweaked(|p| {
                p["spec"].as_object_mut().unwrap().remove("samplingPercentage");
            }),
            TUNNEL
        ));
        assert!(is_legacy_addon_waf(
            &tweaked(|p| p["spec"]["targetRefs"][0]["sectionName"] = json!("")),
            TUNNEL
        ));
        assert!(is_legacy_addon_waf(
            &tweaked(|p| p["spec"]["targetRefs"][0]["sectionName"] = Value::Null),
            TUNNEL
        ));
    }

    #[test]
    fn anything_but_the_exact_legacy_policy_is_kept() {
        let cases: Vec<(&str, Value)> = vec![
            ("already Enforce", tweaked(|p| p["spec"]["mode"] = json!("Enforce"))),
            ("Observe", tweaked(|p| p["spec"]["mode"] = json!("Observe"))),
            (
                "has sectionName",
                tweaked(|p| p["spec"]["targetRefs"][0]["sectionName"] = json!("protected")),
            ),
            (
                "has managed-by",
                tweaked(|p| {
                    p["metadata"]["annotations"][MANAGED_BY_ANNOTATION] = json!(MANAGED_BY_VALUE)
                }),
            ),
            (
                "extra exclusion",
                tweaked(|p| {
                    p["spec"]["ruleSets"][0]["owaspCoreRuleSet"]["ruleExclusions"]["ids"] =
                        json!([920420, 942100])
                }),
            ),
            (
                "excluded tags",
                tweaked(|p| {
                    p["spec"]["ruleSets"][0]["owaspCoreRuleSet"]["ruleExclusions"]["tags"] =
                        json!(["attack-sqli"])
                }),
            ),
            (
                "PL2",
                tweaked(|p| {
                    p["spec"]["ruleSets"][0]["owaspCoreRuleSet"]["paranoiaLevels"]["blocking"] =
                        json!(2)
                }),
            ),
            (
                "tuned threshold",
                tweaked(|p| {
                    p["spec"]["ruleSets"][0]["owaspCoreRuleSet"]["scoreThresholds"]["inbound"] =
                        json!(10)
                }),
            ),
            ("tuned sampling", tweaked(|p| p["spec"]["samplingPercentage"] = json!(50))),
            ("Gateway target", tweaked(|p| p["spec"]["targetRefs"][0]["kind"] = json!("Gateway"))),
            (
                "other route",
                tweaked(|p| p["spec"]["targetRefs"][0]["name"] = json!("tunnel-other")),
            ),
            (
                "two targets",
                tweaked(|p| {
                    let r = p["spec"]["targetRefs"][0].clone();
                    p["spec"]["targetRefs"].as_array_mut().unwrap().push(r);
                }),
            ),
            (
                "two rule sets",
                tweaked(|p| {
                    let r = p["spec"]["ruleSets"][0].clone();
                    p["spec"]["ruleSets"].as_array_mut().unwrap().push(r);
                }),
            ),
            ("no targetRefs", tweaked(|p| p["spec"]["targetRefs"] = Value::Null)),
        ];
        for (what, policy) in cases {
            assert!(!is_legacy_addon_waf(&policy, TUNNEL), "{what}: must be kept");
            assert_ne!(classify_existing_waf(&policy, TUNNEL), ExistingWaf::Legacy, "{what}");
        }
        let managed = tweaked(|p| {
            p["metadata"]["annotations"][MANAGED_BY_ANNOTATION] = json!(MANAGED_BY_VALUE)
        });
        assert_eq!(classify_existing_waf(&managed, TUNNEL), ExistingWaf::Managed);
        let tuned = tweaked(|p| p["spec"]["samplingPercentage"] = json!(50));
        assert_eq!(classify_existing_waf(&tuned, TUNNEL), ExistingWaf::Foreign);
    }

    #[test]
    fn numbers_compare_by_value_like_jq() {
        assert!(is_legacy_addon_waf(
            &tweaked(|p| p["spec"]["samplingPercentage"] = json!(100.0)),
            TUNNEL
        ));
    }

    #[test]
    fn upgrade_keeps_resource_version_and_drops_status() {
        let existing: DynamicObject = serde_json::from_value(legacy_policy()).unwrap();
        let v = serde_json::to_value(upgraded_waf_policy(existing, TUNNEL)).unwrap();
        assert_eq!(v["metadata"]["resourceVersion"], "4242");
        assert_eq!(v["metadata"]["name"], "tunnel-abc12-waf");
        assert_eq!(v["metadata"]["annotations"][MANAGED_BY_ANNOTATION], MANAGED_BY_VALUE);
        assert_eq!(
            v["metadata"]["annotations"]["networking.datumapis.com/display-name"],
            "home-assistant"
        );
        assert_eq!(v["spec"], desired_waf_spec(TUNNEL));
        assert!(v.get("status").is_none());
        assert_eq!(classify_existing_waf(&v, TUNNEL), ExistingWaf::Managed);
    }

    #[test]
    fn protected_rule_detected_by_name() {
        let rule = |name: Option<&str>| HTTPProxyRule {
            name: name.map(str::to_string),
            matches: Vec::new(),
            filters: None,
            backends: None,
        };
        assert!(has_protected_rule(&[rule(None), rule(Some("streams")), rule(Some("protected"))]));
        assert!(!has_protected_rule(&[rule(None), rule(None)]));
        assert!(!has_protected_rule(&[]));
    }

    #[test]
    fn excerpt_is_one_short_line() {
        assert_eq!(excerpt("a\r\n b\t c "), "a b c");
        let long = "é".repeat(300);
        let cut = excerpt(&long);
        assert!(cut.ends_with("..."));
        assert_eq!(cut.chars().count(), EXCERPT_MAX_CHARS + 3);
    }

    #[test]
    fn call_error_names_status_and_response() {
        let e = CallError::Status(403, "forbidden".into());
        assert_eq!(e.to_string(), "HTTP 403: forbidden");
        assert_eq!(
            CallError::NoResponse("timed out".into()).to_string(),
            "no response: timed out"
        );
        assert!(is_denied(&e));
        assert!(!is_denied(&CallError::Status(500, String::new())));
        assert!(!is_denied(&CallError::NoResponse("connect refused".into())));
    }
}
