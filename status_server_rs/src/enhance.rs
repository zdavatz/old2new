//! enhance — Rust orchestrator replacing enhance.sh
//!
//! Usage: enhance <youtube-url> <scale> [--job-name <name>] [--gpu N]
//!
//! Pipeline: download → extract → upscale → gap-check → reassemble → upload
//! Each phase is resumable — checks for existing output before re-running.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------
struct Config {
    url: String,
    scale: u32,
    job_name: String,
    gpu_id: Option<u32>,
    home: PathBuf,
    work_dir: PathBuf,
    input: PathBuf,
    frames_in: PathBuf,
    frames_out: PathBuf,
    script_dir: PathBuf,
}

fn parse_args() -> Config {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: enhance <youtube-url> <scale> [--job-name <name>] [--gpu N]");
        std::process::exit(1);
    }

    let url = args[1].clone();
    let scale: u32 = args[2].parse().unwrap_or(4);
    let mut job_name = String::new();
    let mut gpu_id: Option<u32> = None;

    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--job-name" => {
                job_name = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            "--gpu" => {
                gpu_id = args.get(i + 1).and_then(|s| s.parse().ok());
                i += 2;
            }
            _ => i += 1,
        }
    }

    // Extract video ID from URL
    if job_name.is_empty() {
        job_name = extract_video_id(&url).unwrap_or_else(|| "unknown".to_string());
    }

    let home = PathBuf::from(env::var("HOME").unwrap_or_else(|_| "/root".to_string()));
    let work_dir = home.join("jobs").join(&job_name);
    let input = work_dir.join(format!("{}.mkv", &job_name));
    let frames_in = work_dir.join("frames_in");
    let frames_out = work_dir.join("frames_out");
    let script_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| home.clone());

    // Set CUDA_VISIBLE_DEVICES if gpu specified
    if let Some(g) = gpu_id {
        env::set_var("CUDA_VISIBLE_DEVICES", g.to_string());
    }

    Config {
        url,
        scale,
        job_name,
        gpu_id,
        home,
        work_dir,
        input,
        frames_in,
        frames_out,
        script_dir,
    }
}

