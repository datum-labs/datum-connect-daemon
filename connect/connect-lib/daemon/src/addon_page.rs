//! What the add-on's page shows once paired, and its Re-pair and Unpair,
//! from the running daemon. The page and its server are in `ingress.rs`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use connect_lib::datum_cloud::ha_supervisor::Supervisor;
use connect_lib::datum_cloud::pairing;
use connect_lib::edge_policies::{self, EdgePolicyStatus};

use crate::ingress::{ActionError, BoxFuture, ForgetOutcome, PairedStatus, PairedView, TunnelView};
use crate::{AppState, StopReason};

/// The page polls every 10s; Datum is asked about the tunnels at most this
/// often.
const STATUS_TTL: Duration = Duration::from_secs(5);
/// Edge policies change rarely, and only in the portal.
const EDGE_TTL: Duration = Duration::from_secs(60);
/// Long enough for the page's reply to go out before the restart begins.
const RESTART_DELAY: Duration = Duration::from_millis(500);
const PORTAL: &str = "https://cloud.datum.net";

pub(crate) struct DaemonPaired {
    app: Arc<AppState>,
    /// The key the daemon runs on (`DATUM_SA_KEY_FILE`).
    key_file: Option<PathBuf>,
    /// Where pairing saves its key. Only a key there may be forgotten.
    paired_key_file: Option<PathBuf>,
    supervisor: Option<Supervisor>,
    cache: tokio::sync::Mutex<Option<(Instant, PairedStatus)>>,
    edge: tokio::sync::Mutex<HashMap<String, (Instant, EdgePolicyStatus)>>,
}

impl DaemonPaired {
    pub(crate) fn new(
        app: Arc<AppState>,
        key_file: Option<PathBuf>,
        paired_key_file: Option<PathBuf>,
        supervisor: Option<Supervisor>,
    ) -> Self {
        Self {
            app,
            key_file,
            paired_key_file,
            supervisor,
            cache: Default::default(),
            edge: Default::default(),
        }
    }

    fn key(&self) -> KeyFacts {
        key_facts(self.key_file.as_deref(), self.paired_key_file.as_deref(), self.supervisor.is_some())
    }

    async fn edge_status(&self, tunnel_id: &str) -> Option<EdgePolicyStatus> {
        if !self.app.edge_policies {
            return None;
        }
        if let Some((at, s)) = self.edge.lock().await.get(tunnel_id)
            && at.elapsed() < EDGE_TTL
        {
            return Some(s.clone());
        }
        let client = match self.app.datum.project_control_plane_client(&self.app.project_id).await {
            Ok(pcp) => pcp.client(),
            Err(e) => {
                tracing::debug!("page: cannot reach the project's control plane for edge policies: {e:#}");
                return None;
            }
        };
        let status = edge_policies::edge_policy_status(client, tunnel_id).await;
        self.edge.lock().await.insert(tunnel_id.to_string(), (Instant::now(), status.clone()));
        Some(status)
    }

    async fn fresh_status(&self) -> PairedStatus {
        let key = self.key();
        let mut status = PairedStatus {
            project: self.app.project_id.clone(),
            service_account: key.email,
            key_source: key.source,
            tunnels: Vec::new(),
            can_forget: key.why_not.is_none(),
            why_not: key.why_not,
            error: None,
        };
        match self.app.control.list_active().await {
            Ok(tunnels) => {
                let running: Vec<String> = self.app.running.lock().await.keys().cloned().collect();
                for t in tunnels {
                    let edge = self.edge_status(&t.id).await;
                    status
                        .tunnels
                        .push(tunnel_view(&t, running.contains(&t.id), edge, &self.app.project_id));
                }
            }
            Err(e) => status.error = Some(format!("Could not ask Datum about the tunnel: {e}")),
        }
        status
    }

    async fn forget_inner(&self, unpair: bool) -> Result<ForgetOutcome, ActionError> {
        let key = self.key();
        if let Some(why) = key.why_not {
            return Err(ActionError { status: StatusCode::CONFLICT, message: why });
        }
        let (Some(paired), Some(supervisor)) = (self.paired_key_file.clone(), self.supervisor.clone()) else {
            return Err(ActionError {
                status: StatusCode::CONFLICT,
                message: "Re-pair and Unpair are not available here.".into(),
            });
        };
        // Nothing is deleted unless the restart that completes it can be
        // asked for.
        if let Err(e) = supervisor.self_info().await {
            return Err(ActionError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: format!("Could not reach the Home Assistant Supervisor, so nothing was changed: {e}"),
            });
        }
        if unpair {
            let running: Vec<String> = self.app.running.lock().await.keys().cloned().collect();
            for id in running {
                if let Err((code, body)) =
                    crate::stop_tunnel_internal(&self.app, &id, StopReason::Manual, "addon-page").await
                {
                    tracing::warn!(tunnel = %id, %code, error = %body.0, "Unpair: could not stop the tunnel; carrying on");
                }
            }
        }
        match std::fs::remove_file(&paired) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(ActionError {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: format!("Could not delete the paired key: {e}"),
                });
            }
        }
        let _ = std::fs::remove_file(pairing::pending_path(&paired));
        let account = key.email.clone().unwrap_or_else(|| "an unnamed service account".into());
        tracing::warn!(
            "{}: deleted the paired key for {account}, restarting the add-on. That service account still exists in Datum; delete it in the portal, under the project's Service accounts, once it is no longer needed.",
            if unpair { "Unpaired from the add-on's page" } else { "Re-pairing from the add-on's page" }
        );
        tokio::spawn(async move {
            tokio::time::sleep(RESTART_DELAY).await;
            if let Err(e) = supervisor.restart_self().await {
                tracing::warn!("Could not restart the add-on; restart it from its Info tab to finish: {e}");
            }
        });
        Ok(ForgetOutcome { restarting: true, service_account: key.email })
    }
}

