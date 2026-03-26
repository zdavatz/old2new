//! Fast frame counting and segment calculation for rebalance.
//! Called by rebalance.sh instead of slow Python glob+set.
//!
//! Usage: rebalance_calc <num_gpus> [jobs_dir]
//! Output: one line per GPU with tasks, ready for rebalance.sh to parse
//!   GPU:0 vid1:start:end:scale vid2:start:end:scale
//!   GPU:1 vid1:start:end:scale
//!   ...

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn count_frames(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("frame_") && name.ends_with(".png") {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    names.sort();
    names
}

fn find_todo_range(frames_in: &[String], frames_out: &HashSet<String>) -> (usize, usize, usize) {
    // Find frames in frames_in that are NOT in frames_out
    let todo_indices: Vec<usize> = frames_in
        .iter()
        .enumerate()
        .filter(|(_, name)| !frames_out.contains(*name))
        .map(|(i, _)| i)
        .collect();

    if todo_indices.is_empty() {
        return (0, 0, 0);
    }

    let start = todo_indices[0];
    let end = *todo_indices.last().unwrap() + 1;
    let count = todo_indices.len();
    (start, end, count)
}

struct VideoWork {
    vid: String,
    scale: u32,
    total_in: usize,
    todo_start: usize,
    todo_end: usize,
    todo_count: usize,
}

fn get_scale(vid: &str) -> u32 {
    // Try json/vid.json, json/vid.json.processing.*, json_done/vid.json
    let home = env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let json_dir = Path::new(&home).join("json");
    let done_dir = Path::new(&home).join("json_done");

    for pattern_dir in &[&json_dir, &done_dir] {
        // Direct match
        let direct = pattern_dir.join(format!("{}.json", vid));
        if let Ok(content) = fs::read_to_string(&direct) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(s) = v.get("scale").and_then(|s| s.as_u64()) {
                    return s as u32;
                }
            }
        }
        // Processing variants
        if let Ok(entries) = fs::read_dir(pattern_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(&format!("{}.json.processing", vid)) {
                    if let Ok(content) = fs::read_to_string(entry.path()) {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                            if let Some(s) = v.get("scale").and_then(|s| s.as_u64()) {
                                return s as u32;
                            }
                        }
                    }
                }
            }
        }
    }
    4 // default
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: rebalance_calc <num_gpus> [jobs_dir]");
        std::process::exit(1);
    }

    let num_gpus: usize = args[1].parse().unwrap_or(4);
    let home = env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let jobs_dir = if args.len() > 2 {
        PathBuf::from(&args[2])
    } else {
        Path::new(&home).join("jobs")
    };

    // Scan all job directories
    let mut work_items: Vec<VideoWork> = Vec::new();
    let mut total_todo = 0usize;

    if let Ok(entries) = fs::read_dir(&jobs_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let vid = entry.file_name().to_string_lossy().to_string();
            let fi_dir = entry.path().join("frames_in");
            let fo_dir = entry.path().join("frames_out");

            if !fi_dir.exists() {
                continue;
            }

            let frames_in = count_frames(&fi_dir);
            if frames_in.is_empty() {
                continue;
            }

            let frames_out: HashSet<String> = count_frames(&fo_dir).into_iter().collect();
            let (todo_start, todo_end, todo_count) = find_todo_range(&frames_in, &frames_out);

            if todo_count == 0 {
                continue;
            }

            let scale = get_scale(&vid);
            eprintln!(
                "  {} {:>6} remaining ({}/{} done) scale={}x",
                vid,
                todo_count,
                frames_out.len(),
                frames_in.len() + frames_out.len(),
                scale
            );

            total_todo += todo_count;
            work_items.push(VideoWork {
                vid,
                scale,
                total_in: frames_in.len(),
                todo_start,
                todo_end,
                todo_count,
            });
        }
    }

    if work_items.is_empty() {
        println!("NOTHING");
        return;
    }

    // Sort by remaining (largest first)
    work_items.sort_by(|a, b| b.todo_count.cmp(&a.todo_count));

    eprintln!(
        "  Total: {} frames across {} videos on {} GPUs",
        total_todo,
        work_items.len(),
        num_gpus
    );

    // Distribute work across GPUs
    let mut gpu_tasks: Vec<Vec<String>> = vec![Vec::new(); num_gpus];
    let mut gpu_loads: Vec<usize> = vec![0; num_gpus];

    for work in &work_items {
        if work.todo_count <= total_todo / num_gpus + 1 {
            // Small enough for one GPU — assign to least loaded
            let min_gpu = gpu_loads
                .iter()
                .enumerate()
                .min_by_key(|(_, load)| *load)
                .map(|(i, _)| i)
                .unwrap_or(0);
            gpu_tasks[min_gpu].push(format!(
                "{}:{}:{}:{}",
                work.vid, work.todo_start, work.todo_end, work.scale
            ));
            gpu_loads[min_gpu] += work.todo_count;
        } else {
            // Large video — split across GPUs
            let range = work.todo_end - work.todo_start;
            let per_gpu = (range + num_gpus - 1) / num_gpus;
            for g in 0..num_gpus {
                let start = work.todo_start + g * per_gpu;
                let end = std::cmp::min(work.todo_start + (g + 1) * per_gpu, work.todo_end);
                if start >= work.todo_end {
                    continue;
                }
                gpu_tasks[g].push(format!("{}:{}:{}:{}", work.vid, start, end, work.scale));
                gpu_loads[g] += end - start;
            }
        }
    }

    // Output: one line per GPU
    for (g, tasks) in gpu_tasks.iter().enumerate() {
        if tasks.is_empty() {
            continue;
        }
        println!("GPU:{} {}", g, tasks.join(" "));
    }
}
