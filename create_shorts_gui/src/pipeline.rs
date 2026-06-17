//! Worker-thread pipeline: validate inputs → invoke yt-dlp to extract
//! the requested segment → upload to YouTube. Sends `Event`s back to
//! the UI over a crossbeam channel.

use crate::oauth;
use crate::settings::Settings;
use crate::youtube::{upload_video, VideoBody, VideoSnippet, VideoStatus};
use crossbeam_channel::Sender;
use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

type LineBuf = Arc<Mutex<VecDeque<String>>>;

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
    pub overlay_title: bool,
    pub overlay_color: [u8; 3],
    /// Normalized title position [x, y] in 0.0..=1.0 (0,0 = top-left,
    /// 1,1 = bottom-right). [0,1] = bottom-left (the original placement).
    pub overlay_pos: [f32; 2],
    pub stretch: bool,
    pub stretch_secs: u32,
    pub fade_out: bool,
    pub fade_secs: u32,
    pub fade_in: bool,
    pub fade_in_secs: u32,
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

    // Guard against the yt-dlp merge desync that leaves the segment's audio
    // shorter than its video (sound stops early; the fade-out plays silent).
    // Re-fetches and remuxes aligned audio in place when a shortfall is found,
    // then purges the stale derived caches. No-op for healthy segments.
    repair_segment_audio(&out, &url, &job.start, &job.end, &cache_dir, &video_id, &stamp, &settings, &tx);

    // Optional: time-stretch (slow down) the core clip so it lasts
    // `stretch_secs` seconds longer. Applied to the raw segment *before*
    // the title overlay and fades, so those keep their own independent
    // durations — only the movie content is stretched. Cached as
    // `<video_id>_<stamp>_stretch<secs>.mp4`.
    let stretch_secs = job.stretch_secs.max(1);
    let base = if job.stretch {
        let stretch_path = cache_dir.join(format!("{}_{}_stretch{}.mp4", video_id, stamp, stretch_secs));
        if stretch_path.exists() {
            let _ = tx.send(Event::Log(format!(
                "Reusing cached stretched segment {}",
                stretch_path.display()
            )));
        } else {
            let _ = tx.send(Event::Log(format!("Stretching clip by {}s…", stretch_secs)));
            if let Err(e) = apply_stretch(&out, &stretch_path, &tx, segment_secs, stretch_secs) {
                let _ = std::fs::remove_file(&stretch_path);
                let _ = tx.send(Event::Error(format!("stretch: {}", e)));
                return;
            }
        }
        stretch_path
    } else {
        out.clone()
    };

    // The core clip's duration after the optional stretch — used to drive
    // the progress bars of the downstream (title/fade) re-encodes.
    let core_secs = if job.stretch { segment_secs + stretch_secs as f64 } else { segment_secs };

    // Tag the titled cache filename with the stretch so toggling stretch
    // on/off doesn't serve a stale titled clip.
    let stretch_tag = if job.stretch { format!("_stretch{}", stretch_secs) } else { String::new() };

    // Optional: burn the title into the bottom-left of the frame for
    // 3 seconds starting 1 second in. Output is cached separately per
    // (video_id, segment, title) so Preview → Upload of the same title
    // doesn't re-encode.
    let final_out = if job.overlay_title {
        let overlay_path = cache_dir.join(format!(
            "{}_{}{}_titled_v3_{}_{:02x}{:02x}{:02x}_p{:02}{:02}.mp4",
            video_id,
            stamp,
            stretch_tag,
            short_title_hash(&job.title),
            job.overlay_color[0],
            job.overlay_color[1],
            job.overlay_color[2],
            (job.overlay_pos[0].clamp(0.0, 1.0) * 99.0).round() as u32,
            (job.overlay_pos[1].clamp(0.0, 1.0) * 99.0).round() as u32,
        ));
        if overlay_path.exists() {
            let _ = tx.send(Event::Log(format!(
                "Reusing cached titled segment {}",
                overlay_path.display()
            )));
        } else {
            let _ = tx.send(Event::Log("Burning title overlay…".into()));
            if let Err(e) = apply_title_overlay(&base, &job.title, job.overlay_color, job.overlay_pos, &overlay_path, &tx, core_secs) {
                let _ = std::fs::remove_file(&overlay_path);
                let _ = tx.send(Event::Error(format!("title overlay: {}", e)));
                return;
            }
        }
        overlay_path
    } else {
        base.clone()
    };

    // Optional (when enabled): freeze the FIRST frame for `fade_in_secs`
    // seconds and fade it — picture and sound — in from black/silence, so the
    // short opens on a held first frame that fades up and then starts playing.
    // Applied before the fade-out so the two wrap the movie symmetrically.
    // Cached next to its input as `<stem>_fadein<secs>.mp4`.
    let fade_in_secs = job.fade_in_secs.max(1);
    let after_fade_in = if job.fade_in {
        let fin_path = fade_in_output_path(&final_out, fade_in_secs);
        if fin_path.exists() {
            let _ = tx.send(Event::Log(format!(
                "Reusing cached fade-in segment {}",
                fin_path.display()
            )));
        } else {
            let _ = tx.send(Event::Log(format!(
                "Adding {}s freeze-frame fade-in…",
                fade_in_secs
            )));
            if let Err(e) = apply_fade_in(&final_out, &fin_path, &tx, core_secs, fade_in_secs) {
                let _ = std::fs::remove_file(&fin_path);
                let _ = tx.send(Event::Error(format!("fade-in: {}", e)));
                return;
            }
        }
        fin_path
    } else {
        final_out.clone()
    };

    // Always (when enabled): hold the last frame for `fade_secs` seconds and
    // fade it to black (prolong), with the source's audio continuing under the
    // held frame and fading out with it. Applied last so it wraps whatever we
    // deliver (raw segment, titled, or faded-in segment). Cached next to its
    // input as `<stem>_fadeout<secs>.mp4` so Preview → Upload of the same short
    // (and same duration) doesn't re-encode.
    let fade_secs = job.fade_secs.max(1);
    let delivered = if job.fade_out {
        let fade_path = fade_output_path(&after_fade_in, fade_secs);
        if fade_path.exists() {
            let _ = tx.send(Event::Log(format!(
                "Reusing cached fade-out segment {}",
                fade_path.display()
            )));
        } else {
            // Fetch the few seconds of source audio that follow the cut so the
            // held last frame fades out over *real* continuing sound instead of
            // silence. Cached per (video_id, end, fade_secs); best-effort — if
            // there's no audio after the cut we fall back to a silent hold.
            let tail_audio: Option<PathBuf> = if let Some(end_secs) = parse_timestamp(&job.end) {
                let tail_path = cache_dir.join(format!(
                    "{}_tail_{}_{}.m4a",
                    video_id,
                    job.end.replace(':', "_"),
                    fade_secs
                ));
                if tail_path.exists() {
                    let _ = tx.send(Event::Log(format!(
                        "Reusing cached fade-out audio tail {}",
                        tail_path.display()
                    )));
                    Some(tail_path)
                } else {
                    let _ = tx.send(Event::Log(format!(
                        "Fetching {}s of audio after the cut for the fade-out…",
                        fade_secs
                    )));
                    match download_tail_audio(&url, end_secs, fade_secs, &tail_path, &settings, &tx) {
                        Ok(()) if tail_path.exists() => Some(tail_path),
                        Ok(()) => None,
                        Err(e) => {
                            let _ = std::fs::remove_file(&tail_path);
                            let _ = tx.send(Event::Log(format!(
                                "No continuing audio for fade-out ({}); the held frame fades out then holds silent.",
                                e
                            )));
                            None
                        }
                    }
                }
            } else {
                None
            };

            let _ = tx.send(Event::Log(format!(
                "Adding {}s freeze-frame fade-out…",
                fade_secs
            )));
            if let Err(e) = apply_fade_out(
                &after_fade_in,
                &fade_path,
                &tx,
                core_secs,
                fade_secs,
                tail_audio.as_deref(),
            ) {
                let _ = std::fs::remove_file(&fade_path);
                let _ = tx.send(Event::Error(format!("fade-out: {}", e)));
                return;
            }
        }
        fade_path
    } else {
        after_fade_in.clone()
    };

    if job.preview_only {
        let _ = tx.send(Event::Log(format!("Preview file kept at {}", delivered.display())));
        let _ = tx.send(Event::Preview(delivered));
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

    let upload_size = std::fs::metadata(&delivered).map(|m| m.len()).unwrap_or(size);
    let _ = tx.send(Event::Log(format!("Uploading {:.1} MB…", upload_size as f64 / 1024.0 / 1024.0)));
    let progress_tx = tx.clone();
    let result = upload_video(&access_token, &delivered, &body, |sent, total| {
        let f = if total == 0 { 0.0 } else { sent as f32 / total as f32 };
        let detail = format!("{:.1} / {:.1} MB", sent as f64 / 1_048_576.0, total as f64 / 1_048_576.0);
        let _ = progress_tx.send(Event::Progress { phase: "Uploading".into(), fraction: f, detail });
    });

    // Remove the delivered file plus any distinct intermediates
    // (fade-out, fade-in, titled overlay, raw segment) so the cache dir
    // doesn't accumulate. Removing the same path twice is a harmless
    // ignored error.
    for f in [&delivered, &after_fade_in, &final_out, &base, &out] {
        let _ = std::fs::remove_file(f);
    }

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
    let stderr_capture: LineBuf = Arc::new(Mutex::new(VecDeque::new()));
    let cap_for_thread = stderr_capture.clone();
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, segment_secs, None)));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, segment_secs, Some(cap_for_thread))));
    let status = child.wait().map_err(|e| format!("wait: {}", e))?;
    if let Some(h) = h_o { let _ = h.join(); }
    if let Some(h) = h_e { let _ = h.join(); }
    if !status.success() {
        let detail = summarize_yt_dlp_failure(&stderr_capture);
        let suffix = if detail.is_empty() { String::new() } else { format!(" — {}", detail) };
        return Err(format!("exited with {:?}{}", status.code(), suffix));
    }
    Ok(())
}

