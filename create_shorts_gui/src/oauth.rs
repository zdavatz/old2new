//! Google OAuth2 "installed application" flow + persistent refresh
//! token. The GUI calls `run_auth_flow` once when the user clicks
//! "Sign in to YouTube"; afterwards every upload calls
//! `refresh_access_token` to get a short-lived bearer.

use crate::settings::token_path;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

pub const REDIRECT_PORT: u16 = 8093;
pub const SCOPE: &str = "https://www.googleapis.com/auth/youtube.upload https://www.googleapis.com/auth/youtube";

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Token {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    /// Unix epoch seconds when `access_token` expires.
    #[serde(default)]
    pub expires_at: i64,
}

pub fn load_token() -> Option<Token> {
    let s = fs::read_to_string(token_path()).ok()?;
    serde_json::from_str(&s).ok()
}

pub fn save_token(tok: &Token) -> std::io::Result<()> {
    if let Some(parent) = token_path().parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(token_path(), serde_json::to_string_pretty(tok).unwrap_or_default())
}

pub fn redirect_uri() -> String {
    format!("http://127.0.0.1:{}/", REDIRECT_PORT)
}

pub fn auth_url(client_id: &str) -> String {
    format!(
        "https://accounts.google.com/o/oauth2/v2/auth?response_type=code&client_id={}&redirect_uri={}&scope={}&access_type=offline&prompt=consent",
        urlencoding::encode(client_id),
        urlencoding::encode(&redirect_uri()),
        urlencoding::encode(SCOPE),
    )
}

/// Block on a localhost listener until the OAuth callback hits, then
/// exchange the code for a refresh token. Runs synchronously on the
/// caller's thread — meant to be invoked from a worker thread spawned
/// by the GUI so the UI stays responsive.
pub fn run_auth_flow(
    client_id: &str,
    client_secret: &str,
    log: impl Fn(String),
) -> Result<Token, String> {
    let url = auth_url(client_id);
    log(format!("Opening browser: {}", url));
    if let Err(e) = open::that(&url) {
        log(format!("Could not auto-open browser ({}). Open the URL above manually.", e));
    }

    let listener = TcpListener::bind(("127.0.0.1", REDIRECT_PORT))
        .map_err(|e| format!("Cannot bind 127.0.0.1:{} ({}). Is another sign-in already in progress?", REDIRECT_PORT, e))?;
    listener.set_nonblocking(false).ok();

    log(format!("Waiting for Google to redirect to {}…", redirect_uri()));
    let (mut socket, _) = listener
        .accept()
        .map_err(|e| format!("accept failed: {}", e))?;
    socket
        .set_read_timeout(Some(Duration::from_secs(120)))
        .ok();

    let mut buf = [0u8; 8192];
    let n = socket.read(&mut buf).map_err(|e| format!("read failed: {}", e))?;
    let request = String::from_utf8_lossy(&buf[..n]).to_string();

    let code = parse_code(&request)
        .ok_or_else(|| format!("No `code` in callback. Raw request:\n{}", request))?;

    let body = b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<!doctype html><meta charset=utf-8><title>Signed in</title><body style=\"font-family:sans-serif;padding:3em\"><h1>Signed in!</h1><p>You can close this window and return to <em>create_shorts</em>.</p></body>";
    socket.write_all(body).ok();
    drop(socket);

    log("Authorization code received. Exchanging for refresh token…".to_string());

    let tok = exchange_code(client_id, client_secret, &code)?;
    save_token(&tok).map_err(|e| format!("Could not save token: {}", e))?;
    log(format!("Saved refresh token to {}", token_path().display()));
    Ok(tok)
}

fn parse_code(request: &str) -> Option<String> {
    let first_line = request.lines().next()?;
    let target = first_line.split_whitespace().nth(1)?;
    let query = target.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("code=") {
            return urlencoding::decode(v).ok().map(|c| c.into_owned());
        }
    }
    None
}

fn exchange_code(
    client_id: &str,
    client_secret: &str,
    code: &str,
) -> Result<Token, String> {
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code", code),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri().as_str()),
        ])
        .send()
        .map_err(|e| format!("token request failed: {}", e))?;

    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .map_err(|e| format!("token response parse failed: {}", e))?;

    if !status.is_success() {
        return Err(format!("token exchange {}: {}", status, body));
    }

    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or("no access_token in response")?
        .to_string();
    let refresh_token = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .ok_or("no refresh_token in response — did you forget access_type=offline?")?
        .to_string();
    let expires_in = body
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(3500);
    let expires_at = now_secs() + expires_in - 60;

    Ok(Token { access_token, refresh_token, expires_at })
}

/// Returns a fresh access token, refreshing via the saved refresh token
/// if the current one is missing or near expiry.
pub fn refresh_access_token(
    client_id: &str,
    client_secret: &str,
) -> Result<String, String> {
    let mut tok = load_token().ok_or("not signed in (no token file)")?;
    if !tok.access_token.is_empty() && tok.expires_at > now_secs() + 30 {
        return Ok(tok.access_token);
    }
    if tok.refresh_token.is_empty() {
        return Err("no refresh token saved — please sign in again".to_string());
    }

    let client = reqwest::blocking::Client::new();
    let resp = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("refresh_token", tok.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .map_err(|e| format!("refresh request failed: {}", e))?;

    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .map_err(|e| format!("refresh response parse failed: {}", e))?;

    if !status.is_success() {
        return Err(format!("token refresh {}: {}", status, body));
    }

    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or("no access_token in refresh response")?
        .to_string();
    let expires_in = body
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(3500);

    tok.access_token = access_token.clone();
    tok.expires_at = now_secs() + expires_in - 60;
    save_token(&tok).ok();
    Ok(access_token)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
