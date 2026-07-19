//! Worker-thread pipeline: validate inputs → invoke yt-dlp to extract
//! the requested segment → upload to YouTube. Sends `Event`s back to
//! the UI over a crossbeam channel.

use crate::oauth;
use crate::settings::Settings;
use crate::youtube::{insert_blank_caption, upload_video, VideoBody, VideoSnippet, VideoStatus};
use crossbeam_channel::Sender;
use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

type LineBuf = Arc<Mutex<VecDeque<String>>>;

#[derive(Clone)]
pub enum Event {
    Log(String),
    Progress { phase: String, fraction: f32, detail: String },
    /// Upload finished. `url` is the new YouTube URL. `file` is a copy of the
    /// uploaded video still on disk — the Post-to-LinkedIn button reuses it
    /// instead of re-downloading what we just uploaded. `temp` marks it as
    /// *ours* to delete once the user is done with the success banner; the
    /// direct-upload path reports the user's own file with `temp: false`, so
    /// we never delete something we didn't create.
    Done { url: String, file: Option<PathBuf>, temp: bool },
    /// Preview finished. The UI opens a split page with the **full original
    /// video on top** and the edited clip below. `source_id` is the YouTube id
    /// of the source (embedded on top so Jürg sees the whole, untrimmed
    /// original, not just the downloaded start–end segment) and `start_secs`
    /// deep-links the embed near the segment start. `edited` is the final
    /// processed clip we'd upload (bottom, local file). `original` is the raw
    /// downloaded start–end segment, kept only as an offline fallback for the
    /// top pane when `source_id` can't be embedded.
    Preview { original: PathBuf, edited: PathBuf, source_id: String, start_secs: u32 },
    /// The start–end segment is downloaded and ready to watch in the window
    /// (for marking cut points). `start_secs` lets the viewer show *absolute*
    /// source time (segment offset + start) so the shown timestamps match what
    /// the user types into the cut fields. Kept fractional (e.g. 587.5) so a
    /// `9:47.5` start reads back exactly, not `9:47.0`.
    SourceReady { path: PathBuf, start_secs: f64 },
    Error(String),
    /// The user clicked Cancel; the worker stopped (and killed any running
    /// yt-dlp/ffmpeg child). Reported separately from `Error` so the UI can
    /// show a neutral "Cancelled" instead of a red failure.
    Cancelled,
}

/// Error string returned by `wait_or_cancel` / `upload_video` when the user
/// cancelled. Callers detect cancellation via the shared `cancel` flag (which
/// is what `report_fail` keys on), so this text is effectively internal.
pub const CANCEL_MSG: &str = "cancelled by user";

/// Block until `child` exits, polling the shared `cancel` flag ~10×/s. If the
/// flag flips true mid-run, kill the child (then reap it) and return the
/// cancel sentinel error so the caller can unwind cleanly. This is what makes
/// an in-flight yt-dlp download or ffmpeg encode actually stop the moment the
/// user clicks Cancel — `Child::wait` alone blocks until the process finishes.
pub(crate) fn wait_or_cancel(child: &mut Child, cancel: &AtomicBool) -> Result<ExitStatus, String> {
    loop {
        if cancel.load(Ordering::SeqCst) {
            kill_tree(child);
            let _ = child.wait();
            return Err(CANCEL_MSG.to_string());
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(e) => return Err(format!("wait: {}", e)),
        }
    }
}

/// Put a spawned child in its own process group so Cancel can take down the
/// whole tree. yt-dlp drives ffmpeg as a *grandchild* to download/cut the
/// requested section (`--download-sections --force-keyframes-at-cuts`); a bare
/// `child.kill()` only hits yt-dlp and would orphan that ffmpeg — it keeps
/// downloading after the user "cancelled". Making the child a group leader lets
/// `kill_tree` signal the group. No-op on non-Unix. Call before `.spawn()`.
pub(crate) fn detach_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

/// Kill `child` and, on Unix, its entire process group (the grandchildren
/// `detach_group` corralled). Signals the group *and* the direct child, so even
/// a spawn that wasn't group-detached still dies. Caller reaps with `wait()`.
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        // The child is its own group leader (pgid == pid via process_group(0)),
        // so a negative pid signals the whole group: yt-dlp + its ffmpeg.
        let pid = child.id() as i32;
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

