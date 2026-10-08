//! Home Assistant Core's HTTP settings, as the add-on reaches them through
//! the Supervisor: whether Home Assistant accepts requests that arrive
//! through Datum, and turning that on from the add-on's page.
//!
//! Every request through the tunnel reaches Home Assistant from the add-on's
//! local hop (`127.0.0.1`, or `::1`) carrying `X-Forwarded-For`, as Datum's
//! edge sets it. Home Assistant answers such a request with `400: Bad
//! Request` unless `use_x_forwarded_for` is on and the connecting address is
//! a trusted proxy (`homeassistant/components/http/forwarded.py`).
//!
//! How Home Assistant stores and changes that, from home-assistant/core
//! (`components/http/websocket_api.py`, `config.py`, 2026-10):
//!
//! - The settings live in a store with two slots: `stable`, the last
//!   confirmed config, and `pending`, one waiting for confirmation.
//!   `http/config` returns both, with `revert_at` and `active_config_type`
//!   (`stable`, `pending`, `default`, `default_legacy_port`).
//! - `http/config/configure` takes a whole config, validated by
//!   `HTTP_STORAGE_SCHEMA` (extra keys are refused, so the `created_at`,
//!   `error` and `error_message` metadata the store adds must be dropped),
//!   plus a check that `use_x_forwarded_for` has a trusted proxy. It stores
//!   it as `pending` and restarts Home Assistant, answering
//!   `{"restart": bool}`. Only while Home Assistant is running
//!   (`not_running` otherwise).
//! - A start with a pending config uses it on trial and schedules a revert
//!   five minutes later (`AUTO_REVERT_DELAY`). `http/config/promote` makes it
//!   `stable`; without that, Home Assistant marks it `not_promoted` and
//!   restarts on the old config. A pending config that failed (its `error`
//!   is set) is never applied again.
//! - All three commands are `require_admin`.
//!
//! And from home-assistant/supervisor (`api/proxy.py`,
//! `homeassistant/api.py`) and core (`components/hassio/__init__.py`,
//! `http/auth.py`): an add-on with `homeassistant_api: true` gets Core's
//! websocket at `ws://supervisor/core/websocket`, authenticating with
//! `{"type": "auth", "access_token": $SUPERVISOR_TOKEN}`. The Supervisor
//! checks that token itself and then relays to Core over its own
//! connection, which Core authenticates as the Supervisor's system user
//! (over the Unix socket, or with the Supervisor's refresh token over TCP).
//! The hassio integration creates that user in the admin group and puts it
//! back there if it ever leaves, so `require_admin` passes. The relay only
//! blocks `supervisor/` and `hassio/` commands. `GET /core/api/` answers
//! `API running.` once Core is back after a restart.

use std::net::IpAddr;
use std::time::Duration;

use n0_future::{SinkExt, StreamExt};
use secrecy::ExposeSecret;
use serde_json::{Map, Value, json};
use tokio_websockets::{ClientBuilder, MaybeTlsStream, Message, WebSocketStream};

use super::ha_supervisor::Supervisor;

/// The add-on's local hop connects to Home Assistant from one of these.
pub const LOOPBACK_PROXIES: [&str; 2] = ["127.0.0.1", "::1"];

/// What Home Assistant's store adds to each config slot, and refuses in a
/// config it is given.
const META_KEYS: [&str; 3] = ["created_at", "error", "error_message"];

const USE_X_FORWARDED_FOR: &str = "use_x_forwarded_for";
const TRUSTED_PROXIES: &str = "trusted_proxies";

// ---- What the settings say ----

/// Where Home Assistant is with accepting requests through Datum.
#[derive(Debug, Clone, PartialEq)]
pub enum ProxySetup {
    /// `stable` has `use_x_forwarded_for` on and trusts both loopback
    /// addresses.
    Ready,
    /// Not yet. `config` is what Allow sends: `stable` with only that
    /// changed.
    Needed { config: Value },
    /// Someone has a change waiting for confirmation (in Settings → System
    /// → Network). It is theirs to finish, so it is left alone.
    Pending,
}

/// Reads `http/config`'s result.
pub fn assess(http_config: &Value) -> Result<ProxySetup, String> {
    let stable = http_config
        .get("stable")
        .filter(|s| s.is_object())
        .ok_or("Home Assistant's HTTP settings have no stable config")?;
    if accepts_forwarded(stable) {
        return Ok(ProxySetup::Ready);
    }
    if live_pending(http_config).is_some() {
        return Ok(ProxySetup::Pending);
    }
    Ok(ProxySetup::Needed { config: merged(stable)? })
}

/// A pending config that has not failed. One that has (`error` set) is
/// never applied again, so a new one may replace it.
fn live_pending(http_config: &Value) -> Option<&Value> {
    http_config
        .get("pending")
        .filter(|p| p.is_object())
        .filter(|p| p.get("error").is_none_or(Value::is_null))
}

/// `use_x_forwarded_for` on, and both loopback addresses trusted.
pub fn accepts_forwarded(config: &Value) -> bool {
    let on = config.get(USE_X_FORWARDED_FOR).and_then(Value::as_bool) == Some(true);
    let proxies = trusted_proxies(config).unwrap_or_default();
    on && LOOPBACK_PROXIES
        .iter()
        .all(|ip| covers(&proxies, ip.parse().expect("a literal address")))
}

fn trusted_proxies(config: &Value) -> Result<Vec<String>, String> {
    match config.get(TRUSTED_PROXIES) {
        None | Some(Value::Null) => Ok(Vec::new()),
        // The schema's EnsureList: a single entry is a list of one.
        Some(Value::String(s)) => Ok(vec![s.clone()]),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| v.as_str().map(str::to_string).ok_or_else(|| format!("trusted proxy {v} is not text")))
            .collect(),
        Some(other) => Err(format!("trusted_proxies is {other}, not a list")),
    }
}

/// `base` with `use_x_forwarded_for` on and the loopback addresses added
/// to the trusted proxies, unless an entry already covers them. Every other
/// field is kept exactly, in order (port, SSL, CORS, IP bans, the proxies
/// already there); only the store's metadata is dropped, which Home
/// Assistant would refuse.
pub fn merged(base: &Value) -> Result<Value, String> {
    let mut config: Map<String, Value> = base.as_object().cloned().ok_or("the config is not an object")?;
    for key in META_KEYS {
        config.remove(key);
    }
    let mut proxies = trusted_proxies(base)?;
    for (ip, network) in [("127.0.0.1", "127.0.0.1/32"), ("::1", "::1/128")] {
        if !covers(&proxies, ip.parse().expect("a literal address")) {
            proxies.push(network.to_string());
        }
    }
    config.insert(USE_X_FORWARDED_FOR.into(), Value::Bool(true));
    config.insert(TRUSTED_PROXIES.into(), json!(proxies));
    let config = Value::Object(config);
    validate(&config)?;
    Ok(config)
}

/// The checks Home Assistant makes on the fields this touches, so a config
/// it would refuse is never sent.
pub fn validate(config: &Value) -> Result<(), String> {
    let proxies = trusted_proxies(config)?;
    for p in &proxies {
        if parse_network(p).is_none() {
            return Err(format!("trusted proxy {p:?} is not an IP address or network"));
        }
    }
    let on = config.get(USE_X_FORWARDED_FOR).and_then(Value::as_bool) == Some(true);
    if on && proxies.is_empty() {
        return Err("at least one trusted proxy is required to use X-Forwarded-For".into());
    }
    if let Some(key) = META_KEYS.iter().find(|k| config.get(**k).is_some()) {
        return Err(format!("{key} is store metadata, not a setting"));
    }
    Ok(())
}

