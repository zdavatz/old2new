//! Worker-thread pipeline: validate inputs → invoke yt-dlp to extract
//! the requested segment → upload to YouTube. Sends `Event`s back to
//! the UI over a crossbeam channel.

use crate::oauth;
use crate::settings::Settings;
use crate::youtube::{upload_video, VideoBody, VideoSnippet, VideoStatus};
use crossbeam_channel::Sender;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Clone)]
pub enum Event {
    Log(String),
    Progress { phase: String, fraction: f32, detail: String },
    Done(String),
    Preview(PathBuf),
    Error(String),
}

pub struct Job {
    pub source: String,
    pub start: String,
    pub end: String,
    pub title: String,
    pub description: String,
    pub privacy: String,
    pub preview_only: bool,
}

pub struct UploadJob {
    pub file: PathBuf,
    pub title: String,
    pub description: String,
    pub privacy: String,
}

/// Direct upload of a local video file to YouTube — no yt-dlp, no
/// segment extraction. Used by the Upload tab.
pub fn run_upload(job: UploadJob, settings: Settings, tx: Sender<Event>) {
    let size = match std::fs::metadata(&job.file) {
        Ok(m) => m.len(),
        Err(e) => {
            let _ = tx.send(Event::Error(format!("stat {}: {}", job.file.display(), e)));
            return;
        }
    };
    let _ = tx.send(Event::Log(format!(
        "Uploading {} ({:.1} MB)",
        job.file.display(),
        size as f64 / 1_048_576.0,
    )));

    let _ = tx.send(Event::Log("Refreshing YouTube access token…".into()));
    let access_token = match oauth::refresh_access_token(&settings.client_id, &settings.client_secret) {
        Ok(t) => t,
        Err(e) => {
            let _ = tx.send(Event::Error(format!("auth: {}", e)));
            return;
        }
    };

    let body = VideoBody {
        snippet: VideoSnippet {
            title: &job.title,
            description: &job.description,
            category_id: "22",
        },
        status: VideoStatus {
            privacy_status: &job.privacy,
            self_declared_made_for_kids: false,
        },
    };

    let progress_tx = tx.clone();
    let result = upload_video(&access_token, &job.file, &body, |sent, total| {
        let f = if total == 0 { 0.0 } else { sent as f32 / total as f32 };
        let detail = format!("{:.1} / {:.1} MB", sent as f64 / 1_048_576.0, total as f64 / 1_048_576.0);
        let _ = progress_tx.send(Event::Progress { phase: "Uploading".into(), fraction: f, detail });
    });

    match result {
        Ok(id) => {
            let _ = tx.send(Event::Done(format!("https://www.youtube.com/watch?v={}", id)));
        }
        Err(e) => {
            let _ = tx.send(Event::Error(format!("upload: {}", e)));
        }
    }
}

/// Convert "mm:ss", "hh:mm:ss", or a bare seconds value to seconds.
pub fn parse_timestamp(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split(':').collect();
    let nums: Vec<f64> = parts.iter().map(|p| p.trim().parse().ok().unwrap_or(0.0)).collect();
    match nums.len() {
        1 => Some(nums[0]),
        2 => Some(nums[0] * 60.0 + nums[1]),
        3 => Some(nums[0] * 3600.0 + nums[1] * 60.0 + nums[2]),
        _ => None,
    }
}

fn parse_yt_dlp_pct(line: &str) -> Option<f32> {
    let s = line.trim_start();
    let after = s.strip_prefix("[download]")?.trim_start();
    let pct_end = after.find('%')?;
    let pct: f32 = after[..pct_end].trim().parse().ok()?;
    Some((pct / 100.0).clamp(0.0, 1.0))
}

fn parse_ffmpeg_time(line: &str) -> Option<f64> {
    let i = line.find("time=")?;
    let rest = &line[i + 5..];
    let end = rest.find(' ').unwrap_or(rest.len());
    let ts = rest[..end].trim();
    let parts: Vec<&str> = ts.split(':').collect();
    if parts.len() != 3 { return None; }
    let h: f64 = parts[0].parse().ok()?;
    let m: f64 = parts[1].parse().ok()?;
    let sec: f64 = parts[2].parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + sec)
}

