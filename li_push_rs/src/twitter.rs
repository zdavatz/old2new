//! X / Twitter video posting for li_push.
//!
//! Uploads a video via the chunked v2 `/2/media/upload` flow
//! (INIT → APPEND → FINALIZE → poll STATUS) and creates a tweet with it via
//! `/2/tweets`. Both endpoints use an OAuth 2.0 user-context Bearer token
//! (Authorization Code + PKCE), so this reuses the same browser-callback UX as
//! the LinkedIn flow (callback on port 8092) instead of OAuth 1.0a signing.
//!
//! Credentials: `twitter_credentials.json` (cwd, then $HOME):
//!   {"client_id":"...","client_secret":"..."}
//! Token (written by `--auth-twitter`): `twitter_token.json`.
//!
//! Scopes requested: tweet.read tweet.write users.read media.write
//! offline.access (offline.access yields a refresh token).

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const REDIRECT_URI: &str = "http://localhost:8092/callback";
const SCOPES: &str = "tweet.read tweet.write users.read media.write offline.access";
const AUTHORIZE_URL: &str = "https://x.com/i/oauth2/authorize";
const TOKEN_URL: &str = "https://api.twitter.com/2/oauth2/token";
// v2 chunked media upload lives on api.x.com under dedicated sub-paths
// (/initialize, /{id}/append, /{id}/finalize). The bare /2/media/upload only
// accepts the single-shot image schema.
const MEDIA_BASE: &str = "https://api.x.com/2/media/upload";
const TWEETS_URL: &str = "https://api.twitter.com/2/tweets";
const CHUNK_SIZE: usize = 4 * 1024 * 1024; // 4 MiB per APPEND segment