/// Whether one of `proxies` contains `ip`, as Python's `ip in ip_network(p)`
/// decides it: same address family, and the network's prefix matches.
pub fn covers(proxies: &[String], ip: IpAddr) -> bool {
    proxies
        .iter()
        .filter_map(|p| parse_network(p))
        .any(|(net, len)| network_contains(net, len, ip))
}

/// `a.b.c.d`, `a.b.c.d/len`, an IPv6 address, or one with `/len`. A bare
/// address is a single host, as `ip_network` reads it.
fn parse_network(s: &str) -> Option<(IpAddr, u8)> {
    let s = s.trim();
    let (addr, len) = match s.split_once('/') {
        Some((a, l)) => (a, Some(l)),
        None => (s, None),
    };
    let addr: IpAddr = addr.parse().ok()?;
    let max = if addr.is_ipv4() { 32 } else { 128 };
    let len = match len {
        Some(l) => l.parse::<u8>().ok().filter(|l| *l <= max)?,
        None => max,
    };
    Some((addr, len))
}

fn network_contains(net: IpAddr, len: u8, ip: IpAddr) -> bool {
    match (net, ip) {
        (IpAddr::V4(n), IpAddr::V4(i)) => {
            let mask = if len == 0 { 0 } else { u32::MAX << (32 - u32::from(len)) };
            u32::from(n) & mask == u32::from(i) & mask
        }
        (IpAddr::V6(n), IpAddr::V6(i)) => {
            let mask = if len == 0 { 0 } else { u128::MAX << (128 - u32::from(len)) };
            u128::from(n) & mask == u128::from(i) & mask
        }
        _ => false,
    }
}

// ---- Home Assistant's websocket ----

/// Why a websocket call did not give a result. Never carries the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsError {
    /// Could not connect, or the handshake failed (Home Assistant may be
    /// restarting: the Supervisor answers 502 then).
    Connect(String),
    /// The Supervisor refused the token.
    Auth(String),
    /// The connection ended, as it does when Home Assistant restarts.
    Closed,
    Timeout,
    /// Home Assistant answered the command with an error.
    Command { code: String, message: String },
    Protocol(String),
}

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WsError::Connect(e) => write!(f, "could not connect to Home Assistant: {e}"),
            WsError::Auth(e) => write!(f, "Home Assistant refused the add-on's token: {e}"),
            WsError::Closed => write!(f, "the connection to Home Assistant closed"),
            WsError::Timeout => write!(f, "Home Assistant did not answer in time"),
            WsError::Command { code, message } => write!(f, "{message} ({code})"),
            WsError::Protocol(e) => write!(f, "unexpected answer from Home Assistant: {e}"),
        }
    }
}

impl std::error::Error for WsError {}

/// One authenticated connection to Home Assistant's websocket API.
pub struct HaWebsocket {
    ws: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    next_id: u64,
    timeout: Duration,
}

impl HaWebsocket {
    /// Connects and authenticates. `timeout` bounds the handshake and,
    /// afterwards, each command.
    pub async fn connect(sup: &Supervisor, timeout: Duration) -> Result<Self, WsError> {
        let url = core_websocket_url(sup.base());
        let builder = ClientBuilder::new().uri(&url).map_err(|e| WsError::Connect(format!("bad URL {url}: {e}")))?;
        let (ws, _) = tokio::time::timeout(timeout, builder.connect())
            .await
            .map_err(|_| WsError::Timeout)?
            .map_err(|e| WsError::Connect(sup.redact(e.to_string())))?;
        let mut this = Self { ws, next_id: 1, timeout };
        let hello = this.recv().await?;
        if hello["type"] != "auth_required" {
            return Err(WsError::Protocol(format!("expected auth_required, got {}", hello["type"])));
        }
        this.send(json!({"type": "auth", "access_token": sup.token().expose_secret()})).await?;
        let reply = this.recv().await?;
        match reply["type"].as_str() {
            Some("auth_ok") => Ok(this),
            Some("auth_invalid") => Err(WsError::Auth(sup.redact(
                reply["message"].as_str().unwrap_or("invalid access").to_string(),
            ))),
            other => Err(WsError::Protocol(format!("expected auth_ok, got {other:?}"))),
        }
    }

    /// Sends one command and returns its `result`.
    pub async fn command(&mut self, mut msg: Value) -> Result<Value, WsError> {
        let id = self.next_id;
        self.next_id += 1;
        msg["id"] = json!(id);
        self.send(msg).await?;
        loop {
            let reply = self.recv().await?;
            if reply["id"] != json!(id) || reply["type"] != "result" {
                // Events from a subscription, or a reply to something else.
                continue;
            }
            if reply["success"] == true {
                return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
            }
            return Err(WsError::Command {
                code: reply["error"]["code"].as_str().unwrap_or("unknown_error").to_string(),
                message: reply["error"]["message"].as_str().unwrap_or("no message").to_string(),
            });
        }
    }

    /// Waits for Home Assistant to close the connection, as it does when it
    /// stops; true if it did within `within`.
    pub async fn wait_closed(&mut self, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            match tokio::time::timeout_at(deadline, self.ws.next()).await {
                Err(_) => return false,
                Ok(None) | Ok(Some(Err(_))) => return true,
                Ok(Some(Ok(m))) if m.is_close() => return true,
                Ok(Some(Ok(_))) => continue,
            }
        }
    }

    pub async fn close(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(2), self.ws.close()).await;
    }

    async fn send(&mut self, msg: Value) -> Result<(), WsError> {
        tokio::time::timeout(self.timeout, self.ws.send(Message::text(msg.to_string())))
            .await
            .map_err(|_| WsError::Timeout)?
            .map_err(|_| WsError::Closed)
    }

    async fn recv(&mut self) -> Result<Value, WsError> {
        loop {
            let msg = tokio::time::timeout(self.timeout, self.ws.next())
                .await
                .map_err(|_| WsError::Timeout)?
                .ok_or(WsError::Closed)?
                .map_err(|_| WsError::Closed)?;
            if msg.is_close() {
                return Err(WsError::Closed);
            }
            let Some(text) = msg.as_text() else {
                // Pings are answered by the library; nothing else is sent.
                continue;
            };
            return serde_json::from_str(text).map_err(|e| WsError::Protocol(format!("not JSON: {e}")));
        }
    }
}

/// `ws://supervisor/core/websocket`, from the Supervisor's base URL.
fn core_websocket_url(base: &str) -> String {
    let rest = base
        .strip_prefix("https://")
        .map(|r| format!("wss://{r}"))
        .or_else(|| base.strip_prefix("http://").map(|r| format!("ws://{r}")))
        .unwrap_or_else(|| base.to_string());
    format!("{}/core/websocket", rest.trim_end_matches('/'))
}

/// `http/config`, on a connection of its own.
pub async fn read_http_config(sup: &Supervisor, timeout: Duration) -> Result<Value, WsError> {
    let mut ws = HaWebsocket::connect(sup, timeout).await?;
    let result = ws.command(json!({"type": "http/config"})).await;
    ws.close().await;
    result
}

