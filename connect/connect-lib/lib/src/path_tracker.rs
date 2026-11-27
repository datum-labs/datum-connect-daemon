use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iroh::endpoint::{AfterHandshakeOutcome, Connection, EndpointHooks, WeakConnectionHandle};
use iroh::{EndpointId, RelayUrl, TransportAddr};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathKind {
    Direct(SocketAddr),
    Relay(RelayUrl),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerPath {
    pub kind: PathKind,
    pub rtt: Duration,
}

#[derive(Clone, Debug, Default)]
pub struct PathTracker {
    connections: Arc<Mutex<HashMap<EndpointId, Vec<WeakConnectionHandle>>>>,
}

impl PathTracker {
    pub fn selected_path(&self, remote_id: EndpointId) -> Option<PeerPath> {
        let handles = self.connections.lock().ok()?.get(&remote_id)?.clone();
        handles
            .iter()
            .rev()
            .filter_map(|handle| handle.upgrade())
            .filter(|conn| conn.close_reason().is_none())
            .find_map(|conn| selected_path_of(&conn))
    }
}

fn selected_path_of(conn: &Connection) -> Option<PeerPath> {
    let paths = conn.paths();
    let path = paths.iter().find(|p| p.is_selected())?;
    let kind = match path.remote_addr() {
        TransportAddr::Ip(addr) => PathKind::Direct(*addr),
        TransportAddr::Relay(url) => PathKind::Relay(url.clone()),
        _ => return None,
    };
    Some(PeerPath {
        kind,
        rtt: path.rtt(),
    })
}

impl EndpointHooks for PathTracker {
    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        if let Ok(mut map) = self.connections.lock() {
            let handles = map.entry(conn.remote_id()).or_default();
            handles.retain(|h| h.upgrade().is_some_and(|c| c.close_reason().is_none()));
            handles.push(conn.weak_handle());
        }
        AfterHandshakeOutcome::accept()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::endpoint::{RelayMode, presets};
    use iroh::{Endpoint, SecretKey};

    const ALPN: &[u8] = b"datum/path-tracker-test";

    #[tokio::test]
    async fn reports_selected_path_and_rtt_for_live_connection_only() {
        let server = Endpoint::builder(presets::N0)
            .relay_mode(RelayMode::Disabled)
            .secret_key(SecretKey::generate())
            .alpns(vec![ALPN.to_vec()])
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let tracker = PathTracker::default();
        let client = Endpoint::builder(presets::N0)
            .relay_mode(RelayMode::Disabled)
            .secret_key(SecretKey::generate())
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .hooks(tracker.clone())
            .bind()
            .await
            .unwrap();

        assert_eq!(tracker.selected_path(server.id()), None);

        let accept = tokio::spawn({
            let server = server.clone();
            async move {
                let conn = server.accept().await.unwrap().await.unwrap();
                conn.closed().await;
            }
        });
        let conn = client.connect(server.addr(), ALPN).await.unwrap();
        let (mut send, _recv) = conn.open_bi().await.unwrap();
        send.write_all(b"x").await.unwrap();

        let path = tracker.selected_path(server.id()).expect("live connection has a path");
        assert!(matches!(path.kind, PathKind::Direct(_)));

        conn.close(0u32.into(), b"done");
        drop(conn);
        accept.await.unwrap();
        client.close().await;
        assert_eq!(tracker.selected_path(server.id()), None);
    }
}