pub fn run(job: Job, settings: Settings, tx: Sender<Event>) {
    let _ = tx.send(Event::Log(format!("Starting job for {}", job.source)));
    let segment_secs: f64 = parse_timestamp(&job.end)
        .and_then(|e| parse_timestamp(&job.start).map(|s| (e - s).max(0.0)))
        .unwrap_or(0.0);

    let video_id = extract_video_id(&job.source);
    let url = if job.source.contains("://") {
        job.source.clone()
    } else {
        format!("https://www.youtube.com/watch?v={}", video_id)
    };
    let original_url = format!("https://www.youtube.com/watch?v={}", video_id);

    // Cache cut segments at <cache_dir>/segments/<video_id>_<start>_<end>.mp4.
    // If the exact same (video_id, start, end) is requested again — same
    // video, same timestamps — we skip yt-dlp entirely. Different
    // timestamps re-run yt-dlp with --download-sections, which only
    // pulls the bytes for that range (a few MB for a 30-second clip).
    let stamp = format!("{}_{}", job.start.replace(':', "_"), job.end.replace(':', "_"));
    let cache_dir = segment_cache_dir();
    if let Err(e) = std::fs::create_dir_all(&cache_dir) {
        let _ = tx.send(Event::Error(format!("mkdir {}: {}", cache_dir.display(), e)));
        return;
    }
    let cached = cache_dir.join(format!("{}_{}.mp4", video_id, stamp));

    let out = if cached.exists() {
        let _ = tx.send(Event::Log(format!(
            "Reusing cached segment {} ({:.1} MB)",
            cached.display(),
            cached.metadata().map(|m| m.len()).unwrap_or(0) as f64 / 1_048_576.0,
        )));
        cached.clone()
    } else {
        let _ = tx.send(Event::Log(format!("Downloading {} ({}-{}) → {}", url, job.start, job.end, cached.display())));
        let _ = tx.send(Event::Progress { phase: "Downloading".into(), fraction: 0.0, detail: String::new() });
        if let Err(e) = run_yt_dlp(&url, &job.start, &job.end, &cached, &settings, &tx, segment_secs) {
            let _ = std::fs::remove_file(&cached);
            let _ = tx.send(Event::Error(format!("yt-dlp: {}", e)));
            return;
        }
        cached.clone()
    };

    let size = match std::fs::metadata(&out) {
        Ok(m) => m.len(),
        Err(e) => {
            let _ = tx.send(Event::Error(format!("yt-dlp produced no file: {}", e)));
            return;
        }
    };
    let _ = tx.send(Event::Log(format!("Segment ready: {:.1} MB", size as f64 / 1_048_576.0)));

    if job.preview_only {
        let _ = tx.send(Event::Log(format!("Preview file kept at {}", out.display())));
        let _ = tx.send(Event::Preview(out));
        return;
    }

    let _ = tx.send(Event::Log("Refreshing YouTube access token…".to_string()));
    let access_token = match oauth::refresh_access_token(&settings.client_id, &settings.client_secret) {
        Ok(t) => t,
        Err(e) => {
            let _ = tx.send(Event::Error(format!("auth: {}", e)));
            return;
        }
    };

    let description = format!(
        "{}\n\nOriginal: {} ({}-{})",
        job.description, original_url, job.start, job.end
    );

    let body = VideoBody {
        snippet: VideoSnippet {
            title: &job.title,
            description: &description,
            category_id: "22",
        },
        status: VideoStatus {
            privacy_status: &job.privacy,
            self_declared_made_for_kids: false,
        },
    };

    let _ = tx.send(Event::Log(format!("Uploading {:.1} MB…", size as f64 / 1024.0 / 1024.0)));
    let progress_tx = tx.clone();
    let result = upload_video(&access_token, &out, &body, |sent, total| {
        let f = if total == 0 { 0.0 } else { sent as f32 / total as f32 };
        let detail = format!("{:.1} / {:.1} MB", sent as f64 / 1_048_576.0, total as f64 / 1_048_576.0);
        let _ = progress_tx.send(Event::Progress { phase: "Uploading".into(), fraction: f, detail });
    });

    let _ = std::fs::remove_file(&out);

    match result {
        Ok(id) => {
            let _ = tx.send(Event::Done(format!("https://www.youtube.com/watch?v={}", id)));
        }
        Err(e) => {
            let _ = tx.send(Event::Error(format!("upload: {}", e)));
        }
    }
}

