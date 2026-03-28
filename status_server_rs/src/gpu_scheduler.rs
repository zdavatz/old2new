//! gpu_scheduler — Simple, robust GPU scheduler
//!
//! 1. Scan queue for videos with frames ready
//! 2. Distribute GPUs proportionally (2 videos + 4 GPUs = 2 per video)
//! 3. Run upscale.py segments, WAIT for completion
//! 4. Verify frame count before reassembly (CRITICAL: prevents corrupt uploads)
//! 5. Reassemble + upload only if verified
//! 6. Loop back to 1
//!
//! Usage: gpu_scheduler [NUM_GPUS]

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

struct Cfg {
    home: PathBuf,
    json_dir: PathBuf,
    done_dir: PathBuf,
    jobs_dir: PathBuf,
    num_gpus: u32,
}

#[derive(Clone)]
struct Video {
    id: String,
    scale: u32,
    title: String,
    json_path: PathBuf,
    expected_frames: u64,
}

fn count_frames(dir: &Path) -> u64 {
    fs::read_dir(dir).into_iter().flatten()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name(); let s = n.to_string_lossy();
            s.starts_with("frame_") && s.ends_with(".png") && !s.contains(".tmp")
        })
        .count() as u64
}

/// Find highest frame number in dir (frame_00067516.png → 67516)
fn max_frame_number(dir: &Path) -> u64 {
    fs::read_dir(dir).into_iter().flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with("frame_") && n.ends_with(".png") && !n.contains(".tmp") {
                n.strip_prefix("frame_")?.strip_suffix(".png")?.parse::<u64>().ok()
            } else { None }
        })
        .max().unwrap_or(0)
}

fn read_video(path: &Path) -> Option<Video> {
    let content = fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    let id = v.get("video_id")?.as_str()?.to_string();
    let scale = v.get("scale").and_then(|s| s.as_u64()).unwrap_or(4) as u32;
    let title = v.get("title").and_then(|s| s.as_str()).unwrap_or(&id).to_string();
    let dur = v.get("duration_seconds").and_then(|d| d.as_f64()).unwrap_or(0.0);
    let fps = v.get("fps").and_then(|f| f.as_f64()).unwrap_or(25.0);
    let expected = if dur > 0.0 { (dur * fps) as u64 } else { 0 };
    Some(Video { id, scale, title, json_path: path.to_path_buf(), expected_frames: expected })
}

fn download_and_extract(cfg: &Cfg, vid: &Video) -> bool {
    let work_dir = cfg.jobs_dir.join(&vid.id);
    let input = work_dir.join(format!("{}.mkv", vid.id));
    let fi_dir = work_dir.join("frames_in");
    let _ = fs::create_dir_all(&fi_dir);
    let _ = fs::create_dir_all(work_dir.join("frames_out"));

    // Skip if frames exist
    if count_frames(&fi_dir) > 0 {
        eprintln!("[{}] Frames already extracted: {}", vid.id, count_frames(&fi_dir));
        return true;
    }

    // Download
    if !input.exists() {
        eprintln!("[{}] Downloading...", vid.id);
        let template = format!("{}/{}.%(ext)s", work_dir.display(), vid.id);
        let mut args = vec!["--remote-components", "ejs:github", "-o", &template, "--merge-output-format", "mkv"];
        let cookies = cfg.home.join("cookies.txt");
        let cookies_str = cookies.to_string_lossy().to_string();
        if cookies.exists() { args.push("--cookies"); args.push(&cookies_str); }
        let url = format!("https://www.youtube.com/watch?v={}", vid.id);
        args.push(&url);
        let _ = Command::new("yt-dlp").args(&args).stdout(Stdio::inherit()).stderr(Stdio::inherit()).status();
        if !input.exists() {
            // Find what yt-dlp produced
            if let Ok(entries) = fs::read_dir(&work_dir) {
                for e in entries.filter_map(|e| e.ok()) {
                    let n = e.file_name().to_string_lossy().to_string();
                    if (n.ends_with(".mkv") || n.ends_with(".mp4")) && !n.contains("_2x") && !n.contains("_4x") {
                        let _ = fs::rename(e.path(), &input);
                        break;
                    }
                }
            }
        }
        if !input.exists() { eprintln!("[{}] Download failed!", vid.id); return false; }
    }

    // Extract
    eprintln!("[{}] Extracting...", vid.id);
    let input_str = input.to_string_lossy().to_string();
    let pattern = format!("{}/frame_%08d.png", fi_dir.display());
    let _ = Command::new("ffmpeg")
        .args(&["-i", &input_str, "-qscale:v", "2", &pattern, "-loglevel", "warning", "-stats"])
        .stdout(Stdio::inherit()).stderr(Stdio::inherit()).status();
    count_frames(&fi_dir) > 0
}

