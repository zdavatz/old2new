//! `create_shorts_gui` — desktop app that drives the same pipeline as
//! the `create_short` CLI: yt-dlp segment extract → resumable upload to
//! YouTube. One main form, a Settings dialog for the Google OAuth
//! client_id/secret, and a one-time browser sign-in to mint a refresh
//! token.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod browsers;
mod davaz;
mod deps;
mod history;
mod installer;
mod oauth;
mod pdf;
mod pipeline;
mod shorts;
mod settings;
mod update;
mod whatsapp;
mod youtube;

use crossbeam_channel::{unbounded, Receiver, Sender};
use eframe::egui::{self, RichText};
use pipeline::{Event, Job};
use settings::{log_path, token_path, Settings};
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const MAX_LOG_LINES: usize = 5000;

const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

fn load_icon() -> Option<egui::IconData> {
    let img = image::load_from_memory(ICON_PNG).ok()?;
    let resized = img.resize_exact(256, 256, image::imageops::FilterType::Lanczos3);
    let rgba = resized.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(egui::IconData { rgba: rgba.into_raw(), width: w, height: h })
}

/// macOS .app bundles launched from Finder inherit a stripped PATH
/// (`/usr/bin:/bin:/usr/sbin:/sbin`) that excludes Homebrew's bin
/// directories, so `Command::new("yt-dlp")` etc. silently fail to
/// resolve even when the tools are installed. Prepend the standard
/// Homebrew locations so every later `Command::new` call finds them.
fn ensure_homebrew_on_path() {
    if !cfg!(target_os = "macos") { return; }
    let cur = std::env::var("PATH").unwrap_or_default();
    let mut prefix = String::new();
    for d in ["/opt/homebrew/bin", "/opt/homebrew/sbin", "/usr/local/bin"] {
        if !cur.split(':').any(|p| p == d) && std::path::Path::new(d).exists() {
            if !prefix.is_empty() { prefix.push(':'); }
            prefix.push_str(d);
        }
    }
    if !prefix.is_empty() {
        let new_path = if cur.is_empty() { prefix } else { format!("{}:{}", prefix, cur) };
        std::env::set_var("PATH", new_path);
    }
}

/// Open the split preview: the raw downloaded segment (`original`, top) and
/// the final edited clip (`edited`, bottom) as two stacked video windows so
/// Jürg can play/stop the original to read its timeline and always see the
/// edited version below it.
///
/// On macOS we drive QuickTime Player via AppleScript so the two windows tile
/// top/bottom of the screen — each keeps its own scrubber, timecode and
/// independent play/stop. If automation is unavailable (or the script fails)
/// we fall back to opening both in the default player, unpositioned. On other
/// platforms we just open both. When no edits were applied the two paths are
/// equal and we open a single window.
fn open_split_preview(original: &std::path::Path, edited: &std::path::Path) -> Result<(), String> {
    if original == edited {
        return open::that(edited).map_err(|e| e.to_string());
    }

    #[cfg(target_os = "macos")]
    {
        let script = quicktime_split_script(original, edited);
        match std::process::Command::new("osascript").arg("-e").arg(&script).status() {
            Ok(s) if s.success() => return Ok(()),
            _ => { /* automation denied / QuickTime missing → fall through */ }
        }
    }

    // Fallback: open both in the default player (order matters little since we
    // can't position them). Report the first failure but attempt both.
    let mut err: Option<String> = None;
    if let Err(e) = open::that(original) {
        err = Some(format!("original: {}", e));
    }
    if let Err(e) = open::that(edited) {
        err = Some(match err {
            Some(prev) => format!("{}; edited: {}", prev, e),
            None => format!("edited: {}", e),
        });
    }
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Build the AppleScript that opens both clips in QuickTime Player and tiles
/// them: `original` fills the top half of the screen, `edited` the bottom.
/// Positioning is wrapped in `try` so a resize hiccup still leaves both
/// windows open. We close QuickTime's existing windows first so repeat
/// previews don't pile up and the window indices stay predictable
/// (window 2 = original opened first, window 1 = edited opened last).
#[cfg(target_os = "macos")]
fn quicktime_split_script(original: &std::path::Path, edited: &std::path::Path) -> String {
    format!(
        r#"set topFile to POSIX file {orig}
set botFile to POSIX file {edit}
tell application "QuickTime Player"
  activate
  try
    close every window
  end try
  open topFile
  open botFile
end tell
delay 0.5
try
  tell application "Finder" to set deskBounds to bounds of window of desktop
  set scrW to item 3 of deskBounds
  set scrH to item 4 of deskBounds
  set midY to scrH div 2
  tell application "QuickTime Player"
    set bounds of window 2 to {{0, 25, scrW, midY}}
    set bounds of window 1 to {{0, midY, scrW, scrH}}
  end tell
end try"#,
        orig = applescript_quote(original),
        edit = applescript_quote(edited),
    )
}

/// Quote a path as an AppleScript string literal (escape `\` and `"`).
#[cfg(target_os = "macos")]
fn applescript_quote(p: &std::path::Path) -> String {
    let s = p.to_string_lossy();
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{}\"", escaped)
}

/// Number of most-recent shorts the PDF export includes.
const PDF_SHORTS_COUNT: usize = 10;

/// Headless `--export-pdf [path]`: write the latest-shorts PDF and exit.
/// Lets the same binary serve the GUI and a scriptable CLI.
fn run_export_pdf_cli(args: &[String]) -> ! {
    let pos = args.iter().position(|a| a == "--export-pdf").unwrap();
    let settings = Settings::load();
    let (rows, note) = shorts::latest_rows(&settings, PDF_SHORTS_COUNT);
    eprintln!("Source: {note}");
    let newest = rows.first().map(|r| r.title.clone()).unwrap_or_default();
    let out = args
        .get(pos + 1)
        .filter(|s| !s.starts_with("--"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| pdf::default_output_path(rows.len().max(1), &newest));
    match pdf::export(&rows, &out) {
        Ok(n) => {
            println!("Wrote {} ({} short{})", out.display(), n, if n == 1 { "" } else { "s" });
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("export-pdf failed: {e}");
            std::process::exit(1);
        }
    }
}

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--export-pdf") {
        run_export_pdf_cli(&args);
    }
    ensure_homebrew_on_path();
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(format!("create_shorts v{}", APP_VERSION))
        .with_inner_size([900.0, 720.0])
        .with_min_inner_size([700.0, 500.0]);
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "create_shorts_gui",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct FormState {
    #[serde(default)] source: String,
    #[serde(default)] start: String,
    #[serde(default)] end: String,
    #[serde(default)] title: String,
    #[serde(default)] description: String,
    #[serde(default)] privacy: String,
    #[serde(default)] overlay_title: bool,
    #[serde(default = "default_overlay_color")] overlay_color: [u8; 3],
    // Normalized title position: [x, y] in 0.0..=1.0 (0,0 = top-left,
    // 1,1 = bottom-right). Drives where the title block is placed in the
    // frame. Default [0,1] = bottom-left, matching the original overlay.
    #[serde(default = "default_overlay_pos")] overlay_pos: [f32; 2],
    #[serde(default)] stretch: bool,
    #[serde(default = "default_stretch_secs")] stretch_secs: u32,
    #[serde(default = "default_fade_out")] fade_out: bool,
    #[serde(default = "default_fade_secs")] fade_secs: u32,
    #[serde(default)] fade_in: bool,
    #[serde(default = "default_fade_secs")] fade_in_secs: u32,
    // Remove one or more middle sections from the extracted segment: each
    // `CutRange` (from/till, same timestamp style as start/end, absolute in
    // the source video) is deleted and the remaining parts joined into one
    // clip.
    #[serde(default)] cut_middle: bool,
    #[serde(default = "default_cuts")] cuts: Vec<CutRange>,
}

/// One "remove this middle section" range. Timestamps are strings in the
/// same mm:ss / hh:mm:ss style as start/end (absolute in the source video).
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct CutRange {
    #[serde(default)] from: String,
    #[serde(default)] till: String,
}

/// Fresh forms start with one empty cut row so the fields are visible the
/// moment "Remove middle section(s)" is ticked.
fn default_cuts() -> Vec<CutRange> { vec![CutRange::default()] }

fn default_overlay_color() -> [u8; 3] { [255, 255, 255] }
fn default_overlay_pos() -> [f32; 2] { [0.0, 1.0] }
fn default_fade_out() -> bool { true }
fn default_fade_secs() -> u32 { 10 }
fn default_stretch_secs() -> u32 { 5 }

impl Default for FormState {
    fn default() -> Self {
        Self {
            source: String::new(),
            start: String::new(),
            end: String::new(),
            title: String::new(),
            description: String::new(),
            privacy: String::new(),
            overlay_title: false,
            overlay_color: default_overlay_color(),
            overlay_pos: default_overlay_pos(),
            stretch: false,
            stretch_secs: default_stretch_secs(),
            fade_out: default_fade_out(),
            fade_secs: default_fade_secs(),
            fade_in: false,
            fade_in_secs: default_fade_secs(),
            cut_middle: false,
            cuts: default_cuts(),
        }
    }
}

struct App {
    settings: Settings,
    form: FormState,
    log: Arc<Mutex<Vec<String>>>,
    progress: Arc<Mutex<ProgressInfo>>,
    rx: Option<Receiver<Event>>,
    running: bool,
    /// Shared cancel flag for the main create-short job. Set true by the
    /// Cancel button; the worker polls it, kills any running yt-dlp/ffmpeg
    /// child, and exits with `Event::Cancelled`. A fresh `Arc` is minted per
    /// job in `kick_off` so a previous cancel can't bleed into the next run.
    cancel: Arc<AtomicBool>,
    last_done_url: Option<String>,
    last_error: Option<String>,
    show_settings: bool,
    signing_in: bool,
    signin_rx: Option<Receiver<SignInEvent>>,
    signed_in: bool,
    icon_texture: Option<egui::TextureHandle>,
    deps: deps::DepStatus,
    brew_installing: bool,
    brew_rx: Option<Receiver<BrewEvent>>,
    ytdlp_updating: bool,
    ytdlp_update_rx: Option<Receiver<BrewEvent>>,
    pdf_exporting: bool,
    pdf_rx: Option<Receiver<Result<(std::path::PathBuf, usize, String), String>>>,
    /// Fetching the shorts list (network) for the preview modal.
    pdf_fetching: bool,
    pdf_fetch_rx: Option<Receiver<Result<(Vec<pdf::PdfRow>, String), String>>>,
    /// Preview modal: the full history of shorts, each with a checkbox so the
    /// user picks which go into the PDF. `pdf_preview_selected` is parallel to
    /// `pdf_preview_rows`; the newest `PDF_SHORTS_COUNT` are selected by default.
    pdf_preview_open: bool,
    pdf_preview_rows: Vec<pdf::PdfRow>,
    pdf_preview_selected: Vec<bool>,
    pdf_preview_note: String,
    update_rx: Option<Receiver<Option<update::UpdateInfo>>>,
    update_info: Option<update::UpdateInfo>,
    update_checking: bool,
    update_status_msg: Option<String>,
    cookies_testing: bool,
    cookies_test_rx: Option<Receiver<Result<String, String>>>,
    cookies_status_msg: Option<String>,
    detected_browsers: Vec<&'static str>,
    installing: bool,
    install_rx: Option<Receiver<installer::InstallEvent>>,
    install_progress: Arc<Mutex<ProgressInfo>>,
    wa_sending: bool,
    wa_rx: Option<Receiver<Result<(), String>>>,
    wa_status_msg: Option<String>,
    wa_setup_running: bool,
    wa_setup_rx: Option<Receiver<whatsapp::SetupEvent>>,
    wa_login_running: bool,
    wa_login_rx: Option<Receiver<whatsapp::LoginEvent>>,
    wa_qr: Option<String>,
    wa_show_qr: bool,
    wa_provisioned: bool,
    wa_linked: bool,
    wa_picker_open: bool,
    wa_picker_loading: bool,
    wa_picker_items: Vec<whatsapp::Recipient>,
    wa_picker_error: Option<String>,
    wa_picker_filter: String,
    wa_picker_rx: Option<Receiver<Result<Vec<whatsapp::Recipient>, String>>>,
    davaz_posting: bool,
    davaz_rx: Option<Receiver<Result<davaz::PostResponse, String>>>,
    davaz_status_msg: Option<String>,
    /// Set once a successful POST has been confirmed for the current
    /// `last_done_url` so the button can hide and the success line
    /// stays visible until the user dismisses the banner.
    davaz_posted: bool,
    show_history: bool,
    history_filter: String,
    history_cache: Vec<history::UploadEntry>,
    show_upload: bool,
    upload_file: String,
    upload_title: String,
    upload_description: String,
    upload_privacy: String,
    upload_running: bool,
    /// Cancel flag for the direct-upload (Upload tab) job — same mechanism as
    /// `cancel`, kept separate so the two jobs can run and be cancelled
    /// independently.
    upload_cancel: Arc<AtomicBool>,
    upload_rx: Option<Receiver<Event>>,
    upload_progress: Arc<Mutex<ProgressInfo>>,
    upload_log: Arc<Mutex<Vec<String>>>,
    upload_last_done_url: Option<String>,
    upload_last_error: Option<String>,
}

