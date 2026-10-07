//! The Home Assistant add-on owns exactly one tunnel: the one its
//! `tunnel_label` names. Changing the label makes a new tunnel, which is
//! right, but the old one is still on file here (its real target and its
//! listen key), so without this the daemon's startup reconciliation brings
//! it back and resumes it, and both stay publicly online.
//!
//! The add-on opts in with `DATUM_TUNNEL_EXCLUSIVE_LABEL`. The daemon then
//! resolves the label the way the add-on's run.sh does (the first tunnel
//! with that label, in the order `GET /v1/tunnels` lists them), and every
//! other tunnel this daemon has local state for is "older": not resumed at
//! start, stopped if it is on, and offered for removal on the page. A tunnel
//! this daemon has no local state for belongs to another machine in a
//! shared project and is never touched.
//!
//! Why a label at start rather than an id or a call after start: run.sh only
//! learns ids from the daemon's own API, which listens after reconciliation
//! has already resumed the old tunnel, so an id could only arrive once the
//! old tunnel was back online, and a call racing the resume could lose.

use std::path::Path;

use connect_lib::{Repo, TunnelSummary};

/// The tunnels as the add-on sees them.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Partition {
    /// The tunnel the label names, if it exists yet.
    pub active: Option<TunnelSummary>,
    /// Every other tunnel this daemon has local state for.
    pub older: Vec<TunnelSummary>,
}

/// Splits `tunnels` (in list order) by `label`. `is_local` says whether
/// this daemon has local state for a tunnel id.
pub(crate) fn partition(tunnels: Vec<TunnelSummary>, label: &str, is_local: impl Fn(&str) -> bool) -> Partition {
    let mut out = Partition::default();
    for t in tunnels {
        if out.active.is_none() && t.label == label {
            out.active = Some(t);
        } else if is_local(&t.id) {
            out.older.push(t);
        }
    }
    out
}

/// Whether this daemon has local state for the tunnel: a persisted real
/// target (`daemon_inspector_targets/<id>.txt`) or a listen key
/// (`<project>/<id>/listen_key`).
pub(crate) fn is_locally_known(connect_dir: &Path, project: &str, id: &str) -> bool {
    // Ids are Kubernetes names; anything else is not a path to look at.
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') || id.starts_with('.') {
        return false;
    }
    crate::inspector_target_path(connect_dir, id).is_file()
        || connect_dir.join(project).join(id).join(Repo::LISTEN_KEY_FILE).is_file()
}

/// The log line for an older tunnel that was turned off.
pub(crate) fn stopped_line(t: &TunnelSummary) -> String {
    format!(
        "Stopped older tunnel '{}' ({}, {}); remove it from the Datum Connect page.",
        t.label,
        t.id,
        t.hostnames.first().map(String::as_str).unwrap_or("no address")
    )
}

