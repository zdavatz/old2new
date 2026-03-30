//! tk_push — Upload videos to TikTok via Content Posting API
//!
//! Usage:
//!   First time (get token): tk_push --auth
//!   Upload video:           tk_push <video_file> --title "My Title"
//!
//! Requires: client_key + client_secret in tiktok_credentials.json
//! Token saved to tiktok_token.json

use clap::Parser;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "tk_push")]
struct Cli {
    /// Video file to upload
    video_file: Option<String>,

    /// Video title/caption
    #[arg(long, short)]
    title: Option<String>,

    /// Privacy level: public, friends, self
    #[arg(long, default_value = "public")]
    privacy: String,

    /// Run OAuth2 auth flow to get token
    #[arg(long)]
    auth: bool,

    /// Credentials file
    #[arg(long, default_value = "tiktok_credentials.json")]
    credentials: String,

    /// Token file
    #[arg(long, default_value = "tiktok_token.json")]
    token: String,
}

#[derive(Serialize, Deserialize)]
struct Credentials {
    client_key: String,
    client_secret: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct Token {
    access_token: String,
    refresh_token: String,
    open_id: String,
    #[serde(default)]
    expires_in: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    open_id: String,
    expires_in: u64,
}

#[derive(Deserialize)]
struct InitResponse {
    data: Option<InitData>,
    error: Option<ErrorInfo>,
}

#[derive(Deserialize)]
struct InitData {
    publish_id: String,
    upload_url: String,
}

#[derive(Deserialize)]
struct StatusResponse {
    data: Option<StatusData>,
    error: Option<ErrorInfo>,
}

#[derive(Deserialize)]
struct StatusData {
    status: String,
}

#[derive(Deserialize)]
struct ErrorInfo {
    code: String,
    message: String,
}

fn find_file(name: &str) -> String {
    if Path::new(name).exists() {
        return name.to_string();
    }
    let home = env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let home_path = format!("{}/{}", home, name);
    if Path::new(&home_path).exists() {
        return home_path;
    }
    name.to_string()
}

fn load_credentials(path: &str) -> Result<Credentials, String> {
    let p = find_file(path);
    let data = fs::read_to_string(&p)
        .map_err(|e| format!("Cannot read {}: {}", p, e))?;
    serde_json::from_str(&data)
        .map_err(|e| format!("Invalid JSON in {}: {}", p, e))
}

fn load_token(path: &str) -> Result<Token, String> {
    let p = find_file(path);
    let data = fs::read_to_string(&p)
        .map_err(|e| format!("Cannot read {}: {}", p, e))?;
    serde_json::from_str(&data)
        .map_err(|e| format!("Invalid JSON in {}: {}", p, e))
}

fn save_token(path: &str, token: &Token) {
    let json = serde_json::to_string_pretty(token).expect("serialize token");
    fs::write(path, json).expect("write token");
}

const REDIRECT_URI: &str = "http://localhost:8091/callback";

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if cli.auth {
        auth_flow(&cli).await;
        return;
    }

    let video_file = match &cli.video_file {
        Some(f) => f.clone(),
        None => {
            eprintln!("ERROR: <video_file> required (or use --auth for authentication)");
            std::process::exit(1);
        }
    };

    let video_path = PathBuf::from(&video_file);
    if !video_path.exists() {
        eprintln!("ERROR: File not found: {}", video_file);
        std::process::exit(1);
    }

    let creds = load_credentials(&cli.credentials).unwrap_or_else(|e| {
        eprintln!("ERROR: {}", e);
        std::process::exit(1);
    });

    let token = load_token(&cli.token).unwrap_or_else(|e| {
        eprintln!("ERROR: {}. Run 'tk_push --auth' first.", e);
        std::process::exit(1);
    });

    // Refresh token
    let token = refresh_token(&creds, &token, &cli.token).await;

    let file_size = fs::metadata(&video_path).expect("stat file").len();
    let file_size_mb = file_size / (1024 * 1024);
    let title = cli.title.unwrap_or_else(|| {
        video_path.file_stem().unwrap_or_default()
            .to_string_lossy().to_string()
    });

    let privacy = match cli.privacy.as_str() {
        "public" | "PUBLIC_TO_EVERYONE" => "PUBLIC_TO_EVERYONE",
        "friends" | "MUTUAL_FOLLOW_FRIENDS" => "MUTUAL_FOLLOW_FRIENDS",
        "self" | "SELF_ONLY" => "SELF_ONLY",
        _ => "PUBLIC_TO_EVERYONE",
    };

