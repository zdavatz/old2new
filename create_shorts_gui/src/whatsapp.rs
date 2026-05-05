//! WhatsApp send via Baileys.
//!
//! Self-contained — owns its own auth/ + node_modules/ under the app's
//! config dir. The Node helpers (`send-text.mjs`, `login.mjs`,
//! `package.json`) are bundled into the binary via `include_str!` and
//! materialised on disk via `Setup WhatsApp` in Settings.
//!
//! If the user already has pegelstand's `whatsapp/` linked, they can
//! point Settings → WhatsApp dir at that path and skip the setup step.

use crossbeam_channel::Sender;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const SEND_TEXT_MJS: &str = include_str!("../whatsapp/send-text.mjs");
const LOGIN_MJS: &str = include_str!("../whatsapp/login.mjs");
const LIST_RECIPIENTS_MJS: &str = include_str!("../whatsapp/list-recipients.mjs");
const PACKAGE_JSON: &str = include_str!("../whatsapp/package.json");

#[derive(Debug)]
pub enum WaError {
    NoWhatsappDir,
    NoRecipient,
    Io(String),
    NotLinked(PathBuf),
    NodeNotFound,
    Spawn(String),
    Failed { code: Option<i32>, stderr: String },
}

impl std::fmt::Display for WaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WaError::NoWhatsappDir => write!(f, "WhatsApp dir not configured (Settings)"),
            WaError::NoRecipient => write!(f, "WhatsApp recipient not configured (Settings)"),
            WaError::Io(e) => write!(f, "I/O: {}", e),
            WaError::NotLinked(p) => write!(
                f,
                "Not linked. Click 'Link WhatsApp' in Settings (auth dir: {})",
                p.display()
            ),
            WaError::NodeNotFound => write!(f, "node not found in PATH (install Node.js)"),
            WaError::Spawn(e) => write!(f, "spawn failed: {}", e),
            WaError::Failed { code, stderr } => write!(
                f,
                "exited with {:?}{}",
                code,
                if stderr.is_empty() { String::new() } else { format!(": {}", stderr.trim()) },
            ),
        }
    }
}

/// True if `dir` looks ready: has package.json + send-text.mjs + node_modules.
pub fn is_provisioned(dir: &Path) -> bool {
    dir.join("send-text.mjs").exists()
        && dir.join("package.json").exists()
        && dir.join("node_modules").exists()
}

/// True if `dir` has an auth/ with a creds.json — means a session is linked.
pub fn is_linked(dir: &Path) -> bool {
    dir.join("auth").join("creds.json").exists()
}

/// Setup events streamed to the GUI while npm install runs.
pub enum SetupEvent {
    Log(String),
    Done,
    Error(String),
}

/// Provision `dir`: write package.json, send-text.mjs, login.mjs, then
/// run `npm install`. Streams progress via `tx`.
///
/// Run on a worker thread.
pub fn setup(dir: PathBuf, tx: Sender<SetupEvent>) {
    let _ = tx.send(SetupEvent::Log(format!("Provisioning {}", dir.display())));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        let _ = tx.send(SetupEvent::Error(format!("create_dir_all: {}", e)));
        return;
    }
    if let Err(e) = write_assets(&dir) {
        let _ = tx.send(SetupEvent::Error(e));
        return;
    }
    let _ = tx.send(SetupEvent::Log("Wrote package.json + send-text.mjs + login.mjs".into()));
    let _ = tx.send(SetupEvent::Log("Running: npm install --omit=dev --no-audit --no-fund".into()));

    let npm = if cfg!(target_os = "windows") { "npm.cmd" } else { "npm" };
    let mut cmd = Command::new(npm);
    cmd.current_dir(&dir)
        .args(["install", "--omit=dev", "--no-audit", "--no-fund"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let err = if e.kind() == std::io::ErrorKind::NotFound {
                "npm not found in PATH (install Node.js + npm)".to_string()
            } else {
                format!("npm spawn failed: {}", e)
            };
            let _ = tx.send(SetupEvent::Error(err));
            return;
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let h_out = stdout.map(|r| std::thread::spawn(move || {
        let mut buf = BufReader::new(r);
        let mut line = String::new();
        while buf.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
            let _ = tx_o.send(SetupEvent::Log(line.trim_end_matches(['\r', '\n']).to_string()));
            line.clear();
        }
    }));
    let h_err = stderr.map(|r| std::thread::spawn(move || {
        let mut buf = BufReader::new(r);
        let mut line = String::new();
        while buf.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
            let _ = tx_e.send(SetupEvent::Log(line.trim_end_matches(['\r', '\n']).to_string()));
            line.clear();
        }
    }));
    let status = child.wait();
    if let Some(h) = h_out { let _ = h.join(); }
    if let Some(h) = h_err { let _ = h.join(); }
    match status {
        Ok(s) if s.success() => { let _ = tx.send(SetupEvent::Done); }
        Ok(s) => { let _ = tx.send(SetupEvent::Error(format!("npm install exited with {:?}", s.code()))); }
        Err(e) => { let _ = tx.send(SetupEvent::Error(format!("npm wait failed: {}", e))); }
    }
}

