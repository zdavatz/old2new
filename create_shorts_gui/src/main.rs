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
mod mpv;
mod oauth;
mod pdf;
mod pipeline;
mod preview_server;
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

/// Open the split preview: the **full original video on top** and the edited
/// clip below, stacked in one browser page opened in the default browser.
///
/// The top pane embeds the source video straight from YouTube (`source_id`,
/// deep-linked to `start_secs`) so Jürg sees the whole, untrimmed original —
/// not the downloaded start–end segment, which to him already counts as "cut".
/// The bottom pane is the local edited clip as a `<video controls>` (HTML5
/// controls always show mm:ss / mm:ss; we also render our own readout and add
/// −1s/−1f/+1f/+1s step buttons, and letterbox a small black gutter so the
/// timeline never covers burned-in subtitles). When there's no embeddable id
/// (`source_id` empty) the top pane falls back to the local `original` segment
/// file; if that also equals `edited` (no edits at all) the page shows one pane.
fn open_split_preview(
    original: &std::path::Path,
    edited: &std::path::Path,
    source_id: &str,
    start_secs: u32,
) -> Result<(), String> {
    // The edited clip lives in the segments cache dir, so we drop the page next
    // to it and reference the local video by its (percent-encoded) bare
    // filename — a relative reference resolved against the server root.
    let html_dir = edited
        .parent()
        .ok_or_else(|| "edited clip has no parent directory".to_string())?;

    // Serve the page over http://127.0.0.1 rather than opening it as a file://
    // URL: the YouTube `<iframe>` embed on top rejects a file:// origin with
    // "Fehler 153". The server also byte-serves the local edited clip (Range
    // support) so it plays + scrubs in Safari/Chrome.
    let port = preview_server::ensure_started(html_dir.to_path_buf())
        .map_err(|e| format!("start preview server: {}", e))?;
    let base = format!("http://127.0.0.1:{}", port);

    let edited_src = video_src(edited, html_dir);
    let original_src = video_src(original, html_dir);
    let embed_url = if source_id.is_empty() {
        None
    } else {
        // `enablejsapi=1` lets our step buttons drive the player; `origin` must
        // match the page origin or YouTube errors (Fehler 153).
        Some(format!(
            "https://www.youtube.com/embed/{}?start={}&rel=0&modestbranding=1&enablejsapi=1&origin={}",
            source_id, start_secs, base
        ))
    };
    let same = source_id.is_empty() && original == edited;

    let html = build_preview_html(embed_url.as_deref(), &original_src, &edited_src, same);
    let html_path = html_dir.join("_preview.html");
    std::fs::write(&html_path, html).map_err(|e| format!("write preview page: {}", e))?;
    open::that(format!("{}/_preview.html", base)).map_err(|e| e.to_string())
}

/// `src` for a local video referenced from the preview page in `html_dir`.
/// Sibling (the normal case) → percent-encoded filename (relative); otherwise
/// an absolute `file://` URL.
fn video_src(file: &std::path::Path, html_dir: &std::path::Path) -> String {
    if file.parent() == Some(html_dir) {
        if let Some(name) = file.file_name() {
            return urlencoding::encode(&name.to_string_lossy()).into_owned();
        }
    }
    path_to_file_url(file)
}