fn upscale_on_gpus(cfg: &Cfg, vid: &Video, gpus: &[u32]) -> bool {
    let fi_dir = cfg.jobs_dir.join(&vid.id).join("frames_in");
    let fo_dir = cfg.jobs_dir.join(&vid.id).join("frames_out");
    let count_in = count_frames(&fi_dir);
    let count_out = count_frames(&fo_dir);

    // Use actual max frame number as ground truth (not duration*fps estimate)
    let max_fi = max_frame_number(&fi_dir) as u32;
    let max_fo = max_frame_number(&fo_dir) as u32;
    let total = std::cmp::max(max_fi, max_fo).max(if vid.expected_frames > 0 { vid.expected_frames as u32 } else { count_in as u32 });
    if total == 0 {
        eprintln!("[{}] No frames to upscale! (in={}, expected={})", vid.id, count_in, vid.expected_frames);
        return false;
    }

    let remaining = if total as u64 > count_out { total as u64 - count_out } else { 0 };
    if remaining == 0 {
        eprintln!("[{}] All {} frames already done!", vid.id, total);
        return true;
    }

    eprintln!("[{}] Upscaling on GPUs {:?} (in={}, out={}, expected={}, remaining={})",
        vid.id, gpus, count_in, count_out, total, remaining);

    let upscale_py = format!("{}/upscale.py", cfg.home.display());
    let fi_str = fi_dir.to_string_lossy().to_string();
    let fo_str = fo_dir.to_string_lossy().to_string();
    let scale_str = vid.scale.to_string();

    // Smart splitting: count missing frames per segment, distribute evenly by WORK not by index
    // This prevents one GPU finishing early when prior runs already completed its segment
    let done_set: HashSet<String> = fs::read_dir(&fo_dir).into_iter().flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with("frame_") && n.ends_with(".png") && !n.contains(".tmp") { Some(n) } else { None }
        })
        .collect();

    // Build list of frames_in that still need processing (not in frames_out)
    let all_in: Vec<String> = {
        let mut v: Vec<String> = fs::read_dir(&fi_dir).into_iter().flatten()
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with("frame_") && n.ends_with(".png") && !n.contains(".tmp") { Some(n) } else { None }
            })
            .collect();
        v.sort();
        v
    };

    // Find indices of missing frames in the sorted all_in list
    let missing_indices: Vec<u32> = all_in.iter().enumerate()
        .filter(|(_, name)| !done_set.contains(name.as_str()))
        .map(|(i, _)| i as u32)
        .collect();

    let num_missing = missing_indices.len() as u32;
    eprintln!("[{}] {} missing frames to distribute across {} GPUs", vid.id, num_missing, gpus.len());

    let mut children = Vec::new();

    if gpus.len() > 1 && num_missing > 0 {
        // Split missing frames evenly: find list-index boundaries that give each GPU equal WORK
        let missing_per_gpu = (num_missing + gpus.len() as u32 - 1) / gpus.len() as u32;
        for (i, &gpu) in gpus.iter().enumerate() {
            let work_start = i as u32 * missing_per_gpu;
            let work_end = std::cmp::min((i as u32 + 1) * missing_per_gpu, num_missing);
            if work_start >= num_missing { continue; }

            // Map back to list indices: start from first missing frame, end after last missing frame
            let s = missing_indices[work_start as usize];
            let e = if work_end < num_missing {
                missing_indices[work_end as usize]
            } else {
                count_in as u32  // end of list
            };

            let gpu_log = format!("{}/gpu{}.log", cfg.home.display(), gpu);
            let log_file = fs::OpenOptions::new().create(true).append(true).open(&gpu_log).ok();
            let mut args: Vec<String> = vec![
                upscale_py.clone(), fi_str.clone(), fo_str.clone(), scale_str.clone(),
                "--gpu-id".into(), gpu.to_string(),
            ];
            eprintln!("[{}] GPU {}: list indices {}-{} ({} missing frames)", vid.id, gpu, s, e, work_end - work_start);
            args.extend_from_slice(&["--start".into(), s.to_string(), "--end".into(), e.to_string()]);

            if let Ok(c) = Command::new("python3")
                .args(&args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                .env("CUDA_VISIBLE_DEVICES", gpu.to_string())
                .stdout(log_file.as_ref().map(|f| Stdio::from(f.try_clone().unwrap())).unwrap_or(Stdio::inherit()))
                .stderr(Stdio::inherit())
                .spawn() {
                children.push(c);
            }
        }
    } else {
        // Single GPU or no missing: process all, skip done
        for &gpu in gpus.iter() {
            let gpu_log = format!("{}/gpu{}.log", cfg.home.display(), gpu);
            let log_file = fs::OpenOptions::new().create(true).append(true).open(&gpu_log).ok();
            let args: Vec<String> = vec![
                upscale_py.clone(), fi_str.clone(), fo_str.clone(), scale_str.clone(),
                "--gpu-id".into(), gpu.to_string(),
            ];
            eprintln!("[{}] GPU {}: all frames (skip done)", vid.id, gpu);

            if let Ok(c) = Command::new("python3")
                .args(&args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                .env("CUDA_VISIBLE_DEVICES", gpu.to_string())
                .stdout(log_file.as_ref().map(|f| Stdio::from(f.try_clone().unwrap())).unwrap_or(Stdio::inherit()))
                .stderr(Stdio::inherit())
                .spawn() {
                children.push(c);
            }
        }
    }

    // WAIT for ALL segments
    for mut c in children { let _ = c.wait(); }

    // Return new count
    let new_out = count_frames(&fo_dir);
    eprintln!("[{}] Upscaling done: {} frames_out", vid.id, new_out);
    true
}

fn verify_and_reassemble(cfg: &Cfg, vid: &Video) -> bool {
    let work_dir = cfg.jobs_dir.join(&vid.id);
    let fo_dir = work_dir.join("frames_out");
    let count_out = count_frames(&fo_dir);

    // CRITICAL: verify frame count
    // Use actual max frame number from frames_in (or frames_out) as ground truth
    let fi_dir_check = work_dir.join("frames_in");
    let max_fi = max_frame_number(&fi_dir_check);
    let max_fo = max_frame_number(&fo_dir);
    let actual_total = std::cmp::max(max_fi, max_fo); // highest frame number = total frames

    let expected = if actual_total > 0 { actual_total } else if vid.expected_frames > 0 { vid.expected_frames } else {
        // Fallback: read from job_meta
        let meta = work_dir.join("job_meta.json");
        fs::read_to_string(&meta).ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| {
                let t = v.get("total_frames").and_then(|t| t.as_u64()).unwrap_or(0);
                if t > 0 { Some(t) } else {
                    let d = v.get("duration_seconds").and_then(|d| d.as_f64()).unwrap_or(0.0);
                    let f = v.get("fps").and_then(|f| f.as_f64()).unwrap_or(25.0);
                    if d > 0.0 { Some((d * f) as u64) } else { None }
                }
            }).unwrap_or(0)
    };

    let threshold = if expected > 5 { expected - 5 } else { expected };
    if count_out < threshold {
        eprintln!("[{}] *** NOT ENOUGH FRAMES: {} out of {} expected (max_fi={}, max_fo={}) — SKIPPING REASSEMBLY ***",
            vid.id, count_out, expected, max_fi, max_fo);
        return false;
    }
    eprintln!("[{}] Verified: {}/{} frames — reassembling", vid.id, count_out, expected);

    // Frame gap check
    let gap_bin = ["/root/frame_gap_check", "/usr/local/bin/frame_gap_check"]
        .iter().find(|p| Path::new(p).exists()).map(|s| *s);
    if let Some(bin) = gap_bin {
        let fi_str = work_dir.join("frames_in").to_string_lossy().to_string();
        let fo_str = fo_dir.to_string_lossy().to_string();
        let _ = Command::new(bin).args(&[&fi_str, &fo_str]).status();
    }

    // Find original MKV for audio/brightness reference
    let input_mkv = work_dir.join(format!("{}.mkv", vid.id));
    let output_mkv = work_dir.join(format!("{}_{}{}.mkv", vid.id, vid.scale, "x"));
    if output_mkv.exists() { let _ = fs::remove_file(&output_mkv); }

    // Auto brightness matching (sample 10 frames, compare, compute gamma)
    let fi_dir = work_dir.join("frames_in");
    let vf_filter = get_brightness_filter(&fi_dir, &fo_dir);

    // Reassemble with ffmpeg
    let fo_pattern = format!("{}/frame_%08d.png", fo_dir.display());
    let fps_str = get_video_fps(&input_mkv);
    eprintln!("[{}] Reassembling with ffmpeg (fps={})...", vid.id, fps_str);

    let mut ffmpeg_args = vec![
        "-y", "-framerate", &fps_str, "-i", &fo_pattern,
    ];
    // Add audio from original if it exists
    let input_str = input_mkv.to_string_lossy().to_string();
    if input_mkv.exists() {
        ffmpeg_args.extend_from_slice(&["-i", &input_str, "-map", "0:v", "-map", "1:a?"]);
    }
    let output_str = output_mkv.to_string_lossy().to_string();
    let vf_owned;
    if !vf_filter.is_empty() {
        vf_owned = vf_filter.clone();
        ffmpeg_args.extend_from_slice(&["-vf", &vf_owned]);
    }
    ffmpeg_args.extend_from_slice(&[
        "-c:v", "libx264", "-crf", "18", "-preset", "medium",
        "-c:a", "copy", "-movflags", "+faststart",
        &output_str, "-loglevel", "warning", "-stats",
    ]);
    let _ = Command::new("ffmpeg").args(&ffmpeg_args)
        .stdout(Stdio::inherit()).stderr(Stdio::inherit()).status();

    if !output_mkv.exists() {
        eprintln!("[{}] Reassembly FAILED — no output MKV", vid.id);
        return false;
    }

    // Upload — ONCE only (lock file prevents duplicate uploads)
    let upload_lock = work_dir.join(".uploaded");
    if upload_lock.exists() {
        eprintln!("[{}] Already uploaded (lock file exists) — skipping upload", vid.id);
        return true;
    }
    eprintln!("[{}] Uploading...", vid.id);
    let upload_bin = if Path::new("/root/youtube_upload").exists() {
        "/root/youtube_upload"
    } else { "youtube_upload" };
    let status = Command::new(upload_bin)
        .args(&[&output_str, &format!("--video-id={}", vid.id)])
        .current_dir(&cfg.home)  // youtube_upload needs client_secret.json in cwd
        .stdout(Stdio::inherit()).stderr(Stdio::inherit())
        .status();
    // Only write lock on successful upload (exit code 0)
    if status.map(|s| s.success()).unwrap_or(false) {
        let _ = fs::write(&upload_lock, format!("uploaded_at={}\n", chrono_now()));
        eprintln!("[{}] Upload SUCCESS, lock file written", vid.id);
    } else {
        eprintln!("[{}] Upload FAILED — no lock file, will retry next cycle", vid.id);
    }

    true
}

