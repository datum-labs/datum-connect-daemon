//! Pairing: get a service account key with a person's approval in a browser,
//! instead of having them create one in the portal and paste it in.
//!
//! An appliance must run on a service account key, not on a person's login
//! (see [`super::service_account`]), but making that key by hand is five
//! portal steps and a file to move. Pairing does those steps itself, once,
//! borrowing the person's login only for as long as it takes:
//!
//! 1. Device authorization (RFC 8628) against Datum's IdP: print a link and a
//!    code, poll until the person approves. A code that expires unapproved
//!    is replaced by a new one, up to [`PairingConfig::max_wait`].
//! 2. Find the project, and the organization it belongs to, through the
//!    person's organization memberships. With several and none chosen, or
//!    not the one configured, and a [`ProjectWait`] set, list them and wait
//!    for one to be chosen, holding the token in memory meanwhile, so that
//!    choosing does not cost a second approval.
//! 3. Create a ServiceAccount in the project and wait for its email.
//! 4. Grant it `editor` on the project with one PolicyBinding. The binding
//!    lives in the organization's control plane: a person cannot create
//!    bindings inside the project's own control plane, but an org editor can
//!    create this one.
//! 5. Create a key for it. The server generates the key pair and returns the
//!    whole key file, the one the portal downloads, once, in the create
//!    response. It is written to disk atomically, 0600.
//! 6. Drop the person's token. It is held in memory only, never written or
//!    logged, and scrubbed from every error message.
//!
//! The person's token is a [`SecretString`], so no `Debug` prints it. A
//! refresh token, if the IdP sends one, is never even deserialized.

use std::io::Write;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::{Method, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::service_account::{self, ServiceAccount};

/// datumctl's public login app. The add-on borrows it until it has its own,
/// which is why the approval screen says "datumctl".
/// From datum-cloud/datumctl `internal/authutil/login.go`.
pub const PROD_CLIENT_ID: &str = "328728232771788043";
pub const STAGING_CLIENT_ID: &str = "325848904128073754";

/// What datumctl's device login asks for (`login.go`), which is what was
/// tested end to end against the API. `offline_access` only adds a refresh
/// token, which pairing never keeps (it is not even deserialized), so it
/// costs nothing; whether the API accepts a token issued without it is
/// unverified. Override with [`SCOPE_ENV`] to try the narrower set.
pub const DEFAULT_SCOPE: &str = "openid profile email offline_access";

/// Overrides [`PROD_CLIENT_ID`] / [`STAGING_CLIENT_ID`].
pub const CLIENT_ID_ENV: &str = "DATUM_PAIRING_CLIENT_ID";
/// Overrides [`DEFAULT_SCOPE`].
pub const SCOPE_ENV: &str = "DATUM_PAIRING_SCOPE";

/// How long a person has, across however many codes, before pairing gives up.
pub const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(60 * 60);
/// How long the key is valid for.
pub const KEY_LIFETIME_DAYS: i64 = 365;

/// Marks what pairing created, in the style of the add-on's edge policies
/// (`connect.datum.net/managed-by`), so it can be told apart in the portal.
pub const CREATED_BY_ANNOTATION: &str = "connect.datum.net/created-by";
pub const CREATED_BY_VALUE: &str = "datum-connect-ha-addon";
/// Same key the daemon puts on its Connectors.
pub const DEVICE_NAME_ANNOTATION: &str = "datum.net/device-name";

/// The role the binding grants, as a real binding made in the portal has it.
const ROLE_NAME: &str = "editor";
const ROLE_NAMESPACE: &str = "datum-cloud";

const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// How many ticks to wait for a created resource to settle (SA email,
/// binding Ready). Both were near-immediate in production.
const SETTLE_TICKS: u32 = 60;
const SA_NAME_PREFIX: &str = "home-assistant-";
/// How long before the person's token expires to stop waiting for a
/// project, in ticks: enough to create the service account, binding and
/// key with the token still valid.
const TOKEN_MARGIN_TICKS: u64 = 120;

const RM: &str = "resourcemanager.miloapis.com/v1alpha1";
const IAM: &str = "iam.miloapis.com/v1alpha1";
const IDENTITY: &str = "identity.miloapis.com/v1alpha1";

/// Where pairing reads the project a person chooses while it waits.
pub trait ProjectSource: Send + Sync {
    /// The project set now, if any. An error is logged and asked again.
    fn project(&self) -> Pin<Box<dyn Future<Output = Result<Option<String>, String>> + Send + '_>>;
}

/// Between polls of the [`ProjectSource`].
pub const DEFAULT_PROJECT_POLL: Duration = Duration::from_secs(5);
/// The longest pairing waits for a project, even with the token still valid.
pub const DEFAULT_PROJECT_WAIT: Duration = Duration::from_secs(30 * 60);

/// Wait for a project to be chosen instead of stopping. See [`pair`].
#[derive(Clone)]
pub struct ProjectWait {
    pub source: Arc<dyn ProjectSource>,
    pub poll: Duration,
    /// Also bounded by the person's token: waiting stops two minutes (120
    /// ticks) before it expires, if that is sooner.
    pub max: Duration,
}

impl ProjectWait {
    pub fn new(source: Arc<dyn ProjectSource>) -> Self {
        Self {
            source,
            poll: DEFAULT_PROJECT_POLL,
            max: DEFAULT_PROJECT_WAIT,
        }
    }
}

impl std::fmt::Debug for ProjectWait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectWait")
            .field("poll", &self.poll)
            .field("max", &self.max)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct PairingConfig {
    /// The IdP, e.g. `https://auth.datum.net`.
    pub issuer: String,
    /// The Datum API, e.g. `https://api.datum.net`.
    pub api_url: String,
    pub client_id: String,
    pub scope: String,
    /// The project to pair with. `None` means "the only one the person can
    /// see"; with several, pairing lists them and stops, or waits with
    /// [`PairingConfig::project_wait`].
    pub project: Option<String>,
    /// When set, a missing or unknown project is waited for instead of
    /// ending pairing.
    pub project_wait: Option<ProjectWait>,
    /// Where the key file goes.
    pub key_out: PathBuf,
    /// Recorded on the service account, to tell which device it is for.
    pub device_name: Option<String>,
    pub max_wait: Duration,
    /// One second of protocol time: the unit of the IdP's `interval` and
    /// `expires_in`, of `slow_down`'s +5, and of the waits for created
    /// resources. Only tests change it.
    pub tick: Duration,
}

impl PairingConfig {
    /// Issuer from `DATUM_AUTH_ISSUER` (the same variable the service
    /// account exchange uses), API from `DATUM_API_HOST` / `DATUM_API_ENV`,
    /// client id and scope from [`CLIENT_ID_ENV`] and [`SCOPE_ENV`].
    pub fn from_env(project: Option<String>, key_out: PathBuf) -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let issuer = var(super::external_token_source::AUTH_ISSUER_ENV)
            .unwrap_or_else(|| service_account::DEFAULT_ISSUER.to_string());
        let issuer = issuer.trim_end_matches('/').to_string();
        let client_id = var(CLIENT_ID_ENV).unwrap_or_else(|| default_client_id(&issuer).to_string());
        Self {
            api_url: super::ApiEnv::default().api_url().trim_end_matches('/').to_string(),
            client_id,
            scope: var(SCOPE_ENV).unwrap_or_else(|| DEFAULT_SCOPE.to_string()),
            issuer,
            project: project.filter(|p| !p.trim().is_empty()),
            project_wait: None,
            key_out,
            device_name: Some(crate::friendly_device_name()).filter(|n| !n.is_empty()),
            max_wait: DEFAULT_MAX_WAIT,
            tick: Duration::from_secs(1),
        }
    }
}

/// datumctl's `ResolveClientID`: the staging app for a staging IdP.
fn default_client_id(issuer: &str) -> &'static str {
    let host = url::Url::parse(issuer)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    if host.ends_with(".staging.env.datum.net") {
        STAGING_CLIENT_ID
    } else {
        PROD_CLIENT_ID
    }
}

/// Progress, for the caller to show. Plain data; never carries a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingEvent {
    /// Show this to the person.
    Code {
        url: String,
        user_code: String,
        expires_in: Duration,
    },
    /// The last code expired unapproved; a [`PairingEvent::Code`] follows.
    CodeExpired,
    Approved { email: String },
    /// Several projects and none chosen, or not the one configured: pairing
    /// waits up to `wait` for one of `projects` to be set. Comes again, with
    /// `rejected`, each time one is set that is not listed.
    ChooseProject {
        projects: Vec<ProjectChoice>,
        rejected: Option<String>,
        wait: Duration,
    },
    ProjectSelected { project: String, organization: String },
    ServiceAccountCreated { email: String, project: String },
    /// A previous attempt created this one and was stopped at the grant.
    ServiceAccountReused { email: String, project: String },
    AccessGranted,
    /// The grant was refused again after a previous refusal. Pairing goes
    /// on, on the assumption that an owner has granted access since.
    AccessNotConfirmed { email: String, project: String },
    KeySaved { path: PathBuf },
}

/// A project the approving login can see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectChoice {
    pub id: String,
    pub display_name: String,
    pub organization: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error("The request was declined on the approval screen. Restart to get a new code.")]
    Denied,
    #[error("Nobody approved a code within {minutes} minutes. Restart to get a new code.")]
    TimedOut { minutes: u64 },
    #[error(
        "Choosing a project took too long: none of yours was set within {}, and the approval is no longer good. Restart to try again.",
        duration_words(*.waited)
    )]
    ProjectNotChosen { waited: Duration },
    #[error(
        "Your Datum login can create the service account but can't grant it access. Ask an organization owner or editor to grant role 'editor' to service account {email} on project {project}, then restart"
    )]
    BindingForbidden { email: String, project: String },
    #[error("{} already exists. Delete it to pair again.", .0.display())]
    KeyExists(PathBuf),
    /// Which project to use is the person's call: none, several, or not
    /// the one configured. The message lists what they can see.
    #[error("{0}")]
    Project(String),
    #[error("{0}")]
    Failed(String),
}

/// The outcome of a successful pairing.
#[derive(Debug, Clone)]
pub struct PairedKey {
    pub path: PathBuf,
    pub service_account_email: String,
    pub project: String,
    pub organization: String,
}

