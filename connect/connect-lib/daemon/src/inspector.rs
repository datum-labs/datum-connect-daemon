//! Per-tunnel HTTP traffic inspector — a local reverse-proxy hop inserted
//! between the tunnel and the user's real local target, so requests/responses
//! can be captured for later viewing/replay without touching iroh-proxy-utils
//! (which owns the actual tunnel byte-forwarding and lives outside this repo).
//!
//! Capture is a streaming tee, not a buffering gate: bodies of any size are
//! forwarded in full, with only the first `CAPTURE_CAP_BYTES` bytes retained
//! for display. This matters — a tunnel can carry multi-GB transfers, and
//! this must never try to hold one fully in memory to "record" it.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use http_body::{Body as HttpBody, Frame, SizeHint};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Bodies are captured up to this many bytes; anything past it is still
/// forwarded, just not retained for display.
pub const CAPTURE_CAP_BYTES: usize = 64 * 1024;
/// Oldest exchanges are evicted past this count, per tunnel.
const MAX_EXCHANGES_PER_TUNNEL: usize = 50;

#[derive(Clone, Serialize)]
pub struct CapturedMessage {
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub body_truncated: bool,
}

#[derive(Clone, Serialize)]
pub struct CapturedExchange {
    pub id: String,
    pub timestamp_unix_ms: u128,
    pub method: String,
    pub path: String,
    pub request: CapturedMessage,
    pub response_status: Option<u16>,
    pub response: Option<CapturedMessage>,
}

#[derive(Serialize)]
pub struct ReplayResult {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

#[derive(Serialize)]
pub struct ExchangeSummary {
    pub id: String,
    pub timestamp_unix_ms: u128,
    pub method: String,
    pub path: String,
    pub response_status: Option<u16>,
}

impl From<&CapturedExchange> for ExchangeSummary {
    fn from(e: &CapturedExchange) -> Self {
        Self {
            id: e.id.clone(),
            timestamp_unix_ms: e.timestamp_unix_ms,
            method: e.method.clone(),
            path: e.path.clone(),
            response_status: e.response_status,
        }
    }
}

struct CaptureSink {
    buf: Vec<u8>,
    truncated: bool,
}

impl CaptureSink {
    fn new() -> Self {
        Self { buf: Vec::new(), truncated: false }
    }

    fn push(&mut self, data: &[u8]) {
        if self.truncated {
            return;
        }
        let remaining = CAPTURE_CAP_BYTES.saturating_sub(self.buf.len());
        let take = remaining.min(data.len());
        self.buf.extend_from_slice(&data[..take]);
        if data.len() > take {
            self.truncated = true;
        }
    }

    fn into_message(self, headers: Vec<(String, String)>) -> CapturedMessage {
        CapturedMessage {
            headers,
            body: String::from_utf8_lossy(&self.buf).into_owned(),
            body_truncated: self.truncated,
        }
    }
}

/// Wraps a body, copying up to `CAPTURE_CAP_BYTES` of its data frames into a
/// shared sink as they pass through, while forwarding every frame unchanged
/// regardless of the cap — capture never gates or slows the real transfer.
struct TeeBody<B> {
    inner: B,
    sink: Arc<Mutex<CaptureSink>>,
    /// Fired exactly once when the body genuinely finishes (end of stream or
    /// an error) — lets a caller wait for real completion instead of
    /// guessing with a fixed delay before snapshotting the capture. `None`
    /// when no one needs to know (e.g. the request-side tee, which is
    /// unwrapped synchronously right after the upstream call resolves).
    on_done: Option<oneshot::Sender<()>>,
}

impl<B> HttpBody for TeeBody<B>
where
    B: HttpBody<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<<Self as HttpBody>::Data>, <Self as HttpBody>::Error>>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.inner).poll_frame(cx);
        match &poll {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.sink.lock().unwrap().push(data);
                }
            }
            Poll::Ready(None) | Poll::Ready(Some(Err(_))) => {
                if let Some(tx) = this.on_done.take() {
                    let _ = tx.send(());
                }
            }
            Poll::Pending => {}
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Header names whose *value* is a credential, never the request/response
/// content the inspector exists to show — captured and displayed verbatim
/// otherwise, these would sit in plaintext for as long as the exchange stays
/// in the ring buffer, readable by anyone holding a viewer token. Redacting
/// here (at capture time, not just at display time) means the real value
/// never exists anywhere past this point — not in memory, not in a future
/// on-disk persistence of exchanges, not in a debug log that dumps a
/// `CapturedExchange`.
const SENSITIVE_HEADERS: &[&str] =
    &["authorization", "proxy-authorization", "cookie", "set-cookie", "x-api-key", "x-auth-token"];

fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| {
            let name = k.to_string();
            let value = if SENSITIVE_HEADERS.contains(&name.as_str()) {
                "[redacted]".to_string()
            } else {
                v.to_str().unwrap_or("<binary>").to_string()
            };
            (name, value)
        })
        .collect()
}

fn new_id() -> String {
    use rand::Rng;
    let n: u64 = rand::rng().random();
    format!("ex-{n:016x}")
}

struct InspectorState {
    real_target: Uri,
    client: Client<HttpConnector, TeeBody<Body>>,
    /// Separate client for the upgrade path (`proxy_upgrade`), which sends a
    /// plain empty body rather than a `TeeBody<Body>` — hyper-util's legacy
    /// `Client` is generic over its request body type, so it can't share one
    /// instance across both body types.
    plain_client: Client<HttpConnector, http_body_util::Empty<Bytes>>,
    exchanges: Mutex<VecDeque<CapturedExchange>>,
    /// JoinHandles for spawned upgrade (WebSocket) relay tasks — see
    /// `proxy_upgrade` and `InspectorHandle`'s Drop impl, which aborts all
    /// of these so an active relay doesn't outlive its tunnel.
    relay_tasks: Mutex<Vec<JoinHandle<()>>>,
}

/// Rewrites the outbound request's Host header to match the real target's
/// own authority, instead of leaving the tunnel's public hostname on it.
/// Many local dev servers (Vite, Next.js, Django, ...) validate Host/Origin
/// against an allow-list and reject requests that still carry the public
/// tunnel hostname.
pub fn set_host_header(headers: &mut axum::http::HeaderMap, target_authority: &str) {
    if let Ok(value) = axum::http::HeaderValue::from_str(target_authority) {
        headers.insert(axum::http::header::HOST, value);
    }
}

fn is_upgrade_request(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase().contains("upgrade"))
        .unwrap_or(false)
}

async fn proxy_handler(
    State(state): State<Arc<InspectorState>>,
    req: Request,
) -> Result<Response, (StatusCode, String)> {
    let method = req.method().clone();
    let path = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/").to_string();
    let req_headers = header_pairs(req.headers());

    let mut target_parts = state.real_target.clone().into_parts();
    target_parts.path_and_query = req.uri().path_and_query().cloned();
    let target_uri = Uri::from_parts(target_parts)
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("bad upstream uri: {e}")))?;
    let target_authority = target_uri.authority().map(|a| a.as_str().to_string()).unwrap_or_default();

    // WebSocket/upgrade traffic can't go through the normal capture-and-
    // forward path below — an upgraded connection is a raw duplex byte
    // stream, not a request/response exchange, so it's relayed (not
    // captured) via its own path instead of silently breaking.
    if is_upgrade_request(req.headers()) {
        return proxy_upgrade(state, req, target_uri, target_authority).await;
    }

    let (parts, body) = req.into_parts();
    let req_sink = Arc::new(Mutex::new(CaptureSink::new()));
    let (req_done_tx, req_done_rx) = oneshot::channel();
    let tee_req_body = TeeBody { inner: body, sink: req_sink.clone(), on_done: Some(req_done_tx) };

    let mut out_req = axum::http::Request::new(tee_req_body);
    *out_req.method_mut() = parts.method.clone();
    *out_req.uri_mut() = target_uri;
    *out_req.headers_mut() = parts.headers.clone();
    set_host_header(out_req.headers_mut(), &target_authority);

    let upstream_result = state.client.request(out_req).await;

    let upstream = match upstream_result {
        Ok(resp) => resp,
        Err(e) => {
            // The request body's true completion (or lack thereof) is only
            // knowable once TeeBody's on_done fires — awaiting it here
            // instead of assuming the body finished avoids the same
            // mid-transfer-snapshot race this pass already fixed on the
            // response side. A dropped sender (body never reached EOF)
            // resolves this as an error rather than hanging.
            let state_for_finish = state.clone();
            let method_for_finish = method.to_string();
            let path_for_finish = path.clone();
            tokio::spawn(async move {
                let _ = req_done_rx.await;
                let sink = req_sink.lock().unwrap();
                let request_captured = CaptureSink { buf: sink.buf.clone(), truncated: sink.truncated }
                    .into_message(req_headers);
                record_exchange(&state_for_finish, method_for_finish, path_for_finish, request_captured, None, None);
            });
            return Err((StatusCode::BAD_GATEWAY, format!("upstream error: {e}")));
        }
    };

    let status = upstream.status();
    let resp_headers = header_pairs(upstream.headers());
    let resp_sink = Arc::new(Mutex::new(CaptureSink::new()));
    let (resp_done_tx, resp_done_rx) = oneshot::channel();
    let tee_resp_body =
        TeeBody { inner: upstream.into_body(), sink: resp_sink.clone(), on_done: Some(resp_done_tx) };

    let mut out_resp = Response::new(Body::new(tee_resp_body));
    *out_resp.status_mut() = status;

    // Record once BOTH the request body and the response body have
    // genuinely finished streaming (each signaled by its own TeeBody's
    // on_done) rather than guessing with a fixed delay or assuming
    // completion the moment headers arrive on either side.
    let state_for_finish = state.clone();
    let method_for_finish = method.to_string();
    let path_for_finish = path.clone();
    tokio::spawn(async move {
        let _ = req_done_rx.await;
        let request_captured = {
            let sink = req_sink.lock().unwrap();
            CaptureSink { buf: sink.buf.clone(), truncated: sink.truncated }.into_message(req_headers)
        };
        let _ = resp_done_rx.await;
        let response_captured = {
            let sink = resp_sink.lock().unwrap();
            CaptureSink { buf: sink.buf.clone(), truncated: sink.truncated }.into_message(resp_headers)
        };
        record_exchange(
            &state_for_finish,
            method_for_finish,
            path_for_finish,
            request_captured,
            Some(status.as_u16()),
            Some(response_captured),
        );
    });

    Ok(out_resp)
}

