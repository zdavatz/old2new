//! upscale_rs — Rust Real-ESRGAN upscaler with pipelined I/O
//!
//! Replaces upscale.py with 3-thread pipeline:
//!   Thread 1 (CPU): read frame N+1 from disk
//!   Thread 2 (GPU): upscale frame N with Real-ESRGAN
//!   Thread 3 (CPU): write frame N-1 to disk
//!
//! GPU never waits for I/O → ~50-80% utilization vs ~40% in Python.
//!
//! Usage: upscale_rs <frames_in> <frames_out> <scale> [--tile N] [--start N] [--end N]

use clap::Parser;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Instant;
use tch::{CModule, Device, Kind, Tensor};

#[derive(Parser)]
struct Args {
    frames_in: PathBuf,
    frames_out: PathBuf,
    scale: i64,
    #[arg(long, default_value_t = 0)]
    tile: i64,
    #[arg(long, default_value_t = 0)]
    start: usize,
    #[arg(long, default_value_t = 0)]
    end: usize,
}

/// Load and sort frame paths
fn gather_frames(dir: &Path) -> Vec<PathBuf> {
    let mut frames: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("frame_") && n.ends_with(".png"))
                .unwrap_or(false)
        })
        .collect();
    frames.sort();
    frames
}

/// Auto-detect tile size based on VRAM
fn auto_tile(scale: i64) -> i64 {
    if !tch::Cuda::is_available() {
        return 512;
    }
    let device = Device::Cuda(0);
    let (free, total) = tch::Cuda::mem_get_info(device);
    let vram_gb = total as f64 / 1024.0 / 1024.0 / 1024.0;
    // Same logic as upscale.py
    if vram_gb <= 32.0 {
        512
    } else if vram_gb <= 48.0 {
        512
    } else {
        0 // no tiling for very large VRAM
    }
}

/// Upscale a single tile using the model
fn upscale_tile(model: &CModule, tile: &Tensor, scale: i64) -> Tensor {
    tch::no_grad(|| {
        let input = tile.unsqueeze(0); // [1, C, H, W]
        let output = model.forward_ts(&[&input]).expect("Model forward failed");
        output.squeeze_dim(0) // [C, H, W]
    })
}

/// Upscale a full image with tiling
fn upscale_image(model: &CModule, img: &Tensor, scale: i64, tile_size: i64, tile_pad: i64) -> Tensor {
    let (_, h, w) = img.size3().expect("Expected 3D tensor [C, H, W]");

    if tile_size == 0 || (h <= tile_size && w <= tile_size) {
        // No tiling needed
        return upscale_tile(model, img, scale);
    }

    // Tiled processing
    let out_h = h * scale;
    let out_w = w * scale;
    let mut output = Tensor::zeros([3, out_h, out_w], (Kind::Float, img.device()));

    let tiles_y = (h + tile_size - 1) / tile_size;
    let tiles_x = (w + tile_size - 1) / tile_size;

    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            // Input tile with padding
            let y_start = (ty * tile_size - tile_pad).max(0);
            let y_end = ((ty + 1) * tile_size + tile_pad).min(h);
            let x_start = (tx * tile_size - tile_pad).max(0);
            let x_end = ((tx + 1) * tile_size + tile_pad).min(w);

            let tile = img.narrow(1, y_start, y_end - y_start)
                         .narrow(2, x_start, x_end - x_start);

            let out_tile = upscale_tile(model, &tile, scale);

            // Output tile position (without padding)
            let out_y_start = ty * tile_size * scale;
            let out_x_start = tx * tile_size * scale;

            // Padding offsets in output space
            let pad_y = (ty * tile_size - y_start) * scale;
            let pad_x = (tx * tile_size - x_start) * scale;

            let tile_h = ((ty + 1) * tile_size).min(h) - ty * tile_size;
            let tile_w = ((tx + 1) * tile_size).min(w) - tx * tile_size;
            let out_tile_h = tile_h * scale;
            let out_tile_w = tile_w * scale;

            let cropped = out_tile
                .narrow(1, pad_y, out_tile_h)
                .narrow(2, pad_x, out_tile_w);

            output
                .narrow(1, out_y_start, out_tile_h)
                .narrow(2, out_x_start, out_tile_w)
                .copy_(&cropped);
        }
    }

    output
}

/// Read a PNG frame as a float tensor [C, H, W] in range [0, 1]
fn read_frame(path: &Path, device: Device) -> Option<Tensor> {
    let img = image::open(path).ok()?.to_rgb8();
    let (w, h) = (img.width() as i64, img.height() as i64);
    let raw = img.into_raw();
    let tensor = Tensor::from_slice(&raw)
        .reshape([h, w, 3])
        .permute([2, 0, 1]) // [C, H, W]
        .to_kind(Kind::Float)
        / 255.0;
    Some(tensor.to(device))
}