/// Whether Home Assistant Core answers its API through the Supervisor:
/// `GET /core/api/`, which says `API running.` once Core is up.
pub async fn core_api_up(sup: &Supervisor) -> bool {
    let url = format!("{}/core/api/", sup.base());
    match sup.http().get(&url).bearer_auth(sup.token().expose_secret()).send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

// ---- Allow ----

/// How long each wait of [`allow_forwarded_requests`] may take.
///
/// Home Assistant's five-minute revert clock starts when it is *back*: it
/// schedules the revert while loading the pending config at start
/// (`components/http/config.py`, `async_load_config`: `if
/// store.active_config_type is PENDING: store.async_schedule_revert_to_stable()`),
/// and reports when in `http/config`'s `revert_at`. So the wait for it to
/// come back is not bounded by that revert; only the time from "back on
/// trial" to promote is, and that is measured against `revert_at`.
#[derive(Debug, Clone)]
pub struct AllowTiming {
    /// For Home Assistant to close the websocket after it took the setting,
    /// as it does when its restart begins. A close that never arrives is
    /// not taken as "no restart" by itself (a relay can miss it): Allow
    /// goes on to look for Home Assistant on trial, and only one seen
    /// answering on its old setting, never down, for twice this long
    /// counts as one that did not restart.
    pub restart_begins: Duration,
    /// For it to come back on trial with the new setting, from when it took
    /// it. A Home Assistant Green can take several minutes to restart.
    pub restart_ends: Duration,
    /// The least time left before `revert_at` to start checking and
    /// confirming; with less, Allow stops rather than race the revert.
    pub confirm_margin: Duration,
    /// Home Assistant's revert delay (`AUTO_REVERT_DELAY`), counted from
    /// when it was seen back, if `revert_at` is missing.
    pub revert_after: Duration,
    pub poll: Duration,
    /// Per websocket call.
    pub call: Duration,
}

impl Default for AllowTiming {
    fn default() -> Self {
        Self {
            restart_begins: Duration::from_secs(90),
            restart_ends: Duration::from_secs(15 * 60),
            confirm_margin: Duration::from_secs(45),
            revert_after: Duration::from_secs(5 * 60),
            poll: Duration::from_secs(3),
            call: Duration::from_secs(20),
        }
    }
}

/// Where [`allow_forwarded_requests`] is, for the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowStep {
    Saving,
    Restarting,
    Verifying,
    Confirming,
}

impl AllowStep {
    pub fn describe(self) -> &'static str {
        match self {
            AllowStep::Saving => "Saving the setting in Home Assistant…",
            AllowStep::Restarting => {
                "Home Assistant is restarting with the new setting… This can take several minutes (on a Home Assistant Green, up to about 10)."
            }
            AllowStep::Verifying => "Checking a request the way Datum sends it…",
            AllowStep::Confirming => "Confirming the setting…",
        }
    }
}

/// How [`allow_forwarded_requests`] ended. Every message is for a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowOutcome {
    /// Nothing to do.
    AlreadyAllowed,
    /// A change someone else made is waiting for confirmation; untouched.
    PendingByOther,
    /// On, checked, and confirmed.
    Allowed,
    /// Not confirmed, so Home Assistant goes back to the previous setting
    /// by itself, if it took the new one at all.
    Failed(String),
}

/// What the verify step found: the status a request through the add-on's
/// local hop got, carrying `X-Forwarded-For`.
pub type VerifyResult = Result<u16, String>;

const REVERTS: &str = "Home Assistant goes back to the previous setting by itself, 5 minutes after it restarted with it.";

/// `config` without the store's metadata, to compare slots by content.
fn without_meta(config: &Value) -> Value {
    let mut c = config.clone();
    if let Some(m) = c.as_object_mut() {
        for key in META_KEYS {
            m.remove(key);
        }
    }
    c
}

/// Whether the pending slot holds `wanted` (what Allow sends), failed or
/// not.
fn pending_is(http_config: &Value, wanted: &Value) -> bool {
    http_config.get("pending").filter(|p| p.is_object()).is_some_and(|p| without_meta(p) == *wanted)
}

/// Home Assistant runs `wanted` on trial: started on the pending slot, and
/// that slot is `wanted` and has not failed.
fn on_trial_with(http_config: &Value, wanted: &Value) -> bool {
    http_config["active_config_type"] == "pending" && live_pending(http_config).is_some() && pending_is(http_config, wanted)
}

/// When Home Assistant reverts its trial, from `http/config`'s `revert_at`
/// (an ISO 8601 time on Home Assistant's clock, which inside the add-on is
/// the same machine's), as an instant on ours.
fn revert_instant(http_config: &Value) -> Option<tokio::time::Instant> {
    let at = chrono::DateTime::parse_from_rfc3339(http_config.get("revert_at")?.as_str()?).ok()?;
    let left = at.with_timezone(&chrono::Utc).signed_duration_since(chrono::Utc::now());
    let now = tokio::time::Instant::now();
    Some(match left.to_std() {
        Ok(d) => now + d,
        // Already past.
        Err(_) => now,
    })
}

