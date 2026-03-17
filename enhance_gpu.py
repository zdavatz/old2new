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


def preflight_check():
    """Comprehensive pre-flight check of hardware and software before starting.
    Checks GPU, CPU, RAM, disk, network, and all software dependencies.
    Returns dict of system info or exits with error."""
    import shutil
    print("=" * 60)
    print("PRE-FLIGHT CHECK")
    print("=" * 60)
    errors = []
    warnings = []
    info = {}

    # --- GPU & CUDA ---
    print("\n[GPU]")
    try:
        result = subprocess.run(
            ["nvidia-smi", "--query-gpu=name,memory.total,driver_version,compute_cap",
             "--format=csv,noheader,nounits"],
            capture_output=True, text=True, timeout=5)
        if result.returncode == 0:
            parts = [p.strip() for p in result.stdout.strip().split(",")]
            gpu_name = parts[0]
            gpu_vram_mb = int(parts[1])
            gpu_driver = parts[2]
            gpu_compute = parts[3]
            info["gpu"] = gpu_name
            info["vram_gb"] = gpu_vram_mb / 1024
            print(f"  GPU:      {gpu_name}")
            print(f"  VRAM:     {gpu_vram_mb / 1024:.0f} GB")
            print(f"  Driver:   {gpu_driver}")
            print(f"  Compute:  sm_{gpu_compute.replace('.', '')}")

            # Check if Blackwell (sm_12x) needs newer PyTorch
            major = int(gpu_compute.split(".")[0])
            if major >= 12:
                info["needs_cu128"] = True
                print(f"  WARNING:  Blackwell GPU detected — needs PyTorch 2.6+ with CUDA 12.8")
        else:
            errors.append("nvidia-smi failed — no GPU detected")
    except FileNotFoundError:
        errors.append("nvidia-smi not found — no NVIDIA GPU available")
    except Exception as e:
        errors.append(f"GPU check failed: {e}")

    # --- PyTorch & CUDA compatibility ---
    print("\n[PyTorch]")
    try:
        import torch
        pt_ver = torch.__version__
        cuda_ver = torch.version.cuda or "none"
        cuda_avail = torch.cuda.is_available()
        print(f"  PyTorch:  {pt_ver}")
        print(f"  CUDA:     {cuda_ver}")
        print(f"  GPU OK:   {cuda_avail}")
        info["pytorch"] = pt_ver
        info["cuda"] = cuda_ver

        if cuda_avail:
            cap = torch.cuda.get_device_capability()
            print(f"  Arch:     sm_{cap[0]}{cap[1]}")
            if cap[0] >= 12:
                # Verify PyTorch actually supports this arch
                try:
                    t = torch.zeros(1).cuda().half()
                    print(f"  FP16:     OK")
                except RuntimeError as e:
                    if "no kernel image" in str(e):
                        errors.append(f"PyTorch {pt_ver} does not support sm_{cap[0]}{cap[1]} (Blackwell). "
                                      f"Need: pip install torch torchvision --index-url https://download.pytorch.org/whl/cu128")
                    else:
                        errors.append(f"CUDA FP16 test failed: {e}")

            # Test actual CUDA convolution (catches driver/CUDA mismatches like CUBLAS_STATUS_NOT_SUPPORTED)
            try:
                conv = torch.nn.Conv2d(3, 16, 3, padding=1).cuda().half()
                test_input = torch.randn(1, 3, 64, 64).cuda().half()
                _ = conv(test_input)
                del conv, test_input
                torch.cuda.empty_cache()
                print(f"  Conv2d:   OK (FP16 compute works)")
            except RuntimeError as e:
                err_str = str(e)
                if "CUBLAS_STATUS_NOT_SUPPORTED" in err_str or "INTERNAL ASSERT" in err_str:
                    errors.append(f"CUDA compute test FAILED: driver ({info.get('gpu', {}).get('driver', '?')}) "
                                  f"incompatible with PyTorch {pt_ver}/CUDA {cuda_ver}. "
                                  f"Try: pip install --force-reinstall torch torchvision --index-url https://download.pytorch.org/whl/cu128")
                else:
                    errors.append(f"CUDA Conv2d test failed: {e}")
        else:
            errors.append("CUDA not available — torch.cuda.is_available() returned False")
    except ImportError:
        errors.append("PyTorch not installed — pip install torch")

    # --- CPU benchmark ---
    print("\n[CPU]")
    try:
        cpu_count = os.cpu_count() or 1
        with open("/proc/cpuinfo") as f:
            cpu_name = ""
            cpu_mhz = 0
            for line in f:
                if line.startswith("model name") and not cpu_name:
                    cpu_name = line.split(":", 1)[1].strip()
                if line.startswith("cpu MHz") and cpu_mhz == 0:
                    cpu_mhz = float(line.split(":", 1)[1].strip())
        print(f"  Model:    {cpu_name}")
        print(f"  Cores:    {cpu_count}")
        print(f"  MHz:      {cpu_mhz:.0f}")
        info["cpu"] = cpu_name
        info["cpu_cores"] = cpu_count
        info["cpu_mhz"] = cpu_mhz

        # Single-core benchmark: time a numpy operation as proxy
        import timeit
        bench_time = timeit.timeit("sum(range(1000000))", number=3) / 3
        score = 1.0 / bench_time  # higher is better
        print(f"  Bench:    {score:.1f} (single-core, higher=better)")
        info["cpu_score"] = score
        if score < 5.0:
            warnings.append(f"Slow CPU ({score:.1f} score, {cpu_mhz:.0f} MHz). "
                            f"Frame I/O will bottleneck GPU. Expect 2-4x slower than modern CPUs.")
        if cpu_mhz < 1500:
            warnings.append(f"CPU clock very low ({cpu_mhz:.0f} MHz). "
                            f"Consider a machine with faster per-core speed (>2GHz).")
    except Exception as e:
        warnings.append(f"CPU check failed: {e}")

    # --- RAM ---
    print("\n[RAM]")
    try:
        with open("/proc/meminfo") as f:
            mem_total = mem_avail = 0
            for line in f:
                parts = line.split()
                if parts[0] == "MemTotal:":
                    mem_total = int(parts[1]) // 1024  # MB
                elif parts[0] == "MemAvailable:":
                    mem_avail = int(parts[1]) // 1024  # MB
        print(f"  Total:    {mem_total / 1024:.1f} GB")
        print(f"  Avail:    {mem_avail / 1024:.1f} GB")
        info["ram_gb"] = mem_total / 1024
        if mem_avail < 4096:
            warnings.append(f"Low available RAM ({mem_avail / 1024:.1f} GB). May cause issues with large frames.")
    except Exception as e:
        warnings.append(f"RAM check failed: {e}")

    # --- Disk ---
    print("\n[Disk]")
    try:
        home = os.path.expanduser("~")
        st = os.statvfs(home)
        total_gb = (st.f_blocks * st.f_frsize) / (1024**3)
        free_gb = (st.f_bavail * st.f_frsize) / (1024**3)
        print(f"  Total:    {total_gb:.0f} GB")
        print(f"  Free:     {free_gb:.0f} GB")
        info["disk_free_gb"] = free_gb
        info["disk_total_gb"] = total_gb
    except Exception as e:
        warnings.append(f"Disk check failed: {e}")

    # --- Disk I/O benchmark ---
    try:
        test_file = os.path.expanduser("~/disk_bench_test")
        t0 = time.time()
        with open(test_file, "wb") as f:
            f.write(os.urandom(50 * 1024 * 1024))  # 50MB
        f.close()
        write_speed = 50 / (time.time() - t0)
        t0 = time.time()
        with open(test_file, "rb") as f:
            f.read()
        read_speed = 50 / (time.time() - t0)
        os.remove(test_file)
        print(f"  Write:    {write_speed:.0f} MB/s")
        print(f"  Read:     {read_speed:.0f} MB/s")
        info["disk_write_mbs"] = write_speed
        info["disk_read_mbs"] = read_speed
        if write_speed < 100:
            warnings.append(f"Slow disk write ({write_speed:.0f} MB/s). Frame extraction and output will be slow.")
    except Exception as e:
        warnings.append(f"Disk benchmark failed: {e}")

    # --- ffmpeg ---
    print("\n[Software]")
    try:
        result = subprocess.run(["ffmpeg", "-version"], capture_output=True, text=True, timeout=5)
        line = result.stdout.split("\n")[0]
        print(f"  ffmpeg:   {line}")
        # Check version
        import re
        m = re.search(r"version (\d+\.\d+)", line)
        if m:
            ver = float(m.group(1))
            if ver < 5.0:
                errors.append(f"ffmpeg {ver} too old — cannot merge webm streams. "
                              f"Fix: curl -sL https://johnvansickle.com/ffmpeg/releases/ffmpeg-release-amd64-static.tar.xz | "
                              f"tar xJ --strip-components=1 -C /opt/conda/bin/ --wildcards '*/ffmpeg' '*/ffprobe'")
    except FileNotFoundError:
        errors.append("ffmpeg not found — apt-get install -y ffmpeg")

    # --- Python packages ---
    pkg_issues = []
    try:
        import numpy
        nv = numpy.__version__
        print(f"  numpy:    {nv}")
        if nv.startswith("2."):
            pkg_issues.append(f"numpy {nv} breaks basicsr. Fix: pip install 'numpy==1.26.4'")
    except ImportError:
        pkg_issues.append("numpy not installed")

    try:
        import cv2
        print(f"  opencv:   {cv2.__version__}")
    except ImportError:
        pkg_issues.append("opencv not installed. Fix: pip install 'opencv-python-headless<4.11'")

    try:
        from basicsr.archs.rrdbnet_arch import RRDBNet
        print(f"  basicsr:  OK")
    except ImportError as e:
        if "functional_tensor" in str(e):
            pkg_issues.append("basicsr import fails (functional_tensor removed in new torchvision). "
                              "Fix: sed -i 's/from torchvision.transforms.functional_tensor import rgb_to_grayscale/"
                              "from torchvision.transforms.functional import rgb_to_grayscale/' "
                              "$(python3 -c 'import basicsr; print(basicsr.__path__[0])')/data/degradations.py")
        else:
            pkg_issues.append(f"basicsr import failed: {e}")

    try:
        from realesrgan import RealESRGANer
        print(f"  realesrgan: OK")
    except ImportError:
        pkg_issues.append("realesrgan not installed. Fix: pip install realesrgan")

    try:
        result = subprocess.run(["yt-dlp", "--version"], capture_output=True, text=True, timeout=5)
        print(f"  yt-dlp:   {result.stdout.strip()}")
    except FileNotFoundError:
        pkg_issues.append("yt-dlp not installed. Fix: pip install yt-dlp")

    if pkg_issues:
        for p in pkg_issues:
            errors.append(p)

    # --- PCIe bandwidth (from nvidia-smi) ---
    try:
        result = subprocess.run(
            ["nvidia-smi", "--query-gpu=pcie.link.gen.current,pcie.link.width.current",
             "--format=csv,noheader"],
            capture_output=True, text=True, timeout=5)
        if result.returncode == 0:
            parts = [p.strip() for p in result.stdout.strip().split(",")]
            print(f"\n[PCIe]")
            print(f"  Gen:      {parts[0]}")
            print(f"  Width:    x{parts[1]}")
            gen = int(parts[0])
            width = int(parts[1])
            # Approx bandwidth: Gen3=1GB/s/lane, Gen4=2GB/s/lane, Gen5=4GB/s/lane
            bw = {3: 1, 4: 2, 5: 4}.get(gen, 1) * width
            print(f"  ~BW:      {bw} GB/s")
            if gen < 4:
                warnings.append(f"PCIe Gen{gen} — slower GPU data transfer. Gen4+ recommended.")
    except Exception:
        pass

    # --- Summary ---
    print("\n" + "=" * 60)
    if errors:
        print(f"ERRORS ({len(errors)}):")
        for e in errors:
            print(f"  ✗ {e}")
    if warnings:
        print(f"WARNINGS ({len(warnings)}):")
        for w in warnings:
            print(f"  ! {w}")
    if not errors and not warnings:
        print("ALL CHECKS PASSED")
    elif not errors:
        print("CHECKS PASSED (with warnings)")
    print("=" * 60 + "\n")

    if errors:
        print("Fix the errors above before running enhancement.")
        sys.exit(1)

    return info


