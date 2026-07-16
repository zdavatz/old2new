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
    /// Empty = don't pass `--cookies-from-browser` to yt-dlp.
    /// Otherwise one of: chrome, brave, chromium, edge, firefox, opera, safari, vivaldi.
    #[serde(default)]
    pub cookies_browser: String,
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
            whatsapp_dir: default_whatsapp_dir(),
            whatsapp_recipient: String::new(),
            davaz_token: String::new(),
            linkedin_client_id: String::new(),
            linkedin_client_secret: String::new(),
            linkedin_visibility: default_linkedin_visibility(),
            auto_update_deps: true,
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