/// Write a float tensor [C, H, W] in range [0, 1] as PNG
fn write_frame(tensor: &Tensor, path: &Path) {
    let tensor = (tensor.clamp(0.0, 1.0) * 255.0).to_kind(Kind::Uint8).to(Device::Cpu);
    let (c, h, w) = tensor.size3().expect("Expected [C, H, W]");
    let data: Vec<u8> = Vec::from(tensor.permute([1, 2, 0]).contiguous().view([-1]));
    let img = image::RgbImage::from_raw(w as u32, h as u32, data).expect("Failed to create image");

    // Atomic write: tmp then rename
    let tmp_path = path.with_extension("tmp.png");
    img.save(&tmp_path).expect("Failed to save frame");
    std::fs::rename(&tmp_path, path).expect("Failed to rename");
}

fn main() {
    let args = Args::parse();
    let device = if tch::Cuda::is_available() {
        Device::Cuda(0)
    } else {
        eprintln!("WARNING: CUDA not available, using CPU");
        Device::Cpu
    };

    std::fs::create_dir_all(&args.frames_out).ok();

    // Clean up partial writes
    for entry in std::fs::read_dir(&args.frames_out).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains(".tmp") {
            std::fs::remove_file(entry.path()).ok();
        }
    }

    // Gather and filter frames
    let all_inputs = gather_frames(&args.frames_in);
    if all_inputs.is_empty() {
        eprintln!("No PNG frames found in {}", args.frames_in.display());
        std::process::exit(1);
    }

    let inputs = if args.end > 0 {
        let end = args.end.min(all_inputs.len());
        eprintln!("Segment: frames {}-{} of {} ({} frames)",
            args.start, end, all_inputs.len(), end - args.start);
        all_inputs[args.start..end].to_vec()
    } else {
        all_inputs
    };

    // Skip already-done frames
    let done: HashSet<String> = std::fs::read_dir(&args.frames_out)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();

    let todo: Vec<(PathBuf, PathBuf)> = inputs
        .iter()
        .filter_map(|in_path| {
            let name = in_path.file_name()?.to_string_lossy().to_string();
            if done.contains(&name) {
                None
            } else {
                let out_path = args.frames_out.join(&name);
                Some((in_path.clone(), out_path))
            }
        })
        .collect();

    let total = inputs.len();
    let already_done = total - todo.len();
    if todo.is_empty() {
        eprintln!("All {} frames already upscaled.", total);
        return;
    }
    eprintln!("{}/{} already done, {} remaining", already_done, total, todo.len());

    // Auto tile
    let tile_size = if args.tile > 0 { args.tile } else { auto_tile(args.scale) };

    // Load model
    eprintln!("Loading model...");
    let model_path = std::env::var("REALESRGAN_MODEL")
        .unwrap_or_else(|_| {
            // Search common paths
            let paths = [
                "/root/RealESRGAN_x4plus.pt",
                "./RealESRGAN_x4plus.pt",
            ];
            for p in &paths {
                if Path::new(p).exists() {
                    return p.to_string();
                }
            }
            "RealESRGAN_x4plus.pt".to_string()
        });

    let model = CModule::load_on_device(&model_path, device)
        .expect(&format!("Failed to load model from {}", model_path));
    eprintln!("Model loaded. tile={}, device={:?}", tile_size, device);

    // Pipeline: reader → GPU → writer
    let tile_pad = 10i64;
    let scale = args.scale;
    let todo_len = todo.len();
    let start_time = Instant::now();

    // Channel: reader → GPU
    let (read_tx, read_rx) = mpsc::sync_channel::<(usize, PathBuf, Tensor)>(2);
    // Channel: GPU → writer
    let (write_tx, write_rx) = mpsc::sync_channel::<(usize, PathBuf, Tensor)>(2);

    // Reader thread
    let reader = thread::spawn(move || {
        for (i, (in_path, out_path)) in todo.into_iter().enumerate() {
            match read_frame(&in_path, device) {
                Some(tensor) => {
                    if read_tx.send((i, out_path, tensor)).is_err() {
                        break;
                    }
                }
                None => {
                    eprintln!("  WARNING: Could not read {}", in_path.display());
                }
            }
        }
    });

    // GPU thread
    let gpu_thread = thread::spawn(move || {
        for (i, out_path, input_tensor) in read_rx {
            let half_input = input_tensor.to_kind(Kind::Half);
            let output = upscale_image(&model, &half_input, scale, tile_size, tile_pad);
            let output_float = output.to_kind(Kind::Float);
            if write_tx.send((i, out_path, output_float)).is_err() {
                break;
            }
        }
    });

    // Writer thread (main thread)
    let mut processed = 0usize;
    for (i, out_path, output_tensor) in write_rx {
        write_frame(&output_tensor, &out_path);
        processed += 1;

        if processed % 10 == 0 || processed == todo_len {
            let elapsed = start_time.elapsed().as_secs_f64();
            let fps = processed as f64 / elapsed;
            let remaining = (todo_len - processed) as f64 / fps;
            eprintln!("  {}/{} ({:.1} fps, ~{:.0}m remaining)",
                already_done + processed, total, fps, remaining / 60.0);
        }
    }

    reader.join().ok();
    gpu_thread.join().ok();

    let elapsed = start_time.elapsed().as_secs_f64();
    eprintln!("Upscaling complete in {:.1}h ({:.0}s)", elapsed / 3600.0, elapsed);
}
