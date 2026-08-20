//! Persisted user settings: Google OAuth client credentials, default
//! privacy, optional cookies-from-browser selector. Stored at the
//! platform's standard config directory under `create_shorts/`.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default = "default_privacy")]
    pub default_privacy: String,
    /// Empty = don't pass `--cookies-from-browser` to yt-dlp, which is what you
    /// normally want: signing in makes YouTube meter the HD byte allowance per
    /// session (downloads 403 partway through) and makes yt-dlp >= 2026.08.19
    /// skip the clients that still serve HD. Set it only for videos that need
    /// auth (private, age-gated).
    /// Otherwise one of: chrome, brave, chromium, edge, firefox, opera, safari, vivaldi.
    #[serde(default)]
    pub cookies_browser: String,
    /// One-time migration marker for 1.0.74. Up to 1.0.73 the app auto-filled
    /// `cookies_browser` with the first detected browser on launch, so users
    /// carry a value they never deliberately chose — and which now breaks
    /// downloads. False means that inherited default has not been cleared yet.
    #[serde(default)]
    pub cookies_auto_default_cleared: bool,
    /// Path to the pegelstand `whatsapp/` directory (provides auth/ + node_modules/).
    /// Empty = WhatsApp send disabled.
    #[serde(default)]
    pub whatsapp_dir: String,
    /// WhatsApp recipient: phone number (digits only, e.g. 41791234567)
    /// or full JID (e.g. 1203634XXXX@g.us for groups).
    #[serde(default)]
    pub whatsapp_recipient: String,
    /// Bearer token for the davaz.com videos API. Empty = Post to
    /// davaz.com button disabled. Token comes from `etc/api_tokens` on
    /// the davaz.com server (one token per line).
    #[serde(default)]
    pub davaz_token: String,
    /// LinkedIn app credentials, from an app registered at
    /// developer.linkedin.com with redirect URI `http://localhost:8092/callback`
    /// and the products *Sign In with LinkedIn using OpenID Connect* + *Share
    /// on LinkedIn*. Empty = the Post to LinkedIn button is disabled. Same
    /// bring-your-own-client model as the Google credentials above.
    #[serde(default)]
    pub linkedin_client_id: String,
    #[serde(default)]
    pub linkedin_client_secret: String,
    /// Visibility of the created LinkedIn post: PUBLIC or CONNECTIONS.
    #[serde(default = "default_linkedin_visibility")]
    pub linkedin_visibility: String,
    /// Keep the external tools (yt-dlp, ffmpeg, mpv) up to date by running a
    /// throttled Homebrew upgrade in the background at launch. Defaults on —
    /// a stale yt-dlp/ffmpeg is the most common cause of download failures
    /// (see the `07V4vfc4yKk` "[Errno 2] …raw.mp4.part" incident). Runs at
    /// most once per calendar day (see [`dep_update_ran_today`]).
    #[serde(default = "default_true")]
    pub auto_update_deps: bool,
    /// UI zoom factor from the top-bar A+/A− font-size buttons (1.0 =
    /// default). Scales the whole UI via egui's zoom factor. None = never
    /// touched. Clamped to [0.6, 3.0] on restore so a hand-edited value
    /// can't render the UI unusable.
    #[serde(default)]
    pub ui_zoom: Option<f32>,
}

fn default_privacy() -> String {
    "public".to_string()
}

fn default_true() -> bool {
    true
}

fn default_linkedin_visibility() -> String {
    "PUBLIC".to_string()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            client_secret: String::new(),
            default_privacy: default_privacy(),
            cookies_browser: String::new(),
            // A fresh config has no inherited auto-default to clear.
            cookies_auto_default_cleared: true,
            whatsapp_dir: default_whatsapp_dir(),
            whatsapp_recipient: String::new(),
            davaz_token: String::new(),
            linkedin_client_id: String::new(),
            linkedin_client_secret: String::new(),
            linkedin_visibility: default_linkedin_visibility(),
            auto_update_deps: true,
            ui_zoom: None,
        }
    }
}

/// Default to a self-contained WhatsApp dir under the app's config dir.
/// Setup WhatsApp provisions this dir (npm install + scripts) and Link
/// WhatsApp creates auth/ inside it. Self-contained — no dependency on
/// other projects.
fn default_whatsapp_dir() -> String {
    config_dir().join("whatsapp").to_string_lossy().into_owned()
}

pub fn managed_whatsapp_dir() -> std::path::PathBuf {
    config_dir().join("whatsapp")
}

pub fn config_dir() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("create_shorts")
}

pub fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn token_path() -> PathBuf {
    config_dir().join("token.json")
}

pub fn log_path() -> PathBuf {
    config_dir().join("log.txt")
}

/// Stamp file recording the last calendar day the dependency auto-update
/// ran, so a heavy `brew update` fires at most once/day no matter how often
/// the app is launched.
pub fn dep_update_stamp_path() -> PathBuf {
    config_dir().join("dep_update_check")
}

/// True if the dependency auto-update already ran today (same local
/// calendar day). Missing/unreadable stamp ⇒ false (run it).
pub fn dep_update_ran_today() -> bool {
    let today = chrono::Local::now().date_naive().to_string();
    fs::read_to_string(dep_update_stamp_path())
        .map(|s| s.trim() == today)
        .unwrap_or(false)
}

/// Record that the dependency auto-update ran today.
pub fn mark_dep_update_ran() {
    let today = chrono::Local::now().date_naive().to_string();
    let _ = fs::create_dir_all(config_dir());
    let _ = fs::write(dep_update_stamp_path(), today);
}

/// Stamp recording that an update attempt on the given calendar day
/// confirmed the installed yt-dlp is the newest release available even
/// though its date-version makes it look stale — yt-dlp simply hasn't
/// shipped in over [`crate::deps::YT_DLP_STALE_DAYS`] days. Lets the
/// staleness banner distinguish "old because upstream is quiet" from
/// "old because the user never updated". Format: `YYYY-MM-DD <version>`.
pub fn ytdlp_uptodate_stamp_path() -> PathBuf {
    config_dir().join("ytdlp_uptodate_check")
}

/// True if an update run today (same local calendar day) already confirmed
/// `version` is the newest yt-dlp available. Missing/other-day/other-version
/// stamp ⇒ false (the staleness banner shows). Deliberately expires daily:
/// once upstream releases again, the next update run replaces the version
/// and the stamp stops matching anyway.
pub fn ytdlp_confirmed_latest_today(version: &str) -> bool {
    let today = chrono::Local::now().date_naive().to_string();
    fs::read_to_string(ytdlp_uptodate_stamp_path())
        .map(|s| {
            let mut it = s.split_whitespace();
            it.next() == Some(today.as_str()) && it.next() == Some(version)
        })
        .unwrap_or(false)
}

/// Record that an update attempt today confirmed `version` is the newest
/// yt-dlp available.
pub fn mark_ytdlp_confirmed_latest(version: &str) {
    let today = chrono::Local::now().date_naive().to_string();
    let _ = fs::create_dir_all(config_dir());
    let _ = fs::write(
        ytdlp_uptodate_stamp_path(),
        format!("{} {}", today, version),
    );
}

impl Settings {
    pub fn load() -> Self {
        let path = settings_path();
        match fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let dir = config_dir();
        fs::create_dir_all(&dir)?;
        let s = serde_json::to_string_pretty(self).unwrap_or_default();
        fs::write(settings_path(), s)?;
        Ok(())
    }
}
