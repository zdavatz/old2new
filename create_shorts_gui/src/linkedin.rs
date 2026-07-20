//! Posts a video natively to LinkedIn — the same flow as the `li_push`
//! CLI (`li_push_rs/src/main.rs`), ported from tokio/async `reqwest` to
//! the blocking client the rest of this GUI uses (`oauth.rs`,
//! `youtube.rs`, `davaz.rs`), and shaped as a library: every failure is
//! a `Result`, progress goes to a callback, nothing prints or exits.
//!
//! Two halves:
//!
//! 1. **OAuth** — LinkedIn's authorization-code flow with the
//!    `openid profile w_member_social` scopes, callback on
//!    `localhost:8092` (the port registered on the app at
//!    developer.linkedin.com). Mirrors `oauth.rs`'s Google flow, so the
//!    credentials live in Settings and the token in the config dir —
//!    the CLI's cwd/`$HOME` `linkedin_token.json` is *not* used, since
//!    Jürg runs the .app without a repo checkout.
//! 2. **Upload** — LinkedIn's 4-step Videos API: initialize → upload the
//!    file in the chunks the server dictates (collecting an ETag per
//!    chunk) → finalize → create the post that references the video URN.
//!
//! Unlike the CLI this never decodes the OpenID `id_token` JWT to find
//! the member URN (that needed a base64 dependency); it asks
//! `/v2/userinfo` — which the CLI does anyway, right after the JWT, and
//! lets its answer win.

use crate::settings::{self, Settings};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Must match the redirect URI registered on the LinkedIn app.
pub const REDIRECT_PORT: u16 = 8092;
pub const SCOPE: &str = "openid profile w_member_social";
/// LinkedIn's versioned REST API. Bumping this is a deliberate act — the
/// request/response shapes below are what this version speaks.
const LINKEDIN_VERSION: &str = "202603";
/// LinkedIn rejects video longer than 30 minutes or larger than 500 MB.
/// We check both before spending time on an upload that can't succeed.
pub const MAX_SECS: f64 = 1800.0;
pub const MAX_BYTES: u64 = 500 * 1024 * 1024;

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Token {
    #[serde(default)]
    pub access_token: String,
    /// LinkedIn only issues refresh tokens to approved apps; commonly empty,
    /// in which case the access token is used until it expires and the user
    /// signs in again (same behaviour as the CLI).
    #[serde(default)]
    pub refresh_token: String,
    /// Unix epoch seconds when `access_token` expires.
    #[serde(default)]
    pub expires_at: i64,
    /// The member URN suffix — `urn:li:person:<person_id>` authors the post.
    #[serde(default)]
    pub person_id: String,
}

pub fn token_path() -> PathBuf {
    settings::config_dir().join("linkedin_token.json")
}

pub fn load_token() -> Option<Token> {
    let s = std::fs::read_to_string(token_path()).ok()?;
    serde_json::from_str(&s).ok()
}

pub fn save_token(tok: &Token) -> std::io::Result<()> {
    if let Some(parent) = token_path().parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(token_path(), serde_json::to_string_pretty(tok).unwrap_or_default())
}

/// True when a usable sign-in is on disk. `person_id` matters as much as the
/// token — without it there's no author URN to post as.
pub fn is_signed_in() -> bool {
    matches!(load_token(), Some(t) if !t.access_token.is_empty() && !t.person_id.is_empty())
}

/// One successful LinkedIn post, keyed by the short's YouTube URL. Stored
/// append-only, one JSON object per line, in `linkedin_posts.jsonl` (same
/// pattern as `uploads.jsonl`) — this is what makes the "✅ Posted to
/// LinkedIn" state survive an app restart, the GUI's counterpart to the
/// CLI's `~/li_push_log.jsonl` dedup log.
#[derive(Clone, Serialize, Deserialize)]
pub struct PostRecord {
    pub timestamp: String,
    pub youtube_url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub post_url: String,
}

pub fn posts_log_path() -> PathBuf {
    settings::config_dir().join("linkedin_posts.jsonl")
}

pub fn record_posted(rec: &PostRecord) -> std::io::Result<()> {
    let path = posts_log_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let line = serde_json::to_string(rec).unwrap_or_default();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{}", line)
}

