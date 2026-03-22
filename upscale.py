#!/usr/bin/env python3
"""Minimal Real-ESRGAN upscaler. Threaded I/O pipeline for performance."""

import argparse, glob, os, queue, sys, threading, time
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
    args = p.parse_args()

    os.makedirs(args.frames_out, exist_ok=True)

    # Gather and sort input frames naturally
    inputs = sorted(glob.glob(os.path.join(args.frames_in, "*.png")))
    if not inputs:
        sys.exit(f"No PNG frames found in {args.frames_in}")
    total = len(inputs)

    # Skip already-done frames (resume support)
    todo = [(f, os.path.join(args.frames_out, os.path.basename(f)))
            for f in inputs if not os.path.exists(os.path.join(args.frames_out, os.path.basename(f)))]
    done = total - len(todo)
    if not todo:
        print(f"All {total} frames already upscaled."); return
    print(f"{done}/{total} already done, {len(todo)} remaining")

    # Auto-detect tile size
    tile = args.tile
    if tile == 0 and torch.cuda.is_available():
        vram_gb = torch.cuda.get_device_properties(0).total_memory / 1024**3
        first = cv2.imread(inputs[0], cv2.IMREAD_UNCHANGED)
        mp = (first.shape[1] * first.shape[0]) / 1e6
        if vram_gb <= 24:
            tile = 512
        elif vram_gb <= 32:
            tile = 512 if mp > 1.6 else 0
        else:
            tile = 512 if mp > 2.0 else 0
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

    # I/O pipeline: threaded pre-read + async write
    cpus = os.cpu_count() or 1
    n_read = min(max(cpus // 4, 2), 8)
    n_write = min(max(cpus // 4, 2), 8)
    prefetch = n_read * 4
    read_q = queue.Queue(maxsize=prefetch)
    write_q = queue.Queue(maxsize=prefetch)

    def reader(chunk):
        for seq, (in_path, out_path) in chunk:
            img = cv2.imread(in_path, cv2.IMREAD_UNCHANGED)
            read_q.put((seq, in_path, img, out_path))

    def writer():
        while True:
            item = write_q.get()
            if item is None: break
            out_path, output, in_path = item
            cv2.imwrite(out_path, output)
            try: os.remove(in_path)
            except OSError: pass
            write_q.task_done()

    indexed = list(enumerate(todo))
    chunk_sz = max(1, (len(todo) + n_read - 1) // n_read)
    chunks = [indexed[k:k+chunk_sz] for k in range(0, len(indexed), chunk_sz)]

    for chunk in chunks:
        threading.Thread(target=reader, args=(chunk,), daemon=True).start()
    for _ in range(n_write):
        threading.Thread(target=writer, daemon=True).start()

    # GPU loop with reordering
    start = time.time()
    processed, next_seq, pending = 0, 0, {}
    while processed < len(todo):
        while next_seq not in pending:
            seq, in_path, img, out_path = read_q.get()
            pending[seq] = (in_path, img, out_path)
        in_path, img, out_path = pending.pop(next_seq)
        next_seq += 1
        output, _ = upsampler.enhance(img, outscale=args.scale)
        write_q.put((out_path, output, in_path))
        processed += 1
        if processed % 10 == 0:
            elapsed = time.time() - start
            fps = processed / elapsed
            remain = (len(todo) - processed) / fps if fps > 0 else 0
            print(f"  {done + processed}/{total} ({fps:.1f} fps, ~{remain/60:.0f}m remaining)")
            sys.stdout.flush()

    write_q.join()
    for _ in range(n_write): write_q.put(None)
    elapsed = time.time() - start
    print(f"Upscaling complete in {elapsed/3600:.1f}h ({elapsed:.0f}s)")

if __name__ == "__main__":
    main()
