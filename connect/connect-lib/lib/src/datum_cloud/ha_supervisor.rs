//! The Home Assistant Supervisor, as an add-on sees it, for pairing: show
//! the approval link as a notification, and read the add-on's saved options
//! while pairing waits for a project to be chosen.
//!
//! An add-on's log is plain text, so a link in it cannot be clicked, and on
//! a phone it cannot easily be copied either. A persistent notification (the
//! bell in Home Assistant's sidebar) renders markdown, so the link is one
//! click. The log lines stay as they are, as the fallback.
//!
//! Every call here is best-effort: a failure is the caller's to log as a
//! warning, never a failed pairing.
//!
//! How the Supervisor behaves, from home-assistant/supervisor (2026-10):
//!
//! - `/core/api/*` is proxied to Home Assistant Core
//!   (`supervisor/api/proxy.py`), and only for an add-on whose config sets
//!   `homeassistant_api: true` (`_check_access`); `hassio_api` does not grant
//!   it. Without it the Supervisor answers 401.
//! - Saving the Configuration tab (`POST /addons/self/options`,
//!   `api/apps.py` `options`) only stores the options in the Supervisor's
//!   own state (`save_persist`). `/data/options.json` is written by
//!   `write_options()`, which only `start()` calls, so the file does not
//!   change until the next start.
//! - The Supervisor does not restart an add-on whose options are saved, but
//!   the frontend does offer to: after a save on a started add-on,
//!   `supervisor-app-config.ts` calls `suggestSupervisorAppRestart`, a
//!   "Restart <name>? The app needs to be restarted for the changes to take
//!   effect." dialog (home-assistant/frontend, 2026-10). Pairing therefore
//!   has to survive that restart: see `pairing`'s session file.
//! - `GET /addons/self/options/config` returns the saved options validated
//!   against the add-on's schema, i.e. what `/data/options.json` will hold
//!   on the next start, and only to the add-on itself. It is on the
//!   Supervisor's bypass list (`api/middleware/security.py`), so it needs no
//!   role at all. It is used here rather than `/addons/self/info`, whose
//!   `options` are the same values unvalidated, in a much larger reply.

use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

pub use super::pairing::duration_words;
use super::pairing::{PairingError, PairingEvent, ProjectChoice, ProjectSource, normalize_project};

/// Set by the Supervisor in every add-on's environment (the s6
/// `with-contenv` environment passes it through to run.sh).
pub const TOKEN_ENV: &str = "SUPERVISOR_TOKEN";
/// Overrides [`DEFAULT_URL`]. Only tests and local development set it.
pub const URL_ENV: &str = "DATUM_SUPERVISOR_URL";
pub const DEFAULT_URL: &str = "http://supervisor";

/// One notification, updated in place: creating it again with the same id
/// replaces it, so a new code or a new project list never stacks up.
pub const NOTIFICATION_ID: &str = "datum_connect_pairing";
pub const NOTIFICATION_TITLE: &str = "Datum Connect: connect to Datum";

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the end of pairing waits for queued notifications to go out.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(15);

/// A client for the Supervisor, holding the add-on's Supervisor token.
#[derive(Clone)]
pub struct Supervisor {
    http: reqwest::Client,
    base: String,
    token: SecretString,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor").field("base", &self.base).finish_non_exhaustive()
    }
}