/// Absolute `file://` URL, percent-encoding everything except path separators
/// and the drive colon (so Windows `C:/…` stays intact).
fn path_to_file_url(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !s.starts_with('/') {
        out.push('/'); // Windows drive paths: file:///C:/…
    }
    for b in s.bytes() {
        match b {
            b'/' | b':' | b'-' | b'_' | b'.' | b'~'
            | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' => out.push(b as char),
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Build the self-contained preview page. Top pane = the full original (a
/// YouTube `<iframe>` embed when `embed_url` is set, else the local `original`
/// segment as a `<video>`); bottom pane = the local edited clip as a `<video>`
/// with mm:ss readout + step buttons + letterbox gutter.
fn build_preview_html(embed_url: Option<&str>, original_src: &str, edited_src: &str, same: bool) -> String {
    let css = "\
* { box-sizing: border-box; }\n\
html, body { margin:0; padding:0; height:100%; background:#0b0b0d; color:#e8e8ea;\n\
  font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;\n\
  overflow:hidden; }\n\
.wrap { display:flex; flex-direction:column; height:100vh; width:100vw; }\n\
.pane { flex:1 1 0; display:flex; flex-direction:column; min-height:0; }\n\
.pane + .pane { border-top:2px solid #000; }\n\
.bar { display:flex; align-items:center; gap:12px; padding:8px 14px; background:#161619;\n\
  border-bottom:1px solid #26262b; }\n\
.badge { display:inline-flex; align-items:center; justify-content:center; width:22px; height:22px;\n\
  border-radius:50%; background:#3b82f6; color:#fff; font-size:13px; font-weight:700; flex:0 0 auto; }\n\
.name { font-size:15px; font-weight:600; }\n\
.sub { font-size:12px; color:#9a9aa2; }\n\
.steps { margin-left:auto; display:flex; gap:4px; align-items:center; flex:0 0 auto; }\n\
.steps button { font:600 12px/1 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Arial,sans-serif;\n\
  color:#e8e8ea; background:#26262b; border:1px solid #3a3a42; border-radius:6px;\n\
  padding:6px 9px; cursor:pointer; min-width:36px; }\n\
.steps button:hover { background:#33333b; }\n\
.steps button:active { background:#3b82f6; border-color:#3b82f6; color:#fff; }\n\
.time { margin-left:12px; font-variant-numeric:tabular-nums; font-size:15px; font-weight:600;\n\
  color:#fff; background:#000; padding:3px 10px; border-radius:6px; letter-spacing:.5px; flex:0 0 auto; }\n\
.vidwrap { flex:1 1 0; min-height:0; display:flex; align-items:flex-start; justify-content:center;\n\
  background:#000; overflow:hidden; }\n\
video { width:100%; height:100%; object-fit:contain; object-position:center top; background:#000; display:block; }\n\
.vidwrap.embed { display:block; }\n\
.vidwrap.embed iframe { width:100%; height:100%; border:0; display:block; background:#000; }\n";

    let pane = |badge: &str, name: &str, sub: &str, vid_id: &str, time_id: &str, src: &str| -> String {
        format!(
            "<div class=\"pane\">\n\
  <div class=\"bar\">\n\
    <span class=\"badge\">{badge}</span>\n\
    <span class=\"name\">{name}</span>\n\
    <span class=\"sub\">{sub}</span>\n\
    <span class=\"steps\">\n\
      <button data-v=\"{vid_id}\" data-d=\"-1\" title=\"Back 1 second\">−1s</button>\n\
      <button data-v=\"{vid_id}\" data-d=\"-0.04\" title=\"Back one frame\">−1f</button>\n\
      <button data-v=\"{vid_id}\" data-d=\"0.04\" title=\"Forward one frame\">+1f</button>\n\
      <button data-v=\"{vid_id}\" data-d=\"1\" title=\"Forward 1 second\">+1s</button>\n\
    </span>\n\
    <span class=\"time\" id=\"{time_id}\">0:00 / 0:00</span>\n\
  </div>\n\
  <div class=\"vidwrap\">\n\
    <video id=\"{vid_id}\" src=\"{src}\" controls preload=\"metadata\" playsinline></video>\n\
  </div>\n\
</div>",
            badge = badge, name = name, sub = sub, time_id = time_id, vid_id = vid_id, src = src,
        )
    };

    // Top pane showing the full original video straight from YouTube. The step
    // buttons drive YouTube's player via its IFrame API (`data-yt`), and we poll
    // the player for the mm:ss readout. If the embed can't load (offline, or a
    // content-blocker extension eats youtube.com/iframe_api), the JS reveals the
    // hidden local `<video id="v1">` fallback (the original as downloaded) so the
    // pane always shows a playing video with an mm:ss readout.
    let embed_pane = |badge: &str, name: &str, sub: &str, url: &str, fallback: &str| -> String {
        format!(
            "<div class=\"pane\">\n\
  <div class=\"bar\">\n\
    <span class=\"badge\">{badge}</span>\n\
    <span class=\"name\">{name}</span>\n\
    <span class=\"sub\">{sub}</span>\n\
    <span class=\"steps\">\n\
      <button data-yt=\"1\" data-d=\"-1\" title=\"Back 1 second\">−1s</button>\n\
      <button data-yt=\"1\" data-d=\"-0.04\" title=\"Back one frame\">−1f</button>\n\
      <button data-yt=\"1\" data-d=\"0.04\" title=\"Forward one frame\">+1f</button>\n\
      <button data-yt=\"1\" data-d=\"1\" title=\"Forward 1 second\">+1s</button>\n\
    </span>\n\
    <span class=\"time\" id=\"tyt\">0:00 / 0:00</span>\n\
  </div>\n\
  <div class=\"vidwrap embed\">\n\
    <iframe id=\"ytplayer\" src=\"{url}\" allow=\"fullscreen; encrypted-media; picture-in-picture\" allowfullscreen></iframe>\n\
    <video id=\"v1\" src=\"{fallback}\" controls preload=\"none\" playsinline style=\"display:none\"></video>\n\
  </div>\n\
</div>",
            badge = badge, name = name, sub = sub, url = url.replace('&', "&amp;"), fallback = fallback,
        )
    };

    let top = match embed_url {
        Some(url) => embed_pane(
            "1", "Original (full video)",
            "the whole original, straight from YouTube",
            url, original_src,
        ),
        None => pane("1", "Original", "as downloaded", "v1", "t1", original_src),
    };

    let body = if same {
        pane("▶", "Preview", "no edits applied", "v1", "t1", edited_src)
    } else {
        format!(
            "{}\n{}",
            top,
            pane("2", "Edited cut", "the version we'd upload", "v2", "t2", edited_src),
        )
    };

    // Template kept free of `{}` so the JS below survives .replace() untouched.
    let template = "<!doctype html>\n\
<html lang=\"en\">\n\
<head>\n\
<meta charset=\"utf-8\">\n\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
<title>Preview — Original vs Edited</title>\n\
<style>__CSS__</style>\n\
</head>\n\
<body>\n\
<div class=\"wrap\">\n\
__BODY__\n\
</div>\n\
<script>\n\
function fmt(t){ if(!isFinite(t)||t<0){t=0;} t=Math.floor(t); var m=Math.floor(t/60), s=t%60; return m+':'+(s<10?'0':'')+s; }\n\
var GUT=48;\n\
function layout(v){ if(!v||!v.videoWidth){return;} var w=v.parentElement; var availW=w.clientWidth, availH=w.clientHeight; var usableH=Math.max(availH-GUT,40); var s=Math.min(availW/v.videoWidth, usableH/v.videoHeight); var dw=Math.round(v.videoWidth*s), dh=Math.round(v.videoHeight*s); v.style.width=dw+'px'; v.style.height=(dh+GUT)+'px'; }\n\
function wire(vidId,outId){ var v=document.getElementById(vidId), o=document.getElementById(outId); if(!v){return;} function u(){ if(o){o.textContent=fmt(v.currentTime)+' / '+fmt(v.duration);} } function ll(){ layout(v); } v.addEventListener('loadedmetadata',function(){u();ll();}); v.addEventListener('timeupdate',u); v.addEventListener('durationchange',u); v.addEventListener('seeked',u); window.addEventListener('resize',ll); u(); ll(); }\n\
function step(vidId,delta){ var v=document.getElementById(vidId); if(!v){return;} v.pause(); var t=v.currentTime+delta; if(t<0){t=0;} var d=v.duration; if(isFinite(d)&&t>d){t=d;} v.currentTime=t; }\n\
var ytPlayer=null, ytReady=false, ytFallback=false;\n\
function ytTick(){ var o=document.getElementById('tyt'); if(!o||!ytPlayer||!ytPlayer.getCurrentTime){return;} o.textContent=fmt(ytPlayer.getCurrentTime())+' / '+fmt(ytPlayer.getDuration()); }\n\
function ytStep(delta){ if(!ytPlayer||!ytPlayer.seekTo){return;} try{ytPlayer.pauseVideo();}catch(e){} var t=Math.max(0,(ytPlayer.getCurrentTime()||0)+delta); var d=ytPlayer.getDuration()||0; if(d){t=Math.min(t,d);} ytPlayer.seekTo(t,true); setTimeout(ytTick,120); }\n\
function useFallback(){ if(ytFallback){return;} ytFallback=true; var f=document.getElementById('ytplayer'); if(f){f.style.display='none';} var v=document.getElementById('v1'); if(v){ v.style.display='block'; try{v.load();}catch(e){} wire('v1','tyt'); } }\n\
window.onYouTubeIframeAPIReady=function(){ try{ ytPlayer=new YT.Player('ytplayer',{events:{'onReady':function(){ ytReady=true; ytTick(); setInterval(ytTick,250); },'onError':function(){ useFallback(); }}}); }catch(e){ useFallback(); } };\n\
if(document.getElementById('ytplayer')){ var _yt=document.createElement('script'); _yt.src='https://www.youtube.com/iframe_api'; _yt.onerror=function(){ useFallback(); }; document.head.appendChild(_yt); setTimeout(function(){ if(!ytReady){ useFallback(); } }, 6000); }\n\
Array.prototype.forEach.call(document.querySelectorAll('button[data-d]'), function(b){ b.addEventListener('click', function(){ var dd=parseFloat(b.getAttribute('data-d')); if(b.getAttribute('data-yt')){ if(ytFallback){ step('v1', dd); } else { ytStep(dd); } } else { step(b.getAttribute('data-v'), dd); } }); });\n\
if(!document.getElementById('ytplayer')){ wire('v1','t1'); } wire('v2','t2');\n\
</script>\n\
</body>\n\
</html>";

    template.replace("__CSS__", css).replace("__BODY__", &body)
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

/// In-window preview of the **final edited clip only** (the raw original is
/// watched separately via "Load original"). A single mpv player rendered to an
/// egui texture via libmpv's software render API.
struct PreviewState {
    player: mpv::Player,
    tex: Option<egui::TextureHandle>,
    title: String,
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
    /// Runtime-loaded libmpv (dlopen). `None` if mpv isn't installed — the
    /// Preview then falls back to the browser split view.
    mpv_lib: Option<Arc<mpv::MpvLib>>,
    mpv_load_err: Option<String>,
    /// Active in-window preview (two mpv players), or `None` when idle.
    preview: Option<PreviewState>,
    /// The "watch the original" viewer: an mpv player of the raw start–end
    /// segment, shown in a side panel so the user can scrub it and read cut
    /// timestamps while editing. `source_start_secs` makes the readout show
    /// *absolute* source time (segment offset + start).
    source_player: Option<mpv::Player>,
    source_tex: Option<egui::TextureHandle>,
    source_start_secs: f64,
    /// `brew install mpv` is running (from the "install mpv" banner).
    mpv_installing: bool,
    mpv_install_rx: Option<Receiver<BrewEvent>>,
    /// Homebrew path, probed ONCE at startup (and on Re-check). The banners are
    /// drawn every frame, so calling `deps::brew_path()` there would spawn
    /// `brew --version` 60×/s and stutter the UI — cache it instead.
    brew: Option<String>,
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

/// mm:ss for the preview readouts (matches the old browser page's format).
fn fmt_time(t: f64) -> String {
    let t = if t.is_finite() && t > 0.0 { t as u64 } else { 0 };
    format!("{}:{:02}", t / 60, t % 60)
}

/// mm:ss.s (tenths) for the "watch the original" readout, so the shown time
/// matches the precision users type into the cut fields (e.g. `10:02.6`).
fn fmt_time_tenths(t: f64) -> String {
    let t = if t.is_finite() && t > 0.0 { t } else { 0.0 };
    let m = (t / 60.0) as u64;
    let s = t - (m as f64) * 60.0;
    format!("{}:{:04.1}", m, s)
}

/// One preview pane: the mpv frame as an egui image, a play/pause button, an
/// mm:ss readout, and a seek timeline. `overlay` (original pane only) shades
/// the hand-entered cut ranges red and flashes a "✂ cut out" banner over the
/// picture whenever the playhead is inside a removed range.
fn draw_video_pane(
    ui: &mut egui::Ui,
    player: &mut mpv::Player,
    tex: &mut Option<egui::TextureHandle>,
    badge: &str,
    label: &str,
    overlay: Option<(&[(f64, f64)], f64)>,
) {
    // Size the box to the real video aspect (16:9 until the first frame lands),
    // capped so it stays sane on very wide windows.
    let aspect = player
        .video_size()
        .map(|(w, h)| w as f32 / h as f32)
        .filter(|a| a.is_finite() && *a > 0.1)
        .unwrap_or(16.0 / 9.0);
    let avail_w = ui.available_width().max(160.0);
    let max_w = avail_w.min(760.0);
    let max_h = 340.0_f32;
    let (mut w, mut h) = (max_w, max_w / aspect);
    if h > max_h {
        h = max_h;
        w = max_h * aspect;
    }
    let ppp = ui.ctx().pixels_per_point();
    let pw = (w * ppp).round() as i32;
    let ph = (h * ppp).round() as i32;

    // Pull a fresh frame (if any) into the texture.
    if let Some((fw, fh, rgba)) = player.poll_frame(pw, ph) {
        let img = egui::ColorImage::from_rgba_unmultiplied([fw as usize, fh as usize], rgba);
        match tex {
            Some(t) => t.set(img, egui::TextureOptions::LINEAR),
            None => {
                *tex = Some(ui.ctx().load_texture(
                    format!("mpv-pane-{}", badge),
                    img,
                    egui::TextureOptions::LINEAR,
                ))
            }
        }
    }

    let pos = player.time_pos();
    let dur = {
        let d = player.duration();
        if d.is_finite() && d > 0.0 {
            d
        } else {
            overlay.map(|(_, s)| s).unwrap_or(0.0)
        }
    };

    ui.vertical_centered(|ui| {
        // Label row above the frame.
        ui.allocate_ui(egui::vec2(w, 18.0), |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!(" {} ", badge))
                        .strong()
                        .color(egui::Color32::WHITE)
                        .background_color(egui::Color32::from_rgb(59, 130, 246)),
                );
                ui.label(
                    RichText::new(label)
                        .small()
                        .color(egui::Color32::from_gray(195)),
                );
            });
        });
        // The frame itself.
        let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
        ui.painter().rect_filled(rect, 2.0, egui::Color32::BLACK);
        if let Some(t) = tex.as_ref() {
            ui.painter().image(
                t.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        // "✂ cut out" banner when the playhead is inside a removed range.
        if let Some((cuts, _)) = overlay {
            if cuts.iter().any(|(a, b)| pos >= *a && pos < *b) {
                let banner =
                    egui::Rect::from_min_size(rect.left_top(), egui::vec2(rect.width(), 22.0));
                ui.painter().rect_filled(
                    banner,
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(200, 40, 40, 190),
                );
                ui.painter().text(
                    banner.center(),
                    egui::Align2::CENTER_CENTER,
                    "✂ cut out",
                    egui::FontId::proportional(13.0),
                    egui::Color32::WHITE,
                );
            }
        }
        // Controls row.
        ui.allocate_ui(egui::vec2(w, 26.0), |ui| {
            ui.horizontal(|ui| {
                let sym = if player.is_paused() { "▶ Play" } else { "⏸ Pause" };
                if ui.button(sym).clicked() {
                    player.toggle_pause();
                }
                ui.label(
                    RichText::new(format!("{} / {}", fmt_time(pos), fmt_time(dur)))
                        .monospace()
                        .color(egui::Color32::from_gray(215)),
                );
            });
        });
        // Timeline: seek + cut ranges + playhead.
        draw_timeline(ui, w, player, pos, dur, overlay.map(|(c, _)| c));
    });
}

/// A slim seek bar: grey track, blue played portion, red cut ranges, white
/// playhead. Click or drag to seek.
fn draw_timeline(
    ui: &mut egui::Ui,
    w: f32,
    player: &mut mpv::Player,
    pos: f64,
    dur: f64,
    cuts: Option<&[(f64, f64)]>,
) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 16.0), egui::Sense::click_and_drag());
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, egui::Color32::from_gray(55));
    if dur > 0.0 {
        let frac = (pos / dur).clamp(0.0, 1.0) as f32;
        let px = rect.left() + frac * rect.width();
        painter.rect_filled(
            egui::Rect::from_min_max(rect.left_top(), egui::pos2(px, rect.bottom())),
            3.0,
            egui::Color32::from_rgb(59, 130, 246),
        );
        if let Some(cuts) = cuts {
            for (a, b) in cuts {
                let x0 = rect.left() + (*a / dur).clamp(0.0, 1.0) as f32 * rect.width();
                let x1 = rect.left() + (*b / dur).clamp(0.0, 1.0) as f32 * rect.width();
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(x0, rect.top()),
                        egui::pos2(x1.max(x0 + 1.0), rect.bottom()),
                    ),
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(210, 50, 50, 205),
                );
            }
        }
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(px - 1.0, rect.top() - 2.0),
                egui::pos2(px + 1.0, rect.bottom() + 2.0),
            ),
            0.0,
            egui::Color32::WHITE,
        );
    }
    if (resp.clicked() || resp.dragged()) && dur > 0.0 {
        if let Some(p) = resp.interact_pointer_pos() {
            let frac = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64;
            player.seek_absolute(frac * dur);
        }
    }
}

