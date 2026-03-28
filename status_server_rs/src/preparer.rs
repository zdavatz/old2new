//! preparer — Scans ~/json/ for videos, downloads and extracts frames
//!
//! Runs independently of gpu_scheduler. No GPU needed.
//! For each JSON in ~/json/:
//!   1. Download video via yt-dlp → ~/jobs/<id>/<id>.mkv
//!   2. Extract frames via ffmpeg → ~/jobs/<id>/frames_in/
//!   3. Mark as ready (frames_in exists with frames)
//!
//! gpu_scheduler picks up videos that have frames_in/ ready.
//! Polls every 10 seconds for new JSON files.
//!
//! Usage: preparer [--poll-interval N]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use std::collections::HashSet;

fn main() {
    let args: Vec<String> = env::args().collect();
    let poll_secs: u64 = args.iter().position(|a| a == "--poll-interval")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    let home = PathBuf::from(env::var("HOME").unwrap_or("/root".into()));
    let json_dir = home.join("json");
    let jobs_dir = home.join("jobs");
    let _ = fs::create_dir_all(&json_dir);
    let _ = fs::create_dir_all(&jobs_dir);

    eprintln!("=== Preparer started (poll={}s) ===", poll_secs);

    // Track what we've already prepared to avoid re-processing
    let mut prepared: HashSet<String> = HashSet::new();

    loop {
        // Scan ~/json/ for video JSONs
        if let Ok(entries) = fs::read_dir(&json_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().to_string();
                // Process both .json and .json.processing files
                if !name.contains(".json") { continue; }

                let json_path = entry.path();
                let content = match fs::read_to_string(&json_path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let v: serde_json::Value = match serde_json::from_str(&content) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let id = match v.get("video_id").and_then(|s| s.as_str()) {
                    Some(id) => id.to_string(),
                    None => continue,
                };

                // Skip if already prepared
                if prepared.contains(&id) { continue; }

                let work_dir = jobs_dir.join(&id);
                let fi_dir = work_dir.join("frames_in");

                // Skip if frames already extracted
                if fi_dir.exists() && count_frames(&fi_dir) > 0 {
                    eprintln!("[preparer] {} — frames already exist ({})", id, count_frames(&fi_dir));
                    prepared.insert(id);
                    continue;
                }

                // Prepare this video
                eprintln!("[preparer] {} — starting download + extract", id);
                let _ = fs::create_dir_all(&fi_dir);
                let _ = fs::create_dir_all(work_dir.join("frames_out"));

                let input = work_dir.join(format!("{}.mkv", id));

                // Download if needed
                if !input.exists() {
                    eprintln!("[preparer] {} — downloading...", id);
                    let template = format!("{}/{}.%(ext)s", work_dir.display(), id);
                    let mut dl_args = vec![
                        "--remote-components".to_string(), "ejs:github".to_string(),
                        "-o".to_string(), template,
                        "--merge-output-format".to_string(), "mkv".to_string(),
                    ];
                    let cookies = home.join("cookies.txt");
                    if cookies.exists() {
                        dl_args.push("--cookies".to_string());
                        dl_args.push(cookies.to_string_lossy().to_string());
                    }
                    dl_args.push(format!("https://www.youtube.com/watch?v={}", id));
                    let _ = Command::new("yt-dlp")
                        .args(&dl_args.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                        .stdout(Stdio::inherit()).stderr(Stdio::inherit())
                        .status();

                    // yt-dlp may produce different filename — find and rename
                    if !input.exists() {
                        if let Ok(entries) = fs::read_dir(&work_dir) {
                            for e in entries.filter_map(|e| e.ok()) {
                                let n = e.file_name().to_string_lossy().to_string();
                                if (n.ends_with(".mkv") || n.ends_with(".mp4"))
                                    && !n.contains("_2x") && !n.contains("_4x") {
                                    let _ = fs::rename(e.path(), &input);
                                    break;
                                }
                            }
                        }
                    }
                    if !input.exists() {
                        eprintln!("[preparer] {} — download FAILED", id);
                        continue;
                    }
                    eprintln!("[preparer] {} — download complete", id);
                }

                // Extract frames
                if count_frames(&fi_dir) == 0 {
                    eprintln!("[preparer] {} — extracting frames...", id);
                    let input_str = input.to_string_lossy().to_string();
                    let pattern = format!("{}/frame_%08d.png", fi_dir.display());
                    let _ = Command::new("ffmpeg")
                        .args(&["-i", &input_str, "-qscale:v", "2", &pattern,
                            "-loglevel", "warning", "-stats"])
                        .stdout(Stdio::inherit()).stderr(Stdio::inherit())
                        .status();

                    let n = count_frames(&fi_dir);
                    if n > 0 {
                        eprintln!("[preparer] {} — extracted {} frames", id, n);
                        // Write total_frames back to JSON — gpu_scheduler waits for this
                        if let Ok(mut j) = serde_json::from_str::<serde_json::Value>(&content) {
                            j["total_frames"] = serde_json::json!(n);
                            let _ = fs::write(&json_path, serde_json::to_string_pretty(&j).unwrap_or_default());
                            eprintln!("[preparer] {} — wrote total_frames={} to JSON", id, n);
                        }
                    } else {
                        eprintln!("[preparer] {} — extraction FAILED", id);
                        continue;
                    }
                }

                prepared.insert(id);
            }
        }

        thread::sleep(Duration::from_secs(poll_secs));
    }
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