fn chrono_now() -> String {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    format!("{}", d.as_secs())
}

/// Get fps from video file using ffprobe
fn get_video_fps(path: &Path) -> String {
    if !path.exists() { return "25".to_string(); }
    let out = Command::new("ffprobe")
        .args(&["-v", "quiet", "-select_streams", "v:0",
            "-show_entries", "stream=r_frame_rate",
            "-of", "csv=p=0", &path.to_string_lossy().to_string()])
        .output().ok();
    if let Some(o) = out {
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if s.contains('/') {
            let parts: Vec<&str> = s.split('/').collect();
            if parts.len() == 2 {
                if let (Ok(n), Ok(d)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                    if d > 0.0 { return format!("{:.3}", n / d); }
                }
            }
        }
        if !s.is_empty() { return s; }
    }
    "25".to_string()
}

/// Compute brightness correction filter by sampling original vs enhanced frames
fn get_brightness_filter(fi_dir: &Path, fo_dir: &Path) -> String {
    // Sample up to 10 frames from both dirs, compute average brightness
    let script = r#"
import cv2, os, sys, glob, numpy as np
fi = sorted(glob.glob(os.path.join(sys.argv[1], 'frame_*.png')))
fo = sorted(glob.glob(os.path.join(sys.argv[2], 'frame_*.png')))
if not fi or not fo:
    sys.exit(0)
step_i = max(1, len(fi) // 10)
step_o = max(1, len(fo) // 10)
samples_i = fi[::step_i][:10]
samples_o = fo[::step_o][:10]
avg_i = np.mean([cv2.cvtColor(cv2.imread(f), cv2.COLOR_BGR2GRAY).mean() for f in samples_i])
avg_o = np.mean([cv2.cvtColor(cv2.imread(f), cv2.COLOR_BGR2GRAY).mean() for f in samples_o])
if avg_o > 0 and abs(avg_i - avg_o) > 2:
    gamma = np.log(avg_i / 255.0) / np.log(avg_o / 255.0)
    gamma = max(0.5, min(2.0, gamma))
    if abs(gamma - 1.0) > 0.01:
        print("eq=gamma=%.4f" % gamma)
"#;
    let out = Command::new("python3")
        .args(&["-c", &script,
            &fi_dir.to_string_lossy().to_string(),
            &fo_dir.to_string_lossy().to_string()])
        .output().ok();
    if let Some(o) = out {
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !s.is_empty() {
            eprintln!("[brightness] Applying filter: {}", s);
            return s;
        }
    }
    String::new()
}

fn auto_destroy(cfg: &Cfg) {
    let inst_id = fs::read_to_string(cfg.home.join("instance_meta.json")).ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("instance_id").and_then(|s| s.as_str().map(|s| s.to_string())));
    let api_key = fs::read_to_string(cfg.home.join(".vast_api_key")).ok().map(|s| s.trim().to_string());
    if let (Some(id), Some(key)) = (inst_id, api_key) {
        eprintln!("[scheduler] Auto-destroying instance {}...", id);
        let _ = Command::new("curl")
            .args(&["-s", "-X", "PUT", &format!("https://console.vast.ai/api/v0/instances/{}/", id),
                "-H", &format!("Authorization: Bearer {}", key), "-d", r#"{"state": "stopped"}"#])
            .output();
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let num_gpus = args.get(1).and_then(|s| s.parse().ok()).unwrap_or_else(|| {
        Command::new("nvidia-smi").args(["--query-gpu=name", "--format=csv,noheader"])
            .output().ok().map(|o| String::from_utf8_lossy(&o.stdout).lines().count() as u32).unwrap_or(1)
    });

    let home = PathBuf::from(env::var("HOME").unwrap_or("/root".into()));
    let cfg = Cfg {
        json_dir: home.join("json"), done_dir: home.join("json_done"),
        jobs_dir: home.join("jobs"), home, num_gpus,
    };
    let _ = fs::create_dir_all(&cfg.json_dir);
    let _ = fs::create_dir_all(&cfg.done_dir);

    eprintln!("=== GPU Scheduler started: {} GPUs ===", num_gpus);

    loop {
        // Restore .processing files
        if let Ok(entries) = fs::read_dir(&cfg.json_dir) {
            for e in entries.filter_map(|e| e.ok()) {
                let name = e.file_name().to_string_lossy().to_string();
                if name.contains(".processing") {
                    let base = name.split(".processing").next().unwrap_or(&name);
                    let vid = base.strip_suffix(".json").unwrap_or(base);
                    if cfg.done_dir.join(format!("{}.json", vid)).exists() {
                        let _ = fs::remove_file(e.path());
                    } else {
                        let _ = fs::rename(e.path(), cfg.json_dir.join(base));
                    }
                }
            }
        }

        // Scan queue
        let mut videos: Vec<Video> = Vec::new();
        if let Ok(entries) = fs::read_dir(&cfg.json_dir) {
            for e in entries.filter_map(|e| e.ok()) {
                let name = e.file_name().to_string_lossy().to_string();
                if name.ends_with(".json") {
                    if let Some(v) = read_video(&e.path()) { videos.push(v); }
                }
            }
        }

        if videos.is_empty() {
            // Check for .processing files — don't destroy if work is in progress
            let has_processing = fs::read_dir(&cfg.json_dir)
                .map(|e| e.filter_map(|e| e.ok())
                    .any(|e| e.file_name().to_string_lossy().contains(".processing")))
                .unwrap_or(false);
            if has_processing {
                eprintln!("[scheduler] Queue empty but .processing files exist — waiting 30s");
                thread::sleep(Duration::from_secs(30));
                continue;
            }
            // Check if upload or ffmpeg is still running — don't destroy
            let upload_running = Command::new("pgrep").args(&["-f", "youtube_upload|ffmpeg"])
                .output().map(|o| !o.stdout.is_empty()).unwrap_or(false);
            if upload_running {
                eprintln!("[scheduler] Queue empty but upload/ffmpeg still running — waiting 60s");
                thread::sleep(Duration::from_secs(60));
                continue;
            }
            // Check for unuploaded jobs (MKV exists but no .uploaded lock)
            let has_unuploaded = fs::read_dir(&cfg.jobs_dir).into_iter().flatten()
                .filter_map(|e| e.ok())
                .any(|e| {
                    let p = e.path();
                    p.is_dir() && !p.join(".uploaded").exists() &&
                    fs::read_dir(&p).into_iter().flatten().any(|f| f.ok().map(|f| f.file_name().to_string_lossy().ends_with("x.mkv")).unwrap_or(false))
                });
            if has_unuploaded {
                eprintln!("[scheduler] Queue empty but unuploaded MKVs exist — waiting 60s");
                thread::sleep(Duration::from_secs(60));
                continue;
            }
            eprintln!("[scheduler] Queue empty, all uploaded. Auto-destroy in 10 min.");
            thread::sleep(Duration::from_secs(600));
            // Re-check: both .json and .processing files
            let still_empty = fs::read_dir(&cfg.json_dir)
                .map(|e| e.filter_map(|e| e.ok())
                    .filter(|e| {
                        let n = e.file_name().to_string_lossy().to_string();
                        n.ends_with(".json") || n.contains(".processing")
                    }).count() == 0)
                .unwrap_or(true);
            if still_empty { auto_destroy(&cfg); return; }
            continue;
        }

        eprintln!("[scheduler] {} videos, {} GPUs", videos.len(), num_gpus);

        // Phase 1: Mark all as processing, download+extract in background
        for vid in &videos {
            let proc = cfg.json_dir.join(format!("{}.json.processing", vid.id));
            let _ = fs::rename(&vid.json_path, &proc);
        }

        // Start background download+extract for ALL videos
        let prep_videos = videos.clone();
        let prep_home = cfg.home.clone();
        let prep_jobs = cfg.jobs_dir.clone();
        let prep_handle = thread::spawn(move || {
            for vid in &prep_videos {
                let c = Cfg { home: prep_home.clone(), json_dir: PathBuf::new(), done_dir: PathBuf::new(), jobs_dir: prep_jobs.clone(), num_gpus: 0 };
                download_and_extract(&c, vid);
            }
        });

        // Wait until first video has frames (don't start GPUs on empty dirs)
        let first_fi = cfg.jobs_dir.join(&videos[0].id).join("frames_in");
        while count_frames(&first_fi) == 0 {
            thread::sleep(Duration::from_secs(3));
        }

        // Phase 2+3: Smart scheduling
        // If a video is almost done (<5% remaining), give it ALL GPUs and finish it first
        // Otherwise distribute proportionally
        let all_gpus: Vec<u32> = (0..num_gpus).collect();

        // Sort: nearly-done videos first (use actual max frame number as ground truth)
        videos.sort_by(|a, b| {
            let a_out = count_frames(&cfg.jobs_dir.join(&a.id).join("frames_out"));
            let a_max = max_frame_number(&cfg.jobs_dir.join(&a.id).join("frames_in"))
                .max(max_frame_number(&cfg.jobs_dir.join(&a.id).join("frames_out")));
            let a_expected = a_max.max(a.expected_frames);
            let a_rem = if a_expected > a_out { a_expected - a_out } else { 0 };
            let b_out = count_frames(&cfg.jobs_dir.join(&b.id).join("frames_out"));
            let b_max = max_frame_number(&cfg.jobs_dir.join(&b.id).join("frames_in"))
                .max(max_frame_number(&cfg.jobs_dir.join(&b.id).join("frames_out")));
            let b_expected = b_max.max(b.expected_frames);
            let b_rem = if b_expected > b_out { b_expected - b_out } else { 0 };
            a_rem.cmp(&b_rem)
        });

        // Check if first video is nearly done (<2000 frames or <5% remaining)
        let first_out = count_frames(&cfg.jobs_dir.join(&videos[0].id).join("frames_out"));
        let first_max = max_frame_number(&cfg.jobs_dir.join(&videos[0].id).join("frames_in"))
            .max(max_frame_number(&cfg.jobs_dir.join(&videos[0].id).join("frames_out")));
        let first_expected = first_max.max(videos[0].expected_frames);
        let first_remaining = if first_expected > first_out { first_expected - first_out } else { 0 };
        let nearly_done = first_remaining > 0 && (first_remaining < 2000 || (first_expected > 0 && first_remaining * 100 / first_expected < 5));
        eprintln!("[scheduler] First video: {} out={} expected={} remaining={} nearly_done={}",
            videos[0].id, first_out, first_expected, first_remaining, nearly_done);

        if nearly_done && videos.len() > 1 {
            // Nearly done: 1 GPU finishes it (no --start/--end = processes all gaps),
            // remaining GPUs start on next video in parallel
            eprintln!("[{}] Nearly done ({} remaining) — GPU 0 finishes, GPUs 1-{} start next video",
                videos[0].id, first_remaining, num_gpus - 1);

            let finish_vid = videos[0].clone();
            let next_vid = videos[1].clone();
            let cfg_home1 = cfg.home.clone();
            let cfg_jobs1 = cfg.jobs_dir.clone();
            let cfg_home2 = cfg.home.clone();
            let cfg_jobs2 = cfg.jobs_dir.clone();
            let other_gpus: Vec<u32> = (1..num_gpus).collect();

            // GPU 0 finishes nearly-done video (no --start/--end, skips done frames)
            let h1 = thread::spawn(move || {
                let c = Cfg { home: cfg_home1, json_dir: PathBuf::new(), done_dir: PathBuf::new(), jobs_dir: cfg_jobs1, num_gpus: 1 };
                upscale_on_gpus(&c, &finish_vid, &[0]);
            });
            // Other GPUs start next video
            let h2 = thread::spawn(move || {
                let c = Cfg { home: cfg_home2, json_dir: PathBuf::new(), done_dir: PathBuf::new(), jobs_dir: cfg_jobs2, num_gpus: other_gpus.len() as u32 };
                upscale_on_gpus(&c, &next_vid, &other_gpus);
            });

            let _ = h1.join();
            // Verify + reassemble + upload (BLOCKING — no background threads)
            let proc = cfg.json_dir.join(format!("{}.json.processing", videos[0].id));
            if verify_and_reassemble(&cfg, &videos[0]) {
                let _ = fs::rename(&proc, cfg.done_dir.join(format!("{}.json", videos[0].id)));
                if videos.len() > 1 {
                    eprintln!("[{}] SUCCESS! Cleaning frames (more videos in queue).", videos[0].id);
                    let _ = fs::remove_dir_all(cfg.jobs_dir.join(&videos[0].id));
                } else {
                    eprintln!("[{}] SUCCESS! Keeping frames for manual verification.", videos[0].id);
                }
            } else {
                eprintln!("[{}] NOT READY — back to queue", videos[0].id);
                let _ = fs::rename(&proc, cfg.json_dir.join(format!("{}.json", videos[0].id)));
            }

            let _ = h2.join();
            // Loop back to rescan
            continue;
        }

        // Normal: distribute GPUs proportionally
        let gpus_per = std::cmp::max(1, num_gpus / videos.len() as u32);
        let mut assignments: Vec<Vec<u32>> = Vec::new();
        let mut idx = 0u32;
        for (i, _) in videos.iter().enumerate() {
            let n = if i == videos.len() - 1 { num_gpus - idx } else { gpus_per };
            let mut a = Vec::new();
            for _ in 0..n { if idx < num_gpus { a.push(idx); idx += 1; } }
            eprintln!("[{}] GPUs: {:?}", videos[i].id, a);
            assignments.push(a);
        }

        // Upscale ALL videos in parallel
        let mut handles: Vec<thread::JoinHandle<()>> = Vec::new();
        for (i, vid) in videos.iter().enumerate() {
            let gpus = assignments[i].clone();
            let cfg_home = cfg.home.clone();
            let cfg_jobs = cfg.jobs_dir.clone();
            let v = vid.clone();
            handles.push(thread::spawn(move || {
                let c = Cfg { home: cfg_home, json_dir: PathBuf::new(), done_dir: PathBuf::new(), jobs_dir: cfg_jobs, num_gpus: gpus.len() as u32 };
                upscale_on_gpus(&c, &v, &gpus);
            }));
        }
        // WAIT for ALL
        for h in handles { let _ = h.join(); }

        // Wait for background prep to finish
        let _ = prep_handle.join();

        // Phase 4: Verify + reassemble + upload (BLOCKING — safer than background threads)
        for vid in &videos {
            let proc = cfg.json_dir.join(format!("{}.json.processing", vid.id));
            if verify_and_reassemble(&cfg, vid) {
                let _ = fs::rename(&proc, cfg.done_dir.join(format!("{}.json", vid.id)));
                if videos.len() > 1 {
                    eprintln!("[{}] SUCCESS! Cleaning frames (more videos in queue).", vid.id);
                    let _ = fs::remove_dir_all(cfg.jobs_dir.join(&vid.id));
                } else {
                    eprintln!("[{}] SUCCESS! Keeping frames for manual verification.", vid.id);
                }
            } else {
                eprintln!("[{}] NOT READY — back to queue", vid.id);
                let _ = fs::rename(&proc, cfg.json_dir.join(format!("{}.json", vid.id)));
            }
        }
    }
}
