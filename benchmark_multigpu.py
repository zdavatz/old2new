#!/usr/bin/env python3
"""
Multi-GPU Real-ESRGAN benchmark.
Tests 1, 2, 4, 8 GPUs in parallel using multiprocessing.
"""

import torch
import time
import cv2
import numpy as np
import subprocess
import sys
import multiprocessing
multiprocessing.set_start_method('spawn', force=True)
from multiprocessing import Process, Queue


def benchmark_single_gpu(gpu_id, result_queue, rounds=5):
    """Run benchmark on a single GPU, report fps."""
    try:
        torch.cuda.set_device(gpu_id)
        from realesrgan import RealESRGANer
        from basicsr.archs.rrdbnet_arch import RRDBNet

        img = np.random.randint(0, 255, (1200, 1920, 3), dtype=np.uint8)
        model = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
        up = RealESRGANer(
            scale=4,
            model_path="https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth",
            model=model, tile=512, tile_pad=10, pre_pad=0, half=True, gpu_id=gpu_id
        )

        # Warmup
        up.enhance(img, outscale=2)
        torch.cuda.synchronize(gpu_id)

        # Benchmark
        times = []
        for i in range(rounds):
            torch.cuda.synchronize(gpu_id)
            t0 = time.time()
            up.enhance(img, outscale=2)
            torch.cuda.synchronize(gpu_id)
            times.append(time.time() - t0)

        avg = sum(times) / len(times)
        fps = 1.0 / avg
        result_queue.put((gpu_id, avg, fps))
    except Exception as e:
        result_queue.put((gpu_id, -1, str(e)))


def run_parallel(num_gpus, rounds=5):
    """Run benchmark on N GPUs in parallel, return combined fps."""
    result_queue = Queue()
    processes = []

    for gpu_id in range(num_gpus):
        p = Process(target=benchmark_single_gpu, args=(gpu_id, result_queue, rounds))
        processes.append(p)

    # Start all at once
    t_start = time.time()
    for p in processes:
        p.start()
    for p in processes:
        p.join()
    t_total = time.time() - t_start

    # Collect results
    results = []
    while not result_queue.empty():
        results.append(result_queue.get())
    results.sort(key=lambda x: x[0])

    total_fps = 0
    for gpu_id, avg, fps in results:
        if isinstance(fps, float):
            total_fps += fps
            print(f"  GPU {gpu_id}: {avg:.2f}s/frame = {fps:.2f} fps")
        else:
            print(f"  GPU {gpu_id}: ERROR — {fps}")

    return total_fps


def main():
    num_gpus = torch.cuda.device_count()
    print(f"PyTorch: {torch.__version__}, CUDA: {torch.version.cuda}")
    print(f"GPUs detected: {num_gpus}")
    print()

    for i in range(num_gpus):
        name = torch.cuda.get_device_name(i)
        mem = torch.cuda.get_device_properties(i).total_memory / 1024**3
        print(f"  GPU {i}: {name} ({mem:.0f} GB)")
    print()

    subprocess.run(["nvidia-smi", "--query-gpu=name,power.limit,clocks.max.graphics",
                     "--format=csv,noheader"])
    print()

    print("=" * 70)
    print("BENCHMARK: 1920x1200 (2.3 MP), 2x scale, tile=512, FP16")
    print("=" * 70)

    results = {}
    for n in [1, 2, 4, 8]:
        if n > num_gpus:
            break
        print(f"\n--- {n}x GPU parallel ---")
        total_fps = run_parallel(n)
        results[n] = total_fps
        print(f"  TOTAL: {total_fps:.2f} fps ({n} GPUs)")

    print()
    print("=" * 70)
    print("SUMMARY")
    print("=" * 70)
    baseline = results.get(1, 1)
    for n, fps in results.items():
        speedup = fps / baseline if baseline > 0 else 0
        print(f"  {n}x GPU: {fps:.2f} fps ({speedup:.1f}x speedup)")
    print()
    print("DONE")


if __name__ == "__main__":
    main()
