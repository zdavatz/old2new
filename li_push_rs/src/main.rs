//! li_push — Upload videos to LinkedIn via Videos API + Posts API
//!
//! Usage:
//!   First time (get token): li_push --auth
//!   Upload video:           li_push <video_file> --title "My Title"
//!   Friends only:           li_push <video_file> --title "My Title" --visibility CONNECTIONS
//!
//! Requires: client_id + client_secret in linkedin_credentials.json
//! Token saved to linkedin_token.json

use base64::Engine;
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "li_push")]
struct Cli {
    /// Video file or YouTube URL to upload
    video_file: Option<String>,

    /// Video title/description (auto-fetched from YouTube if not provided)
    #[arg(long, short)]
    title: Option<String>,

    /// Video description for the LinkedIn post
    #[arg(long)]
    description: Option<String>,

    /// Download lower quality (max 1080p) if video exceeds 500MB
    #[arg(long)]
    low_quality: bool,

    /// Visibility: PUBLIC or CONNECTIONS
    #[arg(long, default_value = "PUBLIC")]
    visibility: String,

    /// Run OAuth2 auth flow to get token
    #[arg(long)]
    auth: bool,

    /// Credentials file
    #[arg(long, default_value = "linkedin_credentials.json")]
    credentials: String,

    /// Token file
    #[arg(long, default_value = "linkedin_token.json")]
    token: String,
}

#[derive(Serialize, Deserialize)]
struct Credentials {
    client_id: String,
    client_secret: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct Token {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    person_id: String,
    #[serde(default)]
    expires_in: u64,
}

const REDIRECT_URI: &str = "http://localhost:8092/callback";
const LINKEDIN_VERSION: &str = "202603";
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
    let data =
        fs::read_to_string(&p).map_err(|e| format!("Cannot read {}: {}", p, e))?;
    serde_json::from_str(&data).map_err(|e| format!("Invalid JSON in {}: {}", p, e))
}

fn load_token(path: &str) -> Result<Token, String> {
    let p = find_file(path);
    let data =
        fs::read_to_string(&p).map_err(|e| format!("Cannot read {}: {}", p, e))?;
    serde_json::from_str(&data).map_err(|e| format!("Invalid JSON in {}: {}", p, e))
}

fn save_token(path: &str, token: &Token) {
    let json = serde_json::to_string_pretty(token).expect("serialize token");
    fs::write(path, json).expect("write token");
}

