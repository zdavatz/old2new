use chrono::Utc;
use clap::Parser;
use google_youtube3::api::{Video, VideoSnippet, VideoStatus};
use google_youtube3::oauth2 as yup_oauth2;
use google_youtube3::YouTube;
use hyper::client::HttpConnector;
use hyper::Client;
use hyper_rustls::HttpsConnector;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::{Read as IoRead, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use yup_oauth2::authenticator::Authenticator;
use yup_oauth2::authenticator_delegate::InstalledFlowDelegate;

const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/youtube.upload",
    "https://www.googleapis.com/auth/youtube.readonly",
    "https://www.googleapis.com/auth/youtube",
];

/// Download a segment of a YouTube video and re-upload as a new video.
///
/// Examples:
///   create_short "https://youtu.be/iXs-sL-Sf8w" -s 12:44 -e 13:14 \
///     -t "poco forte" -d "Eugen Beck"
///
///   create_short iXs-sL-Sf8w -s 8:37 -e 8:57 -t "back ache" \
///     -d "Eugen Beck" --privacy unlisted
#[derive(Parser)]
#[command(name = "create_short", version)]
struct Cli {
    /// YouTube URL or bare video ID
    source: String,

    /// Segment start (e.g. 12:44, 1:02:30, or seconds)
    #[arg(short = 's', long)]
    start: String,

    /// Segment end (e.g. 13:14, 1:03:00, or seconds)
    #[arg(short = 'e', long)]
    end: String,

    /// Title of the new YouTube video
    #[arg(short = 't', long)]
    title: String,

    /// Description of the new YouTube video
    #[arg(short = 'd', long)]
    description: String,

    /// Privacy: public, unlisted, or private
    #[arg(short = 'p', long, default_value = "public")]
    privacy: String,

    /// yt-dlp format selector
    #[arg(short = 'f', long, default_value = "bestvideo[height<=2160]+bestaudio/best")]
    format: String,

    /// Output file path (default: /tmp/create_short/<sanitized-title>.mp4)
    #[arg(short = 'o', long)]
    output: Option<String>,

    /// Keep the downloaded file after upload (default: delete)
    #[arg(long)]
    keep: bool,

    /// Download only — skip upload
    #[arg(long)]
    dry_run: bool,

    /// OAuth client secret file
    #[arg(long, default_value = "client_secret.json")]
    client_secret: String,

    /// OAuth token file (Python-format google-auth)
    #[arg(long, default_value = "youtube_token.json")]
    token: String,

    /// YouTube category id (default 22 = People & Blogs)
    #[arg(long, default_value = "22")]
    category_id: String,
}

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

struct HeadlessFlowDelegate;
impl InstalledFlowDelegate for HeadlessFlowDelegate {
    fn present_user_url<'a>(
        &'a self,
        url: &'a str,
        _need_code: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            eprintln!("Open this URL to authorize: {}", url);
            Err("Interactive auth not supported in this binary".to_string())
        })
    }
}

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

async fn refresh_access_token(
    py_token: &PythonToken,
) -> Result<String, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let resp = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", py_token.client_id.as_str()),
            ("client_secret", py_token.client_secret.as_str()),
            ("refresh_token", py_token.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await?;
    let body: serde_json::Value = resp.json().await?;
    body.get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("Token refresh failed: {}", body).into())
}

fn resolve_path(given: &str) -> Option<String> {
    if Path::new(given).exists() {
        return Some(given.to_string());
    }
    let home = env::var("HOME").ok()?;
    let p = format!("{}/{}", home, given);
    if Path::new(&p).exists() {
        Some(p)
    } else {
        None
    }
}

async fn build_authenticator(
    client_secret_path: &str,
    token_path: &str,
) -> Result<Authenticator<HttpsConnector<HttpConnector>>, Box<dyn std::error::Error>> {
    let cs_path = resolve_path(client_secret_path)
        .ok_or_else(|| format!("client_secret not found: {}", client_secret_path))?;
    let cs_data = fs::read_to_string(&cs_path)?;
    let cs_file: ClientSecretFile = serde_json::from_str(&cs_data)?;
    let secret = yup_oauth2::ApplicationSecret {
        client_id: cs_file.installed.client_id.clone(),
        client_secret: cs_file.installed.client_secret.clone(),
        token_uri: cs_file.installed.token_uri.clone(),
        auth_uri: cs_file.installed.auth_uri.clone(),
        redirect_uris: vec!["http://localhost".to_string()],
        ..Default::default()
    };

    let tk_path = resolve_path(token_path)
        .ok_or_else(|| format!("token file not found: {}", token_path))?;
    let td = fs::read_to_string(&tk_path)?;
    let py_token: PythonToken = serde_json::from_str(&td)?;

    eprintln!("Refreshing access token...");
    let fresh_token = refresh_access_token(&py_token).await?;

    // Persist refreshed token back
    {
        let mut updated = py_token.clone();
        updated.token = fresh_token.clone();
        if let Ok(json) = serde_json::to_string_pretty(&updated) {
            let _ = fs::write(&tk_path, json);
        }
    }

    // Seed yup-oauth2 cache
    let cache_dir = std::env::temp_dir().join("create_short_rs_cache");
    fs::create_dir_all(&cache_dir)?;
    let cache_path = cache_dir.join("token_cache.json");
    let mut scope_list: Vec<&str> = SCOPES.to_vec();
    scope_list.sort();
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

/// Extract bare video ID from a URL or pass-through if already an ID.
fn extract_video_id(input: &str) -> String {
    if !input.contains("://") && !input.contains('/') && !input.contains('?') {
        return input.to_string();
    }
    // Try common patterns
    if let Some(idx) = input.find("v=") {
        let rest = &input[idx + 2..];
        let end = rest.find(|c: char| c == '&' || c == '#').unwrap_or(rest.len());
        return rest[..end].to_string();
    }
    if let Some(idx) = input.rfind('/') {
        let rest = &input[idx + 1..];
        let end = rest.find(|c: char| c == '?' || c == '&' || c == '#').unwrap_or(rest.len());
        return rest[..end].to_string();
    }
    input.to_string()
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

fn timestamp_label(start: &str, end: &str) -> String {
    format!("{}-{}", start.replace(':', "_"), end.replace(':', "_"))
}

struct ProgressReader {
    inner: fs::File,
    total: u64,
    read_so_far: u64,
    last_pct: u64,
}
impl ProgressReader {
    fn new(file: fs::File, total: u64) -> Self {
        Self { inner: file, total, read_so_far: 0, last_pct: 0 }
    }
}
impl IoRead for ProgressReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read_so_far += n as u64;
        let pct = if self.total > 0 { self.read_so_far * 100 / self.total } else { 0 };
        if pct != self.last_pct {
            self.last_pct = pct;
            let mb_done = self.read_so_far / (1024 * 1024);
            let mb_total = self.total / (1024 * 1024);
            eprint!("\rUploading: {}% ({}/{}MB)    ", pct, mb_done, mb_total);
        }
        Ok(n)
    }
}
impl Seek for ProgressReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let r = self.inner.seek(pos)?;
        self.read_so_far = r;
        Ok(r)
    }
}