impl Supervisor {
    /// `None` outside an add-on, where there is no [`TOKEN_ENV`]: then there
    /// is nobody to notify and pairing is log-only.
    pub fn from_env() -> Option<Self> {
        let token = std::env::var(TOKEN_ENV).ok().filter(|t| !t.trim().is_empty())?;
        let base = std::env::var(URL_ENV)
            .ok()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_URL.to_string());
        match Self::new(base, SecretString::from(token)) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("Home Assistant notifications are off: {e}");
                None
            }
        }
    }

    pub fn new(base: impl Into<String>, token: SecretString) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| format!("cannot build HTTP client: {e}"))?;
        Ok(Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
            token,
        })
    }

    /// Creates the pairing notification, or replaces it if it is shown.
    pub async fn notify(&self, message: &str) -> Result<(), String> {
        self.call_service(
            "create",
            &json!({
                "notification_id": NOTIFICATION_ID,
                "title": NOTIFICATION_TITLE,
                "message": message,
            }),
        )
        .await
    }

    /// Removes the pairing notification. Not an error if none is shown.
    pub async fn dismiss(&self) -> Result<(), String> {
        self.call_service("dismiss", &json!({"notification_id": NOTIFICATION_ID}))
            .await
    }

    async fn call_service(&self, service: &str, body: &Value) -> Result<(), String> {
        let url = format!("{}/core/api/services/persistent_notification/{service}", self.base);
        let response = self
            .http
            .post(&url)
            .bearer_auth(self.token.expose_secret())
            .json(body)
            .send()
            .await
            .map_err(|e| self.redact(format!("cannot reach the Supervisor: {e}")))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let hint = if status.as_u16() == 401 || status.as_u16() == 403 {
            " (the add-on needs homeassistant_api: true in config.yaml)"
        } else {
            ""
        };
        let text = response.text().await.unwrap_or_default();
        Err(self.redact(format!("Home Assistant answered HTTP {status}{hint}: {}", excerpt(&text))))
    }

    /// The `project` option as last saved on the Configuration tab, which
    /// may be newer than `/data/options.json` (see the module docs).
    pub async fn saved_project(&self) -> Result<Option<String>, String> {
        let url = format!("{}/addons/self/options/config", self.base);
        let response = self
            .http
            .get(&url)
            .bearer_auth(self.token.expose_secret())
            .send()
            .await
            .map_err(|e| self.redact(format!("cannot reach the Supervisor: {e}")))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(self.redact(format!(
                "the Supervisor answered HTTP {status} for the saved options: {}",
                excerpt(&text)
            )));
        }
        // {"result": "ok", "data": {<options>}}
        let v: Value = serde_json::from_str(&text)
            .map_err(|_| "the Supervisor sent saved options that are not JSON".to_string())?;
        let options = v.get("data").unwrap_or(&Value::Null);
        Ok(project_option(options))
    }

    fn redact(&self, message: String) -> String {
        let secret = self.token.expose_secret();
        if secret.is_empty() {
            message
        } else {
            message.replace(secret, "[redacted]")
        }
    }
}

/// `options.project`, normalised, with empty meaning unset.
fn project_option(options: &Value) -> Option<String> {
    options.get("project").and_then(Value::as_str).and_then(normalize_project)
}

type SourceFuture<'a> = Pin<Box<dyn std::future::Future<Output = Result<Option<String>, String>> + Send + 'a>>;

impl ProjectSource for Supervisor {
    fn project(&self) -> SourceFuture<'_> {
        Box::pin(self.saved_project())
    }
}

/// An options file read again on every poll: `{"project": "..."}`, the
/// shape of `/data/options.json`. For tests and development outside Home
/// Assistant; inside it, that file is stale until a restart.
#[derive(Debug, Clone)]
pub struct OptionsFile(pub PathBuf);

impl ProjectSource for OptionsFile {
    fn project(&self) -> SourceFuture<'_> {
        Box::pin(async move {
            let raw = tokio::fs::read_to_string(&self.0)
                .await
                .map_err(|e| format!("cannot read {}: {e}", self.0.display()))?;
            let v: Value =
                serde_json::from_str(&raw).map_err(|_| format!("{} is not JSON", self.0.display()))?;
            Ok(project_option(&v))
        })
    }
}

// ---- The notifications pairing shows ----

/// Shows pairing's progress as one Home Assistant notification. Calls go
/// out one at a time, in order, from a task of their own, so pairing never
/// waits on Home Assistant and a later update never lands before an
/// earlier one.
pub struct PairingNotifier {
    tx: tokio::sync::mpsc::UnboundedSender<Note>,
    task: tokio::task::JoinHandle<()>,
}

enum Note {
    Show(String),
    Dismiss,
}

