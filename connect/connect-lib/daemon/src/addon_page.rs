//! What the add-on's page shows once paired, and its actions (Re-pair,
//! Unpair, Remove for an older tunnel, Allow and Skip for Home Assistant's
//! trusted proxies), from the running daemon. The page and its server are
//! in `ingress.rs`.
//!
//! Home Assistant's trusted proxies are the last step of setting up the
//! add-on. `setup` ends once the key is saved and the daemon takes over, so
//! the daemon's page carries that step on: shown as setup's last step until
//! it is finished (allowed, skipped, or found already set up), and as a
//! row of the tunnel's status after that. It runs in the daemon, not in
//! `setup`, so that the tunnel starts whether or not anyone is looking at
//! the page, and so that the step survives an add-on restart.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime};

use axum::http::StatusCode;
use connect_lib::datum_cloud::ha_core::{self, AllowOutcome, AllowStep, AllowTiming, ProxySetup};
use connect_lib::datum_cloud::ha_supervisor::Supervisor;
use connect_lib::datum_cloud::pairing;
use connect_lib::edge_policies::{self, EdgePolicyStatus};

use crate::exclusive;
use crate::ingress::{
    ActionError, BoxFuture, ForgetOutcome, PairedStatus, PairedView, ProxyStepView, RemoveOutcome, TunnelView,
};
use crate::{AppState, StopReason};

/// The page polls every 10s; Datum is asked about the tunnels at most this
/// often.
const STATUS_TTL: Duration = Duration::from_secs(5);
/// Edge policies change rarely, and only in the portal.
const EDGE_TTL: Duration = Duration::from_secs(60);
/// Home Assistant's HTTP settings change rarely, and each read is a
/// websocket connection of its own.
const PROXY_TTL: Duration = Duration::from_secs(30);
/// Written under the connect dir once the trusted-proxy step of setup is
/// finished, so that it is offered as setup's last step only once.
const PROXY_STEP_MARKER: &str = "addon_proxy_step_finished";
/// Long enough for the page's reply to go out before the restart begins.
const RESTART_DELAY: Duration = Duration::from_millis(500);
/// A tunnel younger than this gets the "a new address usually works within
/// a minute" note.
const NEW_ADDRESS_WINDOW: Duration = Duration::from_secs(10 * 60);
const PORTAL: &str = "https://cloud.datum.net";
/// How long [`DaemonPaired::confirm_ours`] keeps looking at start. Home
/// Assistant reverts a trial 5 minutes after it started on it, so there is
/// no point after that.
const LEFTOVER_TRIAL_WATCH: Duration = Duration::from_secs(5 * 60);

/// The client address the verify step's request claims to forward for: a
/// documentation address (RFC 5737), never a real one.
const VERIFY_FORWARDED_FOR: &str = "203.0.113.10";
const VERIFY_TIMEOUT: Duration = Duration::from_secs(20);

const ALLOWED_MESSAGE: &str = "Home Assistant now accepts connections through Datum.";
const PENDING_MESSAGE: &str = "Home Assistant has a network settings change waiting for confirmation. Finish it in Settings → System → Network (confirm or discard it), then come back here.";

/// Allow, as it runs in the background, and what Home Assistant last said.
/// Shared with the task that runs Allow, which outlives the request.
#[derive(Default)]
struct ProxyJob {
    running: Option<AllowStep>,
    /// The last Allow's outcome: whether it ended well, and what to say.
    last: Option<(bool, String)>,
    /// The setup step is finished (see [`PROXY_STEP_MARKER`]). Read from
    /// disk once, then kept here.
    finished: Option<bool>,
    /// `http/config`, read at most every [`PROXY_TTL`]; `None` inside is
    /// "could not be asked".
    cached: Option<(Instant, Option<ProxySetup>)>,
    /// Bumped whenever the above changes, so a cached page status is not
    /// served past it.
    version: u64,
    /// Allow's own change, found on trial, was taken up automatically
    /// already (see [`DaemonPaired::confirm_ours`]); once per daemon run, so
    /// a check that failed is not repeated until the revert.
    auto_tried: bool,
}

/// The page's status as last built, and what it was built from.
struct CachedStatus {
    at: Instant,
    /// [`ProxyJob::version`] then.
    version: u64,
    /// The tunnels this daemon was running then (sorted ids).
    running: Vec<String>,
    status: PairedStatus,
}

impl CachedStatus {
    /// Still good to serve: young, and neither Allow's state nor the set of
    /// running tunnels has changed since. A tunnel this daemon starts or
    /// stops shows on the next poll, not up to [`STATUS_TTL`] later.
    fn serves(&self, version: u64, running: &[String]) -> bool {
        self.at.elapsed() < STATUS_TTL && self.version == version && self.running == running
    }
}