fn run_yt_dlp(
    url: &str,
    start: &str,
    end: &str,
    format: &str,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    eprintln!("Downloading {} ({}-{}) → {}", url, start, end, output.display());

    let status = Command::new("yt-dlp")
        .args([
            "-f",
            format,
            "--download-sections",
            &format!("*{}-{}", start, end),
            "--force-keyframes-at-cuts",
            "--merge-output-format",
            "mp4",
            "-o",
            output.to_str().ok_or("non-utf8 output path")?,
            url,
        ])
        .status()?;
    if !status.success() {
        return Err(format!("yt-dlp failed with {:?}", status.code()).into());
    }
    if !output.exists() {
        return Err(format!("yt-dlp produced no file at {}", output.display()).into());
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if !["public", "unlisted", "private"].contains(&cli.privacy.as_str()) {
        eprintln!("ERROR: --privacy must be public|unlisted|private");
        std::process::exit(1);
    }

    let video_id = extract_video_id(&cli.source);
    let url = if cli.source.contains("://") {
        cli.source.clone()
    } else {
        format!("https://www.youtube.com/watch?v={}", video_id)
    };

    let output_path = match &cli.output {
        Some(p) => PathBuf::from(p),
        None => {
            let label = sanitize_filename(&cli.title);
            let stamp = timestamp_label(&cli.start, &cli.end);
            PathBuf::from(format!("/tmp/create_short/{}_{}.mp4", label, stamp))
        }
    };

    if let Err(e) = run_yt_dlp(&url, &cli.start, &cli.end, &cli.format, &output_path) {
        eprintln!("ERROR: download failed: {}", e);
        std::process::exit(1);
    }

    let file_size = fs::metadata(&output_path).expect("stat output").len();
    let file_size_mb = file_size / (1024 * 1024);
    eprintln!("Downloaded {} MB", file_size_mb);

    if cli.dry_run {
        println!("DRY RUN — file kept at {}", output_path.display());
        return;
    }

    eprintln!("Authenticating with YouTube...");
    let auth = match build_authenticator(&cli.client_secret, &cli.token).await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("ERROR: auth failed: {}", e);
            std::process::exit(1);
        }
    };
    if let Err(e) = auth.token(SCOPES).await {
        eprintln!("ERROR: could not obtain access token: {}", e);
        std::process::exit(1);
    }

    let client = build_https_client();
    let hub = YouTube::new(client, auth);

    let original_url = format!("https://www.youtube.com/watch?v={}", video_id);
    let full_description = format!(
        "{}\n\nOriginal: {} ({}-{})",
        cli.description, original_url, cli.start, cli.end
    );

    let video = Video {
        snippet: Some(VideoSnippet {
            title: Some(cli.title.clone()),
            description: Some(full_description),
            category_id: Some(cli.category_id.clone()),
            ..Default::default()
        }),
        status: Some(VideoStatus {
            privacy_status: Some(cli.privacy.clone()),
            self_declared_made_for_kids: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };

    let file_reader = fs::File::open(&output_path).unwrap_or_else(|e| {
        eprintln!("ERROR: cannot open {}: {}", output_path.display(), e);
        std::process::exit(1);
    });
    let progress_reader = ProgressReader::new(file_reader, file_size);
    let mime_type: mime::Mime = "video/mp4".parse().expect("mime");

    eprintln!("Uploading {} ({} MB) — title: {} — privacy: {}",
        output_path.display(), file_size_mb, cli.title, cli.privacy);

    let upload_start = std::time::Instant::now();
    let result = hub
        .videos()
        .insert(video)
        .upload_resumable(progress_reader, mime_type)
        .await;
    let elapsed = upload_start.elapsed().as_secs();

    match result {
        Ok((_resp, v)) => {
            let vid = v.id.unwrap_or_default();
            println!();
            println!("DONE: https://www.youtube.com/watch?v={}", vid);
            println!("  Upload time: {}s", elapsed);
        }
        Err(e) => {
            eprintln!();
            eprintln!("ERROR: upload failed: {}", e);
            eprintln!("File kept at {}", output_path.display());
            std::process::exit(1);
        }
    }

    if !cli.keep {
        let _ = fs::remove_file(&output_path);
        eprintln!("Removed local file {}", output_path.display());
    } else {
        eprintln!("Kept local file at {}", output_path.display());
    }
}