enum BrewEvent {
    Log(String),
    Done,
    Error(String),
}

/// "chrome (installed)" if detected, otherwise just the name.
/// Detect YouTube's "not a bot" / "Sign in to confirm" rejection in a
/// yt-dlp error and return a one-line hint pointing the user at the
/// Settings → Cookies-from-browser dropdown. Returns None for unrelated
/// errors so we only show the hint when it's actually useful.
/// A 2D position picker: a 16:9 frame with a draggable mini-box. The box's
/// relative position inside the frame becomes the normalized title position
/// `[x, y]` (0,0 = top-left … 1,1 = bottom-right) written back into `pos`.
fn title_position_picker(ui: &mut egui::Ui, pos: &mut [f32; 2], enabled: bool) {
    // 16:9 to match the typical video frame so placement reads intuitively.
    let size = egui::vec2(160.0, 90.0);
    let sense = if enabled { egui::Sense::click_and_drag() } else { egui::Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(size, sense);

    // Drag/click moves the handle (only when the row is enabled).
    if enabled && (response.dragged() || response.clicked()) {
        if let Some(p) = response.interact_pointer_pos() {
            pos[0] = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            pos[1] = ((p.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
        }
    }

    let visuals = ui.visuals();
    let painter = ui.painter();
    // Frame.
    let bg = if enabled { visuals.extreme_bg_color } else { visuals.faint_bg_color };
    painter.rect_filled(rect, 4.0, bg);
    painter.rect_stroke(rect, 4.0, visuals.widgets.noninteractive.bg_stroke);
    // Center cross-hairs as a light guide.
    let guide = egui::Stroke::new(1.0, visuals.weak_text_color().linear_multiply(0.4));
    painter.line_segment(
        [egui::pos2(rect.center().x, rect.top()), egui::pos2(rect.center().x, rect.bottom())],
        guide,
    );
    painter.line_segment(
        [egui::pos2(rect.left(), rect.center().y), egui::pos2(rect.right(), rect.center().y)],
        guide,
    );

    // Handle (the mini "title" box).
    let handle_center = egui::pos2(
        rect.left() + pos[0] * rect.width(),
        rect.top() + pos[1] * rect.height(),
    );
    let handle = egui::Rect::from_center_size(handle_center, egui::vec2(34.0, 16.0));
    let handle = handle.translate(egui::vec2(
        // Keep the handle visually inside the frame at the extremes.
        (rect.left() - handle.left()).max(0.0) + (rect.right() - handle.right()).min(0.0),
        (rect.top() - handle.top()).max(0.0) + (rect.bottom() - handle.bottom()).min(0.0),
    ));
    let accent = if enabled { visuals.selection.bg_fill } else { visuals.widgets.inactive.bg_fill };
    painter.rect_filled(handle, 2.0, accent);
    painter.rect_stroke(handle, 2.0, egui::Stroke::new(1.0, visuals.strong_text_color()));
    painter.text(
        handle.center(),
        egui::Align2::CENTER_CENTER,
        "Title",
        egui::FontId::proportional(10.0),
        visuals.strong_text_color(),
    );

    if response.hovered() {
        response.on_hover_text("Drag to place the title. Left/right sets horizontal, up/down sets vertical position.");
    }
}

fn bot_detection_hint(err: &str, settings: &Settings) -> Option<String> {
    let lower = err.to_lowercase();
    let is_bot_block = lower.contains("not a bot")
        || lower.contains("sign in to confirm")
        || lower.contains("confirm you");
    if !is_bot_block {
        return None;
    }
    let current = settings.cookies_browser.trim();
    if current.is_empty() {
        Some(
            "YouTube wants cookies. Open Settings (app icon top-right) → Cookies from browser, \
             pick a browser you're signed into YouTube with, then click Test."
                .into(),
        )
    } else {
        Some(format!(
            "YouTube wants cookies. Currently set to `{}` but it didn't provide a valid session. \
             Open Settings → Cookies from browser, click Test, or try another browser \
             (Chrome/Brave/Firefox are most reliable; Safari needs the keychain unlocked).",
            current
        ))
    }
}

fn browser_label(name: &str, detected: &[&'static str]) -> String {
    if detected.iter().any(|d| *d == name) {
        format!("{} (installed)", name)
    } else {
        name.to_string()
    }
}

fn spawn_update_check() -> Receiver<Option<update::UpdateInfo>> {
    let (tx, rx) = unbounded::<Option<update::UpdateInfo>>();
    std::thread::spawn(move || {
        let _ = tx.send(update::check_latest(APP_VERSION));
    });
    rx
}

enum SignInEvent {
    Log(String),
    Done,
    Error(String),
}

#[derive(Default, Clone)]
struct ProgressInfo {
    phase: String,
    fraction: f32,
    detail: String,
}

fn load_persisted_log() -> Vec<String> {
    let Ok(contents) = std::fs::read_to_string(log_path()) else { return Vec::new() };
    let mut v: Vec<String> = contents.lines().map(|s| s.to_string()).collect();
    if v.len() > MAX_LOG_LINES {
        let drop = v.len() - MAX_LOG_LINES;
        v.drain(0..drop);
        let trimmed = v.join("\n");
        let _ = std::fs::write(log_path(), format!("{}\n", trimmed));
    }
    v
}

fn append_to_log_file(line: &str) -> std::io::Result<()> {
    let path = log_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{}", line)
}

fn chrono_like_now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut settings = Settings::load();
        let detected_browsers = browsers::detected();
        // First-launch convenience: if the user hasn't picked a cookie
        // browser yet, default to the most likely installed one. Saved
        // to disk only when the user clicks Save in Settings.
        if settings.cookies_browser.is_empty() {
            if let Some(first) = detected_browsers.first() {
                settings.cookies_browser = (*first).to_string();
            }
        }
        let signed_in = oauth::load_token().map(|t| !t.refresh_token.is_empty()).unwrap_or(false);
        let show_settings = settings.client_id.is_empty() || settings.client_secret.is_empty();
        let mut initial_log = load_persisted_log();
        let stamp = chrono_like_now();
        let marker = format!("─── session started {} ───", stamp);
        let _ = append_to_log_file(&marker);
        initial_log.push(marker);

        let saved_form: FormState = cc
            .storage
            .and_then(|s| eframe::get_value::<FormState>(s, "form_state"))
            .unwrap_or_default();
        let privacy = if !saved_form.privacy.is_empty() {
            saved_form.privacy.clone()
        } else if !settings.default_privacy.is_empty() {
            settings.default_privacy.clone()
        } else {
            "public".to_string()
        };
        let mut s = Self {
            form: FormState { privacy, ..saved_form },
            settings,
            log: Arc::new(Mutex::new(initial_log)),
            progress: Arc::new(Mutex::new(ProgressInfo::default())),
            rx: None,
            running: false,
            cancel: Arc::new(AtomicBool::new(false)),
            last_done_url: None,
            last_error: None,
            show_settings,
            signing_in: false,
            signin_rx: None,
            signed_in,
            icon_texture: None,
            deps: deps::DepStatus::check(),
            brew_installing: false,
            brew_rx: None,
            ytdlp_updating: false,
            ytdlp_update_rx: None,
            pdf_exporting: false,
            pdf_rx: None,
            pdf_fetching: false,
            pdf_fetch_rx: None,
            pdf_preview_open: false,
            pdf_preview_rows: Vec::new(),
            pdf_preview_selected: Vec::new(),
            pdf_preview_note: String::new(),
            update_rx: Some(spawn_update_check()),
            update_info: None,
            update_checking: true,
            update_status_msg: None,
            cookies_testing: false,
            cookies_test_rx: None,
            cookies_status_msg: None,
            detected_browsers,
            installing: false,
            install_rx: None,
            install_progress: Arc::new(Mutex::new(ProgressInfo::default())),
            wa_sending: false,
            wa_rx: None,
            wa_status_msg: None,
            wa_setup_running: false,
            wa_setup_rx: None,
            wa_login_running: false,
            wa_login_rx: None,
            wa_qr: None,
            wa_show_qr: false,
            wa_provisioned: false,
            wa_linked: false,
            wa_picker_open: false,
            wa_picker_loading: false,
            wa_picker_items: Vec::new(),
            wa_picker_error: None,
            wa_picker_filter: String::new(),
            wa_picker_rx: None,
            davaz_posting: false,
            davaz_rx: None,
            davaz_status_msg: None,
            davaz_posted: false,
            show_history: false,
            history_filter: String::new(),
            history_cache: Vec::new(),
            show_upload: false,
            upload_file: String::new(),
            upload_title: String::new(),
            upload_description: String::new(),
            upload_privacy: "public".to_string(),
            upload_running: false,
            upload_cancel: Arc::new(AtomicBool::new(false)),
            upload_rx: None,
            upload_progress: Arc::new(Mutex::new(ProgressInfo::default())),
            upload_log: Arc::new(Mutex::new(Vec::new())),
            upload_last_done_url: None,
            upload_last_error: None,
        };
        s.refresh_wa_status();
        // Auto-*check* yt-dlp freshness on launch (the deps probe above
        // already read its version). We only *log* staleness here and
        // surface a one-click "Update yt-dlp" banner — we deliberately do
        // NOT auto-run the update, because on a Homebrew install that
        // triggers a full `brew update` that pegs the machine (load spikes
        // into the hundreds) right when the user wants to work.
        if let Some(age) = s.deps.yt_dlp_age_days() {
            if s.deps.yt_dlp_is_stale() {
                s.append_log(format!(
                    "yt-dlp is {} days old (> {} days) — update recommended (see banner)",
                    age, deps::YT_DLP_STALE_DAYS
                ));
            }
        }
        s
    }

    /// Update yt-dlp in place, chosen by how it was installed (brew upgrade
    /// vs `yt-dlp -U`). Triggered by the "Update yt-dlp" banner button — a
    /// stale yt-dlp is the most common cause of "video unavailable" /
    /// bot-check download failures (YouTube changes its internals
    /// constantly). Streams output to the log; re-checks deps on completion.
    fn start_ytdlp_update(&mut self) {
        if self.ytdlp_updating { return; }
        let age = self.deps.yt_dlp_age_days().unwrap_or(0);
        let (cmd, args) = deps::yt_dlp_update_command();
        self.append_log(format!(
            "yt-dlp is {} days old — updating ({} {})",
            age, cmd, args.join(" ")
        ));
        let (tx, rx) = unbounded::<BrewEvent>();
        self.ytdlp_update_rx = Some(rx);
        self.ytdlp_updating = true;
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            use std::process::{Command, Stdio};
            let mut c = Command::new(&cmd);
            for a in &args { c.arg(a); }
            c.stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = match c.spawn() {
                Ok(c) => c,
                Err(e) => { let _ = tx.send(BrewEvent::Error(format!("yt-dlp update spawn failed: {}", e))); return; }
            };
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let tx_o = tx.clone();
            let tx_e = tx.clone();
            let h_out = stdout.map(|r| std::thread::spawn(move || {
                let mut buf = BufReader::new(r);
                let mut line = String::new();
                while buf.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                    let _ = tx_o.send(BrewEvent::Log(line.trim_end_matches(['\r','\n']).to_string()));
                    line.clear();
                }
            }));
            let h_err = stderr.map(|r| std::thread::spawn(move || {
                let mut buf = BufReader::new(r);
                let mut line = String::new();
                while buf.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                    let _ = tx_e.send(BrewEvent::Log(line.trim_end_matches(['\r','\n']).to_string()));
                    line.clear();
                }
            }));
            let status = child.wait();
            if let Some(h) = h_out { let _ = h.join(); }
            if let Some(h) = h_err { let _ = h.join(); }
            match status {
                Ok(s) if s.success() => { let _ = tx.send(BrewEvent::Done); }
                Ok(s) => { let _ = tx.send(BrewEvent::Error(format!("yt-dlp update exited with {:?}", s.code()))); }
                Err(e) => { let _ = tx.send(BrewEvent::Error(format!("yt-dlp update wait failed: {}", e))); }
            }
        });
    }

    fn refresh_wa_status(&mut self) {
        let dir = self.settings.whatsapp_dir.trim();
        if dir.is_empty() {
            self.wa_provisioned = false;
            self.wa_linked = false;
            return;
        }
        let p = std::path::Path::new(dir);
        self.wa_provisioned = whatsapp::is_provisioned(p);
        self.wa_linked = whatsapp::is_linked(p);
    }

    fn start_wa_setup(&mut self) {
        if self.wa_setup_running { return; }
        // If user hasn't picked a dir, default to the managed one under config.
        if self.settings.whatsapp_dir.trim().is_empty() {
            self.settings.whatsapp_dir = settings::managed_whatsapp_dir().to_string_lossy().into_owned();
        }
        let dir = std::path::PathBuf::from(self.settings.whatsapp_dir.trim());
        // Always (re-)write the bundled scripts so updates land even if
        // node_modules already exists.
        if let Err(e) = whatsapp::refresh_scripts(&dir) {
            // Not fatal — setup will (re-)write them anyway.
            self.append_log(format!("refresh_scripts: {}", e));
        }
        self.append_log(format!("WhatsApp setup → {}", dir.display()));
        self.wa_status_msg = Some("Setup running…".into());
        self.wa_setup_running = true;
        let (tx, rx) = unbounded::<whatsapp::SetupEvent>();
        self.wa_setup_rx = Some(rx);
        std::thread::spawn(move || whatsapp::setup(dir, tx));
    }

    fn start_wa_login(&mut self) {
        if self.wa_login_running { return; }
        let dir_str = self.settings.whatsapp_dir.trim().to_string();
        if dir_str.is_empty() {
            self.wa_status_msg = Some("Set WhatsApp dir first.".into());
            return;
        }
        let dir = std::path::PathBuf::from(&dir_str);
        if !whatsapp::is_provisioned(&dir) {
            self.wa_status_msg = Some("Run 'Setup WhatsApp' first.".into());
            return;
        }
        self.wa_qr = None;
        self.wa_show_qr = true;
        self.wa_status_msg = Some("Generating QR code…".into());
        self.append_log(format!("WhatsApp link → {}", dir.display()));
        self.wa_login_running = true;
        let (tx, rx) = unbounded::<whatsapp::LoginEvent>();
        self.wa_login_rx = Some(rx);
        std::thread::spawn(move || whatsapp::login(dir, tx));
    }

    fn start_wa_send(&mut self, url: String) {
        if self.wa_sending { return; }
        let title = self.form.title.trim().to_string();
        let message = if title.is_empty() { url.clone() } else { format!("{}\n{}", title, url) };
        let dir = self.settings.whatsapp_dir.clone();
        let recipient = self.settings.whatsapp_recipient.clone();
        if dir.trim().is_empty() {
            self.wa_status_msg = Some("WhatsApp dir not set — open Settings.".into());
            return;
        }
        if recipient.trim().is_empty() {
            self.wa_status_msg = Some("WhatsApp recipient not set — open Settings.".into());
            return;
        }
        self.append_log(format!("Sending YouTube link to WhatsApp recipient {}…", recipient));
        self.wa_status_msg = Some(format!("Sending to {}…", recipient));
        self.wa_sending = true;
        let (tx, rx) = unbounded::<Result<(), String>>();
        self.wa_rx = Some(rx);
        std::thread::spawn(move || {
            let result = whatsapp::send_text(&dir, &recipient, &message)
                .map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
    }

    fn start_davaz_post(&mut self, url: String) {
        if self.davaz_posting { return; }
        let token = self.settings.davaz_token.trim().to_string();
        if token.is_empty() {
            self.davaz_status_msg = Some("davaz.com token not set — open Settings.".into());
            return;
        }
        self.append_log(format!("Posting {} to davaz.com…", url));
        self.davaz_status_msg = Some("Posting to davaz.com…".into());
        self.davaz_posting = true;
        let (tx, rx) = unbounded::<Result<davaz::PostResponse, String>>();
        self.davaz_rx = Some(rx);
        std::thread::spawn(move || {
            let result = davaz::post_video(&token, &url)
                .map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
    }

    fn start_wa_pick_recipient(&mut self) {
        if self.wa_picker_loading { return; }
        let dir = self.settings.whatsapp_dir.clone();
        if dir.trim().is_empty() {
            self.wa_picker_error = Some("Set WhatsApp dir first.".into());
            self.wa_picker_open = true;
            return;
        }
        if !self.wa_provisioned {
            self.wa_picker_error = Some("Run 'Setup WhatsApp' first.".into());
            self.wa_picker_open = true;
            return;
        }
        if !self.wa_linked {
            self.wa_picker_error = Some("Click 'Link WhatsApp' first.".into());
            self.wa_picker_open = true;
            return;
        }
        self.wa_picker_open = true;
        self.wa_picker_error = None;
        self.wa_picker_filter.clear();
        // Show cached items instantly while a fresh fetch runs in the
        // background.
        self.wa_picker_items = whatsapp::load_cached_recipients(&dir);
        self.wa_picker_loading = true;
        let (tx, rx) = unbounded::<Result<Vec<whatsapp::Recipient>, String>>();
        self.wa_picker_rx = Some(rx);
        std::thread::spawn(move || {
            let result = whatsapp::list_recipients(&dir).map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
    }

    fn start_install(&mut self) {
        if self.installing { return; }
        let Some(info) = self.update_info.clone() else { return };
        let Some(dmg_url) = info.dmg_url.clone() else {
            self.last_error = Some("This release has no macOS DMG attached yet.".into());
            return;
        };
        let Some(app) = installer::current_app_bundle() else {
            self.last_error = Some(
                "In-app update is only available when running the installed .app from /Applications. \
                 Open the release page to download manually.".into()
            );
            return;
        };
        if let Err(e) = installer::check_writable_parent(&app) {
            self.last_error = Some(format!(
                "Cannot install update in place: {}. Quit and reinstall manually from the release page.",
                e
            ));
            return;
        }

        self.last_error = None;
        if let Ok(mut p) = self.install_progress.lock() { *p = ProgressInfo::default(); }
        let (tx, rx) = unbounded::<installer::InstallEvent>();
        self.install_rx = Some(rx);
        self.installing = true;
        self.append_log(format!("Starting in-app update to {}…", info.pretty()));

        std::thread::spawn(move || {
            match installer::install_macos(&dmg_url, &app, tx.clone()) {
                Ok(()) => {
                    // Helper script is detached and waiting for our PID
                    // to die. Give the user 600 ms to read the success
                    // line, then exit so the swap can run.
                    std::thread::sleep(std::time::Duration::from_millis(600));
                    std::process::exit(0);
                }
                Err(e) => { let _ = tx.send(installer::InstallEvent::Error(e)); }
            }
        });
    }

    fn start_cookies_test(&mut self) {
        if self.cookies_testing { return; }
        let browser = self.settings.cookies_browser.trim().to_string();
        if browser.is_empty() {
            self.cookies_status_msg = Some("Pick a browser first.".into());
            return;
        }
        self.cookies_status_msg = Some(format!("Testing {} cookies…", browser));
        self.cookies_testing = true;
        let (tx, rx) = unbounded::<Result<String, String>>();
        self.cookies_test_rx = Some(rx);
        let b = browser.clone();
        std::thread::spawn(move || {
            let result = browsers::test_cookies(&b).map(|()| b.clone());
            let _ = tx.send(result);
        });
    }

    fn trigger_update_check(&mut self) {
        if self.update_checking { return; }
        self.update_status_msg = None;
        self.update_checking = true;
        self.update_rx = Some(spawn_update_check());
    }

    fn start_brew_install(&mut self) {
        if self.brew_installing { return; }
        let Some(brew) = deps::brew_path() else {
            self.last_error = Some("Homebrew not installed".into());
            return;
        };
        let missing = self.deps.missing();
        let pkgs = deps::brew_packages_for(&missing);
        if pkgs.is_empty() { return; }

        self.last_error = None;
        let (tx, rx) = unbounded::<BrewEvent>();
        self.brew_rx = Some(rx);
        self.brew_installing = true;
        self.append_log(format!("Running: {} install {}", brew, pkgs.join(" ")));

        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            use std::process::{Command, Stdio};
            let mut cmd = Command::new(&brew);
            cmd.arg("install");
            for p in &pkgs { cmd.arg(p); }
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => { let _ = tx.send(BrewEvent::Error(format!("brew spawn failed: {}", e))); return; }
            };
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let tx_o = tx.clone();
            let tx_e = tx.clone();
            let h_out = stdout.map(|r| std::thread::spawn(move || {
                let mut buf = BufReader::new(r);
                let mut line = String::new();
                while buf.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                    let _ = tx_o.send(BrewEvent::Log(line.trim_end_matches(['\r','\n']).to_string()));
                    line.clear();
                }
            }));
            let h_err = stderr.map(|r| std::thread::spawn(move || {
                let mut buf = BufReader::new(r);
                let mut line = String::new();
                while buf.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                    let _ = tx_e.send(BrewEvent::Log(line.trim_end_matches(['\r','\n']).to_string()));
                    line.clear();
                }
            }));
            let status = child.wait();
            if let Some(h) = h_out { let _ = h.join(); }
            if let Some(h) = h_err { let _ = h.join(); }
            match status {
                Ok(s) if s.success() => { let _ = tx.send(BrewEvent::Done); }
                Ok(s) => { let _ = tx.send(BrewEvent::Error(format!("brew exited with {:?}", s.code()))); }
                Err(e) => { let _ = tx.send(BrewEvent::Error(format!("brew wait failed: {}", e))); }
            }
        });
    }

    fn ensure_icon_texture(&mut self, ctx: &egui::Context) {
        if self.icon_texture.is_some() { return; }
        let Ok(img) = image::load_from_memory(ICON_PNG) else { return };
        let resized = img.resize_exact(64, 64, image::imageops::FilterType::Lanczos3);
        let rgba = resized.to_rgba8();
        let (w, h) = rgba.dimensions();
        let color_image = egui::ColorImage::from_rgba_unmultiplied(
            [w as usize, h as usize],
            rgba.as_raw(),
        );
        self.icon_texture = Some(ctx.load_texture("app-icon", color_image, egui::TextureOptions::LINEAR));
    }

    fn append_log(&self, line: String) {
        let _ = append_to_log_file(&line);
        if let Ok(mut g) = self.log.lock() {
            g.push(line);
            if g.len() > MAX_LOG_LINES {
                let drop = g.len() - MAX_LOG_LINES;
                g.drain(0..drop);
            }
        }
    }

    /// Fetch the latest-shorts list on a worker thread (the channel fetch is a
    /// network call, so we mustn't block the UI) and, when it returns, open the
    /// preview modal so the user can delete items before creating the PDF.
    /// Handled in `drain_events`.
    fn start_pdf_fetch(&mut self) {
        if self.pdf_fetching || self.pdf_exporting { return; }
        // Open the modal immediately so the window feels instant. If we have a
        // cached list, show it right away (newest 10 pre-selected) while an
        // incremental refresh (only the newest few) runs in the background.
        let cached = shorts::load_cache();
        if !cached.is_empty() {
            self.pdf_preview_selected =
                (0..cached.len()).map(|i| i < PDF_SHORTS_COUNT).collect();
            self.pdf_preview_rows = cached.clone();
            self.pdf_preview_note = "cached — checking for new…".into();
        } else {
            self.pdf_preview_rows.clear();
            self.pdf_preview_selected.clear();
            self.pdf_preview_note = "loading…".into();
        }
        self.pdf_preview_open = true;
        self.pdf_fetching = true;
        self.append_log(if cached.is_empty() {
            "📄 Fetching the full shorts history (@gozipa) — first time, this scans the channel…".into()
        } else {
            "📄 Checking @gozipa for new shorts…".to_string()
        });
        let settings = self.settings.clone();
        let (tx, rx) = unbounded::<Result<(Vec<pdf::PdfRow>, String), String>>();
        self.pdf_fetch_rx = Some(rx);
        std::thread::spawn(move || {
            let (rows, note) = shorts::refresh(&settings, &cached);
            let _ = tx.send(Ok((rows, note)));
        });
    }

    /// Export the shorts currently in the preview list (after the user's
    /// deletions) to a PDF on a worker thread, then open it. Handled in
    /// `drain_events`.
    fn export_pdf_from_preview(&mut self) {
        if self.pdf_exporting { return; }
        let rows: Vec<pdf::PdfRow> = self
            .pdf_preview_rows
            .iter()
            .zip(self.pdf_preview_selected.iter())
            .filter(|(_, sel)| **sel)
            .map(|(row, _)| row.clone())
            .collect();
        if rows.is_empty() {
            self.append_log("📄 Nothing to export — select at least one short.".into());
            return;
        }
        self.pdf_preview_open = false;
        self.pdf_exporting = true;
        self.append_log(format!("📄 Building PDF of {} short(s)…", rows.len()));
        let note = self.pdf_preview_note.clone();
        let (tx, rx) = unbounded::<Result<(std::path::PathBuf, usize, String), String>>();
        self.pdf_rx = Some(rx);
        std::thread::spawn(move || {
            let newest = rows.first().map(|r| r.title.clone()).unwrap_or_default();
            let out = pdf::default_output_path(rows.len(), &newest);
            let result = pdf::export(&rows, &out).map(|n| (out, n, note));
            let _ = tx.send(result);
        });
    }

    fn start_job(&mut self) { self.kick_off(false); }
    fn start_preview(&mut self) { self.kick_off(true); }

    fn append_upload_log(&self, line: String) {
        let _ = append_to_log_file(&line);
        if let Ok(mut g) = self.upload_log.lock() {
            g.push(line);
            if g.len() > MAX_LOG_LINES {
                let drop = g.len() - MAX_LOG_LINES;
                g.drain(0..drop);
            }
        }
    }

    fn start_direct_upload(&mut self) {
        if self.upload_running { return; }
        let file = self.upload_file.trim().to_string();
        if file.is_empty() {
            self.upload_last_error = Some("Pick a video file first".into());
            return;
        }
        let path = std::path::PathBuf::from(&file);
        if !path.is_file() {
            self.upload_last_error = Some(format!("File not found: {}", file));
            return;
        }
        if self.upload_title.trim().is_empty() {
            self.upload_last_error = Some("Title is required".into());
            return;
        }
        if !self.signed_in {
            self.upload_last_error = Some("Sign in to YouTube first (Settings)".into());
            return;
        }

        let (tx, rx): (Sender<Event>, Receiver<Event>) = unbounded();
        self.upload_rx = Some(rx);
        self.upload_running = true;
        let cancel = Arc::new(AtomicBool::new(false));
        self.upload_cancel = cancel.clone();
        self.upload_last_done_url = None;
        self.upload_last_error = None;
        if let Ok(mut g) = self.upload_log.lock() { g.clear(); }
        if let Ok(mut p) = self.upload_progress.lock() { *p = ProgressInfo::default(); }

        let job = pipeline::UploadJob {
            file: path,
            title: self.upload_title.trim().to_string(),
            description: self.upload_description.trim().to_string(),
            privacy: self.upload_privacy.clone(),
        };
        let settings = self.settings.clone();
        std::thread::spawn(move || pipeline::run_upload(job, settings, tx, cancel));
    }

    fn kick_off(&mut self, preview_only: bool) {
        if self.running { return; }
        let missing = self.deps.missing();
        if !missing.is_empty() {
            self.last_error = Some(format!(
                "Missing required tools: {}. {}",
                missing.join(", "),
                deps::DepStatus::install_hint(&missing),
            ));
            return;
        }
        if self.form.source.trim().is_empty() {
            self.last_error = Some("URL or video ID is required".into());
            return;
        }
        if self.form.start.trim().is_empty() || self.form.end.trim().is_empty() {
            self.last_error = Some("Start and end timestamps are required".into());
            return;
        }
        if self.form.cut_middle {
            let filled: Vec<&CutRange> = self
                .form
                .cuts
                .iter()
                .filter(|c| !c.from.trim().is_empty() || !c.till.trim().is_empty())
                .collect();
            if filled.is_empty() {
                self.last_error =
                    Some("Add at least one cut-out from/till when \"Remove middle section(s)\" is on".into());
                return;
            }
            if filled.iter().any(|c| c.from.trim().is_empty() || c.till.trim().is_empty()) {
                self.last_error =
                    Some("Every cut-out row needs both a from and a till timestamp".into());
                return;
            }
        }
        // Title is only required when actually uploading.
        if !preview_only && self.form.title.trim().is_empty() {
            self.last_error = Some("Title is required".into());
            return;
        }
        if !preview_only && !self.signed_in {
            self.last_error = Some("Sign in to YouTube first (Settings)".into());
            return;
        }

        let (tx, rx): (Sender<Event>, Receiver<Event>) = unbounded();
        self.rx = Some(rx);
        self.running = true;
        // Fresh flag per job so a Cancel from a prior run can't abort this one.
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();
        self.last_done_url = None;
        self.last_error = None;
        if let Ok(mut g) = self.log.lock() { g.clear(); }
        if let Ok(mut p) = self.progress.lock() { *p = ProgressInfo::default(); }

        let job = Job {
            source: self.form.source.trim().to_string(),
            start: self.form.start.trim().to_string(),
            end: self.form.end.trim().to_string(),
            title: if self.form.title.trim().is_empty() { "preview".to_string() } else { self.form.title.trim().to_string() },
            description: self.form.description.trim().to_string(),
            privacy: self.form.privacy.clone(),
            preview_only,
            overlay_title: self.form.overlay_title,
            overlay_color: self.form.overlay_color,
            overlay_pos: self.form.overlay_pos,
            stretch: self.form.stretch,
            stretch_secs: self.form.stretch_secs,
            fade_out: self.form.fade_out,
            fade_secs: self.form.fade_secs,
            fade_in: self.form.fade_in,
            fade_in_secs: self.form.fade_in_secs,
            cut_middle: self.form.cut_middle,
            cuts: if self.form.cut_middle {
                self.form
                    .cuts
                    .iter()
                    .filter(|c| !c.from.trim().is_empty() || !c.till.trim().is_empty())
                    .map(|c| (c.from.trim().to_string(), c.till.trim().to_string()))
                    .collect()
            } else {
                Vec::new()
            },
        };
        let settings = self.settings.clone();
        std::thread::spawn(move || pipeline::run(job, settings, tx, cancel));
    }

    fn drain_events(&mut self) {
        if let Some(rx) = &self.rx {
            let mut still_running = true;
            loop {
                let ev = match rx.try_recv() {
                    Ok(ev) => ev,
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        // Worker thread ended without a terminal event (it
                        // panicked). Don't leave the UI stuck on "running" /
                        // "Cancelling…" forever — surface it and reset.
                        if still_running {
                            self.last_error = Some("worker exited unexpectedly".into());
                            self.append_log("ERROR: worker exited unexpectedly".into());
                        }
                        still_running = false;
                        break;
                    }
                };
                match ev {
                    Event::Log(s) => self.append_log(s),
                    Event::Progress { phase, fraction, detail } => {
                        if let Ok(mut p) = self.progress.lock() {
                            *p = ProgressInfo { phase, fraction, detail };
                        }
                    }
                    Event::Done(url) => {
                        self.last_done_url = Some(url.clone());
                        self.davaz_posted = false;
                        self.davaz_status_msg = None;
                        let entry = history::UploadEntry {
                            timestamp: chrono_like_now(),
                            url: url.clone(),
                            title: self.form.title.trim().to_string(),
                            source: self.form.source.trim().to_string(),
                            start: self.form.start.trim().to_string(),
                            end: self.form.end.trim().to_string(),
                            privacy: self.form.privacy.clone(),
                        };
                        if let Err(e) = history::append(&entry) {
                            self.append_log(format!("history append failed: {}", e));
                        }
                        self.append_log(format!("DONE: {}", url));
                        still_running = false;
                    }
                    Event::Preview { original, edited } => {
                        if original == edited {
                            self.append_log(format!("Opening preview: {}", edited.display()));
                        } else {
                            self.append_log(format!(
                                "Opening split preview — original (top): {}",
                                original.display()
                            ));
                            self.append_log(format!("edited cut (bottom): {}", edited.display()));
                        }
                        if let Err(e) = open_split_preview(&original, &edited) {
                            self.last_error = Some(format!(
                                "Could not open preview ({}). Files at {} and {}",
                                e,
                                original.display(),
                                edited.display()
                            ));
                        }
                        still_running = false;
                    }
                    Event::Error(e) => {
                        self.last_error = Some(e.clone());
                        self.append_log(format!("ERROR: {}", e));
                        still_running = false;
                    }
                    Event::Cancelled => {
                        self.append_log("⏹ Cancelled.".into());
                        if let Ok(mut p) = self.progress.lock() { *p = ProgressInfo::default(); }
                        still_running = false;
                    }
                }
            }
            if !still_running {
                self.running = false;
                self.rx = None;
            }
        }

        if let Some(rx) = &self.upload_rx {
            let mut still_running = true;
            loop {
                let ev = match rx.try_recv() {
                    Ok(ev) => ev,
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        if still_running {
                            self.upload_last_error = Some("worker exited unexpectedly".into());
                            self.append_upload_log("ERROR: worker exited unexpectedly".into());
                        }
                        still_running = false;
                        break;
                    }
                };
                match ev {
                    Event::Log(s) => self.append_upload_log(s),
                    Event::Progress { phase, fraction, detail } => {
                        if let Ok(mut p) = self.upload_progress.lock() {
                            *p = ProgressInfo { phase, fraction, detail };
                        }
                    }
                    Event::Done(url) => {
                        self.upload_last_done_url = Some(url.clone());
                        let entry = history::UploadEntry {
                            timestamp: chrono_like_now(),
                            url: url.clone(),
                            title: self.upload_title.trim().to_string(),
                            source: self.upload_file.trim().to_string(),
                            start: String::new(),
                            end: String::new(),
                            privacy: self.upload_privacy.clone(),
                        };
                        if let Err(e) = history::append(&entry) {
                            self.append_upload_log(format!("history append failed: {}", e));
                        }
                        self.append_upload_log(format!("DONE: {}", url));
                        still_running = false;
                    }
                    Event::Preview { .. } => {} // not used in direct upload
                    Event::Error(e) => {
                        self.upload_last_error = Some(e.clone());
                        self.append_upload_log(format!("ERROR: {}", e));
                        still_running = false;
                    }
                    Event::Cancelled => {
                        self.append_upload_log("⏹ Cancelled.".into());
                        if let Ok(mut p) = self.upload_progress.lock() { *p = ProgressInfo::default(); }
                        still_running = false;
                    }
                }
            }
            if !still_running {
                self.upload_running = false;
                self.upload_rx = None;
            }
        }

        if let Some(rx) = &self.signin_rx {
            let mut done = false;
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    SignInEvent::Log(s) => self.append_log(s),
                    SignInEvent::Done => {
                        self.signed_in = true;
                        self.last_error = None;
                        self.append_log("Signed in to YouTube.".into());
                        done = true;
                    }
                    SignInEvent::Error(e) => {
                        self.last_error = Some(e.clone());
                        self.append_log(format!("Sign-in error: {}", e));
                        done = true;
                    }
                }
            }
            if done {
                self.signing_in = false;
                self.signin_rx = None;
            }
        }

        if let Some(rx) = &self.update_rx {
            if let Ok(result) = rx.try_recv() {
                self.update_checking = false;
                self.update_rx = None;
                match result {
                    Some(info) => {
                        self.append_log(format!("Update available: {} → {}", APP_VERSION, info.pretty()));
                        self.update_status_msg = Some(format!("Update available: {}", info.pretty()));
                        self.update_info = Some(info);
                    }
                    None => {
                        self.update_status_msg = Some(format!("You're on the latest version (v{}).", APP_VERSION));
                    }
                }
            }
        }

        if let Some(rx) = &self.cookies_test_rx {
            if let Ok(result) = rx.try_recv() {
                self.cookies_testing = false;
                self.cookies_test_rx = None;
                self.cookies_status_msg = Some(match result {
                    Ok(b) => format!("✅ {} cookies look good.", b),
                    Err(e) => format!("❌ {}", e),
                });
            }
        }

        if let Some(rx) = &self.pdf_fetch_rx {
            if let Ok(result) = rx.try_recv() {
                self.pdf_fetching = false;
                self.pdf_fetch_rx = None;
                match result {
                    Ok((rows, note)) => {
                        if rows.is_empty() {
                            self.append_log("📄 No shorts found to preview.".into());
                            if self.pdf_preview_rows.is_empty() {
                                self.pdf_preview_open = false;
                            }
                        } else {
                            self.append_log(format!(
                                "📄 Loaded {} short(s) ({}).",
                                rows.len(),
                                note
                            ));
                            shorts::save_cache(&rows);
                            // Preserve any ticks the user already made (match by
                            // URL); for a first load with nothing yet selected,
                            // auto-select the newest PDF_SHORTS_COUNT (rows come
                            // newest-first from the channel/history).
                            let prev_selected: std::collections::HashSet<String> = self
                                .pdf_preview_rows
                                .iter()
                                .zip(self.pdf_preview_selected.iter())
                                .filter(|(_, s)| **s)
                                .map(|(r, _)| r.url.trim().to_string())
                                .collect();
                            self.pdf_preview_selected = if prev_selected.is_empty() {
                                (0..rows.len()).map(|i| i < PDF_SHORTS_COUNT).collect()
                            } else {
                                rows.iter()
                                    .map(|r| prev_selected.contains(r.url.trim()))
                                    .collect()
                            };
                            self.pdf_preview_rows = rows;
                            self.pdf_preview_note = note;
                            self.pdf_preview_open = true;
                        }
                    }
                    Err(e) => self.append_log(format!("📄 Fetch failed: {e}")),
                }
            }
        }

        if let Some(rx) = &self.pdf_rx {
            if let Ok(result) = rx.try_recv() {
                self.pdf_exporting = false;
                self.pdf_rx = None;
                match result {
                    Ok((out, n, note)) => {
                        self.append_log(format!(
                            "📄 Exported {} short{} ({}) to {}",
                            n,
                            if n == 1 { "" } else { "s" },
                            note,
                            out.display()
                        ));
                        if let Err(e) = open::that(&out) {
                            self.append_log(format!("(couldn't open PDF automatically: {e})"));
                        }
                    }
                    Err(e) => self.append_log(format!("📄 PDF export failed: {e}")),
                }
            }
        }

        if let Some(rx) = &self.install_rx {
            let mut done = false;
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    installer::InstallEvent::Log(s) => self.append_log(s),
                    installer::InstallEvent::Phase(phase) => {
                        if let Ok(mut p) = self.install_progress.lock() {
                            p.phase = phase;
                        }
                    }
                    installer::InstallEvent::DownloadProgress { bytes, total } => {
                        if let Ok(mut p) = self.install_progress.lock() {
                            p.phase = "Downloading update".into();
                            p.fraction = if total == 0 { 0.0 } else { bytes as f32 / total as f32 };
                            p.detail = if total == 0 {
                                format!("{:.1} MB", bytes as f64 / 1_048_576.0)
                            } else {
                                format!(
                                    "{:.1} / {:.1} MB",
                                    bytes as f64 / 1_048_576.0,
                                    total as f64 / 1_048_576.0,
                                )
                            };
                        }
                    }
                    installer::InstallEvent::Done => {
                        self.append_log("Update staged. Restarting…".into());
                        if let Ok(mut p) = self.install_progress.lock() {
                            p.phase = "Restarting…".into();
                            p.fraction = 1.0;
                            p.detail.clear();
                        }
                        // The worker thread calls process::exit shortly
                        // after sending Done; nothing else to do here.
                    }
                    installer::InstallEvent::Error(e) => {
                        self.last_error = Some(e.clone());
                        self.append_log(format!("Update failed: {}", e));
                        done = true;
                    }
                }
            }
            if done {
                self.installing = false;
                self.install_rx = None;
                if let Ok(mut p) = self.install_progress.lock() { *p = ProgressInfo::default(); }
            }
        }

        if let Some(rx) = &self.wa_rx {
            if let Ok(result) = rx.try_recv() {
                self.wa_sending = false;
                self.wa_rx = None;
                match result {
                    Ok(()) => {
                        self.wa_status_msg = Some("✅ Sent via WhatsApp.".into());
                        self.append_log("WhatsApp send: ok".into());
                    }
                    Err(e) => {
                        self.wa_status_msg = Some(format!("❌ {}", e));
                        self.append_log(format!("WhatsApp send failed: {}", e));
                    }
                }
            }
        }

        if let Some(rx) = &self.davaz_rx {
            if let Ok(result) = rx.try_recv() {
                self.davaz_posting = false;
                self.davaz_rx = None;
                match result {
                    Ok(resp) => {
                        let id = resp.id_str();
                        let group = resp.artgroup_id.clone().unwrap_or_default();
                        let title = resp.title.clone().unwrap_or_default();
                        let summary = if group.is_empty() {
                            format!("✅ Posted to davaz.com — id={}", id)
                        } else {
                            format!("✅ Posted to davaz.com — id={} ({}): {}", id, group, title)
                        };
                        self.davaz_posted = true;
                        self.davaz_status_msg = Some(summary.clone());
                        self.append_log(summary);
                        if let Some(tag) = &resp.tag_added {
                            self.append_log(format!("davaz.com tag added: {}", tag));
                        }
                    }
                    Err(e) => {
                        self.davaz_status_msg = Some(format!("❌ davaz.com: {}", e));
                        self.append_log(format!("davaz.com post failed: {}", e));
                    }
                }
            }
        }

        if let Some(rx) = &self.wa_setup_rx {
            let mut done = false;
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    whatsapp::SetupEvent::Log(s) => self.append_log(s),
                    whatsapp::SetupEvent::Done => {
                        self.append_log("WhatsApp setup complete.".into());
                        self.wa_status_msg = Some("✅ Setup complete. Now click 'Link WhatsApp'.".into());
                        done = true;
                    }
                    whatsapp::SetupEvent::Error(e) => {
                        self.append_log(format!("WhatsApp setup failed: {}", e));
                        self.wa_status_msg = Some(format!("❌ Setup failed: {}", e));
                        done = true;
                    }
                }
            }
            if done {
                self.wa_setup_running = false;
                self.wa_setup_rx = None;
                self.refresh_wa_status();
            }
        }

        if let Some(rx) = &self.wa_login_rx {
            let mut done = false;
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    whatsapp::LoginEvent::Log(s) => self.append_log(s),
                    whatsapp::LoginEvent::Qr(qr) => {
                        self.wa_qr = Some(qr);
                        self.wa_show_qr = true;
                        self.wa_status_msg = Some("Scan QR with WhatsApp → Linked Devices.".into());
                    }
                    whatsapp::LoginEvent::Linked => {
                        self.append_log("WhatsApp linked.".into());
                        self.wa_status_msg = Some("✅ Linked!".into());
                        self.wa_qr = None;
                        self.wa_show_qr = false;
                        done = true;
                    }
                    whatsapp::LoginEvent::Error(e) => {
                        self.append_log(format!("WhatsApp link failed: {}", e));
                        self.wa_status_msg = Some(format!("❌ Link failed: {}", e));
                        done = true;
                    }
                }
            }
            if done {
                self.wa_login_running = false;
                self.wa_login_rx = None;
                self.refresh_wa_status();
            }
        }

        if let Some(rx) = &self.wa_picker_rx {
            if let Ok(result) = rx.try_recv() {
                self.wa_picker_loading = false;
                self.wa_picker_rx = None;
                match result {
                    Ok(items) => {
                        self.append_log(format!("Fetched {} WhatsApp groups.", items.len()));
                        self.wa_picker_items = items;
                    }
                    Err(e) => {
                        self.append_log(format!("WhatsApp recipient list failed: {}", e));
                        self.wa_picker_error = Some(e);
                    }
                }
            }
        }

        if let Some(rx) = &self.brew_rx {
            let mut done = false;
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    BrewEvent::Log(s) => self.append_log(s),
                    BrewEvent::Done => {
                        self.append_log("Homebrew install finished.".into());
                        self.deps = deps::DepStatus::check();
                        done = true;
                    }
                    BrewEvent::Error(e) => {
                        self.last_error = Some(e.clone());
                        self.append_log(format!("brew install error: {}", e));
                        done = true;
                    }
                }
            }
            if done {
                self.brew_installing = false;
                self.brew_rx = None;
            }
        }

        if let Some(rx) = &self.ytdlp_update_rx {
            let mut done = false;
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    BrewEvent::Log(s) => self.append_log(s),
                    BrewEvent::Done => {
                        self.deps = deps::DepStatus::check();
                        let ver = self.deps.yt_dlp.clone().unwrap_or_default();
                        self.append_log(format!("yt-dlp update finished — now {}", ver));
                        done = true;
                    }
                    BrewEvent::Error(e) => {
                        self.last_error = Some(e.clone());
                        self.append_log(format!("yt-dlp update error: {}", e));
                        done = true;
                    }
                }
            }
            if done {
                self.ytdlp_updating = false;
                self.ytdlp_update_rx = None;
            }
        }
    }

    fn start_signin(&mut self) {
        if self.signing_in { return; }
        if self.settings.client_id.trim().is_empty() || self.settings.client_secret.trim().is_empty() {
            self.last_error = Some("Enter Client ID and Client Secret in Settings first".into());
            return;
        }
        self.last_error = None;
        let (tx, rx) = unbounded::<SignInEvent>();
        self.signin_rx = Some(rx);
        self.signing_in = true;
        let cid = self.settings.client_id.clone();
        let csec = self.settings.client_secret.clone();
        std::thread::spawn(move || {
            let log_tx = tx.clone();
            let log_fn = move |s: String| { let _ = log_tx.send(SignInEvent::Log(s)); };
            match oauth::run_auth_flow(&cid, &csec, log_fn) {
                Ok(_) => { let _ = tx.send(SignInEvent::Done); }
                Err(e) => { let _ = tx.send(SignInEvent::Error(e)); }
            }
        });
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "form_state", &self.form);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.ensure_icon_texture(ctx);
        if self.running || self.upload_running || self.signing_in || self.brew_installing || self.ytdlp_updating || self.update_checking || self.cookies_testing || self.installing || self.wa_sending || self.wa_setup_running || self.wa_login_running || self.davaz_posting || self.pdf_exporting || self.pdf_fetching {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        }

        egui::TopBottomPanel::top("topbar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 44.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    if ui
                        .button("📜 History")
                        .on_hover_text("Show all videos uploaded from this app")
                        .clicked()
                    {
                        self.history_cache = history::load_all();
                        self.history_filter.clear();
                        self.show_history = true;
                    }
                    if ui
                        .button("⬆ Upload")
                        .on_hover_text("Upload a local video file directly to YouTube")
                        .clicked()
                    {
                        self.show_upload = true;
                    }
                    let pdf_busy = self.pdf_exporting || self.pdf_fetching;
                    if ui
                        .add_enabled(!pdf_busy, egui::Button::new(
                            if pdf_busy { "📄 PDF…" } else { "📄 PDF" }
                        ))
                        .on_hover_text(format!(
                            "Preview the latest {} shorts from @gozipa, delete any you don't want, then export a PDF (clickable links)",
                            PDF_SHORTS_COUNT
                        ))
                        .clicked()
                    {
                        self.start_pdf_fetch();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(tex) = &self.icon_texture {
                            let resp = ui
                                .add(
                                    egui::Image::new(tex)
                                        .max_width(40.0)
                                        .max_height(40.0)
                                        .sense(egui::Sense::click()),
                                )
                                .on_hover_text("Settings");
                            if resp.clicked() { self.show_settings = true; }
                            ui.add_space(8.0);
                        }
                        let badge = if self.signed_in { "✅ signed in" } else { "⚠ not signed in" };
                        ui.label(badge);
                    });
                },
            );
            ui.add_space(2.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(6.0);

            if let Some(info) = self.update_info.clone() {
                let can_in_app_update = cfg!(target_os = "macos")
                    && info.dmg_url.is_some()
                    && installer::current_app_bundle().is_some();
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(220, 240, 255))
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(80, 130, 200)))
                    .inner_margin(8.0)
                    .rounding(4.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.colored_label(
                                egui::Color32::from_rgb(20, 60, 120),
                                format!("⬆ Update available: {} (you have v{})", info.pretty(), APP_VERSION),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if !self.installing {
                                    if ui.button("Dismiss").clicked() {
                                        self.update_info = None;
                                    }
                                }
                                if can_in_app_update {
                                    let label = if self.installing { "Updating…" } else { "Update now" };
                                    let resp = ui.add_enabled(!self.installing, egui::Button::new(label));
                                    if resp.clicked() { self.start_install(); }
                                } else if ui.button("Open release page").clicked() {
                                    let _ = open::that(&info.url);
                                }
                            });
                        });
                        if self.installing {
                            let p = self.install_progress.lock().unwrap().clone();
                            let bar = if p.fraction > 0.0 {
                                egui::ProgressBar::new(p.fraction).show_percentage().animate(true)
                            } else {
                                egui::ProgressBar::new(0.0).animate(true)
                            };
                            let phase_label = if p.phase.is_empty() { "Working".to_string() } else { p.phase.clone() };
                            ui.add_space(4.0);
                            ui.add(bar.text(phase_label));
                            if !p.detail.is_empty() {
                                ui.label(RichText::new(p.detail).weak().small());
                            }
                        }
                    });
                ui.add_space(6.0);
            }

            let missing = self.deps.missing();
            if !missing.is_empty() {
                let brew = deps::brew_path();
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(255, 240, 205))
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(220, 170, 60)))
                    .inner_margin(8.0)
                    .rounding(4.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.colored_label(
                                egui::Color32::from_rgb(140, 80, 0),
                                format!("⚠ Missing required tools: {}", missing.join(", ")),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button("Re-check").clicked() {
                                    self.deps = deps::DepStatus::check();
                                }
                                if cfg!(target_os = "macos") {
                                    if brew.is_some() {
                                        let label = if self.brew_installing { "Installing…" } else { "Install with Homebrew" };
                                        let resp = ui.add_enabled(!self.brew_installing, egui::Button::new(label));
                                        if resp.clicked() { self.start_brew_install(); }
                                    } else if ui.button("How to install Homebrew").clicked() {
                                        let _ = open::that("https://brew.sh");
                                    }
                                }
                            });
                        });
                        if cfg!(target_os = "macos") && brew.is_none() {
                            ui.label("Homebrew is not installed. Open Terminal and run:");
                            let mut cmd = deps::HOMEBREW_INSTALL_CMD.to_string();
                            ui.add(
                                egui::TextEdit::singleline(&mut cmd)
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(f32::INFINITY),
                            );
                            ui.label("Then come back here and click Re-check.");
                        } else {
                            ui.label(deps::DepStatus::install_hint(&missing));
                        }
                    });
                ui.add_space(6.0);
            }

            // yt-dlp staleness banner: shown when yt-dlp is present but
            // older than the threshold. One-click update; we never auto-run
            // it (a brew upgrade pegs the machine — see start_ytdlp_update).
            if self.deps.yt_dlp.is_some() && self.deps.yt_dlp_is_stale() {
                let age = self.deps.yt_dlp_age_days().unwrap_or(0);
                let is_brew = deps::yt_dlp_update_command().1 == vec!["upgrade".to_string(), "yt-dlp".to_string()];
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(255, 240, 205))
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(220, 170, 60)))
                    .inner_margin(8.0)
                    .rounding(4.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.colored_label(
                                egui::Color32::from_rgb(140, 80, 0),
                                format!("⚠ yt-dlp is {} days old — updating is recommended to avoid download failures.", age),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let label = if self.ytdlp_updating { "Updating…" } else { "Update yt-dlp" };
                                let resp = ui.add_enabled(!self.ytdlp_updating, egui::Button::new(label));
                                if resp.clicked() { self.start_ytdlp_update(); }
                            });
                        });
                        if is_brew && !self.ytdlp_updating {
                            ui.label(
                                RichText::new("Note: this runs `brew upgrade yt-dlp`, which first does a full `brew update` and may briefly load the machine.")
                                    .weak().small(),
                            );
                        }
                    });
                ui.add_space(6.0);
            }

            egui::Grid::new("form_grid")
                .num_columns(2)
                .spacing([10.0, 8.0])
                .show(ui, |ui| {
                    ui.label("YouTube URL or ID:");
                    ui.add(egui::TextEdit::singleline(&mut self.form.source).desired_width(f32::INFINITY));
                    ui.end_row();

                    ui.label("Start (mm:ss or hh:mm:ss):");
                    ui.add(egui::TextEdit::singleline(&mut self.form.start).desired_width(160.0));
                    ui.end_row();

                    ui.label("End:");
                    ui.add(egui::TextEdit::singleline(&mut self.form.end).desired_width(160.0));
                    ui.end_row();

                    ui.label("Title:");
                    ui.add(egui::TextEdit::singleline(&mut self.form.title).desired_width(f32::INFINITY));
                    ui.end_row();

                    ui.label("Description:");
                    ui.add(
                        egui::TextEdit::multiline(&mut self.form.description)
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label("Privacy:");
                    ui.horizontal(|ui| {
                        for p in ["public", "unlisted", "private"] {
                            ui.radio_value(&mut self.form.privacy, p.to_string(), p);
                        }
                    });
                    ui.end_row();

                    ui.label("Title overlay:");
                    ui.horizontal(|ui| {
                        ui.checkbox(
                            &mut self.form.overlay_title,
                            "Burn title into video (1s–4s)",
                        ).on_hover_text(
                            "Re-encodes the segment with the title text shown for 3 seconds, starting 1 second in. Use the position box below to place it. Applies to both Preview and Upload.",
                        );
                        ui.add_space(12.0);
                        ui.label("Color:");
                        ui.color_edit_button_srgb(&mut self.form.overlay_color)
                            .on_hover_text("Click to pick the text color. Double-click anywhere to close the picker. A thin black outline is added automatically for legibility over any background.");
                        // egui's color picker stays open until you click
                        // outside the popup area — annoying when the user is
                        // done picking. Double-click closes whichever popup
                        // is currently open (no-op if none).
                        if ui.input(|i| i.pointer.button_double_clicked(egui::PointerButton::Primary)) {
                            ui.memory_mut(|mem| mem.close_popup());
                        }
                    });
                    ui.end_row();

                    let overlay_on = self.form.overlay_title;
                    ui.label("Title position:");
                    // Render the picker as a *direct* grid cell (like the
                    // multiline description above) so the Grid grows the row to
                    // its full height. Wrapping it in a horizontal/vertical
                    // layout stopped the 90px height from bubbling up, which
                    // made the rows below overlap it.
                    title_position_picker(ui, &mut self.form.overlay_pos, overlay_on);
                    ui.end_row();

                    ui.label("");
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "Drag the box above to place the title — x: {:.0}%  y: {:.0}%",
                                self.form.overlay_pos[0] * 100.0,
                                self.form.overlay_pos[1] * 100.0,
                            ))
                            .weak(),
                        );
                        ui.add_space(8.0);
                        if ui
                            .add_enabled(overlay_on, egui::Button::new("Reset to bottom-left").small())
                            .clicked()
                        {
                            self.form.overlay_pos = default_overlay_pos();
                        }
                    });
                    ui.end_row();

                    ui.label("Cut out:");
                    ui.vertical(|ui| {
                        ui.checkbox(
                            &mut self.form.cut_middle,
                            "Remove middle section(s)",
                        ).on_hover_text(
                            "Deletes everything between each from/till pair below (same style as start/end, e.g. 1:15 or 12:44) and joins the remaining parts into one clip. Add as many cuts as you like. Applies to both Preview and Upload.",
                        );
                        let cut_on = self.form.cut_middle;
                        ui.add_enabled_ui(cut_on, |ui| {
                            let mut remove: Option<usize> = None;
                            let count = self.form.cuts.len();
                            for (i, cut) in self.form.cuts.iter_mut().enumerate() {
                                ui.horizontal(|ui| {
                                    ui.label("from");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut cut.from)
                                            .desired_width(70.0)
                                            .hint_text("mm:ss"),
                                    ).on_hover_text("Start of a section to remove (absolute in the source, e.g. 1:15).");
                                    ui.label("till");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut cut.till)
                                            .desired_width(70.0)
                                            .hint_text("mm:ss"),
                                    ).on_hover_text("End of that section (absolute in the source, e.g. 1:40).");
                                    if count > 1
                                        && ui.button("🗑").on_hover_text("Remove this cut").clicked()
                                    {
                                        remove = Some(i);
                                    }
                                });
                            }
                            if let Some(i) = remove {
                                self.form.cuts.remove(i);
                            }
                            if ui
                                .button("➕ Add cut")
                                .on_hover_text("Add another section to remove")
                                .clicked()
                            {
                                self.form.cuts.push(CutRange::default());
                            }
                        });
                    });
                    ui.end_row();

                    ui.label("Stretch:");
                    ui.horizontal(|ui| {
                        ui.checkbox(
                            &mut self.form.stretch,
                            "Slow the clip down to make it longer",
                        ).on_hover_text(
                            "Time-stretches (slows down) the selected clip so it lasts this many seconds longer. Picture and sound stay in sync. Fade-in and fade-out durations are unaffected. Applies to both Preview and Upload.",
                        );
                        ui.add_enabled_ui(self.form.stretch, |ui| {
                            ui.add(
                                egui::DragValue::new(&mut self.form.stretch_secs)
                                    .speed(1.0)
                                    .range(1..=120)
                                    .suffix(" s"),
                            ).on_hover_text("Number of seconds to add to the clip's length (1–120).");
                        });
                    });
                    ui.end_row();

                    ui.label("Fade-in:");
                    ui.horizontal(|ui| {
                        ui.checkbox(
                            &mut self.form.fade_in,
                            "Freeze first frame and fade in from black at the start",
                        ).on_hover_text(
                            "Holds the first frame and fades it — picture and sound — in from black/silence, then the movie starts from that frame. The short ends up that many seconds longer. Applies to both Preview and Upload.",
                        );
                        ui.add_enabled_ui(self.form.fade_in, |ui| {
                            ui.add(
                                egui::DragValue::new(&mut self.form.fade_in_secs)
                                    .speed(1.0)
                                    .range(1..=60)
                                    .suffix(" s"),
                            ).on_hover_text("Fade-in duration in seconds (1–60).");
                        });
                    });
                    ui.end_row();

                    ui.label("Fade-out:");
                    ui.horizontal(|ui| {
                        ui.checkbox(
                            &mut self.form.fade_out,
                            "Freeze last frame and fade to black at the end",
                        ).on_hover_text(
                            "Holds the last frame and fades it — picture and sound — to black/silence. The short ends up that many seconds longer. Applies to both Preview and Upload.",
                        );
                        ui.add_enabled_ui(self.form.fade_out, |ui| {
                            ui.add(
                                egui::DragValue::new(&mut self.form.fade_secs)
                                    .speed(1.0)
                                    .range(1..=60)
                                    .suffix(" s"),
                            ).on_hover_text("Fade-out duration in seconds (1–60).");
                        });
                    });
                    ui.end_row();
                });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let deps_ok = self.deps.missing().is_empty();
                let preview_btn = ui.add_enabled(
                    !self.running && deps_ok,
                    egui::Button::new("Preview").min_size(egui::vec2(110.0, 32.0)),
                ).on_hover_text("Download the segment and open it in your default video player without uploading");
                if preview_btn.clicked() { self.start_preview(); }
                ui.add_space(8.0);
                let label = if self.running { "Working…" } else { "Create short and upload" };
                let btn = ui.add_enabled(
                    !self.running && deps_ok,
                    egui::Button::new(label).min_size(egui::vec2(220.0, 32.0)),
                );
                if btn.clicked() { self.start_job(); }

                // While a job runs, offer a red Cancel that stops the
                // download/encode/upload (kills the running yt-dlp/ffmpeg
                // child) so a wrong timestamp doesn't have to run to the end.
                if self.running {
                    ui.add_space(8.0);
                    let cancelling = self.cancel.load(Ordering::SeqCst);
                    let cancel_label = if cancelling { "Cancelling…" } else { "✖ Cancel" };
                    let cancel_btn = ui.add_enabled(
                        !cancelling,
                        egui::Button::new(RichText::new(cancel_label).color(egui::Color32::WHITE))
                            .fill(egui::Color32::from_rgb(200, 70, 70))
                            .min_size(egui::vec2(120.0, 32.0)),
                    ).on_hover_text("Stop this job (download/encode/upload) and discard it");
                    if cancel_btn.clicked() {
                        self.cancel.store(true, Ordering::SeqCst);
                        self.append_log("Cancel requested — stopping…".into());
                    }
                }
            });

            if let Some(url) = self.last_done_url.clone() {
                ui.add_space(6.0);
                let wa_configured = !self.settings.whatsapp_dir.trim().is_empty()
                    && !self.settings.whatsapp_recipient.trim().is_empty()
                    && self.wa_provisioned
                    && self.wa_linked;
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(220, 245, 220))
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(80, 160, 80)))
                    .inner_margin(10.0)
                    .rounding(4.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.colored_label(
                                egui::Color32::from_rgb(20, 90, 20),
                                RichText::new(format!("✅ Uploaded: {}", url)).strong(),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button("Dismiss").clicked() {
                                    self.last_done_url = None;
                                    self.wa_status_msg = None;
                                    self.davaz_status_msg = None;
                                    self.davaz_posted = false;
                                }
                                if ui.button("Copy URL").clicked() {
                                    ui.ctx().output_mut(|o| o.copied_text = url.clone());
                                }
                                if ui.button("Open in browser").clicked() {
                                    let _ = open::that(&url);
                                }
                                let davaz_configured = !self.settings.davaz_token.trim().is_empty();
                                let davaz_label = if self.davaz_posting {
                                    "Posting…"
                                } else if self.davaz_posted {
                                    "✅ Posted to davaz.com"
                                } else {
                                    "Post to davaz.com"
                                };
                                let davaz_tooltip = if davaz_configured {
                                    "Post this YouTube link to davaz.com so it appears on the site".to_string()
                                } else {
                                    "Set davaz.com Bearer token in Settings first".to_string()
                                };
                                let davaz_btn = ui.add_enabled(
                                    davaz_configured && !self.davaz_posting && !self.davaz_posted,
                                    egui::Button::new(davaz_label),
                                ).on_hover_text(davaz_tooltip);
                                if davaz_btn.clicked() { self.start_davaz_post(url.clone()); }
                                let label = if self.wa_sending { "Sending…" } else { "Send via WA" };
                                let tooltip = if wa_configured {
                                    format!("Send YouTube link to {}", self.settings.whatsapp_recipient)
                                } else if self.settings.whatsapp_dir.trim().is_empty() || self.settings.whatsapp_recipient.trim().is_empty() {
                                    "Configure WhatsApp dir + recipient in Settings first".to_string()
                                } else if !self.wa_provisioned {
                                    "Click 'Setup WhatsApp' in Settings first".to_string()
                                } else {
                                    "Click 'Link WhatsApp' in Settings first".to_string()
                                };
                                let resp = ui.add_enabled(
                                    wa_configured && !self.wa_sending,
                                    egui::Button::new(label),
                                ).on_hover_text(tooltip);
                                if resp.clicked() { self.start_wa_send(url.clone()); }
                                let person_btn = ui.button("Send to person…")
                                    .on_hover_text(
                                        "Open WhatsApp with the YouTube link pre-filled, \
                                         then pick the contact yourself (uses wa.me)",
                                    );
                                if person_btn.clicked() {
                                    let title = self.form.title.trim();
                                    let msg = if title.is_empty() {
                                        url.clone()
                                    } else {
                                        format!("{}\n{}", title, url)
                                    };
                                    let share = format!(
                                        "https://wa.me/?text={}",
                                        urlencoding::encode(&msg),
                                    );
                                    self.append_log(format!("Opening WhatsApp share: {}", share));
                                    if let Err(e) = open::that(&share) {
                                        self.last_error = Some(format!(
                                            "Could not open WhatsApp share URL: {}",
                                            e
                                        ));
                                    }
                                }
                            });
                        });
                        if let Some(msg) = &self.wa_status_msg {
                            ui.label(RichText::new(msg).small());
                        }
                        if let Some(msg) = &self.davaz_status_msg {
                            let color = if msg.starts_with('❌') {
                                egui::Color32::from_rgb(180, 60, 60)
                            } else {
                                egui::Color32::from_rgb(20, 90, 20)
                            };
                            ui.colored_label(color, RichText::new(msg).small().strong());
                        }
                    });
            }

            if self.running {
                let p = self.progress.lock().unwrap().clone();
                if !p.phase.is_empty() || p.fraction > 0.0 {
                    let bar = if p.fraction > 0.0 {
                        egui::ProgressBar::new(p.fraction).show_percentage().animate(true)
                    } else {
                        egui::ProgressBar::new(0.0).animate(true)
                    };
                    let phase_label = if p.phase.is_empty() { "Working".to_string() } else { p.phase.clone() };
                    let bar = bar.text(phase_label);
                    ui.add(bar);
                    if !p.detail.is_empty() {
                        ui.label(RichText::new(p.detail).weak().small());
                    }
                }
            }

            if let Some(err) = &self.last_error {
                ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
                if let Some(hint) = bot_detection_hint(err, &self.settings) {
                    ui.add_space(4.0);
                    egui::Frame::none()
                        .fill(egui::Color32::from_rgb(255, 248, 220))
                        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(200, 170, 60)))
                        .inner_margin(8.0)
                        .rounding(4.0)
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.colored_label(
                                    egui::Color32::from_rgb(110, 80, 0),
                                    RichText::new(hint).strong(),
                                );
                            });
                            ui.add_space(4.0);
                            if ui.button("Open Settings").clicked() {
                                self.show_settings = true;
                            }
                        });
                }
            }

            ui.add_space(6.0);
            ui.separator();
            ui.label(RichText::new("Log").strong());

            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if let Ok(g) = self.log.lock() {
                        for line in g.iter() {
                            ui.monospace(line);
                        }
                    }
                });
        });

        if self.show_settings {
            self.draw_settings(ctx);
        }

        if self.wa_show_qr {
            self.draw_qr_modal(ctx);
        }

        if self.wa_picker_open {
            self.draw_recipient_picker(ctx);
        }

        if self.show_history {
            self.draw_history_modal(ctx);
        }

        if self.show_upload {
            self.draw_upload_modal(ctx);
        }

        if self.pdf_preview_open {
            self.draw_pdf_preview_modal(ctx);
        }
    }
}

