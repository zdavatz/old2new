use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{Html, IntoResponse, Json, Response},
    routing::get,
    Router,
};
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path as StdPath, PathBuf};
use tokio::process::Command;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Default, Clone)]
struct InstanceMeta {
    #[serde(default)]
    label: String,
    #[serde(default)]
    location: String,
    #[serde(default)]
    cost_per_hr: Option<f64>,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    instance_id: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct JobMeta {
    #[serde(default)]
    video_id: String,
    #[serde(default)]
    scale: u32,
    #[serde(default)]
    title: String,
    #[serde(default)]
    display_title: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default)]
    duration_seconds: f64,
    #[serde(default)]
    fps: f64,
    #[serde(default)]
    total_frames: u64,
    #[serde(default)]
    started_at: String,
}

#[derive(Serialize, Default, Clone)]
struct GpuInfo {
    name: String,
    vram_total_mb: u64,
    vram_used_mb: u64,
    vram_free_mb: u64,
    temp_c: u64,
    util_pct: u64,
    power_draw: String,
    driver: String,
}

#[derive(Serialize, Default, Clone)]
struct CpuInfo {
    name: String,
    cores: u32,
    mhz: String,
    load_1m: f64,
    load_5m: f64,
    load_15m: f64,
}

#[derive(Serialize, Default, Clone)]
struct MemInfo {
    total_mb: u64,
    used_mb: u64,
    available_mb: u64,
}

#[derive(Serialize, Default, Clone)]
struct DiskInfo {
    total_gb: f64,
    used_gb: f64,
    free_gb: f64,
    util_pct: f64,
}

#[derive(Serialize, Default, Clone)]
struct SystemSpecs {
    gpus: Vec<GpuInfo>,
    gpu_count: usize,
    cpu: CpuInfo,
    memory: MemInfo,
    disk: DiskInfo,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct TimingInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    download: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extraction: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upscaling: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reassembly: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    youtube_upload: Option<f64>,
}

#[derive(Serialize, Default, Clone)]
struct VideoStatus {
    id: String,
    title: String,
    display_title: String,
    scale: u32,
    duration: f64,
    status: String,
    progress: f64,
    total_frames: u64,
    done_frames: u64,
    eta: String,
    resolution: String,
    timing: TimingInfo,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct UploadEntry {
    #[serde(default)]
    video_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    youtube_url: String,
    #[serde(default)]
    uploaded_at: String,
}

#[derive(Serialize, Default)]
struct StatusResponse {
    total: usize,
    done: usize,
    active: usize,
    videos: Vec<VideoStatus>,
    log_tail: String,
    timestamp: String,
    total_frames: u64,
    done_frames: u64,
    overall_pct: f64,
    fps: f64,
    per_gpu_fps: HashMap<String, f64>,
    eta: String,
    dl_speed: String,
    dl_progress: String,
    system: SystemSpecs,
    instance: InstanceMeta,
    uploads: Vec<UploadEntry>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn home_dir() -> PathBuf {
    dirs_fallback()
}

/// Get the home directory without the `dirs` crate.
fn dirs_fallback() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root"))
}

fn jobs_dir() -> PathBuf {
    home_dir().join("jobs")
}

fn count_png_files(dir: &StdPath) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.starts_with("frame_") && n.ends_with(".png"))
                .unwrap_or(false)
        })
        .count() as u64
}

fn read_file_string(path: &StdPath) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn read_json<T: serde::de::DeserializeOwned>(path: &StdPath) -> Option<T> {
    let data = read_file_string(path)?;
    serde_json::from_str(&data).ok()
}

fn read_tail(path: &StdPath, bytes: u64) -> String {
    let Ok(mut f) = fs::File::open(path) else {
        return String::new();
    };
    let Ok(meta) = f.metadata() else {
        return String::new();
    };
    let size = meta.len();
    if size > bytes {
        use std::io::Seek;
        let _ = f.seek(std::io::SeekFrom::End(-(bytes as i64)));
    }
    let mut buf = String::new();
    let _ = f.read_to_string(&mut buf);
    buf
}

/// Find enhanced MKV in a job directory. Returns the path if found.
fn find_enhanced_mkv(job_dir: &StdPath, title: &str, scale: u32) -> Option<PathBuf> {
    // Try title_Nx.mkv patterns
    for s in [scale, 2, 4] {
        let p = job_dir.join(format!("{title}_{s}x.mkv"));
        if p.exists() {
            return Some(p);
        }
    }
    // Try enhanced_Nx.mkv
    for s in [scale, 2, 4] {
        let p = job_dir.join(format!("enhanced_{s}x.mkv"));
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Infer scale from an MKV filename (e.g. "title_4x.mkv" -> 4).
fn infer_scale_from_mkv(job_dir: &StdPath) -> u32 {
    if let Ok(entries) = fs::read_dir(job_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with("_4x.mkv") {
                return 4;
            }
            if name.ends_with("_2x.mkv") {
                return 2;
            }
        }
    }
    4 // default
}

// ---------------------------------------------------------------------------
// System specs gathering
// ---------------------------------------------------------------------------

async fn get_gpu_info() -> Vec<GpuInfo> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total,memory.used,memory.free,temperature.gpu,utilization.gpu,power.draw,driver_version",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .await;

    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
            if parts.len() >= 8 {
                Some(GpuInfo {
                    name: parts[0].to_string(),
                    vram_total_mb: parts[1].parse().unwrap_or(0),
                    vram_used_mb: parts[2].parse().unwrap_or(0),
                    vram_free_mb: parts[3].parse().unwrap_or(0),
                    temp_c: parts[4].parse().unwrap_or(0),
                    util_pct: parts[5].parse().unwrap_or(0),
                    power_draw: parts[6].to_string(),
                    driver: parts[7].to_string(),
                })
            } else {
                None
            }
        })
        .collect()
}

fn get_cpu_info() -> CpuInfo {
    let mut info = CpuInfo::default();
    if let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo") {
        let mut cores = 0u32;
        for line in cpuinfo.lines() {
            if line.starts_with("model name") && info.name.is_empty() {
                if let Some(val) = line.split(':').nth(1) {
                    info.name = val.trim().to_string();
                }
            }
            if line.starts_with("cpu MHz") && info.mhz.is_empty() {
                if let Some(val) = line.split(':').nth(1) {
                    info.mhz = val.trim().to_string();
                }
            }
            if line.starts_with("processor") {
                cores += 1;
            }
        }
        info.cores = cores;
    }
    if let Ok(loadavg) = fs::read_to_string("/proc/loadavg") {
        let parts: Vec<&str> = loadavg.split_whitespace().collect();
        if parts.len() >= 3 {
            info.load_1m = parts[0].parse().unwrap_or(0.0);
            info.load_5m = parts[1].parse().unwrap_or(0.0);
            info.load_15m = parts[2].parse().unwrap_or(0.0);
        }
    }
    info
}