fn write_assets(dir: &Path) -> Result<(), String> {
    use std::fs::write;
    write(dir.join("package.json"), PACKAGE_JSON).map_err(|e| format!("package.json: {}", e))?;
    write(dir.join("send-text.mjs"), SEND_TEXT_MJS).map_err(|e| format!("send-text.mjs: {}", e))?;
    write(dir.join("login.mjs"), LOGIN_MJS).map_err(|e| format!("login.mjs: {}", e))?;
    write(dir.join("list-recipients.mjs"), LIST_RECIPIENTS_MJS).map_err(|e| format!("list-recipients.mjs: {}", e))?;
    Ok(())
}

/// Re-write only the .mjs scripts (used to refresh after a new release
/// without re-running `npm install`). No-op if dir doesn't exist yet.
pub fn refresh_scripts(dir: &Path) -> Result<(), String> {
    if !dir.exists() { return Ok(()); }
    use std::fs::write;
    write(dir.join("send-text.mjs"), SEND_TEXT_MJS).map_err(|e| format!("send-text.mjs: {}", e))?;
    write(dir.join("login.mjs"), LOGIN_MJS).map_err(|e| format!("login.mjs: {}", e))?;
    write(dir.join("list-recipients.mjs"), LIST_RECIPIENTS_MJS).map_err(|e| format!("list-recipients.mjs: {}", e))?;
    Ok(())
}

/// Login events streamed while the QR scan flow runs.
pub enum LoginEvent {
    Log(String),
    /// ASCII QR code (multi-line). The full block is one event.
    Qr(String),
    Linked,
    Error(String),
}

