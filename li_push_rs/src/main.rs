//! li_push — Upload videos to LinkedIn via Videos API + Posts API
//!
//! Usage:
//!   First time (get token): li_push --auth
//!   Upload video:           li_push <video_file> --title "My Title"
//!   Friends only:           li_push <video_file> --title "My Title" --visibility CONNECTIONS
//!
//! Requires: client_id + client_secret in linkedin_credentials.json
//! Token saved to linkedin_token.json

mod twitter;

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

    /// Run the X / Twitter OAuth2 (PKCE) auth flow to get a token
    #[arg(long)]
    auth_twitter: bool,

    /// Also post the video to X / Twitter (in addition to LinkedIn)
    #[arg(long)]
    twitter: bool,

    /// Post ONLY to X / Twitter (skip LinkedIn)
    #[arg(long)]
    twitter_only: bool,

    /// X / Twitter credentials file (client_id + client_secret)
    #[arg(long, default_value = "twitter_credentials.json")]
    twitter_credentials: String,

    /// X / Twitter token file
    #[arg(long, default_value = "twitter_token.json")]
    twitter_token: String,

    /// Pick a random short Da Vaz video (Enhanced 4K, under 5 min) and upload
    #[arg(long)]
    random_short: bool,

    /// List all previously uploaded videos
    #[arg(long)]
    list: bool,

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

fn upload_log_path() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home).join("li_push_log.jsonl")
}

/// Parse one CSV line into fields, honoring double-quoted fields that may
/// contain commas (RFC4180-style). A naive `split(',')` mis-parses titles
/// like `"ACCORDION LESSON, DPRK"` — the comma inside the quotes shifts every
/// later field, so the trailing `yes` marker lands in the wrong column and the
/// short becomes invisible to --random-short. Doubled quotes (`""`) are an
/// escaped literal quote.
fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(c);
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => fields.push(std::mem::take(&mut cur)),
                _ => cur.push(c),
            }
        }
    }
    fields.push(cur);
    fields
}

fn load_uploaded_ids() -> std::collections::HashSet<String> {
    let path = upload_log_path();
    let mut ids = std::collections::HashSet::new();
    if let Ok(data) = fs::read_to_string(&path) {
        for line in data.lines() {
            if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(yt_id) = entry["youtube_id"].as_str() {
                    if !yt_id.is_empty() {
                        ids.insert(yt_id.to_string());
                    }
                }
            }
        }
    }
    ids
}

fn append_upload_log(youtube_id: &str, title: &str, post_id: &str, post_url: &str) {
    let path = upload_log_path();
    let entry = serde_json::json!({
        "youtube_id": youtube_id,
        "title": title,
        "post_id": post_id,
        "post_url": post_url,
        "timestamp": chrono_now(),
    });
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("open upload log");
    use std::io::Write;
    writeln!(file, "{}", entry).expect("write upload log");
}

fn twitter_log_path() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    PathBuf::from(home).join("li_push_twitter_log.jsonl")
}

fn load_twitter_uploaded_ids() -> std::collections::HashSet<String> {
    let mut ids = std::collections::HashSet::new();
    if let Ok(data) = fs::read_to_string(twitter_log_path()) {
        for line in data.lines() {
            if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(yt_id) = entry["youtube_id"].as_str() {
                    if !yt_id.is_empty() {
                        ids.insert(yt_id.to_string());
                    }
                }
            }
        }
    }
    ids
}

fn append_twitter_log(youtube_id: &str, title: &str, tweet_url: &str) {
    let path = twitter_log_path();
    let entry = serde_json::json!({
        "youtube_id": youtube_id,
        "title": title,
        "tweet_url": tweet_url,
        "timestamp": chrono_now(),
    });
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .expect("open twitter log");
    use std::io::Write;
    writeln!(file, "{}", entry).expect("write twitter log");
}

