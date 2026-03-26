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
            s.starts_with("frame_") && s.ends_with(".png")
        })
        .count() as u64
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

    if count_in == 0 {
        eprintln!("[{}] No frames_in!", vid.id);
        return false;
    }

    eprintln!("[{}] Upscaling on GPUs {:?} (in={}, out={})", vid.id, gpus, count_in, count_out);

    let upscale_py = format!("{}/upscale.py", cfg.home.display());
    let fi_str = fi_dir.to_string_lossy().to_string();
    let fo_str = fo_dir.to_string_lossy().to_string();
    let scale_str = vid.scale.to_string();
    let per_gpu = ((count_in as u32) + gpus.len() as u32 - 1) / gpus.len() as u32;

    let mut children = Vec::new();
    for (i, &gpu) in gpus.iter().enumerate() {
        let s = i as u32 * per_gpu;
        let e = std::cmp::min((i as u32 + 1) * per_gpu, count_in as u32);
        if s >= count_in as u32 { continue; }
        let s_str = s.to_string();
        let e_str = e.to_string();
        let gpu_log = format!("{}/gpu{}.log", cfg.home.display(), gpu);
        let log_file = fs::OpenOptions::new().create(true).append(true).open(&gpu_log).ok();
        eprintln!("[{}] GPU {}: frames {}-{}", vid.id, gpu, s, e);
        if let Ok(c) = Command::new("python3")
            .args(&[&upscale_py as &str, &fi_str, &fo_str, &scale_str, "--start", &s_str, "--end", &e_str])
            .env("CUDA_VISIBLE_DEVICES", gpu.to_string())
            .stdout(log_file.as_ref().map(|f| Stdio::from(f.try_clone().unwrap())).unwrap_or(Stdio::inherit()))
            .stderr(Stdio::inherit())
            .spawn() {
            children.push(c);
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
    let fo_dir = cfg.jobs_dir.join(&vid.id).join("frames_out");
    let count_out = count_frames(&fo_dir);

    // CRITICAL: verify frame count
    let expected = if vid.expected_frames > 0 { vid.expected_frames } else {
        // Fallback: read from job_meta
        let meta = cfg.jobs_dir.join(&vid.id).join("job_meta.json");
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
        eprintln!("[{}] *** NOT ENOUGH FRAMES: {} out of {} expected — SKIPPING REASSEMBLY ***", vid.id, count_out, expected);
        return false;
    }
    eprintln!("[{}] Verified: {}/{} frames — reassembling", vid.id, count_out, expected);

    // Frame gap check
    let gap_bin = ["/root/frame_gap_check", "/usr/local/bin/frame_gap_check"]
        .iter().find(|p| Path::new(p).exists()).map(|s| *s);
    if let Some(bin) = gap_bin {
        let fi_str = cfg.jobs_dir.join(&vid.id).join("frames_in").to_string_lossy().to_string();
        let fo_str = fo_dir.to_string_lossy().to_string();
        let _ = Command::new(bin).args(&[&fi_str, &fo_str]).status();
    }

    // Brightness + reassemble via enhance binary
    let output = cfg.jobs_dir.join(&vid.id).join(format!("{}_{}{}.mkv", vid.id, vid.scale, "x"));
    if output.exists() { let _ = fs::remove_file(&output); } // Remove any corrupt MKV

    let url = format!("https://www.youtube.com/watch?v={}", vid.id);
    let enhance_bin = if Path::new("/root/enhance").exists() { "/root/enhance" } else { "/root/enhance.sh" };
    let s = Command::new(enhance_bin)
        .args(&[&url, &vid.scale.to_string(), "--job-name", &vid.id])
        .stdout(Stdio::inherit()).stderr(Stdio::inherit())
        .status();

    output.exists()
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
            eprintln!("[scheduler] Queue empty. Auto-destroy in 10 min.");
            thread::sleep(Duration::from_secs(600));
            // Re-check
            let still_empty = fs::read_dir(&cfg.json_dir)
                .map(|e| e.filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".json")).count() == 0)
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

        // Sort: nearly-done videos first (use expected_frames, not fi-fo diff)
        videos.sort_by(|a, b| {
            let a_out = count_frames(&cfg.jobs_dir.join(&a.id).join("frames_out"));
            let a_rem = if a.expected_frames > a_out { a.expected_frames - a_out } else { 0 };
            let b_out = count_frames(&cfg.jobs_dir.join(&b.id).join("frames_out"));
            let b_rem = if b.expected_frames > b_out { b.expected_frames - b_out } else { 0 };
            a_rem.cmp(&b_rem)
        });

        // Check if first video is nearly done (<2000 frames or <5% remaining)
        let first_out = count_frames(&cfg.jobs_dir.join(&videos[0].id).join("frames_out"));
        let first_expected = videos[0].expected_frames;
        let first_remaining = if first_expected > first_out { first_expected - first_out } else { 0 };
        let nearly_done = first_remaining > 0 && (first_remaining < 2000 || (first_expected > 0 && first_remaining * 100 / first_expected < 5));

        if nearly_done && videos.len() > 1 {
            // Finish first video on ALL GPUs, then handle rest
            eprintln!("[{}] Nearly done ({} remaining) — finishing on all {} GPUs first",
                videos[0].id, first_remaining, num_gpus);
            upscale_on_gpus(&cfg, &videos[0], &all_gpus);

            // Verify + reassemble + upload immediately
            let proc = cfg.json_dir.join(format!("{}.json.processing", videos[0].id));
            if verify_and_reassemble(&cfg, &videos[0]) {
                eprintln!("[{}] SUCCESS!", videos[0].id);
                let _ = fs::rename(&proc, cfg.done_dir.join(format!("{}.json", videos[0].id)));
                let _ = fs::remove_dir_all(cfg.jobs_dir.join(&videos[0].id));
            } else {
                eprintln!("[{}] NOT READY — back to queue", videos[0].id);
                let _ = fs::rename(&proc, cfg.json_dir.join(format!("{}.json", videos[0].id)));
            }

            // Now handle remaining videos — loop back to scan
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

        // Phase 4: Verify + reassemble + upload (sequential)
        for vid in &videos {
            let proc = cfg.json_dir.join(format!("{}.json.processing", vid.id));

            if verify_and_reassemble(&cfg, vid) {
                eprintln!("[{}] SUCCESS!", vid.id);
                let _ = fs::rename(&proc, cfg.done_dir.join(format!("{}.json", vid.id)));
                let _ = fs::remove_dir_all(cfg.jobs_dir.join(&vid.id));
            } else {
                eprintln!("[{}] NOT READY — back to queue", vid.id);
                let _ = fs::rename(&proc, cfg.json_dir.join(format!("{}.json", vid.id)));
            }
        }
    }
}
