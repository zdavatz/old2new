#!/usr/bin/env python3
"""
Cloud GPU video enhancement using Real-ESRGAN + GFPGAN with PyTorch/CUDA.

Usage on a cloud GPU instance (vast.ai, RunPod, etc.):
    pip install realesrgan gfpgan yt-dlp "numpy<2"
    apt-get install -y ffmpeg
    python3 enhance_gpu.py <youtube-url> [scale]

Scale: 2 or 4 (default: 4)
Uses GFPGAN for face enhancement when faces are detected.
"""

import os
import sys
import time
import subprocess
import glob

def main():
    from basicsr.archs.rrdbnet_arch import RRDBNet
    from realesrgan import RealESRGANer
    import cv2
    import torch

    # --- Args ---
    if len(sys.argv) < 2:
        print("Usage: python3 enhance_gpu.py <youtube-url> [scale] [--job-name NAME]")
        print("  scale: 2 or 4 (default: 4)")
        print("  --job-name: custom directory name under ~/jobs/ (default: video ID)")
        sys.exit(1)

    URL = sys.argv[1]
    SCALE = int(sys.argv[2]) if len(sys.argv) > 2 and not sys.argv[2].startswith("-") else 4
    JOB_NAME = None
    for i, arg in enumerate(sys.argv):
        if arg == "--job-name" and i + 1 < len(sys.argv):
            JOB_NAME = sys.argv[i + 1]

    # --- Extract video ID ---
    import re
    match = re.search(r'[?&]v=([^&]+)', URL)
    if not match:
        match = re.search(r'/([^/?]+)$', URL)
    if not match:
        print("Error: Could not extract video ID")
        sys.exit(1)
    VIDEO_ID = match.group(1)

    dir_name = JOB_NAME if JOB_NAME else VIDEO_ID
    WORKDIR = os.path.join(os.path.expanduser("~"), "jobs", dir_name)
    FRAMES_IN = f"{WORKDIR}/frames_in"
    FRAMES_OUT = f"{WORKDIR}/frames_out"
    INPUT = f"{WORKDIR}/{dir_name}.mkv"
    # Backwards compat: use original.mkv if it exists
    if not os.path.exists(INPUT) and os.path.exists(f"{WORKDIR}/original.mkv"):
        INPUT = f"{WORKDIR}/original.mkv"

    os.makedirs(FRAMES_IN, exist_ok=True)
    os.makedirs(FRAMES_OUT, exist_ok=True)

    # --- GPU info ---
    if torch.cuda.is_available():
        print(f"GPU: {torch.cuda.get_device_name(0)}")
        print(f"VRAM: {torch.cuda.get_device_properties(0).total_memory / 1024**3:.0f} GB")
    else:
        print("WARNING: CUDA not available, running on CPU (very slow)")

    print(f"Scale: {SCALE}x")
    print()

    # --- Speed test ---
    print("Testing download speed...")
    try:
        import urllib.request
        test_url = "https://speed.cloudflare.com/__down?bytes=10000000"  # 10MB
        t0 = time.time()
        urllib.request.urlretrieve(test_url, "/dev/null")
        t1 = time.time()
        speed_mbps = 10 * 8 / (t1 - t0)  # 10MB in megabits
        print(f"Download speed: {speed_mbps:.0f} Mbps")
        if speed_mbps < 50:
            print(f"WARNING: Download speed is very slow ({speed_mbps:.0f} Mbps)!")
            print("Consider using a different host for faster downloads.")
    except Exception as e:
        print(f"Speed test failed: {e}")
    print()

    # --- Download video ---
    if os.path.exists(INPUT):
        print("Video already downloaded.")
    else:
        print("Downloading video...")
        dl_start = time.time()
        subprocess.run(["yt-dlp", "-o", f"{WORKDIR}/{dir_name}.%(ext)s",
                        "--merge-output-format", "mkv", URL], check=True)
        if not os.path.exists(INPUT):
            # yt-dlp may produce different filename, find it
            files = [f for f in glob.glob(f"{WORKDIR}/*.mkv") if "enhanced" not in f]
            if not files:
                files = [f for f in glob.glob(f"{WORKDIR}/*.*") if "enhanced" not in os.path.basename(f) and not os.path.isdir(f)]
            if files:
                os.rename(files[0], INPUT)
            else:
                print("Error: Download failed")
                sys.exit(1)
        dl_elapsed = time.time() - dl_start
        dl_size = os.path.getsize(INPUT) / (1024 * 1024)
        print(f"Downloaded {dl_size:.0f} MB in {dl_elapsed:.0f}s ({dl_size/dl_elapsed*8:.0f} Mbps)")

    # --- Check disk space ---
    result = subprocess.run(["ffprobe", "-v", "quiet", "-select_streams", "v:0",
        "-show_entries", "stream=width,height,r_frame_rate",
        "-show_entries", "format=duration",
        "-of", "default=noprint_wrappers=1", INPUT], capture_output=True, text=True)
    info = dict(line.split("=") for line in result.stdout.strip().split("\n") if "=" in line)
    src_w = int(info.get("width", 0))
    src_h = int(info.get("height", 0))
    duration = float(info.get("duration", 0))
    fps_parts = info.get("r_frame_rate", "25/1").split("/")
    fps_val = int(fps_parts[0]) / int(fps_parts[1]) if len(fps_parts) == 2 else float(fps_parts[0])
    total_frames = int(duration * fps_val)

    # Estimate: input frame ~1MB per 960x720, output frame ~10MB at 4x
    input_frame_size = (src_w * src_h * 3) / (1024 * 1024)  # uncompressed estimate
    output_frame_size = (src_w * SCALE * src_h * SCALE * 3) / (1024 * 1024)
    # PNG compression ~3-5x, use conservative 3x
    est_input_gb = (total_frames * input_frame_size / 3) / 1024
    est_output_gb = (total_frames * output_frame_size / 3) / 1024
    est_total_gb = est_input_gb + est_output_gb + 5  # +5GB for video, model, etc.

    statvfs = os.statvfs(WORKDIR)
    avail_gb = (statvfs.f_frsize * statvfs.f_bavail) / (1024**3)

    print(f"\nVideo: {src_w}x{src_h} @ {fps_val:.0f}fps, {duration:.0f}s ({total_frames} frames)")
    print(f"Estimated disk needed: {est_total_gb:.0f} GB (input: {est_input_gb:.0f} GB + output: {est_output_gb:.0f} GB)")
    print(f"Available disk space:  {avail_gb:.0f} GB")

    if est_total_gb > avail_gb:
        print(f"\nERROR: Not enough disk space! Need ~{est_total_gb:.0f} GB but only {avail_gb:.0f} GB available.")
        print(f"Resize disk to at least {int(est_total_gb * 1.2)} GB and retry.")
        sys.exit(1)

    print()

    # --- Extract frames ---
    existing = sorted(glob.glob(f"{FRAMES_IN}/frame_*.png"))
    if len(existing) > 0:
        print(f"Frames already extracted: {len(existing)}")
    else:
        print("Extracting frames...")
        subprocess.run(["ffmpeg", "-i", INPUT, "-qscale:v", "2",
                        f"{FRAMES_IN}/frame_%08d.png"], check=True, capture_output=True)
        existing = sorted(glob.glob(f"{FRAMES_IN}/frame_*.png"))
        print(f"Extracted {len(existing)} frames")

    TOTAL = len(existing)

    # --- Setup Real-ESRGAN + GFPGAN models ---
    # RealESRGAN_x4plus model is always 4x internally; outscale handles 2x by downsampling
    print(f"\nLoading Real-ESRGAN model (output scale={SCALE}x)...")
    model = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
    upsampler = RealESRGANer(
        scale=4,
        model_path="https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth",
        model=model,
        tile=0,
        tile_pad=10,
        pre_pad=0,
        half=True,
        gpu_id=0 if torch.cuda.is_available() else None
    )
    print("Real-ESRGAN loaded.")

    # GFPGAN disabled: it halluccinates facial features and changes how people look.
    # Not suitable for documentary footage where preserving real appearance matters.
    # Real-ESRGAN alone provides good upscaling without altering faces.
    face_enhancer = None

    # Track whether faces have been seen to avoid unnecessary detection
    faces_seen = [False]
    frames_since_face = [0]
    FACE_CHECK_INTERVAL = 50  # only check for faces every N frames if none seen recently

    def enhance_frame(img, frame_num=0):
        """Enhance a frame: uses GFPGAN if faces likely, otherwise Real-ESRGAN only."""
        if face_enhancer is not None:
            # If faces were seen recently, always try GFPGAN
            # Otherwise only check every FACE_CHECK_INTERVAL frames to avoid overhead
            should_check = faces_seen[0] or (frame_num % FACE_CHECK_INTERVAL == 0)
            if should_check:
                _, restored_faces, output = face_enhancer.enhance(
                    img, has_aligned=False, only_center_face=False, paste_back=True
                )
                if output is not None:
                    if restored_faces:
                        faces_seen[0] = True
                        frames_since_face[0] = 0
                    else:
                        frames_since_face[0] += 1
                        if frames_since_face[0] > 200:
                            faces_seen[0] = False
                    return output
        # Real-ESRGAN only (no face detection overhead)
        output, _ = upsampler.enhance(img, outscale=SCALE)
        return output

    # --- Benchmark first frame ---
    print("Benchmarking...")
    img = cv2.imread(existing[0], cv2.IMREAD_UNCHANGED)
    t0 = time.time()
    output = enhance_frame(img, frame_num=0)
    t1 = time.time()
    per_frame = t1 - t0
    total_est = per_frame * TOTAL
    print(f"Benchmark: {per_frame:.2f}s per frame")
    print(f"Estimated total: {total_est / 3600:.1f} hours for {TOTAL} frames")

    # Save benchmark frame
    out_path = f"{FRAMES_OUT}/frame_00000001.png"
    cv2.imwrite(out_path, output)

    # --- Process all frames ---
    done = len(glob.glob(f"{FRAMES_OUT}/frame_*.png"))
    print(f"\nUpscaling: {done}/{TOTAL} done, {TOTAL - done} remaining...")
    if face_enhancer:
        print("Face enhancement: ENABLED (GFPGAN)")
    else:
        print("Face enhancement: DISABLED")
    sys.stdout.flush()
    start = time.time()

    for i, fpath in enumerate(existing):
        fname = os.path.basename(fpath)
        out_path = f"{FRAMES_OUT}/{fname}"
        if os.path.exists(out_path):
            continue

        img = cv2.imread(fpath, cv2.IMREAD_UNCHANGED)
        output = enhance_frame(img, frame_num=i)
        cv2.imwrite(out_path, output)

        if (i + 1) % 100 == 0:
            elapsed = time.time() - start
            processed = i + 1 - done
            fps = processed / elapsed if elapsed > 0 else 0
            remaining = (TOTAL - i - 1) / fps if fps > 0 else 0
            print(f"  {i+1}/{TOTAL} ({fps:.1f} fps, ~{remaining/60:.0f}m remaining)")
            sys.stdout.flush()

    elapsed = time.time() - start
    print(f"\nUpscaling complete in {elapsed/3600:.1f}h ({elapsed:.0f}s)")

    # --- Get FPS ---
    result = subprocess.run(["ffprobe", "-v", "quiet", "-select_streams", "v:0",
        "-show_entries", "stream=r_frame_rate", "-of", "default=noprint_wrappers=1:nokey=1",
        INPUT], capture_output=True, text=True)
    fps_frac = result.stdout.strip()
    fps_parts = fps_frac.split("/")
    fps = int(fps_parts[0]) // int(fps_parts[1]) if len(fps_parts) == 2 else int(fps_parts[0])

    # --- Reassemble ---
    print(f"\nReassembling video at {fps}fps...")
    src_info = subprocess.run(["ffprobe", "-v", "quiet", "-select_streams", "a",
        "-show_entries", "stream=codec_type", "-of", "csv=p=0", INPUT],
        capture_output=True, text=True)

    output_file = f"{WORKDIR}/{dir_name}_{SCALE}x.mkv"
    if src_info.stdout.strip():
        subprocess.run(["ffmpeg", "-framerate", str(fps), "-i", f"{FRAMES_OUT}/frame_%08d.png",
            "-i", INPUT, "-map", "0:v", "-map", "1:a",
            "-c:v", "libx264", "-crf", "18", "-preset", "slow", "-pix_fmt", "yuv420p",
            "-c:a", "copy", "-y", output_file], check=True, capture_output=True)
    else:
        subprocess.run(["ffmpeg", "-framerate", str(fps), "-i", f"{FRAMES_OUT}/frame_%08d.png",
            "-c:v", "libx264", "-crf", "18", "-preset", "slow", "-pix_fmt", "yuv420p",
            "-y", output_file], check=True, capture_output=True)

    size = os.path.getsize(output_file) / (1024 * 1024)
    print(f"\nDone! Output: {output_file} ({size:.0f} MB)")


if __name__ == "__main__":
    main()