/// YouTube URL → LinkedIn post URL for every recorded post. Unparseable
/// lines are skipped (append-only file, a torn tail line must not take the
/// rest with it); a URL posted twice keeps the newest post URL.
pub fn load_posted() -> std::collections::HashMap<String, String> {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(posts_log_path()) else {
        return Default::default();
    };
    std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<PostRecord>(&l).ok())
        .map(|r| (r.youtube_url, r.post_url))
        .collect()
}

pub fn redirect_uri() -> String {
    format!("http://localhost:{}/callback", REDIRECT_PORT)
}

pub fn auth_url(client_id: &str) -> String {
    format!(
        "https://www.linkedin.com/oauth/v2/authorization?response_type=code&client_id={}&redirect_uri={}&scope={}",
        urlencoding::encode(client_id),
        urlencoding::encode(&redirect_uri()),
        urlencoding::encode(SCOPE),
    )
}

/// Bounded-timeout blocking client. The token/metadata calls are small; a
/// stalled connection should fail in seconds rather than hang the worker.
fn api_client(timeout: Duration) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(format!("create_shorts_gui/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .build()
        .map_err(|e| format!("http client build: {}", e))
}

/// Open the browser, wait for LinkedIn to redirect back to
/// `localhost:8092/callback`, exchange the code, resolve the member URN and
/// save the token. Blocking — call it from a worker thread.
pub fn run_auth_flow(
    client_id: &str,
    client_secret: &str,
    log: impl Fn(String),
) -> Result<Token, String> {
    // Bind *before* opening the browser: if the port is busy, the user should
    // find out now rather than after authorizing in LinkedIn.
    let listener = TcpListener::bind(("127.0.0.1", REDIRECT_PORT)).map_err(|e| {
        format!(
            "Cannot bind 127.0.0.1:{} ({}). Is another LinkedIn sign-in (or the li_push CLI) already running?",
            REDIRECT_PORT, e
        )
    })?;

    let url = auth_url(client_id);
    log(format!("Opening browser: {}", url));
    if let Err(e) = open::that(&url) {
        log(format!("Could not auto-open browser ({}). Open the URL above manually.", e));
    }

    log(format!("Waiting for LinkedIn to redirect to {}…", redirect_uri()));
    let (mut socket, _) = listener.accept().map_err(|e| format!("accept failed: {}", e))?;
    socket.set_read_timeout(Some(Duration::from_secs(120))).ok();

    let mut buf = [0u8; 8192];
    let n = socket.read(&mut buf).map_err(|e| format!("read failed: {}", e))?;
    let request = String::from_utf8_lossy(&buf[..n]).to_string();

    let code = match parse_query_param(&request, "code") {
        Some(c) => c,
        None => {
            // LinkedIn reports refusals as ?error=…&error_description=…
            let msg = match parse_query_param(&request, "error_description")
                .or_else(|| parse_query_param(&request, "error"))
            {
                // Query strings write spaces as `+`, which percent-decoding
                // leaves alone. Only done here, on text headed for the user —
                // never on `code`, where a literal `+` must survive.
                Some(e) => format!("LinkedIn refused the sign-in: {}", e.replace('+', " ")),
                None => format!("No `code` in callback. Raw request:\n{}", request),
            };
            let body = b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<!doctype html><meta charset=utf-8><title>Sign-in failed</title><body style=\"font-family:sans-serif;padding:3em\"><h1>Sign-in failed</h1><p>Return to <em>create_shorts</em> for the reason.</p></body>";
            socket.write_all(body).ok();
            return Err(msg);
        }
    };

    let body = b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<!doctype html><meta charset=utf-8><title>Signed in</title><body style=\"font-family:sans-serif;padding:3em\"><h1>Signed in to LinkedIn!</h1><p>You can close this window and return to <em>create_shorts</em>.</p></body>";
    socket.write_all(body).ok();
    drop(socket);

    log("Authorization code received. Exchanging for an access token…".to_string());
    let mut tok = exchange_code(client_id, client_secret, &code)?;

    log("Fetching your LinkedIn member id…".to_string());
    tok.person_id = fetch_person_id(&tok.access_token)?;

    save_token(&tok).map_err(|e| format!("Could not save token: {}", e))?;
    log(format!("Saved LinkedIn token to {}", token_path().display()));
    Ok(tok)
}

/// Pull a query parameter out of the raw `GET /callback?…` request line.
fn parse_query_param(request: &str, key: &str) -> Option<String> {
    let first_line = request.lines().next()?;
    let target = first_line.split_whitespace().nth(1)?;
    let query = target.split_once('?')?.1;
    let prefix = format!("{}=", key);
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix(&prefix) {
            return urlencoding::decode(v).ok().map(|c| c.into_owned());
        }
    }
    None
}

