//! gpu_scheduler — Scans ~/json/ for videos, assigns GPUs, calls enhance per video
//!
//! 1. Scan ~/json/*.json for queued videos
//! 2. Distribute GPUs proportionally (2 videos + 4 GPUs = 2 per video)
//! 3. Call `enhance <url> <scale> --job-name <id> --gpu N` per video
//! 4. Wait for all enhance processes to finish
//! 5. Move JSON to json_done/, loop back to 1
//! 6. When queue empty: wait for youtube_upload to finish, then auto-destroy
//!
//! enhance handles: download → extract → upscale → verify → reassemble
//! youtube_upload --watch handles: poll for MKVs → upload → lock file
//!
//! Usage: gpu_scheduler [NUM_GPUS]

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
    json_path: PathBuf,
}

fn read_video(path: &Path) -> Option<Video> {
    let content = fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    let id = v.get("video_id")?.as_str()?.to_string();
    let scale = v.get("scale").and_then(|s| s.as_u64()).unwrap_or(4) as u32;
    Some(Video { id, scale, json_path: path.to_path_buf() })
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

/// Check if any process matching pattern is running
fn is_running(pattern: &str) -> bool {
    Command::new("pgrep").args(&["-f", pattern])
        .output().map(|o| !o.stdout.is_empty()).unwrap_or(false)
}

/// Check if unuploaded MKVs exist in jobs dir
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

fn main() {
    let args: Vec<String> = env::args().collect();
    let num_gpus = args.get(1).and_then(|s| s.parse().ok()).unwrap_or_else(|| {
        Command::new("nvidia-smi").args(["--query-gpu=name", "--format=csv,noheader"])
            .output().ok().map(|o| String::from_utf8_lossy(&o.stdout).lines().count() as u32).unwrap_or(1)
    });

    let home = PathBuf::from(env::var("HOME").unwrap_or("/root".into()));
    let cfg = Cfg {
        json_dir: home.join("json"), done_dir: home.join("json_done"),
        jobs_dir: home.join("jobs"), home: home.clone(), num_gpus,
    };
    let _ = fs::create_dir_all(&cfg.json_dir);
    let _ = fs::create_dir_all(&cfg.done_dir);

    // Find enhance binary
    let enhance_bin = if Path::new("/root/enhance").exists() {
        "/root/enhance".to_string()
    } else if Path::new("/usr/local/bin/enhance").exists() {
        "/usr/local/bin/enhance".to_string()
    } else {
        format!("{}/enhance.sh", home.display())
    };

    eprintln!("=== GPU Scheduler started: {} GPUs, enhance={} ===", num_gpus, enhance_bin);

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
                if name.ends_with(".json") && !name.contains(".processing") {
                    if let Some(v) = read_video(&e.path()) { videos.push(v); }
                }
            }
        }

        if videos.is_empty() {
            // Check for .processing files
            let has_processing = fs::read_dir(&cfg.json_dir)
                .map(|e| e.filter_map(|e| e.ok())
                    .any(|e| e.file_name().to_string_lossy().contains(".processing")))
                .unwrap_or(false);
            if has_processing {
                eprintln!("[scheduler] Queue empty but .processing files exist — waiting 30s");
                thread::sleep(Duration::from_secs(30));
                continue;
            }
            // Check if youtube_upload or ffmpeg still running
            if is_running("youtube_upload|ffmpeg") {
                eprintln!("[scheduler] Queue empty but upload/ffmpeg running — waiting 60s");
                thread::sleep(Duration::from_secs(60));
                continue;
            }
            // Check for unuploaded MKVs
            if has_unuploaded_mkvs(&cfg.jobs_dir) {
                eprintln!("[scheduler] Queue empty but unuploaded MKVs exist — waiting 60s");
                thread::sleep(Duration::from_secs(60));
                continue;
            }
            eprintln!("[scheduler] Queue empty, all done. Auto-destroy in 10 min.");
            thread::sleep(Duration::from_secs(600));
            // Re-check
            let still_empty = fs::read_dir(&cfg.json_dir)
                .map(|e| e.filter_map(|e| e.ok())
                    .filter(|e| {
                        let n = e.file_name().to_string_lossy().to_string();
                        n.ends_with(".json") || n.contains(".processing")
                    }).count() == 0)
                .unwrap_or(true);
            if still_empty && !has_unuploaded_mkvs(&cfg.jobs_dir) {
                auto_destroy(&cfg);
                return;
            }
            continue;
        }

        eprintln!("[scheduler] {} videos, {} GPUs", videos.len(), num_gpus);

        // Mark all as processing
        for vid in &videos {
            let proc = cfg.json_dir.join(format!("{}.json.processing", vid.id));
            let _ = fs::rename(&vid.json_path, &proc);
        }

        // Distribute GPUs proportionally across videos
        let gpus_per = std::cmp::max(1, num_gpus / videos.len() as u32);
        let mut gpu_idx = 0u32;
        let mut assignments: Vec<Vec<u32>> = Vec::new();
        for (i, _) in videos.iter().enumerate() {
            let n = if i == videos.len() - 1 { num_gpus - gpu_idx } else { gpus_per };
            let mut gpus = Vec::new();
            for _ in 0..n {
                if gpu_idx < num_gpus { gpus.push(gpu_idx); gpu_idx += 1; }
            }
            assignments.push(gpus);
        }

        // Start enhance for each video in parallel
        let mut handles: Vec<thread::JoinHandle<(String, bool)>> = Vec::new();
        for (i, vid) in videos.iter().enumerate() {
            let gpus = assignments[i].clone();
            let v = vid.clone();
            let bin = enhance_bin.clone();
            let gpu_str = gpus.iter().map(|g| g.to_string()).collect::<Vec<_>>().join(",");
            eprintln!("[{}] Starting enhance on GPUs {:?} (scale={}x)", v.id, gpus, v.scale);

            handles.push(thread::spawn(move || {
                let url = format!("https://www.youtube.com/watch?v={}", v.id);
                let status = Command::new(&bin)
                    .args(&[&url, &v.scale.to_string(), "--job-name", &v.id, "--gpu", &gpu_str])
                    .stdout(Stdio::inherit()).stderr(Stdio::inherit())
                    .status();
                let success = status.map(|s| s.success()).unwrap_or(false);
                (v.id.clone(), success)
            }));
        }

        // Wait for ALL enhance processes
        for h in handles {
            if let Ok((vid_id, success)) = h.join() {
                let proc = cfg.json_dir.join(format!("{}.json.processing", vid_id));
                if success {
                    eprintln!("[{}] enhance completed successfully", vid_id);
                    let _ = fs::rename(&proc, cfg.done_dir.join(format!("{}.json", vid_id)));
                } else {
                    eprintln!("[{}] enhance FAILED — back to queue", vid_id);
                    let _ = fs::rename(&proc, cfg.json_dir.join(format!("{}.json", vid_id)));
                }
            }
        }
    }
}