impl PairingNotifier {
    pub fn spawn(supervisor: Supervisor) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Note>();
        let task = tokio::spawn(async move {
            while let Some(note) = rx.recv().await {
                let result = match &note {
                    Note::Show(message) => supervisor.notify(message).await,
                    Note::Dismiss => supervisor.dismiss().await,
                };
                if let Err(e) = result {
                    let what = match note {
                        Note::Show(_) => "Could not show the pairing link as a Home Assistant notification; use the link in this log instead",
                        Note::Dismiss => "Could not remove the pairing notification from Home Assistant; dismiss it by hand",
                    };
                    tracing::warn!("{what}: {e}");
                }
            }
        });
        Self { tx, task }
    }

    /// Updates the notification for the events a person has to act on.
    pub fn event(&self, event: &PairingEvent) {
        let message = match event {
            PairingEvent::Code { url, user_code, expires_in } => code_message(url, user_code, *expires_in),
            PairingEvent::ChooseProject { projects, rejected, wait } => {
                choose_project_message(projects, rejected.as_deref(), *wait)
            }
            _ => return,
        };
        let _ = self.tx.send(Note::Show(message));
    }

    /// Replaces the notification with why pairing stopped.
    pub fn failed(&self, reason: &str) {
        let _ = self.tx.send(Note::Show(failure_message(reason)));
    }

    /// Removes the notification: pairing is done, or stopped with the
    /// add-on, and its code is no use any more.
    pub fn dismiss(&self) {
        let _ = self.tx.send(Note::Dismiss);
    }

    /// The add-on stopped while pairing waited for a project, with the
    /// login saved: say it carries on, rather than leave a list that reads
    /// as if it were still waiting, or remove it with no word.
    pub fn paused(&self) {
        let _ = self.tx.send(Note::Show(PAUSED_MESSAGE.to_string()));
    }

    /// The end of pairing: the notification goes on success, and says why
    /// on failure.
    pub fn outcome<T>(&self, result: &Result<T, PairingError>) {
        match result {
            Ok(_) => self.dismiss(),
            Err(e) => self.failed(&e.to_string()),
        }
    }

    /// Waits, briefly, for everything queued to be sent.
    pub async fn finish(self) {
        let Self { tx, mut task } = self;
        drop(tx);
        if tokio::time::timeout(FLUSH_TIMEOUT, &mut task).await.is_err() {
            task.abort();
            tracing::warn!("Gave up waiting for Home Assistant to take the pairing notification");
        }
    }
}

/// What [`PairingNotifier::paused`] shows.
pub const PAUSED_MESSAGE: &str =
    "Pairing paused while the add-on restarts. It continues without a new login when the add-on starts again.";

/// The code notification. `url` already carries the code.
pub fn code_message(url: &str, user_code: &str, expires_in: Duration) -> String {
    format!(
        "[Open the Datum approval page]({}) and confirm code **{}**. The code expires in {}; a new one appears here if it does.",
        link_target(url),
        escape(user_code),
        duration_words(expires_in)
    )
}

/// The project list, while pairing waits for one to be set.
pub fn choose_project_message(projects: &[ProjectChoice], rejected: Option<&str>, wait: Duration) -> String {
    let mut out = String::new();
    if let Some(r) = rejected {
        out.push_str(&format!("'{}' isn't one of your projects.\n\n", escape(r)));
    }
    out.push_str(&format!(
        "Your Datum login can see {} projects. Set **project** on the add-on's Configuration tab to one of these ids and click Save. Home Assistant offers to restart the add-on when you save; either way, pairing continues without a new login.\n\n",
        projects.len()
    ));
    for p in projects {
        out.push_str(&format!(
            "- {}: {} (organization {})\n",
            code_span(&p.id),
            escape(&p.display_name),
            escape(&p.organization)
        ));
    }
    out.push_str(&format!(
        "\nIf none is set within {}, restart the add-on to try again.",
        duration_words(wait)
    ));
    out
}

pub fn failure_message(reason: &str) -> String {
    // Hard line breaks, so a multi-line reason keeps its lines.
    let escaped = escape(reason.trim()).replace('\n', "  \n");
    // Most reasons already say what to do, restarting included; saying it
    // twice reads as two different steps.
    if reason.to_ascii_lowercase().contains("restart") {
        format!("Pairing with Datum failed: {escaped}")
    } else {
        format!("Pairing with Datum failed: {escaped}\n\nRestart the add-on to try again.")
    }
}

/// Markdown is rendered, so text from elsewhere (display names, a typed
/// project, a server's error) is escaped rather than trusted to be plain.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '<' | '>' | '#' | '|' | '~' | '!') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// An id as code, so it reads as something to copy; escaped text if it
/// could break out of the code span.
fn code_span(id: &str) -> String {
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_')) {
        format!("`{id}`")
    } else {
        escape(id)
    }
}

/// A URL that cannot end the markdown link early.
fn link_target(url: &str) -> String {
    url.replace(' ', "%20").replace('(', "%28").replace(')', "%29").replace('<', "%3C").replace('>', "%3E")
}