/// Runs the whole pairing. See the module docs.
pub async fn pair(
    cfg: &PairingConfig,
    on_event: &mut (dyn FnMut(PairingEvent) + Send),
) -> Result<PairedKey, PairingError> {
    if cfg.key_out.exists() {
        return Err(PairingError::KeyExists(cfg.key_out.clone()));
    }
    // With a wait, a malformed project is waited past like any other
    // project that is not there. It is only ever compared, never sent.
    if let Some(p) = &cfg.project
        && cfg.project_wait.is_none()
    {
        check_name("project", p)?;
    }
    let http = reqwest::Client::builder()
        .user_agent(crate::datum_http_user_agent())
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| PairingError::Failed(format!("cannot build HTTP client: {e}")))?;

    let login = device_login(cfg, &http, on_event).await?;
    on_event(PairingEvent::Approved {
        email: login.email.clone().unwrap_or_else(|| login.sub.clone()),
    });

    let result = {
        let api = Api {
            http: &http,
            base: cfg.api_url.trim_end_matches('/').to_string(),
            token: &login.token,
        };
        provision(cfg, &api, &login.sub, login.expires_at, on_event).await
    };
    // The person's token goes here, on every path.
    drop(login);
    result
}

async fn provision(
    cfg: &PairingConfig,
    api: &Api<'_>,
    sub: &str,
    token_expires_at: Option<Instant>,
    on_event: &mut (dyn FnMut(PairingEvent) + Send),
) -> Result<PairedKey, PairingError> {
    let projects = list_projects(api, sub).await?;
    let project = match choose_project(&projects, cfg.project.as_deref()) {
        Ok(p) => p,
        Err(e) => match &cfg.project_wait {
            Some(wait) if !projects.is_empty() => {
                wait_for_project(cfg, wait, &projects, token_expires_at, on_event).await?
            }
            _ => return Err(e),
        },
    };
    on_event(PairingEvent::ProjectSelected {
        project: project.name.clone(),
        organization: project.org.clone(),
    });

    // A previous attempt that was refused the grant left its service
    // account behind, and told the person to have it granted access and
    // restart. Picking it up again is what makes that advice work.
    let pending_path = pending_path(&cfg.key_out);
    let pending = Pending::load(&pending_path).filter(|p| p.project == project.name);
    let mut sa = None;
    let mut refused_before = false;
    if let Some(p) = pending {
        if let Some(found) = get_service_account(api, &project.name, &p.service_account).await?
            && found.uid == p.uid
            && !found.email.is_empty()
        {
            on_event(PairingEvent::ServiceAccountReused {
                email: found.email.clone(),
                project: project.name.clone(),
            });
            refused_before = p.binding_refused;
            sa = Some(found);
        }
    }
    let sa = match sa {
        Some(sa) => sa,
        None => {
            let sa = create_service_account(cfg, api, &project.name).await?;
            on_event(PairingEvent::ServiceAccountCreated {
                email: sa.email.clone(),
                project: project.name.clone(),
            });
            sa
        }
    };
    let mut state = Pending {
        project: project.name.clone(),
        service_account: sa.name.clone(),
        uid: sa.uid.clone(),
        email: sa.email.clone(),
        binding_refused: refused_before,
    };
    state.save(&pending_path)?;

    match grant_access(cfg, api, &project, &sa).await {
        Ok(()) => on_event(PairingEvent::AccessGranted),
        Err(PairingError::BindingForbidden { .. }) if refused_before => {
            on_event(PairingEvent::AccessNotConfirmed {
                email: sa.email.clone(),
                project: project.name.clone(),
            });
        }
        Err(e @ PairingError::BindingForbidden { .. }) => {
            state.binding_refused = true;
            state.save(&pending_path)?;
            return Err(e);
        }
        Err(e) => return Err(e),
    }

    let key = create_key(cfg, api, &project.name, &sa).await?;
    write_private_file(&cfg.key_out, &key)
        .map_err(|e| PairingError::Failed(format!("cannot save the key to {}: {e}", cfg.key_out.display())))?;
    let _ = std::fs::remove_file(&pending_path);
    on_event(PairingEvent::KeySaved {
        path: cfg.key_out.clone(),
    });
    Ok(PairedKey {
        path: cfg.key_out.clone(),
        service_account_email: sa.email,
        project: project.name,
        organization: project.org,
    })
}

// ---- Step 1: device authorization ----

/// The person's login. Lives only until provisioning ends.
struct Login {
    token: SecretString,
    /// When the token stops working, if the IdP said.
    expires_at: Option<Instant>,
    sub: String,
    email: Option<String>,
}

#[derive(Deserialize)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    #[serde(default)]
    interval: u64,
    #[serde(default)]
    expires_in: u64,
}

/// Only what is needed. A `refresh_token` in the response is never read.
#[derive(Deserialize)]
struct TokenSuccess {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Deserialize)]
struct OAuthError {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct IdClaims {
    sub: String,
    #[serde(default)]
    email: Option<String>,
}

enum CodeOutcome {
    Approved(Login),
    Expired,
}

async fn device_login(
    cfg: &PairingConfig,
    http: &reqwest::Client,
    on_event: &mut (dyn FnMut(PairingEvent) + Send),
) -> Result<Login, PairingError> {
    let deadline = Instant::now() + cfg.max_wait;
    let timed_out = || PairingError::TimedOut {
        minutes: cfg.max_wait.as_secs().div_ceil(60).max(1),
    };
    loop {
        if Instant::now() >= deadline {
            return Err(timed_out());
        }
        let auth = start_device_authorization(cfg, http).await?;
        let expires_in = Duration::from_secs(if auth.expires_in == 0 { 300 } else { auth.expires_in });
        on_event(PairingEvent::Code {
            url: verification_url(&cfg.issuer, &auth.user_code),
            user_code: auth.user_code.clone(),
            expires_in,
        });
        match poll_for_approval(cfg, http, &auth, deadline).await? {
            CodeOutcome::Approved(login) => return Ok(login),
            CodeOutcome::Expired => {
                if Instant::now() >= deadline {
                    return Err(timed_out());
                }
                on_event(PairingEvent::CodeExpired);
            }
        }
    }
}

/// datumctl forces the v2 login UI's device page regardless of what the IdP
/// returns as `verification_uri`, with the code filled in.
fn verification_url(issuer: &str, user_code: &str) -> String {
    let code: String = url::form_urlencoded::byte_serialize(user_code.as_bytes()).collect();
    format!("{}/ui/v2/login/device?user_code={code}", issuer.trim_end_matches('/'))
}

async fn start_device_authorization(
    cfg: &PairingConfig,
    http: &reqwest::Client,
) -> Result<DeviceAuthorization, PairingError> {
    let response = http
        .post(format!("{}/oauth/v2/device_authorization", cfg.issuer))
        .form(&[("client_id", cfg.client_id.as_str()), ("scope", cfg.scope.as_str())])
        .send()
        .await
        .map_err(|e| PairingError::Failed(format!("cannot reach Datum's login service: {e}")))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(PairingError::Failed(format!(
            "Datum's login service refused to start pairing (HTTP {status}): {}",
            excerpt(&body)
        )));
    }
    serde_json::from_str(&body).map_err(|_| {
        PairingError::Failed("Datum's login service sent an unexpected reply to the pairing request".into())
    })
}

async fn poll_for_approval(
    cfg: &PairingConfig,
    http: &reqwest::Client,
    auth: &DeviceAuthorization,
    deadline: Instant,
) -> Result<CodeOutcome, PairingError> {
    // RFC 8628 §3.2: 5 seconds when the server does not say.
    let mut interval = ticks(cfg, if auth.interval == 0 { 5 } else { auth.interval });
    let expires_at = Instant::now() + ticks(cfg, if auth.expires_in == 0 { 300 } else { auth.expires_in });
    loop {
        tokio::time::sleep(interval).await;
        let now = Instant::now();
        if now >= expires_at || now >= deadline {
            return Ok(CodeOutcome::Expired);
        }
        let sent = http
            .post(format!("{}/oauth/v2/token", cfg.issuer))
            .form(&[
                ("grant_type", DEVICE_CODE_GRANT),
                ("device_code", auth.device_code.as_str()),
                ("client_id", cfg.client_id.as_str()),
            ])
            .send()
            .await;
        // A network blip during a five-minute wait is not worth losing the
        // code over; the next poll tries again.
        let response = match sent {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("pairing: polling the login service failed, retrying: {e}");
                continue;
            }
        };
        let status = response.status();
        let Ok(body) = response.text().await else {
            continue;
        };
        if status.is_success() {
            // Never quote this body: it holds the token.
            let token: TokenSuccess = serde_json::from_str(&body).map_err(|_| {
                PairingError::Failed("Datum's login service sent an unexpected reply after approval".into())
            })?;
            let received = Instant::now();
            let lifetime = token.expires_in.or_else(|| jwt_lifetime_secs(&token.access_token));
            let token_secret = SecretString::from(token.access_token);
            let claims = identity(cfg, http, &token_secret, token.id_token.as_deref()).await?;
            check_name("user id", &claims.sub)?;
            return Ok(CodeOutcome::Approved(Login {
                token: token_secret,
                expires_at: lifetime.map(|secs| received + ticks(cfg, secs)),
                sub: claims.sub,
                email: claims.email.filter(|e| !e.is_empty()),
            }));
        }
        if status.is_server_error() {
            tracing::warn!("pairing: login service answered HTTP {status}, retrying");
            continue;
        }
        let Ok(err) = serde_json::from_str::<OAuthError>(&body) else {
            return Err(PairingError::Failed(format!(
                "Datum's login service refused the code (HTTP {status}): {}",
                excerpt(&body)
            )));
        };
        match err.error.as_str() {
            "authorization_pending" => {}
            "slow_down" => interval += ticks(cfg, 5),
            "expired_token" => return Ok(CodeOutcome::Expired),
            "access_denied" => return Err(PairingError::Denied),
            other => {
                let detail = err.error_description.map(|d| format!(": {d}")).unwrap_or_default();
                return Err(PairingError::Failed(format!(
                    "Datum's login service refused the code ({other}{detail})"
                )));
            }
        }
    }
}

/// Who approved. The ID token came straight from the token endpoint over
/// TLS, which OIDC Core §3.1.3.7 accepts in place of checking its
/// signature, and it is only used to address the person's own resources,
/// which the API authorizes against the access token anyway. Without an ID
/// token, ask the userinfo endpoint.
async fn identity(
    cfg: &PairingConfig,
    http: &reqwest::Client,
    token: &SecretString,
    id_token: Option<&str>,
) -> Result<IdClaims, PairingError> {
    if let Some(claims) = id_token.and_then(decode_jwt_claims) {
        return Ok(claims);
    }
    let response = http
        .get(format!("{}/oidc/v1/userinfo", cfg.issuer))
        .bearer_auth(token.expose_secret())
        .send()
        .await
        .map_err(|e| PairingError::Failed(redact(format!("cannot ask who approved: {e}"), token)))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(PairingError::Failed(redact(
            format!("cannot ask who approved (HTTP {status}): {}", excerpt(&body)),
            token,
        )));
    }
    serde_json::from_str(&body)
        .map_err(|_| PairingError::Failed("the login service did not say who approved".into()))
}

