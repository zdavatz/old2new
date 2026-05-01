//! Browser auto-detection + cookie validation. Used to pick a sensible
//! default for `--cookies-from-browser` and to verify the user's choice
//! actually grants yt-dlp access (Safari's keychain bites here).

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Names yt-dlp accepts for `--cookies-from-browser`.
pub const ALL: &[&str] = &[
    "chrome", "brave", "chromium", "edge", "firefox", "opera", "safari", "vivaldi",
];

/// Returns the names of yt-dlp-recognized browsers whose profile dir is
/// present on disk — the cheapest "is it installed?" probe.
pub fn detected() -> Vec<&'static str> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let mut out: Vec<&'static str> = Vec::new();
    let mut push = |name: &'static str, paths: &[PathBuf]| {
        if paths.iter().any(|p| p.exists()) {
            if !out.contains(&name) { out.push(name); }
        }
    };

    #[cfg(target_os = "macos")]
    {
        let app = home.join("Library").join("Application Support");
        push("chrome",   &[app.join("Google").join("Chrome")]);
        push("brave",    &[app.join("BraveSoftware").join("Brave-Browser")]);
        push("chromium", &[app.join("Chromium")]);
        push("edge",     &[app.join("Microsoft Edge")]);
        push("firefox",  &[app.join("Firefox").join("Profiles")]);
        push("opera",    &[app.join("com.operasoftware.Opera")]);
        push("safari",   &[home.join("Library").join("Cookies").join("Cookies.binarycookies")]);
        push("vivaldi",  &[app.join("Vivaldi")]);
    }
    #[cfg(target_os = "linux")]
    {
        let cfg = home.join(".config");
        push("chrome",   &[cfg.join("google-chrome")]);
        push("brave",    &[cfg.join("BraveSoftware").join("Brave-Browser")]);
        push("chromium", &[cfg.join("chromium")]);
        push("edge",     &[cfg.join("microsoft-edge")]);
        push("firefox",  &[home.join(".mozilla").join("firefox")]);
        push("opera",    &[cfg.join("opera")]);
        push("vivaldi",  &[cfg.join("vivaldi")]);
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            let l = PathBuf::from(local);
            push("chrome",  &[l.join("Google").join("Chrome").join("User Data")]);
            push("brave",   &[l.join("BraveSoftware").join("Brave-Browser").join("User Data")]);
            push("edge",    &[l.join("Microsoft").join("Edge").join("User Data")]);
            push("vivaldi", &[l.join("Vivaldi").join("User Data")]);
        }
        if let Ok(roaming) = std::env::var("APPDATA") {
            let r = PathBuf::from(roaming);
            push("firefox", &[r.join("Mozilla").join("Firefox")]);
            push("opera",   &[r.join("Opera Software").join("Opera Stable")]);
        }
    }
    out
}

/// Run yt-dlp against a public test URL with `--cookies-from-browser
/// <name>` to verify cookies can actually be read. Returns Ok on
/// success, Err with stderr text on failure (so the GUI can surface
/// e.g. "permission denied" / "could not find any browser cookies").
pub fn test_cookies(browser: &str) -> Result<(), String> {
    // "Me at the zoo" — first YouTube video, public, never disappears.
    const URL: &str = "https://www.youtube.com/watch?v=jNQXAC9IVRw";
    let out = Command::new("yt-dlp")
        .arg("--cookies-from-browser").arg(browser)
        .arg("--simulate")
        .arg("--skip-download")
        .arg("--no-warnings")
        .arg("-q")
        .arg(URL)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("yt-dlp not found ({}). Install yt-dlp first.", e))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if err.is_empty() {
        Err(format!("yt-dlp exited with {:?}", out.status.code()))
    } else {
        // Trim noisy "ERROR: " prefix that yt-dlp emits.
        let cleaned = err.lines().find(|l| !l.is_empty()).unwrap_or(&err).to_string();
        Err(cleaned)
    }
}
