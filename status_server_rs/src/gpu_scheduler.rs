//! gpu_scheduler — Rust replacement for multi_gpu_queue.sh
//!
//! Single binary that manages the entire GPU pipeline:
//! 1. Scans queue (~/json/*.json)
//! 2. Counts GPUs
//! 3. Distributes videos proportionally (2 videos + 4 GPUs = 2 GPUs per video)
//! 4. Starts upscale.py with --start/--end segments
//! 5. Monitors GPU idle → auto-rebalance
//! 6. After upscaling → reassemble → upload → auto-destroy
//!
//! Usage: gpu_scheduler [NUM_GPUS]

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------
struct SchedulerConfig {
    home: PathBuf,
    json_dir: PathBuf,
    done_dir: PathBuf,
    jobs_dir: PathBuf,
    num_gpus: u32,
}

impl SchedulerConfig {
    fn new(num_gpus: u32) -> Self {
        let home = PathBuf::from(env::var("HOME").unwrap_or_else(|_| "/root".to_string()));
        Self {
            json_dir: home.join("json"),
            done_dir: home.join("json_done"),
            jobs_dir: home.join("jobs"),
            home,
            num_gpus,
        }
    }
}

// ---------------------------------------------------------------------------
// Video info
// ---------------------------------------------------------------------------
#[derive(Clone, Debug)]
struct VideoJob {
    video_id: String,
    scale: u32,
    title: String,
    url: String,
    json_path: PathBuf,
}

fn read_video_job(path: &Path) -> Option<VideoJob> {
    let content = fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    let video_id = v.get("video_id")?.as_str()?.to_string();
    let scale = v.get("scale").and_then(|s| s.as_u64()).unwrap_or(4) as u32;
    let title = v.get("title").and_then(|s| s.as_str()).unwrap_or(&video_id).to_string();
    let url = format!("https://www.youtube.com/watch?v={}", video_id);
    Some(VideoJob { video_id, scale, title, url, json_path: path.to_path_buf() })
}

// ---------------------------------------------------------------------------
// Frame counting (fast, Rust)
// ---------------------------------------------------------------------------
fn count_frames(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name();
            let s = n.to_string_lossy();
            s.starts_with("frame_") && s.ends_with(".png")
        })
        .count() as u64
}

fn count_todo_frames(fi_dir: &Path, fo_dir: &Path) -> (u64, u64, u64) {
    let fi: HashSet<String> = fs::read_dir(fi_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with("frame_") && n.ends_with(".png") { Some(n) } else { None }
        })
        .collect();
    let fo: HashSet<String> = fs::read_dir(fo_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with("frame_") && n.ends_with(".png") { Some(n) } else { None }
        })
        .collect();
    let total = std::cmp::max(fi.len(), fo.len()) as u64;
    let done = fo.len() as u64;
    let remaining = fi.difference(&fo).count() as u64;
    (total, done, remaining)
}