fn exchange_code(client_id: &str, client_secret: &str, code: &str) -> Result<Token, String> {
    let client = api_client(Duration::from_secs(30))?;
    let resp = client
        .post("https://www.linkedin.com/oauth/v2/accessToken")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri().as_str()),
            ("client_id", client_id),
            ("client_secret", client_secret),
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
    if let Some(err) = body.get("error") {
        let desc = body
            .get("error_description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return Err(format!("token exchange failed: {} {}", err, desc));
    }

    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or("no access_token in response")?
        .to_string();
    let expires_in = body.get("expires_in").and_then(|v| v.as_i64()).unwrap_or(3600);

    Ok(Token {
        access_token,
        refresh_token: body
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        expires_at: now_secs() + expires_in - 60,
        person_id: String::new(),
    })
}

/// Resolve the member URN suffix. `/v2/userinfo` (OpenID) is the documented
/// route and needs only the `openid`/`profile` scopes we request; `/v2/me`
/// is the legacy fallback the CLI also tries.
fn fetch_person_id(access_token: &str) -> Result<String, String> {
    let client = api_client(Duration::from_secs(30))?;

    if let Ok(resp) = client
        .get("https://api.linkedin.com/v2/userinfo")
        .bearer_auth(access_token)
        .send()
    {
        if resp.status().is_success() {
            if let Ok(v) = resp.json::<serde_json::Value>() {
                if let Some(sub) = v.get("sub").and_then(|s| s.as_str()) {
                    if !sub.is_empty() {
                        return Ok(sub.to_string());
                    }
                }
            }
        }
    }

    if let Ok(resp) = client
        .get("https://api.linkedin.com/v2/me")
        .bearer_auth(access_token)
        .send()
    {
        if resp.status().is_success() {
            if let Ok(v) = resp.json::<serde_json::Value>() {
                if let Some(id) = v.get("id").and_then(|s| s.as_str()) {
                    if !id.is_empty() {
                        return Ok(id.to_string());
                    }
                }
            }
        }
    }

    Err("Could not read your LinkedIn member id (userinfo and /v2/me both failed). \
         Check that the app has the 'Sign In with LinkedIn using OpenID Connect' product enabled."
        .to_string())
}

/// A token that's good to use now: refreshed when we hold a refresh token and
/// the current one is near expiry, otherwise returned as-is.
fn usable_token(client_id: &str, client_secret: &str) -> Result<Token, String> {
    let tok = load_token().ok_or("not signed in to LinkedIn — open Settings and sign in")?;
    if tok.person_id.is_empty() {
        return Err("no LinkedIn member id saved — please sign in to LinkedIn again".to_string());
    }
    if tok.access_token.is_empty() {
        return Err("no LinkedIn access token saved — please sign in to LinkedIn again".to_string());
    }
    // Still valid, or nothing we can do about it (no refresh token issued).
    if tok.expires_at > now_secs() + 30 || tok.refresh_token.is_empty() {
        return Ok(tok);
    }

    let client = api_client(Duration::from_secs(30))?;
    let resp = client
        .post("https://www.linkedin.com/oauth/v2/accessToken")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", tok.refresh_token.as_str()),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ])
        .send();

    let refreshed = resp
        .ok()
        .and_then(|r| r.json::<serde_json::Value>().ok())
        .and_then(|body| {
            let at = body.get("access_token")?.as_str()?.to_string();
            if at.is_empty() {
                return None;
            }
            Some(Token {
                access_token: at,
                refresh_token: body
                    .get("refresh_token")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&tok.refresh_token)
                    .to_string(),
                expires_at: now_secs()
                    + body.get("expires_in").and_then(|v| v.as_i64()).unwrap_or(3600)
                    - 60,
                person_id: tok.person_id.clone(),
            })
        });

    match refreshed {
        Some(t) => {
            save_token(&t).ok();
            Ok(t)
        }
        // The old token may still have life in it; let the API be the judge.
        None => Ok(tok),
    }
}

