//! Pairing driven from a page: the Home Assistant add-on's setup panel.
//!
//! [`super::pairing`] does the work; this only runs it on request and keeps
//! what a page needs to show, as plain data ([`SetupStatus`]), while it
//! runs:
//!
//! 1. Nothing happens until someone asks ([`SetupController::start`]), so
//!    that a code is issued for a person who is looking at the page, not
//!    for a log nobody reads.
//! 2. The link and code, then who approved, then the projects to choose
//!    from. Pairing waits for a choice even with only one project
//!    ([`super::pairing::PairingConfig::confirm_project`]), and the
//!    configured `project`, if any, is only the suggestion the page
//!    preselects.
//! 3. The choice ([`SetupController::choose`]), then each step as it
//!    finishes, then done or why not. A failed run can be started again.
//!
//! The status never holds the person's token, nor anything that could
//! carry it: only what [`PairingEvent`]s carry, which is plain data by
//! design, and error messages, which pairing scrubs of the token.
//!
//! The choice reaches pairing through its [`ProjectSource`]. Besides the
//! page, that source can also watch another one (the add-on's saved
//! options): someone who only has the log or the notification can still
//! set `project` on the Configuration tab and Save, as before the page
//! existed. A saved value only counts once it differs from the one the run
//! started with, which is the suggestion, not a choice.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::pairing::{
    self, PairedKey, PairingConfig, PairingError, PairingEvent, ProjectChoice, ProjectSource, ProjectWait,
    normalize_project,
};

/// How often pairing looks for a choice made on the page. A person clicked
/// and is watching, so this is short.
pub const PAGE_POLL: Duration = Duration::from_secs(1);
/// How often the fallback source (the Supervisor) is asked at most.
pub const FALLBACK_POLL: Duration = Duration::from_secs(5);

/// Where a run is. The page shows one screen per phase.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Nothing started: the "Connect to Datum" button.
    #[default]
    Idle,
    /// Asking Datum for a code.
    Starting,
    /// Waiting for the person to approve [`SetupStatus::code`].
    Code,
    /// Approved; finding the projects.
    Approved,
    /// Waiting for a project to be chosen from [`SetupStatus::projects`].
    Choose,
    /// Creating the service account, the grant and the key.
    Working,
    /// The key is saved.
    Done,
    /// Stopped; [`SetupStatus::error`] says why. Can be started again.
    Failed,
}

/// The link and code to approve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodeView {
    pub url: String,
    pub user_code: String,
    pub expires_in_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectView {
    pub id: String,
    pub display_name: String,
    pub organization: String,
}

impl From<&ProjectChoice> for ProjectView {
    fn from(p: &ProjectChoice) -> Self {
        Self {
            id: p.id.clone(),
            display_name: p.display_name.clone(),
            organization: p.organization.clone(),
        }
    }
}

/// What has been made so far, for the progress list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Steps {
    pub service_account: bool,
    pub access: bool,
    pub key: bool,
}

/// Why a run stopped, as the page shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// No code was approved in time, or the approval ran out while waiting
    /// for a project: offer a new code.
    Expired,
    /// Declined on the approval screen.
    Denied,
    /// The service account was made but could not be granted access.
    Forbidden,
    /// Anything else: offer to try again.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorView {
    pub kind: ErrorKind,
    pub message: String,
}

/// Everything the page shows. Never holds a token (see the module docs).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SetupStatus {
    pub phase: Phase,
    pub code: Option<CodeView>,
    /// The previous code expired unapproved, and [`SetupStatus::code`] is
    /// its replacement.
    pub code_renewed: bool,
    /// Who approved.
    pub email: Option<String>,
    pub projects: Vec<ProjectView>,
    /// The project to preselect: the configured one.
    pub suggested: Option<String>,
    /// The project chosen, once it is.
    pub project: Option<String>,
    pub organization: Option<String>,
    pub service_account: Option<String>,
    pub steps: Steps,
    pub error: Option<ErrorView>,
}

impl SetupStatus {
    fn running(&self) -> bool {
        matches!(
            self.phase,
            Phase::Starting | Phase::Code | Phase::Approved | Phase::Choose | Phase::Working
        )
    }
}

/// [`SetupController::choose`] refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChooseError {
    #[error("Not waiting for a project right now.")]
    NotChoosing,
    #[error("That project isn't one of the projects listed.")]
    Unknown,
}

