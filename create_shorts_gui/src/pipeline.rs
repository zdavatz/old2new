//! Worker-thread pipeline: validate inputs → invoke yt-dlp to extract
//! the requested segment → upload to YouTube. Sends `Event`s back to
//! the UI over a crossbeam channel.

use crate::oauth;
use crate::settings::Settings;
use crate::youtube::{upload_video, VideoBody, VideoSnippet, VideoStatus};
use crossbeam_channel::Sender;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Clone)]
pub enum Event {
    Log(String),
    Progress(u64, u64),
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

pub fn run(job: Job, settings: Settings, tx: Sender<Event>) {
    let _ = tx.send(Event::Log(format!("Starting job for {}", job.source)));

    let video_id = extract_video_id(&job.source);
    let url = if job.source.contains("://") {
        job.source.clone()
    } else {
        format!("https://www.youtube.com/watch?v={}", video_id)
    };
    let original_url = format!("https://www.youtube.com/watch?v={}", video_id);

    let tmp = std::env::temp_dir().join("create_shorts_gui");
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        let _ = tx.send(Event::Error(format!("mkdir {}: {}", tmp.display(), e)));
        return;
    }
    let stem = sanitize_filename(&job.title);
    let stamp = format!("{}-{}", job.start.replace(':', "_"), job.end.replace(':', "_"));
    let out = tmp.join(format!("{}_{}.mp4", stem, stamp));

    let _ = tx.send(Event::Log(format!("Downloading {} ({}-{}) → {}", url, job.start, job.end, out.display())));

    if let Err(e) = run_yt_dlp(&url, &job.start, &job.end, &out, &settings, &tx) {
        let _ = tx.send(Event::Error(format!("yt-dlp failed: {}", e)));
        return;
    }

    let size = match std::fs::metadata(&out) {
        Ok(m) => m.len(),
        Err(e) => {
            let _ = tx.send(Event::Error(format!("yt-dlp produced no file: {}", e)));
            return;
        }
    };
    let _ = tx.send(Event::Log(format!("Downloaded {:.1} MB", size as f64 / 1024.0 / 1024.0)));

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
        let _ = progress_tx.send(Event::Progress(sent, total));
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

fn run_yt_dlp(
    url: &str,
    start: &str,
    end: &str,
    out: &PathBuf,
    settings: &Settings,
    tx: &Sender<Event>,
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
        "-o",
        out.to_str().ok_or("non-utf8 path")?,
    ]);
    if !settings.cookies_browser.is_empty() {
        cmd.arg("--cookies-from-browser").arg(&settings.cookies_browser);
    }
    cmd.arg(url);

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("yt-dlp not found ({}). Install with `brew install yt-dlp` or `pipx install yt-dlp`.", e))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_out = tx.clone();
    let tx_err = tx.clone();
    let out_thread = stdout.map(|s| {
        std::thread::spawn(move || stream_lines(s, tx_out))
    });
    let err_thread = stderr.map(|s| {
        std::thread::spawn(move || stream_lines(s, tx_err))
    });

    let status = child.wait().map_err(|e| format!("wait: {}", e))?;
    if let Some(t) = out_thread { let _ = t.join(); }
    if let Some(t) = err_thread { let _ = t.join(); }

    if !status.success() {
        return Err(format!("yt-dlp exited with {:?}", status.code()));
    }
    Ok(())
}

fn stream_lines<R: std::io::Read>(reader: R, tx: Sender<Event>) {
    use std::io::{BufRead, BufReader};
    let mut buf = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match buf.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
                if !trimmed.is_empty() {
                    let _ = tx.send(Event::Log(trimmed));
                }
            }
            Err(_) => break,
        }
    }
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
