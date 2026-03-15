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
- **enhance_gpu.py** — Cloud GPU script: same pipeline but uses PyTorch/CUDA for upscaling. Checks disk space before starting. Supports `--job-name` for custom directory names. Uses `~/jobs/<name>/` for work directories.
- **gcp_setup.sh** — One-command Google Cloud setup: pre-checks video size and disk needs → creates L4 GPU instance → installs all deps → downloads enhance_gpu.py → starts enhancement. Also supports `status` command with ETA.
- **vast_batch.sh** — Batch processing of all 226 davaz.com videos on parallel vast.ai RTX 4090 instances. Embeds video list with titles from YouTube API. Greedy load-balancing across instances. Each instance runs a web status page on port 8080. Supports `test` (single video), `launch` (full batch), `status`, `download`, `destroy`.
- **realesrgan/** — Auto-downloaded binary and models (gitignored). macOS ARM64 binary from github.com/xinntao/Real-ESRGAN
- **jobs/<title>/** — Per-video working directories using movie titles (not video IDs), containing original video, extracted frames, upscaled frames, and final output (gitignored)

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
- Both scripts check disk space before starting and abort with clear error if insufficient
- Processing is resumable: each step checks for existing output before re-running
- The `realesrgan-x4plus` model is used for both 2x and 4x upscaling (general-purpose, best for real-world content)
- Video reassembly uses libx264 with CRF 18 (visually lossless) and copies original audio stream

## Cloud GPU Deployment

- **vast.ai**: Use `pytorch/pytorch:2.1.0-cuda12.1-cudnn8-runtime` image. SSH access via `vastai` CLI. Cheapest option (~$0.34/hr for RTX 4090). Request >=250GB disk for long videos. Use `vast_batch.sh` for automated batch processing. API key stored in `~/.config/vastai/vast_api_key`.
- **RunPod**: Use `runpod/pytorch` image. SSH access via RunPod API. Often sold out on weekends.
- **Google Cloud**: Use `gcp_setup.sh` for automated setup. Image: `pytorch-2-7-cu128-ubuntu-2204-nvidia-570`, machine: `g2-standard-4` + L4 GPU. Requires GPUS_ALL_REGIONS quota increase for new projects.
- API keys stored in `~/.zshrc` as `VAST_API_KEY` and `RUNPOD_API_KEY`
- Google Cloud projects: old2new-490311 (zdavatz@ywesee.com), old2new-davaz (juerg@davaz.com)

## Cloud Python Dependency Fixes

The `realesrgan` package has version conflicts on many cloud images:
- `numpy<2` required (numpy 2.x breaks basicsr)
- `torchvision==0.15.2` and `basicsr==1.4.2` needed if torchvision is too new (missing `functional_tensor`)
- Must uninstall `opencv-python` AND `opencv-contrib-python` before installing `opencv-python-headless`
- On GCP deep learning images, also install `libgl1` and `libglib2.0-0` system packages

## Dependencies

Local (Homebrew): `yt-dlp`, `ffmpeg`, `bc`
Cloud (pip/apt): `realesrgan`, `yt-dlp`, `numpy<2`, `torchvision==0.15.2`, `basicsr==1.4.2`, `opencv-python-headless`, `ffmpeg` (static binary on GCP)