/// Told about every event and the outcome, for the log and notifications.
pub trait SetupObserver: Send + Sync {
    fn event(&self, _event: &PairingEvent) {}
    fn outcome(&self, _result: &Result<PairedKey, PairingError>) {}
}

struct NoObserver;
impl SetupObserver for NoObserver {}

/// Runs pairing for a page. Cheap to clone; every clone is the same run.
#[derive(Clone)]
pub struct SetupController {
    inner: Arc<Inner>,
}

struct Inner {
    /// Copied for each run, with this module's wait in it.
    cfg: PairingConfig,
    source: Arc<PageSource>,
    status: Mutex<SetupStatus>,
    observer: Arc<dyn SetupObserver>,
    done: tokio::sync::watch::Sender<Option<PairedKey>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SetupController {
    /// `cfg.project` becomes the suggestion; `fallback` is where else a
    /// choice may come from (see the module docs). Any `project_wait` in
    /// `cfg` is replaced.
    pub fn new(
        cfg: PairingConfig,
        fallback: Option<Arc<dyn ProjectSource>>,
        observer: Option<Arc<dyn SetupObserver>>,
    ) -> Self {
        Self::with_polls(cfg, fallback, observer, PAGE_POLL, FALLBACK_POLL)
    }

    /// [`SetupController::new`] with other intervals. Only tests change them.
    fn with_polls(
        mut cfg: PairingConfig,
        fallback: Option<Arc<dyn ProjectSource>>,
        observer: Option<Arc<dyn SetupObserver>>,
        page_poll: Duration,
        fallback_every: Duration,
    ) -> Self {
        let suggested = cfg.project.as_deref().and_then(normalize_project);
        let source = Arc::new(PageSource {
            page: Mutex::new(None),
            fallback,
            initial: suggested.clone(),
            fallback_every,
            last_fallback: Mutex::new(None),
        });
        cfg.confirm_project = true;
        cfg.project_wait = Some(ProjectWait {
            source: source.clone(),
            poll: page_poll,
            max: pairing::DEFAULT_PROJECT_WAIT,
        });
        let (done, _) = tokio::sync::watch::channel(None);
        Self {
            inner: Arc::new(Inner {
                cfg,
                source,
                status: Mutex::new(SetupStatus { suggested, ..Default::default() }),
                observer: observer.unwrap_or_else(|| Arc::new(NoObserver)),
                done,
                task: Mutex::new(None),
            }),
        }
    }

    /// The page's view, as of now.
    pub fn status(&self) -> SetupStatus {
        self.inner.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Whether a login saved by an earlier run is waiting to be picked up,
    /// in which case starting needs no new approval.
    pub fn has_saved_session(&self) -> bool {
        self.inner.cfg.session_file.as_deref().is_some_and(std::path::Path::exists)
    }

    /// Starts a run, unless one is going or done. A failed run is replaced
    /// by a new one, which asks for a new code.
    pub fn start(&self) {
        let mut status = self.inner.status.lock().unwrap_or_else(|e| e.into_inner());
        if status.running() || status.phase == Phase::Done {
            return;
        }
        *status = SetupStatus {
            phase: Phase::Starting,
            suggested: status.suggested.clone(),
            ..Default::default()
        };
        *self.inner.source.page.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let inner = self.inner.clone();
        let task = tokio::spawn(async move {
            let observer = inner.observer.clone();
            let mut on_event = |event: PairingEvent| {
                inner.apply(&event);
                observer.event(&event);
            };
            let result = pairing::pair(&inner.cfg, &mut on_event).await;
            inner.finish(&result);
            observer.outcome(&result);
            if let Ok(key) = result {
                inner.done.send_replace(Some(key));
            }
        });
        *self.inner.task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
    }

    /// Chooses `project`, which must be one of those listed.
    pub fn choose(&self, project: &str) -> Result<(), ChooseError> {
        let mut status = self.inner.status.lock().unwrap_or_else(|e| e.into_inner());
        if status.phase != Phase::Choose {
            return Err(ChooseError::NotChoosing);
        }
        let wanted = normalize_project(project).ok_or(ChooseError::Unknown)?;
        let Some(p) = status.projects.iter().find(|p| p.id.eq_ignore_ascii_case(&wanted)).cloned() else {
            return Err(ChooseError::Unknown);
        };
        *self.inner.source.page.lock().unwrap_or_else(|e| e.into_inner()) = Some(p.id.clone());
        status.phase = Phase::Working;
        status.project = Some(p.id);
        status.organization = Some(p.organization);
        Ok(())
    }

    /// Resolves once a run has saved the key.
    pub async fn paired(&self) -> PairedKey {
        let mut rx = self.inner.done.subscribe();
        loop {
            if let Some(key) = rx.borrow_and_update().clone() {
                return key;
            }
            if rx.changed().await.is_err() {
                // The sender lives in `inner`, which `self` holds.
                std::future::pending::<()>().await;
            }
        }
    }

    /// Stops a run in progress, as the add-on stopping does. A login saved
    /// while it waited for a project is left for the next start.
    pub async fn stop(&self) {
        let task = self.inner.task.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Inner {
    fn apply(&self, event: &PairingEvent) {
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        match event {
            PairingEvent::Code { url, user_code, expires_in } => {
                s.phase = Phase::Code;
                s.code = Some(CodeView {
                    url: url.clone(),
                    user_code: user_code.clone(),
                    expires_in_secs: expires_in.as_secs(),
                });
            }
            PairingEvent::CodeExpired => s.code_renewed = true,
            PairingEvent::Approved { email } | PairingEvent::Resumed { email } => {
                s.phase = Phase::Approved;
                s.code = None;
                s.email = Some(email.clone());
            }
            PairingEvent::SessionDropped { .. } => {}
            PairingEvent::ChooseProject { projects, .. } => {
                // A choice already made on the page stands; pairing only
                // has not picked it up yet.
                if s.phase != Phase::Working {
                    s.phase = Phase::Choose;
                }
                s.projects = projects.iter().map(ProjectView::from).collect();
            }
            PairingEvent::ProjectSelected { project, organization } => {
                s.phase = Phase::Working;
                s.project = Some(project.clone());
                s.organization = Some(organization.clone());
            }
            PairingEvent::ServiceAccountCreated { email, .. } | PairingEvent::ServiceAccountReused { email, .. } => {
                s.phase = Phase::Working;
                s.service_account = Some(email.clone());
                s.steps.service_account = true;
            }
            PairingEvent::AccessGranted | PairingEvent::AccessNotConfirmed { .. } => s.steps.access = true,
            PairingEvent::KeySaved { .. } => s.steps.key = true,
        }
    }

    fn finish(&self, result: &Result<PairedKey, PairingError>) {
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        match result {
            Ok(key) => {
                s.phase = Phase::Done;
                s.project = Some(key.project.clone());
                s.organization = Some(key.organization.clone());
                s.service_account = Some(key.service_account_email.clone());
                s.error = None;
            }
            Err(e) => {
                s.phase = Phase::Failed;
                s.code = None;
                s.error = Some(error_view(e));
            }
        }
    }
}

/// The page's words for why pairing stopped. Pairing's own messages say
/// "restart", which on the page is a button instead.
pub fn error_view(e: &PairingError) -> ErrorView {
    let (kind, message) = match e {
        PairingError::TimedOut { .. } => (
            ErrorKind::Expired,
            "The code expired before it was approved.".to_string(),
        ),
        PairingError::ProjectNotChosen { .. } => (
            ErrorKind::Expired,
            "No project was chosen in time, and the sign-in is no longer good. Sign in again to continue.".to_string(),
        ),
        PairingError::Denied => (
            ErrorKind::Denied,
            "The request was declined on the Datum approval page.".to_string(),
        ),
        PairingError::BindingForbidden { email, project } => (
            ErrorKind::Forbidden,
            format!(
                "Your Datum login can create the service account but can't grant it access. Ask an organization owner or editor to grant role 'editor' to service account {email} on project {project}, then click Try again."
            ),
        ),
        other => (ErrorKind::Failed, other.to_string()),
    };
    ErrorView { kind, message }
}

/// The page's choice, else a changed value from the fallback source.
struct PageSource {
    page: Mutex<Option<String>>,
    fallback: Option<Arc<dyn ProjectSource>>,
    /// The fallback's value when the run started: the suggestion.
    initial: Option<String>,
    fallback_every: Duration,
    /// When the fallback was last asked, and what it said.
    last_fallback: Mutex<Option<(Instant, Option<String>)>>,
}

type SourceFuture<'a> = Pin<Box<dyn Future<Output = Result<Option<String>, String>> + Send + 'a>>;

impl ProjectSource for PageSource {
    fn project(&self) -> SourceFuture<'_> {
        Box::pin(async move {
            if let Some(p) = self.page.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                return Ok(Some(p));
            }
            let Some(fallback) = &self.fallback else {
                return Ok(None);
            };
            let recent = match &*self.last_fallback.lock().unwrap_or_else(|e| e.into_inner()) {
                Some((at, v)) if at.elapsed() < self.fallback_every => Some(v.clone()),
                _ => None,
            };
            let value = match recent {
                Some(v) => v,
                None => {
                    // An error is remembered as "nothing set", so that a
                    // Supervisor that is down is asked, and warned about,
                    // once per interval rather than on every poll.
                    let v = match fallback.project().await {
                        Ok(v) => v.as_deref().and_then(normalize_project),
                        Err(e) => {
                            tracing::warn!("pairing: cannot read the add-on's saved options, trying again: {e}");
                            None
                        }
                    };
                    *self.last_fallback.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), v.clone()));
                    v
                }
            };
            Ok(value.filter(|v| Some(v) != self.initial.as_ref()))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use secrecy::SecretString;
    use serde_json::Value;

    use super::*;
    use crate::datum_cloud::ha_supervisor::Supervisor;
    use crate::datum_cloud::pairing::tests::{
        Fake, ORG, PROJECT, Poll, SUP_TOKEN, TOKEN, TempDir, config, good_key, serve, temp_dir,
    };

    const OTHER: &str = "project-abc";

    fn two_projects() -> Fake {
        let mut fake = Fake::new();
        fake.projects.push(("other-org".into(), vec![(OTHER.into(), "uid-abc".into(), "Garage".into())]));
        fake
    }

    struct Setup {
        ctl: SetupController,
        fake: Arc<Mutex<Fake>>,
        cfg: PairingConfig,
        dir: TempDir,
    }

    fn controller(cfg: PairingConfig, fallback: Option<Arc<dyn ProjectSource>>) -> SetupController {
        SetupController::with_polls(cfg, fallback, None, Duration::from_millis(2), Duration::from_millis(2))
    }

    /// A controller against the fakes, with the Supervisor's saved options
    /// as the fallback, as the add-on runs it.
    async fn setup(mut fake: Fake, project: Option<&str>) -> Setup {
        if fake.key_json.is_empty() {
            fake.key_json = good_key();
        }
        let (base, fake) = serve(fake).await;
        let dir = temp_dir();
        let cfg = config(&base, &dir.0, project);
        let sup = Supervisor::new(&base, SecretString::from(SUP_TOKEN)).unwrap();
        let ctl = controller(cfg.clone(), Some(Arc::new(sup)));
        Setup { ctl, fake, cfg, dir }
    }

    /// Polls the status, as the page does, until `phase`, checking every
    /// status seen on the way for secrets. Returns the one in `phase`.
    async fn wait_for(ctl: &SetupController, phase: Phase) -> SetupStatus {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let status = ctl.status();
            assert_no_token(&status);
            if status.phase == phase {
                return status;
            }
            assert!(Instant::now() < deadline, "never reached {phase:?}; last {status:?}");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// What the page gets, as JSON, carries no secret.
    fn assert_no_token(status: &SetupStatus) {
        let json = serde_json::to_string(status).unwrap();
        for secret in [TOKEN, SUP_TOKEN, "refresh-SECRET", "device-code-", "private_key"] {
            assert!(!json.contains(secret), "{secret} in {json}");
        }
    }

    #[tokio::test]
    async fn idle_then_code_then_projects_then_choice_then_done() {
        let mut fake = two_projects();
        fake.polls = VecDeque::from([Poll::Pending, Poll::Approve]);
        let s = setup(fake, None).await;

        assert_eq!(s.ctl.status().phase, Phase::Idle);
        assert!(s.fake.lock().unwrap().requests.is_empty(), "nothing asked of Datum before Connect");

        s.ctl.start();
        let code = wait_for(&s.ctl, Phase::Code).await;
        assert_eq!(
            code.code,
            Some(CodeView {
                url: format!("{}/ui/v2/login/device?user_code=ABCD-EFG1", s.cfg.issuer),
                user_code: "ABCD-EFG1".into(),
                expires_in_secs: 300,
            })
        );
        s.ctl.start();
        assert_eq!(s.fake.lock().unwrap().codes_issued, 1, "a second Connect while one runs is ignored");

        let choose = wait_for(&s.ctl, Phase::Choose).await;
        assert_eq!(choose.email.as_deref(), Some("person@example.com"));
        assert_eq!(choose.code, None, "the code is gone once approved");
        assert_eq!(
            choose.projects,
            [
                ProjectView { id: PROJECT.into(), display_name: "Demo".into(), organization: ORG.into() },
                ProjectView { id: OTHER.into(), display_name: "Garage".into(), organization: "other-org".into() },
            ]
        );
        assert!(s.fake.lock().unwrap().sas.is_empty(), "nothing created before a choice");

        assert_eq!(s.ctl.choose("project-nope"), Err(ChooseError::Unknown));
        s.ctl
            .choose(&format!(" \"{}\" ", PROJECT.to_uppercase()))
            .expect("a listed project, however pasted");
        assert_eq!(s.ctl.status().phase, Phase::Working);
        assert_eq!(s.ctl.choose(OTHER), Err(ChooseError::NotChoosing), "chosen already");

        let done = wait_for(&s.ctl, Phase::Done).await;
        assert_eq!(done.project.as_deref(), Some(PROJECT));
        assert_eq!(done.organization.as_deref(), Some(ORG));
        assert_eq!(done.steps, Steps { service_account: true, access: true, key: true });
        assert!(done.service_account.as_deref().unwrap().starts_with("home-assistant-"));
        assert!(s.cfg.key_out.exists());
        let key = tokio::time::timeout(Duration::from_secs(5), s.ctl.paired())
            .await
            .expect("paired resolves");
        assert_eq!(key.project, PROJECT);

        s.ctl.start();
        assert_eq!(s.ctl.status().phase, Phase::Done, "nothing starts after done");
    }

    #[tokio::test]
    async fn the_only_project_still_waits_for_confirmation_and_the_configured_one_is_suggested() {
        let s = setup(Fake::new(), Some(PROJECT)).await;
        s.ctl.start();
        let choose = wait_for(&s.ctl, Phase::Choose).await;
        assert_eq!(choose.projects.len(), 1);
        assert_eq!(choose.suggested.as_deref(), Some(PROJECT));
        // The Supervisor keeps answering the configured project: that is
        // the suggestion, not a choice.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(s.ctl.status().phase, Phase::Choose);
        assert!(s.fake.lock().unwrap().sas.is_empty());
        s.ctl.choose(PROJECT).unwrap();
        wait_for(&s.ctl, Phase::Done).await;
    }

    #[tokio::test]
    async fn saving_the_project_on_the_configuration_tab_still_works() {
        let mut fake = two_projects();
        fake.options = VecDeque::from(["".into(), "".into(), "".into(), PROJECT.into()]);
        let s = setup(fake, None).await;
        s.ctl.start();
        let done = wait_for(&s.ctl, Phase::Done).await;
        assert_eq!(done.project.as_deref(), Some(PROJECT));
    }

    #[tokio::test]
    async fn an_expired_code_is_replaced_and_running_out_offers_a_new_one() {
        let mut fake = Fake::new();
        fake.polls = VecDeque::from([Poll::Expired, Poll::Pending]);
        let s = setup(fake, None).await;
        s.ctl.start();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let st = s.ctl.status();
            if st.code.as_ref().is_some_and(|c| c.user_code == "ABCD-EFG2") {
                assert!(st.code_renewed);
                break;
            }
            assert!(Instant::now() < deadline, "{st:?}");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // From here every code expires, until max_wait (10s in the test
        // config, an hour in production) runs out.
        s.fake.lock().unwrap().polls = VecDeque::from(vec![Poll::Expired; 100_000]);
        let failed = tokio::time::timeout(Duration::from_secs(20), wait_for(&s.ctl, Phase::Failed))
            .await
            .expect("gives up within max_wait");
        let err = failed.error.unwrap();
        assert_eq!(err.kind, ErrorKind::Expired);
        assert_eq!(err.message, "The code expired before it was approved.");
        assert_eq!(failed.code, None, "an expired code is not left on the page");

        // "Get a new code" is a new run.
        let issued = s.fake.lock().unwrap().codes_issued;
        s.fake.lock().unwrap().polls = VecDeque::from([Poll::Approve]);
        s.ctl.start();
        wait_for(&s.ctl, Phase::Choose).await;
        assert!(s.fake.lock().unwrap().codes_issued > issued);
        assert_eq!(s.ctl.status().error, None);
    }

    #[tokio::test]
    async fn a_declined_approval_says_so_and_can_be_retried() {
        let mut fake = Fake::new();
        fake.polls = VecDeque::from([Poll::Denied]);
        let s = setup(fake, None).await;
        s.ctl.start();
        let failed = wait_for(&s.ctl, Phase::Failed).await;
        assert_eq!(failed.error.unwrap().kind, ErrorKind::Denied);
        s.fake.lock().unwrap().polls = VecDeque::from([Poll::Approve]);
        s.ctl.start();
        wait_for(&s.ctl, Phase::Choose).await;
    }

    #[tokio::test]
    async fn a_refused_grant_asks_for_an_owner() {
        let mut fake = Fake::new();
        fake.binding_status = 403;
        let s = setup(fake, None).await;
        s.ctl.start();
        wait_for(&s.ctl, Phase::Choose).await;
        s.ctl.choose(PROJECT).unwrap();
        let failed = wait_for(&s.ctl, Phase::Failed).await;
        let err = failed.error.clone().unwrap();
        assert_eq!(err.kind, ErrorKind::Forbidden);
        let email = failed.service_account.clone().unwrap();
        assert_eq!(
            err.message,
            format!(
                "Your Datum login can create the service account but can't grant it access. Ask an organization owner or editor to grant role 'editor' to service account {email} on project {PROJECT}, then click Try again."
            )
        );
        assert!(failed.steps.service_account && !failed.steps.access && !failed.steps.key);
    }

    #[tokio::test]
    async fn other_failures_are_plain_and_never_quote_the_token() {
        let mut fake = Fake::new();
        fake.echo_auth_on = Some("/serviceaccounts".into());
        let s = setup(fake, None).await;
        s.ctl.start();
        wait_for(&s.ctl, Phase::Choose).await;
        s.ctl.choose(PROJECT).unwrap();
        let failed = wait_for(&s.ctl, Phase::Failed).await;
        let err = failed.error.unwrap();
        assert_eq!(err.kind, ErrorKind::Failed);
        assert!(err.message.contains("[redacted]"), "{}", err.message);
    }

    #[tokio::test]
    async fn the_status_json_has_the_shape_the_page_reads() {
        let s = setup(Fake::new(), Some(PROJECT)).await;
        let v: Value = serde_json::to_value(s.ctl.status()).unwrap();
        assert_eq!(v["phase"], "idle");
        assert_eq!(v["suggested"], PROJECT);
        assert_eq!(v["steps"], serde_json::json!({"service_account": false, "access": false, "key": false}));
        s.ctl.start();
        let st = wait_for(&s.ctl, Phase::Choose).await;
        let v: Value = serde_json::to_value(st).unwrap();
        assert_eq!(v["phase"], "choose");
        assert_eq!(v["projects"][0]["display_name"], "Demo");
    }

    #[tokio::test]
    async fn stopping_while_choosing_keeps_the_login_for_the_next_start() {
        let s = setup(two_projects(), None).await;
        let mut cfg = s.cfg.clone();
        cfg.session_file = Some(s.dir.0.join("pairing-session.json"));
        let ctl = controller(cfg.clone(), None);
        assert!(!ctl.has_saved_session());
        ctl.start();
        wait_for(&ctl, Phase::Choose).await;
        ctl.stop().await;
        assert!(ctl.has_saved_session(), "kept for the restart");

        // The next start resumes with no new code.
        let issued = s.fake.lock().unwrap().codes_issued;
        let next = controller(cfg, None);
        assert!(next.has_saved_session());
        next.start();
        let choose = wait_for(&next, Phase::Choose).await;
        assert_eq!(choose.email.as_deref(), Some("person@example.com"));
        assert_eq!(s.fake.lock().unwrap().codes_issued, issued, "no new approval");
        next.choose(PROJECT).unwrap();
        wait_for(&next, Phase::Done).await;
        assert!(!next.has_saved_session(), "deleted once pairing ends");
    }
}
