#!/usr/bin/env python3
"""Minimal Real-ESRGAN upscaler. Simple sequential loop — no threads, no deadlocks."""

import argparse, glob, os, sys, time
import cv2
import torch
from basicsr.archs.rrdbnet_arch import RRDBNet
from realesrgan import RealESRGANer

def main():
    p = argparse.ArgumentParser(description="Upscale frames with Real-ESRGAN")
    p.add_argument("frames_in", help="Directory of input PNG frames")
    p.add_argument("frames_out", help="Directory for upscaled output frames")
    p.add_argument("scale", type=int, help="Output scale (2 or 4)")
    p.add_argument("--tile", type=int, default=0, help="Tile size (0 = auto based on VRAM)")
    p.add_argument("--start", type=int, default=0, help="Start frame index (0-based, for segment splitting)")
    p.add_argument("--end", type=int, default=0, help="End frame index (exclusive, 0 = all)")
    p.add_argument("--gpu-id", type=int, default=-1, help="GPU ID for unique tmp files (avoids collisions)")
    args = p.parse_args()

    # GPU-specific tmp suffix to avoid collisions between parallel processes
    gpu_tag = f".gpu{args.gpu_id}" if args.gpu_id >= 0 else ""

    os.makedirs(args.frames_out, exist_ok=True)

    # Clean up only OUR partial writes from interrupted runs (not other GPUs')
    pattern = f"*.tmp{gpu_tag}.png" if gpu_tag else "*.tmp.png"
    for tmp in glob.glob(os.path.join(args.frames_out, pattern)):
        os.remove(tmp)

    # Gather and sort input frames
    all_inputs = sorted(glob.glob(os.path.join(args.frames_in, "frame_*.png")))
    if not all_inputs:
        sys.exit(f"No PNG frames found in {args.frames_in}")

    # Segment splitting: only process frames in [start, end) range
    if args.end > 0:
        inputs = all_inputs[args.start:args.end]
        print(f"Segment: frames {args.start}-{args.end} of {len(all_inputs)} ({len(inputs)} frames)")
    else:
        inputs = all_inputs
    total = len(inputs)

    # Skip already-done frames (resume support)
    todo = []
    for f in inputs:
        out_path = os.path.join(args.frames_out, os.path.basename(f))
        if not os.path.exists(out_path):
            todo.append((f, out_path))
    done = total - len(todo)
    if not todo:
        print(f"All {total} frames already upscaled.")
        return
    print(f"{done}/{total} already done, {len(todo)} remaining")

    # Auto-detect tile size
    # Benchmarked on RTX 5090: tile=512 fastest (0.4fps), tile=768 (0.3fps), tile=1024 (0.2fps)
    # Larger tiles use more VRAM but are slower due to 4x internal processing overhead
    tile = args.tile
    if tile == 0 and torch.cuda.is_available():
        vram_gb = torch.cuda.get_device_properties(0).total_memory / 1024**3
        first = cv2.imread(inputs[0], cv2.IMREAD_UNCHANGED)
        mp = (first.shape[1] * first.shape[0]) / 1e6
        if mp > 1.6:
            tile = 512
        else:
            tile = 0
        print(f"VRAM: {vram_gb:.0f}GB, resolution: {first.shape[1]}x{first.shape[0]} ({mp:.1f} MP), tile={tile or 'none'}")

    # Load model
    model = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
    upsampler = RealESRGANer(
        scale=4,
        model_path="https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth",
        model=model, tile=tile, tile_pad=10, pre_pad=0, half=True,
        gpu_id=0 if torch.cuda.is_available() else None)
    import logging
    logging.getLogger('basicsr').setLevel(logging.WARNING)
    logging.getLogger('realesrgan').setLevel(logging.WARNING)

    # Simple sequential loop — read, upscale, write, delete input
    start = time.time()
    for i, (in_path, out_path) in enumerate(todo):
        img = cv2.imread(in_path, cv2.IMREAD_UNCHANGED)
        if img is None:
            print(f"  WARNING: Could not read {in_path}, skipping")
            continue
        output, _ = upsampler.enhance(img, outscale=args.scale)
        # Atomic write: GPU-specific tmp then rename (no collisions between GPUs)
        tmp_path = out_path.rsplit(".", 1)[0] + f".tmp{gpu_tag}.png"
        cv2.imwrite(tmp_path, output)
        os.rename(tmp_path, out_path)
        # Delete input frame to free disk, but keep last 10 for compare view
        # Skip deletion in segment mode — other GPUs may need the same frames_in dir
        if args.end == 0 and i >= 10:
            old_in_path = todo[i - 10][0]
            try:
                os.remove(old_in_path)
            except OSError:
                pass

        processed = i + 1
        if processed % 10 == 0 or processed == len(todo):
            elapsed = time.time() - start
            fps = processed / elapsed
            remain = (len(todo) - processed) / fps if fps > 0 else 0
            print(f"  {done + processed}/{total} ({fps:.1f} fps, ~{remain/60:.0f}m remaining)")
            sys.stdout.flush()

    elapsed = time.time() - start
    print(f"Upscaling complete in {elapsed/3600:.1f}h ({elapsed:.0f}s)")

if __name__ == "__main__":
    main()