#[derive(Debug)]
pub enum PostError {
    /// Credentials missing from Settings.
    NoCredentials,
    /// No saved sign-in (or it lacks a member id).
    NotSignedIn(String),
    /// File is longer than 30 min or bigger than 500 MB.
    TooBig(String),
    /// 401/403 — the token expired or lacks `w_member_social`.
    Unauthorized(String),
    /// The user hit Cancel.
    Cancelled,
    /// Anything else: network, 4xx/5xx with a body, unreadable file.
    Failed(String),
}

impl std::fmt::Display for PostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PostError::NoCredentials => write!(
                f,
                "LinkedIn Client ID/Secret not set — open Settings"
            ),
            PostError::NotSignedIn(s) => write!(f, "{}", s),
            PostError::TooBig(s) => write!(f, "{}", s),
            PostError::Unauthorized(s) => write!(
                f,
                "LinkedIn rejected the token ({}) — sign in to LinkedIn again in Settings",
                s
            ),
            PostError::Cancelled => write!(f, "cancelled"),
            PostError::Failed(s) => write!(f, "{}", s),
        }
    }
}

/// Refuse a file LinkedIn is guaranteed to reject, before uploading it.
pub fn check_limits(file: &Path, duration_secs: Option<f64>) -> Result<u64, PostError> {
    let size = std::fs::metadata(file)
        .map_err(|e| PostError::Failed(format!("stat {}: {}", file.display(), e)))?
        .len();
    if size > MAX_BYTES {
        return Err(PostError::TooBig(format!(
            "video is {:.0} MB — LinkedIn's limit is 500 MB",
            size as f64 / 1_048_576.0
        )));
    }
    if let Some(d) = duration_secs {
        if d > MAX_SECS {
            return Err(PostError::TooBig(format!(
                "video is {:.1} min — LinkedIn's limit is 30 min",
                d / 60.0
            )));
        }
    }
    Ok(size)
}