pub(crate) struct DaemonPaired {
    app: Arc<AppState>,
    /// The key the daemon runs on (`DATUM_SA_KEY_FILE`).
    key_file: Option<PathBuf>,
    /// Where pairing saves its key. Only a key there may be forgotten.
    paired_key_file: Option<PathBuf>,
    supervisor: Option<Supervisor>,
    cache: tokio::sync::Mutex<Option<CachedStatus>>,
    edge: tokio::sync::Mutex<HashMap<String, (Instant, EdgePolicyStatus)>>,
    proxy_job: Arc<StdMutex<ProxyJob>>,
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
            proxy_job: Default::default(),
        }
    }

    fn key(&self) -> KeyFacts {
        key_facts(self.key_file.as_deref(), self.paired_key_file.as_deref(), self.supervisor.is_some())
    }

    fn job_version(&self) -> u64 {
        self.proxy_job.lock().unwrap_or_else(|e| e.into_inner()).version
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

    /// The tunnels as the page splits them: the add-on's own (or, outside
    /// the add-on, all of them) and the older ones.
    fn split(&self, tunnels: Vec<connect_lib::TunnelSummary>) -> (Vec<connect_lib::TunnelSummary>, Vec<connect_lib::TunnelSummary>) {
        match &self.app.exclusive_label {
            Some(label) => {
                let part = exclusive::partition(tunnels, label, |id| {
                    exclusive::is_locally_known(&self.app.connect_dir, &self.app.project_id, id)
                });
                (part.active.into_iter().collect(), part.older)
            }
            None => (tunnels, Vec::new()),
        }
    }

    /// The tunnels this daemon runs right now, sorted.
    async fn running_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.app.running.lock().await.keys().cloned().collect();
        ids.sort_unstable();
        ids
    }

    async fn fresh_status(&self, running: &[String]) -> PairedStatus {
        let key = self.key();
        let mut status = PairedStatus {
            project: self.app.project_id.clone(),
            service_account: key.email,
            key_source: key.source,
            can_forget: key.why_not.is_none(),
            why_not: key.why_not,
            ..Default::default()
        };
        match self.app.control.list_active().await {
            Ok(tunnels) => {
                let running = |id: &str| running.iter().any(|r| r == id);
                let now = SystemTime::now();
                let (mine, older) = self.split(tunnels);
                for t in mine {
                    let edge = self.edge_status(&t.id).await;
                    status
                        .tunnels
                        .push(tunnel_view(&t, running(&t.id), edge, &self.app.project_id, now));
                }
                for t in older {
                    // Stopped and on their way out: their policies are not
                    // worth a call each.
                    status.older.push(tunnel_view(&t, running(&t.id), None, &self.app.project_id, now));
                }
            }
            Err(e) => status.error = Some(format!("Could not ask Datum about the tunnel: {e}")),
        }
        status.trusted_proxies = self.proxy_step().await;
        status
    }

    fn marker(&self) -> PathBuf {
        self.app.connect_dir.join(PROXY_STEP_MARKER)
    }


    /// Home Assistant's trusted-proxy step, if there is a Supervisor to ask.
    async fn proxy_step(&self) -> Option<ProxyStepView> {
        let sup = self.supervisor.as_ref()?;
        let (running, last, cached) = {
            let job = self.proxy_job.lock().unwrap_or_else(|e| e.into_inner());
            let fresh = job.cached.as_ref().filter(|(at, _)| at.elapsed() < PROXY_TTL).map(|(_, s)| s.clone());
            (job.running, job.last.clone(), fresh)
        };
        let finished = step_finished(&self.marker(), &self.proxy_job);
        if let Some(step) = running {
            return Some(ProxyStepView { state: "working", message: Some(step.describe().into()), setup: !finished });
        }
        let setup = match cached {
            Some(s) => s,
            None => {
                let read = match ha_core::read_http_config(sup, Duration::from_secs(10)).await {
                    Ok(c) => ha_core::assess(&c).map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
                let setup = match read {
                    Ok(s) => Some(s),
                    Err(e) => {
                        tracing::debug!("page: could not read Home Assistant's HTTP settings: {e}");
                        None
                    }
                };
                let mut job = self.proxy_job.lock().unwrap_or_else(|e| e.into_inner());
                job.cached = Some((Instant::now(), setup.clone()));
                setup
            }
        };
        if auto_confirm_due(setup.as_ref(), &self.proxy_job) && self.allow_inner(true).await.is_ok() {
            let step = self.proxy_job.lock().unwrap_or_else(|e| e.into_inner()).running.unwrap_or(AllowStep::Verifying);
            return Some(ProxyStepView { state: "working", message: Some(step.describe().into()), setup: !finished });
        }
        Some(settled_view(&self.marker(), &self.proxy_job, setup.as_ref(), last.as_ref()))
    }

    /// At daemon start: if Home Assistant runs Allow's own change on trial
    /// (the add-on restarted before that Allow could confirm it), confirm
    /// it before Home Assistant reverts it, whether or not anyone has the
    /// page open. Keeps asking for a few minutes while the tunnel (whose
    /// local hop the check goes through) is still being started or Home
    /// Assistant is not answering; stops at the first clear answer.
    pub(crate) async fn confirm_ours(&self) {
        let Some(sup) = self.supervisor.clone() else { return };
        let deadline = Instant::now() + LEFTOVER_TRIAL_WATCH;
        while Instant::now() < deadline {
            match ha_core::read_http_config(&sup, Duration::from_secs(10)).await.map(|c| ha_core::assess(&c)) {
                Ok(Ok(setup)) => {
                    if !auto_confirm_due(Some(&setup), &self.proxy_job) {
                        return;
                    }
                    match self.allow_inner(true).await {
                        Ok(()) => return,
                        // The tunnel isn't running yet, or Allow already runs.
                        Err(e) => tracing::debug!("not confirming Datum's change on trial yet: {}", e.message),
                    }
                }
                Ok(Err(e)) => tracing::debug!("could not read Home Assistant's HTTP settings: {e}"),
                Err(e) => tracing::debug!("could not ask Home Assistant about its HTTP settings: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    /// Where the add-on's tunnel's local hop forwards to: Home Assistant,
    /// which the verify step sends its request to directly. The hop
    /// connects from 127.0.0.1 too, and the connecting address is what Home
    /// Assistant checks, so the answer is the one tunnel traffic gets,
    /// without depending on how the hop treats the request on its way.
    async fn active_target(&self) -> Result<SocketAddr, ActionError> {
        let tunnels = self.app.control.list_active().await.map_err(|e| ActionError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: format!("Could not ask Datum about the tunnel, so nothing was changed: {e}"),
        })?;
        let (mine, _) = self.split(tunnels);
        let inspectors = self.app.inspectors.lock().await;
        let target = mine.iter().find_map(|t| inspectors.get(&t.id).map(|h| h.real_target().clone())).ok_or(ActionError {
            status: StatusCode::CONFLICT,
            message: "The tunnel isn't running yet, so a request through it can't be checked. Try again in a moment.".into(),
        })?;
        target_addr(&target).ok_or(ActionError {
            status: StatusCode::CONFLICT,
            message: format!("The tunnel forwards to {target}, which is not an address a request can be checked against."),
        })
    }

    /// Allow, from the page (`auto` false), or to confirm Allow's own change
    /// found still on trial (`auto` true: nothing is saved, and if the
    /// change is no longer there, or no longer ours, nothing is done).
    async fn allow_inner(&self, auto: bool) -> Result<(), ActionError> {
        let Some(sup) = self.supervisor.clone() else {
            return Err(ActionError {
                status: StatusCode::CONFLICT,
                message: "This needs the Home Assistant Supervisor, which is not available here.".into(),
            });
        };
        let target = self.active_target().await?;
        {
            let mut job = self.proxy_job.lock().unwrap_or_else(|e| e.into_inner());
            if job.running.is_some() {
                return Err(ActionError { status: StatusCode::CONFLICT, message: "Already in progress.".into() });
            }
            job.running = Some(if auto { AllowStep::Verifying } else { AllowStep::Saving });
            if auto {
                job.auto_tried = true;
            } else {
                job.last = None;
            }
            job.version += 1;
        }
        if !auto {
            tracing::info!("Letting Home Assistant accept connections through Datum (from the Datum Connect page); Home Assistant restarts");
        }
        let job = self.proxy_job.clone();
        let marker = self.marker();
        tokio::spawn(async move {
            let progress = job.clone();
            let step = move |step| {
                let mut j = progress.lock().unwrap_or_else(|e| e.into_inner());
                j.running = Some(step);
                j.version += 1;
            };
            let verify = move || verify_forwarded(target);
            let timing = AllowTiming::default();
            let outcome = if auto {
                ha_core::confirm_ours_on_trial(&sup, &timing, step, verify).await
            } else {
                Some(ha_core::allow_forwarded_requests(&sup, &timing, step, verify).await)
            };
            let Some(outcome) = outcome else {
                // Gone, or not ours after all: nothing was done.
                let mut j = job.lock().unwrap_or_else(|e| e.into_inner());
                j.running = None;
                j.cached = None;
                j.version += 1;
                return;
            };
            let (ok, message) = outcome_message(&outcome);
            if ok {
                tracing::info!("{message}");
                finish_step(&marker, &job);
            } else {
                tracing::warn!("Home Assistant does not accept connections through Datum yet: {message}");
            }
            let mut j = job.lock().unwrap_or_else(|e| e.into_inner());
            j.running = None;
            j.last = Some((ok, message));
            j.cached = None;
            j.version += 1;
        });
        Ok(())
    }

    fn skip_inner(&self) -> Result<(), ActionError> {
        if self.supervisor.is_none() {
            return Err(ActionError {
                status: StatusCode::CONFLICT,
                message: "This needs the Home Assistant Supervisor, which is not available here.".into(),
            });
        }
        skip_step(&self.marker(), &self.proxy_job)
    }

    async fn remove_inner(&self, id: &str) -> Result<RemoveOutcome, ActionError> {
        if self.app.exclusive_label.is_none() {
            return Err(ActionError {
                status: StatusCode::CONFLICT,
                message: "Removing older tunnels is only available in the Home Assistant add-on.".into(),
            });
        }
        // Asked afresh, never from the page's cached view: only a tunnel
        // that is older right now may go.
        let tunnels = self.app.control.list_active().await.map_err(|e| ActionError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: format!("Could not ask Datum about the tunnels, so nothing was removed: {e}"),
        })?;
        let (_, older) = self.split(tunnels);
        let Some(t) = removable(&older, id) else {
            return Err(ActionError {
                status: StatusCode::CONFLICT,
                message: "That isn't an older tunnel from this Home Assistant, so it was not removed.".into(),
            });
        };
        let t = t.clone();
        if self.app.running.lock().await.contains_key(&t.id)
            && let Err((code, body)) =
                crate::stop_tunnel_internal(&self.app, &t.id, StopReason::Manual, "addon-page").await
        {
            tracing::warn!(tunnel = %t.id, %code, error = %body.0, "Remove: could not stop the tunnel first; removing it anyway");
        }
        let outcome = crate::delete_tunnel_internal(&self.app, &t.id, "addon-page").await.map_err(|(_, body)| {
            let why = body.0.get("error").and_then(|e| e.as_str()).unwrap_or("unknown error").to_string();
            tracing::warn!(tunnel = %t.id, "Could not remove older tunnel '{}': {why}", t.label);
            ActionError {
                status: StatusCode::BAD_GATEWAY,
                message: format!("Could not remove the tunnel: {why}. Nothing more was changed here; try again."),
            }
        })?;
        tracing::info!(
            "Removed older tunnel '{}' ({}) from Datum{}",
            t.label,
            t.id,
            match &outcome.connector {
                Some(c) => format!(", with its connector {c}"),
                None => String::new(),
            }
        );
        Ok(RemoveOutcome { id: t.id, label: t.label })
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
            let version = self.job_version();
            let running = self.running_ids().await;
            if let Some(c) = cache.as_ref()
                && c.serves(version, &running)
            {
                return c.status.clone();
            }
            let status = self.fresh_status(&running).await;
            *cache = Some(CachedStatus { at: Instant::now(), version: self.job_version(), running, status: status.clone() });
            status
        })
    }

    fn forget(&self, unpair: bool) -> BoxFuture<'_, Result<ForgetOutcome, ActionError>> {
        Box::pin(async move {
            let result = self.forget_inner(unpair).await;
            *self.cache.lock().await = None;
            result
        })
    }

    fn remove(&self, id: String) -> BoxFuture<'_, Result<RemoveOutcome, ActionError>> {
        Box::pin(async move {
            let result = self.remove_inner(&id).await;
            *self.cache.lock().await = None;
            result
        })
    }

    fn allow_proxies(&self) -> BoxFuture<'_, Result<(), ActionError>> {
        Box::pin(async move {
            let result = self.allow_inner(false).await;
            *self.cache.lock().await = None;
            result
        })
    }

    fn skip_proxies(&self) -> BoxFuture<'_, Result<(), ActionError>> {
        Box::pin(async move {
            let result = self.skip_inner();
            *self.cache.lock().await = None;
            result
        })
    }
}