fn chrono_now() -> String {
    // Simple ISO timestamp without chrono dependency
    let output = std::process::Command::new("date")
        .args(["+%Y-%m-%dT%H:%M:%S"])
        .output()
        .ok();
    output
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn list_uploads() {
    let path = upload_log_path();
    if !path.exists() {
        eprintln!("No uploads yet.");
        return;
    }
    let data = fs::read_to_string(&path).unwrap_or_default();
    let mut count = 0;
    for line in data.lines() {
        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
            count += 1;
            eprintln!("{:3}. [{}] {} — {}",
                count,
                entry["youtube_id"].as_str().unwrap_or("-"),
                entry["title"].as_str().unwrap_or("?"),
                entry["post_url"].as_str().unwrap_or(""),
            );
        }
    }
    if count == 0 {
        eprintln!("No uploads yet.");
    } else {
        eprintln!("\nTotal: {} videos uploaded to LinkedIn", count);
    }
}

/// Extract YouTube video ID from a URL or return input if already an ID
fn extract_youtube_id(input: &str) -> Option<String> {
    if input.contains("youtube.com/watch") {
        input.split("v=").nth(1).and_then(|s| s.split('&').next()).map(|s| s.to_string())
    } else if input.contains("youtu.be/") {
        input.split("youtu.be/").nth(1).and_then(|s| s.split('?').next()).map(|s| s.to_string())
    } else if !input.contains('/') && !input.contains('.') && input.len() >= 8 && input.len() <= 15 {
        Some(input.to_string())
    } else {
        None
    }
}

/// Find YouTube URLs in free text (whitespace/bracket separated, trailing
/// punctuation trimmed).
fn find_youtube_urls(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>' | '[' | ']' | '"')) {
        let t = raw.trim_end_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?'));
        if t.contains("youtube.com/watch") || t.contains("youtu.be/") {
            out.push(t.to_string());
        }
    }
    out
}

/// Extract the original-video link from a short's description. Prefers a URL on
/// a line mentioning "original"; otherwise the first YouTube URL that isn't the
/// short itself.
fn extract_original_link(description: &str, short_id: &str) -> Option<String> {
    for line in description.lines() {
        if line.to_lowercase().contains("original") {
            if let Some(url) = find_youtube_urls(line)
                .into_iter()
                .find(|u| extract_youtube_id(u).as_deref() != Some(short_id))
            {
                return Some(url);
            }
        }
    }
    find_youtube_urls(description)
        .into_iter()
        .find(|u| extract_youtube_id(u).as_deref() != Some(short_id))
}

