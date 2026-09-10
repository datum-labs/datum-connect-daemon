use std::path::PathBuf;

use iroh::SecretKey;
use tracing::{info, instrument, warn};
use n0_error::{Result, StackResultExt, StdResultExt};

use crate::{
    config::Config,
    state::State,
};

/// Error returned by [`Repo::default_location`] when the
/// `DATUM_CONNECT_DIR` environment variable is not set.
///
/// Phase 11.5 D-09/D-10: the binary refuses to invent a default
/// location. The `Display` impl prints the multi-line directive
/// message that tells the user how to fix the situation.
#[derive(Debug, Clone)]
pub struct MissingConnectDir;

impl std::fmt::Display for MissingConnectDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(MISSING_CONNECT_DIR_MSG)
    }
}

impl std::error::Error for MissingConnectDir {}

const MISSING_CONNECT_DIR_MSG: &str = "error: DATUM_CONNECT_DIR is not set

The datum-connect binary expects this variable to point to its state
directory (where it stores the iroh listen_key, config, and per-project
state). It is normally set by the datumctl plugin host.

To run via datumctl (preferred):
  datumctl connect tunnel <subcommand> ...

To run datum-connect directly (development):
  export DATUM_CONNECT_DIR=\"$HOME/.datumctl/connect\"
  datum-connect <subcommand> ...

(exit 64)
";

// Repo builds up a series of file path conventions from a root directory path.
#[derive(Debug, Clone)]
pub struct Repo(PathBuf);

impl Repo {
    /// Create a Repo from a path without opening/creating (for sync use cases like update install).
    pub fn from_path(path: PathBuf) -> Self {
        Self(path)
    }

    const CONFIG_FILE: &str = "config.yml";
    const CONNECT_KEY_FILE: &str = "connect_key";
    const PEER_LISTEN_KEY_FILE: &str = "peer_listen_key";
    pub const LISTEN_KEY_FILE: &str = "listen_key";
    const STATE_FILE: &str = "state.yml";
    pub fn default_location() -> Result<PathBuf, MissingConnectDir> {
        match std::env::var("DATUM_CONNECT_DIR") {
            Ok(path) if !path.is_empty() => Ok(PathBuf::from(path)),
            Ok(_) | Err(_) => Err(MissingConnectDir),
        }
    }

    /// Opens or creates a repo at the given base directory.
    pub async fn open_or_create(base_dir: impl Into<PathBuf>) -> Result<Self> {
        let base_dir = base_dir.into();
        tokio::fs::create_dir_all(&base_dir).await?;
        info!("opening repo at {}", base_dir.display());

        let this = Self(base_dir);

        Ok(this)
    }

    pub async fn config(&self) -> Result<Config> {
        let config_file_path = self.0.join(Self::CONFIG_FILE);
        if !config_file_path.exists() {
            warn!("config does not exist. creating new config");
            let cfg = Config::default();
            cfg.write(config_file_path).await?;
            return Ok(cfg);
        };

        Config::from_file(config_file_path).await
    }

    pub async fn load_state(&self) -> Result<crate::StateWrapper> {
        let state_file_path = self.0.join(Self::STATE_FILE);
        let state = if !state_file_path.exists() {
            let state = State::default();
            state.write_to_file(state_file_path).await?;
            state
        } else {
            State::from_file(state_file_path).await?
        };
        Ok(crate::StateWrapper::new(state))
    }

    pub async fn write_state(&self, state: &State) -> Result<()> {
        state.write_to_file(self.0.join(Self::STATE_FILE)).await
    }

    pub async fn write_selected_context(
        &self,
        selected: Option<&crate::SelectedContext>,
    ) -> Result<()> {
        let path = self.0.join(Self::CONFIG_FILE);
        let mut config = if path.exists() {
            let data = tokio::fs::read_to_string(&path)
                .await
                .context("reading config file")?;
            serde_yml::from_str(&data).std_context("parsing config file")?
        } else {
            crate::config::Config::default()
        };
        config.selected_context = selected.cloned();
        config.write(path).await
    }

    pub async fn read_selected_context(&self) -> Result<Option<crate::SelectedContext>> {
        let path = self.0.join(Self::CONFIG_FILE);
        if path.exists() {
            let data = tokio::fs::read_to_string(path)
                .await
                .context("reading config file")?;
            let config: crate::config::Config =
                serde_yml::from_str(&data).std_context("parsing config file")?;
            return Ok(config.selected_context);
        }
        Ok(None)
    }