/// Remove a cut segment plus the intermediate files yt-dlp leaves when killed
/// mid-download/merge (`<stem>.fNNN.mp4`, `<stem>.fNNN.m4a`, `<stem>.part`,
/// `<stem>.ytdl`, `<stem>.temp.mp4`, …). yt-dlp names these `<stem>.<…>`, so we
/// delete every sibling whose name starts with `"<stem>."`. The trailing dot
/// guards against nuking derived caches that share the prefix but not the dot
/// (e.g. `<stem>_stretch5.mp4`, `<stem>_titled_…`).
fn cleanup_partial_download(cached: &std::path::Path) {
    let _ = std::fs::remove_file(cached);
    let (Some(dir), Some(stem)) = (
        cached.parent(),
        cached.file_stem().and_then(|s| s.to_str()),
    ) else {
        return;
    };
    let prefix = format!("{}.", stem);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if let Some(name) = e.file_name().to_str() {
                if name.starts_with(&prefix) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

/// Emit a terminal failure to the UI: a clean `Cancelled` event when the user
/// asked to stop, otherwise the real error. Keying on the flag (not the error
/// text) means every pipeline stage reports cancellation uniformly even though
/// each wraps the sentinel with its own prefix ("yt-dlp: …", "stretch: …").
fn report_fail(tx: &Sender<Event>, cancel: &AtomicBool, msg: String) {
    if cancel.load(Ordering::SeqCst) {
        let _ = tx.send(Event::Cancelled);
    } else {
        let _ = tx.send(Event::Error(msg));
    }
}

pub struct Job {
    pub source: String,
    pub start: String,
    pub end: String,
    pub title: String,
    pub description: String,
    pub privacy: String,
    pub preview_only: bool,
    /// Just download the start–end segment and hand it back (via
    /// `Event::SourceReady`) for the in-window "watch the original" viewer —
    /// no cuts/title/fades, no preview, no upload. Skips even audio repair so
    /// it's fast.
    pub source_only: bool,
    pub overlay_title: bool,
    pub overlay_color: [u8; 3],
    /// Normalized title position [x, y] in 0.0..=1.0 (0,0 = top-left,
    /// 1,1 = bottom-right). [0,1] = bottom-left (the original placement).
    pub overlay_pos: [f32; 2],
    pub stretch: bool,
    pub stretch_secs: u32,
    pub fade_out: bool,
    pub fade_secs: u32,
    /// When `fade_out` is set: hold the last frame at full brightness for
    /// `fade_secs` instead of fading it to black. The audio still ducks out
    /// under the held frame either way — only the picture differs.
    pub fade_out_hold_bright: bool,
    /// Optional text burned onto the held last frame (the fade-out tail) — an
    /// "end card" Jürg fills in himself. Empty = no end card. Only meaningful
    /// when `fade_out` is set.
    pub end_text: String,
    pub end_text_color: [u8; 3],
    /// Normalized end-card text position [x, y] in 0.0..=1.0, same convention
    /// as `overlay_pos`. Default centred.
    pub end_text_pos: [f32; 2],
    pub fade_in: bool,
    pub fade_in_secs: u32,
    /// Remove one or more middle sections from the extracted segment. Each
    /// `(from, till)` is a timestamp pair in the same style as `start`/`end`
    /// (absolute in the source video); those parts are deleted and everything
    /// remaining is concatenated into one clip.
    pub cut_middle: bool,
    pub cuts: Vec<(String, String)>,
    /// Silence the sound. With an empty `mutes` list the whole clip is muted;
    /// otherwise each `(from, till)` names a section to silence (absolute
    /// timestamps like the cut-outs; an empty `from` means the clip start, an
    /// empty `till` the clip end). The picture is untouched.
    pub mute: bool,
    pub mutes: Vec<(String, String)>,
    /// Mix a background-music audio file (e.g. downloaded from Pixabay Music)
    /// under the finished clip: looped/trimmed to the clip's full length
    /// (freeze-frame fades included), faded out at the end, at
    /// `bg_music_volume` percent. Combined with a whole-clip `mute` it
    /// replaces the original sound entirely.
    pub bg_music: bool,
    pub bg_music_path: String,
    pub bg_music_volume: u32,
    /// Stop YouTube from putting its own auto-generated subtitles on the
    /// upload, by declaring the audio language and publishing a blank caption
    /// track in it (see [`block_auto_subtitles`]).
    pub block_auto_subs: bool,
    /// BCP-47 language spoken in the video (`de`, `en`, … or `zxx` for no
    /// speech). Only meaningful when `block_auto_subs` is set.
    pub audio_language: String,
}

pub struct UploadJob {
    pub file: PathBuf,
    pub title: String,
    pub description: String,
    pub privacy: String,
    pub block_auto_subs: bool,
    pub audio_language: String,
}

/// Publish the blank caption track that hides YouTube's auto-generated
/// subtitles, and log what happened.
///
/// Deliberately **never fatal**: by the time this runs the video is already on
/// YouTube, so a caption failure must not turn a successful upload into a red
/// error. It degrades to a warning line instead.
///
/// Right after `videos.insert` the video can still be registering, so a first
/// attempt may 404 — hence the backoff. A missing-scope 403 is not retried:
/// waiting won't grant the scope, only signing in again will.
fn block_auto_subtitles(tx: &Sender<Event>, token: &str, video_id: &str, language: &str) {
    let _ = tx.send(Event::Log(format!(
        "Blocking YouTube auto-subtitles: publishing a blank \"{}\" caption track…",
        language
    )));
    const DELAYS: [u64; 4] = [0, 5, 15, 30];
    for (i, delay) in DELAYS.iter().enumerate() {
        if *delay > 0 {
            std::thread::sleep(std::time::Duration::from_secs(*delay));
        }
        match insert_blank_caption(token, video_id, language) {
            Ok(id) => {
                let _ = tx.send(Event::Log(format!(
                    "✅ Auto-subtitles blocked — blank \"{}\" caption track published ({})",
                    language, id
                )));
                return;
            }
            Err(e) => {
                let hopeless = e.contains("Sign in to YouTube again");
                let last = i + 1 == DELAYS.len();
                if hopeless || last {
                    let _ = tx.send(Event::Log(format!(
                        "⚠ Could not block auto-subtitles: {} — the video itself uploaded fine.",
                        e
                    )));
                    return;
                }
                let _ = tx.send(Event::Log(format!(
                    "Caption attempt {} failed ({}); retrying in {}s…",
                    i + 1,
                    e,
                    DELAYS[i + 1]
                )));
            }
        }
    }
}

/// Direct upload of a local video file to YouTube — no yt-dlp, no
/// segment extraction. Used by the Upload tab.
pub fn run_upload(job: UploadJob, settings: Settings, tx: Sender<Event>, cancel: Arc<AtomicBool>) {
    let size = match std::fs::metadata(&job.file) {
        Ok(m) => m.len(),
        Err(e) => {
            report_fail(&tx, &cancel, format!("stat {}: {}", job.file.display(), e));
            return;
        }
    };
    let _ = tx.send(Event::Log(format!(
        "Uploading {} ({:.1} MB)",
        job.file.display(),
        size as f64 / 1_048_576.0,
    )));

    if cancel.load(Ordering::SeqCst) {
        let _ = tx.send(Event::Cancelled);
        return;
    }
    let _ = tx.send(Event::Log("Refreshing YouTube access token…".into()));
    let access_token = match oauth::refresh_access_token(&settings.client_id, &settings.client_secret) {
        Ok(t) => t,
        Err(e) => {
            report_fail(&tx, &cancel, format!("auth: {}", e));
            return;
        }
    };

    let lang = job.audio_language.clone();
    let body = VideoBody {
        snippet: VideoSnippet {
            title: &job.title,
            description: &job.description,
            category_id: "22",
            default_language: job.block_auto_subs.then_some(lang.as_str()),
            default_audio_language: job.block_auto_subs.then_some(lang.as_str()),
        },
        status: VideoStatus {
            privacy_status: &job.privacy,
            self_declared_made_for_kids: false,
        },
    };

    let progress_tx = tx.clone();
    let result = upload_video(&access_token, &job.file, &body, &cancel, |sent, total| {
        let f = if total == 0 { 0.0 } else { sent as f32 / total as f32 };
        let detail = format!("{:.1} / {:.1} MB", sent as f64 / 1_048_576.0, total as f64 / 1_048_576.0);
        let _ = progress_tx.send(Event::Progress { phase: "Uploading".into(), fraction: f, detail });
    });

    match result {
        Ok(id) => {
            if job.block_auto_subs {
                block_auto_subtitles(&tx, &access_token, &id, &lang);
            }
            // The user's own file — LinkedIn can post it as-is, but it isn't
            // ours to clean up.
            let _ = tx.send(Event::Done {
                url: format!("https://www.youtube.com/watch?v={}", id),
                file: Some(job.file.clone()),
                temp: false,
            });
        }
        Err(e) => {
            report_fail(&tx, &cancel, format!("upload: {}", e));
        }
    }
}

/// Convert "mm:ss", "hh:mm:ss", or a bare seconds value to seconds.
pub fn parse_timestamp(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Accept a comma decimal separator (`11:38,5`) as well as a period —
    // Jürg is Swiss-German and types the comma his OS locale uses. Rust's
    // `f64::parse` only accepts `.`, so without this the seconds part of
    // `38,5` failed to parse and was silently swallowed, collapsing the
    // timestamp to `11:00`.
    let s = s.replace(',', ".");
    // Tolerate a period typed where the mm:ss (or hh:mm) colon belongs
    // (`13.50.6` for `13:50.6`): keep the LAST dot as the decimal point and
    // promote any earlier dots to `:` field separators. A well-formed
    // single-dot timestamp like `11:38.5` is unaffected (its only dot is the
    // last one). Without this, the stray `.` made `13.50.6` one un-parseable
    // token that silently became 0.0, so the cut failed with the confusing
    // "must lie within the segment" error.
    let last_dot = s.rfind('.');
    let s: String = s
        .char_indices()
        .map(|(i, c)| if c == '.' && Some(i) != last_dot { ':' } else { c })
        .collect();
    // Parse each colon-separated field. An empty field (a stray trailing
    // `:`) counts as 0, but any non-empty field that isn't a number makes
    // the whole timestamp invalid (`None`) so the caller surfaces a clear
    // "can't parse timestamp" error instead of a silent 0.
    let mut nums: Vec<f64> = Vec::new();
    for p in s.split(':') {
        let p = p.trim();
        if p.is_empty() {
            nums.push(0.0);
        } else {
            nums.push(p.parse().ok()?);
        }
    }
    match nums.len() {
        1 => Some(nums[0]),
        2 => Some(nums[0] * 60.0 + nums[1]),
        3 => Some(nums[0] * 3600.0 + nums[1] * 60.0 + nums[2]),
        _ => None,
    }
}

/// Format seconds in the same mm:ss.s style `parse_timestamp` reads, so a
/// resolved "whole video" end time round-trips through the cache stamp and
/// every downstream parse.
fn format_timestamp(secs: f64) -> String {
    let s = secs.max(0.0);
    let h = (s / 3600.0).floor();
    let rem = s - h * 3600.0;
    let m = (rem / 60.0).floor();
    let sec = rem - m * 60.0;
    if h > 0.0 {
        format!("{}:{:02}:{:04.1}", h as u64, m as u64, sec)
    } else {
        format!("{}:{:04.1}", m as u64, sec)
    }
}

/// Ask yt-dlp for the video's duration in seconds (metadata only, nothing
/// downloaded — `--print` implies skip-download). Used when the End field is
/// left empty: empty = the whole video.
fn fetch_youtube_duration(url: &str, settings: &Settings) -> Result<f64, String> {
    let mut cmd = Command::new("yt-dlp");
    cmd.args(["--print", "duration", "--no-warnings"]);
    if !settings.cookies_browser.is_empty() {
        cmd.arg("--cookies-from-browser").arg(&settings.cookies_browser);
    }
    cmd.arg(url);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    detach_group(&mut cmd);
    let out = cmd.output().map_err(|e| {
        format!("yt-dlp not found ({}). Install with `brew install yt-dlp`.", e)
    })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        return Err(format!("yt-dlp could not read the video's duration ({})", last));
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.trim().parse::<f64>().ok())
        .filter(|d| *d > 0.0)
        .ok_or_else(|| "yt-dlp returned no duration for this video (live stream?)".into())
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

pub fn run(mut job: Job, settings: Settings, tx: Sender<Event>, cancel: Arc<AtomicBool>) {
    let _ = tx.send(Event::Log(format!("Starting job for {}", job.source)));

    // A source that is an absolute path to an existing file is edited
    // locally — no yt-dlp involved, the segment is cut straight out of the
    // file with ffmpeg. (The form's 📂 Select… button fills the field with
    // such a path.) A bare YouTube URL/ID is never an absolute path, so the
    // two can't be confused.
    let src_path = PathBuf::from(job.source.trim());
    let is_local = src_path.is_absolute() && src_path.is_file();

    let video_id = if is_local {
        local_source_id(&src_path)
    } else {
        extract_video_id(&job.source)
    };
    let url = if job.source.contains("://") {
        job.source.clone()
    } else {
        format!("https://www.youtube.com/watch?v={}", video_id)
    };
    let original_url = format!("https://www.youtube.com/watch?v={}", video_id);

    // Empty Start/End mean "the whole video": start falls back to 0:00, and
    // an empty end resolves to the source's full duration (ffprobe for a
    // local file, a yt-dlp metadata call for YouTube). Resolved here, before
    // the cache stamp, so a full-video job caches and resumes like any other.
    if job.start.trim().is_empty() {
        job.start = "0:00".into();
    }
    if job.end.trim().is_empty() {
        let _ = tx.send(Event::Progress { phase: "Reading duration".into(), fraction: 0.0, detail: String::new() });
        let dur = if is_local {
            probe_duration(&src_path).ok_or_else(|| "could not read the file's duration".to_string())
        } else {
            fetch_youtube_duration(&url, &settings)
        };
        match dur {
            Ok(d) => {
                job.end = format_timestamp(d);
                let _ = tx.send(Event::Log(format!(
                    "No end time given — using the whole video ({}-{})",
                    job.start, job.end
                )));
            }
            Err(e) => {
                report_fail(&tx, &cancel, format!("read duration: {}", e));
                return;
            }
        }
    }

    let segment_secs: f64 = parse_timestamp(&job.end)
        .and_then(|e| parse_timestamp(&job.start).map(|s| (e - s).max(0.0)))
        .unwrap_or(0.0);

    // Cache cut segments at <cache_dir>/segments/<video_id>_<start>_<end>.mp4.
    // If the exact same (video_id, start, end) is requested again — same
    // video, same timestamps — we skip yt-dlp entirely. Different
    // timestamps re-run yt-dlp with --download-sections, which only
    // pulls the bytes for that range (a few MB for a 30-second clip).
    let stamp = format!("{}_{}", job.start.replace(':', "_"), job.end.replace(':', "_"));
    let cache_dir = segment_cache_dir();
    if let Err(e) = std::fs::create_dir_all(&cache_dir) {
        report_fail(&tx, &cancel, format!("mkdir {}: {}", cache_dir.display(), e));
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
    } else if is_local {
        let _ = tx.send(Event::Log(format!(
            "Extracting {}-{} from local file {} → {}",
            job.start, job.end, src_path.display(), cached.display()
        )));
        let _ = tx.send(Event::Progress { phase: "Extracting".into(), fraction: 0.0, detail: String::new() });
        cleanup_partial_download(&cached);
        // Same atomic build-at-a-temp contract as the download path: a crash
        // mid-encode leaves only the `.partial.mp4` (swept next run), never a
        // truncated file under the canonical cache name.
        let partial = cache_dir.join(format!("{}_{}.partial.mp4", video_id, stamp));
        if let Err(e) = extract_local_segment(&src_path, &partial, &job.start, &job.end, &tx, segment_secs, &cancel) {
            cleanup_partial_download(&cached);
            report_fail(&tx, &cancel, format!("extract segment: {}", e));
            return;
        }
        if let Err(e) = std::fs::rename(&partial, &cached) {
            cleanup_partial_download(&cached);
            report_fail(&tx, &cancel, format!("finalize segment: {}", e));
            return;
        }
        cached.clone()
    } else {
        let _ = tx.send(Event::Log(format!("Downloading {} ({}-{}) → {}", url, job.start, job.end, cached.display())));
        let _ = tx.send(Event::Progress { phase: "Downloading".into(), fraction: 0.0, detail: String::new() });
        // Self-heal: clear any stale temps (`.raw.mp4`, `.partial.mp4`,
        // yt-dlp `.part`/`.fNNN`) left by a prior hard-killed run. `cached`
        // doesn't exist in this branch, so this only sweeps the `<stem>.`
        // siblings, never a valid cache.
        cleanup_partial_download(&cached);
        // Build the segment at a `.partial.mp4` temp and only atomically
        // rename it onto the canonical cache name on success. That keeps
        // `cached.exists()` a *valid-only* signal: a crash/SIGKILL mid-build
        // leaves only the temp (swept next run), never a truncated cache file
        // that a later run would blindly reuse or upload.
        let partial = cache_dir.join(format!("{}_{}.partial.mp4", video_id, stamp));
        #[cfg(target_os = "macos")]
        {
            // Fast path: stream-copy the section (no re-encode, CPU-idle) then
            // hardware-decode+encode it to H.264. Keeps full 4K, runs ~2x
            // real-time on the media engine, and stays cool (no thermal
            // throttle in the tail). The raw download is named with a `.raw.`
            // infix so `cleanup_partial_download(&cached)` sweeps it too.
            let raw = cache_dir.join(format!("{}_{}.raw.mp4", video_id, stamp));
            if let Err(e) = run_yt_dlp(&url, &job.start, &job.end, &raw, &settings, &tx, segment_secs, &cancel, true) {
                cleanup_partial_download(&cached);
                report_fail(&tx, &cancel, format!("yt-dlp: {}", e));
                return;
            }
            if let Err(e) = hw_transcode_segment(&raw, &partial, &tx, segment_secs, &cancel) {
                cleanup_partial_download(&cached);
                report_fail(&tx, &cancel, format!("transcode: {}", e));
                return;
            }
            let _ = std::fs::remove_file(&raw);
        }
        #[cfg(not(target_os = "macos"))]
        {
            if let Err(e) = run_yt_dlp(&url, &job.start, &job.end, &partial, &settings, &tx, segment_secs, &cancel, false) {
                cleanup_partial_download(&cached);
                report_fail(&tx, &cancel, format!("yt-dlp: {}", e));
                return;
            }
        }
        if let Err(e) = std::fs::rename(&partial, &cached) {
            cleanup_partial_download(&cached);
            report_fail(&tx, &cancel, format!("finalize segment: {}", e));
            return;
        }
        cached.clone()
    };

    let size = match std::fs::metadata(&out) {
        Ok(m) => m.len(),
        Err(e) => {
            report_fail(&tx, &cancel, format!("yt-dlp produced no file: {}", e));
            return;
        }
    };
    let _ = tx.send(Event::Log(format!("Segment ready: {:.1} MB", size as f64 / 1_048_576.0)));

    // "Watch the original" mode: hand the raw segment straight to the in-window
    // viewer so the user can scrub it and read off cut-point timestamps. No
    // edits, no audio repair (kept fast), no preview/upload.
    if job.source_only {
        let start_secs = parse_timestamp(&job.start).unwrap_or(0.0).max(0.0);
        let _ = tx.send(Event::Log("Original ready — scrub it in the window to find your cut points.".into()));
        let _ = tx.send(Event::SourceReady { path: out.clone(), start_secs });
        return;
    }

    // Guard against the yt-dlp merge desync that leaves the segment's audio
    // shorter than its video (sound stops early; the fade-out plays silent).
    // Re-fetches and remuxes aligned audio in place when a shortfall is found,
    // then purges the stale derived caches. No-op for healthy segments.
    // A locally-extracted segment can't have this desync (one ffmpeg encode,
    // no separate audio download), so it skips the check.
    if !is_local {
        repair_segment_audio(&out, &url, &job.start, &job.end, &cache_dir, &video_id, &stamp, &settings, &tx, &cancel);
        if cancel.load(Ordering::SeqCst) {
            let _ = tx.send(Event::Cancelled);
            return;
        }
    }

    // Optional: silence the sound — the whole clip when no from/till ranges
    // were given, otherwise just those sections (absolute timestamps like the
    // cut-outs; an empty `from` means the clip start, an empty `till` the clip
    // end). Applied *before* the cut-outs so the timestamps map straight onto
    // the source timeline. The video stream is copied untouched (only the
    // audio is filtered + re-encoded), so this stage is fast. Cached as
    // `<video_id>_<stamp>_mute<tag>.mp4`.
    let has_mute = job.mute;
    let mut mute_covers_end = false;
    let (seg, mute_tag) = if has_mute {
        let start_secs = parse_timestamp(&job.start).unwrap_or(0.0);
        let dur = probe_duration(&out).unwrap_or(segment_secs);
        let mut ranges: Vec<(f64, f64)> = Vec::new();
        if job.mutes.is_empty() {
            ranges.push((0.0, dur));
        } else {
            for (from_s, till_s) in &job.mutes {
                let rel_from = if from_s.is_empty() {
                    0.0
                } else {
                    match parse_timestamp(from_s) {
                        Some(v) => v - start_secs,
                        None => {
                            report_fail(&tx, &cancel, format!("mute: can't parse from-timestamp {:?}", from_s));
                            return;
                        }
                    }
                };
                let rel_till = if till_s.is_empty() {
                    dur
                } else {
                    match parse_timestamp(till_s) {
                        Some(v) => v - start_secs,
                        None => {
                            report_fail(&tx, &cancel, format!("mute: can't parse till-timestamp {:?}", till_s));
                            return;
                        }
                    }
                };
                let rf = rel_from.clamp(0.0, dur);
                let rt = rel_till.clamp(0.0, dur);
                if rt <= rf + 0.05 {
                    report_fail(&tx, &cancel, format!(
                        "mute range {}–{} must lie within the segment {}–{} (got offsets {:.1}s–{:.1}s within a {:.1}s clip)",
                        if from_s.is_empty() { "start" } else { from_s },
                        if till_s.is_empty() { "end" } else { till_s },
                        job.start, job.end, rel_from, rel_till, dur
                    ));
                    return;
                }
                ranges.push((rf, rt));
            }
        }
        // When the silence reaches the clip's end, the fade-out must not glue
        // un-muted continuing source audio under the held last frame.
        mute_covers_end = ranges.iter().any(|&(_, t)| t >= dur - 0.05);
        let tag = mutes_tag(&job.mutes);
        let mute_path = cache_dir.join(format!("{}_{}_mute{}.mp4", video_id, stamp, tag));
        if mute_path.exists() {
            let _ = tx.send(Event::Log(format!("Reusing cached muted segment {}", mute_path.display())));
        } else {
            let muted: f64 = ranges.iter().map(|(a, b)| b - a).sum();
            let _ = tx.send(Event::Log(format!(
                "Muting the sound of {} section{} ({:.1}s of {:.1}s silenced)…",
                ranges.len(), if ranges.len() == 1 { "" } else { "s" }, muted, dur
            )));
            if let Err(e) = apply_mute(&out, &mute_path, &ranges, dur, &tx, &cancel) {
                let _ = std::fs::remove_file(&mute_path);
                report_fail(&tx, &cancel, format!("mute: {}", e));
                return;
            }
        }
        (mute_path, format!("_mute{}", tag))
    } else {
        (out.clone(), String::new())
    };

    // Optional: remove one or more middle sections from the segment and
    // concatenate everything that remains into one clip. Each cut's timestamps
    // are absolute in the source (same style as start/end), so we convert them
    // to offsets from `start`. Applied first — before stretch, title, and
    // fades — so every later stage operates on the joined clip and its
    // (shorter) duration. Cached as `<video_id>_<stamp>_cut<tag>.mp4`.
    let has_cuts = job.cut_middle && !job.cuts.is_empty();
    let (clip, segment_secs) = if has_cuts {
        let start_secs = parse_timestamp(&job.start).unwrap_or(0.0);
        let dur = probe_duration(&seg).unwrap_or(segment_secs);
        // Parse + convert every cut to an offset range within the segment.
        let mut ranges: Vec<(f64, f64)> = Vec::new();
        for (from_s, till_s) in &job.cuts {
            let from_abs = match parse_timestamp(from_s) {
                Some(v) => v,
                None => {
                    report_fail(&tx, &cancel, format!("cut-out: can't parse from-timestamp {:?}", from_s));
                    return;
                }
            };
            let till_abs = match parse_timestamp(till_s) {
                Some(v) => v,
                None => {
                    report_fail(&tx, &cancel, format!("cut-out: can't parse till-timestamp {:?}", till_s));
                    return;
                }
            };
            let rel_from = from_abs - start_secs;
            let rel_till = till_abs - start_secs;
            // Allow a cut to touch the segment start (rel_from == 0 → trims the
            // beginning) or end (rel_till == dur → trims the tail); only require
            // a non-empty range that lies within the segment.
            let eps = 0.05_f64;
            if !(rel_from >= -eps && rel_till > rel_from + eps && rel_till <= dur + eps) {
                report_fail(&tx, &cancel, format!(
                    "cut-out range {}–{} must lie within the segment {}–{} (got offsets {:.1}s–{:.1}s within a {:.1}s clip)",
                    from_s, till_s, job.start, job.end, rel_from, rel_till, dur
                ));
                return;
            }
            ranges.push((rel_from.max(0.0), rel_till.min(dur)));
        }
        // Sort by start and reject overlaps — the concat filter needs disjoint,
        // ordered kept-segments.
        ranges.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        for w in ranges.windows(2) {
            if w[1].0 < w[0].1 {
                report_fail(&tx, &cancel,
                    "cut-out ranges overlap — make them separate, non-overlapping sections".to_string());
                return;
            }
        }
        let removed: f64 = ranges.iter().map(|(a, b)| b - a).sum();
        let new_secs = dur - removed;
        if new_secs < 0.1 {
            report_fail(&tx, &cancel,
                "cut-out would remove the entire clip — leave some section uncut".to_string());
            return;
        }
        let cut_path = cache_dir.join(format!("{}_{}{}_cut{}.mp4", video_id, stamp, mute_tag, cuts_tag(&job.cuts)));
        if cut_path.exists() {
            let _ = tx.send(Event::Log(format!("Reusing cached cut segment {}", cut_path.display())));
        } else {
            let _ = tx.send(Event::Log(format!(
                "Cutting out {} section{} ({:.1}s removed) — joining the {:.1}s that remain…",
                ranges.len(), if ranges.len() == 1 { "" } else { "s" }, removed, new_secs
            )));
            if let Err(e) = apply_cut_middle(&seg, &cut_path, &ranges, dur, &tx, new_secs, &cancel) {
                let _ = std::fs::remove_file(&cut_path);
                report_fail(&tx, &cancel, format!("cut-out: {}", e));
                return;
            }
        }
        (cut_path, new_secs)
    } else {
        (seg.clone(), segment_secs)
    };

    // Tag derived caches (stretch, titled) with the mute + cut ranges so
    // toggling either on/off — or changing their bounds — re-encodes instead
    // of serving a stale clip built from the unmuted/un-cut segment.
    let cut_tag = if has_cuts {
        format!("{}_cut{}", mute_tag, cuts_tag(&job.cuts))
    } else {
        mute_tag.clone()
    };

    // Optional: time-stretch (slow down) the core clip so it lasts
    // `stretch_secs` seconds longer. Applied to the (post-cut) segment
    // *before* the title overlay and fades, so those keep their own
    // independent durations — only the movie content is stretched. Cached as
    // `<video_id>_<stamp><cut_tag>_stretch<secs>.mp4`.
    let stretch_secs = job.stretch_secs.max(1);
    let base = if job.stretch {
        let stretch_path = cache_dir.join(format!("{}_{}{}_stretch{}.mp4", video_id, stamp, cut_tag, stretch_secs));
        if stretch_path.exists() {
            let _ = tx.send(Event::Log(format!(
                "Reusing cached stretched segment {}",
                stretch_path.display()
            )));
        } else {
            let _ = tx.send(Event::Log(format!("Stretching clip by {}s…", stretch_secs)));
            if let Err(e) = apply_stretch(&clip, &stretch_path, &tx, segment_secs, stretch_secs, &cancel) {
                let _ = std::fs::remove_file(&stretch_path);
                report_fail(&tx, &cancel, format!("stretch: {}", e));
                return;
            }
        }
        stretch_path
    } else {
        clip.clone()
    };

    // The core clip's duration after the optional stretch — used to drive
    // the progress bars of the downstream (title/fade) re-encodes.
    let core_secs = if job.stretch { segment_secs + stretch_secs as f64 } else { segment_secs };

    // Tag the titled cache filename with the cut and stretch so toggling
    // either on/off doesn't serve a stale titled clip.
    let stretch_tag = format!(
        "{}{}",
        cut_tag,
        if job.stretch { format!("_stretch{}", stretch_secs) } else { String::new() }
    );

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
            if let Err(e) = apply_title_overlay(&base, &job.title, job.overlay_color, job.overlay_pos, &overlay_path, &tx, core_secs, &cancel) {
                let _ = std::fs::remove_file(&overlay_path);
                report_fail(&tx, &cancel, format!("title overlay: {}", e));
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
            if let Err(e) = apply_fade_in(&final_out, &fin_path, &tx, core_secs, fade_in_secs, &cancel) {
                let _ = std::fs::remove_file(&fin_path);
                report_fail(&tx, &cancel, format!("fade-in: {}", e));
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
    let end_card: Option<(&str, [u8; 3], [f32; 2])> = {
        let t = job.end_text.trim();
        if t.is_empty() {
            None
        } else {
            Some((t, job.end_text_color, job.end_text_pos))
        }
    };
    let delivered = if job.fade_out {
        // The cached output depends on more than (input, secs): a "hold bright"
        // ending and an end-card text produce a different file, so fold both
        // into the cache key. With neither (the pre-1.0.59 behaviour) the tag
        // is empty and the filename is unchanged, so old caches still match.
        let variant = fade_variant_tag(job.fade_out_hold_bright, &job.end_text, job.end_text_color, job.end_text_pos);
        let fade_path = fade_output_path(&after_fade_in, fade_secs, &variant);
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
            // When the mute reaches the clip's end, continuing source audio
            // would blast un-muted sound right after the silence — hold silent
            // instead.
            let tail_audio: Option<PathBuf> = if mute_covers_end {
                let _ = tx.send(Event::Log(
                    "Sound is muted through the end of the clip — the held frame stays silent.".into(),
                ));
                None
            } else if let Some(end_secs) = parse_timestamp(&job.end) {
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
                    let fetched = if is_local {
                        extract_local_tail_audio(&src_path, end_secs, fade_secs, &tail_path, &tx, &cancel)
                    } else {
                        download_tail_audio(&url, end_secs, fade_secs, &tail_path, &settings, &tx, &cancel)
                    };
                    match fetched {
                        Ok(()) if tail_path.exists() => Some(tail_path),
                        Ok(()) => None,
                        // A cancel mid-fetch surfaces here as an Err too. Bail
                        // cleanly with Cancelled instead of logging the
                        // misleading "no continuing audio" line and then
                        // spawning a doomed fade-out encode.
                        Err(_) if cancel.load(Ordering::SeqCst) => {
                            let _ = std::fs::remove_file(&tail_path);
                            let _ = tx.send(Event::Cancelled);
                            return;
                        }
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
                &cancel,
                job.fade_out_hold_bright,
                end_card,
            ) {
                let _ = std::fs::remove_file(&fade_path);
                report_fail(&tx, &cancel, format!("fade-out: {}", e));
                return;
            }
        }
        fade_path
    } else {
        after_fade_in.clone()
    };

    // Optional: mix the background-music file under the finished clip —
    // applied last so the music spans everything including the freeze-frame
    // fades, with its own fade-out over the final seconds. The video stream is
    // copied untouched (remux speed). Cached as `<stem>_music<hash>_v<vol>.mp4`
    // where the hash covers the music file's path/size/mtime, so picking a
    // different file (or re-downloading it) re-mixes instead of serving a
    // stale clip.
    let pre_music = delivered.clone();
    let delivered = if job.bg_music && !job.bg_music_path.trim().is_empty() {
        let music = PathBuf::from(job.bg_music_path.trim());
        let vol = job.bg_music_volume.clamp(1, 300);
        let stem = pre_music
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("clip")
            .to_string();
        let music_path = pre_music.with_file_name(format!("{}_music{}_v{}.mp4", stem, music_file_tag(&music), vol));
        if music_path.exists() {
            let _ = tx.send(Event::Log(format!("Reusing cached music mix {}", music_path.display())));
        } else {
            let total = probe_duration(&pre_music).unwrap_or_else(|| {
                core_secs
                    + if job.fade_in { fade_in_secs as f64 } else { 0.0 }
                    + if job.fade_out { fade_secs as f64 } else { 0.0 }
            });
            let _ = tx.send(Event::Log(format!(
                "Mixing background music {} at {}%…",
                music.file_name().and_then(|s| s.to_str()).unwrap_or("(file)"),
                vol
            )));
            if let Err(e) = apply_bg_music(&pre_music, &music, &music_path, total, vol, &tx, &cancel) {
                let _ = std::fs::remove_file(&music_path);
                report_fail(&tx, &cancel, format!("background music: {}", e));
                return;
            }
        }
        music_path
    } else {
        delivered
    };

    if job.preview_only {
        let _ = tx.send(Event::Log(format!("Preview file kept at {}", delivered.display())));
        // Top pane = the full original video (embedded from YouTube via
        // `source_id`, deep-linked to the segment start); bottom = the edited
        // clip (`delivered`). `out` (the raw start–end download) is passed only
        // as an offline fallback for the top pane.
        let start_secs = parse_timestamp(&job.start).unwrap_or(0.0).max(0.0) as u32;
        let _ = tx.send(Event::Preview {
            original: out.clone(),
            edited: delivered.clone(),
            // A local source has no YouTube id to embed — the empty id makes
            // the browser-fallback top pane show the local segment instead.
            source_id: if is_local { String::new() } else { video_id.clone() },
            start_secs,
        });
        return;
    }

    if cancel.load(Ordering::SeqCst) {
        let _ = tx.send(Event::Cancelled);
        return;
    }
    let _ = tx.send(Event::Log("Refreshing YouTube access token…".to_string()));
    let access_token = match oauth::refresh_access_token(&settings.client_id, &settings.client_secret) {
        Ok(t) => t,
        Err(e) => {
            report_fail(&tx, &cancel, format!("auth: {}", e));
            return;
        }
    };

    // The "Original: <url>" suffix only makes sense for a YouTube source — a
    // local file has no public original to link (and its path stays private).
    let description = if is_local {
        job.description.clone()
    } else {
        format!(
            "{}\n\nOriginal: {} ({}-{})",
            job.description, original_url, job.start, job.end
        )
    };

    let lang = job.audio_language.clone();
    let body = VideoBody {
        snippet: VideoSnippet {
            title: &job.title,
            description: &description,
            category_id: "22",
            default_language: job.block_auto_subs.then_some(lang.as_str()),
            default_audio_language: job.block_auto_subs.then_some(lang.as_str()),
        },
        status: VideoStatus {
            privacy_status: &job.privacy,
            self_declared_made_for_kids: false,
        },
    };

    let upload_size = std::fs::metadata(&delivered).map(|m| m.len()).unwrap_or(size);
    let _ = tx.send(Event::Log(format!("Uploading {:.1} MB…", upload_size as f64 / 1024.0 / 1024.0)));
    let progress_tx = tx.clone();
    let result = upload_video(&access_token, &delivered, &body, &cancel, |sent, total| {
        let f = if total == 0 { 0.0 } else { sent as f32 / total as f32 };
        let detail = format!("{:.1} / {:.1} MB", sent as f64 / 1_048_576.0, total as f64 / 1_048_576.0);
        let _ = progress_tx.send(Event::Progress { phase: "Uploading".into(), fraction: f, detail });
    });

    // Remove the intermediates (fade-out, fade-in, titled overlay, raw
    // segment) so the cache dir doesn't accumulate. Removing the same path
    // twice is a harmless ignored error.
    //
    // `delivered` is deliberately KEPT: the success banner's Post-to-LinkedIn
    // button needs the actual video file, and re-downloading from YouTube what
    // we just uploaded is both slow and lower quality (YouTube may still be
    // processing). The GUI deletes it once the banner is dismissed. Skipping it
    // here has to be by *path*, because with no fade-out `delivered` IS
    // `after_fade_in` (and possibly `final_out`/`base` too) — deleting those
    // blind would delete the delivered file with them.
    for f in [&after_fade_in, &final_out, &base, &clip, &seg, &out, &pre_music] {
        if f != &delivered {
            let _ = std::fs::remove_file(f);
        }
    }

    match result {
        Ok(id) => {
            if job.block_auto_subs {
                block_auto_subtitles(&tx, &access_token, &id, &lang);
            }
            let _ = tx.send(Event::Done {
                url: format!("https://www.youtube.com/watch?v={}", id),
                file: Some(delivered.clone()),
                temp: true,
            });
        }
        Err(e) => {
            // Nothing reached YouTube, so there's no success banner to post
            // from — the kept file has no reader and would just leak.
            let _ = std::fs::remove_file(&delivered);
            report_fail(&tx, &cancel, format!("upload: {}", e));
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
    cancel: &Arc<AtomicBool>,
    stream_copy: bool,
) -> Result<(), String> {
    let mut cmd = Command::new("yt-dlp");
    cmd.args([
        "-f",
        "bestvideo[height<=2160]+bestaudio/best",
        "--download-sections",
        &format!("*{}-{}", start, end),
    ]);
    // `--force-keyframes-at-cuts` gives a frame-accurate cut but forces a
    // *re-encode* of the section inside yt-dlp's ffmpeg downloader — slow,
    // CPU-bound (software VP9 decode + libx264). The macOS fast path
    // (`stream_copy`) instead does a pure stream copy here (no re-encode,
    // download-bound, CPU-idle) and re-encodes afterwards via
    // `hw_transcode_segment`, which hardware-decodes the VP9 and
    // hardware-encodes to H.264. Empirically the stream-copy section is
    // already frame-accurate at the start (verified frame-exact even at a
    // mid-segment start), so no boundary trimming is needed downstream.
    if !stream_copy {
        cmd.arg("--force-keyframes-at-cuts");
    }
    cmd.args([
        "--merge-output-format",
        "mp4",
        "--newline",
        "-o",
        out.to_str().ok_or("non-utf8 path")?,
    ]);
    // Only relevant on the re-encode path (`!stream_copy`): redirect yt-dlp's
    // forced-keyframe re-encode onto Apple's hardware H.264 encoder. With the
    // macOS two-step this branch isn't taken (macOS uses `stream_copy`), but
    // it's kept correct in case the re-encode path is ever used on macOS.
    if cfg!(target_os = "macos") && !stream_copy {
        cmd.arg("--downloader-args")
            .arg("ffmpeg_o:-c:v h264_videotoolbox -q:v 65 -allow_sw 1");
    }
    if !settings.cookies_browser.is_empty() {
        cmd.arg("--cookies-from-browser").arg(&settings.cookies_browser);
    }
    cmd.arg(url);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    detach_group(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| {
        format!("yt-dlp not found ({}). Install with `brew install yt-dlp` or `pipx install yt-dlp`.", e)
    })?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let stderr_capture: LineBuf = Arc::new(Mutex::new(VecDeque::new()));
    let cap_for_thread = stderr_capture.clone();
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, segment_secs, None, "Downloading segment".to_string())));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, segment_secs, Some(cap_for_thread), "Downloading segment".to_string())));
    let status = wait_or_cancel(&mut child, cancel)?;
    if let Some(h) = h_o { let _ = h.join(); }
    if let Some(h) = h_e { let _ = h.join(); }
    if !status.success() {
        let detail = summarize_yt_dlp_failure(&stderr_capture);
        let suffix = if detail.is_empty() { String::new() } else { format!(" — {}", detail) };
        return Err(format!("exited with {:?}{}", status.code(), suffix));
    }
    Ok(())
}