    eprintln!("Uploading: {} ({} MB)", video_file, file_size_mb);
    eprintln!("Title: {}", title);
    eprintln!("Privacy: {}", privacy);

    // Step 1: Initialize upload
    let chunk_size: u64 = 10 * 1024 * 1024; // 10 MB chunks
    let total_chunks = if file_size <= chunk_size { 1 } else { file_size / chunk_size + if file_size % chunk_size > 0 { 1 } else { 0 } };

    let client = reqwest::Client::new();

    let init_body = serde_json::json!({
        "post_info": {
            "title": title,
            "privacy_level": privacy,
        },
        "source_info": {
            "source": "FILE_UPLOAD",
            "video_size": file_size,
            "chunk_size": chunk_size,
            "total_chunk_count": total_chunks,
        }
    });

    eprintln!("Initializing upload ({} chunks)...", total_chunks);
    let init_resp = client
        .post("https://open.tiktokapis.com/v2/post/publish/video/init/")
        .header("Authorization", format!("Bearer {}", token.access_token))
        .header("Content-Type", "application/json; charset=UTF-8")
        .json(&init_body)
        .send()
        .await
        .expect("init request failed");

    let init_text = init_resp.text().await.expect("read init response");
    let init: InitResponse = serde_json::from_str(&init_text).unwrap_or_else(|e| {
        eprintln!("ERROR: Failed to parse init response: {}\n{}", e, init_text);
        std::process::exit(1);
    });

    if let Some(err) = &init.error {
        if err.code != "ok" {
            eprintln!("ERROR: Init failed: {} — {}", err.code, err.message);
            std::process::exit(1);
        }
    }

    let init_data = init.data.unwrap_or_else(|| {
        eprintln!("ERROR: No data in init response: {}", init_text);
        std::process::exit(1);
    });

    let publish_id = init_data.publish_id;
    let upload_url = init_data.upload_url;
    eprintln!("Upload URL received. publish_id: {}", publish_id);

    // Step 2: Upload chunks
    let mut file = fs::File::open(&video_path).expect("open file");
    let mut offset: u64 = 0;
    let mut chunk_num: u64 = 0;

    while offset < file_size {
        let this_chunk = std::cmp::min(chunk_size, file_size - offset);
        let mut buf = vec![0u8; this_chunk as usize];
        file.read_exact(&mut buf).expect("read chunk");

        let content_range = format!("bytes {}-{}/{}", offset, offset + this_chunk - 1, file_size);
        let pct = (offset + this_chunk) * 100 / file_size;
        eprint!("\rUploading: {}% ({}/{}MB)    ", pct, (offset + this_chunk) / (1024 * 1024), file_size_mb);

        let resp = client
            .put(&upload_url)
            .header("Content-Range", &content_range)
            .header("Content-Length", this_chunk.to_string())
            .header("Content-Type", "video/mp4")
            .body(buf)
            .send()
            .await
            .expect("chunk upload failed");

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            eprintln!("\nERROR: Chunk {} failed ({}): {}", chunk_num, status, body);
            std::process::exit(1);
        }

        offset += this_chunk;
        chunk_num += 1;
    }
    eprintln!("\nAll {} chunks uploaded.", chunk_num);

    // Step 3: Poll status
    eprintln!("Waiting for TikTok to process...");
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let status_resp = client
            .post("https://open.tiktokapis.com/v2/post/publish/status/fetch/")
            .header("Authorization", format!("Bearer {}", token.access_token))
            .header("Content-Type", "application/json; charset=UTF-8")
            .json(&serde_json::json!({ "publish_id": publish_id }))
            .send()
            .await;

        if let Ok(resp) = status_resp {
            if let Ok(text) = resp.text().await {
                if let Ok(status) = serde_json::from_str::<StatusResponse>(&text) {
                    if let Some(data) = &status.data {
                        eprint!("\r  Status: {}    ", data.status);
                        if data.status == "PUBLISH_COMPLETE" {
                            eprintln!("\n\nUpload complete!");
                            break;
                        } else if data.status.contains("FAILED") || data.status.contains("ERROR") {
                            eprintln!("\n\nUpload FAILED: {}", data.status);
                            std::process::exit(1);
                        }
                    }
                    if let Some(err) = &status.error {
                        if err.code != "ok" {
                            eprintln!("\nStatus error: {} — {}", err.code, err.message);
                        }
                    }
                }
            }
        }
    }

    eprintln!("Video published to TikTok!");
    eprintln!("  Title: {}", title);
    eprintln!("  Size: {} MB", file_size_mb);
}