    pub async fn connect_key(&self) -> Result<SecretKey> {
        let key_file_path = self.0.join(Self::CONNECT_KEY_FILE);
        self.secret_key(key_file_path).await
    }

    /// Stable identity for direct app-to-app peer connections (advertising
    /// local targets and accepting inbound peer dials) — deliberately
    /// separate from `listen_key`/`listen_key_for_project`'s identities
    /// (which are for tunnels routed through Datum's gateway) so a peer's
    /// EndpointId, and thus every ticket handed out for it, stays valid
    /// across daemon restarts.
    ///
    /// **Does NOT, by itself, give peer advertisements their own
    /// `ProxyState` list** — a `ListenNode` built with this key still calls
    /// `Repo::load_state`, which loads the *same* on-disk proxy list every
    /// other `ListenNode` sharing this `Repo`/connect_dir does, regardless
    /// of which secret key it was built with. A caller relying on this key
    /// alone for isolation between peer advertisements and tunnels will be
    /// surprised — see `daemon/src/peer.rs`'s `PeerState::advertised` for
    /// where that isolation is actually enforced today (a persisted
    /// resource-id allowlist at the API layer, found necessary 2026-09-07 —
    /// see NOTES.md). A real fix belongs here, scoping `load_state` (or an
    /// equivalent) by identity/purpose rather than by connect_dir alone.
    pub async fn peer_listen_key(&self) -> Result<SecretKey> {
        let key_file_path = self.0.join(Self::PEER_LISTEN_KEY_FILE);
        self.secret_key(key_file_path).await
    }