fn get_mem_info() -> MemInfo {
    let mut info = MemInfo::default();
    if let Ok(meminfo) = fs::read_to_string("/proc/meminfo") {
        let mut available: Option<u64> = None;
        let mut free: Option<u64> = None;
        for line in meminfo.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                match parts[0] {
                    "MemTotal:" => info.total_mb = parts[1].parse::<u64>().unwrap_or(0) / 1024,
                    "MemAvailable:" => available = Some(parts[1].parse::<u64>().unwrap_or(0) / 1024),
                    "MemFree:" => free = Some(parts[1].parse::<u64>().unwrap_or(0) / 1024),
                    _ => {}
                }
            }
        }
        info.available_mb = available.or(free).unwrap_or(0);
        info.used_mb = info.total_mb.saturating_sub(info.available_mb);
    }
    info
}

fn get_disk_info() -> DiskInfo {
    let mut info = DiskInfo::default();
    // Use statvfs via nix-style call. Simpler: just parse the numbers from libc.
    unsafe {
        let path = std::ffi::CString::new("/").unwrap();
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(path.as_ptr(), &mut stat) == 0 {
            let total = stat.f_blocks as f64 * stat.f_frsize as f64;
            let free = stat.f_bavail as f64 * stat.f_frsize as f64;
            let used = total - free;
            let gb = 1024.0 * 1024.0 * 1024.0;
            info.total_gb = (total / gb * 10.0).round() / 10.0;
            info.free_gb = (free / gb * 10.0).round() / 10.0;
            info.used_gb = (used / gb * 10.0).round() / 10.0;
            if total > 0.0 {
                info.util_pct = (used / total * 1000.0).round() / 10.0;
            }
        }
    }
    info
}

async fn get_system_specs() -> SystemSpecs {
    let gpus = get_gpu_info().await;
    let gpu_count = gpus.len();
    SystemSpecs {
        gpus,
        gpu_count,
        cpu: get_cpu_info(),
        memory: get_mem_info(),
        disk: get_disk_info(),
    }
}

// ---------------------------------------------------------------------------
// Running process detection
// ---------------------------------------------------------------------------

/// Process detection: returns (enhance_jobs, ffmpeg_jobs, upload_jobs)
/// - enhance_jobs: job names with running enhance_gpu.py or upscale.py
/// - ffmpeg_jobs: job names with running ffmpeg (assembling or extracting)
/// - upload_jobs: job names with running youtube_upload
async fn get_running_processes() -> (HashSet<String>, HashSet<String>, HashSet<String>) {
    let output = Command::new("bash")
        .args(["-c", "ps aux | grep -v grep"])
        .output()
        .await;

    let mut enhance_jobs = HashSet::new();
    let mut ffmpeg_jobs = HashSet::new();
    let mut upload_jobs = HashSet::new();

    if let Ok(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            // enhance_gpu.py or upscale.py with --job-name
            if (line.contains("enhance_gpu.py") || line.contains("upscale.py"))
                && line.contains("--job-name")
            {
                if let Some(rest) = line.split("--job-name").nth(1) {
                    if let Some(name) = rest.trim().split_whitespace().next() {
                        enhance_jobs.insert(name.to_string());
                    }
                }
            }
            // ffmpeg with frames_in or frames_out (assembling/extracting)
            if line.contains("ffmpeg") && line.contains("/jobs/") {
                // Extract job name from path like /root/jobs/TITLE/frames_out/
                if let Some(rest) = line.split("/jobs/").nth(1) {
                    if let Some(name) = rest.split('/').next() {
                        ffmpeg_jobs.insert(name.to_string());
                    }
                }
            }
            // youtube_upload with --video-id
            if line.contains("youtube_upload") && !line.contains("grep") {
                // Try to extract job name from the file path
                if let Some(rest) = line.split("/jobs/").nth(1) {
                    if let Some(name) = rest.split('/').next() {
                        upload_jobs.insert(name.to_string());
                    }
                }
            }
        }
    }
    (enhance_jobs, ffmpeg_jobs, upload_jobs)
}

// ---------------------------------------------------------------------------
// GPU worker PID files
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn get_gpu_assignments() -> HashMap<String, String> {
    // Read ~/gpu*.worker.pid files, cross-reference with running processes
    let home = home_dir();
    let mut assignments: HashMap<String, String> = HashMap::new();
    if let Ok(entries) = fs::read_dir(&home) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("gpu") && name.ends_with(".worker.pid") {
                let gpu_id = name
                    .trim_start_matches("gpu")
                    .trim_end_matches(".worker.pid")
                    .to_string();
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    let pid = content.trim().to_string();
                    // Check if PID is alive
                    let proc_path = format!("/proc/{pid}");
                    if StdPath::new(&proc_path).exists() {
                        assignments.insert(gpu_id, pid);
                    }
                }
            }
        }
    }
    assignments
}

// ---------------------------------------------------------------------------
// FPS from log files
// ---------------------------------------------------------------------------

fn parse_fps_from_logs() -> (f64, HashMap<String, f64>, String) {
    let home = home_dir();
    let mut per_gpu_fps: HashMap<String, f64> = HashMap::new();
    let mut log_tail = String::new();

    // Per-GPU logs
    if let Ok(entries) = fs::read_dir(&home) {
        let mut gpu_logs: Vec<(String, PathBuf)> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("gpu") && name.ends_with(".log") {
                    let id = name
                        .trim_start_matches("gpu")
                        .trim_end_matches(".log")
                        .to_string();
                    Some((id, e.path()))
                } else {
                    None
                }
            })
            .collect();
        gpu_logs.sort_by(|a, b| a.0.cmp(&b.0));

        for (gpu_id, path) in &gpu_logs {
            let tail = read_tail(path, 4096);
            // Parse fps: look for pattern like "1234/5678 (2.50 fps"
            let mut last_fps = None;
            for line in tail.lines() {
                if let Some(fps) = parse_fps_line(line) {
                    last_fps = Some(fps);
                }
            }
            if let Some(fps) = last_fps {
                per_gpu_fps.insert(gpu_id.clone(), fps);
            }

            // Build multi-GPU log tail
            let gfps = last_fps.map(|f| format!("{f:.1} fps")).unwrap_or("—".into());
            let last_lines: Vec<&str> = tail.lines().collect();
            let start = last_lines.len().saturating_sub(3);
            log_tail.push_str(&format!("[GPU {gpu_id}] {gfps}\n"));
            for l in &last_lines[start..] {
                log_tail.push_str(l);
                log_tail.push('\n');
            }
            log_tail.push('\n');
        }
    }

    // Fallback: single enhance.log
    if per_gpu_fps.is_empty() {
        let log_path = home.join("enhance.log");
        if log_path.exists() {
            let tail = read_tail(&log_path, 4096);
            let lines: Vec<&str> = tail.lines().collect();
            let start = lines.len().saturating_sub(30);
            log_tail = lines[start..].join("\n");
            for line in tail.lines() {
                if let Some(fps) = parse_fps_line(line) {
                    per_gpu_fps.insert("0".into(), fps);
                }
            }
        }
    }

    let total_fps: f64 = per_gpu_fps.values().sum();
    (total_fps, per_gpu_fps, log_tail)
}

