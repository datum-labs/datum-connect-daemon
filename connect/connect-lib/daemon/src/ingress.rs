//! The Home Assistant add-on's page, served through ingress: Home
//! Assistant's sidebar panel ("Datum Connect"), opened at `/app/<slug>`.
//!
//! One server, two backends. While the add-on has no key, `setup` serves
//! it with a [`SetupController`]: the "Connect to Datum" button, the code,
//! the project list, progress. Once paired, the daemon serves it with
//! [`PairedView`]: the public address, the project, the service account,
//! the tunnel and its edge policies, older tunnels from this Home Assistant
//! with Remove, Home Assistant's trusted-proxy step with Allow, and Re-pair
//! and Unpair. The page
//! itself (`daemon/ingress/`) is the same, and reloads when the mode
//! changes under it.
//!
//! **Who can reach it.** The add-on runs with host networking, so a socket
//! bound to every address would be on the LAN, past Home Assistant's login.
//! It binds only to the address the Supervisor's ingress proxy connects to
//! (the `hassio` bridge's gateway, see
//! [`connect_lib::datum_cloud::ha_supervisor`]), which is not on the LAN.
//! Every add-on on that bridge can reach it there too, so every request
//! must also come from an allowed peer, the Supervisor's own address, or it
//! is refused before any handler runs. Ingress itself has already checked
//! the person's Home Assistant session and admin rights (`panel_admin`
//! defaults to true) by then.
//!
//! **CSRF.** Only the ingress proxy can connect, but POSTs still need the
//! token the page was served with, in a header no cross-site form can set,
//! and, when the browser sends one, an `Origin` matching the host Home
//! Assistant was reached at. Cheap, and it holds if the peer check is ever
//! loosened.
//!
//! **No secrets in the page.** The setup state is [`SetupStatus`], which
//! never holds the person's token; the paired state is built here from
//! public facts. Everything is relative to the ingress path, which the page
//! learns from `X-Ingress-Path` (set by Home Assistant Core's ingress view)
//! through a `<base>`, validated before use.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use connect_lib::datum_cloud::ha_supervisor::Supervisor;
use connect_lib::datum_cloud::pairing_setup::{ChooseError, RestartError, SetupController, SetupStatus};
use serde::{Deserialize, Serialize};
use serde_json::json;

const INDEX_HTML: &str = include_str!("../ingress/index.html");
const APP_JS: &str = include_str!("../ingress/app.js");
const APP_CSS: &str = include_str!("../ingress/app.css");

/// The header the page sends its CSRF token in.
pub(crate) const CSRF_HEADER: &str = "x-datum-connect-csrf";

