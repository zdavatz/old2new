//! Fast frame gap detection and reporting.
//! Scans frames_out for missing frame numbers and reports gaps.
//!
//! Usage: frame_gap_check <frames_in_dir> <frames_out_dir>
//! Exit code: 0 = no gaps, 1 = gaps found (prints missing frame numbers)

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::Path;

fn collect_frame_numbers(dir: &Path) -> HashSet<u64> {
    let Ok(entries) = fs::read_dir(dir) else {
        return HashSet::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("frame_") && name.ends_with(".png") {
                name.strip_prefix("frame_")
                    .and_then(|s| s.strip_suffix(".png"))
                    .and_then(|s| s.parse::<u64>().ok())
            } else {
                None
            }
        })
        .collect()
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: frame_gap_check <frames_in_dir> <frames_out_dir>");
        std::process::exit(2);
    }

    let fi_dir = Path::new(&args[1]);
    let fo_dir = Path::new(&args[2]);

    let fi_nums = collect_frame_numbers(fi_dir);
    let fo_nums = collect_frame_numbers(fo_dir);

    if fi_nums.is_empty() && fo_nums.is_empty() {
        eprintln!("No frames found");
        std::process::exit(0);
    }

    // Find the expected range from frames_out
    let max_num = fo_nums.iter().max().copied().unwrap_or(0);
    if max_num == 0 {
        eprintln!("No output frames");
        std::process::exit(0);
    }

    // Check for gaps in output (1..max_num should all exist)
    let mut gaps: Vec<u64> = (1..=max_num)
        .filter(|n| !fo_nums.contains(n))
        .collect();
    gaps.sort();

    if gaps.is_empty() {
        println!("OK {}", fo_nums.len());
        std::process::exit(0);
    }

    // Check which gaps have input frames available for re-upscaling
    let mut fixable = 0u64;
    let mut unfixable = 0u64;
    for &gap in &gaps {
        if fi_nums.contains(&gap) {
            fixable += 1;
        } else {
            unfixable += 1;
        }
    }

    println!("GAPS {} fixable={} unfixable={}", gaps.len(), fixable, unfixable);
    // Print gap frame numbers (for re-upscaling)
    for gap in &gaps {
        println!("{}", gap);
    }

    std::process::exit(1);
}
