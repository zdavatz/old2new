//! `create_shorts_gui` — desktop app that drives the same pipeline as
//! the `create_short` CLI: yt-dlp segment extract → resumable upload to
//! YouTube. One main form, a Settings dialog for the Google OAuth
//! client_id/secret, and a one-time browser sign-in to mint a refresh
//! token.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod deps;
mod oauth;
mod pipeline;
mod settings;
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

fn main() -> eframe::Result<()> {
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
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}

#[derive(Default, Clone)]
struct FormState {
    source: String,
    start: String,
    end: String,
    title: String,
    description: String,
    privacy: String,
}

struct App {
    settings: Settings,
    form: FormState,
    log: Arc<Mutex<Vec<String>>>,
    progress: Arc<Mutex<(u64, u64)>>,
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
}

enum BrewEvent {
    Log(String),
    Done,
    Error(String),
}

enum SignInEvent {
    Log(String),
    Done,
    Error(String),
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
    fn new() -> Self {
        let settings = Settings::load();
        let signed_in = oauth::load_token().map(|t| !t.refresh_token.is_empty()).unwrap_or(false);
        let show_settings = settings.client_id.is_empty() || settings.client_secret.is_empty();
        let mut initial_log = load_persisted_log();
        let stamp = chrono_like_now();
        let marker = format!("─── session started {} ───", stamp);
        let _ = append_to_log_file(&marker);
        initial_log.push(marker);
        let privacy = if settings.default_privacy.is_empty() {
            "public".to_string()
        } else {
            settings.default_privacy.clone()
        };
        Self {
            form: FormState { privacy, ..Default::default() },
            settings,
            log: Arc::new(Mutex::new(initial_log)),
            progress: Arc::new(Mutex::new((0, 0))),
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
        }
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
        if let Ok(mut p) = self.progress.lock() { *p = (0, 0); }

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
                    Event::Progress(s, t) => {
                        if let Ok(mut p) = self.progress.lock() { *p = (s, t); }
                    }
                    Event::Done(url) => {
                        self.last_done_url = Some(url.clone());
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
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.ensure_icon_texture(ctx);
        if self.running || self.signing_in || self.brew_installing {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        }

        egui::TopBottomPanel::top("topbar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 44.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
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

                if let Some(url) = &self.last_done_url {
                    if ui.link(format!("Open {}", url)).clicked() {
                        let _ = open::that(url);
                    }
                }
            });

            if self.running {
                let (sent, total) = *self.progress.lock().unwrap();
                if total > 0 {
                    let f = sent as f32 / total as f32;
                    ui.add(egui::ProgressBar::new(f).show_percentage().animate(true));
                    ui.label(format!("{:.1} / {:.1} MB", sent as f64 / 1_048_576.0, total as f64 / 1_048_576.0));
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
    }
}

impl App {
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
                        let mut cb = self.settings.cookies_browser.clone();
                        egui::ComboBox::from_id_salt("cookies_browser")
                            .selected_text(if cb.is_empty() { "(none)" } else { cb.as_str() })
                            .show_ui(ui, |ui| {
                                for opt in ["", "chrome", "brave", "chromium", "edge", "firefox", "opera", "safari", "vivaldi"] {
                                    let label = if opt.is_empty() { "(none)" } else { opt };
                                    ui.selectable_value(&mut cb, opt.to_string(), label);
                                }
                            });
                        self.settings.cookies_browser = cb;
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
                ui.add_space(4.0);
                ui.label(RichText::new(format!("Token cached at: {}", token_path().display())).weak());
            });
        self.show_settings = open;
    }
}