/// Whether to take up Allow's own change found on trial: only that, only
/// while no Allow runs, and once per daemon run.
fn auto_confirm_due(setup: Option<&ProxySetup>, job: &StdMutex<ProxyJob>) -> bool {
    let j = job.lock().unwrap_or_else(|e| e.into_inner());
    setup == Some(&ProxySetup::Ours { on_trial: true }) && j.running.is_none() && !j.auto_tried
}

/// Whether setup's last step is finished, read from disk the first time.
fn step_finished(marker: &Path, job: &StdMutex<ProxyJob>) -> bool {
    let mut j = job.lock().unwrap_or_else(|e| e.into_inner());
    *j.finished.get_or_insert_with(|| marker.exists())
}

/// The step's view once Home Assistant has been asked (`setup`, `None` if
/// it could not be). Found already set up (by hand, or by an earlier
/// Allow), the step is finished silently: there is nothing to ask.
fn settled_view(
    marker: &Path,
    job: &StdMutex<ProxyJob>,
    setup: Option<&ProxySetup>,
    last: Option<&(bool, String)>,
) -> ProxyStepView {
    let mut finished = step_finished(marker, job);
    if !finished && setup == Some(&ProxySetup::Ready) {
        finish_step(marker, job);
        finished = true;
    }
    proxy_step_view(setup, last, finished)
}