/// Build a tweet caption: title, optionally with a link on a new line, capped to
/// the 280-character limit.
fn build_tweet_caption(title: &str, link: Option<&str>) -> String {
    match link {
        Some(l) if !l.is_empty() => {
            let suffix = format!("\n{}", l);
            let max_title = 280usize.saturating_sub(suffix.chars().count());
            let title_part = if title.chars().count() > max_title {
                format!("{}…", title.chars().take(max_title.saturating_sub(1)).collect::<String>())
            } else {
                title.to_string()
            };
            format!("{}{}", title_part, suffix)
        }
        _ => {
            if title.chars().count() > 280 {
                format!("{}…", title.chars().take(279).collect::<String>())
            } else {
                title.to_string()
            }
        }
    }
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

    if cli.auth_twitter {
        let creds = twitter::load_credentials(&cli.twitter_credentials).unwrap_or_else(|e| {
            eprintln!("ERROR: {}", e);
            eprintln!("Create {} with:", cli.twitter_credentials);
            eprintln!(r#"  {{"client_id": "...", "client_secret": "..."}}"#);
            std::process::exit(1);
        });
        if let Err(e) = twitter::auth_flow(&creds, &cli.twitter_token).await {
            eprintln!("ERROR: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if cli.list {
        list_uploads();
        return;
    }

    let do_twitter = cli.twitter || cli.twitter_only;
    let do_linkedin = !cli.twitter_only;

    // Dedup set: in twitter-only mode dedup against the X log, otherwise the
    // LinkedIn log (the primary gate for the combined / LinkedIn-only flows).
    let uploaded_ids = if cli.twitter_only {
        load_twitter_uploaded_ids()
    } else {
        load_uploaded_ids()
    };

    // Link to the original (pre-enhancement) video, surfaced on every post.
    // Source order: CSV "Original" column (--random-short) → the short's
    // YouTube description.
    let mut original_link: Option<String> = None;

    // --random-short: pick a random Enhanced 4K short from CSV
    let video_input = if cli.random_short {
        // Read shorts from csv/davaz_enhanced_list.csv
        let csv_path = {
            // Look in CWD first, then next to the binary
            let cwd_path = PathBuf::from("csv/davaz_enhanced_list.csv");
            if cwd_path.exists() {
                cwd_path
            } else {
                let exe_dir = env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                    .unwrap_or_default();
                exe_dir.join("../../csv/davaz_enhanced_list.csv")
            }
        };

        let csv_data = fs::read_to_string(&csv_path).unwrap_or_else(|e| {
            eprintln!("ERROR: Cannot read {}: {}", csv_path.display(), e);
            eprintln!("Run from the old2new directory.");
            std::process::exit(1);
        });

        // Parse CSV: Title,Original,Enhanced 4K,Duration (s),Short
        // Helper: pull the YouTube video ID out of an Enhanced 4K URL field.
        let id_of = |enhanced_url: &str| -> String {
            enhanced_url
                .split("v=").nth(1)
                .and_then(|s| s.split('&').next())
                .unwrap_or("")
                .to_string()
        };

        // First pass: collect the IDs of every NON-short (full-length) row.
        // Some short rows in the CSV mistakenly reuse a full video's ID (the
        // "the OTHER EYE" / "🌝oo🌚" bug — a 15s short pointing at a 30-min,
        // 1.4 GB upload that can never fit LinkedIn's 500 MB limit). Skip any
        // short whose Enhanced ID is also a full-length row so the picker
        // never hands us an unpostable entry, even if the CSV regresses.
        let mut full_video_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for line in csv_data.lines().skip(1) {
            let fields = parse_csv_line(line);
            if fields.len() >= 5 && fields[4].trim() != "yes" {
                let id = id_of(&fields[2]);
                if !id.is_empty() {
                    full_video_ids.insert(id);
                }
            }
        }

        let mut candidates: Vec<(String, String, u64, String)> = Vec::new(); // (id, title, duration, original_url)
        for line in csv_data.lines().skip(1) {
            let fields = parse_csv_line(line);
            if fields.len() >= 5 && fields[4].trim() == "yes" {
                let title = fields[0].to_string();
                let original_url = fields[1].trim().to_string();
                let duration: u64 = fields[3].trim().parse().unwrap_or(0);
                let id = id_of(&fields[2]);

                if id.is_empty() || uploaded_ids.contains(&id) {
                    continue;
                }
                if full_video_ids.contains(&id) {
                    eprintln!(
                        "Skipping '{}' ({}): Enhanced ID {} is a full-length video, not a real short",
                        title.trim(), id, id
                    );
                    continue;
                }
                candidates.push((id, title, duration, original_url));
            }
        }

        if candidates.is_empty() {
            eprintln!("All {} shorts already uploaded to LinkedIn!", uploaded_ids.len());
            std::process::exit(0);
        }

        // Pick random. subsec_nanos() alone was a weak seed — low-resolution
        // clocks and modulo bias made it land on the same index run after run.
        // Mix the full nanosecond count with the PID through splitmix64 for a
        // well-distributed index across separate process invocations.
        let idx = {
            use std::time::SystemTime;
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let mut z = nanos ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            (z % candidates.len() as u64) as usize
        };
        let (id, title, duration, original_url) = &candidates[idx];
        eprintln!("Selected: {} — {} ({}s)", id, title, duration);
        eprintln!("({} shorts available, {} already uploaded)", candidates.len(), uploaded_ids.len());
        if !original_url.is_empty() {
            eprintln!("Original video: {}", original_url);
            original_link = Some(original_url.clone());
        }
        format!("https://www.youtube.com/watch?v={}", id)
    } else {
        match &cli.video_file {
            Some(f) => f.clone(),
            None => {
                eprintln!("ERROR: <video_file> or YouTube URL required (or use --auth/--random-short)");
                std::process::exit(1);
            }
        }
    };

    // Check if input is a YouTube URL or video ID
    let video_input = if !video_input.contains('/') && !video_input.contains('.') && video_input.len() >= 8 && video_input.len() <= 15 {
        format!("https://www.youtube.com/watch?v={}", video_input)
    } else {
        video_input
    };

    // Duplicate check for YouTube videos
    if let Some(yt_id) = extract_youtube_id(&video_input) {
        if uploaded_ids.contains(&yt_id) {
            eprintln!("SKIP: {} already uploaded to LinkedIn. See ~/li_push_log.jsonl", yt_id);
            std::process::exit(0);
        }
    }
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

    // Fallback: if we don't have the original link from the CSV, look for it in
    // the short's YouTube description (create_short appends "Original: <url>").
    if original_link.is_none() && !yt_description.is_empty() {
        let short_id = extract_youtube_id(&video_input).unwrap_or_default();
        original_link = extract_original_link(&yt_description, &short_id);
        if let Some(ref l) = original_link {
            eprintln!("Original video (from description): {}", l);
        }
    }

    let file_size = fs::metadata(&video_path).expect("stat file").len();
    let file_size_mb = file_size as f64 / (1024.0 * 1024.0);

    // Title priority: --title flag > YouTube title > filename
    let title = cli.title.clone().unwrap_or_else(|| {
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
    let description = cli.description.clone().unwrap_or_else(|| {
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

    if do_linkedin {
        upload_to_linkedin(
            &cli,
            &video_path,
            &video_input,
            &title,
            &description,
            visibility,
            file_size,
            file_size_mb,
            original_link.as_deref(),
        )
        .await;
    }

    if do_twitter {
        post_to_twitter(&cli, &video_path, &video_input, &title, original_link.as_deref()).await;
    }

    // Clean up temp file if downloaded from YouTube
    if let Some(tmp) = temp_file {
        let _ = fs::remove_file(&tmp);
        eprintln!("Cleaned up temp file: {}", tmp.display());
    }
}

#[allow(clippy::too_many_arguments)]
async fn upload_to_linkedin(
    cli: &Cli,
    video_path: &Path,
    video_input: &str,
    title: &str,
    description: &str,
    visibility: &str,
    file_size: u64,
    file_size_mb: f64,
    original_link: Option<&str>,
) {
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
    // Surface the original-video link in the post text.
    let commentary = match original_link {
        Some(link) if !link.is_empty() && !description.contains(link) => {
            format!("{}\n\nOriginal: {}", description, link)
        }
        _ => description.to_string(),
    };
    let post_body = serde_json::json!({
        "author": owner,
        "commentary": commentary,
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

    // Log the upload
    let yt_id = extract_youtube_id(video_input).unwrap_or_default();
    append_upload_log(&yt_id, title, post_id, &post_url);

    eprintln!("Video published to LinkedIn!");
    eprintln!("  Post ID: {}", post_id);
    eprintln!("  URL: {}", post_url);
    eprintln!("  Title: {}", title);
    eprintln!("  Size: {:.1} MB", file_size_mb);
    eprintln!("  Visibility: {}", visibility);
}

async fn post_to_twitter(
    cli: &Cli,
    video_path: &Path,
    video_input: &str,
    title: &str,
    original_link: Option<&str>,
) {
    let creds = twitter::load_credentials(&cli.twitter_credentials).unwrap_or_else(|e| {
        eprintln!("ERROR (twitter): {}", e);
        eprintln!(
            "Create {} with client_id + client_secret, then run 'li_push --auth-twitter'.",
            cli.twitter_credentials
        );
        std::process::exit(1);
    });

    let token = twitter::load_token(&cli.twitter_token).unwrap_or_else(|e| {
        eprintln!("ERROR (twitter): {}. Run 'li_push --auth-twitter' first.", e);
        std::process::exit(1);
    });

    let token = twitter::refresh_token(&creds, &token, &cli.twitter_token).await;

    let yt_id = extract_youtube_id(video_input).unwrap_or_default();
    let enhanced_link = if !yt_id.is_empty() {
        format!("https://www.youtube.com/watch?v={}", yt_id)
    } else {
        video_input.to_string()
    };
    // Prefer linking the original video; fall back to the short itself.
    let post_link: &str = original_link.unwrap_or(enhanced_link.as_str());

    // First try a native video tweet — caption is title + original link.
    let video_caption = build_tweet_caption(title, Some(post_link));

    // X rejects video above 1920x1200 (these are Enhanced 4K sources), and is
    // picky about codecs, so transcode to an X-friendly spec first: H.264 High /
    // yuv420p, longest edge <= 1280, even dimensions, 30 fps, AAC audio.
    let x_video_path = env::temp_dir().join(format!("li_push_x_video_{}.mp4", std::process::id()));
    eprintln!("Transcoding to an X-compatible video...");
    let tr = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-i", video_path.to_str().unwrap_or_default(),
            "-vf", "scale='min(1280,iw)':'min(1280,ih)':force_original_aspect_ratio=decrease,scale=trunc(iw/2)*2:trunc(ih/2)*2",
            "-c:v", "libx264",
            "-profile:v", "high",
            "-pix_fmt", "yuv420p",
            "-r", "30",
            "-preset", "veryfast",
            "-crf", "23",
            "-c:a", "aac",
            "-b:a", "128k",
            "-movflags", "+faststart",
            x_video_path.to_str().unwrap_or_default(),
        ])
        .output();
    let upload_path: PathBuf = if matches!(&tr, Ok(o) if o.status.success()) && x_video_path.exists() {
        x_video_path.clone()
    } else {
        eprintln!("WARNING: transcode failed; uploading the original file.");
        video_path.to_path_buf()
    };

    eprintln!("Posting to X / Twitter (native video)...");
    let video_result = twitter::publish_video(&token.access_token, &upload_path, &video_caption).await;
    let _ = fs::remove_file(&x_video_path);
    match video_result {
        Ok(url) => {
            append_twitter_log(&yt_id, title, &url);
            eprintln!("Video published to X!");
            eprintln!("  Tweet: {}", url);
            eprintln!("  Link: {}", post_link);
            eprintln!("  Title: {}", title);
            return;
        }
        Err(e) => {
            eprintln!("WARNING: native video post failed: {}", e);
            eprintln!("Falling back to a frame image + link...");
        }
    }

    // Fallback: post a representative frame (1s in) + the video link.
    let frame_path = env::temp_dir().join(format!("li_push_x_frame_{}.jpg", std::process::id()));
    let ff = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-ss", "1",
            "-i", video_path.to_str().unwrap_or_default(),
            "-frames:v", "1",
            "-q:v", "2",
            frame_path.to_str().unwrap_or_default(),
        ])
        .output();
    let frame_ok = matches!(&ff, Ok(o) if o.status.success() && frame_path.exists());
    if !frame_ok {
        let _ = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-i", video_path.to_str().unwrap_or_default(),
                "-frames:v", "1",
                "-q:v", "2",
                frame_path.to_str().unwrap_or_default(),
            ])
            .output();
    }
    if !frame_path.exists() {
        eprintln!("ERROR: Could not extract a frame for the X image (is ffmpeg installed?).");
        std::process::exit(1);
    }

    // Image caption: title + link (original preferred), capped to 280 chars.
    let caption = build_tweet_caption(title, Some(post_link));

    match twitter::publish_image(&token.access_token, &frame_path, &caption).await {
        Ok(url) => {
            append_twitter_log(&yt_id, title, &url);
            eprintln!("Posted to X (frame + link)!");
            eprintln!("  Tweet: {}", url);
            eprintln!("  Link: {}", post_link);
            eprintln!("  Title: {}", title);
        }
        Err(e) => {
            let _ = fs::remove_file(&frame_path);
            eprintln!("ERROR: X post failed: {}", e);
            std::process::exit(1);
        }
    }
    let _ = fs::remove_file(&frame_path);
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