/// Seconds left on a JWT access token, from its `exp`, for an IdP that
/// leaves out `expires_in`. `None` for an opaque token.
fn jwt_lifetime_secs(jwt: &str) -> Option<u64> {
    #[derive(Deserialize)]
    struct Exp {
        exp: i64,
    }
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let exp: Exp = serde_json::from_slice(&bytes).ok()?;
    u64::try_from(exp.exp - chrono::Utc::now().timestamp()).ok()
}

fn decode_jwt_claims(jwt: &str) -> Option<IdClaims> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

// ---- The API, with the person's token ----

struct Api<'a> {
    http: &'a reqwest::Client,
    base: String,
    token: &'a SecretString,
}

struct Reply {
    status: StatusCode,
    body: Value,
    text: String,
}

impl Api<'_> {
    async fn call(&self, method: Method, path: &str, body: Option<&Value>, what: &str) -> Result<Reply, PairingError> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(self.token.expose_secret());
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| PairingError::Failed(redact(format!("{what} failed: {e}"), self.token)))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| PairingError::Failed(redact(format!("{what} failed reading the reply: {e}"), self.token)))?;
        let body = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok(Reply { status, body, text })
    }

    /// An unexpected reply as an error: the API's own message if it sent a
    /// Kubernetes Status, else the start of the body. Never the token.
    fn failed(&self, what: &str, reply: &Reply) -> PairingError {
        let detail = reply
            .body
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| excerpt(&reply.text));
        PairingError::Failed(redact(format!("{what} failed (HTTP {}): {detail}", reply.status), self.token))
    }
}

fn user_cp(sub: &str) -> String {
    format!("/apis/{IAM}/users/{sub}/control-plane")
}
fn org_cp(org: &str) -> String {
    format!("/apis/{RM}/organizations/{org}/control-plane")
}
fn project_cp(project: &str) -> String {
    format!("/apis/{RM}/projects/{project}/control-plane")
}

// ---- Step 2: which project ----

#[derive(Debug, Clone)]
struct ProjectRef {
    name: String,
    uid: String,
    org: String,
    display_name: String,
}

/// Mirrors datumctl's `internal/discovery`: organization memberships from
/// the person's own control plane, then each organization's projects.
async fn list_projects(api: &Api<'_>, sub: &str) -> Result<Vec<ProjectRef>, PairingError> {
    let what = "listing your Datum organizations";
    let reply = api
        .call(Method::GET, &format!("{}/apis/{RM}/organizationmemberships", user_cp(sub)), None, what)
        .await?;
    if !reply.status.is_success() {
        return Err(api.failed(what, &reply));
    }
    let mut orgs: Vec<String> = Vec::new();
    for item in items(&reply.body) {
        if let Some(org) = item.pointer("/spec/organizationRef/name").and_then(Value::as_str)
            && !orgs.iter().any(|o| o == org)
        {
            orgs.push(org.to_string());
        }
    }

    let mut projects = Vec::new();
    for org in &orgs {
        if check_name("organization", org).is_err() {
            continue;
        }
        let what = format!("listing the projects in organization {org}");
        let reply = api
            .call(Method::GET, &format!("{}/apis/{RM}/projects", org_cp(org)), None, &what)
            .await?;
        // A membership that does not let the person see projects is not
        // where the one they want is; skip it rather than fail.
        if matches!(reply.status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND) {
            tracing::warn!("pairing: cannot list projects in organization {org} (HTTP {})", reply.status);
            continue;
        }
        if !reply.status.is_success() {
            return Err(api.failed(&what, &reply));
        }
        for item in items(&reply.body) {
            let name = item.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default();
            let uid = item.pointer("/metadata/uid").and_then(Value::as_str).unwrap_or_default();
            if name.is_empty() || uid.is_empty() {
                continue;
            }
            let display_name = item
                .pointer("/metadata/annotations/kubernetes.io~1display-name")
                .and_then(Value::as_str)
                .filter(|d| !d.is_empty())
                .unwrap_or(name);
            projects.push(ProjectRef {
                name: name.to_string(),
                uid: uid.to_string(),
                org: org.clone(),
                display_name: display_name.to_string(),
            });
        }
    }

    Ok(projects)
}

/// The project to use, or why there is none: none, several, or not the one
/// configured. The error lists what the person can see.
fn choose_project(projects: &[ProjectRef], wanted: Option<&str>) -> Result<ProjectRef, PairingError> {
    let listing = || {
        projects
            .iter()
            .map(|p| format!("  {} ({}, organization {})", p.name, p.display_name, p.org))
            .collect::<Vec<_>>()
            .join("\n")
    };
    match wanted {
        Some(wanted) => match projects.iter().find(|p| p.name == wanted) {
            Some(p) => Ok(p.clone()),
            None if projects.is_empty() => Err(PairingError::Project(format!(
                "Project {wanted} was not found: your Datum login can't see any projects. Check that you approved with the right account."
            ))),
            None => Err(PairingError::Project(format!(
                "Project {wanted} was not found among the projects your Datum login can see. Set 'project' on the Configuration tab to one of these (the id, not the display name), then restart:\n{}",
                listing()
            ))),
        },
        None => match projects.len() {
            1 => Ok(projects[0].clone()),
            0 => Err(PairingError::Project(
                "Your Datum login can't see any projects. Create one in the Datum portal, or approve with an account that has one, then restart.".into(),
            )),
            n => Err(PairingError::Project(format!(
                "Your Datum login can see {n} projects. Set 'project' on the Configuration tab to the one to use (the id, not the display name), then restart:\n{}",
                listing()
            ))),
        },
    }
}

/// Lists `projects` and waits for one of them to be set in `wait.source`.
/// The person's token stays in memory meanwhile, so that once one is set,
/// pairing carries on with no second approval.
async fn wait_for_project(
    cfg: &PairingConfig,
    wait: &ProjectWait,
    projects: &[ProjectRef],
    token_expires_at: Option<Instant>,
    on_event: &mut (dyn FnMut(PairingEvent) + Send),
) -> Result<ProjectRef, PairingError> {
    let started = Instant::now();
    let mut deadline = started + wait.max;
    if let Some(expires) = token_expires_at {
        let usable_until = expires.checked_sub(ticks(cfg, TOKEN_MARGIN_TICKS)).unwrap_or(started);
        deadline = deadline.min(usable_until.max(started));
    }
    let total = deadline.saturating_duration_since(started);
    let choices: Vec<ProjectChoice> = projects
        .iter()
        .map(|p| ProjectChoice {
            id: p.name.clone(),
            display_name: p.display_name.clone(),
            organization: p.org.clone(),
        })
        .collect();
    let normalize = |v: Option<&str>| v.map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    // The value already judged: polls only act on a change from it.
    let mut seen = normalize(cfg.project.as_deref());
    on_event(PairingEvent::ChooseProject {
        projects: choices.clone(),
        rejected: seen.clone(),
        wait: total,
    });
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(PairingError::ProjectNotChosen { waited: total });
        }
        tokio::time::sleep(wait.poll.min(deadline - now)).await;
        if Instant::now() >= deadline {
            continue;
        }
        let current = match wait.source.project().await {
            Ok(v) => normalize(v.as_deref()),
            Err(e) => {
                tracing::warn!("pairing: cannot read the add-on's saved options, trying again: {e}");
                continue;
            }
        };
        if current == seen {
            continue;
        }
        seen = current.clone();
        if let Some(id) = &current
            && let Some(p) = projects.iter().find(|p| &p.name == id)
        {
            return Ok(p.clone());
        }
        // Cleared, or set to one that is not listed: show the list again,
        // saying which, and keep waiting.
        on_event(PairingEvent::ChooseProject {
            projects: choices.clone(),
            rejected: current,
            wait: deadline.saturating_duration_since(Instant::now()),
        });
    }
}

fn items(list: &Value) -> impl Iterator<Item = &Value> {
    list.get("items").and_then(Value::as_array).into_iter().flatten()
}

// ---- Step 3: the service account ----

#[derive(Debug, Clone)]
struct ServiceAccountRef {
    name: String,
    uid: String,
    email: String,
}

fn service_accounts_path(project: &str) -> String {
    format!("{}/apis/{IAM}/serviceaccounts", project_cp(project))
}

async fn get_service_account(
    api: &Api<'_>,
    project: &str,
    name: &str,
) -> Result<Option<ServiceAccountRef>, PairingError> {
    let what = format!("reading service account {name}");
    let reply = api
        .call(Method::GET, &format!("{}/{name}", service_accounts_path(project)), None, &what)
        .await?;
    if reply.status == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !reply.status.is_success() {
        return Err(api.failed(&what, &reply));
    }
    let field = |p: &str| reply.body.pointer(p).and_then(Value::as_str).unwrap_or_default().to_string();
    Ok(Some(ServiceAccountRef {
        name: name.to_string(),
        uid: field("/metadata/uid"),
        email: field("/status/email"),
    }))
}