/// Download just the audio of the `fade_secs` seconds that follow the cut
/// point (`end_secs`..`end_secs + fade_secs`) so the freeze-frame fade-out can
/// keep playing real source sound under the held frame. Audio-only, tiny.
/// Best-effort: returns an error (which the caller logs and tolerates) if
/// there's nothing after the cut.
fn download_tail_audio(
    url: &str,
    end_secs: f64,
    fade_secs: u32,
    out: &PathBuf,
    settings: &Settings,
    tx: &Sender<Event>,
) -> Result<(), String> {
    let stop = end_secs + fade_secs as f64;
    let mut cmd = Command::new("yt-dlp");
    cmd.args([
        "-f",
        "bestaudio/best",
        "--download-sections",
        &format!("*{:.3}-{:.3}", end_secs, stop),
        // Without forced keyframe cuts yt-dlp keeps everything from the start
        // of the stream up to `stop` instead of just the requested window.
        "--force-keyframes-at-cuts",
        "-x",
        "--audio-format",
        "m4a",
        "--newline",
        "-o",
        out.to_str().ok_or("non-utf8 path")?,
    ]);
    if !settings.cookies_browser.is_empty() {
        cmd.arg("--cookies-from-browser").arg(&settings.cookies_browser);
    }
    cmd.arg(url);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("yt-dlp not found ({})", e))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let stderr_capture: LineBuf = Arc::new(Mutex::new(VecDeque::new()));
    let cap_for_thread = stderr_capture.clone();
    let secs = fade_secs as f64;
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, secs, None)));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, secs, Some(cap_for_thread))));
    let status = child.wait().map_err(|e| format!("wait: {}", e))?;
    if let Some(h) = h_o { let _ = h.join(); }
    if let Some(h) = h_e { let _ = h.join(); }
    if !status.success() {
        let detail = summarize_yt_dlp_failure(&stderr_capture);
        let suffix = if detail.is_empty() { String::new() } else { format!(" — {}", detail) };
        return Err(format!("exited with {:?}{}", status.code(), suffix));
    }
    Ok(())
}