// ---------------------------------------------------------------------------
// GPU utilization
// ---------------------------------------------------------------------------
fn get_gpu_utils() -> Vec<u32> {
    let output = Command::new("nvidia-smi")
        .args(["--query-gpu=utilization.gpu", "--format=csv,noheader,nounits"])
        .output()
        .ok();
    match output {
        Some(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim().parse::<u32>().ok())
                .collect()
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Pipeline phases for a single video
// ---------------------------------------------------------------------------
fn download_video(cfg: &SchedulerConfig, job: &VideoJob) -> bool {
    let work_dir = cfg.jobs_dir.join(&job.video_id);
    let input = work_dir.join(format!("{}.mkv", job.video_id));
    if input.exists() {
        eprintln!("[{}] Already downloaded", job.video_id);
        return true;
    }

    let _ = fs::create_dir_all(&work_dir);
    eprintln!("[{}] Downloading...", job.video_id);

    let output_template = format!("{}/{}.%(ext)s", work_dir.display(), job.video_id);
    let mut args = vec![
        "--remote-components".to_string(), "ejs:github".to_string(),
        "-o".to_string(), output_template,
        "--merge-output-format".to_string(), "mkv".to_string(),
    ];
    let cookies = cfg.home.join("cookies.txt");
    if cookies.exists() {
        args.push("--cookies".to_string());
        args.push(cookies.to_string_lossy().to_string());
    }
    args.push(job.url.clone());

    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let status = Command::new("yt-dlp")
        .args(&args_ref)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();

    if !input.exists() {
        // Find what yt-dlp produced
        if let Ok(entries) = fs::read_dir(&work_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().to_string();
                if (name.ends_with(".mkv") || name.ends_with(".mp4"))
                    && !name.contains("_2x") && !name.contains("_4x")
                {
                    let _ = fs::rename(entry.path(), &input);
                    break;
                }
            }
        }
    }
    input.exists()
}

fn extract_frames(cfg: &SchedulerConfig, job: &VideoJob) -> bool {
    let fi_dir = cfg.jobs_dir.join(&job.video_id).join("frames_in");
    let _ = fs::create_dir_all(&fi_dir);

    if count_frames(&fi_dir) > 0 {
        eprintln!("[{}] Frames already extracted: {}", job.video_id, count_frames(&fi_dir));
        return true;
    }

    let input = cfg.jobs_dir.join(&job.video_id).join(format!("{}.mkv", job.video_id));
    let input_str = input.to_string_lossy().to_string();
    let pattern = format!("{}/frame_%08d.png", fi_dir.display());

    eprintln!("[{}] Extracting frames...", job.video_id);
    let status = Command::new("ffmpeg")
        .args(&["-i", &input_str, "-qscale:v", "2", &pattern, "-loglevel", "warning", "-stats"])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();

    count_frames(&fi_dir) > 0
}

fn run_upscale_segments(cfg: &SchedulerConfig, job: &VideoJob, gpus: &[u32]) -> bool {
    let fi_dir = cfg.jobs_dir.join(&job.video_id).join("frames_in");
    let fo_dir = cfg.jobs_dir.join(&job.video_id).join("frames_out");
    let _ = fs::create_dir_all(&fo_dir);

    let count_in = count_frames(&fi_dir);
    let count_out = count_frames(&fo_dir);

    // Read expected total — check multiple sources (queue JSON has duration×fps)
    let expected_total = {
        let mut total = 0u64;
        // 1. job_meta.json (written by enhance after ffprobe)
        let meta_path = cfg.jobs_dir.join(&job.video_id).join("job_meta.json");
        if let Some(v) = fs::read_to_string(&meta_path).ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) {
            total = v.get("total_frames").and_then(|t| t.as_u64()).unwrap_or(0);
            if total == 0 {
                let dur = v.get("duration_seconds").and_then(|d| d.as_f64()).unwrap_or(0.0);
                let fps = v.get("fps").and_then(|f| f.as_f64()).unwrap_or(25.0);
                if dur > 0.0 { total = (dur * fps) as u64; }
            }
        }
        // 2. Queue JSON (from fetch_video_json.sh — has duration + fps)
        // Also check .processing and .processing.N variants
        if total == 0 {
            for dir in &[&cfg.json_dir, &cfg.done_dir] {
                if let Ok(entries) = fs::read_dir(dir) {
                    for entry in entries.filter_map(|e| e.ok()) {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if name.starts_with(&job.video_id) && (name.ends_with(".json") || name.contains(".json.processing")) {
                            if let Some(v) = fs::read_to_string(entry.path()).ok()
                                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) {
                                let dur = v.get("duration_seconds").and_then(|d| d.as_f64()).unwrap_or(0.0);
                                let fps = v.get("fps").and_then(|f| f.as_f64()).unwrap_or(25.0);
                                if dur > 0.0 { total = (dur * fps) as u64; break; }
                            }
                        }
                    }
                }
                if total > 0 { break; }
            }
        }
        total
    };

    // Use expected_total if available, else max(count_in, count_out)
    let total = if expected_total > 0 {
        expected_total
    } else {
        std::cmp::max(count_in, count_out)
    };

    if count_in == 0 {
        if expected_total > 0 && count_out < expected_total {
            eprintln!("[{}] WARNING: frames_in deleted but only {}/{} output — need re-extract!",
                job.video_id, count_out, expected_total);
            return false;
        }
        eprintln!("[{}] No frames_in, {} frames_out — assuming done", job.video_id, count_out);
        return true;
    }

    // Always start upscale.py — let IT decide if frames are done
    // Don't use count_out vs expected_total here (unreliable with deleted frames)
    eprintln!("[{}] Upscaling: in={} out={} expected={} on {} GPUs",
        job.video_id, count_in, count_out, total, gpus.len());

    let total_in = count_in;
    let per_gpu = ((total_in as u32) + gpus.len() as u32 - 1) / gpus.len() as u32;

    let upscale_py = format!("{}/upscale.py", cfg.home.display());
    let fi_str = fi_dir.to_string_lossy().to_string();
    let fo_str = fo_dir.to_string_lossy().to_string();
    let scale_str = job.scale.to_string();

    let mut children = Vec::new();
    for (i, &gpu) in gpus.iter().enumerate() {
        let s = i as u32 * per_gpu;
        let e = std::cmp::min((i as u32 + 1) * per_gpu, total_in as u32);
        if s >= total_in as u32 { continue; }

        let gpu_log = format!("{}/gpu{}.log", cfg.home.display(), gpu);
        let log_file = fs::OpenOptions::new().create(true).append(true).open(&gpu_log).ok();

        let s_str = s.to_string();
        let e_str = e.to_string();

        let child = Command::new("python3")
            .args(&[&upscale_py as &str, &fi_str, &fo_str, &scale_str, "--start", &s_str, "--end", &e_str])
            .env("CUDA_VISIBLE_DEVICES", gpu.to_string())
            .stdout(log_file.as_ref().map(|f| Stdio::from(f.try_clone().unwrap())).unwrap_or(Stdio::inherit()))
            .stderr(Stdio::inherit())
            .spawn();

        if let Ok(c) = child {
            eprintln!("[{}] GPU {}: frames {}-{}", job.video_id, gpu, s, e);
            children.push((gpu, c));
        }
    }

    // Wait for all
    let mut failed = 0;
    for (gpu, mut child) in children {
        if let Ok(status) = child.wait() {
            if !status.success() {
                eprintln!("[{}] GPU {} segment failed", job.video_id, gpu);
                failed += 1;
            }
        }
    }

    let (_, _, still_remaining) = count_todo_frames(&fi_dir, &fo_dir);
    eprintln!("[{}] Upscaling done, {} still remaining", job.video_id, still_remaining);
    still_remaining == 0
}