/// macOS fast path, step 2: re-encode the stream-copied VP9 section to H.264
/// using Apple's media engine for BOTH decode and encode. `-hwaccel
/// videotoolbox -hwaccel_output_format videotoolbox_vld` keeps decoded frames
/// on the GPU and feeds them straight to `h264_videotoolbox` (near zero-copy),
/// so a 4K cut runs at ~2x real-time with the CPU idle — no software VP9
/// decode, no thermal throttling (the cause of the "last 30% is slow" tail
/// slowdown), and the result is a normal H.264 mp4 so every downstream stage
/// (overlay/fade/stretch) and the cache contract are unchanged.
///
/// The input is already exactly the requested frames (the stream-copy section
/// is frame-accurate), so we transcode the whole clip — no `-ss`/`-t` trim.
/// If hardware decode of this stream isn't available on the running chip, the
/// first attempt fails and we retry with software decode (still HW encode) so
/// the cut always completes, just slower.
#[cfg(target_os = "macos")]
fn hw_transcode_segment(
    raw_in: &std::path::Path,
    out: &std::path::Path,
    tx: &Sender<Event>,
    segment_secs: f64,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let attempts: [&[&str]; 2] = [
        &["-hwaccel", "videotoolbox", "-hwaccel_output_format", "videotoolbox_vld"],
        &[], // software-decode fallback (HW encode still used)
    ];
    let mut last_err = String::new();
    for (i, hw) in attempts.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            return Err(CANCEL_MSG.to_string());
        }
        let _ = tx.send(Event::Log(format!(
            "Transcoding segment (4K, {} decode + hardware encode)…",
            if hw.is_empty() { "software" } else { "hardware" }
        )));
        let mut cmd = Command::new("ffmpeg");
        cmd.arg("-y");
        cmd.args(*hw);
        cmd.args(["-i", raw_in.to_str().ok_or("non-utf8 input path")?]);
        // Map video + (optional) audio explicitly so a silent clip doesn't error.
        cmd.args(["-map", "0:v:0", "-map", "0:a:0?"]);
        cmd.args(["-c:v", "h264_videotoolbox", "-q:v", "65", "-allow_sw", "1"]);
        cmd.args(["-c:a", "aac", "-movflags", "+faststart"]);
        cmd.arg(out.to_str().ok_or("non-utf8 output path")?);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        detach_group(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| format!("ffmpeg spawn ({}): install ffmpeg", e))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let tx_o = tx.clone();
        let tx_e = tx.clone();
        let stderr_capture: LineBuf = Arc::new(Mutex::new(VecDeque::new()));
        let cap_for_thread = stderr_capture.clone();
        let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, segment_secs, None, "Preparing segment".to_string())));
        let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, segment_secs, Some(cap_for_thread), "Preparing segment".to_string())));
        let status = wait_or_cancel(&mut child, cancel)?;
        if let Some(h) = h_o { let _ = h.join(); }
        if let Some(h) = h_e { let _ = h.join(); }
        if status.success() {
            return Ok(());
        }
        let detail = summarize_yt_dlp_failure(&stderr_capture);
        last_err = format!("exited with {:?}{}", status.code(),
            if detail.is_empty() { String::new() } else { format!(" — {}", detail) });
        let _ = std::fs::remove_file(out);
        if i == 0 {
            let _ = tx.send(Event::Log(
                "Hardware decode unavailable for this stream — retrying with software decode…".into()
            ));
        }
    }
    Err(last_err)
}

