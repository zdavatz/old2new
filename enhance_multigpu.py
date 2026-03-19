#!/usr/bin/env python3
"""
Multi-GPU Real-ESRGAN: split one video's frames across N GPUs for parallel upscaling.

Usage:
    python3 enhance_multigpu.py "https://www.youtube.com/watch?v=VIDEO_ID" SCALE [--job-name NAME] [--gpus N]

Each GPU processes 1/N of the frames. 4x faster than single GPU on 4 GPUs.
"""

import os
import sys
import glob
import time
import subprocess
import multiprocessing

def gpu_worker(gpu_id, frames_in_dir, frames_out_dir, frame_list, scale):
    """Upscale a subset of frames on one GPU."""
    os.environ["CUDA_VISIBLE_DEVICES"] = str(gpu_id)

    import torch
    import cv2
    import numpy as np
    from realesrgan import RealESRGANer
    from basicsr.archs.rrdbnet_arch import RRDBNet

    torch.cuda.set_device(0)  # device 0 within CUDA_VISIBLE_DEVICES

    # Detect tiling
    gpu_mem_gb = torch.cuda.get_device_properties(0).total_memory / (1024**3)
    # Read first frame to get resolution
    sample = cv2.imread(frame_list[0])
    h, w = sample.shape[:2]
    mpixels = (w * h) / 1e6

    if gpu_mem_gb >= 28:
        safe_mp = 1.8
    elif gpu_mem_gb >= 20:
        safe_mp = 1.6
    else:
        safe_mp = 0.7

    tile = 512 if mpixels > safe_mp else 0

    print(f"[GPU {gpu_id}] {len(frame_list)} frames, {w}x{h} ({mpixels:.1f}MP), tile={tile}, VRAM={gpu_mem_gb:.0f}GB")
    sys.stdout.flush()

    model = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
    upsampler = RealESRGANer(
        scale=4,
        model_path="https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth",
        model=model, tile=tile, tile_pad=10, pre_pad=0, half=True, gpu_id=0
    )

    done = 0
    skipped = 0
    start = time.time()

    for fpath in frame_list:
        fname = os.path.basename(fpath)
        out_path = os.path.join(frames_out_dir, fname)

        if os.path.exists(out_path):
            skipped += 1
            done += 1
            continue

        img = cv2.imread(fpath, cv2.IMREAD_UNCHANGED)
        if img is None:
            continue

        output, _ = upsampler.enhance(img, outscale=scale)
        cv2.imwrite(out_path, output)
        done += 1

        if (done - skipped) % 10 == 0:
            elapsed = time.time() - start
            processed = done - skipped
            fps = processed / elapsed if elapsed > 0 else 0
            remaining = len(frame_list) - done
            eta_s = remaining / fps if fps > 0 else 0
            eta_m = int(eta_s / 60)
            print(f"[GPU {gpu_id}] {done}/{len(frame_list)} ({fps:.1f} fps, ~{eta_m}m remaining)")
            sys.stdout.flush()

    elapsed = time.time() - start
    processed = done - skipped
    fps = processed / elapsed if elapsed > 0 else 0
    print(f"[GPU {gpu_id}] DONE: {done}/{len(frame_list)} frames in {elapsed:.0f}s ({fps:.1f} fps), {skipped} skipped")
    sys.stdout.flush()