fn run_frame_gap_check(cfg: &SchedulerConfig, job: &VideoJob) -> bool {
    let fi_dir = cfg.jobs_dir.join(&job.video_id).join("frames_in");
    let fo_dir = cfg.jobs_dir.join(&job.video_id).join("frames_out");
    let fi_str = fi_dir.to_string_lossy().to_string();
    let fo_str = fo_dir.to_string_lossy().to_string();

    let bin = if Path::new("/root/frame_gap_check").exists() {
        "/root/frame_gap_check"
    } else if Path::new("/usr/local/bin/frame_gap_check").exists() {
        "/usr/local/bin/frame_gap_check"
    } else {
        eprintln!("[{}] frame_gap_check not found, skipping", job.video_id);
        return true;
    };

    let output = Command::new(bin).args(&[&fi_str, &fo_str]).output();
    if let Ok(o) = output {
        let stdout = String::from_utf8_lossy(&o.stdout);
        if stdout.starts_with("OK") {
            eprintln!("[{}] No frame gaps", job.video_id);
        } else {
            eprintln!("[{}] Frame gaps found, re-upscaling...", job.video_id);
            let upscale_py = format!("{}/upscale.py", cfg.home.display());
            let scale_str = job.scale.to_string();
            let _ = Command::new("python3")
                .args(&[&upscale_py as &str, &fi_str, &fo_str, &scale_str])
                .stdout(Stdio::inherit()).stderr(Stdio::inherit())
                .status();
        }
    }
    true
}

