use chrono::Utc;
use clap::Parser;
use google_gmail1::api::Message;
use google_gmail1::Gmail;
use google_youtube3::api::Video;
use google_youtube3::api::VideoSnippet;
use google_youtube3::api::VideoStatus;
use google_youtube3::YouTube;
use google_youtube3::oauth2 as yup_oauth2;
use hyper::client::HttpConnector;
use hyper::Client;
use hyper_rustls::HttpsConnector;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use yup_oauth2::authenticator::Authenticator;
use yup_oauth2::authenticator_delegate::InstalledFlowDelegate;

/// Upload enhanced video to YouTube, copying title/description from original.
#[derive(Parser)]
#[command(name = "youtube_upload")]
struct Cli {
    /// Path to the enhanced video file
    enhanced_file: String,

    /// Original YouTube video ID (use --video-id for IDs starting with dash)
    #[arg(long = "video-id")]
    video_id: String,

    /// OAuth client secret file
    #[arg(long = "client-secret", default_value = "client_secret.json")]
    client_secret: String,

    /// OAuth token file
    #[arg(long = "token", default_value = "youtube_token.json")]
    token: String,

    /// Email to notify after upload (empty string to skip)
    #[arg(long = "notify", default_value = "juerg@davaz.com")]
    notify: Option<String>,
}

/// Token format compatible with Python google-auth
#[derive(Deserialize, Serialize, Clone)]
struct PythonToken {
    token: String,
    refresh_token: String,
    #[serde(default)]
    token_uri: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    client_secret: String,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expiry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    universe_domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
}

/// Standard Google OAuth installed app format
#[derive(Deserialize)]
struct ClientSecretFile {
    installed: InstalledApp,
}

#[derive(Deserialize)]
struct InstalledApp {
    client_id: String,
    client_secret: String,
    #[serde(default = "default_token_uri")]
    token_uri: String,
    #[serde(default = "default_auth_uri")]
    auth_uri: String,
}

fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".to_string()
}

fn default_auth_uri() -> String {
    "https://accounts.google.com/o/oauth2/auth".to_string()
}

/// Upload log entry appended to ~/upload_log.jsonl
#[derive(Serialize)]
struct UploadLogEntry {
    video_id: String,
    new_video_id: String,
    title: String,
    uploaded_at: String,
    upload_time_s: u64,
    upload_speed_mbps: u64,
    file_size_mb: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    gpu_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extract_time: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upscale_time: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reassemble_time: Option<u64>,
}

/// Headless flow delegate -- fails with a clear error if browser auth is needed.
struct HeadlessFlowDelegate;

impl InstalledFlowDelegate for HeadlessFlowDelegate {
    fn present_user_url<'a>(
        &'a self,
        url: &'a str,
        _need_code: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
    {
        Box::pin(async move {
            Err(format!(
                "Browser auth required but running headless. Visit: {}\n\
                 Run on a machine with a browser first to generate the token file.",
                url
            ))
        })
    }
}

const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/youtube.upload",
    "https://www.googleapis.com/auth/youtube.readonly",
    "https://www.googleapis.com/auth/youtube",
    "https://www.googleapis.com/auth/gmail.send",
];

fn build_https_client() -> Client<HttpsConnector<HttpConnector>> {
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .expect("native TLS roots")
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();
    Client::builder().build(connector)
}

/// Refresh the access token directly via Google's token endpoint.
/// This bypasses yup-oauth2's cache format issues entirely.
async fn refresh_access_token(py_token: &PythonToken) -> Result<String, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let resp = client.post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", py_token.client_id.as_str()),
            ("client_secret", py_token.client_secret.as_str()),
            ("refresh_token", py_token.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await?;

    let body: serde_json::Value = resp.json().await?;
    if let Some(token) = body.get("access_token").and_then(|v| v.as_str()) {
        Ok(token.to_string())
    } else {
        Err(format!("Token refresh failed: {}", body).into())
    }
}