fn extract_video_id(url: &str) -> Option<String> {
    // https://www.youtube.com/watch?v=ABC123
    if let Some(pos) = url.find("v=") {
        let rest = &url[pos + 2..];
        let end = rest.find('&').unwrap_or(rest.len());
        Some(rest[..end].to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------
fn run_cmd(cmd: &str, args: &[&str]) -> bool {
    let status = Command::new(cmd)
        .args(args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    match status {
        Ok(s) => s.success(),
        Err(e) => {
            eprintln!("ERROR: Failed to run {} {:?}: {}", cmd, args, e);
            false
        }
    }
}

fn run_cmd_output(cmd: &str, args: &[&str]) -> Option<String> {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn count_frames(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with("frame_") && name.ends_with(".png")
        })
        .count() as u64
}

fn find_cookies() -> Option<String> {
    let home = env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let path = format!("{}/cookies.txt", home);
    if Path::new(&path).exists() {
        Some(path)
    } else {
        None
    }
}

fn detect_gpu_count() -> u32 {
    run_cmd_output("nvidia-smi", &["--query-gpu=name", "--format=csv,noheader"])
        .map(|s| s.lines().count() as u32)
        .unwrap_or(1)
}

// ---------------------------------------------------------------------------
// Pipeline phases
// ---------------------------------------------------------------------------

fn phase_download(cfg: &Config) -> bool {
    if cfg.input.exists() {
        println!("Video already downloaded: {}", cfg.input.display());
        return true;
    }

    println!("Downloading video...");
    let start = Instant::now();

    let output_template = format!("{}/{}.%(ext)s", cfg.work_dir.display(), cfg.job_name);
    let mut args = vec![
        "--remote-components", "ejs:github",
        "-o", &output_template,
        "--merge-output-format", "mkv",
    ];

    let cookies_path;
    if let Some(ref c) = find_cookies() {
        cookies_path = c.clone();
        args.push("--cookies");
        args.push(&cookies_path);
    }
    args.push(&cfg.url);

    if !run_cmd("yt-dlp", &args) {
        // Try to find what yt-dlp produced
        if let Ok(entries) = fs::read_dir(&cfg.work_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().to_string();
                if (name.ends_with(".mkv") || name.ends_with(".mp4") || name.ends_with(".webm"))
                    && !name.contains("_2x") && !name.contains("_4x")
                {
                    let _ = fs::rename(entry.path(), &cfg.input);
                    break;
                }
            }
        }
    }

    if !cfg.input.exists() {
        eprintln!("ERROR: Download failed");
        return false;
    }

    println!("Downloaded in {:.0}s", start.elapsed().as_secs_f64());
    true
}

fn phase_extract(cfg: &Config) -> bool {
    let existing = count_frames(&cfg.frames_in);
    if existing > 0 {
        println!("Frames already extracted: {}", existing);
        return true;
    }

    println!("Extracting frames...");
    let start = Instant::now();

    let input_str = cfg.input.to_string_lossy().to_string();
    let frame_pattern = format!("{}/frame_%08d.png", cfg.frames_in.display());
    let success = run_cmd("ffmpeg", &[
        "-i", &input_str,
        "-qscale:v", "2",
        &frame_pattern,
        "-loglevel", "warning", "-stats",
    ]);

    let extracted = count_frames(&cfg.frames_in);
    println!("Extracted {} frames in {:.0}s", extracted, start.elapsed().as_secs_f64());
    success && extracted > 0
}

fn phase_upscale(cfg: &Config) -> bool {
    let count_out = count_frames(&cfg.frames_out);
    let count_in = count_frames(&cfg.frames_in);
    let total = std::cmp::max(count_in, count_out);

    if count_out >= total && total > 0 {
        println!("All {} frames already upscaled", count_out);
        return true;
    }

    println!("Upscaling frames ({}x)...", cfg.scale);
    let start = Instant::now();

    let avail_gpus = if cfg.gpu_id.is_some() {
        1
    } else {
        detect_gpu_count()
    };

    let total_frames = count_frames(&cfg.frames_in);

    if avail_gpus > 1 && total_frames > 1000 {
        // Segment split across GPUs
        println!("Segment splitting across {} GPUs ({} frames)", avail_gpus, total_frames);
        let per_gpu = ((total_frames as u32) + avail_gpus - 1) / avail_gpus;
        let mut children = Vec::new();

        let upscale_py = format!("{}/upscale.py", cfg.home.display());
        let fi_str = cfg.frames_in.to_string_lossy().to_string();
        let fo_str = cfg.frames_out.to_string_lossy().to_string();
        let scale_str = cfg.scale.to_string();

        for g in 0..avail_gpus {
            let s = g * per_gpu;
            let e = std::cmp::min((g + 1) * per_gpu, total_frames as u32);
            if s >= total_frames as u32 {
                continue;
            }

            let gpu_log = format!("{}/gpu{}.log", cfg.home.display(), g);
            let log_file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&gpu_log)
                .ok();

            let s_str = s.to_string();
            let e_str = e.to_string();
            println!("  GPU {}: frames {}-{}", g, s, e);
            let child = Command::new("python3")
                .args(&[
                    &upscale_py as &str,
                    &fi_str,
                    &fo_str,
                    &scale_str,
                    "--start", &s_str,
                    "--end", &e_str,
                ])
                .env("CUDA_VISIBLE_DEVICES", g.to_string())
                .stdout(log_file.as_ref().map(|f| Stdio::from(f.try_clone().unwrap())).unwrap_or(Stdio::inherit()))
                .stderr(Stdio::inherit())
                .spawn();

            if let Ok(c) = child {
                children.push(c);
            }
        }

        // Wait for all
        let mut failed = 0;
        for mut child in children {
            if let Ok(status) = child.wait() {
                if !status.success() {
                    failed += 1;
                }
            }
        }
        if failed > 0 {
            eprintln!("WARNING: {} GPU segment(s) failed", failed);
        }
    } else {
        // Single GPU
        let upscale_py = format!("{}/upscale.py", cfg.home.display());
        let fi_str = cfg.frames_in.to_string_lossy().to_string();
        let fo_str = cfg.frames_out.to_string_lossy().to_string();
        let scale_str = cfg.scale.to_string();
        run_cmd("python3", &[
            &upscale_py as &str,
            &fi_str,
            &fo_str,
            &scale_str,
        ]);
    }

    println!("Upscaling complete in {:.1}h", start.elapsed().as_secs_f64() / 3600.0);
    true
}

fn phase_gap_check(cfg: &Config) -> bool {
    println!("Checking for frame gaps...");

    // Try Rust binary first
    let gap_bin = if Path::new("/root/frame_gap_check").exists() {
        Some("/root/frame_gap_check")
    } else if Path::new("/usr/local/bin/frame_gap_check").exists() {
        Some("/usr/local/bin/frame_gap_check")
    } else {
        None
    };

    if let Some(bin) = gap_bin {
        let fi_str = cfg.frames_in.to_string_lossy().to_string();
        let fo_str = cfg.frames_out.to_string_lossy().to_string();
        let output = Command::new(bin)
            .args(&[
                &fi_str as &str,
                &fo_str as &str,
            ])
            .output();

        match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                if stdout.starts_with("OK") {
                    println!("  No gaps — all frames consecutive.");
                    return true;
                }
                // Has gaps — re-upscale them
                let lines: Vec<&str> = stdout.lines().collect();
                if let Some(first) = lines.first() {
                    println!("  {}", first);
                }
                // Gaps need re-upscaling via Python (needs model)
                println!("  Re-upscaling missing frames...");
                let upscale_py = format!("{}/upscale.py", cfg.home.display());
                let scale_str = cfg.scale.to_string();
                run_cmd("python3", &[
                    &upscale_py as &str,
                    &fi_str as &str,
                    &fo_str as &str,
                    &scale_str,
                ]);
            }
            Err(_) => {
                println!("  frame_gap_check failed, skipping");
            }
        }
    } else {
        println!("  frame_gap_check binary not found, skipping");
    }
    true
}