/// Run login.mjs in `dir`. Streams stdout/stderr — QR-code blocks are
/// detected by the QR-CODE-BEGIN/QR-CODE-END markers and buffered into
/// a single `LoginEvent::Qr` so the GUI can show it in a modal.
pub fn login(dir: PathBuf, tx: Sender<LoginEvent>) {
    if !dir.join("send-text.mjs").exists() || !dir.join("node_modules").exists() {
        let _ = tx.send(LoginEvent::Error(
            "WhatsApp dir not provisioned. Click 'Setup WhatsApp' first.".into(),
        ));
        return;
    }
    let mut cmd = Command::new("node");
    cmd.current_dir(&dir)
        .arg("login.mjs")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let err = if e.kind() == std::io::ErrorKind::NotFound {
                "node not found in PATH (install Node.js)".to_string()
            } else {
                format!("node spawn failed: {}", e)
            };
            let _ = tx.send(LoginEvent::Error(err));
            return;
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let h_out = stdout.map(|r| std::thread::spawn(move || {
        let mut buf = BufReader::new(r);
        let mut line = String::new();
        let mut qr_buf: Option<Vec<String>> = None;
        while buf.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
            let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
            if trimmed == "QR-CODE-BEGIN" {
                qr_buf = Some(Vec::new());
            } else if trimmed == "QR-CODE-END" {
                if let Some(buf) = qr_buf.take() {
                    let _ = tx_o.send(LoginEvent::Qr(buf.join("\n")));
                }
            } else if let Some(b) = qr_buf.as_mut() {
                b.push(trimmed);
            } else if trimmed == "LINKED" {
                let _ = tx_o.send(LoginEvent::Linked);
            } else {
                let _ = tx_o.send(LoginEvent::Log(trimmed));
            }
            line.clear();
        }
    }));
    let h_err = stderr.map(|r| std::thread::spawn(move || {
        let mut buf = BufReader::new(r);
        let mut line = String::new();
        while buf.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
            let _ = tx_e.send(LoginEvent::Log(line.trim_end_matches(['\r', '\n']).to_string()));
            line.clear();
        }
    }));
    let status = child.wait();
    if let Some(h) = h_out { let _ = h.join(); }
    if let Some(h) = h_err { let _ = h.join(); }
    match status {
        Ok(s) if s.success() => { /* Linked event already sent */ }
        Ok(s) => { let _ = tx.send(LoginEvent::Error(format!("login.mjs exited with {:?}", s.code()))); }
        Err(e) => { let _ = tx.send(LoginEvent::Error(format!("login wait failed: {}", e))); }
    }
}