/// Stable cache id for a local source file — the local counterpart of the
/// YouTube video id in every cache filename. A sanitized file stem (so the
/// cache dir stays human-readable) plus a short hash of (path, size, mtime),
/// so the same file reuses its cached segments across Preview → Upload while
/// any change to the file's content busts them.
fn local_source_id(path: &std::path::Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let stem: String = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("video")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(40)
        .collect();
    let mut h = DefaultHasher::new();
    path.hash(&mut h);
    if let Ok(m) = path.metadata() {
        m.len().hash(&mut h);
        if let Ok(t) = m.modified() {
            t.hash(&mut h);
        }
    }
    format!("local_{}_{:08x}", stem, h.finish() as u32)
}

/// Cut the start–end segment straight out of a local file with ffmpeg — the
/// local-source counterpart of the yt-dlp download. `-ss` before `-i`
/// fast-seeks to the window and the re-encode makes the cut frame-accurate,
/// producing the same H.264 mp4 (hardware-encoded on macOS via
/// `video_encoder_args`) that every downstream stage and the cache contract
/// expect.
fn extract_local_segment(
    input: &std::path::Path,
    out: &std::path::Path,
    start: &str,
    end: &str,
    tx: &Sender<Event>,
    segment_secs: f64,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let start_secs = parse_timestamp(start).ok_or_else(|| format!("can't parse start {:?}", start))?;
    let end_secs = parse_timestamp(end).ok_or_else(|| format!("can't parse end {:?}", end))?;
    if end_secs <= start_secs {
        return Err(format!("end ({}) must be after start ({})", end, start));
    }
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y");
    cmd.args(["-ss", &format!("{:.3}", start_secs)]);
    cmd.args(["-i", input.to_str().ok_or("non-utf8 input path")?]);
    cmd.args(["-t", &format!("{:.3}", end_secs - start_secs)]);
    // Map video + (optional) audio explicitly so a silent clip doesn't error.
    cmd.args(["-map", "0:v:0", "-map", "0:a:0?"]);
    cmd.args(video_encoder_args());
    cmd.args(["-c:a", "aac", "-movflags", "+faststart"]);
    cmd.arg(out.to_str().ok_or("non-utf8 output path")?);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    detach_group(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg spawn ({}): install ffmpeg", e))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let stderr_capture: LineBuf = Arc::new(Mutex::new(VecDeque::new()));
    let cap_for_thread = stderr_capture.clone();
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, segment_secs, None, "Extracting segment".to_string())));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, segment_secs, Some(cap_for_thread), "Extracting segment".to_string())));
    let status = wait_or_cancel(&mut child, cancel)?;
    if let Some(h) = h_o { let _ = h.join(); }
    if let Some(h) = h_e { let _ = h.join(); }
    if !status.success() {
        let detail = summarize_yt_dlp_failure(&stderr_capture);
        let suffix = if detail.is_empty() { String::new() } else { format!(" — {}", detail) };
        return Err(format!("exited with {:?}{}", status.code(), suffix));
    }
    Ok(())
}