fn phase_reassemble(cfg: &Config) -> bool {
    let output = cfg.work_dir.join(format!("{}_{}{}.mkv",
        cfg.job_name, cfg.scale, "x"));

    if output.exists() {
        println!("Output already exists: {}", output.display());
        return true;
    }

    // Brightness matching: measure original vs enhanced
    let gamma = measure_brightness(cfg);
    let gamma_filter = if (gamma - 1.0).abs() > 0.003 {
        println!("  Applying gamma={:.3} to match original brightness", gamma);
        format!("-vf eq=gamma={:.3}", gamma)
    } else {
        println!("  Brightness matched — no correction needed");
        String::new()
    };

    // Get FPS from input
    let input_str = cfg.input.to_string_lossy().to_string();
    let fps = run_cmd_output("ffprobe", &[
        "-v", "quiet", "-select_streams", "v:0",
        "-show_entries", "stream=r_frame_rate",
        "-of", "csv=p=0",
        &input_str,
    ]).unwrap_or_else(|| "25/1".to_string());

    let fps_val: f64 = if let Some(pos) = fps.find('/') {
        let num: f64 = fps[..pos].parse().unwrap_or(25.0);
        let den: f64 = fps[pos + 1..].parse().unwrap_or(1.0);
        if den > 0.0 { num / den } else { 25.0 }
    } else {
        fps.parse().unwrap_or(25.0)
    };
    let fps_int = fps_val.round() as u32;

    println!("Reassembling video at {}fps...", fps_int);
    let start = Instant::now();

    // Check if source has audio
    let has_audio = run_cmd_output("ffprobe", &[
        "-v", "quiet", "-select_streams", "a",
        "-show_entries", "stream=codec_type",
        "-of", "csv=p=0",
        &input_str,
    ]).map(|s| !s.is_empty()).unwrap_or(false);

    let frame_pattern = format!("{}/frame_%08d.png", cfg.frames_out.display());
    let mut args: Vec<String> = vec![
        "-framerate".into(), fps_int.to_string(),
        "-i".into(), frame_pattern,
    ];
    if has_audio {
        args.extend(["-i".into(), cfg.input.to_string_lossy().to_string()]);
        args.extend(["-map".into(), "0:v".into(), "-map".into(), "1:a".into()]);
    }
    if !gamma_filter.is_empty() {
        // Split -vf eq=gamma=X into proper args
        args.extend(["-vf".into(), format!("eq=gamma={:.3}", gamma)]);
    }
    args.extend([
        "-c:v".into(), "libx264".into(),
        "-crf".into(), "18".into(),
        "-preset".into(), "medium".into(),
        "-pix_fmt".into(), "yuv420p".into(),
    ]);
    if has_audio {
        args.extend(["-c:a".into(), "copy".into()]);
    }
    args.extend(["-y".into(), output.to_string_lossy().to_string()]);
    args.extend(["-loglevel".into(), "warning".into(), "-stats".into()]);

    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_cmd("ffmpeg", &args_ref);

    println!("Reassembly complete in {:.0}s", start.elapsed().as_secs_f64());
    output.exists()
}