/// Send `message` to `recipient` using the configured WhatsApp dir.
/// Blocks until the node process exits — caller should run on a worker
/// thread.
pub fn send_text(
    whatsapp_dir: &str,
    recipient: &str,
    message: &str,
) -> Result<(), WaError> {
    if whatsapp_dir.trim().is_empty() {
        return Err(WaError::NoWhatsappDir);
    }
    if recipient.trim().is_empty() {
        return Err(WaError::NoRecipient);
    }
    let dir = Path::new(whatsapp_dir.trim());
    if !dir.exists() {
        return Err(WaError::Io(format!("dir does not exist: {}", dir.display())));
    }
    if !is_provisioned(dir) {
        return Err(WaError::Io(
            "WhatsApp dir is not provisioned. Click 'Setup WhatsApp' in Settings.".into(),
        ));
    }
    if !is_linked(dir) {
        return Err(WaError::NotLinked(dir.join("auth")));
    }

    let output = Command::new("node")
        .current_dir(dir)
        .arg("send-text.mjs")
        .arg(recipient.trim())
        .arg(message)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                WaError::NodeNotFound
            } else {
                WaError::Spawn(e.to_string())
            }
        })?;

    if output.status.success() {
        Ok(())
    } else {
        Err(WaError::Failed {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct Recipient {
    pub jid: String,
    #[allow(dead_code)]
    pub kind: String,
    pub name: String,
}

/// Run `list-recipients.mjs` and parse `JID|TYPE|NAME` lines into
/// `Recipient`s. Blocks — caller should use a worker thread.
pub fn list_recipients(whatsapp_dir: &str) -> Result<Vec<Recipient>, WaError> {
    if whatsapp_dir.trim().is_empty() {
        return Err(WaError::NoWhatsappDir);
    }
    let dir = Path::new(whatsapp_dir.trim());
    if !dir.exists() {
        return Err(WaError::Io(format!("dir does not exist: {}", dir.display())));
    }
    if !is_provisioned(dir) {
        return Err(WaError::Io(
            "WhatsApp dir is not provisioned. Click 'Setup WhatsApp' in Settings.".into(),
        ));
    }
    if !is_linked(dir) {
        return Err(WaError::NotLinked(dir.join("auth")));
    }
    if !dir.join("list-recipients.mjs").exists() {
        return Err(WaError::Io(
            "list-recipients.mjs missing — click 'Setup WhatsApp' to refresh helpers.".into(),
        ));
    }

    let output = Command::new("node")
        .current_dir(dir)
        .arg("list-recipients.mjs")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                WaError::NodeNotFound
            } else {
                WaError::Spawn(e.to_string())
            }
        })?;

    if !output.status.success() {
        return Err(WaError::Failed {
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let mut out = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.splitn(3, '|');
        let jid = parts.next().unwrap_or("").trim();
        let kind = parts.next().unwrap_or("").trim();
        let name = parts.next().unwrap_or("").trim();
        if jid.is_empty() { continue; }
        out.push(Recipient {
            jid: jid.to_string(),
            kind: kind.to_string(),
            name: name.to_string(),
        });
    }
    let _ = save_cached_recipients(dir, &out);
    Ok(out)
}

/// Path of the recipient cache (one JSON file per linked account).
fn recipient_cache_path(dir: &Path) -> PathBuf {
    dir.join("recipients.json")
}

/// Persist the fetched recipient list so the next picker open is instant.
/// Failure is non-fatal — caller ignores the error.
fn save_cached_recipients(dir: &Path, items: &[Recipient]) -> std::io::Result<()> {
    let mut s = String::from("[");
    for (i, r) in items.iter().enumerate() {
        if i > 0 { s.push(','); }
        s.push_str(&format!(
            "{{\"jid\":{},\"kind\":{},\"name\":{}}}",
            json_string(&r.jid),
            json_string(&r.kind),
            json_string(&r.name),
        ));
    }
    s.push(']');
    std::fs::write(recipient_cache_path(dir), s)
}

/// Read the cached recipient list (instant). Returns an empty vec if the
/// cache is missing or unreadable.
pub fn load_cached_recipients(whatsapp_dir: &str) -> Vec<Recipient> {
    if whatsapp_dir.trim().is_empty() { return Vec::new(); }
    let path = recipient_cache_path(Path::new(whatsapp_dir.trim()));
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new(); };
    parse_recipients_json(&text).unwrap_or_default()
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Tiny parser for the exact shape `save_cached_recipients` writes —
/// avoids a serde_json dep just for this. Returns None on malformed
/// input, callers fall back to an empty list.
fn parse_recipients_json(s: &str) -> Option<Vec<Recipient>> {
    let s = s.trim();
    let s = s.strip_prefix('[')?.strip_suffix(']')?;
    let mut out = Vec::new();
    if s.trim().is_empty() { return Some(out); }
    let mut depth = 0i32;
    let mut start = 0usize;
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'{' => { if depth == 0 { start = i; } depth += 1; }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(r) = parse_recipient_object(&s[start..=i]) {
                        out.push(r);
                    }
                }
            }
            _ => {}
        }
    }
    Some(out)
}

fn parse_recipient_object(s: &str) -> Option<Recipient> {
    let jid = extract_json_string(s, "jid")?;
    let kind = extract_json_string(s, "kind").unwrap_or_default();
    let name = extract_json_string(s, "name").unwrap_or_default();
    Some(Recipient { jid, kind, name })
}

fn extract_json_string(s: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\":", key);
    let start = s.find(&needle)? + needle.len();
    let rest = s[start..].trim_start();
    let rest = rest.strip_prefix('"')?;
    let bytes = rest.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' { return Some(out); }
        if b == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'"' => out.push('"'),
                b'\\' => out.push('\\'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                b'u' => {
                    if i + 5 < bytes.len() {
                        let hex = std::str::from_utf8(&bytes[i + 2..i + 6]).ok()?;
                        let code = u32::from_str_radix(hex, 16).ok()?;
                        if let Some(c) = char::from_u32(code) { out.push(c); }
                        i += 6; continue;
                    }
                    return None;
                }
                other => out.push(other as char),
            }
            i += 2; continue;
        }
        // Take a UTF-8 char from rest at offset i
        let remainder = &rest[i..];
        let c = remainder.chars().next()?;
        out.push(c);
        i += c.len_utf8();
    }
    None
}