/// Download just the audio for the `start`..`end` window as a standalone m4a.
/// yt-dlp's `bestaudio` section download comes out correctly aligned to the
/// requested span (unlike the `bestvideo+bestaudio` *merge*, which can leave
/// the muxed audio truncated). Used by `repair_segment_audio`.
fn download_section_audio(
    url: &str,
    start: &str,
    end: &str,
    out: &PathBuf,
    settings: &Settings,
    tx: &Sender<Event>,
    secs: f64,
) -> Result<(), String> {
    let mut cmd = Command::new("yt-dlp");
    cmd.args([
        "-f",
        "bestaudio/best",
        "--download-sections",
        &format!("*{}-{}", start, end),
        "--force-keyframes-at-cuts",
        "-x",
        "--audio-format",
        "m4a",
        "--newline",
        "-o",
        out.to_str().ok_or("non-utf8 path")?,
    ]);
    if !settings.cookies_browser.is_empty() {
        cmd.arg("--cookies-from-browser").arg(&settings.cookies_browser);
    }
    cmd.arg(url);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("yt-dlp not found ({})", e))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let stderr_capture: LineBuf = Arc::new(Mutex::new(VecDeque::new()));
    let cap_for_thread = stderr_capture.clone();
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, secs, None)));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, secs, Some(cap_for_thread))));
    let status = child.wait().map_err(|e| format!("wait: {}", e))?;
    if let Some(h) = h_o { let _ = h.join(); }
    if let Some(h) = h_e { let _ = h.join(); }
    if !status.success() {
        let detail = summarize_yt_dlp_failure(&stderr_capture);
        let suffix = if detail.is_empty() { String::new() } else { format!(" — {}", detail) };
        return Err(format!("exited with {:?}{}", status.code(), suffix));
    }
    Ok(())
}

/// Repair the segment's audio when the yt-dlp `bestvideo+bestaudio` merge left
/// the muxed audio track shorter than the video (a `--download-sections`
/// keyframe-cut desync — observed e.g. 76.3s audio under an 84s video). Symptom
/// for the user: sound stops several seconds before the picture ends, and the
/// freeze-frame fade-out plays silent because the audio is already over. We
/// re-fetch the same span as a standalone `bestaudio` download (which *is*
/// aligned) and remux it in, replacing the broken track. Best-effort: any
/// failure leaves the original segment untouched. When a repair happens, the
/// derived caches (stretch/titled/fade) for this segment are purged so they
/// regenerate from the fixed audio.
fn repair_segment_audio(
    segment: &std::path::Path,
    url: &str,
    start: &str,
    end: &str,
    cache_dir: &std::path::Path,
    video_id: &str,
    stamp: &str,
    settings: &Settings,
    tx: &Sender<Event>,
) {
    let Some(vdur) = probe_duration(segment) else { return };
    // No audio stream at all → genuinely silent region, nothing to repair.
    if !probe_has_audio(segment) {
        return;
    }
    let adur = probe_audio_duration(segment).unwrap_or(vdur);
    // Only act on a meaningful shortfall (> 0.5s) — small AAC priming/packet
    // boundary differences are normal and harmless.
    if adur + 0.5 >= vdur {
        return;
    }
    let _ = tx.send(Event::Log(format!(
        "Segment audio is {:.1}s but video is {:.1}s (yt-dlp merge desync) — re-fetching aligned audio…",
        adur, vdur
    )));

    let body_audio = cache_dir.join(format!("{}_bodyaudio_{}.m4a", video_id, stamp));
    let _ = std::fs::remove_file(&body_audio);
    if let Err(e) = download_section_audio(url, start, end, &body_audio, settings, tx, vdur) {
        let _ = tx.send(Event::Log(format!(
            "Could not re-fetch aligned audio ({}); keeping original segment.",
            e
        )));
        let _ = std::fs::remove_file(&body_audio);
        return;
    }
    if !body_audio.exists() {
        return;
    }

    // Remux: keep the video as-is, swap in the freshly-fetched aligned audio,
    // trimming whichever is longer so the two stay in lock-step.
    let fixed = cache_dir.join(format!("{}_{}_fixed.mp4", video_id, stamp));
    let status = Command::new("ffmpeg")
        .arg("-y")
        .arg("-i").arg(segment)
        .arg("-i").arg(&body_audio)
        .args(["-map", "0:v:0", "-map", "1:a:0", "-c", "copy", "-shortest", "-movflags", "+faststart"])
        .arg(&fixed)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = std::fs::remove_file(&body_audio);
    match status {
        Ok(s) if s.success() && fixed.exists() => {
            if std::fs::rename(&fixed, segment).is_err() {
                // Cross-device or other rename failure — copy then drop temp.
                if std::fs::copy(&fixed, segment).is_ok() {
                    let _ = std::fs::remove_file(&fixed);
                } else {
                    let _ = std::fs::remove_file(&fixed);
                    let _ = tx.send(Event::Log("Audio remux produced a file but could not replace the segment; keeping original.".into()));
                    return;
                }
            }
            purge_derived_caches(cache_dir, video_id, stamp);
            let na = probe_audio_duration(segment).unwrap_or(0.0);
            let _ = tx.send(Event::Log(format!(
                "Audio realigned: now {:.1}s under {:.1}s of video.",
                na, vdur
            )));
        }
        _ => {
            let _ = std::fs::remove_file(&fixed);
            let _ = tx.send(Event::Log("Audio remux failed; keeping original segment.".into()));
        }
    }
}