/// Relays an upgraded (e.g. WebSocket) connection as a raw byte splice
/// between the caller and the real target, bypassing capture entirely —
/// there's no meaningful request/response exchange to record once a
/// connection has upgraded, and any attempt to buffer/parse the resulting
/// duplex stream as HTTP would just corrupt it.
async fn proxy_upgrade(
    state: Arc<InspectorState>,
    mut req: Request,
    target_uri: Uri,
    target_authority: String,
) -> Result<Response, (StatusCode, String)> {
    let client_upgrade = hyper::upgrade::on(&mut req);

    let (mut parts, _body) = req.into_parts();
    parts.uri = target_uri;
    set_host_header(&mut parts.headers, &target_authority);
    let out_req = axum::http::Request::from_parts(parts, http_body_util::Empty::<Bytes>::new());

    let mut upstream = state
        .plain_client
        .request(out_req)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("upstream error: {e}")))?;

    if upstream.status() != StatusCode::SWITCHING_PROTOCOLS {
        // Real target declined the upgrade — relay its response as-is,
        // headers included (Content-Type, WWW-Authenticate, CORS, etc. all
        // matter for a caller trying to interpret why the upgrade failed).
        let status = upstream.status();
        let headers = upstream.headers().clone();
        let body_bytes = http_body_util::BodyExt::collect(upstream.into_body())
            .await
            .map(|c| c.to_bytes())
            .unwrap_or_default();
        let mut resp = Response::new(Body::from(body_bytes));
        *resp.status_mut() = status;
        *resp.headers_mut() = headers;
        return Ok(resp);
    }

    let status = upstream.status();
    let resp_headers = upstream.headers().clone();
    let server_upgrade = hyper::upgrade::on(&mut upstream);

    let relay_task = tokio::spawn(async move {
        match tokio::try_join!(client_upgrade, server_upgrade) {
            Ok((client_io, server_io)) => {
                let mut client_io = hyper_util::rt::TokioIo::new(client_io);
                let mut server_io = hyper_util::rt::TokioIo::new(server_io);
                if let Err(e) = tokio::io::copy_bidirectional(&mut client_io, &mut server_io).await {
                    tracing::debug!("upgraded connection closed: {e:#}");
                }
            }
            Err(e) => tracing::warn!("upgrade handshake failed: {e:#}"),
        }
    });
    // Tracked so InspectorHandle's Drop can abort it — otherwise an active
    // WebSocket relay would keep running (and keep proxying traffic) even
    // after the tunnel it belongs to is deleted.
    state.relay_tasks.lock().unwrap().push(relay_task);

    let mut resp = Response::new(Body::empty());
    *resp.status_mut() = status;
    *resp.headers_mut() = resp_headers;
    Ok(resp)
}

fn record_exchange(
    state: &InspectorState,
    method: String,
    path: String,
    request: CapturedMessage,
    response_status: Option<u16>,
    response: Option<CapturedMessage>,
) {
    let exchange = CapturedExchange {
        id: new_id(),
        timestamp_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis(),
        method,
        path,
        request,
        response_status,
        response,
    };
    let mut exchanges = state.exchanges.lock().unwrap();
    exchanges.push_front(exchange);
    exchanges.truncate(MAX_EXCHANGES_PER_TUNNEL);
}