async fn auth_flow(cli: &Cli) {
    let creds = load_credentials(&cli.credentials).unwrap_or_else(|e| {
        eprintln!("ERROR: {}", e);
        eprintln!("Create {} with:", cli.credentials);
        eprintln!(r#"  {{"client_key": "...", "client_secret": "..."}}"#);
        std::process::exit(1);
    });

    // Step 1: Open browser for authorization
    let auth_url = format!(
        "https://www.tiktok.com/v2/auth/authorize/?client_key={}&scope=video.publish,video.upload&response_type=code&redirect_uri={}",
        creds.client_key, REDIRECT_URI
    );

    eprintln!("Opening browser for TikTok authorization...");
    eprintln!("If browser doesn't open, visit:\n{}\n", auth_url);
    let _ = open::that(&auth_url);

    // Step 2: Start local server to receive callback
    eprintln!("Waiting for callback on {}...", REDIRECT_URI);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8091")
        .await
        .expect("Cannot bind to port 8091");

    let (mut socket, _) = listener.accept().await.expect("accept connection");

    let mut buf = vec![0u8; 4096];
    let n = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await.expect("read request");
    let request = String::from_utf8_lossy(&buf[..n]);

    // Extract code from GET /callback?code=XXX
    let code = request
        .split("code=").nth(1)
        .and_then(|s| s.split(|c: char| c == '&' || c == ' ' || c == '\r' || c == '\n').next())
        .unwrap_or("");

    if code.is_empty() {
        eprintln!("ERROR: No authorization code received");
        std::process::exit(1);
    }

    // Send response to browser
    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<h1>TikTok authorized! You can close this window.</h1>";
    tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await.ok();

    eprintln!("Authorization code received. Exchanging for token...");

    // Step 3: Exchange code for token
    let client = reqwest::Client::new();
    let token_resp = client
        .post("https://open.tiktokapis.com/v2/oauth/token/")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "client_key={}&client_secret={}&code={}&grant_type=authorization_code&redirect_uri={}",
            creds.client_key, creds.client_secret, code, REDIRECT_URI
        ))
        .send()
        .await
        .expect("token request failed");

    let token_text = token_resp.text().await.expect("read token response");
    let token_data: serde_json::Value = serde_json::from_str(&token_text).unwrap_or_else(|e| {
        eprintln!("ERROR: Failed to parse token response: {}\n{}", e, token_text);
        std::process::exit(1);
    });

    if let Some(err) = token_data.get("error").and_then(|e| e.as_str()) {
        if err != "ok" {
            eprintln!("ERROR: Token exchange failed: {}", token_text);
            std::process::exit(1);
        }
    }

    let token = Token {
        access_token: token_data["access_token"].as_str().unwrap_or("").to_string(),
        refresh_token: token_data["refresh_token"].as_str().unwrap_or("").to_string(),
        open_id: token_data["open_id"].as_str().unwrap_or("").to_string(),
        expires_in: token_data["expires_in"].as_u64().unwrap_or(0),
    };

    save_token(&cli.token, &token);
    eprintln!("Token saved to {}", cli.token);
    eprintln!("open_id: {}", token.open_id);
    eprintln!("Ready to upload videos!");
}

async fn refresh_token(creds: &Credentials, token: &Token, token_path: &str) -> Token {
    eprintln!("Refreshing access token...");
    let client = reqwest::Client::new();
    let resp = client
        .post("https://open.tiktokapis.com/v2/oauth/token/")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "client_key={}&client_secret={}&grant_type=refresh_token&refresh_token={}",
            creds.client_key, creds.client_secret, token.refresh_token
        ))
        .send()
        .await;

    if let Ok(resp) = resp {
        if let Ok(text) = resp.text().await {
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(at) = data["access_token"].as_str() {
                    if !at.is_empty() {
                        let new_token = Token {
                            access_token: at.to_string(),
                            refresh_token: data["refresh_token"].as_str()
                                .unwrap_or(&token.refresh_token).to_string(),
                            open_id: data["open_id"].as_str()
                                .unwrap_or(&token.open_id).to_string(),
                            expires_in: data["expires_in"].as_u64().unwrap_or(0),
                        };
                        save_token(token_path, &new_token);
                        eprintln!("Token refreshed successfully");
                        return new_token;
                    }
                }
            }
        }
    }

    eprintln!("WARNING: Token refresh failed, using existing token");
    token.clone()
}