/// Post `file` to LinkedIn as a native video and return the post's URL.
///
/// `commentary` is the post text, `title` the video's title. `progress` is
/// called with (bytes sent, total) after each chunk; `cancel` is checked
/// between chunks, so a cancelled upload stops within one chunk.
pub fn post_video(
    settings: &Settings,
    file: &Path,
    title: &str,
    commentary: &str,
    visibility: &str,
    cancel: &Arc<AtomicBool>,
    progress: impl Fn(u64, u64),
) -> Result<String, PostError> {
    let client_id = settings.linkedin_client_id.trim();
    let client_secret = settings.linkedin_client_secret.trim();
    if client_id.is_empty() || client_secret.is_empty() {
        return Err(PostError::NoCredentials);
    }

    let file_size = check_limits(file, None)?;
    let token = usable_token(client_id, client_secret).map_err(PostError::NotSignedIn)?;
    let owner = format!("urn:li:person:{}", token.person_id);

    // Uploads are slow by nature — a per-request timeout that fits a 4 MB
    // chunk would kill a healthy transfer on a slow line, so this client is
    // generous where `api_client`'s default is not.
    let client = api_client(Duration::from_secs(300)).map_err(PostError::Failed)?;

    if cancel.load(Ordering::SeqCst) {
        return Err(PostError::Cancelled);
    }

    // ── Step 1: initialize ────────────────────────────────────────────────
    let init_body = serde_json::json!({
        "initializeUploadRequest": {
            "owner": owner,
            "fileSizeBytes": file_size,
            "uploadCaptions": false,
            "uploadThumbnail": false
        }
    });
    let init = rest_post(&client, &token, "videos?action=initializeUpload", &init_body)?;
    let value = &init["value"];
    let video_urn = value["video"]
        .as_str()
        .ok_or_else(|| PostError::Failed(format!("no video URN in initialize response: {}", init)))?
        .to_string();
    let upload_token = value["uploadToken"].as_str().unwrap_or("").to_string();
    let instructions = value["uploadInstructions"]
        .as_array()
        .ok_or_else(|| PostError::Failed(format!("no uploadInstructions in response: {}", init)))?
        .clone();
    if instructions.is_empty() {
        return Err(PostError::Failed("LinkedIn returned no upload instructions".into()));
    }

    // ── Step 2: upload each chunk the server asked for, keeping its ETag ──
    let mut fh = std::fs::File::open(file)
        .map_err(|e| PostError::Failed(format!("open {}: {}", file.display(), e)))?;
    let mut etags: Vec<String> = Vec::new();
    let mut sent: u64 = 0;

    for (i, instruction) in instructions.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            return Err(PostError::Cancelled);
        }
        let upload_url = instruction["uploadUrl"]
            .as_str()
            .ok_or_else(|| PostError::Failed(format!("no uploadUrl in instruction {}", i)))?;
        let first = instruction["firstByte"].as_u64().unwrap_or(0);
        let last = instruction["lastByte"].as_u64().unwrap_or(0);
        if last < first {
            return Err(PostError::Failed(format!(
                "bad chunk range from LinkedIn: {}-{}",
                first, last
            )));
        }
        let len = (last - first + 1) as usize;

        fh.seek(SeekFrom::Start(first))
            .map_err(|e| PostError::Failed(format!("seek {}: {}", first, e)))?;
        let mut buf = vec![0u8; len];
        fh.read_exact(&mut buf)
            .map_err(|e| PostError::Failed(format!("read chunk {}: {}", i, e)))?;

        let resp = client
            .put(upload_url)
            .header("Content-Type", "application/octet-stream")
            .body(buf)
            .send()
            .map_err(|e| PostError::Failed(format!("chunk {} upload failed: {}", i, e)))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            return Err(PostError::Failed(format!(
                "chunk {}/{} failed ({}): {}",
                i + 1,
                instructions.len(),
                status,
                body
            )));
        }
        // Finalize rejects the upload unless every part is named by its ETag.
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .trim_matches('"')
            .to_string();
        etags.push(etag);

        sent += len as u64;
        progress(sent.min(file_size), file_size);
    }

    if cancel.load(Ordering::SeqCst) {
        return Err(PostError::Cancelled);
    }

    // ── Step 3: finalize ─────────────────────────────────────────────────
    let finalize_body = serde_json::json!({
        "finalizeUploadRequest": {
            "video": video_urn,
            "uploadToken": upload_token,
            "uploadedPartIds": etags
        }
    });
    rest_post(&client, &token, "videos?action=finalizeUpload", &finalize_body)?;

    // ── Step 4: create the post that shows the video ─────────────────────
    let post_body = serde_json::json!({
        "author": owner,
        "commentary": commentary,
        "visibility": visibility,
        "distribution": {
            "feedDistribution": "MAIN_FEED",
            "targetEntities": [],
            "thirdPartyDistributionChannels": []
        },
        "content": { "media": { "title": title, "id": video_urn } },
        "lifecycleState": "PUBLISHED",
        "isReshareDisabledByAuthor": false
    });

    let resp = client
        .post("https://api.linkedin.com/rest/posts")
        .bearer_auth(&token.access_token)
        .header("Content-Type", "application/json")
        .header("LinkedIn-Version", LINKEDIN_VERSION)
        .header("X-Restli-Protocol-Version", "2.0.0")
        .json(&post_body)
        .send()
        .map_err(|e| PostError::Failed(format!("create post failed: {}", e)))?;

    let status = resp.status();
    // The post's URN comes back in a header, not the body.
    let post_id = resp
        .headers()
        .get("x-restli-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let text = resp.text().unwrap_or_default();

    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(PostError::Unauthorized(status.to_string()));
    }
    if !status.is_success() {
        return Err(PostError::Failed(format!("create post {}: {}", status, text)));
    }
    if post_id.is_empty() {
        return Err(PostError::Failed(format!(
            "post created but LinkedIn returned no post id: {}",
            text
        )));
    }

    Ok(format!("https://www.linkedin.com/feed/update/{}/", post_id))
}

