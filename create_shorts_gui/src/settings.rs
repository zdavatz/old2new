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
    /// Default tag color when posting to davaz.com. One of "" (no tag),
    /// "yellow" (gold promoted) or "purple" (violet promoted).
    #[serde(default)]
    pub davaz_tag_color: String,
}

fn default_privacy() -> String {
    "public".to_string()
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
            davaz_tag_color: String::new(),
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