/// Delete the stretch/titled/fade caches derived from a given raw segment so
/// they regenerate. Matches `<video_id>_<stamp>_*` (the raw segment itself is
/// `<video_id>_<stamp>.mp4` and the per-video tail/body audio use a different
/// infix, so both are left intact).
fn purge_derived_caches(cache_dir: &std::path::Path, video_id: &str, stamp: &str) {
    let prefix = format!("{}_{}_", video_id, stamp);
    if let Ok(entries) = std::fs::read_dir(cache_dir) {
        for e in entries.flatten() {
            if let Some(name) = e.file_name().to_str() {
                if name.starts_with(&prefix) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

/// Pick the most useful stderr lines to surface in the error banner.
/// Prefers explicit `ERROR:` / `WARNING:` / `HTTP Error` lines so the
/// user immediately sees *why* yt-dlp gave up; falls back to the last
/// few non-progress lines if nothing matched.
fn summarize_yt_dlp_failure(buf: &LineBuf) -> String {
    let q = buf.lock().unwrap();
    let errors: Vec<String> = q
        .iter()
        .filter(|s| {
            let t = s.trim_start();
            t.starts_with("ERROR:")
                || t.starts_with("WARNING:")
                || t.contains("HTTP Error")
                || t.contains("Sign in")
        })
        .cloned()
        .collect();
    let lines: Vec<String> = if !errors.is_empty() {
        errors
    } else {
        q.iter().rev().take(3).rev().cloned().collect()
    };
    lines
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Reads bytes from yt-dlp/ffmpeg and splits on either `\r` or `\n` so
/// in-place progress lines (which use carriage-return only) are
/// captured. yt-dlp `[download] X.X%` and ffmpeg `time=HH:MM:SS` lines
/// are intercepted as Progress events; everything else goes to Log.
fn stream_progress<R: Read>(reader: R, tx: Sender<Event>, segment_secs: f64, capture: Option<LineBuf>) {
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
                        emit_line(&line, &tx, segment_secs, capture.as_ref());
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
        emit_line(&line, &tx, segment_secs, capture.as_ref());
    }
}

fn emit_line(line: &str, tx: &Sender<Event>, segment_secs: f64, capture: Option<&LineBuf>) {
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
    if let Some(cap) = capture {
        if let Ok(mut q) = cap.lock() {
            if q.len() >= 16 {
                q.pop_front();
            }
            q.push_back(line.to_string());
        }
    }
    let _ = tx.send(Event::Log(line.to_string()));
}

/// Short stable hash of the title — used as part of the overlay cache
/// filename so two different titles for the same segment produce
/// distinct cached files. std::hash::DefaultHasher is fine here; we
/// don't need cryptographic strength.
fn short_title_hash(title: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    title.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Probe a single video stream's pixel dimensions via ffprobe. Used to
/// size the rendered title PNG to the exact video resolution so the
/// overlay composite is pixel-perfect (no scaling artifacts).
fn probe_video_size(input: &std::path::Path) -> Result<(u32, u32), String> {
    let output = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-select_streams", "v:0",
            "-show_entries", "stream=width,height",
            "-of", "csv=p=0",
            input.to_str().ok_or("non-utf8 input path")?,
        ])
        .output()
        .map_err(|e| format!("ffprobe spawn ({}): install ffmpeg/ffprobe", e))?;
    if !output.status.success() {
        return Err(format!(
            "ffprobe exit {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim(),
        ));
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let trimmed = line.trim();
    let parts: Vec<&str> = trimmed.split(',').collect();
    if parts.len() < 2 {
        return Err(format!("ffprobe unexpected output: {:?}", trimmed));
    }
    let w: u32 = parts[0]
        .parse()
        .map_err(|e| format!("width parse '{}': {}", parts[0], e))?;
    let h: u32 = parts[1]
        .parse()
        .map_err(|e| format!("height parse '{}': {}", parts[1], e))?;
    Ok((w, h))
}

/// Render the title as a transparent RGBA PNG sized to the video
/// frame, with the text painted in the bottom-left corner inside a
/// translucent black box. Using a frame-sized PNG keeps the ffmpeg
/// overlay invocation trivial (overlay defaults to 0:0). The font is
/// bundled into the binary so we don't depend on any system fonts.
fn render_title_png(
    title: &str,
    fill_color: [u8; 3],
    pos: [f32; 2],
    video_w: u32,
    video_h: u32,
    output: &std::path::Path,
) -> Result<(), String> {
    use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
    use image::{ImageBuffer, Rgba};

    static FONT_BYTES: &[u8] = include_bytes!("../assets/DejaVuSans.ttf");
    let font = FontRef::try_from_slice(FONT_BYTES)
        .map_err(|e| format!("bundled font load: {}", e))?;

    let font_size = (video_h as f32 / 16.92).max(23.4);
    let scale = PxScale::from(font_size);
    let scaled = font.as_scaled(scale);
    let ascent = scaled.ascent();
    let _ = scaled.height(); // line height unused; bottom-left placement uses ascent/descent

    // Measure full title width.
    let measure = |s: &str| -> f32 {
        let mut x = 0.0;
        let mut prev: Option<ab_glyph::GlyphId> = None;
        for c in s.chars() {
            let g = scaled.scaled_glyph(c);
            if let Some(p) = prev {
                x += scaled.kern(p, g.id);
            }
            x += scaled.h_advance(g.id);
            prev = Some(g.id);
        }
        x
    };

    // If the title would overflow ~85% of the video width, truncate it
    // with an ellipsis. Simple char-by-char shrink; titles are short so
    // we don't need anything smarter.
    let max_text_w = video_w as f32 * 0.85;
    let mut display = title.to_string();
    if measure(&display) > max_text_w {
        let ellipsis = "…";
        let mut chars: Vec<char> = display.chars().collect();
        while chars.len() > 1 {
            chars.pop();
            let candidate: String = chars.iter().collect::<String>() + ellipsis;
            if measure(&candidate) <= max_text_w {
                display = candidate;
                break;
            }
        }
    }

    let descent = scaled.descent();
    let text_height = ascent - descent;
    let text_width = measure(&display);

    // Place the text block according to the normalized position. The handle's
    // relative position in the picker maps to the text block's relative
    // position in the frame: x=0 → left margin, x=1 → flush right; y=0 → top,
    // y=1 → bottom. The available travel is the frame minus the text size and
    // a margin on each side, so the text never clips off-screen.
    let margin = (video_h as f32 / 40.0).round();
    let nx = pos[0].clamp(0.0, 1.0);
    let ny = pos[1].clamp(0.0, 1.0);
    let avail_x = (video_w as f32 - text_width - 2.0 * margin).max(0.0);
    let avail_y = (video_h as f32 - text_height - 2.0 * margin).max(0.0);
    let text_origin_x = margin + nx * avail_x;
    let text_top = margin + ny * avail_y;
    // ab_glyph positions glyphs by baseline; baseline = top + ascent.
    let text_origin_y = text_top + ascent;

    // Outline radius scales with font size — thin halo at 720p, more
    // substantial at 4K. Keeps the glyph readable over any video frame
    // without making the text look stroked.
    let outline_radius = ((font_size / 24.0).round() as i32).max(2);
    let mut offsets: Vec<(i32, i32)> = Vec::new();
    let r2 = outline_radius * outline_radius;
    for dy in -outline_radius..=outline_radius {
        for dx in -outline_radius..=outline_radius {
            if dx == 0 && dy == 0 { continue; }
            if dx * dx + dy * dy <= r2 {
                offsets.push((dx, dy));
            }
        }
    }

    // Transparent frame-sized canvas. Nothing else is painted — the
    // final composite shows only text + outline over the live video.
    let mut img: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_pixel(video_w, video_h, Rgba([0, 0, 0, 0]));

    // Source-over alpha composite onto the canvas. The canvas starts
    // fully transparent; we paint black outline pixels first then the
    // white fill on top so the outline always wraps the glyph cleanly.
    let blend = |img: &mut ImageBuffer<Rgba<u8>, Vec<u8>>, px: i32, py: i32, rgb: [u8; 3], coverage: f32| {
        if px < 0 || py < 0 || (px as u32) >= video_w || (py as u32) >= video_h {
            return;
        }
        let a_src = (coverage * 255.0).clamp(0.0, 255.0) as u16;
        if a_src == 0 { return; }
        let pix = img.get_pixel_mut(px as u32, py as u32);
        let inv = 255 - a_src;
        let bg_r = pix[0] as u16;
        let bg_g = pix[1] as u16;
        let bg_b = pix[2] as u16;
        let bg_a = pix[3] as u16;
        let new_r = (rgb[0] as u16 * a_src + bg_r * inv) / 255;
        let new_g = (rgb[1] as u16 * a_src + bg_g * inv) / 255;
        let new_b = (rgb[2] as u16 * a_src + bg_b * inv) / 255;
        let new_a = bg_a + ((255 - bg_a) * a_src) / 255;
        *pix = Rgba([new_r as u8, new_g as u8, new_b as u8, new_a as u8]);
    };

    // Lay out once; render outline pass + fill pass per glyph so we
    // don't have to clone OutlinedGlyph or walk the layout twice.
    let mut x = text_origin_x;
    let mut prev: Option<ab_glyph::GlyphId> = None;
    for c in display.chars() {
        let mut g = scaled.scaled_glyph(c);
        if let Some(p) = prev {
            x += scaled.kern(p, g.id);
        }
        prev = Some(g.id);
        g.position = ab_glyph::point(x, text_origin_y);
        let advance = scaled.h_advance(g.id);
        if let Some(outlined) = font.outline_glyph(g) {
            let bounds = outlined.px_bounds();
            let bx = bounds.min.x as i32;
            let by = bounds.min.y as i32;

            // Outline: rasterize the glyph N times at small offsets in
            // black so the visible halo wraps the eventual white fill.
            for (ox, oy) in &offsets {
                outlined.draw(|gx, gy, coverage| {
                    blend(&mut img, bx + gx as i32 + ox, by + gy as i32 + oy, [0, 0, 0], coverage);
                });
            }
            // Fill: chosen text color on top of the black halo.
            outlined.draw(|gx, gy, coverage| {
                blend(&mut img, bx + gx as i32, by + gy as i32, fill_color, coverage);
            });
        }
        x += advance;
    }

    img.save(output)
        .map_err(|e| format!("write {}: {}", output.display(), e))?;
    Ok(())
}

/// Re-encode `input` with a title overlay drawn in the bottom-left of
/// every frame for 3 seconds, starting 1 second in. Implementation:
/// render the title in Rust to a frame-sized transparent PNG, then
/// composite it via ffmpeg's `overlay` filter (which is always present,
/// unlike `drawtext` which needs ffmpeg built with libfreetype).
fn apply_title_overlay(
    input: &std::path::Path,
    title: &str,
    color: [u8; 3],
    pos: [f32; 2],
    output: &std::path::Path,
    tx: &Sender<Event>,
    segment_secs: f64,
) -> Result<(), String> {
    let (video_w, video_h) = probe_video_size(input)?;
    let _ = tx.send(Event::Log(format!(
        "Rendering title overlay ({}×{}) color #{:02x}{:02x}{:02x} at x{:.0}%/y{:.0}% for {:?}",
        video_w, video_h, color[0], color[1], color[2], pos[0] * 100.0, pos[1] * 100.0, title
    )));

    let png_path = std::env::temp_dir()
        .join(format!("create_shorts_title_{}.png", std::process::id()));
    render_title_png(title, color, pos, video_w, video_h, &png_path)?;

    let _ = tx.send(Event::Progress {
        phase: "Burning title overlay".into(),
        fraction: 0.0,
        detail: String::new(),
    });

    let filter = "[0:v][1:v]overlay=0:0:enable='between(t,1,5)'";
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-y",
        "-i",
        input.to_str().ok_or("non-utf8 input path")?,
        "-i",
        png_path.to_str().ok_or("non-utf8 png path")?,
        "-filter_complex",
        filter,
        "-c:a",
        "copy",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-crf",
        "20",
        "-movflags",
        "+faststart",
        output.to_str().ok_or("non-utf8 output path")?,
    ]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg spawn ({}): install ffmpeg", e))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let stderr_capture: LineBuf = Arc::new(Mutex::new(VecDeque::new()));
    let cap_for_thread = stderr_capture.clone();
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, segment_secs, None)));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, segment_secs, Some(cap_for_thread))));
    let status = child.wait().map_err(|e| format!("wait: {}", e))?;
    if let Some(h) = h_o { let _ = h.join(); }
    if let Some(h) = h_e { let _ = h.join(); }
    let _ = std::fs::remove_file(&png_path);
    if !status.success() {
        let detail = summarize_yt_dlp_failure(&stderr_capture);
        let suffix = if detail.is_empty() { String::new() } else { format!(" — {}", detail) };
        return Err(format!("ffmpeg overlay exited with {:?}{}", status.code(), suffix));
    }
    Ok(())
}

