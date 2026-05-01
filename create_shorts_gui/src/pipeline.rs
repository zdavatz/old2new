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

    // 1. Ensure the *full* video is cached at <cache>/<video_id>.mp4.
    //    Re-cutting the same video with different timestamps reuses
    //    this file instead of hitting yt-dlp again.
    let full = match ensure_full_video_cached(&video_id, &url, &settings, &tx, segment_secs) {
        Ok(p) => p,
        Err(e) => { let _ = tx.send(Event::Error(format!("download: {}", e))); return; }
    };

    let tmp = std::env::temp_dir().join("create_shorts_gui");
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        let _ = tx.send(Event::Error(format!("mkdir {}: {}", tmp.display(), e)));
        return;
    }
    let stem = sanitize_filename(&job.title);
    let stamp = format!("{}-{}", job.start.replace(':', "_"), job.end.replace(':', "_"));
    let out = tmp.join(format!("{}_{}.mp4", stem, stamp));

    // 2. Cut the requested segment from the cached full video.
    if let Err(e) = cut_segment(&full, &job.start, &job.end, &out, segment_secs, &tx) {
        let _ = tx.send(Event::Error(format!("ffmpeg: {}", e)));
        return;
    }

    let size = match std::fs::metadata(&out) {
        Ok(m) => m.len(),
        Err(e) => {
            let _ = tx.send(Event::Error(format!("ffmpeg produced no file: {}", e)));
            return;
        }
    };
    let _ = tx.send(Event::Log(format!("Cut segment: {:.1} MB", size as f64 / 1024.0 / 1024.0)));

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

/// Cache root for full-video downloads. macOS:
/// `~/Library/Caches/create_shorts/videos/`. Cross-platform via `dirs`.
fn cache_root() -> PathBuf {
    let base = dirs::cache_dir().unwrap_or_else(|| std::env::temp_dir());
    base.join("create_shorts").join("videos")
}

fn ensure_full_video_cached(
    video_id: &str,
    url: &str,
    settings: &Settings,
    tx: &Sender<Event>,
    segment_secs: f64,
) -> Result<PathBuf, String> {
    let dir = cache_root();
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {}", dir.display(), e))?;
    let target = dir.join(format!("{}.mp4", video_id));
    if target.exists() {
        let _ = tx.send(Event::Log(format!(
            "Reusing cached full video: {} ({:.1} MB)",
            target.display(),
            target.metadata().map(|m| m.len()).unwrap_or(0) as f64 / 1_048_576.0,
        )));
        return Ok(target);
    }
    let _ = tx.send(Event::Log(format!(
        "Downloading full video {} → {}",
        url,
        target.display(),
    )));
    let _ = tx.send(Event::Progress { phase: "Downloading".into(), fraction: 0.0, detail: String::new() });

    let mut cmd = Command::new("yt-dlp");
    cmd.args([
        "-f",
        "bestvideo[height<=2160]+bestaudio/best",
        "--merge-output-format",
        "mp4",
        "--newline",
        "-o",
        target.to_str().ok_or("non-utf8 path")?,
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
        // yt-dlp may leave a partial file behind — remove it so a retry
        // re-downloads cleanly.
        let _ = std::fs::remove_file(&target);
        return Err(format!("yt-dlp exited with {:?}", status.code()));
    }
    Ok(target)
}

/// Cut [start, end] from `full` into `out` using ffmpeg. Re-encodes for
/// frame-accurate cuts (since libx264 is fast on a few-minute segment
/// and the result will be re-encoded by YouTube anyway).
fn cut_segment(
    full: &PathBuf,
    start: &str,
    end: &str,
    out: &PathBuf,
    segment_secs: f64,
    tx: &Sender<Event>,
) -> Result<(), String> {
    let _ = tx.send(Event::Log(format!("Cutting {}-{} from cache", start, end)));
    let _ = tx.send(Event::Progress { phase: "Encoding segment".into(), fraction: 0.0, detail: String::new() });

    let start_secs = parse_timestamp(start).ok_or("invalid start timestamp")?;

    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-y",
        "-ss", &format!("{:.3}", start_secs),
        "-i", full.to_str().ok_or("non-utf8 input path")?,
        "-t", &format!("{:.3}", segment_secs.max(0.0)),
        "-c:v", "libx264",
        "-preset", "fast",
        "-crf", "18",
        "-c:a", "aac",
        "-b:a", "192k",
        "-movflags", "+faststart",
        out.to_str().ok_or("non-utf8 output path")?,
    ]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg spawn: {}", e))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, segment_secs)));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, segment_secs)));
    let status = child.wait().map_err(|e| format!("ffmpeg wait: {}", e))?;
    if let Some(h) = h_o { let _ = h.join(); }
    if let Some(h) = h_e { let _ = h.join(); }
    if !status.success() {
        return Err(format!("ffmpeg exited with {:?}", status.code()));
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

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}
