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
#[derive(Debug, Clone)]
pub struct AllowTiming {
    /// For Home Assistant to start restarting after it took the setting.
    pub restart_begins: Duration,
    /// For it to come back with the setting on trial. Home Assistant
    /// reverts an unconfirmed setting five minutes after it starts, so this
    /// stays under that.
    pub restart_ends: Duration,
    pub poll: Duration,
    /// Per websocket call.
    pub call: Duration,
}

impl Default for AllowTiming {
    fn default() -> Self {
        Self {
            restart_begins: Duration::from_secs(90),
            restart_ends: Duration::from_secs(4 * 60),
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
            AllowStep::Restarting => "Home Assistant is restarting with the new setting…",
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

const REVERTS: &str = "Home Assistant goes back to the previous setting by itself within 5 minutes.";

/// Turns on `use_x_forwarded_for` with the loopback addresses as trusted
/// proxies, the way Settings → System → Network does: save it as pending,
/// let Home Assistant restart on it, check that a request through the
/// add-on's local hop with `X-Forwarded-For` is no longer refused
/// (`verify`), and only then confirm it. On any failure it is not
/// confirmed, and Home Assistant's own revert restores the old setting.
///
/// A pending change made by someone else is never touched.
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
    let config = match assess(&current) {
        Ok(ProxySetup::Ready) => return AllowOutcome::AlreadyAllowed,
        Ok(ProxySetup::Pending) => return AllowOutcome::PendingByOther,
        Ok(ProxySetup::Needed { config }) => config,
        Err(e) => return Failed(format!("Home Assistant's network settings could not be read, so nothing was changed: {e}.")),
    };

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
    if !ws.wait_closed(timing.restart_begins).await {
        ws.close().await;
        return Failed(format!(
            "Home Assistant saved the setting but did not restart within {} seconds, so it was not confirmed. Restart Home Assistant, then confirm or discard the change in Settings → System → Network.",
            timing.restart_begins.as_secs()
        ));
    }

    // Back up, on trial with the new setting. The old instance can still
    // answer while it shuts down, which is why the active slot is checked
    // rather than just "it answers".
    let deadline = tokio::time::Instant::now() + timing.restart_ends;
    let mut ws = loop {
        if tokio::time::Instant::now() >= deadline {
            return Failed(format!(
                "Home Assistant did not come back with the new setting within {} minutes, so it was not confirmed. {REVERTS}",
                timing.restart_ends.as_secs().div_ceil(60)
            ));
        }
        if core_api_up(sup).await
            && let Ok(mut ws) = HaWebsocket::connect(sup, timing.call).await
        {
            match ws.command(json!({"type": "http/config"})).await {
                Ok(c) if c["active_config_type"] == "pending" && live_pending(&c).is_some() => break ws,
                Ok(c) if c.pointer("/pending/error").is_some_and(|e| !e.is_null()) => {
                    ws.close().await;
                    let why = c.pointer("/pending/error_message").and_then(Value::as_str).unwrap_or_default();
                    return Failed(format!(
                        "Home Assistant could not use the new setting and went back to the previous one{}.",
                        if why.is_empty() { String::new() } else { format!(": {why}") }
                    ));
                }
                _ => ws.close().await,
            }
        }
        tokio::time::sleep(timing.poll).await;
    };

    step(AllowStep::Verifying);
    match verify().await {
        Ok(400) => {
            ws.close().await;
            return Failed(format!(
                "A request sent the way Datum sends it still got 400: Bad Request, so the setting was not confirmed. {REVERTS}"
            ));
        }
        Ok(_) => {}
        Err(e) => {
            ws.close().await;
            return Failed(format!("Could not check a request the way Datum sends it ({e}), so the setting was not confirmed. {REVERTS}"));
        }
    }

    step(AllowStep::Confirming);
    let promoted = ws.command(json!({"type": "http/config/promote"})).await;
    ws.close().await;
    match promoted {
        Ok(_) => AllowOutcome::Allowed,
        Err(e) => Failed(format!("Could not confirm the setting: {e}. {REVERTS}")),
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
        /// API polls left that answer 502 after a restart began.
        down_for: Option<u32>,
        restart_polls: u32,
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
            Self { stable: stable(), active: "stable", restart_polls: 3, apply_ok: true, restarts: true, ..Default::default() }
        }

        fn config(&self) -> Value {
            json!({"stable": self.stable, "pending": self.pending, "revert_at": null,
                   "active_config_type": self.active, "default": {}})
        }

        fn api_poll(&mut self) -> bool {
            match self.down_for {
                Some(0) => {
                    self.down_for = None;
                    if self.apply_ok {
                        self.active = "pending";
                    } else if let Some(p) = self.pending.as_mut() {
                        p["error"] = json!("apply_failed");
                        p["error_message"] = json!("cannot bind");
                    }
                    true
                }
                Some(n) => {
                    self.down_for = Some(n - 1);
                    false
                }
                None => true,
            }
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
                    let mut pending = config;
                    pending["created_at"] = json!("2026-10-07T12:00:00+00:00");
                    pending["error"] = Value::Null;
                    pending["error_message"] = Value::Null;
                    self.pending = Some(pending);
                    if !self.restarts {
                        return (ok(json!({"restart": true})), false);
                    }
                    self.down_for = Some(self.restart_polls);
                    (ok(json!({"restart": true})), true)
                }
                "http/config/promote" => match self.pending.take() {
                    Some(p) => {
                        self.stable = p;
                        self.active = "stable";
                        (ok(Value::Null), false)
                    }
                    None => (err("not_allowed", "No pending HTTP config to promote"), false),
                },
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
        if !line.starts_with("GET /core/websocket ") || fake.lock().unwrap().down_for.is_some() {
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
        ha.restart_polls = u32::MAX;
        let (sup, fake) = serve(ha).await;
        let mut t = fast();
        t.restart_ends = Duration::from_millis(500);
        let outcome = allow_forwarded_requests(&sup, &t, |_| {}, || async { Ok(200) }).await;
        assert!(matches!(&outcome, AllowOutcome::Failed(w) if w.contains("did not come back")), "{outcome:?}");
        assert!(!fake.lock().unwrap().commands.iter().any(|c| c == "http/config/promote"));
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