fn parse_fps_line(line: &str) -> Option<f64> {
    // Match pattern: "1234/5678 (2.50 fps" or "2.50 fps"
    let idx = line.find("fps")?;
    let before = &line[..idx];
    // Walk backwards to find the number
    let trimmed = before.trim_end();
    let start = trimmed.rfind(|c: char| !c.is_ascii_digit() && c != '.')?;
    let num_str = &trimmed[start + 1..];
    num_str.parse::<f64>().ok()
}

// ---------------------------------------------------------------------------
// Queue file parsing (video_work_queue.txt)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct QueueEntry {
    video_id: String,
    scale: u32,
    title: String,
}

/// Read queue from ~/json/ directory — each JSON file is a video to process
fn read_json_queue() -> Vec<JobMeta> {
    let json_dir = home_dir().join("json");
    let mut videos = Vec::new();
    if let Ok(entries) = fs::read_dir(&json_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                if let Some(meta) = read_json::<JobMeta>(&path) {
                    videos.push(meta);
                }
            }
        }
    }
    // Sort by duration descending (longest first)
    videos.sort_by(|a, b| b.duration_seconds.partial_cmp(&a.duration_seconds).unwrap_or(std::cmp::Ordering::Equal));
    videos
}

/// Legacy: read from video_work_queue.txt (pipe-delimited) as fallback
fn read_work_queue() -> Vec<QueueEntry> {
    let path = home_dir().join("video_work_queue.txt");
    let Ok(content) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('|').collect();
            if parts.len() >= 3 {
                Some(QueueEntry {
                    video_id: parts[0].trim().to_string(),
                    scale: parts[1].trim().parse().unwrap_or(4),
                    title: parts[2].trim().to_string(),
                })
            } else {
                None
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Upload log
// ---------------------------------------------------------------------------

fn read_upload_log() -> Vec<UploadEntry> {
    let path = home_dir().join("upload_log.jsonl");
    let Ok(content) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

// ---------------------------------------------------------------------------
// Build full status
// ---------------------------------------------------------------------------

async fn build_status() -> StatusResponse {
    let jobs = jobs_dir();
    let (enhance_jobs, ffmpeg_jobs, upload_jobs) = get_running_processes().await;
    let system = get_system_specs().await;
    let instance: InstanceMeta =
        read_json(&home_dir().join("instance_meta.json")).unwrap_or_default();
    let uploads = read_upload_log();
    let (fps, per_gpu_fps, log_tail) = parse_fps_from_logs();

    // Collect all job directories
    let mut videos: Vec<VideoStatus> = Vec::new();
    let mut seen_titles: HashSet<String> = HashSet::new();

    if let Ok(entries) = fs::read_dir(&jobs) {
        let mut dirs: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        dirs.sort_by_key(|e| e.file_name());

        for entry in dirs {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let title = entry.file_name().to_string_lossy().to_string();
            let job_dir = entry.path();

            // Read job_meta.json if present
            let meta: Option<JobMeta> = read_json(&job_dir.join("job_meta.json"));

            let video_id = meta
                .as_ref()
                .map(|m| m.video_id.clone())
                .unwrap_or_default();
            let scale = meta
                .as_ref()
                .map(|m| {
                    if m.scale > 0 {
                        m.scale
                    } else {
                        infer_scale_from_mkv(&job_dir)
                    }
                })
                .unwrap_or_else(|| infer_scale_from_mkv(&job_dir));
            // Try display_title from: 1) job_meta.json 2) ~/json/{id}.json 3) dir name
            let vid_for_lookup = if video_id.is_empty() { &title } else { &video_id };
            let json_meta: Option<JobMeta> =
                read_json(&home_dir().join("json").join(format!("{}.json", vid_for_lookup)));
            let display_title = meta
                .as_ref()
                .and_then(|m| {
                    if m.display_title.is_empty() {
                        None
                    } else {
                        Some(m.display_title.clone())
                    }
                })
                .or_else(|| {
                    json_meta
                        .as_ref()
                        .and_then(|m| {
                            if m.display_title.is_empty() && !m.title.is_empty() {
                                Some(m.title.clone())
                            } else if !m.display_title.is_empty() {
                                Some(m.display_title.clone())
                            } else {
                                None
                            }
                        })
                })
                .unwrap_or_else(|| title.replace('_', " "));
            // Also get duration/resolution from json/ if not in job_meta
            let duration = meta
                .as_ref()
                .map(|m| m.duration_seconds)
                .filter(|d| *d > 0.0)
                .or_else(|| json_meta.as_ref().map(|m| m.duration_seconds))
                .unwrap_or(0.0);
            let resolution = meta
                .as_ref()
                .filter(|m| m.width > 0 && m.height > 0)
                .map(|m| format!("{}x{}", m.width, m.height))
                .or_else(|| {
                    json_meta
                        .as_ref()
                        .filter(|m| m.width > 0 && m.height > 0)
                        .map(|m| format!("{}x{}", m.width, m.height))
                })
                .unwrap_or_default();

            // Read timing.json
            let timing: TimingInfo = read_json(&job_dir.join("timing.json")).unwrap_or_default();

            // Status detection
            let frames_in_dir = job_dir.join("frames_in");
            let frames_out_dir = job_dir.join("frames_out");
            let count_in = count_png_files(&frames_in_dir);
            let count_out = count_png_files(&frames_out_dir);
            let is_enhancing = enhance_jobs.contains(&title);
            let is_ffmpeg = ffmpeg_jobs.contains(&title);
            let is_uploading = upload_jobs.contains(&title);

            let enhanced_mkv = find_enhanced_mkv(&job_dir, &title, scale);

            let (status, total_frames, done_frames, progress, eta) = if is_uploading {
                // youtube_upload process running
                let size_mb = enhanced_mkv
                    .as_ref()
                    .and_then(|p| fs::metadata(p).ok())
                    .map(|m| m.len() as f64 / (1024.0 * 1024.0))
                    .unwrap_or(0.0);
                (
                    "uploading".to_string(),
                    0u64,
                    0u64,
                    100.0,
                    format!("{:.0} MB", size_mb),
                )
            } else if enhanced_mkv.is_some() && !is_ffmpeg {
                // Enhanced MKV exists, no ffmpeg running = done
                let size_mb = enhanced_mkv
                    .as_ref()
                    .and_then(|p| fs::metadata(p).ok())
                    .map(|m| m.len() as f64 / (1024.0 * 1024.0))
                    .unwrap_or(0.0);
                (
                    "done".to_string(),
                    0u64,
                    0u64,
                    100.0,
                    format!("{:.0} MB", size_mb),
                )
            } else if frames_out_dir.exists() && count_out > 0 {
                let total = count_in.max(count_out);
                let pct = if total > 0 {
                    (count_out as f64 / total as f64 * 1000.0).round() / 10.0
                } else {
                    0.0
                };
                let st = if is_ffmpeg && enhanced_mkv.is_some() {
                    // ffmpeg running + output MKV growing = assembling
                    "assembling"
                } else if is_enhancing {
                    "upscaling"
                } else if total > 0 || count_out > 0 {
                    "paused"
                } else {
                    "queued"
                };
                (st.to_string(), total, count_out, pct, String::new())
            } else if frames_in_dir.exists() {
                let st = if is_enhancing || is_ffmpeg {
                    "extracting"
                } else if count_in > 0 {
                    "paused"
                } else {
                    "queued"
                };
                (st.to_string(), count_in, 0u64, 0.0, String::new())
            } else {
                // Check for input MKV (downloaded state)
                let has_input = job_dir.join(format!("{title}.mkv")).exists()
                    || fs::read_dir(&job_dir)
                        .ok()
                        .map(|entries| {
                            entries.filter_map(|e| e.ok()).any(|e| {
                                let n = e.file_name().to_string_lossy().to_string();
                                n.ends_with(".mkv") || n.ends_with(".mp4") || n.ends_with(".webm")
                            })
                        })
                        .unwrap_or(false);
                let st = if is_enhancing || is_ffmpeg {
                    "downloading"
                } else if has_input {
                    "downloaded"
                } else {
                    "queued"
                };
                (st.to_string(), 0u64, 0u64, 0.0, String::new())
            };

            seen_titles.insert(title.clone());
            videos.push(VideoStatus {
                id: video_id,
                title,
                display_title,
                scale,
                duration,
                status,
                progress,
                total_frames,
                done_frames,
                eta,
                resolution,
                timing,
            });
        }
    }

    // Add queued videos from ~/json/ directory (primary queue source)
    for meta in read_json_queue() {
        let title = meta.title.clone();
        if title.is_empty() || seen_titles.contains(&title) {
            continue;
        }
        // Also match by video_id in case title differs
        let dominated = videos.iter().any(|v| !meta.video_id.is_empty() && v.id == meta.video_id);
        if dominated {
            continue;
        }
        seen_titles.insert(title.clone());
        let resolution = if meta.width > 0 && meta.height > 0 {
            format!("{}x{}", meta.width, meta.height)
        } else {
            String::new()
        };
        videos.push(VideoStatus {
            id: meta.video_id,
            title,
            display_title: if meta.display_title.is_empty() {
                meta.title.replace('_', " ")
            } else {
                meta.display_title
            },
            scale: meta.scale,
            duration: meta.duration_seconds,
            status: "queued".into(),
            resolution,
            ..Default::default()
        });
    }

    // Legacy fallback: also read video_work_queue.txt
    for qe in read_work_queue() {
        if !seen_titles.contains(&qe.title) {
            seen_titles.insert(qe.title.clone());
            videos.push(VideoStatus {
                id: qe.video_id,
                title: qe.title.clone(),
                display_title: qe.title.replace('_', " "),
                scale: qe.scale,
                status: "queued".into(),
                ..Default::default()
            });
        }
    }

    // Aggregates
    let total = videos.len();
    let done_count = videos.iter().filter(|v| v.status == "done").count();
    let active_statuses = ["downloading", "extracting", "upscaling", "assembling", "uploading"];
    let active_count = videos.iter().filter(|v| active_statuses.contains(&v.status.as_str())).count();

    let est_fps_default = 25.0f64;
    let mut total_frames_all: u64 = 0;
    let mut done_frames_all: u64 = 0;
    for v in &videos {
        if v.total_frames > 0 {
            total_frames_all += v.total_frames;
        } else if v.status != "done" {
            total_frames_all += (v.duration * est_fps_default) as u64;
        }
        done_frames_all += v.done_frames;
    }
    let overall_pct = if total_frames_all > 0 {
        (done_frames_all as f64 / total_frames_all as f64 * 1000.0).round() / 10.0
    } else {
        0.0
    };

    // ETA calculation
    let remaining = total_frames_all.saturating_sub(done_frames_all);
    let eta_str = if fps > 0.0 && remaining > 0 {
        let eta_secs = remaining as f64 / fps;
        let h = (eta_secs / 3600.0) as u64;
        let m = ((eta_secs % 3600.0) / 60.0) as u64;
        if h >= 24 {
            let d = h / 24;
            let hr = h % 24;
            format!("{d}d {hr}h {m}m")
        } else if h > 0 {
            format!("{h}h {m}m")
        } else {
            format!("{m}m")
        }
    } else {
        String::new()
    };

    StatusResponse {
        total,
        done: done_count,
        active: active_count,
        videos,
        log_tail,
        timestamp: Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        total_frames: total_frames_all,
        done_frames: done_frames_all,
        overall_pct,
        fps,
        per_gpu_fps,
        eta: eta_str,
        dl_speed: String::new(),
        dl_progress: String::new(),
        system,
        instance,
        uploads,
    }
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

async fn api_status() -> Json<StatusResponse> {
    Json(build_status().await)
}

async fn dashboard_page() -> Html<String> {
    Html(DASHBOARD_HTML.to_string())
}

async fn compare_page(Path(title): Path<String>) -> Html<String> {
    let job_dir = jobs_dir().join(&title);
    let frames_in_dir = job_dir.join("frames_in");
    let frames_out_dir = job_dir.join("frames_out");

    // Find frames present in both directories
    let out_names: HashSet<String> = fs::read_dir(&frames_out_dir)
        .ok()
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    if n.starts_with("frame_") && n.ends_with(".png") {
                        Some(n)
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let mut available: Vec<String> = fs::read_dir(&frames_in_dir)
        .ok()
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    if n.starts_with("frame_") && n.ends_with(".png") && out_names.contains(&n) {
                        Some(n)
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    available.sort();

    if available.is_empty() {
        return Html(
            "<html><body style='background:#0f172a;color:#e2e8f0;font-family:sans-serif;padding:40px'>\
             <h1>No frames available for comparison yet.</h1>\
             <p><a href='/' style='color:#38bdf8'>Back</a></p></body></html>"
                .to_string(),
        );
    }

    let display_title = title.replace('_', " ");
    // The JavaScript will handle frame navigation client-side
    Html(format!(
        r#"<!DOCTYPE html>
<html><head>
<meta charset="utf-8">
<title>Compare: {display_title}</title>
<style>
  * {{ box-sizing: border-box; margin: 0; padding: 0; }}
  body {{ font-family: -apple-system, sans-serif; background: #0f172a; color: #e2e8f0; padding: 20px; }}
  h1 {{ font-size: 1.3rem; margin-bottom: 4px; }}
  .nav {{ margin: 12px 0; display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }}
  .nav a, .nav span {{ padding: 6px 12px; background: #1e293b; border-radius: 4px; color: #38bdf8;
                       text-decoration: none; font-size: 0.85rem; cursor: pointer; }}
  .nav a:hover {{ background: #334155; }}
  .nav .current {{ background: #334155; color: #f8fafc; }}
  .compare {{ display: flex; gap: 12px; margin-top: 12px; }}
  .compare .panel {{ flex: 1; min-width: 0; }}
  .compare .panel h2 {{ font-size: 0.9rem; color: #94a3b8; margin-bottom: 6px; }}
  .compare img {{ width: 100%; height: auto; border-radius: 4px; border: 1px solid #334155; cursor: pointer; }}
  .compare img:hover {{ border-color: #38bdf8; }}
  a.back {{ color: #38bdf8; text-decoration: none; font-size: 0.85rem; }}
</style>
</head><body>
<a class="back" href="/">&larr; Back to dashboard</a>
<h1>{display_title}</h1>
<div class="nav" id="nav"></div>
<div class="compare">
  <div class="panel">
    <h2 id="in-label">Original</h2>
    <a id="in-link" target="_blank"><img id="in-img" alt="Original"></a>
  </div>
  <div class="panel">
    <h2 id="out-label">Enhanced</h2>
    <a id="out-link" target="_blank"><img id="out-img" alt="Enhanced"></a>
  </div>
</div>
<script>
const title = {title_json};
const frames = {frames_json};
const total = frames.length;
let idx = Math.min(Math.floor(total / 5), total - 1);

function show(i) {{
  idx = Math.max(0, Math.min(i, total - 1));
  const f = frames[idx];
  const inUrl = '/frames/' + title + '/in/' + f;
  const outUrl = '/frames/' + title + '/out/' + f;
  document.getElementById('in-img').src = inUrl;
  document.getElementById('in-link').href = inUrl;
  document.getElementById('out-img').src = outUrl;
  document.getElementById('out-link').href = outUrl;
  document.getElementById('in-label').textContent = 'Original (' + f + ')';
  document.getElementById('out-label').textContent = 'Enhanced (' + f + ')';
  document.getElementById('nav').innerHTML =
    '<a onclick="show(0)">First</a>' +
    '<a onclick="show(' + Math.max(0, idx-10) + ')">&laquo; -10</a>' +
    '<a onclick="show(' + Math.max(0, idx-1) + ')">&lsaquo; Prev</a>' +
    '<span class="current">Frame ' + (idx+1) + ' / ' + total + '</span>' +
    '<a onclick="show(' + Math.min(total-1, idx+1) + ')">Next &rsaquo;</a>' +
    '<a onclick="show(' + Math.min(total-1, idx+10) + ')">+10 &raquo;</a>' +
    '<a onclick="show(' + (total-1) + ')">Last</a>';
}}
show(idx);
document.addEventListener('keydown', function(e) {{
  if (e.key === 'ArrowLeft') show(idx - 1);
  else if (e.key === 'ArrowRight') show(idx + 1);
}});
</script>
</body></html>"#,
        display_title = display_title,
        title_json = serde_json::to_string(&title).unwrap_or_default(),
        frames_json = serde_json::to_string(&available).unwrap_or_default(),
    ))
}

async fn serve_frame(Path((title, dir, filename)): Path<(String, String, String)>) -> Response {
    let subdir = match dir.as_str() {
        "in" => "frames_in",
        "out" => "frames_out",
        _ => {
            return (StatusCode::NOT_FOUND, "Not found").into_response();
        }
    };
    let filepath = jobs_dir().join(&title).join(subdir).join(&filename);
    if !filepath.exists() {
        return (StatusCode::NOT_FOUND, "Frame not found").into_response();
    }

    match tokio::fs::read(&filepath).await {
        Ok(data) => {
            let mime = if filename.ends_with(".png") {
                "image/png"
            } else if filename.ends_with(".jpg") || filename.ends_with(".jpeg") {
                "image/jpeg"
            } else {
                "application/octet-stream"
            };
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, "max-age=3600"),
                ],
                data,
            )
                .into_response()
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Read error").into_response(),
    }
}

async fn download_video(Path(title): Path<String>) -> Response {
    let job_dir = jobs_dir().join(&title);

    // Try various enhanced MKV patterns
    let mut filepath: Option<PathBuf> = None;
    for scale in [2, 4] {
        let p = job_dir.join(format!("{title}_{scale}x.mkv"));
        if p.exists() {
            filepath = Some(p);
            break;
        }
        let p = job_dir.join(format!("enhanced_{scale}x.mkv"));
        if p.exists() {
            filepath = Some(p);
            break;
        }
    }

    let Some(filepath) = filepath else {
        return (StatusCode::NOT_FOUND, "Enhanced video not found").into_response();
    };

    let filename = filepath
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let filesize = fs::metadata(&filepath).map(|m| m.len()).unwrap_or(0);

    match tokio::fs::read(&filepath).await {
        Ok(data) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, header::HeaderValue::from_static("video/x-matroska")),
                (header::CONTENT_LENGTH, header::HeaderValue::from_str(&filesize.to_string()).unwrap()),
                (
                    header::CONTENT_DISPOSITION,
                    header::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
                        .unwrap(),
                ),
            ],
            data,
        )
            .into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Read error").into_response(),
    }
}

// ---------------------------------------------------------------------------
// HTML template (embedded)
// ---------------------------------------------------------------------------

const DASHBOARD_HTML: &str = r##"<!DOCTYPE html>
<html><head>
<meta charset="utf-8">
<title>Da Vaz Video Enhancement</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
         background: #0f172a; color: #e2e8f0; padding: 20px; }
  h1 { font-size: 1.5rem; margin-bottom: 4px; color: #f8fafc; }
  .subtitle { color: #94a3b8; margin-bottom: 20px; font-size: 0.9rem; }
  .summary { display: flex; gap: 16px; margin-bottom: 24px; flex-wrap: wrap; }
  .card { background: #1e293b; border-radius: 8px; padding: 16px 20px; min-width: 140px; }
  .card .num { font-size: 2rem; font-weight: 700; color: #38bdf8; }
  .card .label { color: #94a3b8; font-size: 0.85rem; }
  table { width: 100%; border-collapse: collapse; background: #1e293b; border-radius: 8px; overflow: hidden; }
  th { text-align: left; padding: 10px 12px; background: #334155; color: #94a3b8;
       font-size: 0.8rem; text-transform: uppercase; letter-spacing: 0.05em; }
  td { padding: 8px 12px; border-top: 1px solid #334155; font-size: 0.9rem; }
  tr:hover { background: #1e3a5f; }
  .bar-bg { background: #334155; border-radius: 4px; height: 20px; position: relative; overflow: hidden; min-width: 120px; }
  .bar-fg { height: 100%; border-radius: 4px; transition: width 0.5s; }
  .bar-text { position: absolute; top: 0; left: 8px; line-height: 20px; font-size: 0.75rem; font-weight: 600; }
  .status { display: inline-block; padding: 2px 8px; border-radius: 4px; font-size: 0.75rem; font-weight: 600; }
  .status-done { background: #065f46; color: #6ee7b7; }
  .status-downloading { background: #4c1d95; color: #c4b5fd; }
  .status-extracting { background: #713f12; color: #fde68a; }
  .status-upscaling { background: #1e3a5f; color: #38bdf8; }
  .status-assembling { background: #164e63; color: #67e8f9; }
  .status-uploading { background: #14532d; color: #86efac; }
  .status-extracting { background: #713f12; color: #fbbf24; }
  .status-downloaded { background: #3b0764; color: #c084fc; }
  .status-paused { background: #78350f; color: #fdba74; }
  .status-queued { background: #334155; color: #94a3b8; }
  .log { background: #0f172a; border: 1px solid #334155; border-radius: 8px; padding: 12px;
         font-family: monospace; font-size: 0.75rem; max-height: 300px; overflow-y: auto;
         white-space: pre-wrap; color: #94a3b8; margin-top: 20px; }
  .title-col { max-width: 350px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  a { color: #38bdf8; text-decoration: none; }
  a:hover { text-decoration: underline; }
  .instance-bar { background: #1e293b; border-radius: 8px; padding: 10px 18px; margin-bottom: 16px;
         display: flex; gap: 24px; flex-wrap: wrap; font-size: 0.85rem; }
  .instance-bar .item { display: flex; gap: 6px; }
  .instance-bar .ib-label { color: #94a3b8; }
  .instance-bar .value { color: #e2e8f0; font-weight: 500; }
  .specs { display: grid; grid-template-columns: repeat(auto-fit, minmax(280px, 1fr)); gap: 12px; margin-bottom: 24px; }
  .spec-card { background: #1e293b; border-radius: 8px; padding: 14px 18px; }
  .spec-card h3 { font-size: 0.8rem; text-transform: uppercase; letter-spacing: 0.05em; color: #94a3b8; margin-bottom: 8px; }
  .spec-row { display: flex; justify-content: space-between; font-size: 0.85rem; padding: 2px 0; }
  .spec-row .sr-label { color: #94a3b8; }
  .spec-row .value { color: #e2e8f0; font-weight: 500; }
  .spec-row .value.hot { color: #fb923c; }
  .spec-row .value.ok { color: #6ee7b7; }
  .uploads { margin-top: 24px; }
  .uploads h2 { font-size: 1.1rem; margin-bottom: 10px; color: #94a3b8; }
</style>
</head><body>
<h1 id="page-title">Da Vaz Video Enhancement</h1>
<p class="subtitle" id="page-subtitle">Real-ESRGAN AI Upscaling &mdash; auto-refreshes every 30s</p>
<div id="app">Loading...</div>
<script>
function esc(s) { const d = document.createElement('div'); d.textContent = s; return d.innerHTML; }

async function update() {
  try {
    const r = await fetch('/api/status');
    const d = await r.json();
    const app = document.getElementById('app');
    const activeStatuses = ['downloading', 'extracting', 'upscaling', 'assembling', 'uploading'];
    const active = d.videos.filter(v => activeStatuses.includes(v.status));
    const paused = d.videos.filter(v => v.status === 'paused');
    const done = d.videos.filter(v => v.status === 'done');
    const queued = d.videos.filter(v => v.status === 'queued');
    const other = d.videos.filter(v => !['done','upscaling','paused','queued'].includes(v.status));

    const perGpu = d.per_gpu_fps || {};
    const gpuCount = Object.keys(perGpu).length || (d.system.gpu_count || 1);
    const fpsStr = d.fps > 0 ? d.fps.toFixed(1) + ' fps' + (gpuCount > 1 ? ' (' + gpuCount + ' GPUs)' : '') : '\u2014';
    const etaStr = d.eta || '\u2014';
    const framesStr = d.done_frames + ' / ' + d.total_frames;

    // Instance metadata bar
    let h = '';
    if (d.instance && (d.instance.label || d.instance.location)) {
      const i = d.instance;
      let items = '';
      if (i.label) items += '<div class="item"><span class="ib-label">Instance:</span><span class="value">' + esc(i.label) + '</span></div>';
      if (i.location) items += '<div class="item"><span class="ib-label">Location:</span><span class="value">' + esc(i.location) + '</span></div>';
      if (i.cost_per_hr) items += '<div class="item"><span class="ib-label">Cost:</span><span class="value">$' + i.cost_per_hr + '/hr</span></div>';
      if (i.provider) items += '<div class="item"><span class="ib-label">Provider:</span><span class="value">' + esc(i.provider) + '</span></div>';
      if (i.instance_id) items += '<div class="item"><span class="ib-label">ID:</span><span class="value">' + esc(i.instance_id) + '</span></div>';
      h += '<div class="instance-bar">' + items + '</div>';
    }

    // Cost estimates
    const costPerHr = d.instance && d.instance.cost_per_hr ? parseFloat(d.instance.cost_per_hr) : 0;
    let costRemaining = '\u2014';
    if (costPerHr > 0 && d.fps > 0) {
      const totalRemaining = d.total_frames - d.done_frames;
      const remainingHrs = totalRemaining / d.fps / 3600;
      costRemaining = '$' + (remainingHrs * costPerHr).toFixed(1);
    }

    h += '<div class="summary">' +
      '<div class="card"><div class="num">' + d.total + '</div><div class="label">Total Videos</div></div>' +
      '<div class="card"><div class="num" style="color:#6ee7b7">' + done.length + '</div><div class="label">Completed</div></div>' +
      '<div class="card"><div class="num" style="color:#38bdf8">' + active.length + '</div><div class="label">Upscaling Now</div></div>' +
      '<div class="card"><div class="num" style="color:#94a3b8">' + queued.length + '</div><div class="label">Queued</div></div>' +
      '<div class="card"><div class="num" style="font-size:1.2rem">' + framesStr + '</div><div class="label">Frames (' + d.overall_pct + '%)</div></div>' +
      '<div class="card"><div class="num">' + fpsStr + '</div><div class="label">GPU Speed</div></div>' +
      '<div class="card"><div class="num">' + etaStr + '</div><div class="label">ETA</div></div>' +
      '<div class="card"><div class="num" style="color:#fbbf24">' + costRemaining + '</div><div class="label">Cost Remaining</div></div>' +
    '</div>';

    // System specs
    if (d.system) {
      const s = d.system;
      h += '<div class="specs">';
      const gpus = s.gpus || [];
      if (gpus.length > 1) {
        h += '<div class="spec-card"><h3>GPUs (' + gpus.length + 'x ' + esc(gpus[0].name) + ')</h3>';
        for (let i = 0; i < gpus.length; i++) {
          const g = gpus[i];
          const gfps = perGpu[String(i)] ? perGpu[String(i)].toFixed(1) + ' fps' : '\u2014';
          h += '<div class="spec-row"><span class="sr-label">GPU ' + i + '</span><span class="value">' +
               gfps + ' | ' + g.temp_c + '\u00b0C | ' + g.util_pct + '% | ' +
               (g.vram_used_mb/1024).toFixed(1) + '/' + (g.vram_total_mb/1024).toFixed(0) + 'GB</span></div>';
        }
        h += '<div class="spec-row"><span class="sr-label">Driver</span><span class="value">' + esc(gpus[0].driver) + '</span></div>';
        if (gpus[0].power_draw) h += '<div class="spec-row"><span class="sr-label">Power</span><span class="value">' + esc(gpus[0].power_draw) + ' W</span></div>';
        h += '</div>';
      } else if (gpus.length === 1) {
        const g = gpus[0];
        const vramPct = g.vram_total_mb > 0 ? ((g.vram_used_mb / g.vram_total_mb) * 100).toFixed(0) : 0;
        const tempClass = g.temp_c >= 80 ? 'hot' : 'ok';
        h += '<div class="spec-card"><h3>GPU</h3>' +
          '<div class="spec-row"><span class="sr-label">Model</span><span class="value">' + esc(g.name) + '</span></div>' +
          '<div class="spec-row"><span class="sr-label">VRAM</span><span class="value">' + (g.vram_total_mb/1024).toFixed(1) + ' GB (' + (g.vram_used_mb/1024).toFixed(1) + ' GB used, ' + vramPct + '%)</span></div>' +
          '<div class="spec-row"><span class="sr-label">Temperature</span><span class="value ' + tempClass + '">' + g.temp_c + '\u00b0C</span></div>' +
          '<div class="spec-row"><span class="sr-label">Utilization</span><span class="value">' + g.util_pct + '%</span></div>' +
          '<div class="spec-row"><span class="sr-label">Power</span><span class="value">' + esc(g.power_draw) + ' W</span></div>' +
          '<div class="spec-row"><span class="sr-label">Driver</span><span class="value">' + esc(g.driver) + '</span></div>' +
        '</div>';
      }
      if (s.cpu && s.cpu.name) {
        const c = s.cpu;
        const loadStr = c.load_1m !== undefined ? c.load_1m.toFixed(1)+' / '+c.load_5m.toFixed(1)+' / '+c.load_15m.toFixed(1) : '\u2014';
        h += '<div class="spec-card"><h3>CPU</h3>' +
          '<div class="spec-row"><span class="sr-label">Model</span><span class="value">' + esc(c.name) + '</span></div>' +
          '<div class="spec-row"><span class="sr-label">Cores</span><span class="value">' + c.cores + '</span></div>' +
          (c.mhz ? '<div class="spec-row"><span class="sr-label">MHz</span><span class="value">' + esc(c.mhz) + '</span></div>' : '') +
          '<div class="spec-row"><span class="sr-label">Load (1/5/15m)</span><span class="value">' + loadStr + '</span></div>' +
        '</div>';
      }
      if (s.memory && s.memory.total_mb > 0) {
        const m = s.memory;
        const usedPct = m.total_mb > 0 ? ((m.used_mb / m.total_mb) * 100).toFixed(0) : 0;
        h += '<div class="spec-card"><h3>Memory</h3>' +
          '<div class="spec-row"><span class="sr-label">Total</span><span class="value">' + (m.total_mb/1024).toFixed(1) + ' GB</span></div>' +
          '<div class="spec-row"><span class="sr-label">Used</span><span class="value">' + (m.used_mb/1024).toFixed(1) + ' GB (' + usedPct + '%)</span></div>' +
          '<div class="spec-row"><span class="sr-label">Available</span><span class="value">' + (m.available_mb/1024).toFixed(1) + ' GB</span></div>' +
        '</div>';
      }
      if (s.disk && s.disk.total_gb > 0) {
        const dk = s.disk;
        h += '<div class="spec-card"><h3>Disk</h3>' +
          '<div class="spec-row"><span class="sr-label">Total</span><span class="value">' + dk.total_gb + ' GB</span></div>' +
          '<div class="spec-row"><span class="sr-label">Used</span><span class="value">' + dk.used_gb + ' GB (' + dk.util_pct + '%)</span></div>' +
          '<div class="spec-row"><span class="sr-label">Free</span><span class="value">' + dk.free_gb + ' GB</span></div>' +
        '</div>';
      }
      h += '</div>';
    }

    // Video table
    h += '<table><thead><tr>' +
      '<th>#</th><th>Title</th><th>Resolution</th><th>Duration</th><th>Status</th><th>Progress</th><th>Timing</th><th>Compare</th><th>Input</th><th>Output</th>' +
    '</tr></thead><tbody>';

    const order = [...active, ...paused, ...other, ...done, ...queued];
    order.forEach((v, i) => {
      const dur = v.duration >= 3600
        ? Math.floor(v.duration/3600) + ':' + String(Math.floor((v.duration%3600)/60)).padStart(2,'0') + ':' + String(Math.floor(v.duration%60)).padStart(2,'0')
        : Math.floor(v.duration/60) + ':' + String(Math.floor(v.duration%60)).padStart(2,'0');
      const barColor = v.status === 'done' ? '#6ee7b7' : '#38bdf8';
      const ytUrl = v.id ? 'https://www.youtube.com/watch?v=' + v.id : '#';
      const compareLink = v.done_frames > 0 ? '<a href="/compare/' + encodeURIComponent(v.title) + '">view</a>' : '';
      const inputName = v.title + '.mkv';
      const outputName = v.title + '_' + v.scale + 'x.mkv';
      const outputCell = v.status === 'done'
        ? '<a href="/download/' + encodeURIComponent(v.title) + '" style="color:#6ee7b7">' + esc(outputName) + '</a>'
        : esc(outputName);

      const t = v.timing || {};
      const timingStr = (t.download || t.extraction || t.upscaling || t.reassembly || t.youtube_upload)
        ? (t.download ? 'dl:' + Math.round(t.download/60) + 'm ' : '') +
          (t.extraction ? 'ext:' + Math.round(t.extraction/60) + 'm ' : '') +
          (t.upscaling ? 'up:' + (t.upscaling/3600).toFixed(1) + 'h ' : '') +
          (t.reassembly ? 'asm:' + Math.round(t.reassembly/60) + 'm ' : '') +
          (t.youtube_upload ? 'yt:' + Math.round(t.youtube_upload/60) + 'm' : '')
        : '\u2014';

      h += '<tr>' +
        '<td>' + (i+1) + '</td>' +
        '<td class="title-col"><a href="' + ytUrl + '" target="_blank">' + esc(v.display_title || v.title.replace(/_/g, ' ')) + '</a></td>' +
        '<td style="font-size:0.75rem;color:#94a3b8">' + esc(v.resolution || '\u2014') + '</td>' +
        '<td>' + dur + '</td>' +
        '<td><span class="status status-' + v.status + '">' + v.status + '</span></td>' +
        '<td><div class="bar-bg"><div class="bar-fg" style="width:' + v.progress + '%;background:' + barColor + '"></div>' +
            '<span class="bar-text">' + (v.status==='done' ? esc(v.eta) : v.progress > 0 ? v.done_frames+'/'+v.total_frames : '') + '</span></div></td>' +
        '<td style="font-size:0.7rem;color:#94a3b8">' + timingStr + '</td>' +
        '<td>' + compareLink + '</td>' +
        '<td style="font-size:0.75rem;color:#94a3b8">' + esc(inputName) + '</td>' +
        '<td style="font-size:0.75rem">' + outputCell + '</td>' +
      '</tr>';
    });
    h += '</tbody></table>';

    // Uploads section
    if (d.uploads && d.uploads.length > 0) {
      h += '<div class="uploads"><h2>Completed Uploads</h2><table><thead><tr><th>#</th><th>Title</th><th>YouTube</th><th>Uploaded</th></tr></thead><tbody>';
      d.uploads.forEach((u, i) => {
        h += '<tr><td>' + (i+1) + '</td><td>' + esc(u.title) + '</td>' +
          '<td><a href="' + esc(u.youtube_url) + '" target="_blank">' + esc(u.youtube_url) + '</a></td>' +
          '<td>' + esc(u.uploaded_at) + '</td></tr>';
      });
      h += '</tbody></table></div>';
    }

    // Log tail
    if (d.log_tail) {
      h += '<div class="log">' + esc(d.log_tail) + '</div>';
    }
    h += '<p style="margin-top:12px;color:#64748b;font-size:0.75rem">Updated: ' + d.timestamp + '</p>';
    app.innerHTML = h;

    // Update page title
    const activeVideo = d.videos.find(v => v.status === 'upscaling') || d.videos[0];
    const videoName = activeVideo ? (activeVideo.display_title || activeVideo.title.replace(/_/g, ' ')) : '';
    const location = (d.instance && d.instance.location) ? d.instance.location : '';
    const titleParts = [];
    if (videoName) titleParts.push(videoName);
    if (location) titleParts.push(location);
    document.title = titleParts.join(' \u2014 ') || 'Video Enhancement';
    document.getElementById('page-title').textContent = videoName || 'Video Enhancement';

    let subtitleParts = ['Real-ESRGAN AI Upscaling'];
    if (location) subtitleParts.push(location);
    if (costPerHr > 0 && d.eta) {
      let etaHours = 0;
      const hMatch = d.eta.match(/(\d+)h/);
      const mMatch = d.eta.match(/(\d+)m/);
      const dMatch = d.eta.match(/(\d+)d/);
      if (dMatch) etaHours += parseInt(dMatch[1]) * 24;
      if (hMatch) etaHours += parseInt(hMatch[1]);
      if (mMatch) etaHours += parseInt(mMatch[1]) / 60;
      if (etaHours > 0) {
        subtitleParts.push('$' + costPerHr.toFixed(2) + '/hr');
        subtitleParts.push('~$' + (etaHours * costPerHr).toFixed(1) + ' remaining');
      }
    }
    document.getElementById('page-subtitle').textContent = subtitleParts.join(' \u2014 ');
  } catch(e) {
    document.getElementById('app').innerHTML = '<p>Error loading status: ' + e + '</p>';
  }
}
update();
setInterval(update, 30000);
</script>
</body></html>"##;

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(dashboard_page))
        .route("/api/status", get(api_status))
        .route("/compare/{title}", get(compare_page))
        .route("/frames/{title}/{dir}/{filename}", get(serve_frame))
        .route("/download/{title}", get(download_video));

    let addr = "0.0.0.0:8080";
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => {
            eprintln!("Status server listening on {}", addr);
            l
        }
        Err(e) => {
            eprintln!("ERROR: Failed to bind to {}: {}", addr, e);
            std::process::exit(1);
        }
    };
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("ERROR: Server crashed: {}", e);
        std::process::exit(1);
    }
}