pub struct InspectorHandle {
    pub local_addr: std::net::SocketAddr,
    state: Arc<InspectorState>,
    task: JoinHandle<()>,
}

impl Drop for InspectorHandle {
    fn drop(&mut self) {
        self.task.abort();
        for relay in self.state.relay_tasks.lock().unwrap().drain(..) {
            relay.abort();
        }
    }
}

impl InspectorHandle {
    pub fn list(&self) -> Vec<ExchangeSummary> {
        self.state.exchanges.lock().unwrap().iter().map(ExchangeSummary::from).collect()
    }

    pub fn get(&self, id: &str) -> Option<CapturedExchange> {
        self.state.exchanges.lock().unwrap().iter().find(|e| e.id == id).cloned()
    }

    pub fn real_target(&self) -> &Uri {
        &self.state.real_target
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    #[test]
    fn header_pairs_redacts_sensitive_values_only() {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::AUTHORIZATION, "Bearer super-secret".parse().unwrap());
        headers.insert(axum::http::header::COOKIE, "session=super-secret".parse().unwrap());
        headers.insert("x-request-id", "abc123".parse().unwrap());

        let pairs = header_pairs(&headers);
        let get = |name: &str| pairs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());

        assert_eq!(get("authorization"), Some("[redacted]"));
        assert_eq!(get("cookie"), Some("[redacted]"));
        assert_eq!(get("x-request-id"), Some("abc123"));
    }

    /// Real end-to-end proof, not just the pure-function unit test above:
    /// a genuine secret header sent through the actual inspector proxy path
    /// (1) never appears in what gets captured/would be shown to a viewer,
    /// while (2) still reaching the real local target unredacted — this fix
    /// must not silently break the caller's actual auth to their own app.
    #[tokio::test]
    async fn secret_header_is_redacted_in_capture_but_still_reaches_real_target() {
        // A bare-bones local target: read one HTTP/1.1 request, remember
        // whether it carried the real Authorization value, reply 200.
        let target_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target_listener.local_addr().unwrap();
        let target_task = tokio::task::spawn_blocking(move || {
            let listener = std::net::TcpListener::from(target_listener.into_std().unwrap());
            // tokio hands back the underlying socket in non-blocking mode —
            // needs to go back to blocking before using std's blocking accept/read.
            listener.set_nonblocking(false).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            let received = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = b"ok";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(resp.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
            received
        });

        let real_target: Uri = format!("http://{target_addr}").parse().unwrap();
        let handle = start(real_target).await.expect("inspector should start");
        let inspector_addr = handle.local_addr;

        let secret = "Bearer this-is-the-real-secret";
        tokio::task::spawn_blocking(move || {
            let mut stream = TcpStream::connect(inspector_addr).unwrap();
            let req = format!(
                "GET / HTTP/1.1\r\nHost: {inspector_addr}\r\nAuthorization: {secret}\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(req.as_bytes()).unwrap();
            let mut resp = Vec::new();
            let _ = stream.read_to_end(&mut resp);
        })
        .await
        .unwrap();

        let received_by_target = target_task.await.unwrap();
        assert!(
            received_by_target.contains("this-is-the-real-secret"),
            "the real target must still receive the real credential — redaction is capture-only"
        );

        let exchanges = handle.list();
        assert_eq!(exchanges.len(), 1);
        let exchange = handle.get(&exchanges[0].id).expect("exchange should be retrievable");
        let auth_header = exchange
            .request
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.as_str());
        assert_eq!(
            auth_header,
            Some("[redacted]"),
            "the captured/displayable copy must never contain the real secret"
        );
    }
}

/// Starts a new inspector bound to an OS-assigned loopback port, forwarding
/// to `real_target`. Returns immediately once the listener is bound.
pub async fn start(real_target: Uri) -> n0_error::Result<InspectorHandle> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| n0_error::anyerr!("inspector: failed to bind local port: {e}"))?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| n0_error::anyerr!("inspector: failed to read local addr: {e}"))?;

    let client: Client<HttpConnector, TeeBody<Body>> =
        Client::builder(TokioExecutor::new()).build_http();
    let plain_client: Client<HttpConnector, http_body_util::Empty<Bytes>> =
        Client::builder(TokioExecutor::new()).build_http();
    let state = Arc::new(InspectorState {
        real_target,
        client,
        plain_client,
        exchanges: Mutex::new(VecDeque::new()),
        relay_tasks: Mutex::new(Vec::new()),
    });

    let app = axum::Router::new().fallback(proxy_handler).with_state(state.clone());
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!("inspector server exited: {e:#}");
        }
    });

    Ok(InspectorHandle { local_addr, state, task })
}