/// `X-Ingress-Path`, as Home Assistant Core sets it
/// (`homeassistant/components/hassio/ingress.py`).
const INGRESS_PATH_HEADER: &str = "x-ingress-path";

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The paired side of the page, as the daemon provides it.
pub(crate) trait PairedView: Send + Sync {
    fn status(&self) -> BoxFuture<'_, PairedStatus>;
    /// Forgets the paired key and restarts the add-on; `unpair` also stops
    /// the tunnel first.
    fn forget(&self, unpair: bool) -> BoxFuture<'_, Result<ForgetOutcome, ActionError>>;
    /// Removes an older tunnel from this Home Assistant, in Datum and here.
    /// Refuses any other tunnel.
    fn remove(&self, id: String) -> BoxFuture<'_, Result<RemoveOutcome, ActionError>>;
    /// Starts letting Home Assistant accept connections through Datum (see
    /// `connect_lib::datum_cloud::ha_core`). Returns once started; the
    /// status shows how it goes.
    fn allow_proxies(&self) -> BoxFuture<'_, Result<(), ActionError>>;
    /// Ends setup's trusted-proxy step without changing Home Assistant:
    /// the person sets it up in Settings → System → Network instead.
    fn skip_proxies(&self) -> BoxFuture<'_, Result<(), ActionError>>;
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub(crate) struct PairedStatus {
    pub project: String,
    pub service_account: Option<String>,
    /// `paired` (made by pairing, so Re-pair and Unpair work) or
    /// `provided` (pasted or placed by hand).
    pub key_source: &'static str,
    /// The add-on's tunnel (with several only outside the add-on, where no
    /// tunnel is singled out).
    pub tunnels: Vec<TunnelView>,
    /// Other tunnels this Home Assistant has local state for: stopped, and
    /// offered for removal.
    pub older: Vec<TunnelView>,
    /// Whether Home Assistant accepts connections through Datum. `None`
    /// without a Supervisor to ask.
    pub trusted_proxies: Option<ProxyStepView>,
    pub can_forget: bool,
    /// Why Re-pair and Unpair are off, when they are.
    pub why_not: Option<String>,
    /// Datum could not be asked about the tunnels.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct TunnelView {
    pub id: String,
    pub label: String,
    /// `https://<hostname>`, once there is one.
    pub address: Option<String>,
    /// `online`, `starting`, `offline` or `off`.
    pub state: &'static str,
    pub edge: Option<connect_lib::edge_policies::EdgePolicyStatus>,
    pub portal_url: Option<String>,
    /// Created less than half an hour ago: its address may not work in
    /// every browser yet.
    pub new_address: bool,
}

/// Home Assistant's trusted-proxy step, as the page shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ProxyStepView {
    /// `ok`, `needed` (Allow), `pending` (someone else's change waits),
    /// `working` (Allow is running), `failed` (the last Allow did not
    /// finish; Allow again), or `unknown` (Home Assistant could not be
    /// asked).
    pub state: &'static str,
    pub message: Option<String>,
    /// Shown as the last step of setting up the add-on (with Allow and
    /// Skip), rather than as a row of the tunnel's status: until it is
    /// allowed, skipped or found already set up, and right after an Allow
    /// that worked, for its ✓.
    pub setup: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct RemoveOutcome {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ForgetOutcome {
    pub restarting: bool,
    /// The service account the forgotten key belonged to, which still
    /// exists in Datum.
    pub service_account: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ActionError {
    pub status: StatusCode,
    pub message: String,
}

#[derive(Clone)]
pub(crate) enum Backend {
    Setup(SetupController),
    Paired(Arc<dyn PairedView>),
}

impl Backend {
    fn mode(&self) -> &'static str {
        match self {
            Backend::Setup(_) => "setup",
            Backend::Paired(_) => "paired",
        }
    }
}

struct Ingress {
    backend: Backend,
    csrf: String,
    allowed: Vec<IpAddr>,
}

/// The page's router. Serve it with
/// `into_make_service_with_connect_info::<SocketAddr>()`: the peer check
/// needs the connection's address.
pub(crate) fn router(backend: Backend, allowed: Vec<IpAddr>) -> Router {
    let state = Arc::new(Ingress {
        backend,
        csrf: new_csrf_token(),
        allowed: allowed.into_iter().map(|ip| ip.to_canonical()).collect(),
    });
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/app.css", get(app_css))
        .route("/api/state", get(get_state))
        .route("/api/connect", post(connect))
        .route("/api/new-code", post(new_code))
        .route("/api/project", post(choose_project))
        .route("/api/repair", post(repair))
        .route("/api/unpair", post(unpair))
        .route("/api/remove-tunnel", post(remove_tunnel))
        .route("/api/allow-proxies", post(allow_proxies))
        .route("/api/skip-proxies", post(skip_proxies))
        .layer(middleware::from_fn_with_state(state.clone(), csrf_check))
        .layer(middleware::from_fn_with_state(state.clone(), peer_check))
        .layer(middleware::map_response(harden))
        .with_state(state)
}

/// Binds the page's socket. Refuses an unspecified address: with host
/// networking that is every address, the LAN included.
pub(crate) async fn bind(ip: IpAddr, port: u16) -> std::io::Result<tokio::net::TcpListener> {
    if ip.is_unspecified() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("refusing to serve the page on {ip}, which with host networking is every address, the LAN included"),
        ));
    }
    tokio::net::TcpListener::bind(SocketAddr::new(ip, port)).await
}

/// Serves `router` on `listener`, with each connection's address for the
/// peer check, until the task is aborted.
pub(crate) fn serve_on(listener: tokio::net::TcpListener, router: Router) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await {
            tracing::warn!("the Datum Connect page stopped serving: {e}");
        }
    })
}

/// Where to bind and whom to serve, from `--ingress-bind` and
/// `--ingress-allow`. `auto` asks the Supervisor (falling back to its
/// constants), or, outside an add-on, means loopback.
pub(crate) async fn resolve_addresses(
    bind: &str,
    allow: &str,
    supervisor: Option<&Supervisor>,
) -> Result<(IpAddr, Vec<IpAddr>), String> {
    let auto = match (bind.trim() == "auto" || allow.trim() == "auto", supervisor) {
        (true, Some(s)) => Some(s.ingress_addresses().await),
        _ => None,
    };
    let loopback = IpAddr::from([127, 0, 0, 1]);
    let bind_ip = match bind.trim() {
        "auto" => auto.map(|a| a.bind).unwrap_or(loopback),
        ip => ip.parse().map_err(|_| format!("--ingress-bind {ip:?} is not an IP address or auto"))?,
    };
    let allowed = match allow.trim() {
        "auto" => vec![auto.map(|a| a.proxy).unwrap_or(loopback)],
        list => list
            .split(',')
            .map(|ip| ip.trim().parse::<IpAddr>().map_err(|_| format!("--ingress-allow {ip:?} is not an IP address")))
            .collect::<Result<Vec<_>, _>>()?,
    };
    if bind_ip.is_unspecified() {
        return Err(format!(
            "--ingress-bind {bind_ip} would put the page on every address, the LAN included; use the address the Supervisor connects to, or auto"
        ));
    }
    Ok((bind_ip, allowed))
}