/// The log line for an older tunnel that was already off.
pub(crate) fn already_off_line(t: &TunnelSummary) -> String {
    format!(
        "Older tunnel '{}' ({}, {}) is off; remove it from the Datum Connect page.",
        t.label,
        t.id,
        t.hostnames.first().map(String::as_str).unwrap_or("no address")
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn tunnel(id: &str, label: &str) -> TunnelSummary {
        TunnelSummary {
            id: id.into(),
            label: label.into(),
            endpoint: "http://127.0.0.1:1".into(),
            hostnames: vec![format!("{id}.datumproxy.net")],
            enabled: true,
            accepted: true,
            programmed: true,
            connector_metadata_programmed: true,
            connector_ready: true,
            connector_name: None,
            connector_device: None,
            created_at: None,
        }
    }

    fn ids(ts: &[TunnelSummary]) -> Vec<&str> {
        ts.iter().map(|t| t.id.as_str()).collect()
    }

    /// After a label change: the new label's tunnel is active, the old one
    /// is older, and a tunnel from another machine is neither.
    #[test]
    fn only_locally_known_tunnels_are_older() {
        let tunnels = vec![
            tunnel("tunnel-aaa", "home-assistant"),
            tunnel("tunnel-bbb", "other-machine"),
            tunnel("tunnel-ccc", "ha-2"),
        ];
        let local = ["tunnel-aaa", "tunnel-ccc"];
        let p = partition(tunnels, "ha-2", |id| local.contains(&id));
        assert_eq!(p.active.as_ref().map(|t| t.id.as_str()), Some("tunnel-ccc"));
        assert_eq!(ids(&p.older), ["tunnel-aaa"]);
    }

    /// The new label's tunnel isn't made yet: every local one is older,
    /// and still nothing else.
    #[test]
    fn before_the_new_tunnel_exists_every_local_one_is_older() {
        let tunnels = vec![tunnel("tunnel-aaa", "home-assistant"), tunnel("tunnel-bbb", "other-machine")];
        let p = partition(tunnels, "ha-2", |id| id == "tunnel-aaa");
        assert_eq!(p.active, None);
        assert_eq!(ids(&p.older), ["tunnel-aaa"]);
    }

    /// Two with the same label: the first is the add-on's, as run.sh picks
    /// it (`map(select(.label == $l)) | first`); the second, if local, is
    /// older. An active tunnel needs no local state (one adopted after a
    /// reinstall has none yet).
    #[test]
    fn the_first_with_the_label_wins_and_needs_no_local_state() {
        let tunnels = vec![tunnel("tunnel-aaa", "ha"), tunnel("tunnel-bbb", "ha"), tunnel("tunnel-ccc", "ha")];
        let p = partition(tunnels, "ha", |id| id != "tunnel-aaa" && id != "tunnel-ccc");
        assert_eq!(p.active.as_ref().map(|t| t.id.as_str()), Some("tunnel-aaa"));
        assert_eq!(ids(&p.older), ["tunnel-bbb"]);
    }

    #[test]
    fn nothing_local_means_nothing_older() {
        let tunnels = vec![tunnel("tunnel-aaa", "x"), tunnel("tunnel-bbb", "y")];
        let p = partition(tunnels, "ha", |_| false);
        assert_eq!(p, Partition::default());
    }

    #[test]
    fn local_state_is_a_target_or_a_listen_key() {
        let dir = std::env::temp_dir().join(format!("dcd-exclusive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("daemon_inspector_targets")).unwrap();
        std::fs::write(dir.join("daemon_inspector_targets").join("tunnel-aaa.txt"), "http://127.0.0.1:8123").unwrap();
        std::fs::create_dir_all(dir.join("p-1").join("tunnel-bbb")).unwrap();
        std::fs::write(dir.join("p-1").join("tunnel-bbb").join("listen_key"), [0u8; 32]).unwrap();
        // A key under another project is not this project's state.
        std::fs::create_dir_all(dir.join("p-2").join("tunnel-ccc")).unwrap();
        std::fs::write(dir.join("p-2").join("tunnel-ccc").join("listen_key"), [0u8; 32]).unwrap();
        assert!(is_locally_known(&dir, "p-1", "tunnel-aaa"));
        assert!(is_locally_known(&dir, "p-1", "tunnel-bbb"));
        assert!(!is_locally_known(&dir, "p-1", "tunnel-ccc"));
        assert!(!is_locally_known(&dir, "p-1", "tunnel-ddd"));
        assert!(!is_locally_known(&dir, "p-1", "../p-2/tunnel-ccc"));
        assert!(!is_locally_known(&dir, "p-1", ""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_lines_name_the_tunnel() {
        let t = tunnel("tunnel-aaa", "home-assistant");
        assert_eq!(
            stopped_line(&t),
            "Stopped older tunnel 'home-assistant' (tunnel-aaa, tunnel-aaa.datumproxy.net); remove it from the Datum Connect page."
        );
        let mut t = t;
        t.hostnames.clear();
        assert!(already_off_line(&t).contains("(tunnel-aaa, no address) is off"));
    }
}
