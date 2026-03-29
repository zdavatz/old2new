//! gpu_scheduler — Scans ~/jobs/ for videos with frames ready, assigns GPUs, upscales + reassembles
//!
//! 1. Scan ~/jobs/*/frames_in/ for videos with extracted frames
//! 2. Distribute GPUs proportionally (2 videos + 4 GPUs = 2 per video)
//! 3. Run upscale.py with segment splitting per GPU
//! 4. Verify frame count, reassemble with ffmpeg
//! 5. Loop back to 1
//!
//! preparer handles: scan ~/json/ → download → extract → ~/jobs/<id>/frames_in/
//! youtube_upload --watch handles: poll ~/jobs/ for MKVs → upload → .uploaded lock
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

/// Read scale from ~/json/<id>.json (or .processing variant)
/// Read JSON for a video — checks both .json and .json.processing
fn read_json(json_dir: &Path, id: &str) -> Option<serde_json::Value> {
    for suffix in &["", ".processing"] {
        let path = json_dir.join(format!("{}.json{}", id, suffix));
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                return Some(v);
            }
        }
    }
    None
}

fn read_scale(json_dir: &Path, id: &str) -> u32 {
    read_json(json_dir, id)
        .and_then(|v| v.get("scale").and_then(|s| s.as_u64()))
        .unwrap_or(4) as u32
}

/// Check if preparer has finished extraction (total_frames field present in JSON)
fn is_prepared(json_dir: &Path, id: &str) -> bool {
    read_json(json_dir, id)
        .and_then(|v| v.get("total_frames").and_then(|t| t.as_u64()))
        .unwrap_or(0) > 0
}

/// Scan ~/jobs/ for videos that have frames_in ready but no output MKV yet
fn find_ready_videos(cfg: &Cfg) -> Vec<Video> {
    let mut videos = Vec::new();
    if let Ok(entries) = fs::read_dir(&cfg.jobs_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) { continue; }
            let id = entry.file_name().to_string_lossy().to_string();
            let job_dir = entry.path();
            let fi_dir = job_dir.join("frames_in");
            let fo_dir = job_dir.join("frames_out");

            // Skip if preparer hasn't finished (no total_frames in JSON)
            if !is_prepared(&cfg.json_dir, &id) { continue; }

            // Skip if no frames_in
            if count_frames(&fi_dir) == 0 { continue; }

            // Skip if output MKV already exists (reassembly done)
            let has_output = fs::read_dir(&job_dir).into_iter().flatten()
                .any(|f| f.ok().map(|f| {
                    let n = f.file_name().to_string_lossy().to_string();
                    n.ends_with("_2x.mkv") || n.ends_with("_4x.mkv")
                }).unwrap_or(false));
            if has_output { continue; }

            let count_in = count_frames(&fi_dir);
            let count_out = count_frames(&fo_dir);

            // This video needs work (either upscaling or reassembly)
            let scale = read_scale(&cfg.json_dir, &id);
            videos.push(Video { id, scale });

            eprintln!("[scan] {} — in={} out={}", videos.last().unwrap().id, count_in, count_out);
        }
    }
    videos
}