#[derive(Serialize, Deserialize, Clone)]
pub struct Credentials {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Token {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_in: u64,
}

fn find_file(name: &str) -> PathBuf {
    let cwd = PathBuf::from(name);
    if cwd.exists() {
        return cwd;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join(name);
        if p.exists() {
            return p;
        }
    }
    cwd
}

pub fn load_credentials(path: &str) -> Result<Credentials, Box<dyn Error>> {
    let p = find_file(path);
    let data = fs::read_to_string(&p)
        .map_err(|e| format!("Cannot read {}: {}", p.display(), e))?;
    Ok(serde_json::from_str(&data)?)
}

pub fn load_token(path: &str) -> Result<Token, Box<dyn Error>> {
    let p = find_file(path);
    let data = fs::read_to_string(&p)
        .map_err(|e| format!("Cannot read {}: {}", p.display(), e))?;
    Ok(serde_json::from_str(&data)?)
}

pub fn save_token(path: &str, token: &Token) {
    let json = serde_json::to_string_pretty(token).expect("serialize token");
    fs::write(path, json).expect("write token");
}

/// 32 bytes of OS randomness, base64url (no pad) — used for the PKCE verifier
/// and the OAuth `state`.
fn random_b64url() -> String {
    let mut buf = [0u8; 32];
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Browser-based OAuth 2.0 Authorization Code + PKCE flow. Writes the resulting
/// access + refresh token to `token_path`.
pub async fn auth_flow(creds: &Credentials, token_path: &str) -> Result<(), Box<dyn Error>> {
    let verifier = random_b64url();
    let challenge = pkce_challenge(&verifier);
    let state = random_b64url();

    let auth_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        AUTHORIZE_URL,
        urlencoding::encode(&creds.client_id),
        urlencoding::encode(REDIRECT_URI),
        urlencoding::encode(SCOPES),
        urlencoding::encode(&state),
        urlencoding::encode(&challenge),
    );

    eprintln!("Opening browser for X / Twitter authorization...");
    eprintln!("URL: {}\n", auth_url);
    if let Err(e) = open::that(&auth_url) {
        eprintln!("WARNING: Could not open browser automatically: {}", e);
        eprintln!("Please open the URL above manually.");
    }

    // Free port 8092 of any stale listener, then accept one callback.
    if let Ok(output) = std::process::Command::new("lsof").args(["-ti:8092"]).output() {
        if !output.stdout.is_empty() {
            for pid in String::from_utf8_lossy(&output.stdout).trim().lines() {
                let _ = std::process::Command::new("kill").args(["-9", pid.trim()]).output();
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    eprintln!("Waiting for callback on {}...", REDIRECT_URI);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8092")
        .await
        .map_err(|e| format!("Cannot bind to port 8092: {}", e))?;
    let (mut socket, _) = listener.accept().await?;

    let mut buf = vec![0u8; 8192];
    let n = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);

    let query = request.split('?').nth(1).unwrap_or("");
    let query = query.split(' ').next().unwrap_or(query);
    let mut code = String::new();
    let mut got_state = String::new();
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("code=") {
            code = urlencoding::decode(v).map(|c| c.into_owned()).unwrap_or_default();
        } else if let Some(v) = pair.strip_prefix("state=") {
            got_state = urlencoding::decode(v).map(|c| c.into_owned()).unwrap_or_default();
        }
    }

    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<h1>X / Twitter authorized! You can close this window.</h1>";
    tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await.ok();

    if code.is_empty() {
        return Err(format!("No authorization code received. Raw request: {}", request).into());
    }
    if got_state != state {
        return Err("OAuth state mismatch (possible CSRF) — aborting".into());
    }

    eprintln!("Authorization code received. Exchanging for token...");
    let client = reqwest::Client::new();
    let mut req = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded");
    // Confidential clients authenticate the token request with HTTP Basic.
    if !creds.client_secret.is_empty() {
        let basic = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", creds.client_id, creds.client_secret));
        req = req.header("Authorization", format!("Basic {}", basic));
    }
    let body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&code_verifier={}&client_id={}",
        urlencoding::encode(&code),
        urlencoding::encode(REDIRECT_URI),
        urlencoding::encode(&verifier),
        urlencoding::encode(&creds.client_id),
    );
    let resp = req.body(body).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(format!("Token exchange failed ({}): {}", status, text).into());
    }
    let data: serde_json::Value = serde_json::from_str(&text)?;
    let token = Token {
        access_token: data["access_token"].as_str().unwrap_or("").to_string(),
        refresh_token: data["refresh_token"].as_str().unwrap_or("").to_string(),
        expires_in: data["expires_in"].as_u64().unwrap_or(0),
    };
    if token.access_token.is_empty() {
        return Err(format!("No access_token in response: {}", text).into());
    }
    save_token(token_path, &token);
    eprintln!("Token saved to {}", token_path);
    eprintln!("Expires in: {} seconds", token.expires_in);
    eprintln!(
        "Refresh token: {}",
        if token.refresh_token.is_empty() { "none" } else { "yes" }
    );
    eprintln!("Ready to post videos to X!");
    Ok(())
}

/// Refresh the access token if a refresh token is present. Returns the (possibly
/// unchanged) token. A failed refresh logs a warning and returns the original.
pub async fn refresh_token(creds: &Credentials, token: &Token, token_path: &str) -> Token {
    if token.refresh_token.is_empty() {
        return token.clone();
    }
    eprintln!("Refreshing X access token...");
    let client = reqwest::Client::new();
    let mut req = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded");
    if !creds.client_secret.is_empty() {
        let basic = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", creds.client_id, creds.client_secret));
        req = req.header("Authorization", format!("Basic {}", basic));
    }
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        urlencoding::encode(&token.refresh_token),
        urlencoding::encode(&creds.client_id),
    );
    if let Ok(resp) = req.body(body).send().await {
        if let Ok(text) = resp.text().await {
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(at) = data["access_token"].as_str() {
                    if !at.is_empty() {
                        let new = Token {
                            access_token: at.to_string(),
                            refresh_token: data["refresh_token"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .unwrap_or(&token.refresh_token)
                                .to_string(),
                            expires_in: data["expires_in"].as_u64().unwrap_or(0),
                        };
                        save_token(token_path, &new);
                        eprintln!("X token refreshed.");
                        return new;
                    }
                }
            }
        }
    }
    eprintln!("WARNING: X token refresh failed, using existing token.");
    token.clone()
}