def main():
    # Run pre-flight check first
    sys_info = preflight_check()

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

    # --- Cookies for yt-dlp (auto-detect ~/cookies.txt) ---
    COOKIES_FILE = os.path.expanduser("~/cookies.txt")
    ytdlp_cookies = ["--cookies", COOKIES_FILE] if os.path.exists(COOKIES_FILE) else []
    if ytdlp_cookies:
        print(f"Using cookies: {COOKIES_FILE}")

    # --- Pre-download disk check ---
    # Fetch video metadata without downloading to estimate disk needs early
    if not os.path.exists(INPUT):
        print("Fetching video info...")
        try:
            result = subprocess.run(["yt-dlp", "--dump-json", "--no-download"] + ytdlp_cookies + [URL],
                                    capture_output=True, text=True, timeout=60)
            if result.returncode == 0:
                import json as _json
                vinfo = _json.loads(result.stdout)
                pre_w = vinfo.get("width", 0) or 0
                pre_h = vinfo.get("height", 0) or 0
                pre_dur = vinfo.get("duration", 0) or 0
                pre_fps = vinfo.get("fps", 25) or 25
                pre_filesize = vinfo.get("filesize_approx", 0) or vinfo.get("filesize", 0) or 0
                if pre_w and pre_h and pre_dur:
                    pre_frames = int(pre_dur * pre_fps)
                    pre_input_sz = (pre_w * pre_h * 3) / (1024 * 1024)  # uncompressed per frame
                    pre_output_sz = (pre_w * SCALE * pre_h * SCALE * 3) / (1024 * 1024)
                    # PNG compression ~2.5x with 10% safety margin
                    pre_est_gb = (pre_frames * pre_input_sz / 2.5 + pre_frames * pre_output_sz / 2.5) / 1024
                    pre_est_gb = pre_est_gb * 1.1 + 5  # 10% margin + 5GB overhead
                    statvfs = os.statvfs(WORKDIR)
                    pre_avail_gb = (statvfs.f_frsize * statvfs.f_bavail) / (1024**3)
                    print(f"  Video:    {pre_w}x{pre_h} @ {pre_fps}fps, {pre_dur:.0f}s ({pre_frames} frames)")
                    print(f"  Disk est: ~{pre_est_gb:.0f} GB needed, {pre_avail_gb:.0f} GB available")
                    if pre_est_gb > pre_avail_gb:
                        print(f"\n  ERROR: Not enough disk space!")
                        print(f"  Need ~{pre_est_gb:.0f} GB but only {pre_avail_gb:.0f} GB available.")
                        print(f"  Resize disk to at least {int(pre_est_gb * 1.2)} GB or use a larger instance.")
                        sys.exit(1)
                    else:
                        print(f"  Disk OK:  {pre_avail_gb - pre_est_gb:.0f} GB headroom")
                    print()
        except Exception as e:
            print(f"  Warning: Could not pre-check video info: {e}")
            print()

    # --- Download video ---
    if os.path.exists(INPUT):
        print("Video already downloaded.")
    else:
        print("Downloading video...")
        dl_start = time.time()
        subprocess.run(["yt-dlp", "-o", f"{WORKDIR}/{dir_name}.%(ext)s",
                        "--merge-output-format", "mkv"] + ytdlp_cookies + [URL], check=True)
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

    # Estimate disk needs — only count what still needs to be written
    input_frame_size = (src_w * src_h * 3) / (1024 * 1024)  # uncompressed estimate
    output_frame_size = (src_w * SCALE * src_h * SCALE * 3) / (1024 * 1024)
    # PNG compression ~2.5x with 10% safety margin
    existing_input = len(glob.glob(f"{FRAMES_IN}/frame_*.png"))
    existing_output = len(glob.glob(f"{FRAMES_OUT}/frame_*.png"))
    remaining_input = max(0, total_frames - existing_input)
    remaining_output = max(0, total_frames - existing_output)
    est_input_gb = (remaining_input * input_frame_size / 2.5) / 1024
    est_output_gb = (remaining_output * output_frame_size / 2.5) / 1024
    est_remaining_gb = (est_input_gb + est_output_gb) * 1.1 + 5  # 10% margin + 5GB overhead

    statvfs = os.statvfs(WORKDIR)
    avail_gb = (statvfs.f_frsize * statvfs.f_bavail) / (1024**3)

    print(f"\nVideo: {src_w}x{src_h} @ {fps_val:.0f}fps, {duration:.0f}s ({total_frames} frames)")
    print(f"Disk still needed: ~{est_remaining_gb:.0f} GB (input: {est_input_gb:.0f} GB + output: {est_output_gb:.0f} GB)")
    print(f"Available disk space: {avail_gb:.0f} GB")
    if existing_input > 0:
        print(f"Already extracted: {existing_input} frames")
    if existing_output > 0:
        print(f"Already upscaled: {existing_output} frames")

    if est_remaining_gb > avail_gb:
        print(f"\nERROR: Not enough disk space! Need ~{est_remaining_gb:.0f} GB but only {avail_gb:.0f} GB available.")
        print(f"Resize disk to at least {int(est_remaining_gb * 1.2)} GB and retry.")
        sys.exit(1)

    print()

    # --- Extract frames ---
    existing = sorted(glob.glob(f"{FRAMES_IN}/frame_*.png"))
    if len(existing) > 0:
        print(f"Frames already extracted: {len(existing)}")
    else:
        cpu_count = os.cpu_count() or 1
        num_workers = min(max(cpu_count // 2, 1), 16)  # Use half of CPUs, max 16 workers

        if num_workers > 1 and duration > 30:
            # Parallel extraction: split video into segments, extract in parallel
            print(f"Extracting ~{total_frames} frames using {num_workers} parallel workers...")
            sys.stdout.flush()
            segment_dur = duration / num_workers
            procs = []
            for i in range(num_workers):
                start_time = i * segment_dur
                start_frame = int(i * segment_dur * fps_val) + 1  # 1-indexed for ffmpeg output
                cmd = ["ffmpeg", "-ss", f"{start_time:.3f}", "-i", INPUT,
                       "-t", f"{segment_dur:.3f}", "-qscale:v", "2",
                       f"{FRAMES_IN}/frame_%08d_w{i:02d}.png"]
                p = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                procs.append((i, p, start_frame))

            extract_start = time.time()
            # Wait for all workers
            for i, p, _ in procs:
                p.wait()

            # Rename files to sequential frame_XXXXXXXX.png
            print("Renaming frames to sequential order...")
            sys.stdout.flush()
            all_frames = []
            for i in range(num_workers):
                worker_frames = sorted(glob.glob(f"{FRAMES_IN}/frame_*_w{i:02d}.png"))
                all_frames.extend(worker_frames)

            for idx, old_path in enumerate(all_frames, 1):
                new_path = os.path.join(FRAMES_IN, f"frame_{idx:08d}.png")
                os.rename(old_path, new_path)

            existing = sorted(glob.glob(f"{FRAMES_IN}/frame_*.png"))
            extract_elapsed = time.time() - extract_start
            print(f"Extracted {len(existing)} frames in {extract_elapsed:.0f}s ({num_workers} workers)")
        else:
            # Single-process extraction (short video or single CPU)
            print(f"Extracting ~{total_frames} frames...")
            sys.stdout.flush()
            proc = subprocess.Popen(["ffmpeg", "-i", INPUT, "-qscale:v", "2",
                            f"{FRAMES_IN}/frame_%08d.png"],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            extract_start = time.time()
            while proc.poll() is None:
                time.sleep(5)
                count = len(glob.glob(f"{FRAMES_IN}/frame_*.png"))
                if count > 0 and total_frames > 0:
                    pct = count * 100 // total_frames
                    elapsed = time.time() - extract_start
                    fps_extract = count / elapsed if elapsed > 0 else 0
                    remaining = (total_frames - count) / fps_extract if fps_extract > 0 else 0
                    print(f"  Extracting: {count}/{total_frames} ({pct}%) | {fps_extract:.0f} fps | ~{remaining/60:.0f}m remaining")
                    sys.stdout.flush()
            existing = sorted(glob.glob(f"{FRAMES_IN}/frame_*.png"))
            extract_elapsed = time.time() - extract_start
            print(f"Extracted {len(existing)} frames in {extract_elapsed:.0f}s")

    TOTAL = len(existing)

    # --- Setup Real-ESRGAN + GFPGAN models ---
    # RealESRGAN_x4plus model is always 4x internally; outscale handles 2x by downsampling
    # Auto-detect tile size based on resolution and GPU VRAM
    # Tiling processes the image in NxN chunks to fit in VRAM
    # RealESRGAN_x4plus always processes at 4x internally, so output pixels = input * 16
    tile_size = 0  # 0 = no tiling (fastest)
    gpu_mem_gb = 0
    if torch.cuda.is_available():
        gpu_mem_gb = torch.cuda.get_device_properties(0).total_memory / (1024**3)
    pixels = src_w * src_h
    # Empirical data points (no-tile mode, RealESRGAN_x4plus):
    #   1430x1080 (1.54 MP) = OK on 24GB GPU
    #   1920x1200 (2.30 MP) = OOM on 48GB GPU (needed ~50GB total)
    # VRAM scales super-linearly with resolution due to model internals.
    # Safe limits per GPU size (conservative):
    mpixels = pixels / 1e6
    if gpu_mem_gb >= 70:
        safe_mp = 4.0   # 80GB A100: safe up to ~4.0 MP (e.g. 2560x1440)
    elif gpu_mem_gb >= 40:
        safe_mp = 2.0   # 48GB: safe up to ~2.0 MP (e.g. 1920x1040)
    elif gpu_mem_gb >= 20:
        safe_mp = 1.6   # 24GB: safe up to ~1.6 MP (e.g. 1430x1080)
    elif gpu_mem_gb >= 10:
        safe_mp = 0.7   # 12GB: conservative
    else:
        safe_mp = 0.3   # 8GB or less
    if mpixels > safe_mp:
        tile_size = 512 if gpu_mem_gb >= 16 else 384
        if mpixels > safe_mp * 2:
            tile_size = 384 if gpu_mem_gb >= 16 else 192
        if mpixels > safe_mp * 4:
            tile_size = 192
        print(f"Resolution: {src_w}x{src_h} ({mpixels:.1f} MP), VRAM: {gpu_mem_gb:.0f}GB (safe: {safe_mp} MP) — using tile={tile_size}")
    else:
        print(f"Resolution: {src_w}x{src_h} ({mpixels:.1f} MP), VRAM: {gpu_mem_gb:.0f}GB (safe: {safe_mp} MP) — no tiling needed")

    print(f"\nLoading Real-ESRGAN model (output scale={SCALE}x, tile={tile_size})...")
    model = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
    upsampler = RealESRGANer(
        scale=4,
        model_path="https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth",
        model=model,
        tile=tile_size,
        tile_pad=10,
        pre_pad=0,
        half=True,
        gpu_id=0 if torch.cuda.is_available() else None
    )
    print("Real-ESRGAN loaded.")

    # Suppress verbose tile progress output from Real-ESRGAN
    import logging
    logging.getLogger('basicsr').setLevel(logging.WARNING)
    logging.getLogger('realesrgan').setLevel(logging.WARNING)

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
    print(f"Benchmark: {per_frame:.2f}s per frame ({1/per_frame:.2f} fps)")
    print(f"Estimated total: {total_est / 3600:.1f} hours for {TOTAL} frames")

    # Cost estimate from instance metadata
    meta_file = os.path.expanduser("~/instance_meta.json")
    if os.path.exists(meta_file):
        try:
            import json as _json
            with open(meta_file) as f:
                meta = _json.load(f)
            cost_hr = float(meta.get("cost_per_hr", 0))
            if cost_hr > 0:
                est_cost = cost_hr * total_est / 3600
                print(f"Estimated cost: ${est_cost:.0f} (${cost_hr}/hr × {total_est / 3600:.1f}h)")
        except Exception:
            pass

    # Abort if benchmark is too slow (>10s per frame = very inefficient GPU)
    if per_frame > 10.0:
        print(f"\nWARNING: Very slow benchmark ({per_frame:.1f}s/frame)!")
        print(f"This GPU may not be suitable for this workload.")
        print(f"At this speed: {total_est / 3600:.0f} hours, likely >$100.")
        print(f"Consider using a different GPU (RTX 5090 with tiling is typically 2-4s/frame).")
        print(f"To proceed anyway, set FORCE_SLOW=1 environment variable.")
        if not os.environ.get("FORCE_SLOW"):
            print("ABORTING — too slow. Use FORCE_SLOW=1 to override.")
            sys.exit(1)
        print("FORCE_SLOW=1 set — proceeding despite slow benchmark.")

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

    # Use parallel I/O: pre-read frames on CPU threads, write output on CPU threads
    # GPU processing happens on main thread (CUDA is not thread-safe)
    from concurrent.futures import ThreadPoolExecutor
    import queue

    cpu_count = os.cpu_count() or 1
    read_workers = min(max(cpu_count // 4, 2), 8)
    write_workers = min(max(cpu_count // 4, 2), 8)
    prefetch_size = read_workers * 2  # buffer ahead of GPU

    print(f"I/O pipeline: {read_workers} read workers, {write_workers} write workers, prefetch={prefetch_size}")
    sys.stdout.flush()

    # Build list of frames to process
    todo = []
    for i, fpath in enumerate(existing):
        fname = os.path.basename(fpath)
        out_path = f"{FRAMES_OUT}/{fname}"
        if not os.path.exists(out_path):
            todo.append((i, fpath, fname, out_path))

    if todo:
        # Pre-read queue: (index, img, frame_num, out_path)
        read_queue = queue.Queue(maxsize=prefetch_size)
        write_queue = queue.Queue(maxsize=prefetch_size)
        read_done = [False]

        def reader():
            for i, fpath, fname, out_path in todo:
                img = cv2.imread(fpath, cv2.IMREAD_UNCHANGED)
                read_queue.put((i, img, out_path))
            read_done[0] = True

        def writer():
            while True:
                item = write_queue.get()
                if item is None:
                    break
                out_path, output = item
                cv2.imwrite(out_path, output)
                write_queue.task_done()

        import threading
        read_thread = threading.Thread(target=reader, daemon=True)
        read_thread.start()

        write_threads = []
        for _ in range(write_workers):
            t = threading.Thread(target=writer, daemon=True)
            t.start()
            write_threads.append(t)

        processed = 0
        for _ in range(len(todo)):
            i, img, out_path = read_queue.get()
            output = enhance_frame(img, frame_num=i)
            write_queue.put((out_path, output))
            processed += 1

            if processed % 10 == 0:
                elapsed = time.time() - start
                fps = processed / elapsed if elapsed > 0 else 0
                remaining_frames = len(todo) - processed
                remaining = remaining_frames / fps if fps > 0 else 0
                total_done = done + processed
                print(f"  {total_done}/{TOTAL} ({fps:.1f} fps, ~{remaining/60:.0f}m remaining)")
                sys.stdout.flush()

        # Wait for all writes to finish
        write_queue.join()
        for _ in write_threads:
            write_queue.put(None)
        for t in write_threads:
            t.join()

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
