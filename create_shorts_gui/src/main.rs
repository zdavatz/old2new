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
mod pipeline;
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

fn main() -> eframe::Result<()> {
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

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
struct FormState {
    #[serde(default)] source: String,
    #[serde(default)] start: String,
    #[serde(default)] end: String,
    #[serde(default)] title: String,
    #[serde(default)] description: String,
    #[serde(default)] privacy: String,
}

struct App {
    settings: Settings,
    form: FormState,
    log: Arc<Mutex<Vec<String>>>,
    progress: Arc<Mutex<ProgressInfo>>,
    rx: Option<Receiver<Event>>,
    running: bool,
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
}

enum BrewEvent {
    Log(String),
    Done,
    Error(String),
}

/// "chrome (installed)" if detected, otherwise just the name.
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
        };
        s.refresh_wa_status();
        s
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
        let tag_color = davaz::detect_tag_color(&self.form.description).to_string();
        let tag_label = if tag_color.is_empty() { "no tag" } else { tag_color.as_str() };
        self.append_log(format!("Posting {} to davaz.com (tag: {})…", url, tag_label));
        self.davaz_status_msg = Some("Posting to davaz.com…".into());
        self.davaz_posting = true;
        let (tx, rx) = unbounded::<Result<davaz::PostResponse, String>>();
        self.davaz_rx = Some(rx);
        std::thread::spawn(move || {
            let result = davaz::post_video(&token, &url, &tag_color)
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

    fn start_job(&mut self) { self.kick_off(false); }
    fn start_preview(&mut self) { self.kick_off(true); }

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
        };
        let settings = self.settings.clone();
        std::thread::spawn(move || pipeline::run(job, settings, tx));
    }

    fn drain_events(&mut self) {
        if let Some(rx) = &self.rx {
            let mut still_running = true;
            while let Ok(ev) = rx.try_recv() {
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
                    Event::Preview(path) => {
                        self.append_log(format!("Opening preview: {}", path.display()));
                        if let Err(e) = open::that(&path) {
                            self.last_error = Some(format!("Could not open preview ({}). File at {}", e, path.display()));
                        }
                        still_running = false;
                    }
                    Event::Error(e) => {
                        self.last_error = Some(e.clone());
                        self.append_log(format!("ERROR: {}", e));
                        still_running = false;
                    }
                }
            }
            if !still_running {
                self.running = false;
                self.rx = None;
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
        if self.running || self.signing_in || self.brew_installing || self.update_checking || self.cookies_testing || self.installing || self.wa_sending || self.wa_setup_running || self.wa_login_running || self.davaz_posting {
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
    }
}

impl App {
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
                                "Auto-detected from description: write 'yellow' or 'purple' \
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
