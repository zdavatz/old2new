//! Verify the external CLIs the pipeline shells out to (yt-dlp,
//! ffmpeg, ffprobe). Runs cheaply on startup; the GUI re-runs it when
//! the user clicks "Re-check" after installing.

use std::process::{Command, Stdio};

#[derive(Default, Clone)]
pub struct DepStatus {
    pub yt_dlp: Option<String>,
    pub ffmpeg: Option<String>,
    pub ffprobe: Option<String>,
}

impl DepStatus {
    pub fn check() -> Self {
        Self {
            yt_dlp: probe("yt-dlp", &["--version"]),
            ffmpeg: probe("ffmpeg", &["-version"]),
            ffprobe: probe("ffprobe", &["-version"]),
        }
    }

    pub fn missing(&self) -> Vec<&'static str> {
        let mut m = Vec::new();
        if self.yt_dlp.is_none() { m.push("yt-dlp"); }
        if self.ffmpeg.is_none() { m.push("ffmpeg"); }
        if self.ffprobe.is_none() { m.push("ffprobe"); }
        m
    }

    pub fn install_hint(missing: &[&str]) -> String {
        if missing.is_empty() { return String::new(); }
        if cfg!(target_os = "macos") {
            format!("Install: brew install {}", brew_packages_for(missing).join(" "))
        } else if cfg!(target_os = "windows") {
            let pkgs = missing.join(" ");
            format!("Install: scoop install {}  (or: winget install {})", pkgs, pkgs)
        } else {
            let pkgs = brew_packages_for(missing).join(" ");
            format!("Install: sudo apt install -y {}  (or your distro's equivalent)", pkgs)
        }
    }
}

/// Map missing tool names to the Homebrew/apt package that provides
/// them. ffprobe and ffmpeg both come from the `ffmpeg` package.
pub fn brew_packages_for(missing: &[&str]) -> Vec<String> {
    let mut pkgs = Vec::new();
    for &m in missing {
        let pkg = match m {
            "yt-dlp" => "yt-dlp",
            "ffmpeg" | "ffprobe" => "ffmpeg",
            other => other,
        };
        if !pkgs.iter().any(|p: &String| p == pkg) {
            pkgs.push(pkg.to_string());
        }
    }
    pkgs
}

/// Path to a `brew` binary on PATH or one of the conventional locations,
/// or `None` if Homebrew is not installed.
pub fn brew_path() -> Option<String> {
    if !cfg!(target_os = "macos") { return None; }
    if Command::new("brew").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success()) {
        return Some("brew".to_string());
    }
    for candidate in ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"] {
        if std::path::Path::new(candidate).exists() {
            return Some(candidate.to_string());
        }
    }
    None
}

pub const HOMEBREW_INSTALL_CMD: &str =
    "/bin/bash -c \"$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\"";

fn probe(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    if !out.status.success() { return None; }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let first = stdout.lines().next().unwrap_or("").trim();
    Some(first.to_string())
}
