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
| **SD-4x** | RTX 4090 | 24GB | SD videos ≤1.6 MP (e.g. 640x480) | 7.0 fps | ~$0.50 | None |
| **HD-2x (low-res)** | RTX 5090 | 32GB | HD videos ≤1.6 MP (e.g. 960x720) | 1.7 fps | ~$0.69 | None |
| **HD-2x (high-res)** | RTX 5090 | 32GB | HD videos >1.6 MP (e.g. 1920x1200) | 0.5 fps | ~$0.69 | tile=512 |

The RTX 4090 handles all SD videos without tiling. The RTX 5090 handles HD videos — low-res HD without tiling (1.7 fps), high-res HD with tiling (0.5 fps). Tiling is auto-detected per video based on resolution vs VRAM.

### RTX 4090 vs RTX 5090 for Real-ESRGAN

| Aspect | RTX 4090 (24GB) | RTX 5090 (32GB) |
|--------|-----------------|-----------------|
| SD 4x (640x480) | **7.0 fps** (16 vCPUs) | ~2+ fps |
| HD 2x (960x720) | tiling (slow) | **1.7 fps** (no tile) |
| HD 2x (1920x1200) | 0.3 fps (tiling) | **0.5 fps** (tile=512) |
| Cost/hr | ~$0.50 | ~$0.69 |
| Best for | SD videos ≤1.6 MP | HD videos (any resolution) |

**Datacenter GPUs are NOT suitable for Real-ESRGAN** — tested:
- B200 179GB: 0.57 fps (1.76s/frame) — same speed as RTX 5090 but **4x more expensive** ($3.13/hr vs $0.76/hr). Only draws 205W of 1000W TDP — Real-ESRGAN cannot utilize the compute
- A100 80GB: 0.07 fps (14s/frame) — low clock speeds
- L40S 48GB: 0.3 fps (3.3s/frame) — slower than RTX 5090 despite more VRAM, lower TDP (350W vs 575W)
- RTX Pro 6000 WS Max-Q 96GB: 0.44 fps — power-throttled at 300W

**Rule of thumb**: Use RTX 4090 for SD (≤1.6 MP). Use RTX 5090 for HD. More VRAM does NOT help — even 179GB B200 is no faster than 32GB RTX 5090. Consumer GPUs always win on price/performance for Real-ESRGAN.

### GPU Power Limit & Variant Comparison

GPU power limit directly impacts Real-ESRGAN performance. "Max-Q" / workstation variants throttle under sustained load:

| GPU | Power | Clock | fps (1920x1200, tile=512) | $/hr |
|-----|-------|-------|---------------------------|------|
| RTX Pro 6000 S (Server, 96GB) | 600W | 2430 MHz | **0.62 fps** | $0.73 |
| B200 (Datacenter, 179GB) | 1000W (205W actual) | 1965 MHz | 0.57 fps | $3.13 |
| RTX 5090 (Gaming, 32GB) | 575W | 3090 MHz | **0.56 fps** | $0.76 |
| RTX Pro 6000 WS (Max-Q, 96GB) | 300W | 3090 MHz | 0.44 fps | $1.20 |

The pre-flight check now queries `nvidia-smi` for `power.limit` and warns about low-power GPU variants.

### Tiling vs No-Tile Performance

Counter-intuitively, **tiling is faster than no-tile** for high-resolution inputs. RealESRGAN processes at 4x internally, so 1920x1200 becomes 7680x4800 — processing this as one image is slower than 12 smaller tiles:

| Mode | 1920x1200 on RTX 5090 (32GB) | 1920x1200 on Pro 6000 (96GB) |
|------|------------------------------|------------------------------|
| tile=512 | **0.56 fps** | **0.62 fps** |
| no-tile | 0.13 fps (VRAM swapping) | 0.13 fps (compute bottleneck) |

### ncnn-vulkan: NOT Viable on Cloud GPUs

Tested extensively — **ncnn-vulkan does not work for cloud GPU upscaling**:

| Approach | Result | fps |
|----------|--------|-----|
| Pre-built binary (2022) | Fails — doesn't know Blackwell GPUs | — |
| Self-compiled from source | Vulkan ICD fails in Docker (`vkCreateInstance -9`) | — |
| Custom Docker image with `NVIDIA_DRIVER_CAPABILITIES=all` | Vulkan detects GPU but ncnn falls back to CPU | 0.005 fps |
| **PyTorch/CUDA (our approach)** | **Works perfectly** | **0.56-0.62 fps** |

A Docker image with ncnn-vulkan is available at `ghcr.io/zdavatz/realesrgan-ncnn-vulkan:latest` for testing, but **PyTorch/CUDA is 125x faster** on Blackwell GPUs. Always use `enhance_gpu.py` for cloud upscaling.

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
- **[Packet.ai](https://packet.ai)**: GPU aggregator (by hosted.ai). **DEPLOYMENT API BROKEN** (as of 2026-03-18). Lists offers correctly (B300 262GB from $3.45/hr, H100 80GB from $0.92/hr, RTX Pro 6000 96GB from $0.83/hr) but POST /deployments returns INTERNAL_ERROR for all GPUs, all regions, all providers. Bug report sent. Requires $50 minimum wallet balance.
- **[Lambda Labs](https://lambdalabs.com)**: **NOT SUITABLE**. No consumer GPUs (only A10/A100/H100/B200/GH200), perpetually sold out (zero capacity across all regions as of 2026-03-18), and expensive ($1.48-$6.08/hr for single GPU). Datacenter GPUs are proven unsuitable for Real-ESRGAN.
- **[Google Cloud](https://cloud.google.com)**: Always available. L4 at ~$0.70/hr. Use `gcp_setup.sh` for one-command setup. Requires GPUS_ALL_REGIONS quota for new projects.

**Note**: On cloud instances, always use the Python/CUDA approach (`enhance_gpu.py`). The ncnn-vulkan binary does not work in Docker containers — Vulkan ICD fails or falls back to CPU (125x slower). See "ncnn-vulkan: NOT Viable on Cloud GPUs" above.

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
