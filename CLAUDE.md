# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

old2new enhances old Da Vaz videos using Real-ESRGAN AI upscaling. There are two approaches depending on the environment:

1. **Local (macOS)**: `enhance.sh` — uses Real-ESRGAN ncnn-vulkan binary (Vulkan/Metal)
2. **Cloud GPU**: `enhance_gpu.py` — uses Real-ESRGAN Python package (PyTorch/CUDA)

## Architecture

- **enhance.sh** — macOS script: download (yt-dlp) → detect hardware → benchmark → interactive menu → extract frames (ffmpeg) → upscale (Real-ESRGAN ncnn-vulkan) → reassemble (ffmpeg)
- **enhance_gpu.py** — Cloud GPU script: same pipeline but uses PyTorch/CUDA for upscaling. Used on vast.ai and RunPod instances.
- **realesrgan/** — Auto-downloaded binary and models (gitignored). macOS ARM64 binary from github.com/xinntao/Real-ESRGAN
- **jobs/<video-id>/** — Per-video working directories containing original video, extracted frames, upscaled frames, and final output (gitignored)

## Key Details

- Local: Real-ESRGAN ncnn-vulkan uses Vulkan for GPU compute — works on Apple Silicon (Metal via MoltenVK)
- Cloud: ncnn-vulkan does NOT work in most Docker containers (no Vulkan driver). Use the Python package with CUDA instead.
- Cloud Python deps: `realesrgan`, `yt-dlp`, `numpy<2` (numpy 2.x breaks basicsr)
- The script benchmarks the actual machine before presenting time estimates
- Processing is resumable: each step checks for existing output before re-running
- The `realesrgan-x4plus` model is used for both 2x and 4x upscaling (general-purpose, best for real-world content)
- Video reassembly uses libx264 with CRF 18 (visually lossless) and copies original audio stream

## Cloud GPU Deployment

- **vast.ai**: Use `ubuntu:22.04` or `pytorch/pytorch` image. Install deps via pip/apt. SSH access via `vastai` CLI.
- **RunPod**: Use `runpod/pytorch` or `nvidia/cuda` image. SSH access via RunPod CLI or API.
- API keys stored in `~/.zshrc` as `VAST_API_KEY` and `RUNPOD_API_KEY`

## Dependencies

Local (Homebrew): `yt-dlp`, `ffmpeg`, `bc`
Cloud (pip/apt): `realesrgan`, `yt-dlp`, `numpy<2`, `ffmpeg`, `bc`
