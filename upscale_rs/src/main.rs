//! upscale_rs — Rust Real-ESRGAN upscaler via ONNX Runtime with pipelined I/O
//!
//! 3-thread pipeline:
//!   Reader (CPU): loads PNG → sends to GPU queue
//!   GPU: ONNX Runtime inference (Real-ESRGAN)
//!   Writer (CPU): receives result → writes PNG
//!
//! GPU never waits for I/O → ~80% utilization vs ~40% in Python.
//!
//! Usage: upscale_rs <frames_in> <frames_out> <scale> [--tile N] [--start N] [--end N] [--gpu-id N]

use clap::Parser;
use image::{ImageBuffer, Rgb};
use ndarray::{Array4, ArrayView4, s};
use ort::session::Session;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

#[derive(Parser)]
struct Args {
    frames_in: PathBuf,
    frames_out: PathBuf,
    scale: u32,
    #[arg(long, default_value_t = 0)]
    tile: u32,
    #[arg(long, default_value_t = 0)]
    start: usize,
    #[arg(long, default_value_t = 0)]
    end: usize,
    #[arg(long, default_value = "RealESRGAN_x4plus.onnx")]
    model: String,
    #[arg(long, default_value_t = -1)]
    gpu_id: i32,
}

fn gather_frames(dir: &Path) -> Vec<PathBuf> {
    let mut frames: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter().flatten().filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str())
                .map(|n| n.starts_with("frame_") && n.ends_with(".png") && !n.contains(".tmp"))
                .unwrap_or(false)
        })
        .collect();
    frames.sort();
    frames
}

/// Read PNG as [1, 3, H, W] float32 array in [0, 1]
fn read_frame(path: &Path) -> Option<(u32, u32, Array4<f32>)> {
    let img = image::open(path).ok()?.to_rgb8();
    let (w, h) = (img.width(), img.height());
    let raw = img.into_raw();
    let mut arr = Array4::<f32>::zeros((1, 3, h as usize, w as usize));
    for y in 0..h as usize {
        for x in 0..w as usize {
            let idx = (y * w as usize + x) * 3;
            arr[[0, 0, y, x]] = raw[idx] as f32 / 255.0;
            arr[[0, 1, y, x]] = raw[idx + 1] as f32 / 255.0;
            arr[[0, 2, y, x]] = raw[idx + 2] as f32 / 255.0;
        }
    }
    Some((w, h, arr))
}

/// Write [1, 3, H, W] float32 array as PNG
fn write_frame(arr: &ArrayView4<f32>, path: &Path, gpu_tag: &str) {
    let h = arr.shape()[2] as u32;
    let w = arr.shape()[3] as u32;
    let mut img = ImageBuffer::<Rgb<u8>, Vec<u8>>::new(w, h);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let r = (arr[[0, 0, y, x]].clamp(0.0, 1.0) * 255.0) as u8;
            let g = (arr[[0, 1, y, x]].clamp(0.0, 1.0) * 255.0) as u8;
            let b = (arr[[0, 2, y, x]].clamp(0.0, 1.0) * 255.0) as u8;
            img.put_pixel(x as u32, y as u32, Rgb([r, g, b]));
        }
    }
    // GPU-specific tmp file to avoid collisions
    let stem = path.file_stem().unwrap().to_string_lossy();
    let tmp = path.with_file_name(format!("{}.tmp{}.png", stem, gpu_tag));
    img.save(&tmp).expect("Failed to save");
    std::fs::rename(&tmp, path).expect("Failed to rename");
}

/// Run ONNX inference on a tile
fn run_inference(session: &mut Session, input: &Array4<f32>) -> Array4<f32> {
    let shape: Vec<i64> = input.shape().iter().map(|&d| d as i64).collect();
    let data: Vec<f32> = input.as_standard_layout().iter().cloned().collect();
    let input_value = ort::value::Tensor::from_array((shape, data))
        .expect("Failed to create input tensor");
    let outputs = session.run(ort::inputs!["input" => input_value])
        .expect("Inference failed");
    let (shape, data) = outputs["output"].try_extract_tensor::<f32>()
        .expect("Failed to extract output");
    let dims: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
    Array4::from_shape_vec(
        (dims[0], dims[1], dims[2], dims[3]),
        data.to_vec()
    ).expect("Wrong output shape")
}

