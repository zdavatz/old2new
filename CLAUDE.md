# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

old2new enhances old Da Vaz videos using Real-ESRGAN AI upscaling. The main entry point is `enhance.sh`, a self-contained bash script that downloads YouTube videos, upscales frames, and reassembles the output.

## Architecture

- **enhance.sh** — Single script handling the full pipeline: download (yt-dlp) → extract frames (ffmpeg) → upscale (Real-ESRGAN) → reassemble (ffmpeg)
- **realesrgan/** — Auto-downloaded binary and models (gitignored). macOS ARM64 binary from github.com/xinntao/Real-ESRGAN
- **jobs/<video-id>/** — Per-video working directories containing original video, extracted frames, upscaled frames, and final output (gitignored)

## Key Details

- Real-ESRGAN uses Vulkan for GPU compute — works on Apple Silicon (Metal via MoltenVK)
- The script benchmarks the actual machine before presenting time estimates
- Processing is resumable: each step checks for existing output before re-running
- The `realesrgan-x4plus` model is used for both 2x and 4x upscaling (general-purpose, best for real-world content)
- Video reassembly uses libx264 with CRF 18 (visually lossless) and copies original audio stream

## Dependencies

All installed via Homebrew: `yt-dlp`, `ffmpeg`, `bc`