async fn build_authenticator(
    client_secret_path: &str,
    token_path: &str,
) -> Result<Authenticator<HttpsConnector<HttpConnector>>, Box<dyn std::error::Error>> {
    // Read client secret
    let cs_data = fs::read_to_string(client_secret_path).map_err(|e| {
        format!(
            "ERROR: Client secret not found: {}\n\
             Download from Google Cloud Console -> Credentials -> OAuth 2.0 Client IDs\n\
             {}",
            client_secret_path, e
        )
    })?;
    let cs_file: ClientSecretFile = serde_json::from_str(&cs_data)?;

    let secret = yup_oauth2::ApplicationSecret {
        client_id: cs_file.installed.client_id.clone(),
        client_secret: cs_file.installed.client_secret.clone(),
        token_uri: cs_file.installed.token_uri.clone(),
        auth_uri: cs_file.installed.auth_uri.clone(),
        redirect_uris: vec!["http://localhost".to_string()],
        ..Default::default()
    };

    // Read the Python-format token and refresh it ourselves
    let td = fs::read_to_string(token_path).map_err(|e| {
        format!("ERROR: Token file not found: {} — {}", token_path, e)
    })?;
    let py_token: PythonToken = serde_json::from_str(&td)?;

    eprintln!("Refreshing access token...");
    let fresh_token = refresh_access_token(&py_token).await?;
    eprintln!("Token refreshed successfully");

    // Save refreshed token back to Python-format file
    {
        let mut updated = py_token.clone();
        updated.token = fresh_token.clone();
        if let Ok(json) = serde_json::to_string_pretty(&updated) {
            let _ = fs::write(token_path, json);
        }
    }

    // Now seed yup-oauth2 cache with the fresh token (valid for 1 hour)
    let cache_dir = std::env::temp_dir().join("youtube_upload_rs_cache");
    fs::create_dir_all(&cache_dir)?;
    let cache_path = cache_dir.join("token_cache.json");

    let mut scope_list: Vec<&str> = SCOPES.to_vec();
    scope_list.sort();

    // Write fresh token with future expiry so yup-oauth2 uses it directly
    let expires_at = Utc::now().timestamp() + 3500;
    let token_info = serde_json::json!([{
        "scopes": scope_list,
        "token": {
            "access_token": fresh_token,
            "refresh_token": py_token.refresh_token,
            "expires_at_timestamp": expires_at,
        }
    }]);
    fs::write(&cache_path, serde_json::to_string(&token_info)?)?;

    let auth = yup_oauth2::InstalledFlowAuthenticator::builder(
        secret,
        yup_oauth2::InstalledFlowReturnMethod::Interactive,
    )
    .flow_delegate(Box::new(HeadlessFlowDelegate))
    .persist_tokens_to_disk(&cache_path)
    .build()
    .await?;

    Ok(auth)
}

/// Save refreshed access token back to the Python-compatible token file
async fn save_token_if_refreshed(
    auth: &Authenticator<HttpsConnector<HttpConnector>>,
    token_path: &str,
) {
    if let Ok(token) = auth.token(SCOPES).await {
        let access_token = token.token().unwrap_or_default().to_string();

        // Read existing token file to preserve refresh_token and other fields
        if let Ok(existing) = fs::read_to_string(token_path) {
            if let Ok(mut py_token) = serde_json::from_str::<PythonToken>(&existing) {
                if py_token.token != access_token && !access_token.is_empty() {
                    py_token.token = access_token;
                    if let Ok(json) = serde_json::to_string_pretty(&py_token) {
                        let _ = fs::write(token_path, json);
                        eprintln!("Token refreshed and saved to {}", token_path);
                    }
                }
            }
        }
    }
}

fn get_gpu_model() -> Option<String> {
    Command::new("nvidia-smi")
        .args(["--query-gpu=name", "--format=csv,noheader"])
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if s.is_empty() {
                    None
                } else {
                    Some(s.lines().next().unwrap_or("").to_string())
                }
            } else {
                None
            }
        })
}