/// Where to keep cut segments so an identical `(video_id, start, end)`
/// re-run skips yt-dlp entirely. macOS:
/// `~/Library/Caches/create_shorts/segments/`.
fn segment_cache_dir() -> PathBuf {
    let base = dirs::cache_dir().unwrap_or_else(|| std::env::temp_dir());
    base.join("create_shorts").join("segments")
}

/// Run yt-dlp with `--download-sections` to fetch only the requested
/// time range — much faster than downloading the full source. Sized
/// for ~10s segments out of multi-hour videos: 30 MB-ish typical, not
/// gigabytes.
fn run_yt_dlp(
    url: &str,
    start: &str,
    end: &str,
    out: &PathBuf,
    settings: &Settings,
    tx: &Sender<Event>,
    segment_secs: f64,
) -> Result<(), String> {
    let mut cmd = Command::new("yt-dlp");
    cmd.args([
        "-f",
        "bestvideo[height<=2160]+bestaudio/best",
        "--download-sections",
        &format!("*{}-{}", start, end),
        "--force-keyframes-at-cuts",
        "--merge-output-format",
        "mp4",
        "--newline",
        "-o",
        out.to_str().ok_or("non-utf8 path")?,
    ]);
    if !settings.cookies_browser.is_empty() {
        cmd.arg("--cookies-from-browser").arg(&settings.cookies_browser);
    }
    cmd.arg(url);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        format!("yt-dlp not found ({}). Install with `brew install yt-dlp` or `pipx install yt-dlp`.", e)
    })?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, segment_secs)));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, segment_secs)));
    let status = child.wait().map_err(|e| format!("wait: {}", e))?;
    if let Some(h) = h_o { let _ = h.join(); }
    if let Some(h) = h_e { let _ = h.join(); }
    if !status.success() {
        return Err(format!("yt-dlp exited with {:?}", status.code()));
    }
    Ok(())
}

/// Reads bytes from yt-dlp/ffmpeg and splits on either `\r` or `\n` so
/// in-place progress lines (which use carriage-return only) are
/// captured. yt-dlp `[download] X.X%` and ffmpeg `time=HH:MM:SS` lines
/// are intercepted as Progress events; everything else goes to Log.
fn stream_progress<R: Read>(reader: R, tx: Sender<Event>, segment_secs: f64) {
    let mut br = std::io::BufReader::new(reader);
    let mut buf = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        match br.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                let b = byte[0];
                if b == b'\r' || b == b'\n' {
                    if !buf.is_empty() {
                        let line = String::from_utf8_lossy(&buf).to_string();
                        emit_line(&line, &tx, segment_secs);
                        buf.clear();
                    }
                } else {
                    buf.push(b);
                }
            }
            Err(_) => break,
        }
    }
    if !buf.is_empty() {
        let line = String::from_utf8_lossy(&buf).to_string();
        emit_line(&line, &tx, segment_secs);
    }
}

fn emit_line(line: &str, tx: &Sender<Event>, segment_secs: f64) {
    if let Some(frac) = parse_yt_dlp_pct(line) {
        let _ = tx.send(Event::Progress {
            phase: "Downloading".into(),
            fraction: frac,
            detail: line.trim().to_string(),
        });
        return;
    }
    if let Some(t) = parse_ffmpeg_time(line) {
        if segment_secs > 0.0 {
            let frac = ((t / segment_secs) as f32).clamp(0.0, 1.0);
            let _ = tx.send(Event::Progress {
                phase: "Encoding segment".into(),
                fraction: frac,
                detail: format!("{:.1}s / {:.1}s", t, segment_secs),
            });
            return;
        }
    }
    let _ = tx.send(Event::Log(line.to_string()));
}

fn extract_video_id(input: &str) -> String {
    if !input.contains("://") && !input.contains('/') && !input.contains('?') {
        return input.to_string();
    }
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