    /// Return a fresh listen key always written to a timestamp-suffixed file
    /// (`listen_key[.<project_id>].<YYYYMMDDHHmmss>`) so a stale key from a previous
    /// `listen` is never accidentally reused. The plain `listen_key` name is only
    /// used inside per-tunnel subdirectories where the key is intentionally stable.
    pub async fn listen_key(&self, project_id: Option<&str>) -> Result<SecretKey> {
        let key = SecretKey::generate(&mut rand::rng());
        let now = chrono::Local::now().format("%Y%m%d%H%M%S");
        let suffix = match project_id {
            Some(pid) => format!("{}.{}", pid, now),
            None => now.to_string(),
        };
        let key_file_path = self.0.join(format!("{}.{}", Self::LISTEN_KEY_FILE, suffix));
        if let Some(parent) = key_file_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&key_file_path, key.to_bytes()).await?;
        Ok(key)
    }

    /// Project-scoped listen key. Each project gets its own iroh identity so
    /// Connectors registered in different projects don't collide on the iroh
    /// DNS record (the controller assigns ownership to one and leaves the
    /// others with `IrohDNSPublished=False; DeferredToOwner`, which manifests
    /// as a tunnel that reports ready but silently drops data).
    ///
    /// On first access for any project, if the legacy flat `listen_key` exists
    /// it is moved into this project's directory so the user keeps continuity
    /// with whatever Connector that key was registered as. Subsequent projects
    /// (no legacy file left) get freshly generated keys.
    pub async fn listen_key_for_project(&self, project_id: &str) -> Result<SecretKey> {
        let project_dir = self.0.join(project_id);
        let key_file_path = project_dir.join(Self::LISTEN_KEY_FILE);
        if !key_file_path.exists() {
            let legacy = self.0.join(Self::LISTEN_KEY_FILE);
            if legacy.exists() {
                tokio::fs::create_dir_all(&project_dir).await?;
                info!(
                    "migrating legacy listen_key {} -> {} for project {project_id}",
                    legacy.display(),
                    key_file_path.display(),
                );
                tokio::fs::rename(&legacy, &key_file_path).await?;
            }
        }
        self.secret_key(key_file_path).await
    }

    /// Per-tunnel listen key. Each named tunnel gets its own iroh identity so
    /// tunnels in the same project don't collide on the iroh DNS record.
    ///
    /// On first access, if a legacy flat `listen_key` exists at the repo root
    /// for this project, it is moved into `<project_id>/<tunnel_name>/listen_key`
    /// (preserving the key value for continuity with the registered Connector).
    /// Subsequent tunnels in the same project (no legacy file left) get freshly
    /// generated keys.
    /// Legacy flat key location at the repo root (same as the old
    /// `Repo::listen_key()` path).
    const LEGACY_LISTEN_KEY: &'static str = "listen_key";

    #[instrument("repo", skip_all)]
    pub async fn listen_key_for_tunnel(
        &self,
        project_id: &str,
        tunnel_name: &str,
    ) -> Result<SecretKey> {
        let tunnel_dir = self.0.join(project_id).join(tunnel_name);
        let key_file_path = tunnel_dir.join(Self::LISTEN_KEY_FILE);

        if !key_file_path.exists() {
            // Check for legacy key at repo root (the old flat layout).
            let legacy = self.0.join(Self::LEGACY_LISTEN_KEY);
            if legacy.exists() {
                tokio::fs::create_dir_all(&tunnel_dir).await?;
                info!(
                    "migrating legacy listen_key {} -> {} for project {project_id} tunnel {tunnel_name}",
                    legacy.display(),
                    key_file_path.display(),
                );
                tokio::fs::rename(&legacy, &key_file_path).await?;
            } else {
                n0_error::bail_any!("KEY_NOT_FOUND");
            }
        }

        let key = tokio::fs::read(&key_file_path).await?;
        let key = key.as_slice().try_into().anyerr()?;
        Ok(SecretKey::from_bytes(key))
    }

    /// Persist a key for a tunnel (used when regenerating a key for resume).
    pub async fn save_listen_key_for_tunnel(
        &self,
        project_id: &str,
        tunnel_name: &str,
        key: &SecretKey,
    ) -> Result<()> {
        let tunnel_dir = self.0.join(project_id).join(tunnel_name);
        let key_file_path = tunnel_dir.join(Self::LISTEN_KEY_FILE);
        tokio::fs::create_dir_all(&tunnel_dir).await?;
        tokio::fs::write(&key_file_path, key.to_bytes()).await?;
        Ok(())
    }

    async fn secret_key(&self, key_file_path: PathBuf) -> Result<SecretKey> {
        if !key_file_path.exists() {
            warn!("secret key does not exist. creating new key");
            if let Some(parent) = key_file_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            return self.create_key(&key_file_path).await;
        };

        let key = tokio::fs::read(key_file_path).await?;
        let key = key.as_slice().try_into().anyerr()?;
        Ok(SecretKey::from_bytes(key))
    }

    async fn create_key(&self, key_file_path: &PathBuf) -> Result<SecretKey> {
        let key = SecretKey::generate(&mut rand::rng());
        tokio::fs::write(key_file_path, key.to_bytes()).await?;
        Ok(key)
    }

    /// Get the base directory path of this repo
    pub fn path(&self) -> &PathBuf {
        &self.0
    }

    /// Delete the local state directory for a tunnel
    pub async fn delete_tunnel_dir(&self, project_id: &str, tunnel_name: &str) -> Result<()> {
        let tunnel_dir = self.0.join(project_id).join(tunnel_name);
        if tunnel_dir.exists() {
            tokio::fs::remove_dir_all(&tunnel_dir).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo_dir() -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("datum-repo-test-{}", uuid::Uuid::new_v4()));
        path
    }

    #[tokio::test]
    async fn listen_key_for_project_migrates_legacy_into_first_project() {
        // The legacy `listen_key` lived at the repo root and was reused for
        // every project the CLI talked to. The migration must move (not copy)
        // it into the first project that requests it, so the second project
        // gets a fresh identity instead of joining the cross-project DNS race.
        let repo = Repo::open_or_create(temp_repo_dir()).await.unwrap();
        // Create a legacy key at the plain LISTEN_KEY_FILE path (no timestamp).
        let legacy = SecretKey::generate(&mut rand::rng());
        let legacy_bytes = legacy.to_bytes();
        let legacy_path = repo.0.join(Repo::LISTEN_KEY_FILE);
        tokio::fs::write(&legacy_path, &legacy_bytes)
            .await
            .expect("should write legacy key");
        assert!(legacy_path.exists(), "precondition: legacy key exists");

        let p1 = repo.listen_key_for_project("project-a").await.unwrap();
        assert_eq!(
            p1.to_bytes(),
            legacy_bytes,
            "first project must adopt the legacy key"
        );
        assert!(!legacy_path.exists(), "legacy file must be gone after migration");
        let p1_path = repo
            .0
            .join("project-a")
            .join(Repo::LISTEN_KEY_FILE);
        assert!(p1_path.exists(), "key must now live under the project dir");

        let p2 = repo.listen_key_for_project("project-b").await.unwrap();
        assert_ne!(
            p2.to_bytes(),
            legacy_bytes,
            "second project must get a fresh key, not the legacy one"
        );
    }

    #[tokio::test]
    async fn listen_key_for_project_is_stable_across_calls() {
        let repo = Repo::open_or_create(temp_repo_dir()).await.unwrap();
        let first = repo.listen_key_for_project("project-x").await.unwrap();
        let second = repo.listen_key_for_project("project-x").await.unwrap();
        assert_eq!(
            first.to_bytes(),
            second.to_bytes(),
            "repeat calls must return the same persisted key"
        );
    }

    #[tokio::test]
    async fn listen_key_for_project_generates_fresh_without_legacy() {
        let repo = Repo::open_or_create(temp_repo_dir()).await.unwrap();
        let key = repo.listen_key_for_project("only-project").await.unwrap();
        let legacy_path = repo.0.join(Repo::LISTEN_KEY_FILE);
        assert!(!legacy_path.exists(), "no legacy must be created");
        let project_path = repo
            .0
            .join("only-project")
            .join(Repo::LISTEN_KEY_FILE);
        assert!(project_path.exists());
        assert_eq!(tokio::fs::read(&project_path).await.unwrap(), key.to_bytes());
    }

    // ── Per-tunnel key tests ──────────────────────────────────────────

    #[tokio::test]
    async fn listen_key_for_tunnel_fresh_project_generates_key_at_per_tunnel_path() {
        let repo = Repo::open_or_create(temp_repo_dir()).await.unwrap();
        // Pre-create the key so listen_key_for_tunnel can read it.
        let tunnel_dir = repo.0.join("my-project").join("my-tunnel");
        tokio::fs::create_dir_all(&tunnel_dir).await.unwrap();
        let key_path = tunnel_dir.join(Repo::LISTEN_KEY_FILE);
        let seed_key = SecretKey::generate(&mut rand::rng());
        tokio::fs::write(&key_path, seed_key.to_bytes()).await.unwrap();

        let key = repo
            .listen_key_for_tunnel("my-project", "my-tunnel")
            .await
            .unwrap();
        assert!(key_path.exists(), "key must exist at per-tunnel path");
        assert_eq!(tokio::fs::read(&key_path).await.unwrap(), key.to_bytes());
    }

    #[tokio::test]
    async fn listen_key_for_tunnel_migrates_legacy_key_to_default_tunnel() {
        let repo = Repo::open_or_create(temp_repo_dir()).await.unwrap();
        // Create a legacy key at the project root (plain name, no timestamp).
        let legacy_key = SecretKey::generate(&mut rand::rng());
        let legacy_bytes = legacy_key.to_bytes();
        let legacy_path = repo.0.join(Repo::LISTEN_KEY_FILE);
        tokio::fs::write(&legacy_path, &legacy_bytes)
            .await
            .expect("should write legacy key");
        assert!(legacy_path.exists(), "precondition: legacy key exists");

        // Access per-tunnel for "default" tunnel — should migrate.
        let key = repo
            .listen_key_for_tunnel("proj-migrate", "default")
            .await
            .unwrap();
        assert_eq!(
            key.to_bytes(),
            legacy_bytes,
            "migrated key must match the legacy key value"
        );
        assert!(
            !legacy_path.exists(),
            "legacy file must be removed after migration"
        );
        let expected_path = repo
            .0
            .join("proj-migrate")
            .join("default")
            .join(Repo::LISTEN_KEY_FILE);
        assert!(expected_path.exists(), "key must now live at per-tunnel path");
    }

    #[tokio::test]
    async fn listen_key_for_tunnel_is_stable_across_calls() {
        let repo = Repo::open_or_create(temp_repo_dir()).await.unwrap();
        // Pre-create the key.
        let tunnel_dir = repo.0.join("stable-proj").join("stable-tunnel");
        tokio::fs::create_dir_all(&tunnel_dir).await.unwrap();
        let key_path = tunnel_dir.join(Repo::LISTEN_KEY_FILE);
        let seed_key = SecretKey::generate(&mut rand::rng());
        tokio::fs::write(&key_path, seed_key.to_bytes()).await.unwrap();

        let first = repo
            .listen_key_for_tunnel("stable-proj", "stable-tunnel")
            .await
            .unwrap();
        let second = repo
            .listen_key_for_tunnel("stable-proj", "stable-tunnel")
            .await
            .unwrap();
        assert_eq!(
            first.to_bytes(),
            second.to_bytes(),
            "repeat calls must return the same persisted key"
        );
    }

    #[tokio::test]
    async fn listen_key_for_tunnel_two_tunnels_get_distinct_keys() {
        let repo = Repo::open_or_create(temp_repo_dir()).await.unwrap();
        // Pre-create distinct keys for two tunnels.
        for name in ["tunnel-a", "tunnel-b"] {
            let tunnel_dir = repo.0.join("multi-proj").join(name);
            tokio::fs::create_dir_all(&tunnel_dir).await.unwrap();
            let key_path = tunnel_dir.join(Repo::LISTEN_KEY_FILE);
            let seed_key = SecretKey::generate(&mut rand::rng());
            tokio::fs::write(&key_path, seed_key.to_bytes()).await.unwrap();
        }
        let key_a = repo
            .listen_key_for_tunnel("multi-proj", "tunnel-a")
            .await
            .unwrap();
        let key_b = repo
            .listen_key_for_tunnel("multi-proj", "tunnel-b")
            .await
            .unwrap();
        assert_ne!(
            key_a.to_bytes(),
            key_b.to_bytes(),
            "two tunnels in the same project must get distinct keys"
        );
    }

    #[tokio::test]
    async fn listen_key_for_tunnel_errors_when_key_missing() {
        let repo = Repo::open_or_create(temp_repo_dir()).await.unwrap();
        let result = repo
            .listen_key_for_tunnel("missing-proj", "missing-tunnel")
            .await;
        assert!(
            result.is_err(),
            "should error when key does not exist (no legacy migration)"
        );
    }
}