/// Turns on `use_x_forwarded_for` with the loopback addresses as trusted
/// proxies, the way Settings → System → Network does: save it as pending,
/// let Home Assistant restart on it, check that a request through the
/// add-on's local hop with `X-Forwarded-For` is no longer refused
/// (`verify`), and only then confirm it, before Home Assistant's own revert
/// is due. On any failure it is not confirmed, and that revert restores the
/// old setting.
///
/// Allow's own change, found pending (an earlier Allow that gave up while
/// Home Assistant was still restarting, then Retry), is carried on rather
/// than refused. A pending change made by someone else is never touched.
pub async fn allow_forwarded_requests<V, F>(
    sup: &Supervisor,
    timing: &AllowTiming,
    mut step: impl FnMut(AllowStep),
    verify: V,
) -> AllowOutcome
where
    V: FnOnce() -> F,
    F: std::future::Future<Output = VerifyResult>,
{
    use AllowOutcome::Failed;

    let mut ws = match HaWebsocket::connect(sup, timing.call).await {
        Ok(ws) => ws,
        Err(e) => return Failed(format!("Could not reach Home Assistant, so nothing was changed: {e}.")),
    };
    let current = match ws.command(json!({"type": "http/config"})).await {
        Ok(c) => c,
        Err(e) => return Failed(format!("Could not read Home Assistant's network settings, so nothing was changed: {e}.")),
    };
    let unreadable = |e: &str| Failed(format!("Home Assistant's network settings could not be read, so nothing was changed: {e}."));
    let setup = match assess(&current) {
        Ok(ProxySetup::Ready) => return AllowOutcome::AlreadyAllowed,
        Ok(s) => s,
        Err(e) => return unreadable(&e),
    };
    // What Allow sends, and recognises as its own when it finds it
    // pending: stable with only the forwarding settings changed.
    let wanted = match merged(&current["stable"]) {
        Ok(w) => w,
        Err(e) => return unreadable(&e),
    };
    let started = tokio::time::Instant::now();
    // `None`: already on trial with our change. `Some(seen_down)`: wait
    // for that, knowing whether the restart was seen begin.
    let restarted = match setup {
        ProxySetup::Ready => return AllowOutcome::AlreadyAllowed,
        ProxySetup::Pending if on_trial_with(&current, &wanted) => {
            // Back on trial with our change (Retry after an Allow that gave
            // up waiting): check and confirm it.
            ws.close().await;
            None
        }
        ProxySetup::Pending if pending_is(&current, &wanted) => {
            // Our change, saved but not running yet: Home Assistant may be
            // on its way down. Wait for it as after saving.
            step(AllowStep::Restarting);
            ws.close().await;
            Some(false)
        }
        ProxySetup::Pending => return AllowOutcome::PendingByOther,
        ProxySetup::Needed { config } => {
            step(AllowStep::Saving);
            match ws.command(json!({"type": "http/config/configure", "config": config})).await {
                Ok(r) if r["restart"] == true => {}
                Ok(_) => return Failed("Home Assistant did not take the new setting, so it did not restart. Nothing was changed.".into()),
                Err(WsError::Command { code, .. }) if code == "not_running" => {
                    return Failed("Home Assistant is still starting. Try again in a minute.".into());
                }
                Err(e) => return Failed(format!("Home Assistant did not take the new setting: {e}. Nothing was changed.")),
            }
            step(AllowStep::Restarting);
            let closed = ws.wait_closed(timing.restart_begins).await;
            ws.close().await;
            Some(closed)
        }
    };

    let (mut ws, trial) = match restarted {
        None => match HaWebsocket::connect(sup, timing.call).await {
            Ok(ws) => (ws, current),
            Err(e) => return Failed(format!("Could not reach Home Assistant to confirm the setting: {e}. {REVERTS}")),
        },
        Some(seen_down) => match wait_for_trial(sup, timing, &wanted, seen_down, started).await {
            Ok(back) => back,
            Err(why) => return Failed(why),
        },
    };
    let seen_back = tokio::time::Instant::now();

    // Confirm before Home Assistant's revert, or not at all.
    let revert_at = revert_instant(&trial).unwrap_or(seen_back + timing.revert_after);
    let left = revert_at.saturating_duration_since(tokio::time::Instant::now());
    if left < timing.confirm_margin {
        ws.close().await;
        return Failed(format!(
            "Home Assistant came back with the new setting, but reverts it in {} seconds: too soon to check and confirm it safely, so it was not confirmed. It goes back to the previous setting by itself then; try again after that.",
            left.as_secs()
        ));
    }
    let verify_by = revert_at - timing.confirm_margin / 2;

    step(AllowStep::Verifying);
    match tokio::time::timeout_at(verify_by, verify()).await {
        Ok(Ok(400)) => {
            ws.close().await;
            return Failed(format!(
                "A request sent the way Datum sends it still got 400: Bad Request, so the setting was not confirmed. {REVERTS}"
            ));
        }
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            ws.close().await;
            return Failed(format!("Could not check a request the way Datum sends it ({e}), so the setting was not confirmed. {REVERTS}"));
        }
        Err(_) => {
            ws.close().await;
            return Failed(format!(
                "Checking a request the way Datum sends it did not finish before Home Assistant's own revert was due, so the setting was not confirmed. {REVERTS}"
            ));
        }
    }

    step(AllowStep::Confirming);
    let promoted = tokio::time::timeout_at(revert_at, ws.command(json!({"type": "http/config/promote"}))).await;
    ws.close().await;
    match promoted {
        Ok(Ok(_)) => AllowOutcome::Allowed,
        Ok(Err(e)) => Failed(format!("Could not confirm the setting: {e}. {REVERTS}")),
        Err(_) => Failed(format!("Home Assistant did not confirm the setting before its own revert was due. {REVERTS}")),
    }
}

/// Waits for Home Assistant to be back on trial with `wanted`, and returns
/// a connection to it and its `http/config`. `seen_down`: its restart was
/// already seen begin (the websocket closed). `started`: when the setting
/// was saved (or found saved).
async fn wait_for_trial(
    sup: &Supervisor,
    timing: &AllowTiming,
    wanted: &Value,
    mut seen_down: bool,
    started: tokio::time::Instant,
) -> Result<(HaWebsocket, Value), String> {
    let deadline = started + timing.restart_ends;
    // Seen answering on the old setting, never down, for this long: it did
    // not restart.
    let no_restart_after = started + timing.restart_begins * 2;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "Home Assistant did not come back with the new setting within {} minutes, so it was not confirmed. If it comes back with it later, it goes back to the previous setting by itself 5 minutes after that.",
                timing.restart_ends.as_secs().div_ceil(60)
            ));
        }
        // The old instance can still answer while it shuts down, which is
        // why the active slot is checked rather than just "it answers".
        if !core_api_up(sup).await {
            seen_down = true;
        } else {
            match HaWebsocket::connect(sup, timing.call).await {
                Err(_) => seen_down = true,
                Ok(mut ws) => match ws.command(json!({"type": "http/config"})).await {
                    Ok(c) if on_trial_with(&c, wanted) => return Ok((ws, c)),
                    Ok(c) if pending_is(&c, wanted) && c.pointer("/pending/error").is_some_and(|e| !e.is_null()) => {
                        ws.close().await;
                        let error = c.pointer("/pending/error").and_then(Value::as_str).unwrap_or_default();
                        if error == "not_promoted" {
                            return Err("Home Assistant went back to the previous setting before the add-on could confirm the new one (it reverts an unconfirmed setting 5 minutes after restarting with it). Nothing else was changed; try again.".into());
                        }
                        let why = c.pointer("/pending/error_message").and_then(Value::as_str).unwrap_or_default();
                        return Err(format!(
                            "Home Assistant could not use the new setting and went back to the previous one{}.",
                            if why.is_empty() { String::new() } else { format!(": {why}") }
                        ));
                    }
                    Ok(c) => {
                        ws.close().await;
                        if !pending_is(&c, wanted) {
                            return Err("The new setting is no longer waiting in Home Assistant (it was confirmed, discarded or replaced in Settings → System → Network), so the add-on did not confirm it.".into());
                        }
                        if !seen_down && tokio::time::Instant::now() >= no_restart_after {
                            return Err(
                                "Home Assistant saved the setting but did not restart, so it was not confirmed. Restart Home Assistant, then confirm or discard the change in Settings → System → Network."
                                    .into(),
                            );
                        }
                    }
                    Err(_) => {
                        ws.close().await;
                        seen_down = true;
                    }
                },
            }
        }
        tokio::time::sleep(timing.poll).await;
    }
}

// ---- At daemon start ----

/// What the daemon found out at start, for its log.
#[derive(Debug, Clone, PartialEq)]
pub struct StartupCheck {
    /// The pairing notification was asked to go (it may not have been
    /// shown at all; dismissing is idempotent).
    pub dismissed: Result<(), String>,
    /// `None` when Home Assistant could not be asked.
    pub proxies: Option<ProxySetup>,
}