impl PairedView for DaemonPaired {
    fn status(&self) -> BoxFuture<'_, PairedStatus> {
        Box::pin(async move {
            let mut cache = self.cache.lock().await;
            if let Some((at, s)) = cache.as_ref()
                && at.elapsed() < STATUS_TTL
            {
                return s.clone();
            }
            let s = self.fresh_status().await;
            *cache = Some((Instant::now(), s.clone()));
            s
        })
    }

    fn forget(&self, unpair: bool) -> BoxFuture<'_, Result<ForgetOutcome, ActionError>> {
        Box::pin(async move {
            let result = self.forget_inner(unpair).await;
            *self.cache.lock().await = None;
            result
        })
    }
}

#[derive(Debug, PartialEq)]
struct KeyFacts {
    email: Option<String>,
    source: &'static str,
    why_not: Option<String>,
}

/// Whose key the daemon runs on, and whether the page may forget it: only
/// one pairing saved, and only with a Supervisor to restart the add-on.
fn key_facts(key_file: Option<&Path>, paired_key_file: Option<&Path>, has_supervisor: bool) -> KeyFacts {
    let email = key_file
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("client_email").and_then(|e| e.as_str()).map(str::to_string));
    let paired = matches!((key_file, paired_key_file), (Some(k), Some(p)) if k == p);
    let why_not = if !paired {
        Some("This add-on runs on a key you provided, not one made by connecting to Datum. To connect from this page instead, clear service_account_key on the Configuration tab (and remove any key file in /share), then restart.".to_string())
    } else if !has_supervisor {
        Some("Re-pair and Unpair restart the add-on through the Home Assistant Supervisor, which is not available here.".to_string())
    } else {
        None
    };
    KeyFacts { email, source: if paired { "paired" } else { "provided" }, why_not }
}

/// One tunnel, as the page shows it.
fn tunnel_view(t: &connect_lib::TunnelSummary, running: bool, edge: Option<EdgePolicyStatus>, project: &str) -> TunnelView {
    let state = if !t.enabled {
        "off"
    } else if t.connector_ready && t.programmed {
        "online"
    } else if running {
        "starting"
    } else {
        "offline"
    };
    TunnelView {
        id: t.id.clone(),
        label: t.label.clone(),
        address: t.hostnames.first().map(|h| format!("https://{h}")),
        state,
        edge,
        portal_url: Some(format!("{PORTAL}/project/{project}/edge/{}/overview", t.id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(enabled: bool, ready: bool, hostnames: &[&str]) -> connect_lib::TunnelSummary {
        connect_lib::TunnelSummary {
            id: "t-1".into(),
            label: "home-assistant".into(),
            endpoint: "http://127.0.0.1:1".into(),
            hostnames: hostnames.iter().map(|h| h.to_string()).collect(),
            enabled,
            accepted: ready,
            programmed: ready,
            connector_metadata_programmed: ready,
            connector_ready: ready,
            connector_name: None,
            connector_device: None,
            created_at: None,
        }
    }

    #[test]
    fn a_tunnel_reads_as_a_person_would_say_it() {
        let v = tunnel_view(&summary(true, true, &["abc.datumproxy.net"]), true, None, "p-1");
        assert_eq!(v.state, "online");
        assert_eq!(v.address.as_deref(), Some("https://abc.datumproxy.net"));
        assert_eq!(v.portal_url.as_deref(), Some("https://cloud.datum.net/project/p-1/edge/t-1/overview"));
        assert_eq!(tunnel_view(&summary(true, false, &[]), true, None, "p").state, "starting");
        assert_eq!(tunnel_view(&summary(true, false, &[]), false, None, "p").state, "offline");
        assert_eq!(tunnel_view(&summary(false, true, &[]), false, None, "p").state, "off");
        assert_eq!(tunnel_view(&summary(true, true, &[]), true, None, "p").address, None);
    }

    #[test]
    fn only_a_paired_key_can_be_forgotten_and_its_secret_is_never_read_out() {
        let dir = std::env::temp_dir().join(format!("addon-page-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paired = dir.join("service-account.json");
        std::fs::write(
            &paired,
            r#"{"type":"datum_service_account","client_email":"home-assistant-x@p-1.identity.datumapis.com","private_key":"-----BEGIN SECRET-----"}"#,
        )
        .unwrap();
        let facts = key_facts(Some(&paired), Some(&paired), true);
        assert_eq!(
            facts,
            KeyFacts {
                email: Some("home-assistant-x@p-1.identity.datumapis.com".into()),
                source: "paired",
                why_not: None
            }
        );
        let pasted = dir.join("pasted-service-account.json");
        std::fs::copy(&paired, &pasted).unwrap();
        let facts = key_facts(Some(&pasted), Some(&paired), true);
        assert_eq!(facts.source, "provided");
        assert!(facts.why_not.unwrap().contains("clear service_account_key"));
        assert!(key_facts(Some(&paired), Some(&paired), false).why_not.unwrap().contains("Supervisor"));
        assert_eq!(key_facts(None, None, true).email, None);

        let status = PairedStatus {
            project: "p-1".into(),
            service_account: key_facts(Some(&paired), Some(&paired), true).email,
            key_source: "paired",
            ..Default::default()
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("SECRET") && !json.contains("private_key"), "{json}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