/// Skip: the step ends without changing Home Assistant. Not while Allow
/// runs, whose outcome would otherwise be lost from view.
fn skip_step(marker: &Path, job: &StdMutex<ProxyJob>) -> Result<(), ActionError> {
    {
        let mut j = job.lock().unwrap_or_else(|e| e.into_inner());
        if j.running.is_some() {
            return Err(ActionError { status: StatusCode::CONFLICT, message: "Allow is in progress; wait for it to finish.".into() });
        }
        // A failure from an earlier Allow is no longer the step's to show.
        j.last = None;
    }
    tracing::info!(
        "Trusted proxies skipped on the Datum Connect page: until they are set up (Settings → System → Network, or Allow on the page), the public address returns 400: Bad Request"
    );
    finish_step(marker, job);
    Ok(())
}

/// Marks setup's last step finished, on disk and in `job`. Best-effort: if
/// the file cannot be written, the step is offered again after a restart,
/// which is harmless.
fn finish_step(marker: &Path, job: &StdMutex<ProxyJob>) {
    if let Err(e) = std::fs::write(marker, b"") {
        tracing::debug!("could not record the finished trusted-proxy step at {}: {e}", marker.display());
    }
    let mut j = job.lock().unwrap_or_else(|e| e.into_inner());
    j.finished = Some(true);
    j.version += 1;
}

