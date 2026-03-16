# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

old2new enhances old Da Vaz videos using Real-ESRGAN AI upscaling. There are two approaches depending on the environment:

1. **Local (macOS)**: `enhance.sh` — uses Real-ESRGAN ncnn-vulkan binary (Vulkan/Metal)
2. **Cloud GPU**: `enhance_gpu.py` — uses Real-ESRGAN Python package (PyTorch/CUDA)
3. **Google Cloud (one-command)**: `gcp_setup.sh` — creates instance, installs deps, runs enhancement
4. **Batch (vast.ai)**: `vast_batch.sh` — parallel upscaling of all 226 davaz.com videos on multiple RTX 4090 instances

## Architecture

- **enhance.sh** — macOS script: accepts YouTube URL or local video file → detect hardware → benchmark → check disk space → interactive menu → extract frames (ffmpeg) → upscale (Real-ESRGAN ncnn-vulkan) → reassemble (ffmpeg)
- **enhance_gpu.py** — Cloud GPU script: same pipeline but uses PyTorch/CUDA for upscaling. Runs comprehensive pre-flight check (GPU, CPU, RAM, disk, PCIe, software) before any processing. Parallel frame extraction using multiple ffmpeg workers (up to 16). Parallel I/O pipeline (threaded pre-read + async write) to overlap CPU I/O with GPU compute. Auto-tiling based on VRAM size to prevent OOM. Supports `--job-name` for custom directory names. Uses `~/jobs/<name>/` for work directories.
- **gcp_setup.sh** — One-command Google Cloud setup: pre-checks video size and disk needs → creates L4 GPU instance → installs all deps → downloads enhance_gpu.py → starts enhancement. Also supports `status` command with ETA.
- **vast_batch.sh** — Versatile vast.ai script. Supports: (1) any YouTube URL as first arg for single video enhancement, (2) `test` for testing with a davaz.com video, (3) `launch N` for batch processing all 226 davaz.com videos on N parallel RTX 4090 instances. Also: `status` (shows dashboard URLs), `download`, `destroy`, `list`. Auto-detects HD and recommends 2x. Fetches video info via yt-dlp. Web dashboard via bore.pub tunnel.
- **realesrgan/** — Auto-downloaded binary and models (gitignored). macOS ARM64 binary from github.com/xinntao/Real-ESRGAN
- **jobs/<title>/** — Per-video working directories using movie titles (not video IDs). Files named after title: `<title>.mkv` (input), `<title>_4x.mkv` (output). Contains extracted frames in `frames_in/` and upscaled frames in `frames_out/` (gitignored)

## Important

- URLs passed to `enhance.sh` and `gcp_setup.sh` must be quoted (e.g., `./enhance.sh "https://..."`) because `?` in YouTube URLs is interpreted as a glob by zsh
- `ffprobe -print_format flat` outputs dots in variable names (e.g., `streams.stream.0.width`), which must be converted to underscores via `sed 's/\./_/g'` before `eval` in bash
- `enhance_gpu.py` uses `os.path.expanduser("~")` for the jobs directory — never hardcode `/root/jobs` as cloud instances may run as different users
- `enhance_gpu.py --job-name` allows custom directory names (used by vast_batch.sh to name dirs after movie titles)
- `vast_batch.sh` embeds all 226 davaz.com video IDs, durations, definitions, and titles. Video list was sourced from the davaz2 MySQL database and YouTube Data API v3.
- GCP deep learning images have broken apt ffmpeg deps — use static ffmpeg binary from johnvansickle.com instead
- GCP disk must be sized before instance creation. Default SSD quota is 500GB. Use `growpart` + `resize2fs` if disk is resized after creation.
- 4x upscale of HD video (1080p) needs ~650GB disk. Recommend 2x for HD source videos.

## Key Details

- Local: Real-ESRGAN ncnn-vulkan uses Vulkan for GPU compute — works on Apple Silicon (Metal via MoltenVK)
- Cloud: ncnn-vulkan does NOT work in most Docker containers (no Vulkan driver). Use the Python package with CUDA instead.
- `enhance_gpu.py` runs a pre-flight check that validates: GPU CUDA arch compatibility, PyTorch/CUDA versions, CPU single-core benchmark, RAM, disk space + I/O speed, PCIe gen/width, ffmpeg version, and all Python package versions. Exits with specific fix commands if anything fails.
- Pre-download disk check: fetches video metadata via `yt-dlp --dump-json` (no download) to estimate disk needs from resolution × duration × scale. Aborts before downloading if disk is insufficient.
- CPU single-core speed matters: Xeon Phi (1.4GHz, 272 cores) was 4x slower than EPYC (2.25GHz, 32 cores) with same RTX 5090 GPU because cv2.imread/imwrite bottlenecks on per-core speed. Prefer machines with >2GHz per-core.
- RTX 5090 (Blackwell, sm_120) needs PyTorch 2.6+ with CUDA 12.8. The `pytorch:2.1.0-cuda12.1` Docker image must be upgraded: `pip install torch torchvision --index-url https://download.pytorch.org/whl/cu128`. Also patch basicsr: `sed -i 's/functional_tensor/functional/' .../degradations.py` and suppress tile spam: `sed -i "s/print(f'.*Tile/pass  # /" .../realesrgan/utils.py`
- Processing is resumable: each step checks for existing output before re-running
- The `realesrgan-x4plus` model is used for both 2x and 4x upscaling (general-purpose, best for real-world content)
- GFPGAN is DISABLED — it hallucinates facial features and changes how people look. Not suitable for documentary footage. Real-ESRGAN alone provides good upscaling.
- Video reassembly uses libx264 with CRF 18 (visually lossless) and copies original audio stream
- Frame extraction uses parallel ffmpeg workers (up to 16) when multiple CPUs are available. Always enable parallel extraction on new instances.
- Auto-tiling for VRAM management: RTX 4090 (24GB) safe up to 1.6 MP, RTX 5090/48GB up to 2.0 MP, A100 80GB up to 4.0 MP. Higher resolutions automatically use tile=512 or smaller.
- Web dashboard (`status_server.py`) shows progress bars, input/output filenames, side-by-side frame comparison, system specs (GPU/CPU/RAM/disk), and instance metadata (cost, location, provider). On vast.ai, use `bore.pub` tunnel for HTTP access (direct ports often blocked by host firewall).
- Instance metadata stored in `~/instance_meta.json` (label, location, cost_per_hr, provider, instance_id) — displayed in dashboard header.

## Cloud GPU Deployment

- **vast.ai**: Use `pytorch/pytorch:2.1.0-cuda12.1-cudnn8-runtime` image (upgrade PyTorch for RTX 5090). SSH access via `vastai` CLI. Cheapest option (~$0.34/hr for RTX 4090/5090). Request >=250GB disk for short videos, >=2TB for long HD films (1920x1200 @ 2x needs ~1.8TB). Use `vast_batch.sh` for automated batch processing. API key stored in `~/.config/vastai/vast_api_key`. When choosing instances: check CPU clock speed (>2GHz), disk space, and PCIe gen — not just GPU and price.
- **RunPod**: Use `runpod/pytorch` image. SSH access via RunPod API. Often sold out on weekends.
- **Google Cloud**: Use `gcp_setup.sh` for automated setup. Image: `pytorch-2-7-cu128-ubuntu-2204-nvidia-570`, machine: `g2-standard-4` + L4 GPU. Requires GPUS_ALL_REGIONS quota increase for new projects.
- API keys stored in `~/.zshrc` as `VAST_API_KEY` and `RUNPOD_API_KEY`
- Google Cloud projects: old2new-490311 (zdavatz@ywesee.com), old2new-davaz (juerg@davaz.com)

## Cloud Python Dependency Fixes

The `realesrgan` package has version conflicts on many cloud images:
- `numpy==1.26.4` required (numpy 2.x breaks basicsr)
- `torchvision==0.15.2` and `basicsr==1.4.2` needed if torchvision is too new (missing `functional_tensor`)
- Must uninstall `opencv-python` AND `opencv-contrib-python` before installing `opencv-python-headless<4.11` (4.11+ requires numpy>=2)
- On cloud images, install `libgl1` and `libglib2.0-0` system packages
- PyTorch Docker image ships ffmpeg 4.3 which can't merge webm — install static ffmpeg 7.x from johnvansickle.com (replace `/opt/conda/bin/ffmpeg`)

## Dependencies

Local (Homebrew): `yt-dlp`, `ffmpeg`, `bc`
Cloud (pip/apt): `realesrgan`, `yt-dlp`, `numpy<2`, `torchvision==0.15.2`, `basicsr==1.4.2`, `opencv-python-headless`, `ffmpeg` (static binary on GCP)
Dashboard: `bore` (for vast.ai HTTP tunneling, installed from github.com/ekzhang/bore)