fn read_json_file(path: &Path) -> Option<serde_json::Value> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Validate enhanced file exists
    let enhanced_path = PathBuf::from(&cli.enhanced_file);
    if !enhanced_path.exists() {
        eprintln!("ERROR: File not found: {}", cli.enhanced_file);
        std::process::exit(1);
    }

    let file_size = fs::metadata(&enhanced_path)
        .expect("cannot stat file")
        .len();
    let file_size_mb = file_size / (1024 * 1024);

    // Authenticate
    println!("Authenticating with YouTube...");
    let auth = match build_authenticator(&cli.client_secret, &cli.token).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("ERROR: Authentication failed: {}", e);
            std::process::exit(1);
        }
    };

    // Pre-fetch a token to trigger refresh if needed
    match auth.token(SCOPES).await {
        Ok(_) => {}
        Err(e) => {
            eprintln!("ERROR: Could not obtain access token: {}", e);
            std::process::exit(1);
        }
    }

    // Save refreshed token back to Python-compatible format
    save_token_if_refreshed(&auth, &cli.token).await;

    let client = build_https_client();
    let hub = YouTube::new(client.clone(), auth.clone());

    // Fetch original video details
    println!("Fetching details for original video {}...", cli.video_id);
    let (_, list_result) = hub
        .videos()
        .list(&vec!["snippet".to_string(), "status".to_string()])
        .add_id(&cli.video_id)
        .doit()
        .await
        .unwrap_or_else(|e| {
            eprintln!("ERROR: Failed to fetch video details: {}", e);
            std::process::exit(1);
        });

    let items = list_result.items.unwrap_or_default();
    if items.is_empty() {
        eprintln!("ERROR: Video {} not found on YouTube", cli.video_id);
        std::process::exit(1);
    }

    let snippet = items[0].snippet.as_ref().unwrap();
    let original_title = snippet.title.clone().unwrap_or_default();
    let description = snippet.description.clone().unwrap_or_default();
    let tags = snippet.tags.clone().unwrap_or_default();
    let category_id = snippet
        .category_id
        .clone()
        .unwrap_or_else(|| "22".to_string());

    let desc_preview = if description.len() > 100 {
        format!("{}...", &description[..100])
    } else {
        description.clone()
    };
    println!("  Original title: {}", original_title);
    println!("  Description: {}", desc_preview);
    println!();

    // Build upload title
    let upload_title = if original_title.contains("(Enhanced") {
        original_title.clone()
    } else {
        format!("{} (Enhanced 4K)", original_title)
    };

    println!(
        "Uploading: {} ({} MB)",
        enhanced_path.file_name().unwrap().to_string_lossy(),
        file_size_mb
    );
    println!("Title: {}", upload_title);
    println!("Privacy: public");
    println!();

    // Build the Video resource
    let video = Video {
        snippet: Some(VideoSnippet {
            title: Some(upload_title.clone()),
            description: Some(description.clone()),
            tags: Some(tags),
            category_id: Some(category_id),
            ..Default::default()
        }),
        status: Some(VideoStatus {
            privacy_status: Some("public".to_string()),
            self_declared_made_for_kids: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };

    // Upload with timing
    let upload_start = std::time::Instant::now();

    let file_reader = fs::File::open(&enhanced_path).unwrap_or_else(|e| {
        eprintln!("ERROR: Cannot open file: {}", e);
        std::process::exit(1);
    });

    let mime_type: mime::Mime = "video/x-matroska"
        .parse()
        .expect("valid mime type");

    let result = hub
        .videos()
        .insert(video)
        .upload_resumable(file_reader, mime_type)
        .await;

    let upload_elapsed = upload_start.elapsed();
    let elapsed_secs = upload_elapsed.as_secs();
    let elapsed_mins = elapsed_secs as f64 / 60.0;
    let upload_speed_mbps = if elapsed_secs > 0 {
        (file_size_mb as f64 * 8.0) / elapsed_secs as f64
    } else {
        0.0
    };

    let new_video_id = match result {
        Ok((_response, video)) => {
            let vid = video.id.unwrap_or_default();
            println!();
            println!(
                "Upload complete! Video ID: {}, URL: https://www.youtube.com/watch?v={}",
                vid, vid
            );
            println!(
                "  Upload time: {}s ({:.1}m)",
                elapsed_secs, elapsed_mins
            );
            println!("  Upload speed: {:.0} Mbps", upload_speed_mbps);
            vid
        }
        Err(e) => {
            let err_str = format!("{}", e);
            if err_str.contains("quotaExceeded") {
                eprintln!(
                    "ERROR: YouTube API quota exceeded. Quota resets at midnight Pacific (07:00 UTC)."
                );
                eprintln!(
                    "The enhanced file has NOT been deleted. Retry after quota reset."
                );
            } else {
                eprintln!("ERROR: Upload failed: {}", e);
            }
            std::process::exit(1);
        }
    };

    // Update timing.json in job directory
    let job_dir = enhanced_path.parent().unwrap_or(Path::new("."));
    let timing_path = job_dir.join("timing.json");
    if let Err(e) = update_timing_json(&timing_path, elapsed_secs, upload_speed_mbps) {
        eprintln!("Warning: Could not update timing.json: {}", e);
    }

    // Append to ~/upload_log.jsonl
    if let Err(e) = append_upload_log(
        &cli.video_id,
        &new_video_id,
        &original_title,
        elapsed_secs,
        upload_speed_mbps,
        file_size_mb,
        job_dir,
    ) {
        eprintln!("Warning: Could not append to upload_log.jsonl: {}", e);
    }

    // Send email notification
    if let Some(ref email) = cli.notify {
        if !email.is_empty() {
            let gmail_hub = Gmail::new(client, auth);
            send_email(
                &gmail_hub,
                email,
                &original_title,
                &cli.video_id,
                &new_video_id,
            )
            .await;
        }
    }
}

fn update_timing_json(
    timing_path: &Path,
    upload_secs: u64,
    speed_mbps: f64,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut timing: serde_json::Value = if timing_path.exists() {
        let data = fs::read_to_string(timing_path)?;
        serde_json::from_str(&data).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if let Some(obj) = timing.as_object_mut() {
        obj.insert(
            "youtube_upload".to_string(),
            serde_json::json!(upload_secs),
        );
        obj.insert(
            "upload_speed_mbps".to_string(),
            serde_json::json!(speed_mbps.round() as u64),
        );
    }

    fs::write(timing_path, serde_json::to_string(&timing)?)?;
    Ok(())
}

fn append_upload_log(
    video_id: &str,
    new_video_id: &str,
    title: &str,
    upload_secs: u64,
    speed_mbps: f64,
    file_size_mb: u64,
    job_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let gpu_model = get_gpu_model();

    // Try to read resolution from job_meta.json
    let resolution = read_json_file(&job_dir.join("job_meta.json")).and_then(|v| {
        let w = v.get("width")?.as_u64()?;
        let h = v.get("height")?.as_u64()?;
        Some(format!("{}x{}", w, h))
    });

    // Try to read timing breakdown from timing.json
    let timing = read_json_file(&job_dir.join("timing.json"));
    let extract_time = timing.as_ref().and_then(|v| {
        v.get("extract")
            .and_then(|x| x.as_u64())
            .or_else(|| v.get("extract_frames").and_then(|x| x.as_u64()))
    });
    let upscale_time = timing.as_ref().and_then(|v| {
        v.get("upscale")
            .and_then(|x| x.as_u64())
            .or_else(|| v.get("upscale_frames").and_then(|x| x.as_u64()))
    });
    let reassemble_time = timing.as_ref().and_then(|v| {
        v.get("reassemble")
            .and_then(|x| x.as_u64())
            .or_else(|| v.get("reassemble_video").and_then(|x| x.as_u64()))
    });

    let entry = UploadLogEntry {
        video_id: video_id.to_string(),
        new_video_id: new_video_id.to_string(),
        title: title.to_string(),
        uploaded_at: Utc::now().to_rfc3339(),
        upload_time_s: upload_secs,
        upload_speed_mbps: speed_mbps.round() as u64,
        file_size_mb,
        gpu_model,
        resolution,
        extract_time,
        upscale_time,
        reassemble_time,
    };

    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let log_path = PathBuf::from(home).join("upload_log.jsonl");

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    let json_line = serde_json::to_string(&entry)?;
    writeln!(file, "{}", json_line)?;

    Ok(())
}

async fn send_email(
    gmail: &Gmail<HttpsConnector<HttpConnector>>,
    to_email: &str,
    title: &str,
    original_video_id: &str,
    new_video_id: &str,
) {
    let original_url = format!(
        "https://www.youtube.com/watch?v={}",
        original_video_id
    );
    let enhanced_url = format!(
        "https://www.youtube.com/watch?v={}",
        new_video_id
    );
    let subject = format!("[completed] {} (Enhanced 4K)", title);

    let body = format!(
        "Video Enhancement Completed\n\
         \n\
         Title: {} (Enhanced 4K)\n\
         Original: {}\n\
         Enhanced: {}\n\
         Scale: 4x\n\
         Model: Real-ESRGAN x4plus\n\
         \n\
         Checklist:\n\
         - [x] Upscaling completed\n\
         - [x] Uploaded to YouTube\n\
         - [ ] Quality reviewed\n\
         - [ ] Old video deleted from YouTube\n\
         \n\
         ---\n\
         Generated by old2new\n\
         https://github.com/zdavatz/old2new\n",
        title, original_url, enhanced_url
    );

    // Build RFC 2822 email
    let email_raw = format!(
        "To: {}\r\nSubject: {}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{}",
        to_email, subject, body
    );

    let msg = Message::default();
    let cursor = std::io::Cursor::new(email_raw.into_bytes());
    let mime_type: mime::Mime = "message/rfc822".parse().expect("valid mime");

    match gmail
        .users()
        .messages_send(msg, "me")
        .upload(cursor, mime_type)
        .await
    {
        Ok(_) => println!("Email sent to {}", to_email),
        Err(e) => eprintln!("Warning: Could not send email: {}", e),
    }
}
