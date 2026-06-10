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
    pub fade_out: bool,
    pub fade_secs: u32,
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

    // Optional: burn the title into the bottom-left of the frame for
    // 3 seconds starting 1 second in. Output is cached separately per
    // (video_id, segment, title) so Preview → Upload of the same title
    // doesn't re-encode.
    let final_out = if job.overlay_title {
        let overlay_path = cache_dir.join(format!(
            "{}_{}_titled_v2_{}_{:02x}{:02x}{:02x}.mp4",
            video_id,
            stamp,
            short_title_hash(&job.title),
            job.overlay_color[0],
            job.overlay_color[1],
            job.overlay_color[2],
        ));
        if overlay_path.exists() {
            let _ = tx.send(Event::Log(format!(
                "Reusing cached titled segment {}",
                overlay_path.display()
            )));
        } else {
            let _ = tx.send(Event::Log("Burning title overlay…".into()));
            if let Err(e) = apply_title_overlay(&out, &job.title, job.overlay_color, &overlay_path, &tx, segment_secs) {
                let _ = std::fs::remove_file(&overlay_path);
                let _ = tx.send(Event::Error(format!("title overlay: {}", e)));
                return;
            }
        }
        overlay_path
    } else {
        out.clone()
    };

    // Always (when enabled): freeze the last frame for `fade_secs` seconds
    // and fade it — picture and sound — to black/silence. Applied last so it
    // wraps whatever we deliver (raw segment or titled segment). Cached
    // next to its input as `<stem>_fade<secs>.mp4` so Preview → Upload of the
    // same short (and same duration) doesn't re-encode.
    let fade_secs = job.fade_secs.max(1);
    let delivered = if job.fade_out {
        let fade_path = fade_output_path(&final_out, fade_secs);
        if fade_path.exists() {
            let _ = tx.send(Event::Log(format!(
                "Reusing cached fade-out segment {}",
                fade_path.display()
            )));
        } else {
            let _ = tx.send(Event::Log(format!(
                "Adding {}s freeze-frame fade-out…",
                fade_secs
            )));
            if let Err(e) = apply_fade_out(&final_out, &fade_path, &tx, segment_secs, fade_secs) {
                let _ = std::fs::remove_file(&fade_path);
                let _ = tx.send(Event::Error(format!("fade-out: {}", e)));
                return;
            }
        }
        fade_path
    } else {
        final_out.clone()
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
    // (titled overlay, raw segment) so the cache dir doesn't accumulate.
    // Removing the same path twice is a harmless ignored error.
    for f in [&delivered, &final_out, &out] {
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

    // Bottom-left text placement: margins from the edges, baseline
    // positioned so the visible text bottom sits at video_h - margin.
    let margin_left = (video_h as f32 / 40.0).round();
    let margin_bottom = (video_h as f32 / 30.0).round();
    let text_origin_x = margin_left;
    let text_origin_y = video_h as f32 - margin_bottom - text_height + ascent;

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
    output: &std::path::Path,
    tx: &Sender<Event>,
    segment_secs: f64,
) -> Result<(), String> {
    let (video_w, video_h) = probe_video_size(input)?;
    let _ = tx.send(Event::Log(format!(
        "Rendering title overlay ({}×{}) color #{:02x}{:02x}{:02x} for {:?}",
        video_w, video_h, color[0], color[1], color[2], title
    )));

    let png_path = std::env::temp_dir()
        .join(format!("create_shorts_title_{}.png", std::process::id()));
    render_title_png(title, color, video_w, video_h, &png_path)?;

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
/// stem as the input, with a `_fade<secs>` suffix. Keying on the input
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
    parent.join(format!("{}_fade{}.mp4", stem, fade_secs))
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

/// Append a `fade_secs`-second freeze-frame fade-out to the end of `input`:
/// `tpad=stop_mode=clone:stop_duration=N` clones the last frame for
/// N seconds, then `fade=t=out` ramps those N seconds to black. Audio
/// (when present) is padded with N s of silence and faded out in
/// lockstep. The result is N seconds longer than the input.
fn apply_fade_out(
    input: &std::path::Path,
    output: &std::path::Path,
    tx: &Sender<Event>,
    segment_secs: f64,
    fade_secs: u32,
) -> Result<(), String> {
    let fade: f64 = fade_secs.max(1) as f64;
    let dur = probe_duration(input).unwrap_or(segment_secs);
    let fade_start = if dur > 0.0 { dur } else { segment_secs.max(0.0) };
    let has_audio = probe_has_audio(input);
    let _ = tx.send(Event::Log(format!(
        "Freeze-frame fade-out: holding last frame {:.1}s–{:.1}s (audio: {})",
        fade_start,
        fade_start + fade,
        if has_audio { "yes" } else { "none" },
    )));

    let vf = format!(
        "tpad=stop_mode=clone:stop_duration={fade},fade=t=out:st={start:.3}:d={fade}",
        fade = fade,
        start = fade_start,
    );
    let af = format!(
        "apad=pad_dur={fade},afade=t=out:st={start:.3}:d={fade}",
        fade = fade,
        start = fade_start,
    );

    let _ = tx.send(Event::Progress {
        phase: "Adding fade-out".into(),
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

    // Progress is measured against the padded total length so the bar
    // reaches 100% at the real end of the encode.
    let total = fade_start + fade;
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