/// Cache path for the faded variant of a segment: same directory and
/// stem as the input, with a `_fadeout<secs>` suffix. Keying on the input
/// filename means a titled segment and a plain segment get distinct
/// faded caches; keying on the duration means changing the fade length
/// re-encodes rather than serving a stale cache. Preview → Upload of the
/// same short and same duration reuses the file.
fn fade_output_path(input: &std::path::Path, fade_secs: u32) -> PathBuf {
    let parent = input.parent().unwrap_or_else(|| std::path::Path::new("."));
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("segment");
    parent.join(format!("{}_fadeout{}.mp4", stem, fade_secs))
}

/// Cache path for the faded-in variant of a segment: same directory and
/// stem as the input, with a `_fadein<secs>` suffix. Distinct from the
/// fade-out suffix so a clip can be faded in, faded out, or both and each
/// stage gets its own cache.
fn fade_in_output_path(input: &std::path::Path, fade_secs: u32) -> PathBuf {
    let parent = input.parent().unwrap_or_else(|| std::path::Path::new("."));
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("segment");
    parent.join(format!("{}_fadein{}.mp4", stem, fade_secs))
}

/// Probe the container duration in seconds.
fn probe_duration(input: &std::path::Path) -> Option<f64> {
    let output = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-show_entries", "format=duration",
            "-of", "csv=p=0",
            input.to_str()?,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Duration of the first audio stream in seconds, if there is one. Used to
/// detect the yt-dlp `--download-sections` merge desync where the muxed audio
/// track comes out shorter than the video track.
fn probe_audio_duration(input: &std::path::Path) -> Option<f64> {
    let output = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-select_streams", "a:0",
            "-show_entries", "stream=duration",
            "-of", "csv=p=0",
            input.to_str()?,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// True if the input has at least one audio stream. Determines whether
/// we pad+fade the audio tail or just drop audio for the freeze frame.
fn probe_has_audio(input: &std::path::Path) -> bool {
    Command::new("ffprobe")
        .args([
            "-v", "error",
            "-select_streams", "a:0",
            "-show_entries", "stream=codec_type",
            "-of", "csv=p=0",
            input.to_str().unwrap_or(""),
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("audio"))
        .unwrap_or(false)
}

/// Hold the last frame of `input` for `fade_secs` seconds and fade that held
/// frame to black ("prolong"), so the short ends up N seconds longer. The
/// **sound keeps running underneath the held frame and fades out with it**:
/// `tail_audio` (a clip of the source's audio from the cut point onward,
/// fetched by the caller) is concatenated after the input's own audio and the
/// whole thing fades out over the held window. If no continuing audio is
/// available (cut at the end of the source, or the input has no audio) we fall
/// back to fading the input's own last N s of sound and holding silence.
fn apply_fade_out(
    input: &std::path::Path,
    output: &std::path::Path,
    tx: &Sender<Event>,
    segment_secs: f64,
    fade_secs: u32,
    tail_audio: Option<&std::path::Path>,
) -> Result<(), String> {
    let fade: f64 = fade_secs.max(1) as f64;
    let dur = probe_duration(input).unwrap_or(segment_secs);
    let freeze_start = if dur > 0.0 { dur } else { segment_secs.max(0.0) };
    let has_audio = probe_has_audio(input);
    // Use the continuing audio only if the input has sound to follow it and
    // the tail actually exists with an audio stream.
    let use_tail = has_audio
        && tail_audio
            .map(|p| p.exists() && probe_has_audio(p))
            .unwrap_or(false);
    let _ = tx.send(Event::Log(format!(
        "Freeze-frame fade-out: holding last frame {:.1}s–{:.1}s ({})",
        freeze_start,
        freeze_start + fade,
        if use_tail {
            "sound keeps playing from the source and fades out"
        } else if has_audio {
            "no continuing source audio — sound fades out then holds silent"
        } else {
            "no audio track"
        },
    )));

    // Video: clone the last frame for `fade` seconds, then fade that held tail
    // to black over the same window.
    let vf = format!(
        "tpad=stop_mode=clone:stop_duration={fade},fade=t=out:st={start:.3}:d={fade}",
        fade = fade,
        start = freeze_start,
    );

    let _ = tx.send(Event::Progress {
        phase: "Adding fade-out".into(),
        fraction: 0.0,
        detail: String::new(),
    });

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y")
        .arg("-i")
        .arg(input.to_str().ok_or("non-utf8 input path")?);
    if use_tail {
        cmd.arg("-i")
            .arg(tail_audio.unwrap().to_str().ok_or("non-utf8 tail path")?);
        // Normalize both audio streams to a common format so concat accepts
        // them, glue the continuing source audio after the input's own audio,
        // then fade the whole thing out over the held window.
        let fc = format!(
            "[0:v]{vf}[v];\
             [0:a]aresample=async=1,aformat=sample_rates=48000:channel_layouts=stereo[a0];\
             [1:a]aresample=async=1,aformat=sample_rates=48000:channel_layouts=stereo[a1];\
             [a0][a1]concat=n=2:v=0:a=1[acat];\
             [acat]afade=t=out:st={start:.3}:d={fade}[a]",
            vf = vf,
            start = freeze_start,
            fade = fade,
        );
        cmd.arg("-filter_complex").arg(&fc).args(["-map", "[v]", "-map", "[a]"]);
    } else if has_audio {
        // No continuation: fade the input's own last N s of sound, then pad
        // silence to fill the held frame.
        let afade_start = (freeze_start - fade).max(0.0);
        let af = format!(
            "afade=t=out:st={s:.3}:d={fade},apad=pad_dur={fade}",
            s = afade_start,
            fade = fade,
        );
        cmd.arg("-vf").arg(&vf).arg("-af").arg(&af);
    } else {
        cmd.arg("-vf").arg(&vf).arg("-an");
    }
    cmd.args([
        "-c:v", "libx264",
        "-preset", "veryfast",
        "-crf", "20",
    ]);
    if has_audio {
        cmd.args(["-c:a", "aac"]);
    }
    cmd.args(["-movflags", "+faststart"]);
    cmd.arg(output.to_str().ok_or("non-utf8 output path")?);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    // Progress is measured against the held total length so the bar reaches
    // 100% at the real end of the encode.
    let total = freeze_start + fade;
    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg spawn ({}): install ffmpeg", e))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let stderr_capture: LineBuf = Arc::new(Mutex::new(VecDeque::new()));
    let cap_for_thread = stderr_capture.clone();
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, total, None)));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, total, Some(cap_for_thread))));
    let status = child.wait().map_err(|e| format!("wait: {}", e))?;
    if let Some(h) = h_o { let _ = h.join(); }
    if let Some(h) = h_e { let _ = h.join(); }
    if !status.success() {
        let detail = summarize_yt_dlp_failure(&stderr_capture);
        let suffix = if detail.is_empty() { String::new() } else { format!(" — {}", detail) };
        return Err(format!("ffmpeg fade-out exited with {:?}{}", status.code(), suffix));
    }
    Ok(())
}