fn new_csrf_token() -> String {
    use rand::Rng;
    let bytes: [u8; 32] = rand::rng().random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---- Middleware ----

/// Refuses every request not from an allowed peer (see the module docs).
async fn peer_check(State(state): State<Arc<Ingress>>, request: Request, next: Next) -> Response {
    let peer = request.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip().to_canonical());
    match peer {
        Some(ip) if state.allowed.contains(&ip) => next.run(request).await,
        other => {
            let who = other.map(|ip| ip.to_string()).unwrap_or_else(|| "an unknown address".into());
            tracing::warn!(
                "Refused a request to the Datum Connect page from {who}: only the Home Assistant Supervisor's ingress proxy ({}) may connect. If that is the Supervisor's address on this system, set DATUM_INGRESS_ALLOW.",
                state.allowed.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
            );
            (StatusCode::FORBIDDEN, Json(json!({"error": "forbidden", "message": "Not allowed."}))).into_response()
        }
    }
}

/// POSTs need the page's token and, if the browser sent one, a same-origin
/// `Origin` (see the module docs).
async fn csrf_check(State(state): State<Arc<Ingress>>, request: Request, next: Next) -> Response {
    if request.method() != Method::POST {
        return next.run(request).await;
    }
    let headers = request.headers();
    let token_ok = headers
        .get(CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| constant_time_eq(t.as_bytes(), state.csrf.as_bytes()));
    if !token_ok || !same_origin(headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "csrf", "message": "This page is out of date. Reload it and try again."})),
        )
            .into_response();
    }
    next.run(request).await
}

/// No `Origin` (an older browser, or a non-browser client through the
/// proxy) passes on the token alone. One that is sent must name the host
/// Home Assistant was reached at, which Core passes on as
/// `X-Forwarded-Host`, or the `Host` the browser sent.
fn same_origin(headers: &HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok())
        && site != "same-origin"
    {
        return false;
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Some(origin_host) = origin
        .to_str()
        .ok()
        .and_then(|o| o.split_once("://"))
        .map(|(_, rest)| rest.trim_end_matches('/').to_ascii_lowercase())
    else {
        return false;
    };
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .map(|h| h.split(',').next().unwrap_or_default().trim().to_ascii_lowercase());
    host.is_some_and(|h| h == origin_host)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Headers every response gets: nothing cached, nothing sniffed, nothing
/// referred, and the page only framed by Home Assistant itself.
async fn harden(mut response: Response) -> Response {
    let h = response.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'self'; form-action 'none'; frame-ancestors 'self'",
        ),
    );
    response
}

// ---- Handlers ----

/// The page, with this server's CSRF token, the mode, and a `<base>` for
/// the ingress path when Home Assistant sent a valid one. Without it the
/// relative URLs still resolve against the page's own URL, which ends in a
/// slash under ingress.
async fn index(State(state): State<Arc<Ingress>>, headers: HeaderMap) -> Response {
    let base = headers
        .get(INGRESS_PATH_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(valid_ingress_path)
        .map(|p| format!("<base href=\"{p}/\">\n"))
        .unwrap_or_default();
    let html = INDEX_HTML
        .replace("{{CSRF}}", &state.csrf)
        .replace("{{MODE}}", state.backend.mode())
        .replace("{{BASE}}", &base);
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

/// `/api/hassio_ingress/<token>`, as Core builds it, and nothing else, so
/// that a header cannot inject markup or point the page elsewhere.
fn valid_ingress_path(path: &str) -> Option<&str> {
    let token = path.strip_prefix("/api/hassio_ingress/")?;
    let ok = !token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    ok.then_some(path)
}

async fn app_js() -> Response {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], APP_JS).into_response()
}

async fn app_css() -> Response {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS).into_response()
}

#[derive(Serialize)]
struct StateBody {
    mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    setup: Option<SetupStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    paired: Option<PairedStatus>,
}

async fn state_body(state: &Ingress) -> StateBody {
    match &state.backend {
        Backend::Setup(ctl) => StateBody { mode: "setup", setup: Some(ctl.status()), paired: None },
        Backend::Paired(view) => StateBody { mode: "paired", setup: None, paired: Some(view.status().await) },
    }
}

async fn get_state(State(state): State<Arc<Ingress>>) -> Json<StateBody> {
    Json(state_body(&state).await)
}