/// Extract the `fade_secs` seconds of audio that follow the cut point from a
/// local source file — the local counterpart of `download_tail_audio`.
/// Best-effort like its sibling: an error (nothing after the cut, no audio
/// stream) is logged and tolerated by the caller, which falls back to a
/// silent hold.
fn extract_local_tail_audio(
    input: &std::path::Path,
    end_secs: f64,
    fade_secs: u32,
    out: &std::path::Path,
    tx: &Sender<Event>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y");
    cmd.args(["-ss", &format!("{:.3}", end_secs)]);
    cmd.args(["-i", input.to_str().ok_or("non-utf8 input path")?]);
    cmd.args(["-t", &fade_secs.to_string()]);
    cmd.args(["-vn", "-c:a", "aac"]);
    cmd.arg(out.to_str().ok_or("non-utf8 output path")?);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    detach_group(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg spawn ({})", e))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let stderr_capture: LineBuf = Arc::new(Mutex::new(VecDeque::new()));
    let cap_for_thread = stderr_capture.clone();
    let secs = fade_secs as f64;
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, secs, None, "Fetching fade audio".to_string())));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, secs, Some(cap_for_thread), "Fetching fade audio".to_string())));
    let status = wait_or_cancel(&mut child, cancel)?;
    if let Some(h) = h_o { let _ = h.join(); }
    if let Some(h) = h_e { let _ = h.join(); }
    if !status.success() {
        let detail = summarize_yt_dlp_failure(&stderr_capture);
        let suffix = if detail.is_empty() { String::new() } else { format!(" — {}", detail) };
        return Err(format!("exited with {:?}{}", status.code(), suffix));
    }
    // A seek past the end of the file can "succeed" with a header-only file —
    // treat that as no continuing audio so the caller falls back cleanly.
    if out.metadata().map(|m| m.len()).unwrap_or(0) < 1024 {
        let _ = std::fs::remove_file(out);
        return Err("no audio after the cut point".to_string());
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
    cancel: &Arc<AtomicBool>,
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

    detach_group(&mut cmd);
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
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, secs, None, "Fetching fade audio".to_string())));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, secs, Some(cap_for_thread), "Fetching fade audio".to_string())));
    let status = wait_or_cancel(&mut child, cancel)?;
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
    cancel: &Arc<AtomicBool>,
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

    detach_group(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("yt-dlp not found ({})", e))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let stderr_capture: LineBuf = Arc::new(Mutex::new(VecDeque::new()));
    let cap_for_thread = stderr_capture.clone();
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, secs, None, "Fetching aligned audio".to_string())));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, secs, Some(cap_for_thread), "Fetching aligned audio".to_string())));
    let status = wait_or_cancel(&mut child, cancel)?;
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
    cancel: &Arc<AtomicBool>,
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
    if let Err(e) = download_section_audio(url, start, end, &body_audio, settings, tx, vdur, cancel) {
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
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y")
        .arg("-i").arg(segment)
        .arg("-i").arg(&body_audio)
        .args(["-map", "0:v:0", "-map", "1:a:0", "-c", "copy", "-shortest", "-movflags", "+faststart"])
        .arg(&fixed)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_group(&mut cmd);
    let status = match cmd.spawn() {
        Ok(mut child) => wait_or_cancel(&mut child, cancel),
        Err(e) => Err(format!("ffmpeg spawn: {}", e)),
    };
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
fn stream_progress<R: Read>(reader: R, tx: Sender<Event>, segment_secs: f64, capture: Option<LineBuf>, phase: String) {
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
                        emit_line(&line, &tx, segment_secs, capture.as_ref(), &phase);
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
        emit_line(&line, &tx, segment_secs, capture.as_ref(), &phase);
    }
}