def main():
    multiprocessing.set_start_method('spawn', force=True)

    if len(sys.argv) < 3:
        print("Usage: python3 enhance_multigpu.py URL SCALE [--job-name NAME] [--gpus N]")
        sys.exit(1)

    url = sys.argv[1]
    scale = int(sys.argv[2])

    # Parse optional args
    job_name = None
    num_gpus = None
    i = 3
    while i < len(sys.argv):
        if sys.argv[i] == "--job-name" and i + 1 < len(sys.argv):
            job_name = sys.argv[i + 1]
            i += 2
        elif sys.argv[i] == "--gpus" and i + 1 < len(sys.argv):
            num_gpus = int(sys.argv[i + 1])
            i += 2
        else:
            i += 1

    import torch
    if num_gpus is None:
        num_gpus = torch.cuda.device_count()

    print(f"Multi-GPU Real-ESRGAN: {num_gpus} GPUs, scale={scale}x")
    for g in range(num_gpus):
        print(f"  GPU {g}: {torch.cuda.get_device_name(g)}")

    # Setup directories
    home = os.path.expanduser("~")
    if job_name:
        dir_name = job_name
    else:
        # Extract video ID from URL
        import re
        m = re.search(r'[?&]v=([^&]+)', url)
        dir_name = m.group(1) if m else "video"

    workdir = os.path.join(home, "jobs", dir_name)
    frames_in = os.path.join(workdir, "frames_in")
    frames_out = os.path.join(workdir, "frames_out")
    os.makedirs(frames_in, exist_ok=True)
    os.makedirs(frames_out, exist_ok=True)

    # Step 1: Download video
    input_file = os.path.join(workdir, f"{dir_name}.mkv")
    if not os.path.exists(input_file):
        print(f"\n=== Step 1: Download ===")
        subprocess.run([
            "yt-dlp", "-f", "bestvideo+bestaudio/best",
            "--merge-output-format", "mkv",
            "-o", input_file, url
        ], check=True)
    else:
        print(f"Video already downloaded: {input_file}")

    # Get video info
    r = subprocess.run(
        ["ffprobe", "-v", "quiet", "-select_streams", "v:0",
         "-show_entries", "stream=r_frame_rate,width,height",
         "-of", "csv=p=0", input_file],
        capture_output=True, text=True)
    parts = r.stdout.strip().split(",")
    src_w, src_h = int(parts[0]), int(parts[1])
    fps_str = parts[2]
    fps_num, fps_den = fps_str.split("/")
    fps = int(fps_num) / int(fps_den)

    r2 = subprocess.run(
        ["ffprobe", "-v", "quiet", "-count_frames", "-select_streams", "v:0",
         "-show_entries", "stream=nb_read_frames",
         "-of", "csv=p=0", input_file],
        capture_output=True, text=True, timeout=30)
    total_frames_est = int(r2.stdout.strip()) if r2.stdout.strip().isdigit() else int(fps * 3876)

    print(f"Video: {src_w}x{src_h} @ {fps:.1f}fps, ~{total_frames_est} frames")

    # Step 2: Extract frames
    existing = sorted(glob.glob(os.path.join(frames_in, "frame_*.png")))
    if len(existing) < total_frames_est * 0.9:
        print(f"\n=== Step 2: Extract frames ===")
        subprocess.run([
            "ffmpeg", "-i", input_file,
            "-qscale:v", "1", "-qmin", "1", "-qmax", "1", "-vsync", "0",
            os.path.join(frames_in, "frame_%08d.png")
        ], check=True)
        existing = sorted(glob.glob(os.path.join(frames_in, "frame_*.png")))
    print(f"Frames extracted: {len(existing)}")

    # Step 3: Split frames across GPUs
    print(f"\n=== Step 3: Upscale on {num_gpus} GPUs ===")
    chunks = [[] for _ in range(num_gpus)]
    for i, fpath in enumerate(existing):
        chunks[i % num_gpus].append(fpath)

    for g in range(num_gpus):
        print(f"  GPU {g}: {len(chunks[g])} frames")

    # Launch parallel workers
    t_start = time.time()
    processes = []
    for g in range(num_gpus):
        p = multiprocessing.Process(
            target=gpu_worker,
            args=(g, frames_in, frames_out, chunks[g], scale)
        )
        processes.append(p)
        p.start()

    for p in processes:
        p.join()

    t_elapsed = time.time() - t_start
    done_frames = len(glob.glob(os.path.join(frames_out, "frame_*.png")))
    combined_fps = done_frames / t_elapsed if t_elapsed > 0 else 0
    print(f"\nUpscaling complete: {done_frames}/{len(existing)} frames in {t_elapsed:.0f}s ({combined_fps:.1f} fps combined)")

    # Step 4: Reassemble
    output_file = os.path.join(workdir, f"{dir_name}_{scale}x.mkv")
    if not os.path.exists(output_file):
        print(f"\n=== Step 4: Reassemble ===")
        subprocess.run([
            "ffmpeg", "-framerate", fps_str,
            "-i", os.path.join(frames_out, "frame_%08d.png"),
            "-i", input_file,
            "-map", "0:v", "-map", "1:a?",
            "-c:v", "libx264", "-crf", "18", "-preset", "medium", "-pix_fmt", "yuv420p",
            "-c:a", "copy",
            output_file
        ], check=True)
        size_mb = os.path.getsize(output_file) / (1024 * 1024)
        print(f"Enhanced video: {output_file} ({size_mb:.0f} MB)")
    else:
        print(f"Enhanced video already exists: {output_file}")

    print(f"\n=== DONE ===")
    print(f"Total time: {t_elapsed:.0f}s ({t_elapsed/60:.1f} min)")
    print(f"Combined fps: {combined_fps:.1f} ({num_gpus} GPUs)")


if __name__ == "__main__":
    main()