fn read_chunk(file: &mut fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = file.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// Upload `video_path` via the v2 chunked endpoints (initialize → append →
/// finalize → poll status) and return the media id.
async fn upload_video_media(
    client: &reqwest::Client,
    access_token: &str,
    video_path: &Path,
) -> Result<String, Box<dyn Error>> {
    let total = fs::metadata(video_path)?.len();
    let bearer = format!("Bearer {}", access_token);

    // INITIALIZE (JSON body, no `command` field — that's the old v1.1 protocol)
    let init_body = serde_json::json!({
        "media_category": "tweet_video",
        "media_type": "video/mp4",
        "total_bytes": total,
    });
    let resp = client
        .post(format!("{}/initialize", MEDIA_BASE))
        .header("Authorization", &bearer)
        .header("Content-Type", "application/json")
        .json(&init_body)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(format!("media initialize failed ({}): {}", status, text).into());
    }
    let v: serde_json::Value = serde_json::from_str(&text)?;
    let media_id = v["data"]["id"]
        .as_str()
        .or_else(|| v["data"]["media_id_string"].as_str())
        .or_else(|| v["media_id_string"].as_str())
        .or_else(|| v["id"].as_str())
        .ok_or_else(|| format!("no media id in initialize response: {}", text))?
        .to_string();
    eprintln!("X media_id: {}", media_id);

    // APPEND — media id in the path; body carries segment_index + the bytes.
    let append_url = format!("{}/{}/append", MEDIA_BASE, media_id);
    let mut file = fs::File::open(video_path)?;
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut seg = 0usize;
    let total_segments = (total as usize).div_ceil(CHUNK_SIZE);
    loop {
        let n = read_chunk(&mut file, &mut buf)?;
        if n == 0 {
            break;
        }
        let part = reqwest::multipart::Part::bytes(buf[..n].to_vec())
            .file_name("blob")
            .mime_str("application/octet-stream")?;
        let form = reqwest::multipart::Form::new()
            .text("segment_index", seg.to_string())
            .part("media", part);
        let resp = client
            .post(&append_url)
            .header("Authorization", &bearer)
            .multipart(form)
            .send()
            .await?;
        if !resp.status().is_success() {
            let st = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("media append segment {} failed ({}): {}", seg, st, body).into());
        }
        eprint!(
            "\rUploading video to X: segment {}/{} ({:.1} MB)    ",
            seg + 1,
            total_segments.max(seg + 1),
            n as f64 / (1024.0 * 1024.0)
        );
        seg += 1;
    }
    eprintln!("\nAll {} segment(s) uploaded to X.", seg);

    // FINALIZE
    let resp = client
        .post(format!("{}/{}/finalize", MEDIA_BASE, media_id))
        .header("Authorization", &bearer)
        .header("Content-Length", "0")
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(format!("media finalize failed ({}): {}", status, text).into());
    }
    let v: serde_json::Value = serde_json::from_str(&text)?;
    let mut info = v["data"]["processing_info"].clone();
    if info.is_null() {
        info = v["processing_info"].clone();
    }

    // Poll STATUS until the video finishes transcoding.
    let mut tries = 0;
    while !info.is_null() {
        let state = info["state"].as_str().unwrap_or("");
        if state == "succeeded" {
            break;
        }
        if state == "failed" {
            return Err(format!("X video processing failed: {}", info).into());
        }
        let wait = info["check_after_secs"].as_u64().unwrap_or(5).clamp(1, 30);
        eprintln!("X video processing ({}), waiting {}s...", state, wait);
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        tries += 1;
        if tries > 60 {
            return Err("X video processing timed out".into());
        }
        let status_url = format!(
            "{}?command=STATUS&media_id={}",
            MEDIA_BASE,
            urlencoding::encode(&media_id)
        );
        let resp = client
            .get(&status_url)
            .header("Authorization", &bearer)
            .send()
            .await?;
        let st = resp.status();
        let text = resp.text().await?;
        if !st.is_success() {
            return Err(format!("media status failed ({}): {}", st, text).into());
        }
        let v: serde_json::Value = serde_json::from_str(&text)?;
        info = v["data"]["processing_info"].clone();
        if info.is_null() {
            info = v["processing_info"].clone();
        }
    }

    Ok(media_id)
}