/// `host:port` of an `http://` target, numeric only.
fn target_addr(target: &axum::http::Uri) -> Option<SocketAddr> {
    let host = target.host()?.trim_start_matches('[').trim_end_matches(']');
    let ip: std::net::IpAddr = host.parse().ok()?;
    Some(SocketAddr::new(ip, target.port_u16().unwrap_or(80)))
}

/// The older tunnel `id`, if it is one. Never the add-on's own tunnel, and
/// never one this device has no local state for: neither is in `older`.
fn removable<'a>(older: &'a [connect_lib::TunnelSummary], id: &str) -> Option<&'a connect_lib::TunnelSummary> {
    older.iter().find(|t| t.id == id)
}

/// What the page says about the trusted-proxy step. `finished`: setup's
/// last step is behind us, so this is a row of the status, not the step.
/// An Allow that just worked still shows as the step, with its ✓ and Done.
fn proxy_step_view(setup: Option<&ProxySetup>, last: Option<&(bool, String)>, finished: bool) -> ProxyStepView {
    let just_allowed = matches!(last, Some((true, _)));
    let as_step = !finished || just_allowed;
    let view = |state, message: Option<&str>| ProxyStepView { state, message: message.map(str::to_string), setup: as_step };
    match (setup, last) {
        (Some(ProxySetup::Ready), _) => view("ok", None),
        // The last Allow failed: say why, whatever Home Assistant shows
        // while it reverts (its pending trial is ours, not someone else's).
        (_, Some((false, why))) => view("failed", Some(why)),
        (Some(ProxySetup::Pending), _) => view("pending", Some(PENDING_MESSAGE)),
        // Allow's own change, waiting (not taken up automatically, or not
        // on trial yet): Allow carries it on.
        (Some(ProxySetup::Ours { .. }), _) => view("needed", None),
        (Some(ProxySetup::Needed { .. }), _) => view("needed", None),
        (None, _) => view("unknown", Some("Could not ask Home Assistant about its network settings.")),
    }
}

/// Whether Allow ended well, and what to say about it.
fn outcome_message(outcome: &AllowOutcome) -> (bool, String) {
    match outcome {
        AllowOutcome::Allowed | AllowOutcome::AlreadyAllowed => (true, ALLOWED_MESSAGE.into()),
        AllowOutcome::PendingByOther => (false, PENDING_MESSAGE.into()),
        AllowOutcome::Failed(why) => (false, why.clone()),
    }
}