fn emit_line(line: &str, tx: &Sender<Event>, segment_secs: f64, capture: Option<&LineBuf>, phase: &str) {
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
                phase: phase.to_string(),
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
/// frame, painted at the normalized `pos`. Using a frame-sized PNG keeps
/// the ffmpeg overlay invocation trivial (overlay defaults to 0:0). The
/// font is bundled into the binary so we don't depend on any system fonts.
/// `title` may contain newlines — the lines are stacked and aligned inside
/// the block by the same `pos[0]` that places the block in the frame.
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

    // Split on newlines so an end card can be several lines. A string without
    // newlines yields exactly one line, so the single-line path is unchanged.
    let raw_lines: Vec<&str> = title.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    let n_lines = raw_lines.len().max(1) as f32;

    let mut font_size = (video_h as f32 / 16.92).max(23.4);
    // Shrink the font if a tall multi-line block would overflow the frame.
    // One line is far below the limit, so this never touches single-line text.
    {
        let probe = font.as_scaled(PxScale::from(font_size));
        let block_h = (probe.height() + probe.line_gap()) * (n_lines - 1.0)
            + (probe.ascent() - probe.descent());
        let max_h = video_h as f32 * 0.85;
        if block_h > max_h {
            font_size = (font_size * max_h / block_h).max(10.0);
        }
    }
    let scale = PxScale::from(font_size);
    let scaled = font.as_scaled(scale);
    let ascent = scaled.ascent();
    let line_advance = scaled.height() + scaled.line_gap();

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

    // If a line would overflow ~85% of the video width, truncate it with an
    // ellipsis. Simple char-by-char shrink; lines are short so we don't need
    // anything smarter.
    let max_text_w = video_w as f32 * 0.85;
    let truncate = |s: &str| -> String {
        let mut display = s.to_string();
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
        display
    };
    let lines: Vec<String> = raw_lines.iter().map(|l| truncate(l)).collect();

    let descent = scaled.descent();
    // The block is as wide as its widest line and as tall as the stacked lines.
    let text_height = line_advance * (lines.len() as f32 - 1.0) + (ascent - descent);
    let text_width = lines.iter().map(|l| measure(l)).fold(0.0f32, f32::max);

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
    for (i, line) in lines.iter().enumerate() {
        // Each line sits inside the block the same way the block sits in the
        // frame: flush left at x=0, centred at x=0.5, flush right at x=1.
        let mut x = text_origin_x + (text_width - measure(line)) * nx;
        let baseline = text_origin_y + i as f32 * line_advance;
        let mut prev: Option<ab_glyph::GlyphId> = None;
        for c in line.chars() {
            let mut g = scaled.scaled_glyph(c);
            if let Some(p) = prev {
                x += scaled.kern(p, g.id);
            }
            prev = Some(g.id);
            g.position = ab_glyph::point(x, baseline);
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
/// Video-encoder arguments for the re-encode steps (title overlay, fades,
/// stretch). On macOS we use Apple's hardware H.264 encoder
/// (`h264_videotoolbox`) — on Apple Silicon (M-series) the dedicated media
/// engine encodes 4K in near-real-time, whereas the CPU-bound `libx264`
/// crawls on 4K sources (the original cause of the "Encoding segment" being
/// painfully slow). Everywhere else we keep `libx264 -preset veryfast`.
///
/// `-q:v` is VideoToolbox's constant-quality knob (1–100, higher = better,
/// Apple-Silicon only); 60 is visually transparent for this content. We pin
/// `-pix_fmt yuv420p` so the output stays 8-bit 4:2:0 (broadly compatible /
/// what YouTube expects) and `-allow_sw 1` lets ffmpeg fall back to a
/// software VideoToolbox path rather than erroring if the HW encoder is
/// momentarily unavailable.
fn video_encoder_args() -> Vec<&'static str> {
    if cfg!(target_os = "macos") {
        vec![
            "-c:v", "h264_videotoolbox",
            "-q:v", "65",
            "-allow_sw", "1",
            "-pix_fmt", "yuv420p",
        ]
    } else {
        vec!["-c:v", "libx264", "-preset", "veryfast", "-crf", "20"]
    }
}

/// Decode-side acceleration hook for the re-encode stages (placed before `-i`).
/// **Currently a no-op** on every platform: the apparent "hang" on heavy jobs
/// turned out to be the mpv in-window-preview deadlock (fixed in `mpv.rs`), not
/// CPU starvation from software-decoding 4K. Software decode is fast on Apple
/// Silicon (multi-core ~7× realtime) and avoids the videotoolbox copy-back
/// penalty (~2× slower wall-clock for the same output), so we keep it off. Kept
/// as a helper so it can be flipped back on for genuinely slow/few-core hosts.
fn hwaccel_decode_args() -> Vec<&'static str> {
    vec![]
}

/// Filename-safe tag encoding all cut ranges, e.g. two cuts 0:10–0:15 and
/// 0:30–0:40 → `0_10_0_15-0_30_0_40`. Folded into derived cache filenames so
/// changing the cuts busts the cache.
fn cuts_tag(cuts: &[(String, String)]) -> String {
    cuts.iter()
        .map(|(f, t)| format!("{}_{}", f.replace(':', "_"), t.replace(':', "_")))
        .collect::<Vec<_>>()
        .join("-")
}

/// Cache-filename tag for the mute ranges. An empty list (mute the whole
/// clip) becomes `all`; inside a pair an empty `from`/`till` (= clip
/// start/end) becomes `s`/`e` so the tag stays unambiguous.
fn mutes_tag(mutes: &[(String, String)]) -> String {
    if mutes.is_empty() {
        return "all".into();
    }
    mutes
        .iter()
        .map(|(f, t)| {
            format!(
                "{}_{}",
                if f.is_empty() { "s".to_string() } else { f.replace(':', "_") },
                if t.is_empty() { "e".to_string() } else { t.replace(':', "_") },
            )
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// Short stable id for the chosen background-music file — path + size + mtime
/// hashed, so the cached mix is reused for the same file but re-mixed when a
/// different file is picked (or the same one re-downloaded/edited).
fn music_file_tag(path: &std::path::Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    path.hash(&mut h);
    if let Ok(m) = path.metadata() {
        m.len().hash(&mut h);
        if let Ok(t) = m.modified() {
            t.hash(&mut h);
        }
    }
    format!("{:08x}", h.finish() as u32)
}

/// Mix `music` under `input`'s existing sound at `volume_pct` percent. The
/// music is looped when shorter than the clip (`-stream_loop -1`), trimmed to
/// the clip's length, and faded out over its last ~3 seconds so it never ends
/// mid-note. `amix normalize=0` keeps the original audio at its own level
/// instead of halving both. The video stream is copied bit-for-bit, so this
/// runs at remux speed regardless of resolution.
fn apply_bg_music(
    input: &std::path::Path,
    music: &std::path::Path,
    output: &std::path::Path,
    dur: f64,
    volume_pct: u32,
    tx: &Sender<Event>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let _ = tx.send(Event::Progress {
        phase: "Mixing music".into(),
        fraction: 0.0,
        detail: String::new(),
    });
    let vol = volume_pct as f64 / 100.0;
    let fade_d = 3.0_f64.min(dur.max(0.1));
    let fade_st = (dur - fade_d).max(0.0);
    // atrim caps the endlessly-looped music at the clip length (and sends EOF,
    // so amix duration=first terminates cleanly).
    let filter = format!(
        "[1:a]volume={vol:.3},atrim=0:{dur:.3},asetpts=PTS-STARTPTS,afade=t=out:st={fade_st:.3}:d={fade_d:.3}[m];[0:a][m]amix=inputs=2:duration=first:dropout_transition=0:normalize=0[a]"
    );

    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-y",
        "-i",
        input.to_str().ok_or("non-utf8 input path")?,
        "-stream_loop",
        "-1",
        "-i",
        music.to_str().ok_or("non-utf8 music path")?,
        "-filter_complex",
        &filter,
        "-map",
        "0:v",
        "-map",
        "[a]",
        "-c:v",
        "copy",
        "-c:a",
        "aac",
        "-movflags",
        "+faststart",
        output.to_str().ok_or("non-utf8 output path")?,
    ]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    detach_group(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg spawn ({}): install ffmpeg", e))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let stderr_capture: LineBuf = Arc::new(Mutex::new(VecDeque::new()));
    let cap_for_thread = stderr_capture.clone();
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, dur, None, "Mixing music".to_string())));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, dur, Some(cap_for_thread), "Mixing music".to_string())));
    let status = wait_or_cancel(&mut child, cancel)?;
    if let Some(h) = h_o { let _ = h.join(); }
    if let Some(h) = h_e { let _ = h.join(); }
    if !status.success() {
        let detail = summarize_yt_dlp_failure(&stderr_capture);
        let suffix = if detail.is_empty() { String::new() } else { format!(" — {}", detail) };
        return Err(format!("ffmpeg music mix exited with {:?}{}", status.code(), suffix));
    }
    Ok(())
}

/// Silence the given `(from, till)` ranges (seconds, relative to the start of
/// `input`; may overlap). Only the audio is filtered (`volume=0` inside the
/// ranges, timeline-enabled) and re-encoded — the video stream is copied
/// bit-for-bit, so this runs at remux speed regardless of resolution.
fn apply_mute(
    input: &std::path::Path,
    output: &std::path::Path,
    ranges: &[(f64, f64)],
    dur: f64,
    tx: &Sender<Event>,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let _ = tx.send(Event::Progress {
        phase: "Muting sound".into(),
        fraction: 0.0,
        detail: String::new(),
    });
    // The single quotes are ffmpeg filter-parser quoting: they keep the commas
    // inside between() from being read as filter-option separators.
    let expr = ranges
        .iter()
        .map(|(a, b)| format!("between(t,{a},{b})"))
        .collect::<Vec<_>>()
        .join("+");
    let filter = format!("volume=enable='{}':volume=0", expr);

    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-y",
        "-i",
        input.to_str().ok_or("non-utf8 input path")?,
        "-c:v",
        "copy",
        "-af",
        &filter,
        "-c:a",
        "aac",
        "-movflags",
        "+faststart",
        output.to_str().ok_or("non-utf8 output path")?,
    ]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    detach_group(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg spawn ({}): install ffmpeg", e))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let stderr_capture: LineBuf = Arc::new(Mutex::new(VecDeque::new()));
    let cap_for_thread = stderr_capture.clone();
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, dur, None, "Muting sound".to_string())));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, dur, Some(cap_for_thread), "Muting sound".to_string())));
    let status = wait_or_cancel(&mut child, cancel)?;
    if let Some(h) = h_o { let _ = h.join(); }
    if let Some(h) = h_e { let _ = h.join(); }
    if !status.success() {
        let detail = summarize_yt_dlp_failure(&stderr_capture);
        let suffix = if detail.is_empty() { String::new() } else { format!(" — {}", detail) };
        return Err(format!("ffmpeg mute exited with {:?}{}", status.code(), suffix));
    }
    Ok(())
}