/// At every daemon start inside the add-on. A key exists, so pairing is
/// over: its notification, if one was left behind, has no use any more.
/// Then whether Home Assistant accepts requests through Datum yet, for a
/// one-line hint. Best-effort throughout.
pub async fn startup_check(sup: &Supervisor, timeout: Duration) -> StartupCheck {
    let dismissed = sup.dismiss().await;
    let proxies = match read_http_config(sup, timeout).await {
        Ok(c) => assess(&c).ok(),
        Err(e) => {
            tracing::debug!("could not read Home Assistant's HTTP settings: {e}");
            None
        }
    };
    StartupCheck { dismissed, proxies }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::SecretString;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncWriteExt;

    const SUP_TOKEN: &str = "supervisor-token-SECRET-1234";

    fn stable() -> Value {
        json!({
            "server_port": 8123,
            "cors_allowed_origins": ["https://cast.home-assistant.io"],
            "ip_ban_enabled": true,
            "login_attempts_threshold": 5,
            "ssl_profile": "modern",
            "use_x_frame_options": true,
            "trusted_proxies": ["172.30.33.0/24"],
            "server_host": ["0.0.0.0", "::"],
            "created_at": "2026-09-01T10:00:00+00:00",
            "error": null,
            "error_message": null,
        })
    }

    // ---- merge, containment, validation ----

    #[test]
    fn merge_keeps_every_field_and_adds_only_what_is_missing() {
        let m = merged(&stable()).unwrap();
        let mut want = stable();
        let w = want.as_object_mut().unwrap();
        for k in META_KEYS {
            w.remove(k);
        }
        w.insert("use_x_forwarded_for".into(), json!(true));
        w.insert("trusted_proxies".into(), json!(["172.30.33.0/24", "127.0.0.1/32", "::1/128"]));
        assert_eq!(m, want);
        // Field order is kept too (serde_json preserves insertion order here
        // only with preserve_order; equality above is what matters).
        assert!(accepts_forwarded(&m));
    }

    #[test]
    fn proxies_already_covered_are_not_added_again() {
        let mut base = stable();
        base["trusted_proxies"] = json!(["127.0.0.0/8", "::1"]);
        let m = merged(&base).unwrap();
        assert_eq!(m["trusted_proxies"], json!(["127.0.0.0/8", "::1"]));
        base["trusted_proxies"] = json!("127.0.0.1");
        assert_eq!(merged(&base).unwrap()["trusted_proxies"], json!(["127.0.0.1", "::1/128"]));
        base.as_object_mut().unwrap().remove("trusted_proxies");
        assert_eq!(merged(&base).unwrap()["trusted_proxies"], json!(["127.0.0.1/32", "::1/128"]));
    }

    #[test]
    fn containment_is_by_network_and_family() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let v = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(covers(&v(&["127.0.0.1/32"]), ip("127.0.0.1")));
        assert!(covers(&v(&["127.0.0.0/8"]), ip("127.0.0.1")));
        assert!(covers(&v(&["0.0.0.0/0"]), ip("127.0.0.1")));
        assert!(covers(&v(&["127.0.0.1"]), ip("127.0.0.1")));
        assert!(!covers(&v(&["127.0.0.2/32"]), ip("127.0.0.1")));
        assert!(!covers(&v(&["10.0.0.0/8", "172.30.32.0/23"]), ip("127.0.0.1")));
        assert!(covers(&v(&["::1/128"]), ip("::1")));
        assert!(covers(&v(&["::/0"]), ip("::1")));
        assert!(!covers(&v(&["fd00::/8"]), ip("::1")));
        // A v4 network never holds a v6 address, nor the other way round.
        assert!(!covers(&v(&["0.0.0.0/0"]), ip("::1")));
        assert!(!covers(&v(&["::/0"]), ip("127.0.0.1")));
        assert!(!covers(&v(&["::ffff:127.0.0.1/128"]), ip("127.0.0.1")));
        assert!(!covers(&v(&["junk", "127.0.0.1/33"]), ip("127.0.0.1")));
    }

    #[test]
    fn ready_only_with_both_loopbacks_and_the_switch() {
        let mut c = stable();
        assert!(!accepts_forwarded(&c));
        c["use_x_forwarded_for"] = json!(true);
        c["trusted_proxies"] = json!(["127.0.0.1/32"]);
        assert!(!accepts_forwarded(&c), "::1 missing");
        c["trusted_proxies"] = json!(["127.0.0.0/8", "::1/128"]);
        assert!(accepts_forwarded(&c));
        c["use_x_forwarded_for"] = json!(false);
        assert!(!accepts_forwarded(&c));
    }

    #[test]
    fn validation_matches_home_assistant() {
        assert!(validate(&json!({"use_x_forwarded_for": true, "trusted_proxies": []})).unwrap_err().contains("at least one trusted proxy"));
        assert!(validate(&json!({"use_x_forwarded_for": true})).is_err());
        assert!(validate(&json!({"use_x_forwarded_for": false})).is_ok());
        assert!(validate(&json!({"trusted_proxies": ["not-an-ip"]})).is_err());
        assert!(validate(&json!({"trusted_proxies": [5]})).is_err());
        assert!(validate(&json!({"use_x_forwarded_for": true, "trusted_proxies": ["::1"], "created_at": "x"})).is_err());
        assert!(merged(&json!({"trusted_proxies": ["nope"]})).is_err());
    }

    #[test]
    fn assess_reads_the_slots() {
        let mut c = json!({"stable": stable(), "pending": null, "revert_at": null});
        match assess(&c).unwrap() {
            ProxySetup::Needed { config } => assert_eq!(config, merged(&stable()).unwrap()),
            other => panic!("{other:?}"),
        }
        // Someone's change waits for confirmation: hands off.
        c["pending"] = json!({"server_port": 8124, "error": null});
        assert_eq!(assess(&c).unwrap(), ProxySetup::Pending);
        // A pending config that failed is dead; stable is the base.
        c["pending"] = json!({"server_port": 8124, "error": "not_promoted"});
        assert!(matches!(assess(&c).unwrap(), ProxySetup::Needed { .. }));
        // Already on: nothing to do, whatever is pending.
        c["stable"] = merged(&stable()).unwrap();
        c["pending"] = json!({"server_port": 8124, "error": null});
        assert_eq!(assess(&c).unwrap(), ProxySetup::Ready);
        assert!(assess(&json!({"pending": null})).is_err());
    }

    #[test]
    fn websocket_url_from_the_supervisor_base() {
        assert_eq!(core_websocket_url("http://supervisor"), "ws://supervisor/core/websocket");
        assert_eq!(core_websocket_url("http://127.0.0.1:9/"), "ws://127.0.0.1:9/core/websocket");
        assert_eq!(core_websocket_url("https://x"), "wss://x/core/websocket");
    }

    // ---- A fake Home Assistant behind the Supervisor ----

    #[derive(Default)]
    struct FakeHa {
        stable: Value,
        pending: Option<Value>,
        active: &'static str,
        /// Down (the Supervisor answers 502) until then, after a restart
        /// began.
        down_until: Option<std::time::Instant>,
        /// How long a restart takes.
        restart_takes: Duration,
        /// Whether configure's connection is closed as Home Assistant stops
        /// (false: a relay that never passes the close on).
        closes: bool,
        /// Home Assistant's AUTO_REVERT_DELAY, from when it is back.
        revert_after: Duration,
        revert_due: Option<std::time::Instant>,
        /// Whether a promote came in before the revert was due.
        promoted_in_time: Option<bool>,
        /// Whether the restart takes the new config (or fails to apply it).
        apply_ok: bool,
        /// Whether configure restarts at all.
        restarts: bool,
        /// Command types received, in order.
        commands: Vec<String>,
        configured: Vec<Value>,
        auth_tokens: Vec<String>,
        dismissals: u32,
        /// Every raw text frame received, to check for leaks.
        frames: VecDeque<String>,
    }

    impl FakeHa {
        fn new() -> Self {
            Self {
                stable: stable(),
                active: "stable",
                restart_takes: Duration::from_millis(60),
                closes: true,
                revert_after: Duration::from_secs(300),
                apply_ok: true,
                restarts: true,
                ..Default::default()
            }
        }

        /// What configure stores as pending.
        fn stored(config: Value) -> Value {
            let mut pending = config;
            pending["created_at"] = json!("2026-10-07T12:00:00+00:00");
            pending["error"] = Value::Null;
            pending["error_message"] = Value::Null;
            pending
        }

        /// Home Assistant as it is now: back from a restart once it has
        /// taken long enough (on trial with the pending config, revert
        /// scheduled from then), and reverted once that is due.
        fn tick(&mut self) {
            let now = std::time::Instant::now();
            if self.down_until.is_some_and(|t| now >= t) {
                self.down_until = None;
                if self.apply_ok {
                    self.active = "pending";
                    // async_load_config: a pending start schedules the revert.
                    self.revert_due = Some(now + self.revert_after);
                } else if let Some(p) = self.pending.as_mut() {
                    p["error"] = json!("apply_failed");
                    p["error_message"] = json!("cannot bind");
                }
            }
            if self.active == "pending" && self.revert_due.is_some_and(|t| now >= t) {
                // _async_revert_to_stable (and its restart, instant here).
                self.revert_due = None;
                self.active = "stable";
                if let Some(p) = self.pending.as_mut() {
                    p["error"] = json!("not_promoted");
                }
            }
        }

        fn down(&mut self) -> bool {
            self.tick();
            self.down_until.is_some()
        }

        fn config(&mut self) -> Value {
            self.tick();
            let revert_at = self.revert_due.map(|t| {
                let left = t.saturating_duration_since(std::time::Instant::now());
                (chrono::Utc::now() + chrono::Duration::from_std(left).unwrap()).to_rfc3339()
            });
            json!({"stable": self.stable, "pending": self.pending, "revert_at": revert_at,
                   "active_config_type": self.active, "default": {}})
        }

        fn api_poll(&mut self) -> bool {
            !self.down()
        }

        /// The reply, and whether to drop the connection after it.
        fn command(&mut self, msg: &Value) -> (Value, bool) {
            let id = msg["id"].clone();
            let kind = msg["type"].as_str().unwrap_or_default().to_string();
            self.commands.push(kind.clone());
            let ok = |r: Value| json!({"id": id, "type": "result", "success": true, "result": r});
            let err = |code: &str, m: &str| json!({"id": id, "type": "result", "success": false, "error": {"code": code, "message": m}});
            match kind.as_str() {
                "http/config" => (ok(self.config()), false),
                "http/config/configure" => {
                    let config = msg["config"].clone();
                    if let Err(e) = validate(&config) {
                        return (err("invalid_format", &e), false);
                    }
                    self.configured.push(config.clone());
                    self.pending = Some(Self::stored(config));
                    if !self.restarts {
                        return (ok(json!({"restart": true})), false);
                    }
                    self.down_until = Some(std::time::Instant::now() + self.restart_takes);
                    (ok(json!({"restart": true})), self.closes)
                }
                "http/config/promote" => {
                    self.tick();
                    match self.pending.take() {
                        Some(p) if p["error"].is_null() => {
                            self.promoted_in_time = Some(self.revert_due.is_none_or(|t| std::time::Instant::now() < t));
                            self.stable = p;
                            self.active = "stable";
                            self.revert_due = None;
                            (ok(Value::Null), false)
                        }
                        other => {
                            self.pending = other;
                            (err("not_allowed", "No pending HTTP config to promote"), false)
                        }
                    }
                }
                _ => (err("unknown_command", "Unknown command."), false),
            }
        }
    }

    async fn serve(fake: FakeHa) -> (Supervisor, Arc<Mutex<FakeHa>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let fake = Arc::new(Mutex::new(fake));
        let shared = fake.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(handle(sock, shared.clone()));
            }
        });
        (Supervisor::new(base, SecretString::from(SUP_TOKEN)).unwrap(), fake)
    }

    async fn handle(mut sock: tokio::net::TcpStream, fake: Arc<Mutex<FakeHa>>) {
        let mut peek = [0u8; 2048];
        let head = loop {
            let n = sock.peek(&mut peek).await.unwrap_or(0);
            if n == 0 {
                return;
            }
            let text = String::from_utf8_lossy(&peek[..n]).into_owned();
            if text.contains("\r\n\r\n") {
                break text;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let line = head.lines().next().unwrap_or_default().to_string();
        let authed = head.lines().any(|l| l.eq_ignore_ascii_case(&format!("authorization: Bearer {SUP_TOKEN}")));
        let reply = |status: u16, body: &str| {
            format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
        };
        if line.starts_with("GET /core/api/ ") {
            let mut buf = vec![0u8; head.len()];
            let _ = tokio::io::AsyncReadExt::read_exact(&mut sock, &mut buf).await;
            let up = authed && fake.lock().unwrap().api_poll();
            let r = if !authed { reply(401, "{}") } else if up { reply(200, r#"{"message":"API running."}"#) } else { reply(502, "{}") };
            let _ = sock.write_all(r.as_bytes()).await;
            return;
        }
        if line.starts_with("POST /core/api/services/persistent_notification/dismiss ") {
            let mut buf = vec![0u8; head.len()];
            let _ = tokio::io::AsyncReadExt::read_exact(&mut sock, &mut buf).await;
            if authed {
                fake.lock().unwrap().dismissals += 1;
            }
            let _ = sock.write_all(reply(if authed { 200 } else { 401 }, "[]").as_bytes()).await;
            return;
        }
        if !line.starts_with("GET /core/websocket ") || fake.lock().unwrap().down() {
            // What the Supervisor answers while Core restarts.
            let _ = sock.write_all(reply(502, "{}").as_bytes()).await;
            return;
        }
        let Ok((_, mut ws)) = tokio_websockets::ServerBuilder::new().accept(sock).await else { return };
        let _ = ws.send(Message::text(json!({"type": "auth_required", "ha_version": "2026.10.0"}).to_string())).await;
        let Some(Ok(auth)) = ws.next().await else { return };
        let auth: Value = serde_json::from_str(auth.as_text().unwrap_or("{}")).unwrap_or_default();
        let token = auth["access_token"].as_str().unwrap_or_default().to_string();
        fake.lock().unwrap().auth_tokens.push(token.clone());
        if token != SUP_TOKEN {
            let _ = ws.send(Message::text(json!({"type": "auth_invalid", "message": "Invalid access"}).to_string())).await;
            return;
        }
        let _ = ws.send(Message::text(json!({"type": "auth_ok", "ha_version": "2026.10.0"}).to_string())).await;
        while let Some(Ok(msg)) = ws.next().await {
            let Some(text) = msg.as_text() else { continue };
            let parsed: Value = serde_json::from_str(text).unwrap_or_default();
            let (reply, drop_after) = {
                let mut f = fake.lock().unwrap();
                f.frames.push_back(text.to_string());
                f.command(&parsed)
            };
            let _ = ws.send(Message::text(reply.to_string())).await;
            if drop_after {
                // Home Assistant stopping.
                let _ = ws.close().await;
                return;
            }
        }
    }

    fn fast() -> AllowTiming {
        AllowTiming {
            restart_begins: Duration::from_secs(5),
            restart_ends: Duration::from_secs(10),
            confirm_margin: Duration::from_millis(500),
            revert_after: Duration::from_secs(300),
            poll: Duration::from_millis(20),
            call: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn allow_saves_waits_verifies_then_promotes() {
        let (sup, fake) = serve(FakeHa::new()).await;
        let mut steps = Vec::new();
        let verified = Arc::new(Mutex::new(None::<&'static str>));
        let v = verified.clone();
        let fake2 = fake.clone();
        let outcome = allow_forwarded_requests(&sup, &fast(), |s| steps.push(s), || async move {
            // Verified only once Home Assistant runs the new config.
            *v.lock().unwrap() = Some(fake2.lock().unwrap().active);
            Ok(200)
        })
        .await;
        assert_eq!(outcome, AllowOutcome::Allowed);
        assert_eq!(steps, [AllowStep::Saving, AllowStep::Restarting, AllowStep::Verifying, AllowStep::Confirming]);
        assert_eq!(*verified.lock().unwrap(), Some("pending"));
        let f = fake.lock().unwrap();
        assert_eq!(f.configured, [merged(&stable()).unwrap()]);
        assert_eq!(f.commands.last().map(String::as_str), Some("http/config/promote"));
        assert!(accepts_forwarded(&f.stable));
        assert_eq!(f.stable["server_host"], json!(["0.0.0.0", "::"]), "kept");
        assert!(f.pending.is_none());
        assert!(f.auth_tokens.iter().all(|t| t == SUP_TOKEN));
    }

    /// The check fails: no promote, so Home Assistant's own revert brings
    /// the old setting back.
    #[tokio::test]
    async fn a_failed_check_is_never_promoted() {
        for verify in [Ok(400), Err("connection refused".to_string())] {
            let (sup, fake) = serve(FakeHa::new()).await;
            let outcome = allow_forwarded_requests(&sup, &fast(), |_| {}, move || async move { verify }).await;
            let AllowOutcome::Failed(why) = outcome else { panic!("{outcome:?}") };
            assert!(why.contains("not confirmed") && why.contains("goes back"), "{why}");
            let f = fake.lock().unwrap();
            assert!(!f.commands.iter().any(|c| c == "http/config/promote"), "{:?}", f.commands);
            assert!(f.pending.is_some(), "left for Home Assistant to revert");
            assert!(!accepts_forwarded(&f.stable));
        }
    }

    #[tokio::test]
    async fn a_config_that_did_not_apply_is_not_promoted() {
        let mut ha = FakeHa::new();
        ha.apply_ok = false;
        let (sup, fake) = serve(ha).await;
        let outcome = allow_forwarded_requests(&sup, &fast(), |_| {}, || async { Ok(200) }).await;
        let AllowOutcome::Failed(why) = outcome else { panic!("{outcome:?}") };
        assert!(why.contains("could not use the new setting") && why.contains("cannot bind"), "{why}");
        assert!(!fake.lock().unwrap().commands.iter().any(|c| c == "http/config/promote"));
    }

    #[tokio::test]
    async fn no_restart_and_no_return_both_end_unconfirmed() {
        let mut ha = FakeHa::new();
        ha.restarts = false;
        let (sup, fake) = serve(ha).await;
        let mut t = fast();
        t.restart_begins = Duration::from_millis(300);
        let outcome = allow_forwarded_requests(&sup, &t, |_| {}, || async { Ok(200) }).await;
        assert!(matches!(&outcome, AllowOutcome::Failed(w) if w.contains("did not restart")), "{outcome:?}");
        assert!(!fake.lock().unwrap().commands.iter().any(|c| c == "http/config/promote"));

        let mut ha = FakeHa::new();
        ha.restart_takes = Duration::from_secs(3600);
        let (sup, fake) = serve(ha).await;
        let mut t = fast();
        t.restart_ends = Duration::from_millis(500);
        let outcome = allow_forwarded_requests(&sup, &t, |_| {}, || async { Ok(200) }).await;
        assert!(matches!(&outcome, AllowOutcome::Failed(w) if w.contains("did not come back")), "{outcome:?}");
        assert!(!fake.lock().unwrap().commands.iter().any(|c| c == "http/config/promote"));
    }

    /// Real timing scaled down 600 times (1 minute is 100 ms): the 90 s for
    /// the restart to begin, 15 minutes for Home Assistant to come back,
    /// its 5-minute revert, and a 45 s margin.
    fn scaled() -> AllowTiming {
        AllowTiming {
            restart_begins: Duration::from_millis(150),
            restart_ends: Duration::from_millis(1500),
            confirm_margin: Duration::from_millis(75),
            revert_after: Duration::from_millis(500),
            poll: Duration::from_millis(5),
            call: Duration::from_secs(5),
        }
    }

    /// Seen on a Home Assistant Green: Allow started 13:31:04 and gave up
    /// at 13:35:05, four minutes on, while Home Assistant was still
    /// restarting; it came back on the new setting later. Its revert clock
    /// starts when it is back, so a restart of 6 to 10 minutes is waited
    /// for, and the setting confirmed well before the revert.
    #[tokio::test]
    async fn a_slow_restart_is_waited_for_and_confirmed_before_the_revert() {
        for minutes in [6u64, 8, 10] {
            let mut ha = FakeHa::new();
            ha.revert_after = scaled().revert_after;
            ha.restart_takes = Duration::from_millis(minutes * 100);
            let (sup, fake) = serve(ha).await;
            let outcome = allow_forwarded_requests(&sup, &scaled(), |_| {}, || async { Ok(200) }).await;
            assert_eq!(outcome, AllowOutcome::Allowed, "{minutes} minutes");
            let f = fake.lock().unwrap();
            assert_eq!(f.promoted_in_time, Some(true), "{minutes} minutes");
            assert!(accepts_forwarded(&f.stable));
        }

        // 0.3.5's four-minute limit gave up on the same restart.
        let mut ha = FakeHa::new();
        ha.revert_after = scaled().revert_after;
        ha.restart_takes = Duration::from_millis(800);
        let (sup, fake) = serve(ha).await;
        let mut t = scaled();
        t.restart_ends = Duration::from_millis(400);
        let outcome = allow_forwarded_requests(&sup, &t, |_| {}, || async { Ok(200) }).await;
        let AllowOutcome::Failed(why) = outcome else { panic!("{outcome:?}") };
        assert!(why.contains("did not come back") && why.contains("5 minutes after that"), "{why}");
        assert!(!fake.lock().unwrap().commands.iter().any(|c| c == "http/config/promote"));
    }

    /// The close of configure's connection never arrives (the relay missed
    /// it), and Home Assistant is already back on trial at the first look:
    /// carry on, without saving again.
    #[tokio::test]
    async fn back_on_trial_at_the_first_look_carries_on() {
        let mut ha = FakeHa::new();
        ha.closes = false;
        ha.revert_after = scaled().revert_after;
        ha.restart_takes = Duration::from_millis(50);
        let (sup, fake) = serve(ha).await;
        let mut steps = Vec::new();
        let outcome = allow_forwarded_requests(&sup, &scaled(), |s| steps.push(s), || async { Ok(200) }).await;
        assert_eq!(outcome, AllowOutcome::Allowed);
        assert_eq!(steps, [AllowStep::Saving, AllowStep::Restarting, AllowStep::Verifying, AllowStep::Confirming]);
        let f = fake.lock().unwrap();
        assert_eq!(f.configured.len(), 1, "saved once");
        assert_eq!(f.promoted_in_time, Some(true));
    }

    /// Retry after an Allow that gave up: Home Assistant is on trial with
    /// Allow's own change. It is checked and confirmed, not refused as
    /// someone else's, and not saved again.
    #[tokio::test]
    async fn our_own_change_on_trial_is_confirmed_on_retry() {
        let mut ha = FakeHa::new();
        ha.pending = Some(FakeHa::stored(merged(&stable()).unwrap()));
        ha.active = "pending";
        ha.revert_due = Some(std::time::Instant::now() + Duration::from_secs(120));
        let (sup, fake) = serve(ha).await;
        let mut steps = Vec::new();
        let outcome = allow_forwarded_requests(&sup, &scaled(), |s| steps.push(s), || async { Ok(200) }).await;
        assert_eq!(outcome, AllowOutcome::Allowed);
        assert_eq!(steps, [AllowStep::Verifying, AllowStep::Confirming]);
        let f = fake.lock().unwrap();
        assert!(f.configured.is_empty());
        assert_eq!(f.commands, ["http/config", "http/config/promote"]);
        assert!(accepts_forwarded(&f.stable));

        // Saved but Home Assistant not on it yet (still going down): wait
        // for it, then confirm.
        let mut ha = FakeHa::new();
        ha.pending = Some(FakeHa::stored(merged(&stable()).unwrap()));
        ha.revert_after = scaled().revert_after;
        // It answers at first (the old instance), then goes down.
        let (sup, fake) = serve(ha).await;
        let restart = {
            let fake = fake.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(40)).await;
                fake.lock().unwrap().down_until = Some(std::time::Instant::now() + Duration::from_millis(100));
            })
        };
        let outcome = allow_forwarded_requests(&sup, &scaled(), |_| {}, || async { Ok(200) }).await;
        restart.await.unwrap();
        assert_eq!(outcome, AllowOutcome::Allowed);
        assert!(fake.lock().unwrap().configured.is_empty());
    }

    /// Too little time left before Home Assistant's revert to check and
    /// confirm safely: stop, and say so; nothing is promoted.
    #[tokio::test]
    async fn too_close_to_the_revert_is_not_confirmed() {
        let mut ha = FakeHa::new();
        ha.revert_after = Duration::from_millis(40);
        ha.restart_takes = Duration::from_millis(30);
        let (sup, fake) = serve(ha).await;
        let mut steps = Vec::new();
        let outcome = allow_forwarded_requests(&sup, &scaled(), |s| steps.push(s), || async { Ok(200) }).await;
        let AllowOutcome::Failed(why) = outcome else { panic!("{outcome:?}") };
        assert!(why.contains("too soon to check and confirm"), "{why}");
        assert!(!steps.contains(&AllowStep::Verifying));
        assert!(!fake.lock().unwrap().commands.iter().any(|c| c == "http/config/promote"));

        // A check that would run past the revert is cut short.
        let mut ha = FakeHa::new();
        ha.revert_after = Duration::from_millis(300);
        ha.restart_takes = Duration::from_millis(30);
        let (sup, fake) = serve(ha).await;
        let outcome = allow_forwarded_requests(&sup, &scaled(), |_| {}, || async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(200)
        })
        .await;
        let AllowOutcome::Failed(why) = outcome else { panic!("{outcome:?}") };
        assert!(why.contains("did not finish before Home Assistant's own revert"), "{why}");
        assert!(!fake.lock().unwrap().commands.iter().any(|c| c == "http/config/promote"));

        // Already reverted by the time it is seen back: say that.
        let mut ha = FakeHa::new();
        ha.revert_after = Duration::ZERO;
        ha.restart_takes = Duration::from_millis(30);
        let (sup, _) = serve(ha).await;
        let outcome = allow_forwarded_requests(&sup, &scaled(), |_| {}, || async { Ok(200) }).await;
        let AllowOutcome::Failed(why) = outcome else { panic!("{outcome:?}") };
        assert!(why.contains("went back to the previous setting before the add-on could confirm"), "{why}");
    }

    #[test]
    fn revert_at_is_read_as_home_assistant_writes_it() {
        let soon = (chrono::Utc::now() + chrono::Duration::seconds(120)).format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string();
        let at = revert_instant(&json!({"revert_at": soon})).unwrap();
        let left = at.saturating_duration_since(tokio::time::Instant::now());
        assert!(left > Duration::from_secs(115) && left <= Duration::from_secs(120), "{left:?}");
        let past = revert_instant(&json!({"revert_at": "2020-01-01T00:00:00+00:00"})).unwrap();
        assert!(past <= tokio::time::Instant::now());
        assert_eq!(revert_instant(&json!({"revert_at": null})), None);
        assert_eq!(revert_instant(&json!({"revert_at": "soon"})), None);
    }

    /// Someone's change waits in Settings → System → Network: untouched.
    #[tokio::test]
    async fn an_existing_pending_change_is_left_alone() {
        let mut ha = FakeHa::new();
        ha.pending = Some(json!({"server_port": 8124, "error": null}));
        let (sup, fake) = serve(ha).await;
        let outcome = allow_forwarded_requests(&sup, &fast(), |_| {}, || async { Ok(200) }).await;
        assert_eq!(outcome, AllowOutcome::PendingByOther);
        let f = fake.lock().unwrap();
        assert_eq!(f.commands, ["http/config"]);
        assert_eq!(f.pending, Some(json!({"server_port": 8124, "error": null})));
    }

    #[tokio::test]
    async fn already_allowed_changes_nothing() {
        let mut ha = FakeHa::new();
        ha.stable = merged(&stable()).unwrap();
        let (sup, fake) = serve(ha).await;
        let outcome = allow_forwarded_requests(&sup, &fast(), |_| {}, || async { Ok(200) }).await;
        assert_eq!(outcome, AllowOutcome::AlreadyAllowed);
        assert_eq!(fake.lock().unwrap().commands, ["http/config"]);
    }

    /// The token goes to the Supervisor in the auth message, and nowhere
    /// else: not in a command, and not in an error.
    #[tokio::test]
    async fn the_token_stays_in_the_auth_message() {
        let (sup, fake) = serve(FakeHa::new()).await;
        let _ = allow_forwarded_requests(&sup, &fast(), |_| {}, || async { Ok(200) }).await;
        {
            let f = fake.lock().unwrap();
            assert!(!f.frames.is_empty());
            assert!(f.frames.iter().all(|t| !t.contains(SUP_TOKEN)), "{:?}", f.frames);
        }

        let bad = Supervisor::new(sup.base().to_string(), SecretString::from("wrong-token-SECRET")).unwrap();
        let err = HaWebsocket::connect(&bad, Duration::from_secs(5)).await.err().unwrap();
        assert!(matches!(err, WsError::Auth(_)), "{err:?}");
        assert!(!err.to_string().contains("wrong-token-SECRET"));
    }

    #[tokio::test]
    async fn startup_dismisses_the_notification_and_reads_the_setting() {
        let (sup, fake) = serve(FakeHa::new()).await;
        let check = startup_check(&sup, Duration::from_secs(5)).await;
        assert_eq!(check.dismissed, Ok(()));
        assert!(matches!(check.proxies, Some(ProxySetup::Needed { .. })));
        assert_eq!(fake.lock().unwrap().dismissals, 1);
        // Again on the next start: idempotent, and still best-effort.
        let check = startup_check(&sup, Duration::from_secs(5)).await;
        assert_eq!(check.dismissed, Ok(()));
        assert_eq!(fake.lock().unwrap().dismissals, 2);

        // Nothing answering: nothing fails, and nothing is known.
        let gone = Supervisor::new("http://127.0.0.1:9", SecretString::from(SUP_TOKEN)).unwrap();
        let check = startup_check(&gone, Duration::from_secs(2)).await;
        assert!(check.dismissed.is_err());
        assert_eq!(check.proxies, None);
    }
}