fn measure_brightness(cfg: &Config) -> f64 {
    // Use Python for OpenCV brightness measurement (can't avoid it)
    let result = run_cmd_output("python3", &["-c", &format!(
        r#"
import cv2, numpy as np, subprocess, glob

frames_out = sorted(glob.glob('{}/frame_*.png'))
if not frames_out:
    print('1.0')
    exit()

sample_idx = [int(i * len(frames_out) / 10) for i in range(10)]
enh_means = []
for idx in sample_idx:
    img = cv2.imread(frames_out[idx], cv2.IMREAD_UNCHANGED)
    if img is not None:
        enh_means.append(img.mean())

import os
duration = 0
try:
    r = subprocess.run(['ffprobe', '-v', 'quiet', '-show_entries', 'format=duration', '-of', 'csv=p=0', '{}'], capture_output=True, text=True, timeout=10)
    duration = float(r.stdout.strip())
except:
    pass

if duration <= 0:
    print('1.0')
    exit()

orig_means = []
for i in range(10):
    ts = duration * i / 10
    r = subprocess.run(['ffmpeg', '-ss', str(ts), '-i', '{}', '-frames:v', '1', '-f', 'rawvideo', '-pix_fmt', 'bgr24', '-'], capture_output=True, timeout=10)
    if r.returncode == 0 and len(r.stdout) > 0:
        arr = np.frombuffer(r.stdout, dtype=np.uint8)
        orig_means.append(arr.mean())

if not orig_means or not enh_means:
    print('1.0')
    exit()

orig_avg = np.mean(orig_means)
enh_avg = np.mean(enh_means)
if enh_avg > 0 and orig_avg > 0:
    gamma = max(0.7, min(1.3, orig_avg / enh_avg))
    print(f'{{gamma:.3f}}')
else:
    print('1.0')
"#,
        cfg.frames_out.display(),
        cfg.input.display(),
        cfg.input.display(),
    )]);

    result
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or(1.0)
}

fn phase_upload(cfg: &Config) -> bool {
    let output = cfg.work_dir.join(format!("{}_{}{}.mkv",
        cfg.job_name, cfg.scale, "x"));

    if !output.exists() {
        eprintln!("No output file found, skipping upload");
        return false;
    }

    println!("\nUploading to YouTube...");

    let video_id = extract_video_id(&cfg.url).unwrap_or_else(|| cfg.job_name.clone());
    let vid_arg = format!("--video-id={}", video_id);

    // Try Rust binary
    let upload_bin = if Path::new(&format!("{}/youtube_upload", cfg.home.display())).exists() {
        Some(format!("{}/youtube_upload", cfg.home.display()))
    } else {
        run_cmd_output("which", &["youtube_upload"])
    };

    if let Some(bin) = upload_bin {
        let output_str = output.to_string_lossy().to_string();
        let client_secret = format!("{}/client_secret.json", cfg.home.display());
        let token = format!("{}/youtube_token.json", cfg.home.display());
        let success = run_cmd(&bin, &[
            &vid_arg,
            &output_str,
            "--client-secret", &client_secret,
            "--token", &token,
        ]);
        if success {
            return true;
        }
        eprintln!("Rust upload failed, trying Python fallback...");
    }

    // Python fallback
    let py_upload = format!("{}/youtube_upload.py", cfg.home.display());
    if Path::new(&py_upload).exists() {
        let output_str = output.to_string_lossy().to_string();
        return run_cmd("python3", &[
            &py_upload,
            &vid_arg,
            &output_str,
        ]);
    }

    eprintln!("No upload binary found. Keeping work directory for retry.");
    false
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------
fn main() {
    let cfg = parse_args();

    // Create dirs
    let _ = fs::create_dir_all(&cfg.frames_in);
    let _ = fs::create_dir_all(&cfg.frames_out);

    println!("Video ID:  {}", extract_video_id(&cfg.url).unwrap_or_default());
    println!("Scale:     {}x", cfg.scale);
    println!("Job name:  {}", cfg.job_name);
    println!("Work dir:  {}", cfg.work_dir.display());
    println!();

    // Fast resume: if frames exist, skip download/extract
    let existing_in = count_frames(&cfg.frames_in);
    let existing_out = count_frames(&cfg.frames_out);

    if existing_in > 0 || existing_out > 0 {
        println!("Resuming: {} input frames, {} output frames", existing_in, existing_out);
    } else {
        if !phase_download(&cfg) {
            std::process::exit(1);
        }
        if !phase_extract(&cfg) {
            std::process::exit(1);
        }
    }

    if !phase_upscale(&cfg) {
        std::process::exit(1);
    }

    phase_gap_check(&cfg);

    if !phase_reassemble(&cfg) {
        std::process::exit(1);
    }

    if !phase_upload(&cfg) {
        eprintln!("Upload failed. Keeping work directory for retry.");
        std::process::exit(1);
    }

    println!("\nUpload successful. Cleaning up work directory...");
    // Don't delete work dir — let multi_gpu_queue.sh handle cleanup
}