/// Remove one or more sections (each `(rel_from, rel_till)` in seconds,
/// relative to the start of `input`, **sorted and non-overlapping**) and
/// concatenate everything that remains into `output`. Uses ffmpeg's
/// trim/concat filter graph — arbitrary cut points rarely land on keyframes,
/// so this re-encodes (hardware H.264 on macOS, libx264 elsewhere).
/// `progress_secs` is the expected *output* duration, used only to drive the
/// progress bar.
fn apply_cut_middle(
    input: &std::path::Path,
    output: &std::path::Path,
    ranges: &[(f64, f64)],
    dur: f64,
    tx: &Sender<Event>,
    progress_secs: f64,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let _ = tx.send(Event::Progress {
        phase: "Cutting out section".into(),
        fraction: 0.0,
        detail: String::new(),
    });

    // Build the list of kept intervals: [0, cut1_from], [cut1_till, cut2_from],
    // …, [lastCut_till, dur]. Skip any *empty* interval so a cut that touches
    // the segment start or end (or two abutting cuts) doesn't emit a
    // zero-length concat part (ffmpeg rejects those / renders garbage). Trim
    // each kept interval for video and audio, resetting timestamps to 0 so
    // concat joins them seamlessly, then concat all kept parts in order.
    let mut boundaries: Vec<(f64, f64)> = Vec::new();
    let mut cursor = 0.0_f64;
    for &(from, till) in ranges {
        if from - cursor > 0.05 {
            boundaries.push((cursor, from));
        }
        cursor = till;
    }
    if dur - cursor > 0.05 {
        boundaries.push((cursor, dur));
    }
    if boundaries.is_empty() {
        return Err("nothing left to keep after the cut-outs".into());
    }

    let mut parts = String::new();
    let mut concat_inputs = String::new();
    for (i, &(seg_start, seg_end)) in boundaries.iter().enumerate() {
        // Video/audio trim for kept part i.
        let vtrim = format!("trim=start={seg_start}:end={seg_end}");
        let atrim = format!("atrim=start={seg_start}:end={seg_end}");
        parts.push_str(&format!(
            "[0:v]{vtrim},setpts=PTS-STARTPTS[v{i}];[0:a]{atrim},asetpts=PTS-STARTPTS[a{i}];",
        ));
        concat_inputs.push_str(&format!("[v{i}][a{i}]"));
    }
    let n = boundaries.len();
    let filter = format!("{parts}{concat_inputs}concat=n={n}:v=1:a=1[v][a]");

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y");
    cmd.args(hwaccel_decode_args());
    cmd.args([
        "-i",
        input.to_str().ok_or("non-utf8 input path")?,
        "-filter_complex",
        &filter,
        "-map",
        "[v]",
        "-map",
        "[a]",
    ]);
    cmd.args(video_encoder_args());
    cmd.args([
        "-c:a",
        "aac",
        "-movflags",
        "+faststart",
        output.to_str().ok_or("non-utf8 output path")?,
    ]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    detach_group(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg spawn ({}): install ffmpeg", e))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let stderr_capture: LineBuf = Arc::new(Mutex::new(VecDeque::new()));
    let cap_for_thread = stderr_capture.clone();
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, progress_secs, None, "Cutting out sections".to_string())));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, progress_secs, Some(cap_for_thread), "Cutting out sections".to_string())));
    let status = wait_or_cancel(&mut child, cancel)?;
    if let Some(h) = h_o { let _ = h.join(); }
    if let Some(h) = h_e { let _ = h.join(); }
    if !status.success() {
        let detail = summarize_yt_dlp_failure(&stderr_capture);
        let suffix = if detail.is_empty() { String::new() } else { format!(" — {}", detail) };
        return Err(format!("ffmpeg cut exited with {:?}{}", status.code(), suffix));
    }
    Ok(())
}

fn apply_title_overlay(
    input: &std::path::Path,
    title: &str,
    color: [u8; 3],
    pos: [f32; 2],
    output: &std::path::Path,
    tx: &Sender<Event>,
    segment_secs: f64,
    cancel: &Arc<AtomicBool>,
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
    cmd.arg("-y");
    cmd.args(hwaccel_decode_args()); // HW-decode the (4K) video input; PNG input unaffected
    cmd.args([
        "-i",
        input.to_str().ok_or("non-utf8 input path")?,
        "-i",
        png_path.to_str().ok_or("non-utf8 png path")?,
        "-filter_complex",
        filter,
        "-c:a",
        "copy",
    ]);
    cmd.args(video_encoder_args());
    cmd.args([
        "-movflags",
        "+faststart",
        output.to_str().ok_or("non-utf8 output path")?,
    ]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    detach_group(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg spawn ({}): install ffmpeg", e))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let stderr_capture: LineBuf = Arc::new(Mutex::new(VecDeque::new()));
    let cap_for_thread = stderr_capture.clone();
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, segment_secs, None, "Burning in title".to_string())));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, segment_secs, Some(cap_for_thread), "Burning in title".to_string())));
    let status = wait_or_cancel(&mut child, cancel)?;
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
fn fade_output_path(input: &std::path::Path, fade_secs: u32, variant: &str) -> PathBuf {
    let parent = input.parent().unwrap_or_else(|| std::path::Path::new("."));
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("segment");
    parent.join(format!("{}_fadeout{}{}.mp4", stem, fade_secs, variant))
}