#[tokio::main]
async fn main() {
    // Write PID file
    let pid = std::process::id();
    let pid_path = format!(
        "{}/li_push.pid",
        env::var("HOME").unwrap_or_else(|_| "/root".to_string())
    );
    fs::write(&pid_path, pid.to_string()).ok();

    let cli = Cli::parse();

    if cli.auth {
        auth_flow(&cli).await;
        return;
    }

    let video_input = match &cli.video_file {
        Some(f) => f.clone(),
        None => {
            eprintln!("ERROR: <video_file> or YouTube URL required (or use --auth for authentication)");
            std::process::exit(1);
        }
    };

    // Check if input is a YouTube URL or video ID
    let video_input = if !video_input.contains('/') && !video_input.contains('.') && video_input.len() >= 8 && video_input.len() <= 15 {
        // Looks like a YouTube video ID (e.g. "dQw4w9WgXcQ")
        format!("https://www.youtube.com/watch?v={}", video_input)
    } else {
        video_input
    };
    let is_youtube = video_input.contains("youtube.com/") || video_input.contains("youtu.be/");
    let mut yt_title = String::new();
    let mut yt_description = String::new();
    let mut temp_file: Option<PathBuf> = None;
    let mut yt_meta_ref: Option<serde_json::Value> = None;

    let video_path = if is_youtube {
        eprintln!("Downloading from YouTube: {}", video_input);

        // Fetch metadata first (title, description)
        let meta_output = std::process::Command::new("yt-dlp")
            .args(["--dump-json", "--no-download", &video_input])
            .output()
            .expect("yt-dlp not found. Install with: brew install yt-dlp");

        if meta_output.status.success() {
            if let Ok(yt_meta) = serde_json::from_slice::<serde_json::Value>(&meta_output.stdout) {
                yt_title = yt_meta["title"].as_str().unwrap_or("").to_string();
                yt_description = yt_meta["description"].as_str().unwrap_or("").to_string();
                eprintln!("YouTube title: {}", yt_title);
                yt_meta_ref = Some(yt_meta.clone());

                // Check duration (LinkedIn max 30 min)
                if let Some(duration) = yt_meta["duration"].as_f64() {
                    if duration > 1800.0 {
                        eprintln!("ERROR: Video is {:.0}s ({:.1} min) — LinkedIn max is 30 minutes",
                            duration, duration / 60.0);
                        std::process::exit(1);
                    }
                }
            }
        }

        // Check estimated filesize before downloading (LinkedIn max 500MB)
        let mut filesize_ok = true;
        if let Some(filesize) = yt_meta_ref.as_ref()
            .and_then(|m| m["filesize_approx"].as_f64().or_else(|| m["filesize"].as_f64()))
        {
            let size_mb = filesize / (1024.0 * 1024.0);
            if filesize > 500.0 * 1024.0 * 1024.0 {
                eprintln!("ERROR: Video is ~{:.0} MB — LinkedIn max is 500 MB", size_mb);
                eprintln!("Use --low-quality to download a smaller format (max 1080p)");
                filesize_ok = false;
            }
        }
        if !filesize_ok && !cli.low_quality {
            std::process::exit(1);
        }

        // Download video to temp file
        let tmp_dir = env::temp_dir();
        let tmp_path = tmp_dir.join("li_push_yt_download.mp4");
        eprintln!("Downloading best quality...");
        let format_arg = if cli.low_quality {
            "bestvideo[height<=1080][ext=mp4]+bestaudio[ext=m4a]/best[height<=1080]"
        } else {
            "bestvideo[ext=mp4]+bestaudio[ext=m4a]/best[ext=mp4]/best"
        };
        let dl_status = std::process::Command::new("yt-dlp")
            .args([
                "-f", format_arg,
                "--merge-output-format", "mp4",
                "-o", tmp_path.to_str().unwrap(),
                "--force-overwrites",
                "--no-playlist",
                &video_input,
            ])
            .status()
            .expect("yt-dlp failed");

        if !dl_status.success() {
            eprintln!("ERROR: yt-dlp download failed");
            std::process::exit(1);
        }

        // Verify file size after download
        if let Ok(fmeta) = fs::metadata(&tmp_path) {
            let size_mb = fmeta.len() as f64 / (1024.0 * 1024.0);
            if fmeta.len() > 500 * 1024 * 1024 {
                eprintln!("ERROR: Downloaded file is {:.0} MB — LinkedIn max is 500 MB", size_mb);
                eprintln!("Use --low-quality to download a smaller format");
                let _ = fs::remove_file(&tmp_path);
                std::process::exit(1);
            }
            eprintln!("Downloaded: {:.1} MB", size_mb);
        }

        temp_file = Some(tmp_path.clone());
        tmp_path
    } else {
        let p = PathBuf::from(&video_input);
        if !p.exists() {
            eprintln!("ERROR: File not found: {}", video_input);
            std::process::exit(1);
        }
        p
    };

    let creds = load_credentials(&cli.credentials).unwrap_or_else(|e| {
        eprintln!("ERROR: {}", e);
        std::process::exit(1);
    });

    let token = load_token(&cli.token).unwrap_or_else(|e| {
        eprintln!("ERROR: {}. Run 'li_push --auth' first.", e);
        std::process::exit(1);
    });

    if token.person_id.is_empty() {
        eprintln!("ERROR: No person_id in token file. Run 'li_push --auth' first.");
        std::process::exit(1);
    }

    // Refresh token if we have a refresh_token
    let token = refresh_token(&creds, &token, &cli.token).await;

    let file_size = fs::metadata(&video_path).expect("stat file").len();
    let file_size_mb = file_size as f64 / (1024.0 * 1024.0);

    // Title priority: --title flag > YouTube title > filename
    let title = cli.title.unwrap_or_else(|| {
        if !yt_title.is_empty() {
            yt_title.clone()
        } else {
            video_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        }
    });

    // Description: --description flag > YouTube description > title
    let description = cli.description.unwrap_or_else(|| {
        if !yt_description.is_empty() {
            // Truncate to first 1000 chars for LinkedIn
            yt_description.chars().take(1000).collect()
        } else {
            title.clone()
        }
    });

    let visibility = match cli.visibility.to_uppercase().as_str() {
        "CONNECTIONS" | "FRIENDS" => "CONNECTIONS",
        _ => "PUBLIC",
    };

    eprintln!("Uploading: {} ({:.1} MB)", video_path.display(), file_size_mb);
    eprintln!("Title: {}", title);
    eprintln!("Visibility: {}", visibility);

    let client = reqwest::Client::new();
    let owner = format!("urn:li:person:{}", token.person_id);

    // Step 1: Initialize Upload
    eprintln!("Initializing upload...");
    let init_body = serde_json::json!({
        "initializeUploadRequest": {
            "owner": owner,
            "fileSizeBytes": file_size,
            "uploadCaptions": false,
            "uploadThumbnail": false
        }
    });

    let init_resp = client
        .post("https://api.linkedin.com/rest/videos?action=initializeUpload")
        .header("Authorization", format!("Bearer {}", token.access_token))
        .header("Content-Type", "application/json")
        .header("LinkedIn-Version", LINKEDIN_VERSION)
        .header("X-Restli-Protocol-Version", "2.0.0")
        .json(&init_body)
        .send()
        .await
        .expect("init request failed");

    let init_status = init_resp.status();
    let init_text = init_resp.text().await.expect("read init response");

    if !init_status.is_success() {
        eprintln!(
            "ERROR: Initialize upload failed ({}): {}",
            init_status, init_text
        );
        std::process::exit(1);
    }

    let init_data: serde_json::Value = serde_json::from_str(&init_text).unwrap_or_else(|e| {
        eprintln!(
            "ERROR: Failed to parse init response: {}\n{}",
            e, init_text
        );
        std::process::exit(1);
    });

    let value = &init_data["value"];
    let video_urn = value["video"]
        .as_str()
        .expect("no video URN in response")
        .to_string();
    let upload_token = value["uploadToken"]
        .as_str()
        .unwrap_or("")
        .to_string();

    let upload_instructions = value["uploadInstructions"]
        .as_array()
        .expect("no uploadInstructions in response");

    let total_chunks = upload_instructions.len();
    eprintln!(
        "Video URN: {}\nUpload chunks: {}",
        video_urn, total_chunks
    );

    // Step 2: Upload chunks
    let mut file = fs::File::open(&video_path).expect("open file");
    let mut etags: Vec<String> = Vec::new();

    for (i, instruction) in upload_instructions.iter().enumerate() {
        let upload_url = instruction["uploadUrl"]
            .as_str()
            .expect("no uploadUrl in instruction");
        let first_byte = instruction["firstByte"].as_u64().unwrap_or(0);
        let last_byte = instruction["lastByte"].as_u64().unwrap_or(0);
        let chunk_len = last_byte - first_byte + 1;

        file.seek(SeekFrom::Start(first_byte)).expect("seek file");
        let mut buf = vec![0u8; chunk_len as usize];
        file.read_exact(&mut buf).expect("read chunk");

        let pct = (i + 1) * 100 / total_chunks;
        eprint!(
            "\rUploading: {}% (chunk {}/{}, {:.1} MB)    ",
            pct,
            i + 1,
            total_chunks,
            chunk_len as f64 / (1024.0 * 1024.0)
        );

        let resp = client
            .put(upload_url)
            .header("Content-Type", "application/octet-stream")
            .body(buf)
            .send()
            .await
            .expect("chunk upload failed");

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            eprintln!("\nERROR: Chunk {} failed ({}): {}", i, status, body);
            std::process::exit(1);
        }

        // Extract ETag from response headers
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .trim_matches('"')
            .to_string();

        if etag.is_empty() {
            eprintln!("\nWARNING: No ETag for chunk {}", i);
        }
        etags.push(etag);
    }
    eprintln!("\nAll {} chunks uploaded.", total_chunks);

    // Step 3: Finalize Upload
    eprintln!("Finalizing upload...");
    let finalize_body = serde_json::json!({
        "finalizeUploadRequest": {
            "video": video_urn,
            "uploadToken": upload_token,
            "uploadedPartIds": etags
        }
    });

    let finalize_resp = client
        .post("https://api.linkedin.com/rest/videos?action=finalizeUpload")
        .header("Authorization", format!("Bearer {}", token.access_token))
        .header("Content-Type", "application/json")
        .header("LinkedIn-Version", LINKEDIN_VERSION)
        .header("X-Restli-Protocol-Version", "2.0.0")
        .json(&finalize_body)
        .send()
        .await
        .expect("finalize request failed");

    let finalize_status = finalize_resp.status();
    let finalize_text = finalize_resp.text().await.unwrap_or_default();

    if !finalize_status.is_success() {
        eprintln!(
            "ERROR: Finalize upload failed ({}): {}",
            finalize_status, finalize_text
        );
        std::process::exit(1);
    }
    eprintln!("Upload finalized.");

    // Step 4: Create Post
    eprintln!("Creating LinkedIn post...");
    let post_body = serde_json::json!({
        "author": owner,
        "commentary": description,
        "visibility": visibility,
        "distribution": {
            "feedDistribution": "MAIN_FEED",
            "targetEntities": [],
            "thirdPartyDistributionChannels": []
        },
        "content": {
            "media": {
                "title": title,
                "id": video_urn
            }
        },
        "lifecycleState": "PUBLISHED",
        "isReshareDisabledByAuthor": false
    });

    let post_resp = client
        .post("https://api.linkedin.com/rest/posts")
        .header("Authorization", format!("Bearer {}", token.access_token))
        .header("Content-Type", "application/json")
        .header("LinkedIn-Version", LINKEDIN_VERSION)
        .header("X-Restli-Protocol-Version", "2.0.0")
        .json(&post_body)
        .send()
        .await
        .expect("create post failed");

    let post_status = post_resp.status();
    let post_headers = post_resp.headers().clone();
    let post_text = post_resp.text().await.unwrap_or_default();

    if !post_status.is_success() {
        eprintln!(
            "ERROR: Create post failed ({}): {}",
            post_status, post_text
        );
        std::process::exit(1);
    }

    // LinkedIn returns the post URN in the x-restli-id header
    let post_id = post_headers
        .get("x-restli-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("(unknown)");

    // Build post URL from URN: urn:li:share:123456 -> https://www.linkedin.com/feed/update/urn:li:share:123456/
    let post_url = format!("https://www.linkedin.com/feed/update/{}/", post_id);

    eprintln!("Video published to LinkedIn!");
    eprintln!("  Post ID: {}", post_id);
    eprintln!("  URL: {}", post_url);
    eprintln!("  Title: {}", title);
    eprintln!("  Size: {:.1} MB", file_size_mb);
    eprintln!("  Visibility: {}", visibility);

    // Clean up temp file if downloaded from YouTube
    if let Some(tmp) = temp_file {
        let _ = fs::remove_file(&tmp);
        eprintln!("Cleaned up temp file: {}", tmp.display());
    }
}