/// Upload a single image via the single-shot v2 endpoint (`tweet_image`) and
/// return the media id. Native video upload via the chunked endpoints is
/// rejected for this app tier, so we post a representative frame + a link to the
/// video instead.
async fn upload_image_media(
    client: &reqwest::Client,
    access_token: &str,
    image_path: &Path,
) -> Result<String, Box<dyn Error>> {
    let bytes = fs::read(image_path)?;
    eprintln!(
        "Uploading image to X: {} ({:.1} KB)",
        image_path.display(),
        bytes.len() as f64 / 1024.0
    );
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name("frame.jpg")
        .mime_str("image/jpeg")?;
    let form = reqwest::multipart::Form::new()
        .text("media_category", "tweet_image")
        .text("media_type", "image/jpeg")
        .part("media", part);
    let resp = client
        .post(MEDIA_BASE)
        .header("Authorization", format!("Bearer {}", access_token))
        .multipart(form)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(format!("image upload failed ({}): {}", status, text).into());
    }
    let v: serde_json::Value = serde_json::from_str(&text)?;
    let media_id = v["data"]["id"]
        .as_str()
        .or_else(|| v["data"]["media_id_string"].as_str())
        .or_else(|| v["media_id_string"].as_str())
        .or_else(|| v["id"].as_str())
        .ok_or_else(|| format!("no media id in upload response: {}", text))?
        .to_string();
    eprintln!("X media_id: {}", media_id);
    Ok(media_id)
}

/// Create a tweet with `caption` and the given media id. Returns the tweet id.
async fn create_tweet(
    client: &reqwest::Client,
    access_token: &str,
    caption: &str,
    media_id: &str,
) -> Result<String, Box<dyn Error>> {
    let body = serde_json::json!({
        "text": caption,
        "media": { "media_ids": [media_id] }
    });
    let resp = client
        .post(TWEETS_URL)
        .header("Authorization", format!("Bearer {}", access_token))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(format!("create tweet failed ({}): {}", status, text).into());
    }
    let v: serde_json::Value = serde_json::from_str(&text)?;
    let id = v["data"]["id"]
        .as_str()
        .ok_or_else(|| format!("no tweet id in response: {}", text))?
        .to_string();
    Ok(id)
}

/// Upload `video_path` natively and post it as a tweet with `caption`.
/// Returns the tweet URL (https://x.com/i/web/status/<id>).
pub async fn publish_video(
    access_token: &str,
    video_path: &Path,
    caption: &str,
) -> Result<String, Box<dyn Error>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;
    let media_id = upload_video_media(&client, access_token, video_path).await?;
    let id = create_tweet(&client, access_token, caption, &media_id).await?;
    Ok(format!("https://x.com/i/web/status/{}", id))
}

/// Upload `image_path` and post it as a tweet with `caption` (which should carry
/// the link to the video). Returns the tweet URL (https://x.com/i/web/status/<id>).
pub async fn publish_image(
    access_token: &str,
    image_path: &Path,
    caption: &str,
) -> Result<String, Box<dyn Error>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .build()?;
    let media_id = upload_image_media(&client, access_token, image_path).await?;
    let id = create_tweet(&client, access_token, caption, &media_id).await?;
    Ok(format!("https://x.com/i/web/status/{}", id))
}