async fn create_service_account(
    cfg: &PairingConfig,
    api: &Api<'_>,
    project: &str,
) -> Result<ServiceAccountRef, PairingError> {
    let mut annotations = serde_json::Map::new();
    annotations.insert(CREATED_BY_ANNOTATION.into(), CREATED_BY_VALUE.into());
    if let Some(device) = &cfg.device_name {
        annotations.insert(DEVICE_NAME_ANNOTATION.into(), device.clone().into());
    }
    let what = format!("creating a service account in project {project}");
    // Five random characters rarely collide; when they do, roll again.
    for _ in 0..3 {
        let name = format!("{SA_NAME_PREFIX}{}", random_suffix(5));
        let body = json!({
            "apiVersion": IAM,
            "kind": "ServiceAccount",
            "metadata": {"name": name, "annotations": annotations},
        });
        let reply = api
            .call(Method::POST, &service_accounts_path(project), Some(&body), &what)
            .await?;
        if reply.status == StatusCode::CONFLICT {
            continue;
        }
        if reply.status == StatusCode::FORBIDDEN {
            return Err(PairingError::Failed(redact(
                format!(
                    "Your Datum login may not create service accounts in project {project} (HTTP 403). Approve with an account that is an owner or editor of the project, or use your own service account key (see the add-on's documentation)."
                ),
                api.token,
            )));
        }
        if !reply.status.is_success() {
            return Err(api.failed(&what, &reply));
        }
        let created_uid = reply
            .body
            .pointer("/metadata/uid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // The email is filled in by a controller, a moment after creation.
        for _ in 0..SETTLE_TICKS {
            if let Some(sa) = get_service_account(api, project, &name).await?
                && !sa.email.is_empty()
            {
                let uid = if sa.uid.is_empty() { created_uid } else { sa.uid };
                return Ok(ServiceAccountRef { uid, ..sa });
            }
            tokio::time::sleep(cfg.tick).await;
        }
        return Err(PairingError::Failed(format!(
            "Created service account {name} in project {project}, but it was not given an email within a minute. Delete it in the portal and restart."
        )));
    }
    Err(PairingError::Failed(format!(
        "{what} failed: three random names in a row were taken"
    )))
}

// ---- Step 4: one PolicyBinding ----

async fn grant_access(
    cfg: &PairingConfig,
    api: &Api<'_>,
    project: &ProjectRef,
    sa: &ServiceAccountRef,
) -> Result<(), PairingError> {
    let namespace = format!("organization-{}", project.org);
    let name = format!("{}-{}", project.name, sa.name);
    let path = format!(
        "{}/apis/{IAM}/namespaces/{namespace}/policybindings",
        org_cp(&project.org)
    );
    // The shape of a binding the portal made, field for field.
    let body = json!({
        "apiVersion": IAM,
        "kind": "PolicyBinding",
        "metadata": {"name": name, "namespace": namespace},
        "spec": {
            "resourceSelector": {"resourceRef": {
                "apiGroup": "resourcemanager.miloapis.com",
                "kind": "Project",
                "name": project.name,
                "uid": project.uid,
            }},
            "roleRef": {"name": ROLE_NAME, "namespace": ROLE_NAMESPACE},
            "subjects": [{"kind": "ServiceAccount", "name": sa.name, "uid": sa.uid}],
        },
    });
    let what = format!("granting {} access to project {}", sa.email, project.name);
    let reply = api.call(Method::POST, &path, Some(&body), &what).await?;
    match reply.status {
        // Conflict: an earlier attempt made this exact binding (the name
        // carries the service account's random suffix).
        s if s.is_success() || s == StatusCode::CONFLICT => {}
        StatusCode::FORBIDDEN => {
            return Err(PairingError::BindingForbidden {
                email: sa.email.clone(),
                project: project.name.clone(),
            });
        }
        _ => return Err(api.failed(&what, &reply)),
    }

    let mut last = String::new();
    for _ in 0..SETTLE_TICKS {
        let reply = api
            .call(Method::GET, &format!("{path}/{name}"), None, &what)
            .await?;
        if !reply.status.is_success() {
            return Err(api.failed(&what, &reply));
        }
        let ready = reply
            .body
            .pointer("/status/conditions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|c| c.get("type").and_then(Value::as_str) == Some("Ready"));
        if let Some(ready) = ready {
            if ready.get("status").and_then(Value::as_str) == Some("True") {
                return Ok(());
            }
            last = ready
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
        }
        tokio::time::sleep(cfg.tick).await;
    }
    let detail = if last.is_empty() { String::new() } else { format!(": {last}") };
    Err(PairingError::Failed(redact(
        format!("Created policy binding {name}, but it did not become ready within a minute{detail}"),
        api.token,
    )))
}

// ---- Step 5: the key ----

async fn create_key(
    cfg: &PairingConfig,
    api: &Api<'_>,
    project: &str,
    sa: &ServiceAccountRef,
) -> Result<String, PairingError> {
    let path = format!("{}/apis/{IDENTITY}/serviceaccountkeys", project_cp(project));
    let expires = (chrono::Utc::now() + chrono::Duration::days(KEY_LIFETIME_DAYS))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let what = format!("creating a key for {}", sa.email);
    for attempt in 0..3 {
        let name = if attempt == 0 {
            format!("{}-key", sa.name)
        } else {
            format!("{}-key-{}", sa.name, random_suffix(5))
        };
        // No publicKey: the server generates the pair and returns the
        // private half, once, in this response.
        let body = json!({
            "apiVersion": IDENTITY,
            "kind": "ServiceAccountKey",
            "metadata": {"name": name},
            "spec": {"serviceAccountUserName": sa.email, "expirationDate": expires},
        });
        let reply = api.call(Method::POST, &path, Some(&body), &what).await?;
        if reply.status == StatusCode::CONFLICT {
            continue;
        }
        if !reply.status.is_success() {
            return Err(api.failed(&what, &reply));
        }
        // milo-os/zitadel-provider sets status.privateKey to the key file
        // marshalled to a string, not to a nested object.
        let Some(key) = reply.body.pointer("/status/privateKey").and_then(Value::as_str) else {
            return Err(PairingError::Failed(format!(
                "Created key {name} for {}, but Datum did not return it. Delete the key in the portal and restart.",
                sa.email
            )));
        };
        validate_key(key, &cfg.issuer)?;
        return Ok(key.to_string());
    }
    Err(PairingError::Failed(format!(
        "{what} failed: three key names in a row were taken"
    )))
}

/// The same check the add-on and the daemon make when they load a key, so
/// a key that would not work is never written. Never quotes the key.
fn validate_key(raw: &str, issuer: &str) -> Result<(), PairingError> {
    let bad = |why: &str| PairingError::Failed(format!("Datum returned a key that is not usable: {why}"));
    let v: Value = serde_json::from_str(raw).map_err(|_| bad("not JSON"))?;
    if v.get("type").and_then(Value::as_str) != Some(service_account::KEY_TYPE) {
        return Err(bad("wrong type"));
    }
    for field in ["client_id", "private_key_id", "private_key", "scope"] {
        if v.get(field).and_then(Value::as_str).is_none_or(str::is_empty) {
            return Err(bad(&format!("no {field}")));
        }
    }
    ServiceAccount::from_json(raw, issuer).map_err(|e| bad(&e.to_string()))?;
    Ok(())
}

/// Writes `contents` to `path` so that no reader ever sees a partial file:
/// a temporary file next to it, created 0600, synced, then renamed over.
pub fn write_private_file(path: &Path, contents: &str) -> std::io::Result<()> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "key".into());
    let tmp = dir.join(format!(".{file_name}.tmp-{}", random_suffix(8)));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    // So the rename itself survives a power cut.
    #[cfg(unix)]
    {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

// ---- Resuming after a refused grant ----

/// What a stopped pairing leaves next to the key path, so a restart picks up
/// the same service account rather than making another one. No secrets.
#[derive(Debug, Serialize, Deserialize)]
struct Pending {
    project: String,
    service_account: String,
    uid: String,
    email: String,
    binding_refused: bool,
}

/// `<key_out>.pending`.
pub fn pending_path(key_out: &Path) -> PathBuf {
    let mut name = key_out.as_os_str().to_owned();
    name.push(".pending");
    PathBuf::from(name)
}

impl Pending {
    fn load(path: &Path) -> Option<Self> {
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    }

    fn save(&self, path: &Path) -> Result<(), PairingError> {
        let raw = serde_json::to_string(self).expect("plain struct serializes");
        write_private_file(path, &raw).map_err(|e| {
            PairingError::Failed(format!("cannot save pairing progress to {}: {e}", path.display()))
        })
    }
}

// ---- Helpers ----

/// "5 minutes", "1 minute", "45 seconds".
pub fn duration_words(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        return format!("{secs} seconds");
    }
    let m = secs.div_ceil(60);
    if m == 1 { "1 minute".into() } else { format!("{m} minutes") }
}

fn ticks(cfg: &PairingConfig, n: u64) -> Duration {
    cfg.tick.saturating_mul(u32::try_from(n).unwrap_or(u32::MAX))
}

/// Lowercase letters and digits, so names stay DNS-1123.
fn random_suffix(len: usize) -> String {
    use rand::Rng;
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..len)
        .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
        .collect()
}

/// Names go into URL paths, so only accept what a resource name can be.
fn check_name(what: &str, name: &str) -> Result<(), PairingError> {
    let ok = !name.is_empty()
        && name.len() <= 253
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'));
    if ok {
        Ok(())
    } else {
        Err(PairingError::Failed(format!("{what} {name:?} is not a valid name")))
    }
}

fn excerpt(body: &str) -> String {
    let flat: String = body.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let flat = flat.trim();
    if flat.is_empty() {
        return "no response body".into();
    }
    flat.chars().take(300).collect()
}