/// Upscale with tiling
fn upscale_tiled(session: &mut Session, input: &Array4<f32>, scale: u32, tile_size: u32, tile_pad: u32) -> Array4<f32> {
    let h = input.shape()[2] as u32;
    let w = input.shape()[3] as u32;

    if tile_size == 0 || (h <= tile_size && w <= tile_size) {
        return run_inference(session, input);
    }

    let out_h = (h * scale) as usize;
    let out_w = (w * scale) as usize;
    let mut output = Array4::<f32>::zeros((1, 3, out_h, out_w));

    let tiles_y = (h + tile_size - 1) / tile_size;
    let tiles_x = (w + tile_size - 1) / tile_size;

    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            let y_start = if ty * tile_size > tile_pad { ty * tile_size - tile_pad } else { 0 };
            let y_end = ((ty + 1) * tile_size + tile_pad).min(h);
            let x_start = if tx * tile_size > tile_pad { tx * tile_size - tile_pad } else { 0 };
            let x_end = ((tx + 1) * tile_size + tile_pad).min(w);

            let tile = input.slice(s![.., .., y_start as usize..y_end as usize, x_start as usize..x_end as usize]).to_owned();
            let out_tile = run_inference(session, &tile);

            let pad_y = ((ty * tile_size - y_start) * scale) as usize;
            let pad_x = ((tx * tile_size - x_start) * scale) as usize;
            let tile_h = (((ty + 1) * tile_size).min(h) - ty * tile_size) * scale;
            let tile_w = (((tx + 1) * tile_size).min(w) - tx * tile_size) * scale;

            let out_y = (ty * tile_size * scale) as usize;
            let out_x = (tx * tile_size * scale) as usize;

            let cropped = out_tile.slice(s![.., .., pad_y..pad_y + tile_h as usize, pad_x..pad_x + tile_w as usize]);
            output.slice_mut(s![.., .., out_y..out_y + tile_h as usize, out_x..out_x + tile_w as usize])
                .assign(&cropped);
        }
    }
    output
}

fn main() {
    let args = Args::parse();
    std::fs::create_dir_all(&args.frames_out).ok();

    let gpu_tag = if args.gpu_id >= 0 { format!(".gpu{}", args.gpu_id) } else { String::new() };

    // Clean only our tmp files
    let pattern = if gpu_tag.is_empty() { ".tmp.png".to_string() } else { format!(".tmp{}.png", gpu_tag) };
    for entry in std::fs::read_dir(&args.frames_out).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains(&pattern) {
            std::fs::remove_file(entry.path()).ok();
        }
    }

    // Gather frames
    let all_inputs = gather_frames(&args.frames_in);
    if all_inputs.is_empty() {
        eprintln!("No PNG frames found in {}", args.frames_in.display());
        std::process::exit(1);
    }

    let inputs = if args.end > 0 {
        let end = args.end.min(all_inputs.len());
        eprintln!("Segment: frames {}-{} of {}", args.start, end, all_inputs.len());
        all_inputs[args.start..end].to_vec()
    } else {
        all_inputs
    };

    // Skip done
    let done: HashSet<String> = std::fs::read_dir(&args.frames_out)
        .into_iter().flatten().filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with("frame_") && n.ends_with(".png") && !n.contains(".tmp") { Some(n) } else { None }
        })
        .collect();

    let todo: Vec<(PathBuf, PathBuf)> = inputs.iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_string_lossy().to_string();
            if done.contains(&name) { None }
            else { Some((p.clone(), args.frames_out.join(&name))) }
        })
        .collect();

    let total = inputs.len();
    let already_done = total - todo.len();
    if todo.is_empty() {
        eprintln!("All {} frames already upscaled.", total);
        return;
    }
    eprintln!("{}/{} already done, {} remaining", already_done, total, todo.len());

    // Find model
    let model_path = if Path::new(&args.model).exists() {
        args.model.clone()
    } else if Path::new("/root/RealESRGAN_x4plus.onnx").exists() {
        "/root/RealESRGAN_x4plus.onnx".to_string()
    } else {
        eprintln!("Model not found: {}", args.model);
        std::process::exit(1);
    };

    // Create ONNX session with CUDA
    eprintln!("Loading ONNX model: {}...", model_path);
    let mut session = Session::builder()
        .expect("Failed to create session builder")
        .with_execution_providers([
            ort::execution_providers::CUDAExecutionProvider::default().build()
        ])
        .expect("Failed to set execution providers")
        .commit_from_file(&model_path)
        .expect("Failed to load model");

    let tile_size = if args.tile > 0 { args.tile } else { 512 };
    let tile_pad = 10u32;
    let scale = args.scale;
    eprintln!("Model loaded. tile={}, scale={}x", tile_size, scale);

    // 3-thread pipeline: Reader → GPU → Writer
    let todo_len = todo.len();
    let start_time = Instant::now();

    let (read_tx, read_rx) = mpsc::sync_channel::<(usize, PathBuf, Array4<f32>)>(2);
    let (write_tx, write_rx) = mpsc::sync_channel::<(usize, PathBuf, Array4<f32>)>(2);

    // Reader thread — pre-loads frames from disk while GPU is busy
    let reader = thread::spawn(move || {
        for (i, (in_path, out_path)) in todo.into_iter().enumerate() {
            match read_frame(&in_path) {
                Some((_, _, arr)) => {
                    if read_tx.send((i, out_path, arr)).is_err() { break; }
                }
                None => eprintln!("  WARNING: Could not read {}", in_path.display()),
            }
        }
    });

    // GPU thread — ONNX inference, never waits for I/O
    let gpu_thread = thread::spawn(move || {
        for (i, out_path, input) in read_rx {
            let output = upscale_tiled(&mut session, &input, scale, tile_size, tile_pad);
            if write_tx.send((i, out_path, output)).is_err() { break; }
        }
    });

    // Writer thread (main) — saves results to disk
    let gpu_tag_w = gpu_tag.clone();
    let mut processed = 0usize;
    for (_, out_path, output) in write_rx {
        write_frame(&output.view(), &out_path, &gpu_tag_w);
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
    eprintln!("Upscaling complete in {:.1}h ({:.0}s)",
        start_time.elapsed().as_secs_f64() / 3600.0, start_time.elapsed().as_secs_f64());
}