/// Cache discriminator for a fade-out variant. Empty when the ending is the
/// classic fade-to-black with no end card (so pre-1.0.59 cache filenames stay
/// valid); otherwise a short tag encoding the "hold bright" flag and a hash of
/// the end-card text + colour + position, so changing any of those re-encodes
/// instead of serving a stale file.
fn fade_variant_tag(hold_bright: bool, end_text: &str, color: [u8; 3], pos: [f32; 2]) -> String {
    use std::hash::{Hash, Hasher};
    let mut tag = String::new();
    if hold_bright {
        tag.push('b');
    }
    if !end_text.trim().is_empty() {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        end_text.trim().hash(&mut h);
        color.hash(&mut h);
        pos[0].to_bits().hash(&mut h);
        pos[1].to_bits().hash(&mut h);
        tag.push_str(&format!("t{:x}", h.finish()));
    }
    tag
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

/// Hold the last frame of `input` for `fade_secs` seconds ("prolong"), so the
/// short ends up N seconds longer. The held picture either fades to black
/// (`hold_bright == false`, the original behaviour) or stays at full brightness
/// (`hold_bright == true`) — a still end-frame Jürg can leave up. When
/// `end_card` is given, its text is rendered to a frame-sized PNG (reusing the
/// title renderer) and composited **only over the held tail**, so it appears as
/// an end card; on the fade-to-black variant it darkens along with the frame.
///
/// The **sound keeps running underneath the held frame and fades out with it**
/// in both variants: `tail_audio` (a clip of the source's audio from the cut
/// point onward, fetched by the caller) is concatenated after the input's own
/// audio and the whole thing fades out over the held window. If no continuing
/// audio is available (cut at the end of the source, or the input has no audio)
/// we fall back to fading the input's own last N s of sound and holding silence.
fn apply_fade_out(
    input: &std::path::Path,
    output: &std::path::Path,
    tx: &Sender<Event>,
    segment_secs: f64,
    fade_secs: u32,
    tail_audio: Option<&std::path::Path>,
    cancel: &Arc<AtomicBool>,
    hold_bright: bool,
    end_card: Option<(&str, [u8; 3], [f32; 2])>,
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

    // Render the end-card text (if any) to a frame-sized transparent PNG, the
    // same way the title overlay does — so no `drawtext`/libfreetype needed.
    let end_png: Option<PathBuf> = if let Some((text, color, pos)) = end_card {
        let (video_w, video_h) = probe_video_size(input)?;
        let p = std::env::temp_dir()
            .join(format!("create_shorts_endcard_{}.png", std::process::id()));
        render_title_png(text, color, pos, video_w, video_h, &p)?;
        let _ = tx.send(Event::Log(format!("End-card text rendered: {:?}", text)));
        Some(p)
    } else {
        None
    };

    let _ = tx.send(Event::Log(format!(
        "Freeze-frame {}: holding last frame {:.1}s–{:.1}s ({})",
        if hold_bright { "hold (bright)" } else { "fade-out" },
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

    // Video core: clone the last frame for `fade` seconds. Fading it to black is
    // optional (skipped for the "hold bright" ending). Used directly on the
    // simple `-vf` path (no end card, no continuing audio).
    let mut vf = format!("tpad=stop_mode=clone:stop_duration={fade}", fade = fade);
    if !hold_bright {
        vf.push_str(&format!(
            ",fade=t=out:st={start:.3}:d={fade}",
            start = freeze_start,
            fade = fade,
        ));
    }

    let _ = tx.send(Event::Progress {
        phase: "Adding fade-out".into(),
        fraction: 0.0,
        detail: String::new(),
    });

    // An end-card overlay needs a second video input, and the tail-audio path
    // already needs `-filter_complex`; either forces the complex branch.
    let complex = use_tail || end_png.is_some();

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y");
    cmd.args(hwaccel_decode_args()); // HW-decode the (4K) video input
    cmd.arg("-i")
        .arg(input.to_str().ok_or("non-utf8 input path")?);
    // Input indices: 0 = video; then (if present) the end-card PNG; then (if
    // used) the continuing tail audio. Assigned in the same order they're added.
    let mut next_idx = 1u32;
    let png_idx = if let Some(p) = &end_png {
        cmd.arg("-i").arg(p.to_str().ok_or("non-utf8 png path")?);
        let i = next_idx;
        next_idx += 1;
        Some(i)
    } else {
        None
    };
    let tail_idx = if use_tail {
        cmd.arg("-i")
            .arg(tail_audio.unwrap().to_str().ok_or("non-utf8 tail path")?);
        let i = next_idx;
        next_idx += 1;
        Some(i)
    } else {
        None
    };
    let _ = next_idx;

    if complex {
        // Build the video chain step by step, threading the current label:
        // tpad → (overlay end card over the held window) → (fade to black).
        let mut fc = String::new();
        let mut cur = String::from("0:v");
        fc.push_str(&format!(
            "[{cur}]tpad=stop_mode=clone:stop_duration={fade}[v0];",
            cur = cur,
            fade = fade,
        ));
        cur = String::from("v0");
        if let Some(pi) = png_idx {
            fc.push_str(&format!(
                "[{cur}][{pi}:v]overlay=0:0:enable='between(t,{s:.3},{e:.3})'[v1];",
                cur = cur,
                pi = pi,
                s = freeze_start,
                e = freeze_start + fade,
            ));
            cur = String::from("v1");
        }
        if !hold_bright {
            fc.push_str(&format!(
                "[{cur}]fade=t=out:st={s:.3}:d={fade}[v2];",
                cur = cur,
                s = freeze_start,
                fade = fade,
            ));
            cur = String::from("v2");
        }
        let vlabel = format!("[{}]", cur);
        // Audio: same as before — concat the continuing tail then fade, or fade
        // the input's own tail and pad silence; drop audio entirely if none.
        let have_audio = if use_tail {
            let ti = tail_idx.unwrap();
            fc.push_str(&format!(
                "[0:a]aresample=async=1,aformat=sample_rates=48000:channel_layouts=stereo[a0];\
                 [{ti}:a]aresample=async=1,aformat=sample_rates=48000:channel_layouts=stereo[a1];\
                 [a0][a1]concat=n=2:v=0:a=1[acat];\
                 [acat]afade=t=out:st={start:.3}:d={fade}[a];",
                ti = ti,
                start = freeze_start,
                fade = fade,
            ));
            true
        } else if has_audio {
            let afade_start = (freeze_start - fade).max(0.0);
            fc.push_str(&format!(
                "[0:a]afade=t=out:st={s:.3}:d={fade},apad=pad_dur={fade}[a];",
                s = afade_start,
                fade = fade,
            ));
            true
        } else {
            false
        };
        let fc = fc.trim_end_matches(';').to_string();
        cmd.arg("-filter_complex").arg(&fc).arg("-map").arg(&vlabel);
        if have_audio {
            cmd.args(["-map", "[a]"]);
        } else {
            cmd.arg("-an");
        }
    } else if has_audio {
        // No continuation, no end card: fade the input's own last N s of sound,
        // then pad silence to fill the held frame.
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
    cmd.args(video_encoder_args());
    if has_audio {
        cmd.args(["-c:a", "aac"]);
    }
    cmd.args(["-movflags", "+faststart"]);
    cmd.arg(output.to_str().ok_or("non-utf8 output path")?);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    // Progress is measured against the held total length so the bar reaches
    // 100% at the real end of the encode.
    let total = freeze_start + fade;
    detach_group(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg spawn ({}): install ffmpeg", e))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let stderr_capture: LineBuf = Arc::new(Mutex::new(VecDeque::new()));
    let cap_for_thread = stderr_capture.clone();
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, total, None, "Adding fade-out".to_string())));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, total, Some(cap_for_thread), "Adding fade-out".to_string())));
    let status = wait_or_cancel(&mut child, cancel);
    if let Some(h) = h_o { let _ = h.join(); }
    if let Some(h) = h_e { let _ = h.join(); }
    // Drop the temp end-card PNG on every exit path (success, error, cancel).
    if let Some(p) = &end_png {
        let _ = std::fs::remove_file(p);
    }
    let status = status?;
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
    cancel: &Arc<AtomicBool>,
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
    cmd.arg("-y");
    cmd.args(hwaccel_decode_args()); // HW-decode the (4K) video input
    cmd.arg("-i")
        .arg(input.to_str().ok_or("non-utf8 input path")?)
        .arg("-vf")
        .arg(&vf);
    if has_audio {
        cmd.arg("-af").arg(&af);
    } else {
        cmd.arg("-an");
    }
    cmd.args(video_encoder_args());
    if has_audio {
        cmd.args(["-c:a", "aac"]);
    }
    cmd.args(["-movflags", "+faststart"]);
    cmd.arg(output.to_str().ok_or("non-utf8 output path")?);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    // Progress against the padded total length so the bar reaches 100% at the
    // real end of the encode.
    let total = if dur > 0.0 { dur + fade } else { segment_secs.max(0.0) + fade };
    detach_group(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg spawn ({}): install ffmpeg", e))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let stderr_capture: LineBuf = Arc::new(Mutex::new(VecDeque::new()));
    let cap_for_thread = stderr_capture.clone();
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, total, None, "Adding fade-in".to_string())));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, total, Some(cap_for_thread), "Adding fade-in".to_string())));
    let status = wait_or_cancel(&mut child, cancel)?;
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
    cancel: &Arc<AtomicBool>,
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
    cmd.arg("-y");
    cmd.args(hwaccel_decode_args()); // HW-decode the (4K) video input
    cmd.arg("-i")
        .arg(input.to_str().ok_or("non-utf8 input path")?)
        .arg("-vf")
        .arg(&vf);
    if has_audio {
        cmd.arg("-af").arg(&af);
    } else {
        cmd.arg("-an");
    }
    cmd.args(video_encoder_args());
    if has_audio {
        cmd.args(["-c:a", "aac"]);
    }
    cmd.args(["-movflags", "+faststart"]);
    cmd.arg(output.to_str().ok_or("non-utf8 output path")?);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    // Progress against the stretched (target) length so the bar reaches
    // 100% at the real end of the encode.
    detach_group(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg spawn ({}): install ffmpeg", e))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let tx_o = tx.clone();
    let tx_e = tx.clone();
    let stderr_capture: LineBuf = Arc::new(Mutex::new(VecDeque::new()));
    let cap_for_thread = stderr_capture.clone();
    let h_o = stdout.map(|r| std::thread::spawn(move || stream_progress(r, tx_o, target, None, "Stretching clip".to_string())));
    let h_e = stderr.map(|r| std::thread::spawn(move || stream_progress(r, tx_e, target, Some(cap_for_thread), "Stretching clip".to_string())));
    let status = wait_or_cancel(&mut child, cancel)?;
    if let Some(h) = h_o { let _ = h.join(); }
    if let Some(h) = h_e { let _ = h.join(); }
    if !status.success() {
        let detail = summarize_yt_dlp_failure(&stderr_capture);
        let suffix = if detail.is_empty() { String::new() } else { format!(" — {}", detail) };
        return Err(format!("ffmpeg stretch exited with {:?}{}", status.code(), suffix));
    }
    Ok(())
}

pub fn extract_video_id(input: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_timestamp_round_trips_through_parse() {
        // Exact renderings, all parseable by parse_timestamp.
        assert_eq!(format_timestamp(0.0), "0:00.0");
        assert_eq!(format_timestamp(83.5), "1:23.5");
        assert_eq!(format_timestamp(348.0), "5:48.0");
        assert_eq!(format_timestamp(3723.2), "1:02:03.2");
        // Round trip within the 0.1s the format keeps.
        for secs in [0.0, 12.3, 59.9, 60.0, 599.4, 3599.9, 3600.0, 7345.6] {
            let back = parse_timestamp(&format_timestamp(secs)).expect("parses back");
            assert!((back - secs).abs() < 0.05001, "{} -> {}", secs, back);
        }
    }

    #[test]
    fn parse_timestamp_accepts_period_and_comma_decimals() {
        // Whole seconds.
        assert_eq!(parse_timestamp("14:07"), Some(847.0));
        // Canonical style mm:ss.s.
        assert_eq!(parse_timestamp("13:50.6"), Some(830.6));
        // Period decimal.
        assert_eq!(parse_timestamp("11:38.5"), Some(698.5));
        // Comma decimal (Swiss/German locale) — the regression this guards.
        assert_eq!(parse_timestamp("11:38,5"), Some(698.5));
        assert_eq!(parse_timestamp("9:47,5"), Some(587.5));
        // hh:mm:ss with a comma decimal in the seconds.
        assert_eq!(parse_timestamp("1:02:03,25"), Some(3723.25));
        // Bare seconds with a comma.
        assert_eq!(parse_timestamp("38,5"), Some(38.5));
    }

    #[test]
    fn parse_timestamp_tolerates_period_as_field_separator() {
        // Period typed where the mm:ss colon belongs: last dot stays the
        // decimal, earlier dots become field separators. `13.50.6` == `13:50.6`.
        assert_eq!(parse_timestamp("13.50.6"), Some(830.6));
        // hh.mm.ss.d all-dots.
        assert_eq!(parse_timestamp("1.02.03.25"), Some(3723.25));
    }

    #[test]
    fn parse_timestamp_rejects_garbage_and_empty() {
        // Genuinely unreadable input returns None (so callers surface a clear
        // error) instead of silently collapsing to 0.
        assert_eq!(parse_timestamp(""), None);
        assert_eq!(parse_timestamp("  "), None);
        assert_eq!(parse_timestamp("abc"), None);
        assert_eq!(parse_timestamp("13:xx"), None);
        assert_eq!(parse_timestamp("1:2:3:4"), None);
    }

    /// End-to-end check of the local-file source path: generates a 10 s test
    /// video, cuts a 2.0–5.5 s segment out of it via `extract_local_segment`
    /// and pulls the 3 s of audio after the cut via `extract_local_tail_audio`
    /// — the exact functions the 📂 Select… flow runs.
    #[test]
    #[ignore = "shells out to ffmpeg — run with `cargo test -- --ignored`"]
    fn extract_local_segment_cuts_the_requested_window() {
        let dir = std::env::temp_dir().join("cs_local_extract_test");
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("test source.mp4"); // space on purpose
        let ok = Command::new("ffmpeg")
            .args([
                "-y",
                "-f", "lavfi", "-i", "testsrc=duration=10:size=320x240:rate=25",
                "-f", "lavfi", "-i", "sine=frequency=440:duration=10",
                "-c:v", "libx264", "-c:a", "aac", "-shortest",
                src.to_str().unwrap(),
            ])
            .status()
            .expect("ffmpeg not installed")
            .success();
        assert!(ok, "test-source generation failed");

        let (tx, _rx) = crossbeam_channel::unbounded();
        let cancel = Arc::new(AtomicBool::new(false));

        let out = dir.join("seg.mp4");
        extract_local_segment(&src, &out, "0:02", "0:05,5", &tx, 3.5, &cancel).unwrap();
        let dur = probe_duration(&out).expect("segment has no duration");
        assert!((dur - 3.5).abs() < 0.3, "segment is {dur}s, expected ~3.5s");

        let tail = dir.join("tail.m4a");
        extract_local_tail_audio(&src, 5.5, 3, &tail, &tx, &cancel).unwrap();
        assert!(tail.metadata().unwrap().len() > 1024);
        // Past the end of the file there is no audio to continue with —
        // must error so the fade-out falls back to a silent hold.
        let none = dir.join("tail_none.m4a");
        assert!(extract_local_tail_audio(&src, 60.0, 3, &none, &tx, &cancel).is_err());

        // Cache id: filename-safe, stable across calls on the same file.
        let id = local_source_id(&src);
        assert!(id.starts_with("local_test_source_"), "unexpected id {id}");
        assert_eq!(id, local_source_id(&src));

        let _ = std::fs::remove_dir_all(&dir);
    }
}