/// One request to Home Assistant from the address the tunnel's local hop
/// connects from, carrying `X-Forwarded-For` and `X-Forwarded-Proto` as
/// Datum's edge does: Home Assistant answers it 400 unless it trusts that
/// address as a proxy. Returns the status.
pub(crate) async fn verify_forwarded(target: SocketAddr) -> Result<u16, String> {
    use http_body_util::Empty;
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;
    let client: Client<_, Empty<bytes::Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    let req = axum::http::Request::get(format!("http://{target}/"))
        .header("x-forwarded-for", VERIFY_FORWARDED_FOR)
        .header("x-forwarded-proto", "https")
        .body(Empty::new())
        .map_err(|e| e.to_string())?;
    match tokio::time::timeout(VERIFY_TIMEOUT, client.request(req)).await {
        Ok(Ok(r)) => Ok(r.status().as_u16()),
        Ok(Err(e)) => Err(format!("the request failed: {e}")),
        Err(_) => Err(format!("no answer within {} seconds", VERIFY_TIMEOUT.as_secs())),
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

/// One tunnel, as the page shows it. `running`: this daemon serves it now,
/// which is what its badge says first.
///
/// Datum's `enabled` is only whether the tunnel's ConnectorAdvertisement
/// exists. The edge routes by the HTTPProxy's connector, not by that
/// advertisement, so a tunnel this daemon runs serves traffic whether or
/// not it is there. Seen on a Home Assistant Green (0.3.5) after a
/// reinstall adopted the old tunnel: the daemon ran it and its address
/// answered, while the page said "Off" from `enabled`. So a running tunnel
/// is never "Off": it is "Online" once its connector is ready and its
/// proxy programmed, "Starting" before. `enabled` only tells apart, for a
/// tunnel this daemon does not run, one turned off from one that should
/// be on.
fn tunnel_view(
    t: &connect_lib::TunnelSummary,
    running: bool,
    edge: Option<EdgePolicyStatus>,
    project: &str,
    now: SystemTime,
) -> TunnelView {
    let state = match (running, t.enabled, t.connector_ready && t.programmed) {
        (true, _, true) => "online",
        (true, _, false) => "starting",
        (false, false, _) => "off",
        (false, true, true) => "online",
        (false, true, false) => "offline",
    };
    TunnelView {
        id: t.id.clone(),
        label: t.label.clone(),
        address: t.hostnames.first().map(|h| format!("https://{h}")),
        state,
        edge,
        portal_url: Some(format!("{PORTAL}/project/{project}/edge/{}/overview", t.id)),
        new_address: t.created_within(NEW_ADDRESS_WINDOW, now),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exclusive::tests::tunnel;

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
        let now = SystemTime::now();
        let v = tunnel_view(&summary(true, true, &["abc.datumproxy.net"]), true, None, "p-1", now);
        assert_eq!(v.state, "online");
        assert_eq!(v.address.as_deref(), Some("https://abc.datumproxy.net"));
        assert_eq!(v.portal_url.as_deref(), Some("https://cloud.datum.net/project/p-1/edge/t-1/overview"));
        assert_eq!(tunnel_view(&summary(true, false, &[]), true, None, "p", now).state, "starting");
        assert_eq!(tunnel_view(&summary(true, false, &[]), false, None, "p", now).state, "offline");
        assert_eq!(tunnel_view(&summary(false, true, &[]), false, None, "p", now).state, "off");
        assert_eq!(tunnel_view(&summary(true, true, &[]), true, None, "p", now).address, None);
    }

    /// The badge follows what this daemon runs, not Datum's `enabled`
    /// (whether the ConnectorAdvertisement exists), which read false for a
    /// tunnel the daemon was serving on a real device: "Off" while online.
    #[test]
    fn a_running_tunnel_is_never_off() {
        let now = SystemTime::now();
        let not_enabled_but_serving = summary(false, true, &["abc.datumproxy.net"]);
        assert_eq!(tunnel_view(&not_enabled_but_serving, true, None, "p", now).state, "online");
        assert_eq!(tunnel_view(&summary(false, false, &[]), true, None, "p", now).state, "starting");
        // Not running here: Datum's view.
        assert_eq!(tunnel_view(&not_enabled_but_serving, false, None, "p", now).state, "off");
        assert_eq!(tunnel_view(&summary(true, true, &[]), false, None, "p", now).state, "online");
        assert_eq!(tunnel_view(&summary(true, false, &[]), false, None, "p", now).state, "offline");
    }

    /// The page's status is rebuilt as soon as this daemon starts or stops
    /// a tunnel, or Allow moves on, not only every few seconds.
    #[test]
    fn the_status_is_rebuilt_when_what_runs_changes() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let c = CachedStatus { at: Instant::now(), version: 3, running: ids(&[]), status: PairedStatus::default() };
        assert!(c.serves(3, &ids(&[])));
        assert!(!c.serves(3, &ids(&["tunnel-mkbpd"])), "a tunnel started since");
        assert!(!c.serves(4, &ids(&[])), "Allow moved on");
        let old = CachedStatus { at: Instant::now() - STATUS_TTL, ..c };
        assert!(!old.serves(3, &ids(&[])), "too old");
    }

    /// The "a new address usually works within a minute" note: only for a
    /// tunnel whose HTTPProxy is less than 10 minutes old.
    #[test]
    fn only_a_young_tunnel_gets_the_new_address_note() {
        // 2026-10-07T12:00:00Z
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_791_374_400);
        let mut t = summary(true, true, &["abc.datumproxy.net"]);
        t.created_at = Some("2026-10-07T11:55:00Z".into());
        assert!(tunnel_view(&t, true, None, "p", now).new_address, "5 minutes old");
        t.created_at = Some("2026-10-07T11:49:00Z".into());
        assert!(!tunnel_view(&t, true, None, "p", now).new_address, "11 minutes old");
        t.created_at = Some("2026-09-01T08:00:00Z".into());
        assert!(!tunnel_view(&t, true, None, "p", now).new_address, "a month old");
        t.created_at = None;
        assert!(!tunnel_view(&t, true, None, "p", now).new_address, "unknown");
    }

    /// Remove only takes a tunnel from the older list, which never holds
    /// the add-on's own tunnel or one from another machine.
    #[test]
    fn only_an_older_tunnel_is_removable() {
        let tunnels = vec![tunnel("tunnel-old", "home-assistant"), tunnel("tunnel-new", "ha-2"), tunnel("tunnel-far", "other")];
        let part = exclusive::partition(tunnels, "ha-2", |id| id != "tunnel-far");
        assert_eq!(removable(&part.older, "tunnel-old").map(|t| t.label.as_str()), Some("home-assistant"));
        assert!(removable(&part.older, "tunnel-new").is_none(), "the active tunnel");
        assert!(removable(&part.older, "tunnel-far").is_none(), "another machine's");
        assert!(removable(&part.older, "tunnel-nope").is_none());
    }

    #[test]
    fn the_proxy_step_reads_as_a_person_would_say_it() {
        let needed = ProxySetup::Needed { config: serde_json::json!({}) };
        for finished in [false, true] {
            assert_eq!(proxy_step_view(Some(&ProxySetup::Ready), None, finished).state, "ok");
            assert_eq!(proxy_step_view(Some(&needed), None, finished).state, "needed");
            let pending = proxy_step_view(Some(&ProxySetup::Pending), None, finished);
            assert_eq!(pending.state, "pending");
            assert!(pending.message.unwrap().contains("Settings → System → Network"));
            assert_eq!(proxy_step_view(None, None, finished).state, "unknown");
            let failed = (false, "A request ... still got 400".to_string());
            // Failed wins while Home Assistant reverts its trial of our change.
            assert_eq!(proxy_step_view(Some(&ProxySetup::Pending), Some(&failed), finished).state, "failed");
            assert_eq!(proxy_step_view(Some(&needed), Some(&failed), finished).message.as_deref(), Some(failed.1.as_str()));
            // Once it reads as on, that is what counts.
            assert_eq!(proxy_step_view(Some(&ProxySetup::Ready), Some(&failed), finished).state, "ok");
            // The setup step until finished; a row after.
            assert_eq!(proxy_step_view(Some(&needed), None, finished).setup, !finished);
        }
        // Allow's own change waiting is never someone else's: Allow is offered.
        for on_trial in [false, true] {
            let ours = ProxySetup::Ours { on_trial };
            assert_eq!(proxy_step_view(Some(&ours), None, false).state, "needed");
            assert_eq!(proxy_step_view(Some(&ours), None, true).state, "needed");
        }
        // Right after an Allow that worked, still the step, for its ✓ Done.
        let allowed = (true, ALLOWED_MESSAGE.to_string());
        let v = proxy_step_view(Some(&ProxySetup::Ready), Some(&allowed), true);
        assert_eq!((v.state, v.setup), ("ok", true));
        assert_eq!(outcome_message(&AllowOutcome::Allowed), (true, ALLOWED_MESSAGE.to_string()));
        assert!(!outcome_message(&AllowOutcome::PendingByOther).0);
        assert_eq!(outcome_message(&AllowOutcome::Failed("x".into())), (false, "x".to_string()));
    }

    fn step_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("addon-step-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Setup's last step, as the page goes through it, with Home
    /// Assistant's answers given (the Allow flow itself, configure → wait →
    /// verify → promote, is tested in `ha_core`).
    #[test]
    fn the_setup_step_from_start_to_done() {
        let needed = ProxySetup::Needed { config: serde_json::json!({}) };

        // Already set up: finished silently, never offered.
        let dir = step_dir("ready");
        let marker = dir.join(PROXY_STEP_MARKER);
        let job = StdMutex::new(ProxyJob::default());
        let v = settled_view(&marker, &job, Some(&ProxySetup::Ready), None);
        assert_eq!((v.state, v.setup), ("ok", false));
        assert!(marker.exists(), "remembered across restarts");

        // Not set up: offered as the last step, and still after a failed
        // Allow (Retry or Skip), which never finishes it.
        let dir = step_dir("allow");
        let marker = dir.join(PROXY_STEP_MARKER);
        let job = StdMutex::new(ProxyJob::default());
        let v = settled_view(&marker, &job, Some(&needed), None);
        assert_eq!((v.state, v.setup), ("needed", true));
        let failed = (false, "still got 400. Home Assistant goes back to the previous setting by itself within 5 minutes.".to_string());
        let v = settled_view(&marker, &job, Some(&needed), Some(&failed));
        assert_eq!((v.state, v.setup), ("failed", true));
        assert!(v.message.unwrap().contains("goes back"));
        assert!(!marker.exists());
        // Allow worked: finished, and shown with its ✓ until the page moves on.
        finish_step(&marker, &job);
        let allowed = (true, ALLOWED_MESSAGE.to_string());
        let v = settled_view(&marker, &job, Some(&ProxySetup::Ready), Some(&allowed));
        assert_eq!((v.state, v.setup), ("ok", true));
        assert!(marker.exists());
        // After a restart: a ✓ row.
        let job = StdMutex::new(ProxyJob::default());
        let v = settled_view(&marker, &job, Some(&ProxySetup::Ready), None);
        assert_eq!((v.state, v.setup), ("ok", false));

        // Skip: finished without touching Home Assistant; the row then
        // warns, with Allow. A failure shown before is dropped.
        let dir = step_dir("skip");
        let marker = dir.join(PROXY_STEP_MARKER);
        let job = StdMutex::new(ProxyJob { last: Some(failed.clone()), ..Default::default() });
        job.lock().unwrap().running = Some(AllowStep::Restarting);
        assert_eq!(skip_step(&marker, &job).unwrap_err().status, StatusCode::CONFLICT, "not while Allow runs");
        assert!(!marker.exists());
        job.lock().unwrap().running = None;
        skip_step(&marker, &job).unwrap();
        assert!(marker.exists());
        let last = job.lock().unwrap().last.clone();
        assert_eq!(last, None);
        let v = settled_view(&marker, &job, Some(&needed), None);
        assert_eq!((v.state, v.setup), ("needed", false));
        // Remembered across restarts.
        let job = StdMutex::new(ProxyJob::default());
        assert!(!settled_view(&marker, &job, Some(&needed), None).setup);

        // Home Assistant could not be asked: offered, and Skip still works.
        let dir = step_dir("unknown");
        let marker = dir.join(PROXY_STEP_MARKER);
        let job = StdMutex::new(ProxyJob::default());
        let v = settled_view(&marker, &job, None, None);
        assert_eq!((v.state, v.setup), ("unknown", true));
        for d in ["ready", "allow", "skip", "unknown"] {
            let _ = std::fs::remove_dir_all(step_dir(d));
        }
    }

    /// After a restart, Allow's own change on trial is taken up by itself,
    /// once, and only while no Allow runs; any other state never is.
    #[test]
    fn only_our_change_on_trial_is_confirmed_by_itself() {
        let job = StdMutex::new(ProxyJob::default());
        let ours = ProxySetup::Ours { on_trial: true };
        assert!(auto_confirm_due(Some(&ours), &job));
        for other in [
            ProxySetup::Pending,
            ProxySetup::Ours { on_trial: false },
            ProxySetup::Ready,
            ProxySetup::Needed { config: serde_json::json!({}) },
        ] {
            assert!(!auto_confirm_due(Some(&other), &job), "{other:?}");
        }
        assert!(!auto_confirm_due(None, &job));
        job.lock().unwrap().running = Some(AllowStep::Restarting);
        assert!(!auto_confirm_due(Some(&ours), &job), "an Allow runs");
        job.lock().unwrap().running = None;
        job.lock().unwrap().auto_tried = true;
        assert!(!auto_confirm_due(Some(&ours), &job), "once per run");
    }

    #[test]
    fn the_verify_target_is_the_hops_numeric_target() {
        let a = |s: &str| target_addr(&s.parse().unwrap());
        assert_eq!(a("http://127.0.0.1:8123"), Some("127.0.0.1:8123".parse().unwrap()));
        assert_eq!(a("http://127.0.0.1"), Some("127.0.0.1:80".parse().unwrap()));
        assert_eq!(a("http://[::1]:8123/"), Some("[::1]:8123".parse().unwrap()));
        assert_eq!(a("http://homeassistant.local:8123"), None);
    }

    /// The verify step's request carries `X-Forwarded-For` and goes to Home
    /// Assistant directly, from 127.0.0.1 like the tunnel's local hop: a
    /// Home Assistant that does not trust that address answers 400, one
    /// that does answers as usual.
    #[tokio::test]
    async fn verify_sends_a_forwarded_request_from_the_hops_address() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let trusted = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(StdMutex::new(Vec::<String>::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ha = listener.local_addr().unwrap();
        {
            let trusted = trusted.clone();
            let seen = seen.clone();
            tokio::spawn(async move {
                while let Ok((mut s, _)) = listener.accept().await {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                    seen.lock().unwrap().push(head.clone());
                    let forwarded = head.contains("x-forwarded-for: 203.0.113.10");
                    let status = if forwarded && !trusted.load(Ordering::SeqCst) { "400 Bad Request" } else { "200 OK" };
                    let _ = s
                        .write_all(format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").as_bytes())
                        .await;
                }
            });
        }
        // The address the check uses is the tunnel's hop's real target.
        let target = crate::parse_real_target(&format!("http://{ha}")).unwrap();
        let (handle, _) = crate::spawn_inspector(&target).await.unwrap();
        let target = target_addr(handle.real_target()).unwrap();
        assert_eq!(target, ha);
        assert_eq!(verify_forwarded(target).await, Ok(400));
        trusted.store(true, Ordering::SeqCst);
        assert_eq!(verify_forwarded(target).await, Ok(200));
        {
            let seen = seen.lock().unwrap();
            assert!(seen.iter().all(|h| h.contains("x-forwarded-proto: https")), "{seen:?}");
        }
        drop(handle);
        let gone = verify_forwarded("127.0.0.1:9".parse().unwrap()).await;
        assert!(gone.is_err(), "{gone:?}");
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
            trusted_proxies: Some(proxy_step_view(None, Some(&(false, "boom".into())), false)),
            older: vec![tunnel_view(&summary(false, false, &["old.datumproxy.net"]), false, None, "p-1", SystemTime::now())],
            ..Default::default()
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("SECRET") && !json.contains("private_key") && !json.contains("token"), "{json}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