fn reassemble_video(cfg: &SchedulerConfig, job: &VideoJob) -> bool {
    let work_dir = cfg.jobs_dir.join(&job.video_id);
    let output = work_dir.join(format!("{}_{}{}.mkv", job.video_id, job.scale, "x"));
    let input = work_dir.join(format!("{}.mkv", job.video_id));

    if output.exists() {
        eprintln!("[{}] Output already exists", job.video_id);
        return true;
    }

    // Brightness matching (Python — needs OpenCV)
    let fo_str = work_dir.join("frames_out").to_string_lossy().to_string();
    let input_str = input.to_string_lossy().to_string();
    let gamma = Command::new("python3")
        .args(&["-c", &format!(
            "import cv2,numpy as np,subprocess,glob;fo=sorted(glob.glob('{}/frame_*.png'));enh=[cv2.imread(fo[int(i*len(fo)/10)]).mean() for i in range(10) if cv2.imread(fo[int(i*len(fo)/10)]) is not None];dur=float(subprocess.run(['ffprobe','-v','quiet','-show_entries','format=duration','-of','csv=p=0','{}'],capture_output=True,text=True,timeout=10).stdout.strip() or '0');orig=[np.frombuffer(subprocess.run(['ffmpeg','-ss',str(dur*i/10),'-i','{}','-frames:v','1','-f','rawvideo','-pix_fmt','bgr24','-'],capture_output=True,timeout=10).stdout,dtype=np.uint8).mean() for i in range(10) if len(subprocess.run(['ffmpeg','-ss',str(dur*i/10),'-i','{}','-frames:v','1','-f','rawvideo','-pix_fmt','bgr24','-'],capture_output=True,timeout=10).stdout)>0];print(f'{{max(0.7,min(1.3,np.mean(orig)/np.mean(enh))):.3f}}' if orig and enh else '1.0')",
            fo_str, input_str, input_str, input_str
        )])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<f64>().ok())
        .unwrap_or(1.0);

    if (gamma - 1.0).abs() > 0.003 {
        eprintln!("[{}] Brightness gamma={:.3}", job.video_id, gamma);
    }

    // Get FPS
    let fps_str = Command::new("ffprobe")
        .args(&["-v", "quiet", "-select_streams", "v:0", "-show_entries", "stream=r_frame_rate", "-of", "csv=p=0", &input_str])
        .output().ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "25/1".to_string());
    let fps_int: u32 = if let Some(pos) = fps_str.find('/') {
        let n: f64 = fps_str[..pos].parse().unwrap_or(25.0);
        let d: f64 = fps_str[pos+1..].parse().unwrap_or(1.0);
        (n / d).round() as u32
    } else { fps_str.parse().unwrap_or(25) };

    // Has audio?
    let has_audio = Command::new("ffprobe")
        .args(&["-v", "quiet", "-select_streams", "a", "-show_entries", "stream=codec_type", "-of", "csv=p=0", &input_str])
        .output().ok()
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false);

    eprintln!("[{}] Reassembling at {}fps...", job.video_id, fps_int);
    let frame_pattern = format!("{}/frame_%08d.png", work_dir.join("frames_out").display());
    let output_str = output.to_string_lossy().to_string();

    let mut args: Vec<String> = vec![
        "-framerate".into(), fps_int.to_string(),
        "-i".into(), frame_pattern,
    ];
    if has_audio {
        args.extend(["-i".into(), input_str.clone(), "-map".into(), "0:v".into(), "-map".into(), "1:a".into()]);
    }
    if (gamma - 1.0).abs() > 0.003 {
        args.extend(["-vf".into(), format!("eq=gamma={:.3}", gamma)]);
    }
    args.extend([
        "-c:v".into(), "libx264".into(), "-crf".into(), "18".into(),
        "-preset".into(), "medium".into(), "-pix_fmt".into(), "yuv420p".into(),
    ]);
    if has_audio { args.extend(["-c:a".into(), "copy".into()]); }
    args.extend(["-y".into(), output_str, "-loglevel".into(), "warning".into(), "-stats".into()]);

    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let _ = Command::new("ffmpeg").args(&args_ref).stdout(Stdio::inherit()).stderr(Stdio::inherit()).status();
    output.exists()
}