/// Prepend a `fade_secs`-second freeze-frame fade-in to the start of `input`:
/// `tpad=start_mode=clone:start_duration=N` clones the first frame for N
/// seconds, then `fade=t=in:st=0:d=N` ramps that opening from black up to the
/// picture, after which the movie plays from that same first frame. Audio
/// (when present) is delayed by N s of silence and faded in over the movie's
/// first N seconds. The result is N seconds longer than the input.
fn apply_fade_in(
    input: &std::path::Path,
    output: &std::path::Path,
    tx: &Sender<Event>,
    segment_secs: f64,
    fade_secs: u32,
) -> Result<(), String> {
    let fade: f64 = fade_secs.max(1) as f64;
    let dur = probe_duration(input).unwrap_or(segment_secs);
    let has_audio = probe_has_audio(input);
    let _ = tx.send(Event::Log(format!(
        "Freeze-frame fade-in: holding first frame 0.0s–{:.1}s (audio: {})",
        fade,
        if has_audio { "yes" } else { "none" },
    )));

    let vf = format!(
        "tpad=start_mode=clone:start_duration={fade},fade=t=in:st=0:d={fade}",
        fade = fade,
    );
    // Delay the movie's audio by `fade` s (silence under the held first frame),
    // then fade it in over the first `fade` s once playback starts.
    let delay_ms = (fade * 1000.0).round() as u64;
    let af = format!(
        "adelay={ms}:all=1,afade=t=in:st={fade:.3}:d={fade}",
        ms = delay_ms,
        fade = fade,
    );

    let _ = tx.send(Event::Progress {
        phase: "Adding fade-in".into(),
        fraction: 0.0,
        detail: String::new(),
    });

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y")
        .arg("-i")
        .arg(input.to_str().ok_or("non-utf8 input path")?)
        .arg("-vf")
        .arg(&vf);
    if has_audio {
        cmd.arg("-af").arg(&af);
    } else {
        cmd.arg("-an");
    }
    cmd.args([
        "-c:v", "libx264",
        "-preset", "veryfast",
        "-crf", "20",
    ]);
    if has_audio {
        cmd.args(["-c:a", "aac"]);
    }
    cmd.args(["-movflags", "+faststart"]);
    cmd.arg(output.to_str().ok_or("non-utf8 output path")?);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    // Progress against the padded total length so the bar reaches 100% at the
    // real end of the encode.
    let total = if dur > 0.0 { dur + fade } else { segment_secs.max(0.0) + fade };
    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg spawn ({}): install ffmpeg", e))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let stderr_capture: LineBuf = Arc::new(Mutex::new(VecDeque::new()));
    let cap_for_thread = stderr_capture.clone();
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, total, None)));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, total, Some(cap_for_thread))));
    let status = child.wait().map_err(|e| format!("wait: {}", e))?;
    if let Some(h) = h_o { let _ = h.join(); }
    if let Some(h) = h_e { let _ = h.join(); }
    if !status.success() {
        let detail = summarize_yt_dlp_failure(&stderr_capture);
        let suffix = if detail.is_empty() { String::new() } else { format!(" — {}", detail) };
        return Err(format!("ffmpeg fade-in exited with {:?}{}", status.code(), suffix));
    }
    Ok(())
}