#[cfg(test)]
mod default_location_tests {
    use super::*;

    // Both crates are Rust edition 2024 — std::env::set_var /
    // remove_var require the `unsafe` block. The shared ENV_LOCK
    // serializes against the other env-mutating tests in the crate
    // (datum_cloud/external_token_source.rs, datum_cloud/mod.rs).

    #[test]
    fn returns_ok_when_var_set() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let saved = std::env::var("DATUM_CONNECT_DIR").ok();
        unsafe { std::env::set_var("DATUM_CONNECT_DIR", "/tmp/test-connect-dir"); }

        let got = Repo::default_location();

        // Restore before asserting so a panic doesn't leak the mutation.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("DATUM_CONNECT_DIR", v),
                None => std::env::remove_var("DATUM_CONNECT_DIR"),
            }
        }

        match got {
            Ok(p) => assert_eq!(p, PathBuf::from("/tmp/test-connect-dir")),
            Err(e) => panic!("expected Ok, got Err({e})"),
        }
    }

    #[test]
    fn returns_err_when_var_empty() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let saved = std::env::var("DATUM_CONNECT_DIR").ok();
        unsafe { std::env::set_var("DATUM_CONNECT_DIR", ""); }

        let got = Repo::default_location();

        unsafe {
            match saved {
                Some(v) => std::env::set_var("DATUM_CONNECT_DIR", v),
                None => std::env::remove_var("DATUM_CONNECT_DIR"),
            }
        }

        assert!(matches!(got, Err(MissingConnectDir)));
    }

    #[test]
    fn returns_err_when_var_unset() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let saved = std::env::var("DATUM_CONNECT_DIR").ok();
        unsafe { std::env::remove_var("DATUM_CONNECT_DIR"); }

        let got = Repo::default_location();

        unsafe {
            if let Some(v) = saved {
                std::env::set_var("DATUM_CONNECT_DIR", v);
            }
        }

        assert!(matches!(got, Err(MissingConnectDir)));
    }

    #[test]
    fn missing_connect_dir_display_contains_directive() {
        // Pure formatting check — no env mutation needed.
        let msg = format!("{}", MissingConnectDir);
        assert!(msg.contains("DATUM_CONNECT_DIR is not set"), "msg = {msg}");
        assert!(msg.contains("datumctl connect tunnel"), "msg = {msg}");
        assert!(msg.contains("export DATUM_CONNECT_DIR=\"$HOME/.datumctl/connect\""), "msg = {msg}");
        assert!(msg.contains("(exit 64)"), "msg = {msg}");
    }
}