impl App {
    /// Preview the full shorts history, letting the user tick which ones go
    /// into the PDF (the newest `PDF_SHORTS_COUNT` are pre-selected). Each row
    /// is one movie/short.
    fn draw_pdf_preview_modal(&mut self, ctx: &egui::Context) {
        // Keep the selection vector in lockstep with the rows.
        if self.pdf_preview_selected.len() != self.pdf_preview_rows.len() {
            self.pdf_preview_selected.resize(self.pdf_preview_rows.len(), false);
        }
        let mut open = self.pdf_preview_open;
        let mut export_now = false;
        let mut cancel = false;
        let mut set_all: Option<bool> = None;
        egui::Window::new("Shorts for PDF")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(720.0)
            .default_height(520.0)
            .show(ctx, |ui| {
                let total = self.pdf_preview_rows.len();
                let selected = self.pdf_preview_selected.iter().filter(|s| **s).count();
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "{} of {} short{} selected — {}",
                            selected,
                            total,
                            if total == 1 { "" } else { "s" },
                            self.pdf_preview_note
                        ))
                        .strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Deselect all").clicked() {
                            set_all = Some(false);
                        }
                        if ui.button("Select all").clicked() {
                            set_all = Some(true);
                        }
                    });
                });
                ui.label(
                    RichText::new("Tick the shorts to include, then Create PDF.")
                        .small()
                        .weak(),
                );
                ui.separator();

                if total == 0 {
                    ui.add_space(20.0);
                    ui.vertical_centered(|ui| {
                        if self.pdf_fetching {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Loading shorts from @gozipa…");
                            });
                        } else {
                            ui.label(RichText::new("No shorts found.").weak());
                        }
                    });
                } else {
                    // Shrink vertically to the content so the buttons sit right
                    // under the last item (no big gap), but cap the height and
                    // scroll when the list is long. Reserve room for the button row.
                    let max_h = (ui.available_height() - 40.0).max(80.0);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .max_height(max_h)
                        .show(ui, |ui| {
                        for (i, row) in self.pdf_preview_rows.iter().enumerate() {
                            ui.horizontal(|ui| {
                                let sel = &mut self.pdf_preview_selected[i];
                                ui.checkbox(sel, "")
                                    .on_hover_text("Include this short in the PDF");
                                ui.vertical(|ui| {
                                    let title = if row.title.trim().is_empty() {
                                        "(untitled)"
                                    } else {
                                        row.title.trim()
                                    };
                                    ui.label(RichText::new(format!("{}. {}", i + 1, title)).strong());
                                    ui.horizontal_wrapped(|ui| {
                                        if ui
                                            .link(RichText::new(row.url.trim()).monospace().small())
                                            .on_hover_text("Open in browser")
                                            .clicked()
                                        {
                                            let _ = open::that(row.url.trim());
                                        }
                                        if !row.meta.trim().is_empty() {
                                            ui.label(
                                                RichText::new(row.meta.trim()).small().weak(),
                                            );
                                        }
                                    });
                                });
                            });
                            ui.separator();
                        }
                    });
                }

                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(selected > 0, egui::Button::new("📄 Create PDF"))
                        .clicked()
                    {
                        export_now = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if let Some(v) = set_all {
            for s in self.pdf_preview_selected.iter_mut() {
                *s = v;
            }
        }
        if cancel {
            open = false;
        }
        self.pdf_preview_open = open;
        if export_now {
            self.export_pdf_from_preview();
        }
    }

    fn draw_history_modal(&mut self, ctx: &egui::Context) {
        let mut open = self.show_history;
        egui::Window::new("Upload history")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(720.0)
            .default_height(520.0)
            .show(ctx, |ui| {
                let total = self.history_cache.len();
                ui.horizontal(|ui| {
                    ui.label(format!("{} upload{}", total, if total == 1 { "" } else { "s" }));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Refresh").clicked() {
                            self.history_cache = history::load_all();
                        }
                        ui.add(
                            egui::TextEdit::singleline(&mut self.history_filter)
                                .hint_text("Filter by title or URL")
                                .desired_width(260.0),
                        );
                    });
                });

                if total == 0 {
                    ui.add_space(20.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new("No uploads yet — once you upload a short, it will appear here.")
                                .weak(),
                        );
                    });
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(format!("File: {}", history::history_path().display()))
                            .small()
                            .weak(),
                    );
                    return;
                }

                ui.separator();
                let filter = self.history_filter.trim().to_lowercase();
                let entries = self.history_cache.clone();
                egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                    for entry in entries.iter() {
                        if !filter.is_empty() {
                            let hay = format!("{}\n{}", entry.title.to_lowercase(), entry.url.to_lowercase());
                            if !hay.contains(&filter) {
                                continue;
                            }
                        }
                        ui.add_space(2.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(&entry.timestamp).monospace().small().weak());
                            ui.add_space(8.0);
                            let title = if entry.title.is_empty() { "(untitled)" } else { entry.title.as_str() };
                            if ui
                                .link(RichText::new(title).strong())
                                .on_hover_text(format!("Open {} in browser", entry.url))
                                .clicked()
                            {
                                let _ = open::that(&entry.url);
                            }
                            if !entry.privacy.is_empty() {
                                ui.label(RichText::new(format!("[{}]", entry.privacy)).small().weak());
                            }
                        });
                        ui.horizontal_wrapped(|ui| {
                            ui.add_space(120.0);
                            ui.label(RichText::new(&entry.url).monospace().small().weak());
                            if !entry.start.is_empty() || !entry.end.is_empty() {
                                ui.label(
                                    RichText::new(format!(" ({}–{})", entry.start, entry.end))
                                        .small()
                                        .weak(),
                                );
                            }
                        });
                        ui.separator();
                    }
                });
            });
        self.show_history = open;
    }

    fn draw_upload_modal(&mut self, ctx: &egui::Context) {
        // Accept files dropped anywhere while the modal is open. egui
        // raises hovered_files first, then dropped_files once the user
        // releases the mouse. We only care about the latter.
        let dropped = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .find_map(|f| f.path.clone())
        });
        if let Some(path) = dropped {
            self.upload_file = path.to_string_lossy().into_owned();
        }

        let mut open = self.show_upload;
        egui::Window::new("Upload to YouTube")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(720.0)
            .default_height(540.0)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(
                        "Upload a local video file directly to YouTube. \
                         No yt-dlp, no segment extraction.",
                    )
                    .small()
                    .weak(),
                );
                ui.add_space(6.0);

                if !self.signed_in {
                    ui.colored_label(
                        egui::Color32::from_rgb(180, 100, 30),
                        "⚠ You must sign in to YouTube first — open Settings.",
                    );
                    ui.add_space(6.0);
                }

                egui::Grid::new("upload_form_grid")
                    .num_columns(2)
                    .spacing([10.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Video file:");
                        ui.horizontal(|ui| {
                            if ui
                                .button("Choose file…")
                                .on_hover_text("Open the system file picker")
                                .clicked()
                            {
                                if let Some(path) = rfd::FileDialog::new()
                                    .add_filter("Video", &["mp4", "mov", "m4v", "mkv", "webm", "avi"])
                                    .add_filter("Any file", &["*"])
                                    .pick_file()
                                {
                                    self.upload_file = path.to_string_lossy().into_owned();
                                }
                            }
                            ui.add(
                                egui::TextEdit::singleline(&mut self.upload_file)
                                    .hint_text("/path/to/video.mp4  (or drag a file into this window)")
                                    .desired_width(f32::INFINITY),
                            );
                        });
                        ui.end_row();

                        ui.label("Title:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.upload_title)
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();

                        ui.label("Description:");
                        ui.add(
                            egui::TextEdit::multiline(&mut self.upload_description)
                                .desired_rows(4)
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();

                        ui.label("Privacy:");
                        ui.horizontal(|ui| {
                            for p in ["public", "unlisted", "private"] {
                                ui.radio_value(&mut self.upload_privacy, p.to_string(), p);
                            }
                        });
                        ui.end_row();
                    });

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let label = if self.upload_running { "Uploading…" } else { "Upload to YouTube" };
                    let btn = ui.add_enabled(
                        !self.upload_running && self.signed_in,
                        egui::Button::new(label).min_size(egui::vec2(200.0, 32.0)),
                    );
                    if btn.clicked() { self.start_direct_upload(); }

                    if self.upload_running {
                        ui.add_space(8.0);
                        let cancelling = self.upload_cancel.load(Ordering::SeqCst);
                        let cancel_label = if cancelling { "Cancelling…" } else { "✖ Cancel" };
                        let cancel_btn = ui.add_enabled(
                            !cancelling,
                            egui::Button::new(RichText::new(cancel_label).color(egui::Color32::WHITE))
                                .fill(egui::Color32::from_rgb(200, 70, 70))
                                .min_size(egui::vec2(120.0, 32.0)),
                        ).on_hover_text("Stop the upload and discard it");
                        if cancel_btn.clicked() {
                            self.upload_cancel.store(true, Ordering::SeqCst);
                            self.append_upload_log("Cancel requested — stopping…".into());
                        }
                    }
                });

                if self.upload_running {
                    let p = self.upload_progress.lock().unwrap().clone();
                    ui.add_space(6.0);
                    let bar = if p.fraction > 0.0 {
                        egui::ProgressBar::new(p.fraction).show_percentage().animate(true)
                    } else {
                        egui::ProgressBar::new(0.0).animate(true)
                    };
                    let phase_label = if p.phase.is_empty() { "Working".to_string() } else { p.phase.clone() };
                    ui.add(bar.text(phase_label));
                    if !p.detail.is_empty() {
                        ui.label(RichText::new(p.detail).weak().small());
                    }
                }

                if let Some(err) = &self.upload_last_error {
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
                }

                if let Some(url) = self.upload_last_done_url.clone() {
                    ui.add_space(6.0);
                    egui::Frame::none()
                        .fill(egui::Color32::from_rgb(220, 245, 220))
                        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(80, 160, 80)))
                        .inner_margin(10.0)
                        .rounding(4.0)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.colored_label(
                                    egui::Color32::from_rgb(20, 90, 20),
                                    RichText::new(format!("✅ Uploaded: {}", url)).strong(),
                                );
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if ui.button("Copy URL").clicked() {
                                        ui.ctx().output_mut(|o| o.copied_text = url.clone());
                                    }
                                    if ui.button("Open in browser").clicked() {
                                        let _ = open::that(&url);
                                    }
                                });
                            });
                        });
                }

                ui.add_space(8.0);
                ui.separator();
                ui.label(RichText::new("Log").strong());
                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .stick_to_bottom(true)
                    .max_height(160.0)
                    .show(ui, |ui| {
                        if let Ok(g) = self.upload_log.lock() {
                            for line in g.iter() {
                                ui.monospace(line);
                            }
                        }
                    });
            });
        self.show_upload = open;
    }

    fn draw_qr_modal(&mut self, ctx: &egui::Context) {
        let mut open = self.wa_show_qr;
        egui::Window::new("Link WhatsApp")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(560.0)
            .show(ctx, |ui| {
                ui.label("Open WhatsApp on your phone → Settings → Linked Devices → Link a Device, then scan:");
                ui.add_space(6.0);
                if let Some(qr) = self.wa_qr.clone() {
                    egui::Frame::none()
                        .fill(egui::Color32::WHITE)
                        .inner_margin(8.0)
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(qr)
                                        .monospace()
                                        .color(egui::Color32::BLACK),
                                )
                                .selectable(false),
                            );
                        });
                } else if self.wa_login_running {
                    ui.label(RichText::new("Generating QR code…").italics());
                } else {
                    ui.label("No QR code yet.");
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        self.wa_show_qr = false;
                        // Note: the node child keeps running until it
                        // times out (5 min) — we just stop showing the
                        // modal. Could be improved with a kill switch.
                    }
                });
            });
        // If the user closed the window via the X button, mirror that.
        if !open {
            self.wa_show_qr = false;
        }
    }

    fn draw_recipient_picker(&mut self, ctx: &egui::Context) {
        let mut open = self.wa_picker_open;
        let mut chosen: Option<String> = None;
        egui::Window::new("Pick WhatsApp recipient")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(420.0)
            .default_height(360.0)
            .show(ctx, |ui| {
                if let Some(err) = &self.wa_picker_error {
                    ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
                    ui.add_space(6.0);
                }
                if self.wa_picker_loading && self.wa_picker_items.is_empty() {
                    // First-time fetch (no cache yet) — full-screen loader.
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Connecting and fetching groups…");
                    });
                    return;
                }
                if self.wa_picker_items.is_empty() && self.wa_picker_error.is_none() {
                    ui.label(RichText::new(
                        "No WhatsApp groups found on this account.\n\
                         For 1:1 chats, type the phone number (digits only, e.g. 41791234567).",
                    ).weak());
                    return;
                }
                ui.horizontal(|ui| {
                    ui.label("Filter:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.wa_picker_filter)
                            .desired_width(ui.available_width() - 110.0),
                    );
                    if self.wa_picker_loading {
                        ui.spinner();
                        ui.label(RichText::new("refreshing").weak().small());
                    }
                });
                ui.add_space(4.0);
                let filter = self.wa_picker_filter.to_lowercase();
                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        for r in &self.wa_picker_items {
                            if !filter.is_empty()
                                && !r.name.to_lowercase().contains(&filter)
                                && !r.jid.to_lowercase().contains(&filter)
                            {
                                continue;
                            }
                            let icon = if r.kind == "group" { "👥" } else { "👤" };
                            let display_name = if r.name.is_empty() {
                                // For unnamed 1:1 contacts, show the phone
                                // number portion of the JID as a hint.
                                r.jid.split('@').next().unwrap_or(&r.jid).to_string()
                            } else {
                                r.name.clone()
                            };
                            let label = if r.name.is_empty() && r.kind != "group" {
                                format!("{}  +{}", icon, display_name)
                            } else {
                                format!("{}  {}", icon, display_name)
                            };
                            if ui.selectable_label(false, label).clicked() {
                                chosen = Some(r.jid.clone());
                            }
                        }
                    });
            });
        if let Some(jid) = chosen {
            self.settings.whatsapp_recipient = jid;
            self.wa_picker_open = false;
        }
        if !open {
            self.wa_picker_open = false;
        }
    }

    fn draw_settings(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        egui::Window::new("Settings")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(560.0)
            .show(ctx, |ui| {
                ui.label(RichText::new("Google OAuth client").strong());
                ui.label(
                    "Create an OAuth 2.0 Client (type: Desktop) at\n\
                     https://console.cloud.google.com/apis/credentials,\n\
                     enable the YouTube Data API v3, and paste the values below.",
                );
                ui.add_space(6.0);

                egui::Grid::new("settings_grid")
                    .num_columns(2)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Client ID:");
                        ui.add(egui::TextEdit::singleline(&mut self.settings.client_id).desired_width(f32::INFINITY));
                        ui.end_row();

                        ui.label("Client Secret:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.client_secret)
                                .password(true)
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();

                        ui.label("Default privacy:");
                        ui.horizontal(|ui| {
                            for p in ["public", "unlisted", "private"] {
                                ui.radio_value(&mut self.settings.default_privacy, p.to_string(), p);
                            }
                        });
                        ui.end_row();

                        ui.label("Cookies from browser:");
                        ui.horizontal(|ui| {
                            let mut cb = self.settings.cookies_browser.clone();
                            egui::ComboBox::from_id_salt("cookies_browser")
                                .selected_text(if cb.is_empty() { "(none)".to_string() } else { browser_label(&cb, &self.detected_browsers) })
                                .show_ui(ui, |ui| {
                                    for opt in std::iter::once("").chain(browsers::ALL.iter().copied()) {
                                        let label = if opt.is_empty() { "(none)".to_string() } else { browser_label(opt, &self.detected_browsers) };
                                        ui.selectable_value(&mut cb, opt.to_string(), label);
                                    }
                                });
                            self.settings.cookies_browser = cb;
                            let test_label = if self.cookies_testing { "Testing…" } else { "Test" };
                            if ui.add_enabled(!self.cookies_testing, egui::Button::new(test_label)).clicked() {
                                self.start_cookies_test();
                            }
                        });
                        ui.end_row();
                        if !self.detected_browsers.is_empty() {
                            ui.label("");
                            ui.label(RichText::new(format!("Detected: {}", self.detected_browsers.join(", "))).weak().small());
                            ui.end_row();
                        }
                        if let Some(msg) = &self.cookies_status_msg {
                            ui.label("");
                            ui.label(RichText::new(msg).small());
                            ui.end_row();
                        }

                        ui.label("WhatsApp dir:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.whatsapp_dir)
                                .hint_text(settings::managed_whatsapp_dir().to_string_lossy().as_ref())
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();

                        ui.label("WhatsApp recipient:");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.settings.whatsapp_recipient)
                                    .hint_text("phone (41791234567) or group JID (…@g.us)")
                                    .desired_width(ui.available_width() - 90.0),
                            );
                            let label = if self.wa_picker_loading { "Loading…" } else { "📞 Browse" };
                            let pick_btn = ui.add_enabled(
                                self.wa_provisioned && self.wa_linked && !self.wa_picker_loading,
                                egui::Button::new(label),
                            ).on_hover_text("Browse linked WhatsApp groups");
                            if pick_btn.clicked() { self.start_wa_pick_recipient(); }
                        });
                        ui.end_row();

                        ui.label("WhatsApp status:");
                        let status = if self.settings.whatsapp_dir.trim().is_empty() {
                            "Not configured".to_string()
                        } else if !self.wa_provisioned {
                            "Not provisioned — click Setup".to_string()
                        } else if !self.wa_linked {
                            "Not linked — click Link".to_string()
                        } else {
                            "✅ Provisioned and linked".to_string()
                        };
                        ui.label(RichText::new(status).small());
                        ui.end_row();

                        ui.label("");
                        ui.horizontal(|ui| {
                            let setup_label = if self.wa_setup_running { "Setting up…" } else { "Setup WhatsApp" };
                            let setup_btn = ui.add_enabled(
                                !self.wa_setup_running && !self.wa_login_running,
                                egui::Button::new(setup_label),
                            ).on_hover_text("Write helper scripts and run npm install in the WhatsApp dir");
                            if setup_btn.clicked() { self.start_wa_setup(); }

                            let link_label = if self.wa_login_running { "Linking…" } else { "Link WhatsApp" };
                            let link_btn = ui.add_enabled(
                                self.wa_provisioned && !self.wa_setup_running && !self.wa_login_running,
                                egui::Button::new(link_label),
                            ).on_hover_text("Show QR code so you can link this device with WhatsApp on your phone");
                            if link_btn.clicked() { self.start_wa_login(); }

                            if !self.wa_provisioned {
                                ui.label(RichText::new("(run Setup first)").weak().small());
                            }
                        });
                        ui.end_row();
                        if let Some(msg) = &self.wa_status_msg {
                            ui.label("");
                            ui.label(RichText::new(msg).small());
                            ui.end_row();
                        }

                        ui.label("davaz.com token:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.settings.davaz_token)
                                .password(true)
                                .hint_text("Bearer token from etc/api_tokens on davaz.com")
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();

                        ui.label("davaz.com tag color:");
                        ui.label(
                            RichText::new(
                                "Auto-detected by the server from the YouTube description. \
                                 Write a recognized color name (e.g. yellow / purple / red) \
                                 anywhere in the description text.",
                            )
                            .small()
                            .weak(),
                        );
                        ui.end_row();
                    });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        if let Err(e) = self.settings.save() {
                            self.last_error = Some(format!("save settings: {}", e));
                        } else {
                            self.last_error = None;
                            self.append_log("Settings saved.".into());
                        }
                    }
                    let signin_label = if self.signing_in { "Signing in…" } else if self.signed_in { "Re-sign in to YouTube" } else { "Sign in to YouTube" };
                    let signin = ui.add_enabled(!self.signing_in, egui::Button::new(signin_label));
                    if signin.clicked() { self.start_signin(); }
                });
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                ui.label(RichText::new("Updates").strong());
                ui.horizontal(|ui| {
                    let label = if self.update_checking { "Checking…" } else { "Check for updates" };
                    if ui.add_enabled(!self.update_checking, egui::Button::new(label)).clicked() {
                        self.trigger_update_check();
                    }
                    if let Some(msg) = &self.update_status_msg {
                        ui.label(msg);
                    } else {
                        ui.label(format!("Current version: v{}", APP_VERSION));
                    }
                });
                ui.add_space(4.0);
                ui.label(RichText::new(format!("Token cached at: {}", token_path().display())).weak());
            });
        self.show_settings = open;
    }
}