fn excerpt(body: &str) -> String {
    let flat: String = body.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let flat = flat.trim();
    if flat.is_empty() {
        return "no response body".into();
    }
    flat.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_message_is_a_link_and_the_code() {
        let m = code_message(
            "https://auth.datum.net/ui/v2/login/device?user_code=DPDX-JRRN",
            "DPDX-JRRN",
            Duration::from_secs(300),
        );
        assert_eq!(
            m,
            "[Open the Datum approval page](https://auth.datum.net/ui/v2/login/device?user_code=DPDX-JRRN) and confirm code **DPDX-JRRN**. The code expires in 5 minutes; a new one appears here if it does."
        );
    }

    #[test]
    fn a_url_cannot_break_out_of_the_link() {
        let m = code_message("https://x.example/a)b (c)", "C", Duration::from_secs(60));
        assert!(m.contains("(https://x.example/a%29b%20%28c%29)"), "{m}");
    }

    #[test]
    fn project_list_escapes_what_it_did_not_write() {
        let projects = vec![
            ProjectChoice { id: "p-1".into(), display_name: "Home [x](javascript:alert(1))".into(), organization: "o-1".into() },
            ProjectChoice { id: "weird`id".into(), display_name: "W".into(), organization: "o_2".into() },
        ];
        let m = choose_project_message(&projects, Some("*nope*"), Duration::from_secs(1800));
        assert!(m.starts_with("'\\*nope\\*' isn't one of your projects.\n\n"), "{m}");
        assert!(m.contains("Set **project** on the add-on's Configuration tab to one of these ids and click Save. Home Assistant offers to restart the add-on when you save; either way, pairing continues without a new login."), "{m}");
        assert!(!m.contains("No restart needed"), "{m}");
        assert!(m.contains("- `p-1`: Home \\[x\\]\\(javascript:alert\\(1\\)\\) (organization o-1)"), "{m}");
        assert!(m.contains("- weird\\`id: W (organization o\\_2)"), "{m}");
        assert!(m.contains("within 30 minutes"), "{m}");
    }

    #[test]
    fn failure_keeps_lines_and_says_to_restart() {
        let m = failure_message("first\nsecond");
        assert_eq!(m, "Pairing with Datum failed: first  \nsecond\n\nRestart the add-on to try again.");
        let m = failure_message("The request was declined. Restart to get a new code.");
        assert_eq!(m, "Pairing with Datum failed: The request was declined. Restart to get a new code.");
    }

    #[test]
    fn from_env_needs_the_supervisor_token() {
        let _lock = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var(TOKEN_ENV);
            std::env::remove_var(URL_ENV);
        }
        assert!(Supervisor::from_env().is_none(), "no token, no notifications");
        unsafe { std::env::set_var(TOKEN_ENV, "  ") };
        assert!(Supervisor::from_env().is_none(), "a blank token is no token");
        unsafe { std::env::set_var(TOKEN_ENV, "sup-token") };
        let s = Supervisor::from_env().expect("token set");
        assert_eq!(s.base, DEFAULT_URL);
        assert!(!format!("{s:?}").contains("sup-token"));
        unsafe { std::env::set_var(URL_ENV, "http://127.0.0.1:9/") };
        assert_eq!(Supervisor::from_env().unwrap().base, "http://127.0.0.1:9");
        unsafe {
            std::env::remove_var(TOKEN_ENV);
            std::env::remove_var(URL_ENV);
        }
    }

    #[tokio::test]
    async fn options_file_reads_the_project_each_time() {
        let path = std::env::temp_dir().join(format!("options-{}.json", uuid::Uuid::new_v4()));
        let src = OptionsFile(path.clone());
        assert!(src.project().await.is_err(), "missing file");
        std::fs::write(&path, r#"{"project": ""}"#).unwrap();
        assert_eq!(src.project().await.unwrap(), None);
        std::fs::write(&path, r#"{"project": " p-1 "}"#).unwrap();
        assert_eq!(src.project().await.unwrap().as_deref(), Some("p-1"));
        std::fs::write(&path, r#"{"project": "\"P-1\""}"#).unwrap();
        assert_eq!(src.project().await.unwrap().as_deref(), Some("p-1"), "normalised like every other project id");
        let _ = std::fs::remove_file(&path);
    }
}