/// Metadata + a local copy of a YouTube video, for the case where the short
/// isn't on disk any more (an older one picked from History, say). Mirrors the
/// CLI's `li_push <youtube-id>` path: `--dump-json` first so an over-long or
/// over-large video is refused *before* the download, then fetch the file.
///
/// The caller owns the returned file and is expected to delete it.
pub fn download_youtube(
    url: &str,
    settings: &Settings,
    cancel: &Arc<AtomicBool>,
    log: impl Fn(String),
) -> Result<(PathBuf, String), PostError> {
    let mut meta_cmd = std::process::Command::new("yt-dlp");
    meta_cmd.args(["--dump-json", "--no-download", "--no-playlist"]);
    if !settings.cookies_browser.is_empty() {
        meta_cmd.arg("--cookies-from-browser").arg(&settings.cookies_browser);
    }
    meta_cmd.arg(url);
    let meta_out = meta_cmd.output().map_err(|e| {
        PostError::Failed(format!(
            "yt-dlp not found ({}). Install with `brew install yt-dlp`.",
            e
        ))
    })?;
    if !meta_out.status.success() {
        return Err(PostError::Failed(format!(
            "yt-dlp could not read {}: {}",
            url,
            String::from_utf8_lossy(&meta_out.stderr).trim()
        )));
    }
    let meta: serde_json::Value = serde_json::from_slice(&meta_out.stdout)
        .map_err(|e| PostError::Failed(format!("parse yt-dlp metadata: {}", e)))?;

    let title = meta["title"].as_str().unwrap_or_default().to_string();
    if let Some(d) = meta["duration"].as_f64() {
        if d > MAX_SECS {
            return Err(PostError::TooBig(format!(
                "video is {:.1} min — LinkedIn's limit is 30 min",
                d / 60.0
            )));
        }
    }

    if cancel.load(Ordering::SeqCst) {
        return Err(PostError::Cancelled);
    }

    let out = std::env::temp_dir().join(format!("create_shorts_linkedin_{}.mp4", std::process::id()));
    let _ = std::fs::remove_file(&out);
    log(format!("Downloading {} from YouTube for LinkedIn…", url));

    let mut cmd = std::process::Command::new("yt-dlp");
    cmd.args([
        "-f",
        "bestvideo[ext=mp4]+bestaudio[ext=m4a]/best[ext=mp4]/best",
        "--merge-output-format",
        "mp4",
        "--force-overwrites",
        "--no-playlist",
        "-o",
        out.to_str().ok_or_else(|| PostError::Failed("non-utf8 temp path".into()))?,
    ]);
    if !settings.cookies_browser.is_empty() {
        cmd.arg("--cookies-from-browser").arg(&settings.cookies_browser);
    }
    cmd.arg(url);
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    // yt-dlp drives ffmpeg as a grandchild, so Cancel has to take down the
    // whole process group — same reason `pipeline` does this.
    crate::pipeline::detach_group(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| PostError::Failed(format!("yt-dlp spawn failed: {}", e)))?;
    let stderr = child.stderr.take();
    let status = crate::pipeline::wait_or_cancel(&mut child, cancel);
    let err_text = stderr
        .map(|mut s| {
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            buf
        })
        .unwrap_or_default();

    if cancel.load(Ordering::SeqCst) {
        let _ = std::fs::remove_file(&out);
        return Err(PostError::Cancelled);
    }
    let status = status.map_err(|e| {
        let _ = std::fs::remove_file(&out);
        PostError::Failed(e)
    })?;
    if !status.success() {
        let _ = std::fs::remove_file(&out);
        let tail: Vec<&str> = err_text.lines().rev().take(3).collect();
        return Err(PostError::Failed(format!(
            "yt-dlp download failed: {}",
            tail.into_iter().rev().collect::<Vec<_>>().join(" — ")
        )));
    }

    // The pre-check used yt-dlp's *estimate*; this is the real thing.
    match check_limits(&out, None) {
        Ok(size) => log(format!("Downloaded {:.1} MB", size as f64 / 1_048_576.0)),
        Err(e) => {
            let _ = std::fs::remove_file(&out);
            return Err(e);
        }
    }
    Ok((out, title))
}

