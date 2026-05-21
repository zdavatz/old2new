//! Posts a YouTube URL to https://davaz.com/api/videos so the new
//! short shows up on Jürg's site without him having to add it by
//! hand. Bearer-token auth — token lives in Settings, sourced from
//! the server's `etc/api_tokens`.
//!
//! On success the server returns 201 with `{id, artgroup_id, title,
//! url, duration_seconds, tag_added}`. We surface those fields to the
//! caller so the GUI can show a concrete confirmation in the log.

use serde::Deserialize;
use std::time::Duration;

const ENDPOINT: &str = "https://davaz.com/api/videos";

#[derive(Clone, Debug, Deserialize)]
pub struct PostResponse {
    #[serde(default)]
    pub id: Option<serde_json::Value>,
    #[serde(default)]
    pub artgroup_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub duration_seconds: Option<u64>,
    /// `null` when no tag was added; otherwise `{bucket, label}`.
    #[serde(default)]
    pub tag_added: Option<serde_json::Value>,
}

impl PostResponse {
    pub fn id_str(&self) -> String {
        match &self.id {
            Some(serde_json::Value::Number(n)) => n.to_string(),
            Some(serde_json::Value::String(s)) => s.clone(),
            _ => "?".to_string(),
        }
    }
}

#[derive(Debug)]
pub enum PostError {
    /// Token field empty — caller should tell user to set it in Settings.
    NoToken,
    /// 401/403 — token missing or wrong on the server.
    Unauthorized,
    /// 409 — video already exists. Carries the existing id from server.
    AlreadyExists(String),
    /// 422/4xx with body — surface server's error message verbatim.
    BadRequest(String),
    /// 5xx or network failure.
    Server(String),
}

impl std::fmt::Display for PostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PostError::NoToken => write!(f, "davaz.com token not set — open Settings"),
            PostError::Unauthorized => write!(f, "davaz.com rejected the token (401/403)"),
            PostError::AlreadyExists(id) => write!(f, "video already on davaz.com (id={})", id),
            PostError::BadRequest(s) => write!(f, "{}", s),
            PostError::Server(s) => write!(f, "{}", s),
        }
    }
}

/// POSTs `{url}` to davaz.com. Blocking. The server picks the tag
/// color by scanning the YouTube description (which it fetches from
/// the YouTube API) for a recognized color word — so this client
/// doesn't need to know the supported colors. Adding a new color is
/// a server-only change on davaz.com.
pub fn post_video(token: &str, url: &str) -> Result<PostResponse, PostError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(PostError::NoToken);
    }

    let mut body = serde_json::Map::new();
    body.insert("url".into(), serde_json::Value::String(url.to_string()));

    let client = reqwest::blocking::Client::builder()
        .user_agent(format!("create_shorts_gui/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| PostError::Server(format!("http client: {}", e)))?;

    let resp = client
        .post(ENDPOINT)
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&body).unwrap_or_default())
        .send()
        .map_err(|e| PostError::Server(format!("POST {}: {}", ENDPOINT, e)))?;

    let status = resp.status();
    let text = resp.text().unwrap_or_default();

    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(PostError::Unauthorized);
    }

    if status.as_u16() == 409 {
        let existing = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("id").cloned())
            .map(|v| match v {
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::String(s) => s,
                _ => "?".to_string(),
            })
            .unwrap_or_else(|| "?".to_string());
        return Err(PostError::AlreadyExists(existing));
    }

    if status.is_client_error() {
        let msg = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(|s| s.to_string()))
            .unwrap_or_else(|| format!("HTTP {}: {}", status.as_u16(), text));
        return Err(PostError::BadRequest(msg));
    }

    if !status.is_success() {
        return Err(PostError::Server(format!("HTTP {}: {}", status.as_u16(), text)));
    }

    serde_json::from_str::<PostResponse>(&text)
        .map_err(|e| PostError::Server(format!("parse response: {} ({})", e, text)))
}