async fn auth_flow(cli: &Cli) {
    let creds = load_credentials(&cli.credentials).unwrap_or_else(|e| {
        eprintln!("ERROR: {}", e);
        eprintln!("Create {} with:", cli.credentials);
        eprintln!(r#"  {{"client_id": "...", "client_secret": "..."}}"#);
        std::process::exit(1);
    });

    // Step 1: Open browser for authorization
    let auth_url = format!(
        "https://www.linkedin.com/oauth/v2/authorization?response_type=code&client_id={}&redirect_uri={}&scope=openid%20profile%20w_member_social",
        creds.client_id,
        urlencoding::encode(REDIRECT_URI)
    );

    eprintln!("Opening browser for LinkedIn authorization...");
    eprintln!("URL: {}\n", auth_url);

    if let Err(e) = open::that(&auth_url) {
        eprintln!("WARNING: Could not open browser automatically: {}", e);
        eprintln!("Please open the URL above manually.");
    }

    // Step 2: Kill stale listeners on port 8092, then start local callback server
    if let Ok(output) = std::process::Command::new("lsof")
        .args(["-ti:8092"])
        .output()
    {
        if !output.stdout.is_empty() {
            let pids = String::from_utf8_lossy(&output.stdout);
            for pid in pids.trim().lines() {
                let _ = std::process::Command::new("kill")
                    .args(["-9", pid.trim()])
                    .output();
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    eprintln!("Waiting for callback on {}...", REDIRECT_URI);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8092")
        .await
        .expect("Cannot bind to port 8092");

    let (mut socket, _) = listener.accept().await.expect("accept connection");

    let mut buf = vec![0u8; 4096];
    let n = tokio::io::AsyncReadExt::read(&mut socket, &mut buf)
        .await
        .expect("read request");
    let request = String::from_utf8_lossy(&buf[..n]);

    // Extract code from GET /callback?code=XXX
    let query = request.split('?').nth(1).unwrap_or("");
    let query = query.split(' ').next().unwrap_or(query);
    let code: String = query
        .split('&')
        .find(|p| p.starts_with("code="))
        .and_then(|p| p.strip_prefix("code="))
        .unwrap_or("")
        .to_string();
    let code =
        urlencoding::decode(&code).unwrap_or(std::borrow::Cow::Borrowed(&code)).to_string();

    if code.is_empty() {
        eprintln!("ERROR: No authorization code received");
        eprintln!("Raw request: {}", request);
        std::process::exit(1);
    }

    // Send response to browser
    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<h1>LinkedIn authorized! You can close this window.</h1>";
    tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes())
        .await
        .ok();

    eprintln!("Authorization code received. Exchanging for token...");

    // Step 3: Exchange code for token
    let client = reqwest::Client::new();
    let token_resp = client
        .post("https://www.linkedin.com/oauth/v2/accessToken")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&client_secret={}",
            urlencoding::encode(&code),
            urlencoding::encode(REDIRECT_URI),
            urlencoding::encode(&creds.client_id),
            urlencoding::encode(&creds.client_secret)
        ))
        .send()
        .await
        .expect("token exchange request failed");

    let token_status = token_resp.status();
    let token_text = token_resp.text().await.expect("read token response");

    if !token_status.is_success() {
        eprintln!(
            "ERROR: Token exchange failed ({}): {}",
            token_status, token_text
        );
        std::process::exit(1);
    }

    let token_data: serde_json::Value =
        serde_json::from_str(&token_text).unwrap_or_else(|e| {
            eprintln!(
                "ERROR: Failed to parse token response: {}\n{}",
                e, token_text
            );
            std::process::exit(1);
        });

    if let Some(err) = token_data.get("error") {
        eprintln!("ERROR: Token exchange failed: {}", err);
        eprintln!("Description: {}", token_data.get("error_description").unwrap_or(&serde_json::Value::Null));
        std::process::exit(1);
    }

    let access_token = token_data["access_token"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let refresh_token_str = token_data["refresh_token"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let expires_in = token_data["expires_in"].as_u64().unwrap_or(0);

    if access_token.is_empty() {
        eprintln!("ERROR: No access_token in response: {}", token_text);
        std::process::exit(1);
    }

    // Extract person_id from id_token JWT (returned with openid scope)
    let mut jwt_person_id = String::new();
    if let Some(id_token) = token_data["id_token"].as_str() {
        // JWT format: header.payload.signature — decode payload
        let parts: Vec<&str> = id_token.split('.').collect();
        if parts.len() >= 2 {
            let payload = parts[1];
            // Add padding
            let padded = match payload.len() % 4 {
                2 => format!("{}==", payload),
                3 => format!("{}=", payload),
                _ => payload.to_string(),
            };
            if let Ok(decoded) = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(padded.trim_end_matches('='))
            {
                if let Ok(claims) = serde_json::from_slice::<serde_json::Value>(&decoded) {
                    if let Some(sub) = claims["sub"].as_str() {
                        jwt_person_id = sub.to_string();
                        eprintln!("Person ID from id_token: {}", jwt_person_id);
                    }
                }
            }
        }
    }
    eprintln!("DEBUG: token response keys: {:?}", token_data.as_object().map(|o| o.keys().collect::<Vec<_>>()));

    // Step 4: Get person ID — try multiple endpoints
    eprintln!("Fetching person ID...");

    // Use JWT person_id if available, otherwise try API endpoints
    let mut person_id = jwt_person_id;

    // Try /v2/userinfo
    if let Ok(resp) = client
        .get("https://api.linkedin.com/v2/userinfo")
        .header("Authorization", format!("Bearer {}", access_token))
        .send()
        .await
    {
        if resp.status().is_success() {
            if let Ok(text) = resp.text().await {
                if let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(sub) = data["sub"].as_str() {
                        person_id = sub.to_string();
                    }
                }
            }
        }
    }

    // Try /v2/me if userinfo failed
    if person_id.is_empty() {
        if let Ok(resp) = client
            .get("https://api.linkedin.com/v2/me")
            .header("Authorization", format!("Bearer {}", access_token))
            .send()
            .await
        {
            if resp.status().is_success() {
                if let Ok(text) = resp.text().await {
                    if let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(id) = data["id"].as_str() {
                            person_id = id.to_string();
                        }
                    }
                }
            }
        }
    }

    // If still empty, ask user to provide it
    if person_id.is_empty() {
        eprintln!("WARNING: Could not fetch person ID automatically.");
        eprintln!("Please find your LinkedIn person ID and enter it.");
        eprintln!("You can find it at: https://www.linkedin.com/me/ (the URL will show your ID)");
        eprintln!("Or try: curl -H 'Authorization: Bearer {}' https://api.linkedin.com/v2/me", access_token);
        // Use a placeholder — user can update linkedin_token.json manually
        person_id = "UNKNOWN".to_string();
    }


    let token = Token {
        access_token,
        refresh_token: refresh_token_str,
        person_id: person_id.clone(),
        expires_in,
    };

    save_token(&cli.token, &token);
    eprintln!("Token saved to {}", cli.token);
    eprintln!("Person ID: {}", person_id);
    eprintln!("Expires in: {} seconds", expires_in);
    eprintln!("Ready to upload videos!");
}

async fn refresh_token(creds: &Credentials, token: &Token, token_path: &str) -> Token {
    if token.refresh_token.is_empty() {
        eprintln!("No refresh token available, using existing access token");
        return token.clone();
    }

    eprintln!("Refreshing access token...");
    let client = reqwest::Client::new();
    let resp = client
        .post("https://www.linkedin.com/oauth/v2/accessToken")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}&client_secret={}",
            token.refresh_token, creds.client_id, creds.client_secret
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
                            refresh_token: data["refresh_token"]
                                .as_str()
                                .unwrap_or(&token.refresh_token)
                                .to_string(),
                            person_id: token.person_id.clone(),
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