fn upload_video(cfg: &SchedulerConfig, job: &VideoJob) -> bool {
    let output = cfg.jobs_dir.join(&job.video_id).join(format!("{}_{}{}.mkv", job.video_id, job.scale, "x"));
    if !output.exists() {
        eprintln!("[{}] No output MKV, skipping upload", job.video_id);
        return false;
    }

    eprintln!("[{}] Uploading to YouTube...", job.video_id);
    let vid_arg = format!("--video-id={}", job.video_id);
    let output_str = output.to_string_lossy().to_string();
    let client_secret = format!("{}/client_secret.json", cfg.home.display());
    let token = format!("{}/youtube_token.json", cfg.home.display());

    // Try Rust binary
    let upload_bin = cfg.home.join("youtube_upload");
    if upload_bin.exists() {
        let s = Command::new(&upload_bin)
            .args(&[&vid_arg, &output_str, "--client-secret", &client_secret, "--token", &token])
            .stdout(Stdio::inherit()).stderr(Stdio::inherit())
            .status();
        if s.map(|s| s.success()).unwrap_or(false) { return true; }
        eprintln!("[{}] Rust upload failed, trying Python...", job.video_id);
    }

    // Python fallback
    let py = cfg.home.join("youtube_upload.py");
    if py.exists() {
        let py_str = py.to_string_lossy().to_string();
        let s = Command::new("python3")
            .args(&[&py_str, &vid_arg, &output_str])
            .stdout(Stdio::inherit()).stderr(Stdio::inherit())
            .status();
        return s.map(|s| s.success()).unwrap_or(false);
    }

    eprintln!("[{}] No upload binary found", job.video_id);
    false
}