/// POST a JSON body to a versioned `rest/` endpoint and return the parsed
/// response (an empty body parses as `null`, which finalize returns).
fn rest_post(
    client: &reqwest::blocking::Client,
    token: &Token,
    path: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, PostError> {
    let url = format!("https://api.linkedin.com/rest/{}", path);
    let resp = client
        .post(&url)
        .bearer_auth(&token.access_token)
        .header("Content-Type", "application/json")
        .header("LinkedIn-Version", LINKEDIN_VERSION)
        .header("X-Restli-Protocol-Version", "2.0.0")
        .json(body)
        .send()
        .map_err(|e| PostError::Failed(format!("POST {}: {}", url, e)))?;

    let status = resp.status();
    let text = resp.text().unwrap_or_default();

    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(PostError::Unauthorized(status.to_string()));
    }
    if !status.is_success() {
        return Err(PostError::Failed(format!("{} → {}: {}", path, status, text)));
    }
    if text.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(&text)
        .map_err(|e| PostError::Failed(format!("parse {} response: {} ({})", path, e, text)))
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(target: &str) -> String {
        format!("GET {} HTTP/1.1\r\nHost: localhost:8092\r\n\r\n", target)
    }

    #[test]
    fn post_record_round_trips_and_tolerates_sparse_lines() {
        // A minimal line (only the required fields) must keep parsing —
        // the log is append-only and read back on every startup.
        let sparse = r#"{"timestamp":"t","youtube_url":"https://youtu.be/x"}"#;
        let r: PostRecord = serde_json::from_str(sparse).expect("sparse line parses");
        assert!(r.title.is_empty() && r.post_url.is_empty());

        let rec = PostRecord {
            timestamp: "2026-07-20 10:00:00".into(),
            youtube_url: "https://www.youtube.com/watch?v=abc".into(),
            title: "Titel".into(),
            post_url: "https://www.linkedin.com/feed/update/urn:li:share:1".into(),
        };
        let line = serde_json::to_string(&rec).unwrap();
        let back: PostRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back.youtube_url, rec.youtube_url);
        assert_eq!(back.post_url, rec.post_url);
    }

    #[test]
    fn parse_query_param_reads_code_and_decodes_it() {
        let r = req("/callback?code=AQTb%2Fxyz%3D%3D&state=1");
        assert_eq!(parse_query_param(&r, "code").as_deref(), Some("AQTb/xyz=="));
    }

    #[test]
    fn parse_query_param_finds_a_later_param() {
        // LinkedIn puts `code` last when it echoes `state` back.
        let r = req("/callback?state=abc&code=xyz");
        assert_eq!(parse_query_param(&r, "code").as_deref(), Some("xyz"));
        // A prefix match must not be mistaken for the key itself.
        assert_eq!(parse_query_param(&r, "cod"), None);
    }

    #[test]
    fn parse_query_param_reports_a_refusal_instead_of_a_code() {
        let r = req("/callback?error=user_cancelled_login&error_description=The+user+cancelled");
        assert_eq!(parse_query_param(&r, "code"), None);
        assert_eq!(
            parse_query_param(&r, "error").as_deref(),
            Some("user_cancelled_login")
        );
        // Percent-decoded, but `+` is left alone — `run_auth_flow` turns those
        // into spaces for display, which must never happen to a `code`.
        assert_eq!(
            parse_query_param(&r, "error_description").as_deref(),
            Some("The+user+cancelled")
        );
    }

    #[test]
    fn parse_query_param_keeps_a_plus_in_a_code() {
        let r = req("/callback?code=ab%2Bcd");
        assert_eq!(parse_query_param(&r, "code").as_deref(), Some("ab+cd"));
    }

    #[test]
    fn parse_query_param_survives_a_junk_request() {
        assert_eq!(parse_query_param("", "code"), None);
        assert_eq!(parse_query_param(&req("/callback"), "code"), None);
        assert_eq!(parse_query_param("garbage", "code"), None);
    }

    #[test]
    fn auth_url_carries_the_registered_redirect_and_scopes() {
        let u = auth_url("my client/id");
        assert!(u.contains("client_id=my%20client%2Fid"), "{}", u);
        assert!(u.contains("response_type=code"), "{}", u);
        assert!(
            u.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8092%2Fcallback"),
            "{}",
            u
        );
        assert!(u.contains("w_member_social"), "{}", u);
    }
}
