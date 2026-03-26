//! GPU monitor: detects idle GPUs and triggers rebalance automatically.
//!
//! Runs as a background task in the status server. Every 60s:
//! 1. Query nvidia-smi for per-GPU utilization
//! 2. If any GPU < 10% for >2 min AND other GPUs are active → trigger rebalance
//! 3. Rebalance: kill upscale.py, run rebalance.sh to redistribute frames

use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time;

/// Track per-GPU idle state
struct GpuState {
    idle_since: Option<Instant>,
}

/// Start the GPU monitor as a background task
pub fn start_monitor(rebalancing: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let mut gpu_states: HashMap<u32, GpuState> = HashMap::new();
        let mut last_rebalance = Instant::now();

        loop {
            time::sleep(Duration::from_secs(60)).await;

            // Don't rebalance if we just did one (cooldown 5 min)
            if last_rebalance.elapsed() < Duration::from_secs(300) {
                continue;
            }

            // Don't rebalance if another rebalance is running
            if rebalancing.load(Ordering::Relaxed) {
                continue;
            }

            // Get GPU utilization from nvidia-smi
            let gpu_utils = match get_gpu_utilization() {
                Some(u) => u,
                None => continue,
            };

            let num_gpus = gpu_utils.len();
            if num_gpus <= 1 {
                continue;
            }

            // Count active upscale.py processes
            let upscale_count = count_upscale_processes();
            if upscale_count == 0 {
                // No upscaling running — nothing to rebalance
                // Reset idle states
                gpu_states.clear();
                continue;
            }

            // Update idle states
            let now = Instant::now();
            let mut idle_gpus = 0u32;
            let mut active_gpus = 0u32;

            for (gpu_id, util_pct) in &gpu_utils {
                let state = gpu_states.entry(*gpu_id).or_insert(GpuState {
                    idle_since: None,
                });

                if *util_pct < 10 {
                    // GPU is idle
                    if state.idle_since.is_none() {
                        state.idle_since = Some(now);
                    }
                    if let Some(since) = state.idle_since {
                        if now.duration_since(since) > Duration::from_secs(120) {
                            idle_gpus += 1;
                        }
                    }
                } else {
                    // GPU is active
                    state.idle_since = None;
                    active_gpus += 1;
                }
            }

            // Trigger rebalance if: idle GPUs > 0 AND active GPUs > 0 AND upscale running
            if idle_gpus > 0 && active_gpus > 0 && upscale_count > 0 {
                eprintln!(
                    "[rebalance] {} idle GPUs, {} active, {} upscale processes — triggering rebalance",
                    idle_gpus, active_gpus, upscale_count
                );

                rebalancing.store(true, Ordering::Relaxed);
                last_rebalance = Instant::now();

                // Run rebalance.sh in background
                let num = num_gpus as u32;
                tokio::spawn(async move {
                    run_rebalance(num).await;
                });

                // Reset idle states after rebalance
                gpu_states.clear();
            }
        }
    });
}

/// Query nvidia-smi for per-GPU utilization percentage
fn get_gpu_utilization() -> Option<Vec<(u32, u32)>> {
    let output = Command::new("nvidia-smi")
        .args(["--query-gpu=index,utilization.gpu", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results = Vec::new();
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if parts.len() >= 2 {
            if let (Ok(idx), Ok(util)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                results.push((idx, util));
            }
        }
    }
    Some(results)
}

/// Count running upscale.py processes
fn count_upscale_processes() -> u32 {
    let output = Command::new("pgrep")
        .args(["-f", "upscale.py"])
        .output();

    match output {
        Ok(o) => {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .count() as u32
        }
        Err(_) => 0,
    }
}

/// Run rebalance.sh
async fn run_rebalance(num_gpus: u32) {
    eprintln!("[rebalance] Starting rebalance.sh with {} GPUs", num_gpus);

    let result = tokio::process::Command::new("/root/rebalance.sh")
        .arg(num_gpus.to_string())
        .stdout(std::process::Stdio::from(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/root/rebalance.log")
                .unwrap_or_else(|_| std::fs::File::create("/dev/null").unwrap()),
        ))
        .stderr(std::process::Stdio::from(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/root/rebalance.log")
                .unwrap_or_else(|_| std::fs::File::create("/dev/null").unwrap()),
        ))
        .spawn();

    match result {
        Ok(mut child) => {
            let _ = child.wait().await;
            eprintln!("[rebalance] rebalance.sh completed");
        }
        Err(e) => {
            eprintln!("[rebalance] Failed to start rebalance.sh: {}", e);
        }
    }
}