fn wrong_mode(state: &Ingress) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({"error": "mode", "message": format!("Not available while the add-on is in {} mode. Reload the page.", state.backend.mode())})),
    )
        .into_response()
}

async fn connect(State(state): State<Arc<Ingress>>) -> Response {
    let Backend::Setup(ctl) = &state.backend else {
        return wrong_mode(&state);
    };
    ctl.start();
    Json(state_body(&state).await).into_response()
}

/// "Get a new code": drops the code on the page and asks for another, for
/// when Datum's approval page failed on it. Refused once approved.
async fn new_code(State(state): State<Arc<Ingress>>) -> Response {
    let Backend::Setup(ctl) = &state.backend else {
        return wrong_mode(&state);
    };
    match ctl.restart() {
        Ok(()) => Json(state_body(&state).await).into_response(),
        Err(e) => {
            let status = match e {
                RestartError::Approved | RestartError::Done => StatusCode::CONFLICT,
            };
            (status, Json(json!({"error": "new_code", "message": e.to_string()}))).into_response()
        }
    }
}

#[derive(Deserialize)]
struct ChooseBody {
    project: String,
}

async fn choose_project(State(state): State<Arc<Ingress>>, Json(body): Json<ChooseBody>) -> Response {
    let Backend::Setup(ctl) = &state.backend else {
        return wrong_mode(&state);
    };
    match ctl.choose(&body.project) {
        Ok(()) => Json(state_body(&state).await).into_response(),
        Err(e) => {
            let status = match e {
                ChooseError::NotChoosing => StatusCode::CONFLICT,
                ChooseError::Unknown => StatusCode::BAD_REQUEST,
            };
            (status, Json(json!({"error": "project", "message": e.to_string()}))).into_response()
        }
    }
}

async fn repair(State(state): State<Arc<Ingress>>) -> Response {
    forget(&state, false).await
}

async fn unpair(State(state): State<Arc<Ingress>>) -> Response {
    forget(&state, true).await
}

#[derive(Deserialize)]
struct RemoveBody {
    id: String,
}

async fn remove_tunnel(State(state): State<Arc<Ingress>>, Json(body): Json<RemoveBody>) -> Response {
    let Backend::Paired(view) = &state.backend else {
        return wrong_mode(&state);
    };
    match view.remove(body.id).await {
        Ok(outcome) => Json(outcome).into_response(),
        Err(e) => (e.status, Json(json!({"error": "action", "message": e.message}))).into_response(),
    }
}

async fn allow_proxies(State(state): State<Arc<Ingress>>) -> Response {
    let Backend::Paired(view) = &state.backend else {
        return wrong_mode(&state);
    };
    match view.allow_proxies().await {
        Ok(()) => Json(state_body(&state).await).into_response(),
        Err(e) => (e.status, Json(json!({"error": "action", "message": e.message}))).into_response(),
    }
}

async fn skip_proxies(State(state): State<Arc<Ingress>>) -> Response {
    let Backend::Paired(view) = &state.backend else {
        return wrong_mode(&state);
    };
    match view.skip_proxies().await {
        Ok(()) => Json(state_body(&state).await).into_response(),
        Err(e) => (e.status, Json(json!({"error": "action", "message": e.message}))).into_response(),
    }
}