/// Belt and braces: nothing should put the token into a message, but if a
/// server ever echoes it back, it stops here.
fn redact(message: String, token: &SecretString) -> String {
    let secret = token.expose_secret();
    if secret.is_empty() {
        message
    } else {
        message.replace(secret, "[redacted]")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::datum_cloud::ha_supervisor::{self, PairingNotifier, Supervisor};
    use crate::datum_cloud::service_account::tests::{pkcs1_key_json, test_key};

    const TOKEN: &str = "user-access-token-SECRET-1234567890";
    const SUP_TOKEN: &str = "supervisor-token-SECRET-0987654321";
    const NOTIFY_PREFIX: &str = "/core/api/services/persistent_notification/";
    const OPTIONS_PATH: &str = "/addons/self/options/config";
    const SUB: &str = "300000000000000001";
    const ORG: &str = "datum-demos-iy50km";
    const PROJECT: &str = "project-7r4rl";
    const PROJECT_UID: &str = "11111111-2222-3333-4444-555555555555";

    #[derive(Clone, Copy, Debug)]
    enum Poll {
        Pending,
        SlowDown,
        Expired,
        Denied,
        Approve,
    }

    #[derive(Debug)]
    struct Request {
        method: String,
        path: String,
        authorization: Option<String>,
        body: String,
    }

    /// Datum's IdP and API on loopback, in the style of the token-exchange
    /// tests in `external_token_source.rs`: just enough of each endpoint,
    /// scripted per test, recording every request.
    struct Fake {
        polls: VecDeque<Poll>,
        codes_issued: u32,
        code_expires_in: u64,
        /// org -> [(name, uid, display name)]
        projects: Vec<(String, Vec<(String, String, String)>)>,
        /// GETs of a fresh SA before its email appears.
        email_after: u32,
        sa_gets: u32,
        sas: HashMap<String, (String, String)>, // name -> (uid, email)
        binding_status: u16,
        bindings: Vec<Value>,
        ready_after: u32,
        binding_gets: u32,
        key_json: String,
        /// Make this path fail with a body that echoes the Authorization header.
        echo_auth_on: Option<String>,
        /// The token response's `expires_in`.
        token_expires_in: u64,
        /// The Supervisor's answer to a notification call.
        notify_status: u16,
        /// The saved `project`, one per read of the add-on's options; the
        /// last one repeats. `!500` answers HTTP 500 instead.
        options: VecDeque<String>,
        requests: Vec<Request>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                polls: VecDeque::from([Poll::Approve]),
                codes_issued: 0,
                code_expires_in: 300,
                projects: vec![(
                    ORG.into(),
                    vec![(PROJECT.into(), PROJECT_UID.into(), "Demo".into())],
                )],
                email_after: 0,
                sa_gets: 0,
                sas: HashMap::new(),
                binding_status: 201,
                bindings: Vec::new(),
                ready_after: 0,
                binding_gets: 0,
                key_json: String::new(),
                echo_auth_on: None,
                token_expires_in: 43199,
                notify_status: 200,
                options: VecDeque::from([String::new()]),
                requests: Vec::new(),
            }
        }

        fn handle(&mut self, req: &Request) -> (u16, Value) {
            if let Some(p) = &self.echo_auth_on
                && req.path.ends_with(p.as_str())
            {
                return (500, json!({"message": format!("boom, you sent {:?}", req.authorization)}));
            }
            let path = req.path.as_str();
            let authed = req.authorization.as_deref() == Some(&format!("Bearer {TOKEN}"));
            let supervisor = req.authorization.as_deref() == Some(&format!("Bearer {SUP_TOKEN}"));
            if path.starts_with(NOTIFY_PREFIX) || path == OPTIONS_PATH {
                if !supervisor {
                    return (401, json!({"message": "401: Unauthorized"}));
                }
                if path == OPTIONS_PATH {
                    let project = if self.options.len() > 1 {
                        self.options.pop_front().unwrap()
                    } else {
                        self.options.front().cloned().unwrap_or_default()
                    };
                    if project == "!500" {
                        return (500, json!({"result": "error", "message": "busy"}));
                    }
                    return (200, json!({"result": "ok", "data": {
                        "project": project, "tunnel_label": "home-assistant", "repair": false,
                    }}));
                }
                return (self.notify_status, json!([]));
            }
            match (req.method.as_str(), path) {
                ("POST", "/oauth/v2/device_authorization") => {
                    self.codes_issued += 1;
                    let n = self.codes_issued;
                    (200, json!({
                        "device_code": format!("device-code-{n}"),
                        "user_code": format!("ABCD-EFG{n}"),
                        "verification_uri": "https://ignored.example/device",
                        "expires_in": self.code_expires_in,
                        "interval": 1,
                    }))
                }
                ("POST", "/oauth/v2/token") => match self.polls.pop_front().unwrap_or(Poll::Pending) {
                    Poll::Pending => (400, json!({"error": "authorization_pending"})),
                    Poll::SlowDown => (400, json!({"error": "slow_down"})),
                    Poll::Expired => (400, json!({"error": "expired_token"})),
                    Poll::Denied => (400, json!({"error": "access_denied"})),
                    Poll::Approve => {
                        let claims = json!({"sub": SUB, "email": "person@example.com"});
                        let id_token = format!(
                            "e30.{}.sig",
                            URL_SAFE_NO_PAD.encode(claims.to_string())
                        );
                        (200, json!({
                            "access_token": TOKEN,
                            "refresh_token": "refresh-SECRET",
                            "id_token": id_token,
                            "token_type": "Bearer",
                            "expires_in": self.token_expires_in,
                        }))
                    }
                },
                _ if !authed => (401, json!({"message": "Unauthorized"})),
                ("GET", p) if p == format!("/apis/iam.miloapis.com/v1alpha1/users/{SUB}/control-plane/apis/resourcemanager.miloapis.com/v1alpha1/organizationmemberships") => {
                    let items: Vec<Value> = self
                        .projects
                        .iter()
                        .map(|(org, _)| json!({"spec": {"organizationRef": {"name": org}}}))
                        .collect();
                    (200, json!({"items": items}))
                }
                ("GET", p) if p.starts_with("/apis/resourcemanager.miloapis.com/v1alpha1/organizations/") && p.ends_with("/control-plane/apis/resourcemanager.miloapis.com/v1alpha1/projects") => {
                    let org = p.split('/').nth(5).unwrap();
                    let items: Vec<Value> = self
                        .projects
                        .iter()
                        .filter(|(o, _)| o == org)
                        .flat_map(|(_, ps)| ps.iter())
                        .map(|(name, uid, display)| json!({"metadata": {
                            "name": name, "uid": uid,
                            "annotations": {"kubernetes.io/display-name": display},
                        }}))
                        .collect();
                    (200, json!({"items": items}))
                }
                ("POST", p) if p == format!("/apis/resourcemanager.miloapis.com/v1alpha1/projects/{PROJECT}/control-plane/apis/iam.miloapis.com/v1alpha1/serviceaccounts") => {
                    let v: Value = serde_json::from_str(&req.body).unwrap();
                    let name = v["metadata"]["name"].as_str().unwrap().to_string();
                    let uid = format!("uid-{name}");
                    let email = format!("{name}@{PROJECT}.identity.datumapis.com");
                    self.sas.insert(name.clone(), (uid.clone(), email));
                    (201, json!({"metadata": {"name": name, "uid": uid}}))
                }
                ("GET", p) if p.starts_with(&format!("/apis/resourcemanager.miloapis.com/v1alpha1/projects/{PROJECT}/control-plane/apis/iam.miloapis.com/v1alpha1/serviceaccounts/")) => {
                    let name = p.rsplit('/').next().unwrap();
                    let Some((uid, email)) = self.sas.get(name).cloned() else {
                        return (404, json!({"message": "not found"}));
                    };
                    self.sa_gets += 1;
                    let email = if self.sa_gets > self.email_after { email } else { String::new() };
                    (200, json!({"metadata": {"name": name, "uid": uid}, "status": {"email": email}}))
                }
                ("POST", p) if p == format!("/apis/resourcemanager.miloapis.com/v1alpha1/organizations/{ORG}/control-plane/apis/iam.miloapis.com/v1alpha1/namespaces/organization-{ORG}/policybindings") => {
                    if self.binding_status == 201 {
                        self.bindings.push(serde_json::from_str(&req.body).unwrap());
                    }
                    (self.binding_status, json!({"message": "binding reply"}))
                }
                ("GET", p) if p.starts_with(&format!("/apis/resourcemanager.miloapis.com/v1alpha1/organizations/{ORG}/control-plane/apis/iam.miloapis.com/v1alpha1/namespaces/organization-{ORG}/policybindings/")) => {
                    self.binding_gets += 1;
                    let status = if self.binding_gets > self.ready_after { "True" } else { "Unknown" };
                    (200, json!({"status": {"conditions": [
                        {"type": "TargetValid", "status": "True"},
                        {"type": "Ready", "status": status, "message": "reconciling"},
                    ]}}))
                }
                ("POST", p) if p == format!("/apis/resourcemanager.miloapis.com/v1alpha1/projects/{PROJECT}/control-plane/apis/identity.miloapis.com/v1alpha1/serviceaccountkeys") => {
                    let mut v: Value = serde_json::from_str(&req.body).unwrap();
                    v["status"] = json!({"authProviderKeyId": "kid-abc", "privateKey": self.key_json});
                    (201, v)
                }
                _ => (404, json!({"message": format!("no route for {} {}", req.method, req.path)})),
            }
        }
    }

    async fn serve(fake: Fake) -> (String, Arc<Mutex<Fake>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let fake = Arc::new(Mutex::new(fake));
        let shared = fake.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let fake = shared.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let (head, body) = loop {
                        let read = sock.read(&mut chunk).await.unwrap_or(0);
                        if read == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..read]);
                        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                            continue;
                        };
                        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                        let len = head
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_string()))
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break (head, String::from_utf8_lossy(&buf[end + 4..end + 4 + len]).into_owned());
                        }
                    };
                    let mut lines = head.lines();
                    let mut first = lines.next().unwrap_or_default().split(' ');
                    let method = first.next().unwrap_or_default().to_string();
                    let path = first.next().unwrap_or_default().to_string();
                    let authorization = lines
                        .filter_map(|l| l.split_once(':'))
                        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                        .map(|(_, v)| v.trim().to_string());
                    let req = Request { method, path, authorization, body };
                    let (status, reply) = {
                        let mut fake = fake.lock().unwrap();
                        let out = fake.handle(&req);
                        fake.requests.push(req);
                        out
                    };
                    let reply = reply.to_string();
                    let response = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (base, fake)
    }

    struct Run {
        result: Result<PairedKey, PairingError>,
        events: Vec<PairingEvent>,
        fake: Arc<Mutex<Fake>>,
        cfg: PairingConfig,
        _dir: TempDir,
    }

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn temp_dir() -> TempDir {
        let dir = std::env::temp_dir().join(format!("pairing-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn config(base: &str, dir: &Path, project: Option<&str>) -> PairingConfig {
        PairingConfig {
            issuer: base.into(),
            api_url: base.into(),
            client_id: PROD_CLIENT_ID.into(),
            scope: DEFAULT_SCOPE.into(),
            project: project.map(str::to_string),
            project_wait: None,
            key_out: dir.join("service-account.json"),
            device_name: Some("homeassistant".into()),
            max_wait: Duration::from_secs(10),
            tick: Duration::from_millis(1),
        }
    }

    fn good_key() -> String {
        pkcs1_key_json(&test_key())
    }

    async fn run_with(mut fake: Fake, project: Option<&str>, tweak: impl FnOnce(&mut PairingConfig)) -> Run {
        if fake.key_json.is_empty() {
            fake.key_json = good_key();
        }
        let (base, fake) = serve(fake).await;
        let dir = temp_dir();
        let mut cfg = config(&base, &dir.0, project);
        tweak(&mut cfg);
        run_cfg(cfg, fake, dir).await
    }

    async fn run_cfg(cfg: PairingConfig, fake: Arc<Mutex<Fake>>, dir: TempDir) -> Run {
        let mut events = Vec::new();
        let result = pair(&cfg, &mut |e| events.push(e)).await;
        Run { result, events, fake, cfg, _dir: dir }
    }

    fn token_requests(fake: &Fake) -> usize {
        fake.requests.iter().filter(|r| r.path == "/oauth/v2/token").count()
    }

    #[tokio::test]
    async fn pending_then_slow_down_then_approved_pairs_end_to_end() {
        let mut fake = Fake::new();
        fake.polls = VecDeque::from([Poll::Pending, Poll::SlowDown, Poll::Approve]);
        let run = run_with(fake, Some(PROJECT), |_| {}).await;
        let paired = run.result.expect("pairs");
        let fake = run.fake.lock().unwrap();
        assert_eq!(token_requests(&fake), 3);
        assert_eq!(fake.codes_issued, 1);

        // The device authorization request: datumctl's client and scope.
        let auth = &fake.requests[0];
        assert_eq!(auth.path, "/oauth/v2/device_authorization");
        let form: HashMap<String, String> =
            url::form_urlencoded::parse(auth.body.as_bytes()).into_owned().collect();
        assert_eq!(form["client_id"], PROD_CLIENT_ID);
        assert_eq!(form["scope"], "openid profile email offline_access");
        let poll = fake.requests.iter().find(|r| r.path == "/oauth/v2/token").unwrap();
        let form: HashMap<String, String> =
            url::form_urlencoded::parse(poll.body.as_bytes()).into_owned().collect();
        assert_eq!(form["grant_type"], DEVICE_CODE_GRANT);
        assert_eq!(form["device_code"], "device-code-1");

        let sa_name = fake.sas.keys().next().unwrap().clone();
        let email = format!("{sa_name}@{PROJECT}.identity.datumapis.com");
        assert_eq!(paired.service_account_email, email);
        assert_eq!(paired.project, PROJECT);
        assert_eq!(paired.organization, ORG);
        assert_eq!(
            run.events,
            vec![
                PairingEvent::Code {
                    url: format!("{}/ui/v2/login/device?user_code=ABCD-EFG1", run.cfg.issuer),
                    user_code: "ABCD-EFG1".into(),
                    expires_in: Duration::from_secs(300),
                },
                PairingEvent::Approved { email: "person@example.com".into() },
                PairingEvent::ProjectSelected { project: PROJECT.into(), organization: ORG.into() },
                PairingEvent::ServiceAccountCreated { email: email.clone(), project: PROJECT.into() },
                PairingEvent::AccessGranted,
                PairingEvent::KeySaved { path: run.cfg.key_out.clone() },
            ]
        );
        assert!(!pending_path(&run.cfg.key_out).exists(), "progress file removed on success");
    }

    #[tokio::test]
    async fn slow_down_adds_five_seconds_to_the_interval() {
        let mut fake = Fake::new();
        fake.polls = VecDeque::from([Poll::SlowDown, Poll::Approve]);
        let started = Instant::now();
        // 20ms ticks: interval 1 tick, then 6 ticks after slow_down.
        let run = run_with(fake, Some(PROJECT), |c| c.tick = Duration::from_millis(20)).await;
        run.result.expect("pairs");
        assert!(started.elapsed() >= Duration::from_millis(20 + 6 * 20), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn an_expired_code_is_replaced_with_a_new_one() {
        let mut fake = Fake::new();
        fake.polls = VecDeque::from([Poll::Pending, Poll::Expired, Poll::Approve]);
        let run = run_with(fake, Some(PROJECT), |_| {}).await;
        run.result.expect("pairs on the second code");
        let codes: Vec<_> = run
            .events
            .iter()
            .filter_map(|e| match e {
                PairingEvent::Code { user_code, .. } => Some(user_code.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(codes, ["ABCD-EFG1", "ABCD-EFG2"]);
        assert!(run.events.contains(&PairingEvent::CodeExpired));
        assert_eq!(run.fake.lock().unwrap().codes_issued, 2);
    }

    #[tokio::test]
    async fn a_code_past_its_expires_in_is_replaced_without_the_server_saying_so() {
        let mut fake = Fake::new();
        fake.code_expires_in = 3; // ticks
        fake.polls = VecDeque::from([Poll::Pending, Poll::Pending, Poll::Pending, Poll::Pending, Poll::Approve]);
        let run = run_with(fake, Some(PROJECT), |c| c.tick = Duration::from_millis(30)).await;
        run.result.expect("pairs");
        assert!(run.fake.lock().unwrap().codes_issued >= 2);
    }

    #[tokio::test]
    async fn gives_up_after_max_wait() {
        let mut fake = Fake::new();
        fake.polls = VecDeque::from(vec![Poll::Expired; 1000]);
        let run = run_with(fake, Some(PROJECT), |c| c.max_wait = Duration::from_millis(200)).await;
        let err = run.result.unwrap_err();
        assert!(matches!(err, PairingError::TimedOut { minutes: 1 }), "{err}");
        assert!(run.fake.lock().unwrap().codes_issued > 1);
        assert!(!run.cfg.key_out.exists());
    }

    #[tokio::test]
    async fn access_denied_stops() {
        let mut fake = Fake::new();
        fake.polls = VecDeque::from([Poll::Pending, Poll::Denied]);
        let run = run_with(fake, Some(PROJECT), |_| {}).await;
        assert!(matches!(run.result, Err(PairingError::Denied)));
        let fake = run.fake.lock().unwrap();
        assert!(fake.sas.is_empty());
        assert_eq!(fake.codes_issued, 1);
    }

    #[tokio::test]
    async fn the_only_project_is_used_when_none_is_set() {
        let run = run_with(Fake::new(), None, |_| {}).await;
        assert_eq!(run.result.expect("pairs").project, PROJECT);
    }

    #[tokio::test]
    async fn several_projects_and_none_set_lists_them() {
        let mut fake = Fake::new();
        fake.projects.push((
            "other-org".into(),
            vec![("project-abc".into(), "uid-abc".into(), "Garage".into())],
        ));
        let run = run_with(fake, None, |_| {}).await;
        let err = run.result.unwrap_err().to_string();
        assert!(err.contains("can see 2 projects"), "{err}");
        assert!(err.contains("set 'project' on the Configuration tab") || err.contains("Set 'project' on the Configuration tab"), "{err}");
        assert!(err.contains("project-7r4rl (Demo, organization datum-demos-iy50km)"), "{err}");
        assert!(err.contains("project-abc (Garage, organization other-org)"), "{err}");
        assert!(run.fake.lock().unwrap().sas.is_empty(), "nothing created");
    }

    #[tokio::test]
    async fn a_project_in_the_second_org_is_found_with_its_org() {
        let mut fake = Fake::new();
        fake.projects.insert(0, ("first-org".into(), vec![]));
        let run = run_with(fake, Some(PROJECT), |_| {}).await;
        assert_eq!(run.result.expect("pairs").organization, ORG);
    }

    #[tokio::test]
    async fn a_configured_project_that_is_not_there_is_an_error() {
        let run = run_with(Fake::new(), Some("project-nope"), |_| {}).await;
        let err = run.result.unwrap_err();
        assert!(matches!(err, PairingError::Project(_)));
        let err = err.to_string();
        assert!(err.contains("project-nope was not found"), "{err}");
        assert!(err.contains("project-7r4rl"), "{err}");
    }

    #[tokio::test]
    async fn waits_for_the_service_account_email() {
        let mut fake = Fake::new();
        fake.email_after = 3;
        let run = run_with(fake, Some(PROJECT), |_| {}).await;
        let paired = run.result.expect("pairs");
        let fake = run.fake.lock().unwrap();
        assert!(fake.sa_gets >= 4);
        let (name, (uid, _)) = fake.sas.iter().next().unwrap();
        assert!(name.starts_with("home-assistant-") && name.len() == "home-assistant-".len() + 5, "{name}");
        assert!(name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        assert!(paired.service_account_email.starts_with(name.as_str()));

        let create = fake
            .requests
            .iter()
            .find(|r| r.method == "POST" && r.path.ends_with("/serviceaccounts"))
            .unwrap();
        let body: Value = serde_json::from_str(&create.body).unwrap();
        assert_eq!(body["apiVersion"], "iam.miloapis.com/v1alpha1");
        assert_eq!(body["kind"], "ServiceAccount");
        assert!(body["metadata"].get("namespace").is_none(), "cluster-scoped");
        assert_eq!(body["metadata"]["annotations"][CREATED_BY_ANNOTATION], CREATED_BY_VALUE);
        assert_eq!(body["metadata"]["annotations"][DEVICE_NAME_ANNOTATION], "homeassistant");

        // The binding names that SA by name and uid, on the project by uid.
        let binding = &fake.bindings[0];
        assert_eq!(
            binding,
            &json!({
                "apiVersion": "iam.miloapis.com/v1alpha1",
                "kind": "PolicyBinding",
                "metadata": {"name": format!("{PROJECT}-{name}"), "namespace": format!("organization-{ORG}")},
                "spec": {
                    "resourceSelector": {"resourceRef": {
                        "apiGroup": "resourcemanager.miloapis.com", "kind": "Project",
                        "name": PROJECT, "uid": PROJECT_UID,
                    }},
                    "roleRef": {"name": "editor", "namespace": "datum-cloud"},
                    "subjects": [{"kind": "ServiceAccount", "name": name, "uid": uid}],
                },
            })
        );
    }

    #[tokio::test]
    async fn waits_for_the_binding_to_be_ready() {
        let mut fake = Fake::new();
        fake.ready_after = 4;
        let run = run_with(fake, Some(PROJECT), |_| {}).await;
        run.result.expect("pairs");
        assert_eq!(run.fake.lock().unwrap().binding_gets, 5);
    }

    #[tokio::test]
    async fn a_binding_that_never_gets_ready_is_an_error_and_no_key_is_made() {
        let mut fake = Fake::new();
        fake.ready_after = u32::MAX;
        let run = run_with(fake, Some(PROJECT), |_| {}).await;
        let err = run.result.unwrap_err().to_string();
        assert!(err.contains("did not become ready") && err.contains("reconciling"), "{err}");
        assert!(!run.fake.lock().unwrap().requests.iter().any(|r| r.path.ends_with("/serviceaccountkeys")));
    }

    #[tokio::test]
    async fn a_refused_binding_gives_the_guidance_and_a_restart_reuses_the_account() {
        let mut fake = Fake::new();
        fake.binding_status = 403;
        let run = run_with(fake, Some(PROJECT), |_| {}).await;
        let sa_name = run.fake.lock().unwrap().sas.keys().next().unwrap().clone();
        let email = format!("{sa_name}@{PROJECT}.identity.datumapis.com");
        assert_eq!(
            run.result.unwrap_err().to_string(),
            format!(
                "Your Datum login can create the service account but can't grant it access. Ask an organization owner or editor to grant role 'editor' to service account {email} on project {PROJECT}, then restart"
            )
        );
        assert!(!run.cfg.key_out.exists());
        assert!(pending_path(&run.cfg.key_out).exists(), "kept to resume");

        // The restart: same account, the grant refused again (the owner
        // granted it under another name), so carry on and make the key.
        {
            let mut fake = run.fake.lock().unwrap();
            fake.polls = VecDeque::from([Poll::Approve]);
        }
        let Run { cfg, fake, _dir, .. } = run;
        let again = run_cfg(cfg, fake, _dir).await;
        let paired = again.result.expect("pairs on restart");
        assert_eq!(paired.service_account_email, email);
        assert!(again.events.contains(&PairingEvent::ServiceAccountReused { email: email.clone(), project: PROJECT.into() }));
        assert!(again.events.contains(&PairingEvent::AccessNotConfirmed { email: email.clone(), project: PROJECT.into() }));
        assert_eq!(again.fake.lock().unwrap().sas.len(), 1, "no second service account");
        assert!(again.cfg.key_out.exists());
        assert!(!pending_path(&again.cfg.key_out).exists());
    }

    #[tokio::test]
    async fn the_key_is_written_whole_and_private() {
        let key = good_key();
        let mut fake = Fake::new();
        fake.key_json = key.clone();
        let run = run_with(fake, Some(PROJECT), |_| {}).await;
        run.result.expect("pairs");
        assert_eq!(std::fs::read_to_string(&run.cfg.key_out).unwrap(), key);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&run.cfg.key_out).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Nothing left over from the temp-file-and-rename.
        let names: Vec<_> = std::fs::read_dir(run.cfg.key_out.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["service-account.json"]);

        let fake = run.fake.lock().unwrap();
        let create = fake.requests.iter().find(|r| r.path.ends_with("/serviceaccountkeys")).unwrap();
        let body: Value = serde_json::from_str(&create.body).unwrap();
        let sa_name = fake.sas.keys().next().unwrap();
        assert_eq!(body["metadata"]["name"], format!("{sa_name}-key"));
        assert_eq!(body["spec"]["serviceAccountUserName"], format!("{sa_name}@{PROJECT}.identity.datumapis.com"));
        assert!(body["spec"].get("publicKey").is_none(), "server-generated");
        let expires = chrono::DateTime::parse_from_rfc3339(body["spec"]["expirationDate"].as_str().unwrap()).unwrap();
        let days = (expires.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_days();
        assert!((364..=365).contains(&days), "{days}");
    }

    #[tokio::test]
    async fn an_unusable_key_is_never_written() {
        let mut fake = Fake::new();
        let mut v: Value = serde_json::from_str(&good_key()).unwrap();
        v.as_object_mut().unwrap().remove("scope");
        fake.key_json = v.to_string();
        let run = run_with(fake, Some(PROJECT), |_| {}).await;
        let err = run.result.unwrap_err().to_string();
        assert!(err.contains("not usable: no scope"), "{err}");
        assert!(!run.cfg.key_out.exists());
    }

    #[tokio::test]
    async fn an_existing_key_file_is_never_overwritten() {
        let (base, fake) = serve(Fake::new()).await;
        let dir = temp_dir();
        let cfg = config(&base, &dir.0, Some(PROJECT));
        std::fs::write(&cfg.key_out, "keep me").unwrap();
        let run = run_cfg(cfg, fake, dir).await;
        assert!(matches!(run.result, Err(PairingError::KeyExists(_))));
        assert!(run.fake.lock().unwrap().requests.is_empty(), "no code asked for");
        assert_eq!(std::fs::read_to_string(&run.cfg.key_out).unwrap(), "keep me");
    }

    #[tokio::test]
    async fn the_token_never_appears_in_an_error() {
        // Every API step, made to fail with a body that quotes the request's
        // Authorization header back.
        for step in [
            "/organizationmemberships",
            "/projects",
            "/serviceaccounts",
            "/policybindings",
            "/serviceaccountkeys",
        ] {
            let mut fake = Fake::new();
            fake.echo_auth_on = Some(step.into());
            let run = run_with(fake, Some(PROJECT), |_| {}).await;
            let err = run.result.expect_err(step);
            let shown = format!("{err} / {err:?}");
            assert!(shown.contains("[redacted]"), "{step}: the echo reached the error: {shown}");
            assert!(!shown.contains(TOKEN), "{step}: token leaked: {shown}");
            assert!(!shown.contains("refresh-SECRET"), "{step}: refresh token leaked: {shown}");
        }
    }

    // ---- Waiting for a project, and the Home Assistant notification ----

    fn two_projects() -> Fake {
        let mut fake = Fake::new();
        fake.projects.push((
            "other-org".into(),
            vec![("project-abc".into(), "uid-abc".into(), "Garage".into())],
        ));
        fake
    }

    fn supervisor(base: &str) -> Supervisor {
        Supervisor::new(base, SecretString::from(SUP_TOKEN)).unwrap()
    }

    fn wait_on(sup: &Supervisor) -> ProjectWait {
        ProjectWait {
            source: Arc::new(sup.clone()),
            poll: Duration::from_millis(2),
            max: Duration::from_secs(10),
        }
    }

    /// As the add-on runs it: a wait on the Supervisor's saved options, and
    /// every event and the outcome shown as a notification.
    async fn run_notified(mut fake: Fake, project: Option<&str>, tweak: impl FnOnce(&mut PairingConfig)) -> Run {
        if fake.key_json.is_empty() {
            fake.key_json = good_key();
        }
        let (base, fake) = serve(fake).await;
        let dir = temp_dir();
        let mut cfg = config(&base, &dir.0, project);
        let sup = supervisor(&base);
        cfg.project_wait = Some(wait_on(&sup));
        tweak(&mut cfg);
        let notifier = PairingNotifier::spawn(sup);
        let mut events = Vec::new();
        let result = pair(&cfg, &mut |e| {
            notifier.event(&e);
            events.push(e);
        })
        .await;
        notifier.outcome(&result);
        notifier.finish().await;
        Run { result, events, fake, cfg, _dir: dir }
    }

    /// (service, body) of each notification call, in order, after checking
    /// each one carried the Supervisor token and nothing of the person's.
    fn notifications(fake: &Fake) -> Vec<(String, Value)> {
        fake.requests
            .iter()
            .filter_map(|r| {
                let service = r.path.strip_prefix(NOTIFY_PREFIX)?;
                assert_eq!(r.method, "POST");
                assert_eq!(r.authorization.as_deref(), Some(format!("Bearer {SUP_TOKEN}").as_str()));
                Some((service.to_string(), serde_json::from_str(&r.body).unwrap()))
            })
            .collect()
    }

    fn chooses(events: &[PairingEvent]) -> Vec<Option<String>> {
        events
            .iter()
            .filter_map(|e| match e {
                PairingEvent::ChooseProject { rejected, .. } => Some(rejected.clone()),
                _ => None,
            })
            .collect()
    }

    /// Nothing the person's login or the Supervisor's token could leak
    /// through: events (what the log prints), notification bodies, errors.
    fn assert_no_secrets(run: &Run) {
        let fake = run.fake.lock().unwrap();
        for r in fake.requests.iter().filter(|r| r.path.starts_with(NOTIFY_PREFIX) || r.path == OPTIONS_PATH) {
            assert_ne!(r.authorization.as_deref(), Some(format!("Bearer {TOKEN}").as_str()), "user token sent to the Supervisor");
            for secret in [TOKEN, SUP_TOKEN, "refresh-SECRET"] {
                assert!(!r.body.contains(secret), "{secret} in {} body: {}", r.path, r.body);
            }
        }
        let shown = format!("{:?} {:?}", run.events, run.result.as_ref().err().map(|e| e.to_string()));
        for secret in [TOKEN, SUP_TOKEN, "refresh-SECRET"] {
            assert!(!shown.contains(secret), "{secret} in events or error: {shown}");
        }
    }

    #[tokio::test]
    async fn several_projects_wait_for_a_choice_and_pair_with_one_approval() {
        let mut fake = two_projects();
        fake.options = VecDeque::from(["".into(), "".into(), "project-nope".into(), "project-nope".into(), PROJECT.into()]);
        let run = run_notified(fake, None, |_| {}).await;
        let paired = run.result.as_ref().expect("pairs once the project is set");
        assert_eq!(paired.project, PROJECT);
        assert_eq!(paired.organization, ORG);
        assert!(run.cfg.key_out.exists());

        {
            let fake = run.fake.lock().unwrap();
            assert_eq!(fake.codes_issued, 1, "no second approval");
            assert_eq!(token_requests(&fake), 1);
            assert!(fake.requests.iter().filter(|r| r.path == OPTIONS_PATH).count() >= 5);
            // Nothing was created while waiting, and only in the chosen project.
            assert_eq!(fake.sas.len(), 1);
        }
        assert_eq!(chooses(&run.events), [None, Some("project-nope".to_string())]);
        let choose_at = run.events.iter().position(|e| matches!(e, PairingEvent::ChooseProject { .. })).unwrap();
        let selected_at = run
            .events
            .iter()
            .position(|e| matches!(e, PairingEvent::ProjectSelected { .. }))
            .unwrap();
        assert!(choose_at < selected_at);
        let PairingEvent::ChooseProject { projects, wait, .. } = &run.events[choose_at] else { unreachable!() };
        assert_eq!(
            projects,
            &[
                ProjectChoice { id: PROJECT.into(), display_name: "Demo".into(), organization: ORG.into() },
                ProjectChoice { id: "project-abc".into(), display_name: "Garage".into(), organization: "other-org".into() },
            ]
        );
        assert!(*wait <= Duration::from_secs(10));

        // The notification: the code, the list, the list again naming the
        // wrong id, then gone. Always the same id, so each replaces the last.
        let notes = notifications(&run.fake.lock().unwrap());
        let services: Vec<&str> = notes.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(services, ["create", "create", "create", "dismiss"]);
        for (service, body) in &notes {
            assert_eq!(body["notification_id"], ha_supervisor::NOTIFICATION_ID);
            if service == "create" {
                assert_eq!(body["title"], "Datum Connect: connect to Datum");
            } else {
                assert_eq!(body, &json!({"notification_id": "datum_connect_pairing"}));
            }
        }
        let code = notes[0].1["message"].as_str().unwrap();
        assert_eq!(
            code,
            format!(
                "[Open the Datum approval page]({}/ui/v2/login/device?user_code=ABCD-EFG1) and confirm code **ABCD-EFG1**. The code expires in 5 minutes; a new one appears here if it does.",
                run.cfg.issuer
            )
        );
        let list = notes[1].1["message"].as_str().unwrap();
        assert!(list.contains("Set **project** on the add-on's Configuration tab to one of these ids and click Save. No restart needed; pairing continues automatically."), "{list}");
        assert!(list.contains("- `project-7r4rl`: Demo (organization datum-demos-iy50km)"), "{list}");
        assert!(list.contains("- `project-abc`: Garage (organization other-org)"), "{list}");
        assert!(!list.contains("isn't one of your projects"), "{list}");
        let again = notes[2].1["message"].as_str().unwrap();
        assert!(again.starts_with("'project-nope' isn't one of your projects."), "{again}");
        assert!(again.contains("`project-abc`"), "{again}");
        assert_no_secrets(&run);
    }

    #[tokio::test]
    async fn every_new_code_updates_the_notification() {
        let mut fake = Fake::new();
        fake.polls = VecDeque::from([Poll::Expired, Poll::Approve]);
        let run = run_notified(fake, Some(PROJECT), |_| {}).await;
        run.result.as_ref().expect("pairs");
        let notes = notifications(&run.fake.lock().unwrap());
        let messages: Vec<String> = notes.iter().map(|(s, b)| format!("{s} {}", b["message"].as_str().unwrap_or(""))).collect();
        assert_eq!(messages.len(), 3, "{messages:?}");
        assert!(messages[0].starts_with("create ") && messages[0].contains("**ABCD-EFG1**"), "{messages:?}");
        assert!(messages[1].starts_with("create ") && messages[1].contains("**ABCD-EFG2**"), "{messages:?}");
        assert_eq!(messages[2], "dismiss ");
        assert!(chooses(&run.events).is_empty(), "project was set and valid");
        assert!(!run.fake.lock().unwrap().requests.iter().any(|r| r.path == OPTIONS_PATH), "no options read");
    }

    #[tokio::test]
    async fn the_only_project_is_used_without_waiting() {
        let run = run_notified(Fake::new(), None, |_| {}).await;
        assert_eq!(run.result.as_ref().expect("pairs").project, PROJECT);
        assert!(chooses(&run.events).is_empty());
        assert!(!run.fake.lock().unwrap().requests.iter().any(|r| r.path == OPTIONS_PATH));
    }

    #[tokio::test]
    async fn a_configured_project_that_is_not_there_is_waited_past() {
        // One project visible, but another configured: wait, don't guess.
        let mut fake = Fake::new();
        fake.options = VecDeque::from(["project-nope".into(), "project-nope".into(), PROJECT.into()]);
        let run = run_notified(fake, Some("project-nope"), |_| {}).await;
        assert_eq!(run.result.as_ref().expect("pairs").project, PROJECT);
        assert_eq!(chooses(&run.events), [Some("project-nope".to_string())], "the stale value is not re-reported");
        let notes = notifications(&run.fake.lock().unwrap());
        assert!(notes[1].1["message"].as_str().unwrap().starts_with("'project-nope' isn't one of your projects."));
    }

    #[tokio::test]
    async fn a_failed_options_read_is_retried() {
        let mut fake = two_projects();
        fake.options = VecDeque::from(["!500".into(), "!500".into(), PROJECT.into()]);
        let run = run_notified(fake, None, |_| {}).await;
        assert_eq!(run.result.as_ref().expect("pairs").project, PROJECT);
        assert_eq!(chooses(&run.events), [None]);
    }

    #[tokio::test]
    async fn the_wait_stops_before_the_token_runs_out() {
        let mut fake = two_projects();
        // 125 ticks of 10ms: the token is good for 1.25s, and waiting stops
        // 120 ticks (1.2s) before that.
        fake.token_expires_in = 125;
        let started = Instant::now();
        let run = run_notified(fake, None, |c| c.tick = Duration::from_millis(10)).await;
        let err = run.result.as_ref().unwrap_err();
        assert!(matches!(err, PairingError::ProjectNotChosen { .. }), "{err}");
        assert!(err.to_string().starts_with("Choosing a project took too long: none of yours was set within "), "{err}");
        assert!(err.to_string().ends_with(" seconds, and the approval is no longer good. Restart to try again."), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert!(run.fake.lock().unwrap().sas.is_empty(), "nothing created");
        assert!(!run.cfg.key_out.exists());

        let notes = notifications(&run.fake.lock().unwrap());
        let (service, last) = notes.last().unwrap();
        assert_eq!(service, "create", "a failure stays up, with the reason");
        let message = last["message"].as_str().unwrap();
        assert!(message.starts_with("Pairing with Datum failed: Choosing a project took too long"), "{message}");
        assert!(message.ends_with("Restart to try again."), "{message}");
        assert_eq!(message.matches("estart").count(), 1, "restart advice once: {message}");
        assert_no_secrets(&run);
    }

    #[tokio::test]
    async fn the_wait_stops_at_its_maximum() {
        let run = run_notified(two_projects(), None, |c| {
            c.project_wait.as_mut().unwrap().max = Duration::from_millis(100);
        })
        .await;
        assert!(matches!(run.result, Err(PairingError::ProjectNotChosen { .. })));
        let reads = run.fake.lock().unwrap().requests.iter().filter(|r| r.path == OPTIONS_PATH).count();
        assert!(reads >= 2, "polled while waiting: {reads}");
    }

    #[test]
    fn the_wait_bound_falls_back_to_a_jwt_exp() {
        let exp = chrono::Utc::now().timestamp() + 600;
        let jwt = format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(json!({"exp": exp}).to_string()));
        let left = jwt_lifetime_secs(&jwt).unwrap();
        assert!((598..=600).contains(&left), "{left}");
        assert_eq!(jwt_lifetime_secs("opaque-token"), None);
        let past = format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(json!({"exp": 1}).to_string()));
        assert_eq!(jwt_lifetime_secs(&past), None);
    }

    #[tokio::test]
    async fn failing_notifications_never_fail_pairing() {
        for status in [401, 500] {
            let mut fake = two_projects();
            fake.notify_status = status;
            fake.options = VecDeque::from(["".into(), PROJECT.into()]);
            let run = run_notified(fake, None, |_| {}).await;
            assert_eq!(run.result.as_ref().expect("pairs regardless").project, PROJECT, "HTTP {status}");
            assert_eq!(notifications(&run.fake.lock().unwrap()).len(), 3, "each still tried once");
        }
    }

    #[tokio::test]
    async fn a_supervisor_that_is_not_there_never_fails_pairing() {
        let (base, fake) = serve({
            let mut f = Fake::new();
            f.key_json = good_key();
            f
        })
        .await;
        let dir = temp_dir();
        let cfg = config(&base, &dir.0, None);
        // Nothing listens on a port just freed.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        let notifier = PairingNotifier::spawn(supervisor(&closed));
        let mut events = Vec::new();
        let result = pair(&cfg, &mut |e| {
            notifier.event(&e);
            events.push(e);
        })
        .await;
        notifier.outcome(&result);
        notifier.finish().await;
        result.expect("pairs");
        drop((fake, dir));
    }

    #[tokio::test]
    async fn without_a_wait_several_projects_still_stop_and_nothing_is_asked_of_the_supervisor() {
        let run = run_with(two_projects(), None, |_| {}).await;
        assert!(matches!(run.result, Err(PairingError::Project(_))));
        let fake = run.fake.lock().unwrap();
        assert!(
            !fake.requests.iter().any(|r| r.path.starts_with(NOTIFY_PREFIX) || r.path == OPTIONS_PATH),
            "log-only"
        );
    }

    #[tokio::test]
    async fn supervisor_errors_never_quote_its_token() {
        let mut fake = Fake::new();
        fake.echo_auth_on = Some("/persistent_notification/create".into());
        let (base, _fake) = serve(fake).await;
        let err = supervisor(&base).notify("hi").await.unwrap_err();
        assert!(err.contains("[redacted]") && !err.contains(SUP_TOKEN), "{err}");
    }

    #[tokio::test]
    async fn the_saved_project_is_read_from_the_supervisor() {
        let mut fake = Fake::new();
        fake.options = VecDeque::from(["".into(), "  p-1 ".into()]);
        let (base, fake) = serve(fake).await;
        let sup = supervisor(&base);
        assert_eq!(sup.saved_project().await.unwrap(), None, "empty is unset");
        assert_eq!(sup.saved_project().await.unwrap().as_deref(), Some("p-1"));
        let fake = fake.lock().unwrap();
        let read = fake.requests.iter().find(|r| r.path == OPTIONS_PATH).unwrap();
        assert_eq!(read.method, "GET");
        assert_eq!(read.authorization.as_deref(), Some(format!("Bearer {SUP_TOKEN}").as_str()));
        drop(fake);
        let wrong = Supervisor::new(base, SecretString::from("wrong")).unwrap();
        assert!(wrong.saved_project().await.unwrap_err().contains("HTTP 401"));
    }

    #[test]
    fn redact_scrubs_every_occurrence() {
        let token = SecretString::from("abc123");
        assert_eq!(redact("x abc123 y abc123".into(), &token), "x [redacted] y [redacted]");
        assert_eq!(format!("{:?}", token).contains("abc123"), false);
    }

    #[test]
    fn staging_issuer_gets_the_staging_client() {
        assert_eq!(default_client_id("https://auth.datum.net"), PROD_CLIENT_ID);
        assert_eq!(default_client_id("https://auth.staging.env.datum.net"), STAGING_CLIENT_ID);
    }

    #[test]
    fn from_env_honours_the_overrides() {
        let _lock = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let vars = ["DATUM_AUTH_ISSUER", "DATUM_API_HOST", "DATUM_API_ENV", CLIENT_ID_ENV, SCOPE_ENV];
        let clear = || {
            for v in vars {
                unsafe { std::env::remove_var(v) };
            }
        };
        clear();
        let cfg = PairingConfig::from_env(Some("".into()), "k.json".into());
        assert_eq!(cfg.issuer, "https://auth.datum.net");
        assert_eq!(cfg.api_url, "https://api.datum.net");
        assert_eq!(cfg.client_id, PROD_CLIENT_ID);
        assert_eq!(cfg.scope, DEFAULT_SCOPE);
        assert_eq!(cfg.project, None, "empty means unset");
        unsafe {
            std::env::set_var("DATUM_AUTH_ISSUER", "https://auth.staging.env.datum.net/");
            std::env::set_var("DATUM_API_HOST", "api.staging.env.datum.net");
            std::env::set_var(SCOPE_ENV, "openid profile email");
        }
        let cfg = PairingConfig::from_env(Some("p".into()), "k.json".into());
        assert_eq!(cfg.issuer, "https://auth.staging.env.datum.net");
        assert_eq!(cfg.api_url, "https://api.staging.env.datum.net");
        assert_eq!(cfg.client_id, STAGING_CLIENT_ID);
        assert_eq!(cfg.scope, "openid profile email");
        unsafe { std::env::set_var(CLIENT_ID_ENV, "42") };
        assert_eq!(PairingConfig::from_env(None, "k.json".into()).client_id, "42");
        clear();
    }
}