/// The "watch the original" pane: the raw segment as an egui image, a big
/// **absolute source-time** readout (segment offset + start, tenths precision)
/// so the shown time matches what the user types into the cut fields, the
/// hand-entered cut ranges overlaid, and play/pause + frame-step + seek.
fn draw_source_pane(
    ui: &mut egui::Ui,
    player: &mut mpv::Player,
    tex: &mut Option<egui::TextureHandle>,
    start_secs: f64,
    cuts: &[(f64, f64)],
    seg_secs: f64,
) {
    let aspect = player
        .video_size()
        .map(|(w, h)| w as f32 / h as f32)
        .filter(|a| a.is_finite() && *a > 0.1)
        .unwrap_or(16.0 / 9.0);
    let w = ui.available_width().max(160.0);
    // Leave room below the frame for the readout + controls + timeline.
    let h = (w / aspect).min((ui.available_height() - 96.0).max(120.0));
    let ppp = ui.ctx().pixels_per_point();
    let pw = (w * ppp).round() as i32;
    let ph = (h * ppp).round() as i32;

    if let Some((fw, fh, rgba)) = player.poll_frame(pw, ph) {
        let img = egui::ColorImage::from_rgba_unmultiplied([fw as usize, fh as usize], rgba);
        match tex {
            Some(t) => t.set(img, egui::TextureOptions::LINEAR),
            None => *tex = Some(ui.ctx().load_texture("mpv-source", img, egui::TextureOptions::LINEAR)),
        }
    }

    let pos = player.time_pos();
    let dur = {
        let d = player.duration();
        if d.is_finite() && d > 0.0 { d } else { seg_secs }
    };

    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
    ui.painter().rect_filled(rect, 2.0, egui::Color32::BLACK);
    if let Some(t) = tex.as_ref() {
        ui.painter().image(
            t.id(),
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    }
    if cuts.iter().any(|(a, b)| pos >= *a && pos < *b) {
        let banner = egui::Rect::from_min_size(rect.left_top(), egui::vec2(rect.width(), 22.0));
        ui.painter().rect_filled(banner, 0.0, egui::Color32::from_rgba_unmultiplied(200, 40, 40, 190));
        ui.painter().text(
            banner.center(),
            egui::Align2::CENTER_CENTER,
            "✂ cut out",
            egui::FontId::proportional(13.0),
            egui::Color32::WHITE,
        );
    }

    // Big absolute-time readout — the value to copy into a cut field.
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Source time:").strong());
        ui.label(
            RichText::new(fmt_time_tenths(start_secs + pos))
                .monospace()
                .size(22.0)
                .strong()
                .color(egui::Color32::from_rgb(40, 110, 210)),
        );
        ui.label(
            RichText::new(format!("(segment {} / {})", fmt_time(pos), fmt_time(dur)))
                .small()
                .weak(),
        );
    });

    ui.horizontal(|ui| {
        let sym = if player.is_paused() { "▶ Play" } else { "⏸ Pause" };
        if ui.button(sym).clicked() {
            player.toggle_pause();
        }
        if ui.button("−1s").on_hover_text("Back 1 second").clicked() {
            player.seek_absolute((pos - 1.0).max(0.0));
        }
        if ui.button("−1f").on_hover_text("Back one frame").clicked() {
            player.seek_absolute((pos - 0.04).max(0.0));
        }
        if ui.button("+1f").on_hover_text("Forward one frame").clicked() {
            player.seek_absolute(pos + 0.04);
        }
        if ui.button("+1s").on_hover_text("Forward 1 second").clicked() {
            player.seek_absolute(pos + 1.0);
        }
    });

    draw_timeline(ui, w, player, pos, dur, Some(cuts));
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
        let (mpv_lib, mpv_load_err) = match mpv::MpvLib::load() {
            Ok(l) => (Some(Arc::new(l)), None),
            Err(e) => (None, Some(e)),
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
            mpv_lib,
            mpv_load_err,
            preview: None,
            source_player: None,
            source_tex: None,
            source_start_secs: 0.0,
            mpv_installing: false,
            mpv_install_rx: None,
            brew: deps::brew_path(),
        };
        if let Some(e) = &s.mpv_load_err {
            s.append_log(format!(
                "In-window video preview unavailable ({}). Preview will open in the browser instead. Install mpv to enable it.",
                e
            ));
        }
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

    /// Install mpv (which provides libmpv) via Homebrew, streaming to the log.
    /// On success we re-dlopen libmpv so the in-window preview lights up with
    /// no restart. Kept separate from `start_brew_install` because mpv is
    /// optional — its absence only downgrades Preview to the browser.
    fn start_mpv_install(&mut self) {
        if self.mpv_installing { return; }
        let Some(brew) = deps::brew_path() else {
            self.last_error = Some("Homebrew not installed".into());
            return;
        };
        self.last_error = None;
        let (tx, rx) = unbounded::<BrewEvent>();
        self.mpv_install_rx = Some(rx);
        self.mpv_installing = true;
        self.append_log(format!("Running: {} install mpv", brew));
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            use std::process::{Command, Stdio};
            let mut cmd = Command::new(&brew);
            cmd.arg("install").arg("mpv");
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

    /// Retry loading libmpv (after the user installed mpv or clicked Re-check).
    fn recheck_mpv(&mut self) {
        match mpv::MpvLib::load() {
            Ok(l) => {
                self.mpv_lib = Some(Arc::new(l));
                self.mpv_load_err = None;
                self.append_log("mpv found — Preview will now play in the window.".into());
            }
            Err(e) => {
                self.mpv_load_err = Some(e.clone());
                self.append_log(format!("mpv still not found: {}", e));
            }
        }
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

    fn start_job(&mut self) { self.kick_off(false, false); }
    fn start_preview(&mut self) { self.kick_off(true, false); }
    /// Download the start–end segment and open it in the in-window viewer so
    /// the user can scrub it and read off cut-point timestamps before editing.
    fn start_load_source(&mut self) { self.kick_off(false, true); }

    /// Snapshot the currently-entered cut ranges as offsets (seconds) from the
    /// segment start, clamped to the segment. Empty when "Remove middle
    /// section(s)" is off. These paint red over the original's timeline.
    fn preview_cut_ranges(&self, seg_secs: f64) -> Vec<(f64, f64)> {
        if !self.form.cut_middle {
            return Vec::new();
        }
        let start = pipeline::parse_timestamp(&self.form.start).unwrap_or(0.0);
        let mut out = Vec::new();
        for c in &self.form.cuts {
            let (f, t) = (c.from.trim(), c.till.trim());
            if f.is_empty() || t.is_empty() {
                continue;
            }
            if let (Some(fa), Some(ta)) = (pipeline::parse_timestamp(f), pipeline::parse_timestamp(t)) {
                let rf = (fa - start).clamp(0.0, seg_secs);
                let rt = (ta - start).clamp(0.0, seg_secs);
                if rt > rf {
                    out.push((rf, rt));
                }
            }
        }
        out
    }

    /// Build the in-window preview: an mpv player for the original segment and
    /// one for the edited clip, each showing its first frame. Stored in
    /// `self.preview` and drawn in the central panel.
    fn open_in_window_preview(
        &mut self,
        ctx: &egui::Context,
        edited: &std::path::Path,
    ) -> Result<(), String> {
        let lib = self
            .mpv_lib
            .clone()
            .ok_or_else(|| "libmpv not loaded".to_string())?;
        // Drop any prior preview first.
        self.preview = None;
        let player = mpv::Player::open(lib, ctx, &edited.to_string_lossy())
            .map_err(|e| format!("edited: {}", e))?;
        self.preview = Some(PreviewState {
            player,
            tex: None,
            title: self.form.title.trim().to_string(),
        });
        Ok(())
    }

    /// Draw the in-window preview panes (original on top, edited below) with
    /// per-pane play/pause, mm:ss readout, a seek timeline, and the cut-range
    /// overlay. No-op when no preview is active.
    /// Draw the "watch the original" side panel: the raw segment playing so the
    /// user can scrub it, with an absolute source-time readout and the live cut
    /// overlay. No-op when no original is loaded.
    fn draw_source_viewer(&mut self, ui: &mut egui::Ui) {
        let playing = self.source_player.as_ref().map(|p| !p.is_paused()).unwrap_or(false);
        if playing {
            ui.ctx().request_repaint();
        }
        // Compute the overlay ranges before borrowing the player mutably.
        let dur = self.source_player.as_ref().map(|p| p.duration()).unwrap_or(0.0);
        let seg_secs = if dur > 0.0 {
            dur
        } else {
            let s = pipeline::parse_timestamp(&self.form.start).unwrap_or(0.0);
            let e = pipeline::parse_timestamp(&self.form.end).unwrap_or(s);
            (e - s).max(0.0)
        };
        let cuts = self.preview_cut_ranges(seg_secs);
        let start = self.source_start_secs;

        let mut close = false;
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("👁 Original — find your cut points").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("✖ Close").clicked() {
                    close = true;
                }
            });
        });
        ui.label(
            RichText::new("Scrub the video, read “Source time”, and type it into a Cut out from/till field.")
                .small()
                .weak(),
        );
        ui.add_space(4.0);
        if let Some(player) = self.source_player.as_mut() {
            draw_source_pane(ui, player, &mut self.source_tex, start, &cuts, seg_secs);
        }
        if close {
            self.source_player = None;
            self.source_tex = None;
        }
    }

    /// Draw the in-window preview of the **final edited clip only** (a single
    /// pane). The raw original is watched separately via "Load original".
    fn draw_preview(&mut self, ui: &mut egui::Ui) {
        let Some(preview) = self.preview.as_mut() else { return; };
        if !preview.player.is_paused() {
            ui.ctx().request_repaint();
        }
        let mut close = false;
        ui.add_space(8.0);
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(18, 18, 22))
            .inner_margin(8.0)
            .rounding(4.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("▶ Preview — edited clip").strong().color(egui::Color32::WHITE));
                    if !preview.title.is_empty() {
                        ui.label(
                            RichText::new(&preview.title)
                                .small()
                                .color(egui::Color32::from_gray(170)),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("✖ Close").clicked() {
                            close = true;
                        }
                    });
                });
                ui.add_space(6.0);
                draw_video_pane(
                    ui,
                    &mut preview.player,
                    &mut preview.tex,
                    "▶",
                    "The final cut we'd upload",
                    None,
                );
            });
        if close {
            self.preview = None;
        }
    }

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

    fn kick_off(&mut self, preview_only: bool, source_only: bool) {
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
        // Cuts / title / sign-in aren't needed to just watch the original.
        if self.form.cut_middle && !source_only {
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
        if !preview_only && !source_only && self.form.title.trim().is_empty() {
            self.last_error = Some("Title is required".into());
            return;
        }
        if !preview_only && !source_only && !self.signed_in {
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
            source_only,
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

    fn drain_events(&mut self, ctx: &egui::Context) {
        // Clone the Receiver so the loop doesn't hold an immutable borrow of
        // `self` — the Preview arm needs `&mut self` to open the mpv players.
        if let Some(rx) = self.rx.clone() {
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
                    Event::Preview { original, edited, source_id, start_secs } => {
                        // Prefer the in-window mpv preview; fall back to the
                        // browser split view if libmpv isn't available or a
                        // player fails to open.
                        let mut used_window = false;
                        if self.mpv_lib.is_some() {
                            match self.open_in_window_preview(ctx, &edited) {
                                Ok(()) => {
                                    used_window = true;
                                    self.append_log(
                                        "Preview ready — the edited clip is playing in the window.".into(),
                                    );
                                }
                                Err(e) => self.append_log(format!(
                                    "In-window preview failed ({}); opening in the browser instead.",
                                    e
                                )),
                            }
                        }
                        if !used_window {
                            if source_id.is_empty() {
                                self.append_log(format!("Opening split preview — original (top): {}", original.display()));
                            } else {
                                self.append_log(format!(
                                    "Opening split preview — full original {} (top), edited (bottom): {}",
                                    source_id, edited.display()
                                ));
                            }
                            if let Err(e) = open_split_preview(&original, &edited, &source_id, start_secs) {
                                self.last_error = Some(format!(
                                    "Could not open preview ({}). Edited clip at {}",
                                    e,
                                    edited.display()
                                ));
                            }
                        }
                        still_running = false;
                    }
                    Event::SourceReady { path, start_secs } => {
                        // Open the raw segment in the in-window viewer (if mpv
                        // is available) so the user can scrub for cut points.
                        if let Some(lib) = self.mpv_lib.clone() {
                            match mpv::Player::open(lib, ctx, &path.to_string_lossy()) {
                                Ok(p) => {
                                    self.source_player = Some(p);
                                    self.source_tex = None;
                                    self.source_start_secs = start_secs;
                                    self.append_log("Original loaded — scrub it to find cut points.".into());
                                }
                                Err(e) => {
                                    self.last_error = Some(format!("Could not open the original in the window ({}).", e));
                                }
                            }
                        } else {
                            // No mpv: fall back to the system player.
                            let _ = open::that(&path);
                            self.append_log(format!("Opened original in your video player: {}", path.display()));
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
                    Event::Preview { .. } => {} // not used in direct upload tab
                    Event::SourceReady { .. } => {} // not used in direct upload tab
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

        // Clone so `recheck_mpv` (&mut self) can run after the channel drains.
        if let Some(rx) = self.mpv_install_rx.clone() {
            let mut done = false;
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    BrewEvent::Log(s) => self.append_log(s),
                    BrewEvent::Done => {
                        self.append_log("mpv install finished.".into());
                        done = true;
                    }
                    BrewEvent::Error(e) => {
                        self.last_error = Some(e.clone());
                        self.append_log(format!("mpv install error: {}", e));
                        done = true;
                    }
                }
            }
            if done {
                self.mpv_installing = false;
                self.mpv_install_rx = None;
                self.recheck_mpv();
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
        self.drain_events(ctx);
        self.ensure_icon_texture(ctx);
        if self.running || self.upload_running || self.signing_in || self.brew_installing || self.mpv_installing || self.ytdlp_updating || self.update_checking || self.cookies_testing || self.installing || self.wa_sending || self.wa_setup_running || self.wa_login_running || self.davaz_posting || self.pdf_exporting || self.pdf_fetching {
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

        // "Watch the original" side panel — the raw segment playing in the same
        // window (never a popup), so the form (cut fields) stays visible beside
        // it. Added before the CentralPanel so it docks to the right edge.
        if self.source_player.is_some() {
            egui::SidePanel::right("original_viewer")
                .resizable(true)
                .default_width(520.0)
                .show(ctx, |ui| {
                    self.draw_source_viewer(ui);
                });
        }

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
                let brew = self.brew.clone();
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
                                    self.brew = deps::brew_path();
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

            // mpv (optional) banner: without libmpv the Preview can't play in
            // this window and falls back to the browser split view. Offer a
            // one-click install. Informational (not blocking) — mpv is optional.
            if self.mpv_lib.is_none() {
                let brew = self.brew.clone();
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(224, 238, 255))
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(90, 150, 220)))
                    .inner_margin(8.0)
                    .rounding(4.0)
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.colored_label(
                                egui::Color32::from_rgb(30, 80, 150),
                                "ℹ Preview opens in your browser because mpv isn't installed. Install mpv to play the preview inside this window (with the cut overlay).",
                            );
                        });
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if cfg!(target_os = "macos") {
                                if brew.is_some() {
                                    let label = if self.mpv_installing { "Installing mpv…" } else { "Install mpv with Homebrew" };
                                    let resp = ui.add_enabled(!self.mpv_installing, egui::Button::new(label));
                                    if resp.clicked() { self.start_mpv_install(); }
                                } else if ui.button("How to install Homebrew").clicked() {
                                    let _ = open::that("https://brew.sh");
                                }
                            }
                            if ui.button("Re-check").clicked() {
                                self.recheck_mpv();
                            }
                            ui.label(
                                RichText::new(if cfg!(target_os = "macos") {
                                    "or run 'brew install mpv' in Terminal"
                                } else if cfg!(target_os = "windows") {
                                    "install mpv (e.g. 'scoop install mpv' or from mpv.io), then Re-check"
                                } else {
                                    "install libmpv (e.g. 'sudo apt install -y libmpv2' or 'mpv'), then Re-check"
                                })
                                .small()
                                .color(egui::Color32::from_gray(110)),
                            );
                        });
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
                // Watch the raw original in the window first, to find cut points.
                let load_label = if self.source_player.is_some() { "👁 Reload original" } else { "👁 Load original" };
                let load_btn = ui.add_enabled(
                    !self.running && deps_ok,
                    egui::Button::new(load_label).min_size(egui::vec2(140.0, 32.0)),
                ).on_hover_text(
                    "Download the start–end segment and play it here in the window, so you can scrub it and read off the timestamps for your cut points before editing",
                );
                if load_btn.clicked() { self.start_load_source(); }
                ui.add_space(8.0);
                let preview_btn = ui.add_enabled(
                    !self.running && deps_ok,
                    egui::Button::new("Preview").min_size(egui::vec2(110.0, 32.0)),
                ).on_hover_text(if self.mpv_lib.is_some() {
                    "Build the clip and play it right here — original on top, your edited cut below — without uploading"
                } else {
                    "Download the segment and open it in your default video player without uploading"
                });
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

            self.draw_preview(ui);

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