/// Build an `atempo` filter chain that slows audio to `ratio` (<1.0).
/// A single `atempo` instance only accepts 0.5–2.0, so to reach a ratio
/// below 0.5 we chain `atempo=0.5` stages until what's left is in range.
/// e.g. ratio 0.2 → `atempo=0.5,atempo=0.4` (0.5 × 0.4 = 0.2).
fn atempo_chain(mut ratio: f64) -> String {
    let mut parts: Vec<String> = Vec::new();
    while ratio < 0.5 {
        parts.push("atempo=0.5".to_string());
        ratio *= 2.0;
    }
    parts.push(format!("atempo={:.6}", ratio));
    parts.join(",")
}

/// Time-stretch `input` so it lasts `stretch_secs` seconds longer: slow
/// the video by multiplying the presentation timestamps (`setpts`) and
/// slow the audio with an `atempo` chain by the same factor, so picture
/// and sound stay in sync — only playback speed changes. No frames are
/// dropped or added; the existing frames just play back slower.
fn apply_stretch(
    input: &std::path::Path,
    output: &std::path::Path,
    tx: &Sender<Event>,
    segment_secs: f64,
    stretch_secs: u32,
) -> Result<(), String> {
    let extra = stretch_secs.max(1) as f64;
    let dur = probe_duration(input).unwrap_or(segment_secs).max(0.001);
    let target = dur + extra;
    let pts_factor = target / dur; // > 1 → slower video
    let atempo = dur / target; // < 1 → slower audio
    let has_audio = probe_has_audio(input);
    let _ = tx.send(Event::Log(format!(
        "Stretching {:.1}s → {:.1}s (×{:.3} slower, audio: {})",
        dur,
        target,
        pts_factor,
        if has_audio { "yes" } else { "none" },
    )));

    let vf = format!("setpts={:.6}*PTS", pts_factor);
    let af = atempo_chain(atempo);

    let _ = tx.send(Event::Progress {
        phase: "Stretching clip".into(),
        fraction: 0.0,
        detail: String::new(),
    });

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y")
        .arg("-i")
        .arg(input.to_str().ok_or("non-utf8 input path")?)
        .arg("-vf")
        .arg(&vf);
    if has_audio {
        cmd.arg("-af").arg(&af);
    } else {
        cmd.arg("-an");
    }
    cmd.args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20"]);
    if has_audio {
        cmd.args(["-c:a", "aac"]);
    }
    cmd.args(["-movflags", "+faststart"]);
    cmd.arg(output.to_str().ok_or("non-utf8 output path")?);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    // Progress against the stretched (target) length so the bar reaches
    // 100% at the real end of the encode.
    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg spawn ({}): install ffmpeg", e))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let stderr_capture: LineBuf = Arc::new(Mutex::new(VecDeque::new()));
    let cap_for_thread = stderr_capture.clone();
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, target, None)));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, target, Some(cap_for_thread))));
    let status = child.wait().map_err(|e| format!("wait: {}", e))?;
    if let Some(h) = h_o { let _ = h.join(); }
    if let Some(h) = h_e { let _ = h.join(); }
    if !status.success() {
        let detail = summarize_yt_dlp_failure(&stderr_capture);
        let suffix = if detail.is_empty() { String::new() } else { format!(" — {}", detail) };
        return Err(format!("ffmpeg stretch exited with {:?}{}", status.code(), suffix));
    }
    Ok(())
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