fn upscale_on_gpus(cfg: &Cfg, vid: &Video, gpus: &[u32]) {
    let fi_dir = cfg.jobs_dir.join(&vid.id).join("frames_in");
    let fo_dir = cfg.jobs_dir.join(&vid.id).join("frames_out");
    let count_in = count_frames(&fi_dir);
    let count_out = count_frames(&fo_dir);

    let max_fi = max_frame_number(&fi_dir);
    let max_fo = max_frame_number(&fo_dir);
    let expected = std::cmp::max(max_fi, max_fo).max(count_in);
    let remaining = if expected > count_out { expected - count_out } else { 0 };

    if remaining == 0 {
        eprintln!("[{}] All {} frames already upscaled", vid.id, expected);
        return;
    }

    eprintln!("[{}] Upscaling on GPUs {:?} (in={}, out={}, remaining={})",
        vid.id, gpus, count_in, count_out, remaining);

    let upscale_py = format!("{}/upscale.py", cfg.home.display());
    let fi_str = fi_dir.to_string_lossy().to_string();
    let fo_str = fo_dir.to_string_lossy().to_string();
    let scale_str = vid.scale.to_string();

    // Smart splitting: distribute by actual missing frames
    let done_set: HashSet<String> = fs::read_dir(&fo_dir).into_iter().flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with("frame_") && n.ends_with(".png") && !n.contains(".tmp") { Some(n) } else { None }
        })
        .collect();

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

    let missing_indices: Vec<u32> = all_in.iter().enumerate()
        .filter(|(_, name)| !done_set.contains(name.as_str()))
        .map(|(i, _)| i as u32)
        .collect();

    let num_missing = missing_indices.len() as u32;
    eprintln!("[{}] {} missing frames across {} GPUs", vid.id, num_missing, gpus.len());

    let mut children = Vec::new();

    if gpus.len() > 1 && num_missing > 0 {
        let per_gpu = (num_missing + gpus.len() as u32 - 1) / gpus.len() as u32;
        for (i, &gpu) in gpus.iter().enumerate() {
            let work_start = i as u32 * per_gpu;
            let work_end = std::cmp::min((i as u32 + 1) * per_gpu, num_missing);
            if work_start >= num_missing { continue; }

            let s = missing_indices[work_start as usize];
            let e = if work_end < num_missing { missing_indices[work_end as usize] } else { count_in as u32 };

            let gpu_log = format!("{}/gpu{}.log", cfg.home.display(), gpu);
            let log_file = fs::OpenOptions::new().create(true).append(true).open(&gpu_log).ok();
            let args: Vec<String> = vec![
                upscale_py.clone(), fi_str.clone(), fo_str.clone(), scale_str.clone(),
                "--gpu-id".into(), gpu.to_string(),
                "--start".into(), s.to_string(), "--end".into(), e.to_string(),
            ];
            eprintln!("[{}] GPU {}: indices {}-{} ({} missing)", vid.id, gpu, s, e, work_end - work_start);

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

    for mut c in children { let _ = c.wait(); }
    eprintln!("[{}] Upscaling done: {} frames_out", vid.id, count_frames(&fo_dir));
}

fn reassemble(cfg: &Cfg, vid: &Video) -> bool {
    let work_dir = cfg.jobs_dir.join(&vid.id);
    let fi_dir = work_dir.join("frames_in");
    let fo_dir = work_dir.join("frames_out");
    let count_out = count_frames(&fo_dir);
    let max_fi = max_frame_number(&fi_dir);
    let max_fo = max_frame_number(&fo_dir);
    let expected = std::cmp::max(max_fi, max_fo);

    // Verify frame count
    let threshold = if expected > 5 { expected - 5 } else { expected };
    if count_out < threshold {
        eprintln!("[{}] NOT ENOUGH FRAMES: {} of {} — skipping reassembly", vid.id, count_out, expected);
        return false;
    }
    eprintln!("[{}] Verified: {}/{} frames — reassembling", vid.id, count_out, expected);

    // Frame gap check
    let gap_bin = ["/root/frame_gap_check", "/usr/local/bin/frame_gap_check"]
        .iter().find(|p| Path::new(p).exists()).map(|s| *s);
    if let Some(bin) = gap_bin {
        let _ = Command::new(bin)
            .args(&[&fi_dir.to_string_lossy().to_string(), &fo_dir.to_string_lossy().to_string()])
            .status();
    }

    // Brightness matching
    let vf_filter = get_brightness_filter(&fi_dir, &fo_dir);

    // Reassemble
    let input_mkv = work_dir.join(format!("{}.mkv", vid.id));
    let output_mkv = work_dir.join(format!("{}_{}{}.mkv", vid.id, vid.scale, "x"));
    if output_mkv.exists() { let _ = fs::remove_file(&output_mkv); }

    let fo_pattern = format!("{}/frame_%08d.png", fo_dir.display());
    let fps_str = get_video_fps(&input_mkv);
    eprintln!("[{}] ffmpeg reassembly (fps={})...", vid.id, fps_str);

    let mut ffmpeg_args = vec!["-y", "-framerate", &fps_str, "-i", &fo_pattern];
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

    if output_mkv.exists() {
        eprintln!("[{}] Reassembly complete: {}", vid.id, output_str);
        // Write confirmed_frames_out + output_mkv to JSON — youtube_upload waits for this
        if let Some(mut j) = read_json(&cfg.json_dir, &vid.id) {
            j["confirmed_frames_out"] = serde_json::json!(count_out);
            j["output_mkv"] = serde_json::json!(output_str);
            // Write to both json and json.processing (whichever exists)
            for suffix in &["", ".processing"] {
                let path = cfg.json_dir.join(format!("{}.json{}", vid.id, suffix));
                if path.exists() {
                    let _ = fs::write(&path, serde_json::to_string_pretty(&j).unwrap_or_default());
                    eprintln!("[{}] Wrote confirmed_frames_out={} to JSON", vid.id, count_out);
                    break;
                }
            }
        }
        true
    } else {
        eprintln!("[{}] Reassembly FAILED", vid.id);
        false
    }
}

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

fn get_brightness_filter(fi_dir: &Path, fo_dir: &Path) -> String {
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
        .args(&["-c", script,
            &fi_dir.to_string_lossy().to_string(),
            &fo_dir.to_string_lossy().to_string()])
        .output().ok();
    if let Some(o) = out {
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !s.is_empty() {
            eprintln!("[brightness] filter: {}", s);
            return s;
        }
    }
    String::new()
}

/// Check if any process matching pattern is running
fn is_running(pattern: &str) -> bool {
    Command::new("pgrep").args(&["-f", pattern])
        .output().map(|o| !o.stdout.is_empty()).unwrap_or(false)
}

fn has_unuploaded_mkvs(jobs_dir: &Path) -> bool {
    fs::read_dir(jobs_dir).into_iter().flatten()
        .filter_map(|e| e.ok())
        .any(|e| {
            let p = e.path();
            p.is_dir() && !p.join(".uploaded").exists() &&
            fs::read_dir(&p).into_iter().flatten()
                .any(|f| f.ok().map(|f| {
                    let n = f.file_name().to_string_lossy().to_string();
                    n.ends_with("_2x.mkv") || n.ends_with("_4x.mkv")
                }).unwrap_or(false))
        })
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
    let _ = fs::create_dir_all(&cfg.done_dir);

    eprintln!("=== GPU Scheduler started: {} GPUs ===", num_gpus);

    loop {
        // Find videos with frames ready for upscaling
        let videos = find_ready_videos(&cfg);

        if videos.is_empty() {
            if is_running("youtube_upload|ffmpeg|preparer") {
                thread::sleep(Duration::from_secs(30));
                continue;
            }
            if has_unuploaded_mkvs(&cfg.jobs_dir) {
                eprintln!("[scheduler] Waiting for uploads...");
                thread::sleep(Duration::from_secs(60));
                continue;
            }
            // Check if preparer might still add videos
            let has_json = fs::read_dir(&cfg.json_dir)
                .map(|e| e.filter_map(|e| e.ok())
                    .any(|e| e.file_name().to_string_lossy().contains(".json")))
                .unwrap_or(false);
            if has_json {
                eprintln!("[scheduler] Waiting for preparer to extract frames...");
                thread::sleep(Duration::from_secs(15));
                continue;
            }
            eprintln!("[scheduler] Nothing to do. Auto-destroy in 10 min.");
            thread::sleep(Duration::from_secs(600));
            if find_ready_videos(&cfg).is_empty() && !has_unuploaded_mkvs(&cfg.jobs_dir) {
                auto_destroy(&cfg);
                return;
            }
            continue;
        }

        eprintln!("[scheduler] {} videos ready, {} GPUs — processing first video with all GPUs", videos.len(), num_gpus);

        // Process ONE video at a time with ALL GPUs (sequential, not parallel)
        let vid = &videos[0];
        let all_gpus: Vec<u32> = (0..num_gpus).collect();
        eprintln!("[{}] All {} GPUs assigned", vid.id, num_gpus);

        upscale_on_gpus(&cfg, vid, &all_gpus);

        // Reassemble if complete
        let fo_dir = cfg.jobs_dir.join(&vid.id).join("frames_out");
        let fi_dir = cfg.jobs_dir.join(&vid.id).join("frames_in");
        // Use videos slice for reassembly loop compatibility
        let single = vec![vid.clone()];
        for vid in &single {
            let fo_dir = cfg.jobs_dir.join(&vid.id).join("frames_out");
            let fi_dir = cfg.jobs_dir.join(&vid.id).join("frames_in");
            let count_out = count_frames(&fo_dir);
            let expected = std::cmp::max(max_frame_number(&fi_dir), max_frame_number(&fo_dir));
            let threshold = if expected > 5 { expected - 5 } else { expected };

            if count_out >= threshold {
                if reassemble(&cfg, vid) {
                    // Move JSON to done
                    let json_path = cfg.json_dir.join(format!("{}.json", vid.id));
                    let proc_path = cfg.json_dir.join(format!("{}.json.processing", vid.id));
                    let done_path = cfg.done_dir.join(format!("{}.json", vid.id));
                    if json_path.exists() { let _ = fs::rename(&json_path, &done_path); }
                    else if proc_path.exists() { let _ = fs::rename(&proc_path, &done_path); }
                }
            }
        }
    }
}