fn auto_destroy(cfg: &SchedulerConfig) {
    let inst_id = fs::read_to_string(cfg.home.join("instance_meta.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("instance_id").and_then(|s| s.as_str().map(|s| s.to_string())));
    let api_key = fs::read_to_string(cfg.home.join(".vast_api_key")).ok().map(|s| s.trim().to_string());

    if let (Some(id), Some(key)) = (inst_id, api_key) {
        eprintln!("[scheduler] Auto-destroying instance {}...", id);
        let _ = Command::new("curl")
            .args(&["-s", "-X", "PUT",
                &format!("https://console.vast.ai/api/v0/instances/{}/", id),
                "-H", &format!("Authorization: Bearer {}", key),
                "-d", r#"{"state": "stopped"}"#])
            .output();
    }
}

// ---------------------------------------------------------------------------
// Main scheduler loop
// ---------------------------------------------------------------------------
fn main() {
    let args: Vec<String> = env::args().collect();
    let num_gpus = if args.len() > 1 {
        args[1].parse().unwrap_or(0)
    } else {
        0
    };
    let num_gpus = if num_gpus > 0 { num_gpus } else {
        get_gpu_utils().len() as u32
    };

    let cfg = SchedulerConfig::new(num_gpus);
    let _ = fs::create_dir_all(&cfg.json_dir);
    let _ = fs::create_dir_all(&cfg.done_dir);

    eprintln!("=== GPU Scheduler started: {} GPUs ===", num_gpus);

    loop {
        // Restore any .processing files back to .json first
        // (leftover from previous interrupted runs)
        if let Ok(entries) = fs::read_dir(&cfg.json_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.contains(".processing") {
                    // vid.json.processing or vid.json.processing.N → vid.json
                    let base = name.split(".processing").next().unwrap_or(&name);
                    let new_path = cfg.json_dir.join(base);
                    // Skip if already uploaded
                    let vid = base.strip_suffix(".json").unwrap_or(base);
                    if cfg.done_dir.join(format!("{}.json", vid)).exists() {
                        let _ = fs::remove_file(entry.path());
                        continue;
                    }
                    let _ = fs::rename(entry.path(), &new_path);
                }
            }
        }

        // Scan queue
        let mut jobs: Vec<VideoJob> = Vec::new();
        if let Ok(entries) = fs::read_dir(&cfg.json_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".json") {
                    if let Some(job) = read_video_job(&entry.path()) {
                        jobs.push(job);
                    }
                }
            }
        }

        if jobs.is_empty() {
            // Check if any upscale.py still running
            let upscale_count = Command::new("pgrep").args(&["-f", "upscale.py"])
                .output().ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
                .unwrap_or(0);

            if upscale_count == 0 {
                eprintln!("[scheduler] Queue empty, no upscaling. Auto-destroy in 10 min.");
                thread::sleep(Duration::from_secs(600));

                // Re-check
                let still_empty = fs::read_dir(&cfg.json_dir)
                    .map(|e| e.filter_map(|e| e.ok())
                        .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
                        .count() == 0)
                    .unwrap_or(true);

                if still_empty {
                    auto_destroy(&cfg);
                    return;
                }
            }
            thread::sleep(Duration::from_secs(30));
            continue;
        }

        // Sort by estimated remaining (largest first for better distribution)
        jobs.sort_by(|a, b| {
            let a_rem = count_todo_frames(
                &cfg.jobs_dir.join(&a.video_id).join("frames_in"),
                &cfg.jobs_dir.join(&a.video_id).join("frames_out"),
            ).2;
            let b_rem = count_todo_frames(
                &cfg.jobs_dir.join(&b.video_id).join("frames_in"),
                &cfg.jobs_dir.join(&b.video_id).join("frames_out"),
            ).2;
            b_rem.cmp(&a_rem)
        });

        eprintln!("[scheduler] {} videos, {} GPUs", jobs.len(), num_gpus);

        // Phase 1: Download + extract all videos in background (CPU-only, parallel to GPU work)
        // Start a background thread that downloads+extracts ALL videos
        // The upscaling phase will wait for frames_in to appear before starting
        let prep_jobs = jobs.clone();
        let prep_json_dir = cfg.json_dir.clone();
        let prep_jobs_dir = cfg.jobs_dir.clone();
        let prep_home = cfg.home.clone();
        let prep_handle = thread::spawn(move || {
            for job in &prep_jobs {
                let proc_path = prep_json_dir.join(format!("{}.json.processing", job.video_id));
                let _ = fs::rename(&job.json_path, &proc_path);

                let local_cfg = SchedulerConfig {
                    home: prep_home.clone(),
                    json_dir: prep_json_dir.clone(),
                    done_dir: PathBuf::new(),
                    jobs_dir: prep_jobs_dir.clone(),
                    num_gpus: 0,
                };

                if !download_video(&local_cfg, job) {
                    eprintln!("[{}] Download failed, skipping", job.video_id);
                    let _ = fs::rename(&proc_path, &job.json_path);
                    continue;
                }
                if !extract_frames(&local_cfg, job) {
                    eprintln!("[{}] Extraction failed, skipping", job.video_id);
                    let _ = fs::rename(&proc_path, &job.json_path);
                    continue;
                }
                eprintln!("[{}] Pre-download+extract complete", job.video_id);
            }
        });

        // Wait for at least the first video to be ready (has frames_in)
        let first_vid = &jobs[0].video_id;
        let first_fi = cfg.jobs_dir.join(first_vid).join("frames_in");
        while count_frames(&first_fi) == 0 {
            thread::sleep(Duration::from_secs(5));
        }
        eprintln!("[{}] Frames ready, starting upscaling", first_vid);

        // Phase 2: Distribute GPUs proportionally
        let all_gpus: Vec<u32> = (0..num_gpus).collect();
        let gpus_per_video = std::cmp::max(1, num_gpus / jobs.len() as u32);
        let mut gpu_assignments: Vec<Vec<u32>> = Vec::new();
        let mut gpu_idx = 0u32;
        for (i, _) in jobs.iter().enumerate() {
            let mut assigned = Vec::new();
            let count = if i == jobs.len() - 1 {
                num_gpus - gpu_idx // Last video gets remaining GPUs
            } else {
                gpus_per_video
            };
            for _ in 0..count {
                if gpu_idx < num_gpus {
                    assigned.push(gpu_idx);
                    gpu_idx += 1;
                }
            }
            eprintln!("[{}] Assigned GPUs: {:?}", jobs[i].video_id, assigned);
            gpu_assignments.push(assigned);
        }

        // Phase 3: Upscale all videos in parallel with dynamic GPU reallocation
        // When a video finishes, its GPUs are redistributed to remaining videos
        use std::sync::mpsc;
        let (tx, rx) = mpsc::channel::<(usize, Vec<u32>)>(); // (job_index, freed_gpus)

        let mut active_jobs: Vec<usize> = (0..jobs.len()).collect();
        let mut current_assignments = gpu_assignments.clone();

        // Start initial upscaling
        let mut handles: Vec<(usize, thread::JoinHandle<bool>)> = Vec::new();
        for (i, job) in jobs.iter().enumerate() {
            let gpus = current_assignments[i].clone();
            if gpus.is_empty() { continue; }
            let cfg_home = cfg.home.clone();
            let cfg_jobs = cfg.jobs_dir.clone();
            let job_clone = job.clone();
            let tx_clone = tx.clone();
            let idx = i;

            let handle = thread::spawn(move || {
                let local_cfg = SchedulerConfig {
                    home: cfg_home,
                    json_dir: PathBuf::new(),
                    done_dir: PathBuf::new(),
                    jobs_dir: cfg_jobs,
                    num_gpus: gpus.len() as u32,
                };
                let ok = run_upscale_segments(&local_cfg, &job_clone, &gpus);
                let _ = tx_clone.send((idx, gpus));
                ok
            });
            handles.push((i, handle));
        }
        drop(tx); // Drop sender so rx iterator ends when all threads done

        // Monitor: when a video finishes, redistribute its GPUs
        let mut results = vec![false; jobs.len()];
        let mut remaining_handles: Vec<(usize, thread::JoinHandle<bool>)> = Vec::new();

        for (finished_idx, freed_gpus) in rx {
            eprintln!("[scheduler] Video {} finished, freeing GPUs {:?}", jobs[finished_idx].video_id, freed_gpus);

            // Collect the handle result
            let pos = handles.iter().position(|(i, _)| *i == finished_idx);
            if let Some(p) = pos {
                let (_, handle) = handles.remove(p);
                results[finished_idx] = handle.join().unwrap_or(false);
            }

            // Find still-running videos
            let still_running: Vec<usize> = handles.iter().map(|(i, _)| *i).collect();

            if still_running.is_empty() || freed_gpus.is_empty() {
                continue;
            }

            // Redistribute freed GPUs to remaining videos
            // Kill current upscale.py for remaining videos, restart with more GPUs
            eprintln!("[scheduler] Redistributing {} freed GPUs to {} remaining videos",
                freed_gpus.len(), still_running.len());

            // Kill all running upscale.py
            let _ = Command::new("pkill").args(&["-9", "-f", "upscale.py"]).status();
            thread::sleep(Duration::from_secs(2));

            // Collect all available GPUs
            let mut all_free_gpus = freed_gpus.clone();
            for &idx in &still_running {
                all_free_gpus.extend(&current_assignments[idx]);
            }
            all_free_gpus.sort();
            all_free_gpus.dedup();

            // Redistribute proportionally
            let per_video = std::cmp::max(1, all_free_gpus.len() / still_running.len());
            let mut gpu_pos = 0;

            // Wait for old handles (they were killed)
            let old_handles: Vec<_> = handles.drain(..).collect();
            for (_, h) in old_handles {
                let _ = h.join();
            }

            // Restart with new GPU assignments
            let (tx2, rx2) = mpsc::channel::<(usize, Vec<u32>)>();
            for (vi, &job_idx) in still_running.iter().enumerate() {
                let count = if vi == still_running.len() - 1 {
                    all_free_gpus.len() - gpu_pos
                } else {
                    per_video
                };
                let assigned: Vec<u32> = all_free_gpus[gpu_pos..gpu_pos + count].to_vec();
                gpu_pos += count;

                eprintln!("[{}] Reassigned GPUs: {:?}", jobs[job_idx].video_id, assigned);
                current_assignments[job_idx] = assigned.clone();

                let cfg_home = cfg.home.clone();
                let cfg_jobs = cfg.jobs_dir.clone();
                let job_clone = jobs[job_idx].clone();
                let tx_clone = tx2.clone();
                let idx = job_idx;

                let handle = thread::spawn(move || {
                    let local_cfg = SchedulerConfig {
                        home: cfg_home,
                        json_dir: PathBuf::new(),
                        done_dir: PathBuf::new(),
                        jobs_dir: cfg_jobs,
                        num_gpus: assigned.len() as u32,
                    };
                    let ok = run_upscale_segments(&local_cfg, &job_clone, &assigned);
                    let _ = tx_clone.send((idx, assigned));
                    ok
                });
                handles.push((job_idx, handle));
            }
            drop(tx2);

            // Continue monitoring with new receiver
            for (idx2, gpus2) in rx2 {
                eprintln!("[scheduler] Video {} finished (redistributed)", jobs[idx2].video_id);
                let pos = handles.iter().position(|(i, _)| *i == idx2);
                if let Some(p) = pos {
                    let (_, handle) = handles.remove(p);
                    results[idx2] = handle.join().unwrap_or(false);
                }
            }
            break; // All done after redistribution
        }

        // Collect any remaining handles
        for (i, handle) in handles {
            results[i] = handle.join().unwrap_or(false);
        }

        // Wait for background download+extract to finish
        let _ = prep_handle.join();

        // Phase 4: Gap check + reassemble + upload (sequential per video)
        for (i, job) in jobs.iter().enumerate() {
            let proc_path = cfg.json_dir.join(format!("{}.json.processing", job.video_id));

            run_frame_gap_check(&cfg, job);

            if !reassemble_video(&cfg, job) {
                eprintln!("[{}] Reassembly failed", job.video_id);
                let _ = fs::rename(&proc_path, &job.json_path);
                continue;
            }

            if upload_video(&cfg, job) {
                eprintln!("[{}] SUCCESS — uploaded!", job.video_id);
                let _ = fs::rename(&proc_path, cfg.done_dir.join(format!("{}.json", job.video_id)));
                // Cleanup work dir
                let work_dir = cfg.jobs_dir.join(&job.video_id);
                let _ = fs::remove_dir_all(&work_dir);
            } else {
                eprintln!("[{}] Upload failed, keeping for retry", job.video_id);
                let _ = fs::rename(&proc_path, &job.json_path);
            }
        }

        // Loop back to check for new videos
    }
}
