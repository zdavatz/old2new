# old2new

Improve video quality of old Da Vaz movies using [Real-ESRGAN](https://github.com/xinntao/Real-ESRGAN) AI upscaling.

## Requirements

- macOS (Apple Silicon) for local runs
- [yt-dlp](https://github.com/yt-dlp/yt-dlp) — `brew install yt-dlp`
- [ffmpeg](https://ffmpeg.org/) — `brew install ffmpeg`
- bc — `brew install bc` (usually pre-installed)

Real-ESRGAN is downloaded automatically on first run.

## Usage

### Local (macOS)

```bash
./enhance.sh "<youtube-url>"
./enhance.sh /path/to/video.mp4
```

**Important**: Always quote YouTube URLs to prevent the shell from interpreting `?` as a glob character. Local file paths don't need quoting (unless they contain spaces).

The script will:
1. Download the video via yt-dlp
2. Detect your machine specs (chip, GPU cores, RAM)
3. Benchmark one frame at 2x and 4x to estimate processing time
4. Check available disk space and warn if insufficient
5. Present enhancement options with time estimates:
   - **Option 1**: Minimal enhance (2x upscale)
   - **Option 2**: Maximum enhance (4x upscale)
6. Extract frames, upscale with Real-ESRGAN, and reassemble with original audio

### Google Cloud (one-command setup)

The `gcp_setup.sh` script handles everything: creates a GPU instance, installs deps, and starts enhancement.

```bash
# Setup and run
./gcp_setup.sh "<youtube-url>" [scale] [project-id] [zone]

# Check progress
./gcp_setup.sh status [project-id]
```

The script pre-checks video resolution and disk needs before creating the instance. If 4x upscale exceeds the 500GB GCP disk quota, it will suggest 2x instead.

#### Examples

```bash
# 4x upscale (default)
./gcp_setup.sh "https://www.youtube.com/watch?v=rX4ADnOa3G4"

# 2x upscale (for HD source videos or limited disk)
./gcp_setup.sh "https://www.youtube.com/watch?v=aefe1fn7Kf0" 2 old2new-davaz

# Check status with ETA
./gcp_setup.sh status old2new-davaz

# Download result when done
gcloud compute scp old2new-gpu:~/jobs/*/enhanced_*.mkv . --project=old2new-davaz --zone=us-central1-a

# DELETE instance when done to stop billing!
gcloud compute instances delete old2new-gpu --project=old2new-davaz --zone=us-central1-a
```

#### Prerequisites

1. Install gcloud CLI: `brew install --cask google-cloud-sdk`
2. Authenticate: `gcloud auth login`
3. Create a project with billing enabled
4. Request GPUS_ALL_REGIONS quota increase to 1

### Single Video (vast.ai — any YouTube URL)

Enhance any YouTube video with one command on a vast.ai RTX 4090:

```bash
# One-time setup
pipx install vastai
vastai set api-key YOUR_KEY

# Enhance a video
./vast_batch.sh "https://www.youtube.com/watch?v=d6ph7n4k35Y"

# Monitor (shows dashboard URL)
./vast_batch.sh status

# Download when done
./vast_batch.sh download

# Clean up
./vast_batch.sh destroy
```

The script auto-detects HD videos and recommends 2x upscale. You can also specify the scale:

```bash
./vast_batch.sh "https://www.youtube.com/watch?v=d6ph7n4k35Y" 2
```

### Single Video / Batch (TensorDock)

Enhance videos on TensorDock GPU instances (SSH VMs with RTX 4090, auto-sized disk):

```bash
# One-time setup: export TENSORDOCK_API_KEY in ~/.bashrc

# Test with a single video (auto-calculates disk needs)
./tensordock_batch.sh test mgUOHubnEC8
./tensordock_batch.sh status          # shows dashboard URL, SSH command
./tensordock_batch.sh ssh 0           # SSH into instance
./tensordock_batch.sh download        # download when done
./tensordock_batch.sh destroy         # clean up

# Launch batch (multiple parallel instances)
./tensordock_batch.sh launch 4
./tensordock_batch.sh status

# List all 226 videos
./tensordock_batch.sh list
```

- **Auto disk sizing**: fetches exact resolution via `yt-dlp --dump-json`, calculates disk with 2.5x PNG compression + 20% safety margin
- **Auto GPU selection**: HD videos (>1.6 MP) auto-switch to RTX 5090 — refuses to launch on RTX 4090 where tiling would be 8x slower
- **Proven profiles**: SD-4x on RTX 4090 Ottawa (2.6 fps, $0.41/hr, 650GB) | HD-2x on RTX 5090 Chubbuck (1700-3000GB, ~$0.75/hr)
- Queue multiple videos on one instance — fully automated pipeline per video:
  1. Upscale with Real-ESRGAN → 2. Upload to YouTube (copies title + "Enhanced 4K" suffix) → 3. Email juerg@davaz.com with old + new links → 4. Delete .mkv to free disk
- OAuth credentials (`client_secret.json`, `youtube_token.json`) auto-deployed to instances via cloud-init write_files
- Fast startup: disables Ubuntu unattended-upgrades via cloud-init bootcmd, skips apt update (~3min vs ~12min)
- Direct SSH access (user `user`, not root)
- Web dashboard via nginx reverse proxy (reliable, no dropped connections)
- Cloud-init auto-installs all dependencies (PyTorch, Real-ESRGAN, ffmpeg; auto-detects Blackwell GPUs for CUDA 12.8)

### Batch Processing (vast.ai — all 226 davaz.com videos)

The `vast_batch.sh` script also processes all 226 davaz.com videos in parallel on multiple RTX 4090 instances:

```bash
# Test with a single video first to check quality
./vast_batch.sh test
./vast_batch.sh status          # get dashboard URL
./vast_batch.sh download        # download when done
./vast_batch.sh destroy         # clean up test instance

# Launch full batch (4 parallel instances, ~15 days, ~$490)
./vast_batch.sh launch 4
./vast_batch.sh status          # monitor + open dashboard URLs in browser
./vast_batch.sh download        # download completed videos
./vast_batch.sh destroy         # clean up when done

# List all 226 videos
./vast_batch.sh list
```

- SD videos get 4x upscale, HD videos get 2x
- Job directories use movie titles (e.g., `~/jobs/CAMBODIA_DUST_of_LIFE/`)
- Greedy load-balancing distributes videos evenly across instances
- Each instance has a web dashboard with progress bars, log tail, and side-by-side frame comparison
- Dashboard accessible via `bore.pub` tunnel (vast.ai) or direct IP (GCP)

### Cloud GPU (manual setup via vast.ai / RunPod)

For processing individual videos, use `enhance_gpu.py` on a cloud GPU instance with CUDA + PyTorch:

```bash
# On a cloud instance with CUDA:
pip install realesrgan yt-dlp "numpy<2" "torchvision==0.15.2" "basicsr==1.4.2" opencv-python-headless
apt-get install -y ffmpeg
python3 enhance_gpu.py "<youtube-url>" [scale]
python3 enhance_gpu.py "<youtube-url>" [scale] --job-name "Movie_Title"
```

### Output

Enhanced videos are saved as `jobs/<title>/<title>_<scale>x.mkv`. For example:

```
jobs/009_ChickenPick/
  009_ChickenPick.mkv        # original downloaded video
  009_ChickenPick_4x.mkv     # enhanced output
  frames_in/                  # original extracted frames
  frames_out/                 # upscaled frames
```

Without `--job-name`, the YouTube video ID is used as the directory and file name.

The process is resumable — if interrupted, re-run the same command and it will skip already-completed steps (download, frame extraction, upscaled frames).

#### Pre-flight Check

`enhance_gpu.py` runs a comprehensive pre-flight check before any processing:

- **GPU**: CUDA compute capability, VRAM, driver version, Blackwell (sm_120) detection
- **PyTorch**: Version, CUDA version, FP16 kernel compatibility test
- **CPU**: Model, clock speed, single-core benchmark (warns if too slow)
- **RAM**: Total and available memory
- **Disk**: Space available, read/write speed benchmark
- **PCIe**: Generation and lane width (bandwidth estimate)
- **Software**: ffmpeg (need 5+), numpy (need <2), opencv, basicsr, realesrgan, yt-dlp

If any check fails, it exits with specific fix commands — no wasted time downloading on broken instances.

After the pre-flight check, a **pre-download disk estimate** fetches video metadata via `yt-dlp --dump-json` (without downloading) to calculate disk needs based on resolution, duration, and scale. If disk space is insufficient, it aborts before downloading — preventing wasted bandwidth and time on undersized instances.

#### Performance Tips

- **CPU cores matter for upscaling**: 4 vCPUs = 2.6 fps, 16 vCPUs = 7.0 fps on the same RTX 4090. The I/O pipeline needs 8 read + 8 write threads to keep the GPU fed. Always request 16+ vCPUs.
- **Disk speed matters**: 624 MB/s = 2.6 fps, 1207 MB/s NVMe = 7.0 fps. Need >=1000 MB/s NVMe for full GPU utilization.
- **CPU clock speed matters**: A Xeon Phi (272 cores @ 1.4GHz) was 4x slower than an EPYC (32 cores @ 2.25GHz) with the same RTX 5090, because `cv2.imread`/`cv2.imwrite` bottleneck on per-core speed. Prefer machines with >2GHz per-core.
- **RTX 5090** (Blackwell): Needs PyTorch 2.6+ with CUDA 12.8. The default Docker image must be upgraded.
- Frame extraction uses parallel ffmpeg workers (up to 16) on multi-core machines.
- Upscaling uses a parallel I/O pipeline (threaded pre-read + async write) to overlap CPU and GPU work.
- VRAM-based auto-tiling prevents OOM errors on different GPU sizes.

## GPU Selection Guide

Two GPU profiles handle the entire davaz.com video collection:

| Profile | GPU | VRAM | Use Case | Speed | Cost/hr | Tiling |
|---------|-----|------|----------|-------|---------|--------|
| **SD-4x** | RTX 4090 | 24GB | SD videos ≤1.6 MP (e.g. 640x480) | 2.6 fps | ~$0.41 | None |
| **HD-2x** | RTX 5090 | 32GB | HD videos >1.6 MP (e.g. 1920x1200) | 1.7 fps | ~$0.75-1.05 | None |

The RTX 4090 is the workhorse — faster per frame, cheaper, handles 79 of 83 SD videos. The RTX 5090 is needed for HD videos where the 4090's 24GB VRAM causes tiling (8x slower). The script auto-detects resolution and selects the right GPU.

### RTX 4090 vs RTX 5090 for Real-ESRGAN

| Aspect | RTX 4090 (24GB) | RTX 5090 (32GB) |
|--------|-----------------|-----------------|
| SD 4x (640x480) | **2.6 fps** | ~2+ fps |
| HD 2x (1920x1200) | 0.3 fps (tiling) | **1.7 fps** |
| Cost/hr | ~$0.41 | ~$0.75 |
| Best for | SD videos ≤1.6 MP | HD videos ≤2.3 MP |

**A100 is NOT suitable for Real-ESRGAN** — despite 80GB VRAM, it runs at 0.07 fps (14s/frame) due to low clock speeds. Never use A100/datacenter GPUs for upscaling.

**Rule of thumb**: Use RTX 4090 for SD (≤1.6 MP). Use RTX 5090 for HD (≤2.3 MP).

## Performance Reference

| Hardware | Per frame | 11 min video (~16k frames) | Cost |
|----------|-----------|---------------------------|------|
| M5 (10 GPU cores) | ~10s | ~45h | — |
| M3 Ultra (76 GPU cores) | ~1.5-2s | ~7-9h | — |
| RTX 4090 (TensorDock, CUDA) | ~0.4s | ~1.8h | ~$0.74 |
| RTX 5090 (vast.ai, CUDA) | ~0.6s | ~2.7h | ~$1.07/hr |
| A100 80GB (NOT recommended) | ~14s | ~62h | ~$66 |
| L4 (Google Cloud, CUDA) | ~2s | ~9h | ~$6.30 |
| H200 NVL (RunPod, CUDA) | ~1-1.5s | ~5-6h | ~$17-20 |

### Cloud GPU Providers

- **[vast.ai](https://vast.ai)**: Cheapest option. RTX 4090 at ~$0.34/hr, RTX 5090 at ~$0.34/hr, A100 80GB at ~$0.66/hr. CLI: `pip install vastai`. Use `vast_batch.sh` for automated batch processing.
- **[TensorDock](https://tensordock.com)**: SSH VMs with auto-sized disk. RTX 4090 at ~$0.41/hr (SD videos), RTX 5090 at ~$0.75/hr (HD videos). Auto-detects tiling risk and selects correct GPU. Proven: 2.6 fps on SD-4x, queue 10+ videos per instance. Use `tensordock_batch.sh`.
- **[RunPod](https://runpod.io)**: **NOT WORKING** (as of 2026-03-18). Pods show "RUNNING" but never actually start — uptime stays 0, no SSH ports assigned. Tested with RTX Pro 6000 Blackwell (96GB, $1.69/hr) and RTX 5090, multiple datacenters, various Docker images, with/without network volumes. Platform-level issue. `runpod_launch.sh` script exists but is unusable until RunPod fixes this.
- **[Google Cloud](https://cloud.google.com)**: Always available. L4 at ~$0.70/hr. Use `gcp_setup.sh` for one-command setup. Requires GPUS_ALL_REGIONS quota for new projects.

**Note**: On cloud instances, use the Python/CUDA approach (`enhance_gpu.py`) instead of the ncnn-vulkan binary, as Vulkan drivers are often not available in Docker containers.

### Enhancement Status Check

Check which videos already have an Enhanced 4K version on YouTube:

```bash
# Check enhancement status (requires YouTube OAuth credentials)
venv/bin/python3 check_enhanced.py       # finds enhanced vs missing videos
venv/bin/python3 fetch_missing_videos.py  # fetches actual resolution for missing videos
```

Results are saved to:
- `enhanced_status.json` — summary of enhanced vs non-enhanced videos
- `not_enhanced.json` — detailed list with youtube_id, resolution, GPU requirement per video
- `not_enhanced_rtx4090.json` — SD videos for RTX 4090 (72 videos, 24.8h, 4x upscale)
- `not_enhanced_rtx5090.json` — HD videos for RTX 5090 (143 videos, 33.5h, 2x upscale)

As of March 2026: 11 of 226 videos enhanced, 215 remaining (72 SD → RTX 4090 4x, 143 HD → RTX 5090 2x). Videos are split by YouTube definition (hd/sd), not by resolution.

## License

GPL-3.0