async fn forget(state: &Ingress, unpair: bool) -> Response {
    let Backend::Paired(view) = &state.backend else {
        return wrong_mode(state);
    };
    match view.forget(unpair).await {
        Ok(outcome) => Json(outcome).into_response(),
        Err(e) => (e.status, Json(json!({"error": "action", "message": e.message}))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use connect_lib::datum_cloud::pairing::PairingConfig;
    use tower::ServiceExt;

    const SUPERVISOR: &str = "172.30.32.2";

    struct FakePaired {
        forgets: std::sync::Mutex<Vec<bool>>,
        removes: std::sync::Mutex<Vec<String>>,
        allows: std::sync::Mutex<u32>,
        skips: std::sync::Mutex<u32>,
    }

    impl PairedView for FakePaired {
        fn status(&self) -> BoxFuture<'_, PairedStatus> {
            Box::pin(async {
                PairedStatus {
                    project: "project-7r4rl".into(),
                    service_account: Some("home-assistant-abcde@project-7r4rl.identity.datumapis.com".into()),
                    key_source: "paired",
                    tunnels: vec![TunnelView {
                        id: "t1".into(),
                        label: "home-assistant".into(),
                        address: Some("https://abc.datumproxy.net".into()),
                        state: "online",
                        edge: None,
                        portal_url: None,
                        new_address: false,
                    }],
                    older: vec![TunnelView {
                        id: "t0".into(),
                        label: "ha-old".into(),
                        address: Some("https://old.datumproxy.net".into()),
                        state: "off",
                        edge: None,
                        portal_url: None,
                        new_address: false,
                    }],
                    trusted_proxies: Some(ProxyStepView { state: "needed", message: None, setup: true }),
                    can_forget: true,
                    why_not: None,
                    error: None,
                }
            })
        }
        fn forget(&self, unpair: bool) -> BoxFuture<'_, Result<ForgetOutcome, ActionError>> {
            self.forgets.lock().unwrap().push(unpair);
            Box::pin(async { Ok(ForgetOutcome { restarting: true, service_account: Some("sa@x".into()) }) })
        }
        fn remove(&self, id: String) -> BoxFuture<'_, Result<RemoveOutcome, ActionError>> {
            self.removes.lock().unwrap().push(id.clone());
            Box::pin(async move {
                if id == "t0" {
                    Ok(RemoveOutcome { id, label: "ha-old".into() })
                } else {
                    Err(ActionError { status: StatusCode::CONFLICT, message: "not an older tunnel".into() })
                }
            })
        }
        fn allow_proxies(&self) -> BoxFuture<'_, Result<(), ActionError>> {
            *self.allows.lock().unwrap() += 1;
            Box::pin(async { Ok(()) })
        }
        fn skip_proxies(&self) -> BoxFuture<'_, Result<(), ActionError>> {
            *self.skips.lock().unwrap() += 1;
            Box::pin(async { Ok(()) })
        }
    }

    fn setup_backend() -> Backend {
        // Never started in these tests, so nothing is ever dialled.
        let dir = std::env::temp_dir().join("ingress-test-unused");
        let cfg = PairingConfig::from_env(Some("project-7r4rl".into()), dir.join("k.json"));
        Backend::Setup(SetupController::new(cfg, None, None))
    }

    fn paired_backend() -> (Backend, Arc<FakePaired>) {
        let fake = Arc::new(FakePaired {
            forgets: Default::default(),
            removes: Default::default(),
            allows: Default::default(),
            skips: Default::default(),
        });
        (Backend::Paired(fake.clone()), fake)
    }

    fn app(backend: Backend) -> Router {
        router(backend, vec![SUPERVISOR.parse().unwrap()])
    }

    fn request(method: Method, path: &str, from: &str) -> axum::http::request::Builder {
        let addr: SocketAddr = SocketAddr::new(from.parse().unwrap(), 50000);
        Request::builder()
            .method(method)
            .uri(path)
            .extension(ConnectInfo(addr))
    }

    async fn send(app: &Router, req: Request) -> (StatusCode, HeaderMap, String) {
        let response = app.clone().oneshot(req).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, headers, String::from_utf8(body.to_vec()).unwrap())
    }

    /// The page and its token, as the browser gets them through ingress.
    async fn page(app: &Router) -> (String, String) {
        let (status, _, html) = send(
            app,
            request(Method::GET, "/", SUPERVISOR)
                .header(INGRESS_PATH_HEADER, "/api/hassio_ingress/AbC-12_x")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let token = html.split("name=\"datum-csrf\" content=\"").nth(1).unwrap().split('"').next().unwrap().to_string();
        (html, token)
    }

    fn post_json(path: &str, token: Option<&str>) -> axum::http::request::Builder {
        let mut b = request(Method::POST, path, SUPERVISOR)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::HOST, "homeassistant.local:8123");
        if let Some(t) = token {
            b = b.header(CSRF_HEADER, t);
        }
        b
    }

    #[tokio::test]
    async fn only_the_supervisor_may_connect() {
        let app = app(setup_backend());
        for from in ["172.30.33.5", "192.168.1.20", "127.0.0.1", "172.30.32.1"] {
            for path in ["/", "/api/state", "/app.js"] {
                let (status, _, body) = send(&app, request(Method::GET, path, from).body(Body::empty()).unwrap()).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{from} {path}");
                assert!(!body.contains("datum-csrf"), "nothing of the page leaks to {from}");
            }
        }
        // An IPv4-mapped IPv6 peer is the same address.
        let (status, _, _) = send(&app, request(Method::GET, "/api/state", "::ffff:172.30.32.2").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        // No connection info at all (not served the way `serve_on` does) is refused.
        let bare = Request::builder().uri("/api/state").body(Body::empty()).unwrap();
        assert_eq!(send(&app, bare).await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn the_page_uses_the_ingress_path_and_relative_urls() {
        let app = app(setup_backend());
        let (html, token) = page(&app).await;
        assert!(html.contains("<base href=\"/api/hassio_ingress/AbC-12_x/\">"), "{html}");
        assert_eq!(token.len(), 64);
        assert!(html.contains("data-mode=\"setup\""));
        for asset in [INDEX_HTML, APP_JS, APP_CSS] {
            for absolute in ["src=\"/", "href=\"/", "fetch(\"/", "post(\"/", "url(/", "http://", "https://"] {
                assert!(!asset.contains(absolute), "absolute URL {absolute:?} in a page asset");
            }
        }
        // A bad header gets no <base>, so it cannot inject markup or send
        // the page's requests elsewhere.
        for bad in ["/api/hassio_ingress/x\"><script>", "//evil.example/api/hassio_ingress/x", "/elsewhere", "/api/hassio_ingress/"] {
            let (_, _, html) = send(
                &app,
                request(Method::GET, "/", SUPERVISOR).header(INGRESS_PATH_HEADER, bad).body(Body::empty()).unwrap(),
            )
            .await;
            assert!(!html.contains("<base"), "{bad}: {html}");
        }
        let (_, headers, _) = send(&app, request(Method::GET, "/", SUPERVISOR).body(Body::empty()).unwrap()).await;
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert!(headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap().contains("script-src 'self'"));
        let (status, headers, js) = send(&app, request(Method::GET, "/app.js", SUPERVISOR).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/javascript"));
        assert!(js.contains("api/state"));
    }

    #[tokio::test]
    async fn posts_need_the_page_token_and_a_matching_origin() {
        let app = app(setup_backend());
        let (_, token) = page(&app).await;

        let cases: Vec<(&str, axum::http::request::Builder)> = vec![
            ("no token", post_json("/api/project", None)),
            ("wrong token", post_json("/api/project", Some(&"0".repeat(64)))),
            ("short token", post_json("/api/project", Some(&token[..10]))),
            ("cross-site origin", post_json("/api/project", Some(&token)).header(header::ORIGIN, "https://evil.example")),
            ("null origin", post_json("/api/project", Some(&token)).header(header::ORIGIN, "null")),
            ("cross-site fetch", post_json("/api/project", Some(&token)).header("sec-fetch-site", "cross-site")),
        ];
        for (what, builder) in cases {
            let (status, _, body) = send(&app, builder.body(Body::from(r#"{"project":"p"}"#)).unwrap()).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{what}");
            assert!(body.contains("\"csrf\""), "{what}: {body}");
        }

        // Same origin, through a proxy that names the original host.
        let ok = post_json("/api/project", Some(&token))
            .header(header::ORIGIN, "https://ha.example.com")
            .header("x-forwarded-host", "ha.example.com")
            .header("sec-fetch-site", "same-origin");
        let (status, _, body) = send(&app, ok.body(Body::from(r#"{"project":"project-7r4rl"}"#)).unwrap()).await;
        // Past CSRF; refused only because nothing is waiting for a project.
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let ok = post_json("/api/project", Some(&token)).header(header::ORIGIN, "http://homeassistant.local:8123");
        assert_eq!(send(&app, ok.body(Body::from(r#"{"project":"x"}"#)).unwrap()).await.0, StatusCode::CONFLICT);

        // GETs need no token.
        let (status, _, _) = send(&app, request(Method::GET, "/api/state", SUPERVISOR).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn setup_state_is_served_and_paired_actions_are_not() {
        let app = app(setup_backend());
        let (_, token) = page(&app).await;
        let (status, _, body) = send(&app, request(Method::GET, "/api/state", SUPERVISOR).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["mode"], "setup");
        assert_eq!(v["setup"]["phase"], "idle");
        assert_eq!(v["setup"]["suggested"], "project-7r4rl");
        assert!(v.get("paired").is_none());
        let (status, _, _) = send(&app, post_json("/api/unpair", Some(&token)).body(Body::from("{}")).unwrap()).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn new_code_needs_the_supervisor_and_the_page_token() {
        // A run that does start dials only a closed loopback port, never
        // Datum, and fails there.
        let dir = std::env::temp_dir().join("ingress-test-new-code-unused");
        let mut cfg = PairingConfig::from_env(None, dir.join("k.json"));
        cfg.issuer = "http://127.0.0.1:9".into();
        cfg.api_url = "http://127.0.0.1:9".into();
        let app = app(Backend::Setup(SetupController::new(cfg, None, None)));
        let (_, token) = page(&app).await;

        for from in ["172.30.33.5", "192.168.1.20", "127.0.0.1"] {
            let req = request(Method::POST, "/api/new-code", from)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::HOST, "homeassistant.local:8123")
                .header(CSRF_HEADER, &token);
            let (status, _, body) = send(&app, req.body(Body::from("{}")).unwrap()).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{from}");
            assert!(body.contains("\"forbidden\""), "{from}: {body}");
        }
        let refused: Vec<(&str, axum::http::request::Builder)> = vec![
            ("no token", post_json("/api/new-code", None)),
            ("wrong token", post_json("/api/new-code", Some(&"0".repeat(64)))),
            ("cross-site origin", post_json("/api/new-code", Some(&token)).header(header::ORIGIN, "https://evil.example")),
            ("cross-site fetch", post_json("/api/new-code", Some(&token)).header("sec-fetch-site", "cross-site")),
        ];
        for (what, builder) in refused {
            let (status, _, body) = send(&app, builder.body(Body::from("{}")).unwrap()).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{what}");
            assert!(body.contains("\"csrf\""), "{what}: {body}");
        }
        let (_, _, body) = send(&app, request(Method::GET, "/api/state", SUPERVISOR).body(Body::empty()).unwrap()).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["setup"]["phase"], "idle", "nothing started by a refused request");

        // With the token it goes through, and answers with the new state.
        let ok = post_json("/api/new-code", Some(&token)).header(header::ORIGIN, "http://homeassistant.local:8123");
        let (status, _, body) = send(&app, ok.body(Body::from("{}")).unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["mode"], "setup");
        assert!(matches!(v["setup"]["phase"].as_str(), Some("starting" | "failed")), "{body}");

        // Not a paired action.
        let (backend, _) = paired_backend();
        let paired = self::app(backend);
        let (_, token) = page(&paired).await;
        let (status, _, _) = send(&paired, post_json("/api/new-code", Some(&token)).body(Body::from("{}")).unwrap()).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn paired_status_and_actions() {
        let (backend, fake) = paired_backend();
        let app = app(backend);
        let (html, token) = page(&app).await;
        assert!(html.contains("data-mode=\"paired\""));
        let (status, _, body) = send(&app, request(Method::GET, "/api/state", SUPERVISOR).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["mode"], "paired");
        assert_eq!(v["paired"]["tunnels"][0]["address"], "https://abc.datumproxy.net");
        assert_eq!(v["paired"]["tunnels"][0]["state"], "online");
        assert_eq!(v["paired"]["key_source"], "paired");

        let (status, _, _) = send(&app, post_json("/api/connect", Some(&token)).body(Body::from("{}")).unwrap()).await;
        assert_eq!(status, StatusCode::CONFLICT, "no pairing while paired");
        let (status, _, _) = send(&app, post_json("/api/unpair", None).body(Body::from("{}")).unwrap()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(fake.forgets.lock().unwrap().is_empty(), "refused before acting");
        let (status, _, body) = send(&app, post_json("/api/repair", Some(&token)).body(Body::from("{}")).unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, _, _) = send(&app, post_json("/api/unpair", Some(&token)).body(Body::from("{}")).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(*fake.forgets.lock().unwrap(), [false, true]);
    }

    /// Remove, Allow and Skip change things in Datum, in Home Assistant or
    /// in what setup offers, so they get the same gate as every other POST:
    /// only the Supervisor, and only with the page's token and a
    /// same-origin request.
    #[tokio::test]
    async fn remove_allow_and_skip_need_the_supervisor_and_the_page_token() {
        let (backend, fake) = paired_backend();
        let app = app(backend);
        let (_, token) = page(&app).await;
        let bodies =
            [("/api/remove-tunnel", r#"{"id":"t0"}"#), ("/api/allow-proxies", "{}"), ("/api/skip-proxies", "{}")];

        for (path, body) in bodies {
            for from in ["172.30.33.5", "192.168.1.20", "127.0.0.1"] {
                let req = request(Method::POST, path, from)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::HOST, "homeassistant.local:8123")
                    .header(CSRF_HEADER, &token);
                let (status, _, reply) = send(&app, req.body(Body::from(body)).unwrap()).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{path} from {from}");
                assert!(reply.contains("\"forbidden\""), "{path} {from}: {reply}");
            }
            let refused: Vec<(&str, axum::http::request::Builder)> = vec![
                ("no token", post_json(path, None)),
                ("wrong token", post_json(path, Some(&"0".repeat(64)))),
                ("cross-site origin", post_json(path, Some(&token)).header(header::ORIGIN, "https://evil.example")),
                ("cross-site fetch", post_json(path, Some(&token)).header("sec-fetch-site", "cross-site")),
            ];
            for (what, builder) in refused {
                let (status, _, reply) = send(&app, builder.body(Body::from(body)).unwrap()).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {what}");
                assert!(reply.contains("\"csrf\""), "{path} {what}: {reply}");
            }
        }
        assert!(fake.removes.lock().unwrap().is_empty(), "refused before acting");
        assert_eq!(*fake.allows.lock().unwrap(), 0, "refused before acting");
        assert_eq!(*fake.skips.lock().unwrap(), 0, "refused before acting");

        let ok = post_json("/api/remove-tunnel", Some(&token)).header(header::ORIGIN, "http://homeassistant.local:8123");
        let (status, _, reply) = send(&app, ok.body(Body::from(r#"{"id":"t0"}"#)).unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        assert_eq!(serde_json::from_str::<serde_json::Value>(&reply).unwrap()["label"], "ha-old");
        // The view decides what may go; its refusal is passed on.
        let (status, _, reply) =
            send(&app, post_json("/api/remove-tunnel", Some(&token)).body(Body::from(r#"{"id":"t1"}"#)).unwrap()).await;
        assert_eq!(status, StatusCode::CONFLICT, "{reply}");
        assert!(reply.contains("not an older tunnel"));
        assert_eq!(*fake.removes.lock().unwrap(), ["t0", "t1"]);
        // A body without an id is refused before the view sees it.
        let (status, _, _) = send(&app, post_json("/api/remove-tunnel", Some(&token)).body(Body::from("{}")).unwrap()).await;
        assert!(status.is_client_error());
        assert_eq!(fake.removes.lock().unwrap().len(), 2);

        let (status, _, reply) = send(&app, post_json("/api/allow-proxies", Some(&token)).body(Body::from("{}")).unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        assert_eq!(serde_json::from_str::<serde_json::Value>(&reply).unwrap()["mode"], "paired");
        assert_eq!(*fake.allows.lock().unwrap(), 1);

        let (status, _, reply) = send(&app, post_json("/api/skip-proxies", Some(&token)).body(Body::from("{}")).unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        assert_eq!(serde_json::from_str::<serde_json::Value>(&reply).unwrap()["mode"], "paired");
        assert_eq!(*fake.skips.lock().unwrap(), 1);

        // Not setup actions.
        let app = self::app(setup_backend());
        let (_, token) = page(&app).await;
        for (path, body) in bodies {
            let (status, _, _) = send(&app, post_json(path, Some(&token)).body(Body::from(body)).unwrap()).await;
            assert_eq!(status, StatusCode::CONFLICT, "{path}");
        }
    }

    /// What the page gets: the tunnels, older ones and the proxy step, and
    /// nothing secret.
    #[tokio::test]
    async fn paired_state_carries_older_tunnels_and_the_proxy_step() {
        let (backend, _) = paired_backend();
        let app = app(backend);
        let (status, _, body) = send(&app, request(Method::GET, "/api/state", SUPERVISOR).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["paired"]["older"][0]["id"], "t0");
        assert_eq!(v["paired"]["tunnels"][0]["new_address"], false);
        assert_eq!(v["paired"]["trusted_proxies"]["state"], "needed");
        assert_eq!(v["paired"]["trusted_proxies"]["setup"], true);
        for secret in ["token", "SUPERVISOR", "private_key", "Bearer"] {
            assert!(!body.contains(secret), "{secret} in {body}");
        }
    }

    #[tokio::test]
    async fn addresses_resolve_safely() {
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(resolve_addresses("auto", "auto", None).await.unwrap(), (loopback, vec![loopback]));
        assert_eq!(
            resolve_addresses("172.30.32.1", "172.30.32.2, 10.0.0.9", None).await.unwrap(),
            ("172.30.32.1".parse().unwrap(), vec!["172.30.32.2".parse().unwrap(), "10.0.0.9".parse().unwrap()])
        );
        assert!(resolve_addresses("0.0.0.0", "auto", None).await.is_err());
        assert!(resolve_addresses("::", "auto", None).await.is_err());
        assert!(resolve_addresses("nope", "auto", None).await.is_err());
        assert!(resolve_addresses("auto", "1.2.3", None).await.is_err());
        assert!(bind("0.0.0.0".parse().unwrap(), 0).await.is_err());
        assert!(bind("::".parse().unwrap(), 0).await.is_err());
    }

    #[tokio::test]
    async fn a_real_listener_refuses_other_peers() {
        // End to end over TCP: loopback is not the allowed peer here.
        let app = router(setup_backend(), vec!["172.30.32.2".parse().unwrap()]);
        let listener = bind("127.0.0.1".parse().unwrap(), 0).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = serve_on(listener, app);
        let status = raw_get(addr, "/api/state").await;
        assert!(status.starts_with("HTTP/1.1 403"), "{status}");
        task.abort();

        let app = router(setup_backend(), vec!["127.0.0.1".parse().unwrap()]);
        let listener = bind("127.0.0.1".parse().unwrap(), 0).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = serve_on(listener, app);
        let status = raw_get(addr, "/api/state").await;
        assert!(status.starts_with("HTTP/1.1 200"), "{status}");
        task.abort();
    }

    async fn raw_get(addr: SocketAddr, path: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }
}
