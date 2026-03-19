# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

old2new enhances old Da Vaz videos using Real-ESRGAN AI upscaling. There are two approaches depending on the environment:

1. **Local (macOS)**: `enhance.sh` — uses Real-ESRGAN ncnn-vulkan binary (Vulkan/Metal)
2. **Cloud GPU**: `enhance_gpu.py` — uses Real-ESRGAN Python package (PyTorch/CUDA)
3. **Google Cloud (one-command)**: `gcp_setup.sh` — creates instance, installs deps, runs enhancement
4. **Batch (vast.ai)**: `vast_batch.sh` — parallel upscaling of all 226 davaz.com videos on multiple RTX 4090 instances
5. **Batch (TensorDock)**: `tensordock_batch.sh` — SSH VMs with auto-sized disk, RTX 4090 instances

## Architecture

- **enhance.sh** — macOS script: accepts YouTube URL or local video file → detect hardware → benchmark → check disk space → interactive menu → extract frames (ffmpeg) → upscale (Real-ESRGAN ncnn-vulkan) → reassemble (ffmpeg)
- **enhance_gpu.py** — Cloud GPU script: same pipeline but uses PyTorch/CUDA for upscaling. Runs comprehensive pre-flight check (GPU, CPU, RAM, disk, PCIe, software) before any processing. Parallel frame extraction using multiple ffmpeg workers (up to 16). Parallel I/O pipeline (threaded pre-read + async write) to overlap CPU I/O with GPU compute. Auto-tiling based on VRAM size to prevent OOM. Supports `--job-name` for custom directory names. Uses `~/jobs/<name>/` for work directories.
- **gcp_setup.sh** — One-command Google Cloud setup: pre-checks video size and disk needs → creates L4 GPU instance → installs all deps → downloads enhance_gpu.py → starts enhancement. Also supports `status` command with ETA.
- **vast_batch.sh** — Versatile vast.ai script. Supports: (1) any YouTube URL as first arg for single video enhancement, (2) `test` for testing with a davaz.com video, (3) `launch N` for batch processing all 226 davaz.com videos on N parallel RTX 4090 instances. Also: `status` (shows dashboard URLs), `download`, `destroy`, `list`. Auto-detects HD and recommends 2x. Fetches video info via yt-dlp. Web dashboard via bore.pub tunnel.
- **runpod_launch.sh** — RunPod GPU pod script (CURRENTLY BROKEN — RunPod platform issue, pods never start). Uses ncnn-vulkan binary instead of PyTorch for lean setup. Supports network volumes for fast pod start, embedded OAuth credentials, RTX Pro 6000 Blackwell (96GB) or RTX 5090. Commands: `test`, `launch`, `status`, `ssh`, `download`, `destroy`, `destroy-all`, `list`.
- **tensordock_batch.sh** — TensorDock GPU VM script. Similar to vast_batch.sh but uses TensorDock API for SSH VMs. Auto-calculates disk size from video resolution × duration × scale before creating instances. Auto-detects tiling risk and switches to RTX 5090 for HD videos. Supports: `test [VIDEO_ID]`, `launch N`, `status`, `ssh N`, `download`, `destroy`, `list`. Cloud-init auto-installs all deps (PyTorch, Real-ESRGAN, ffmpeg, Google API client). Disables Ubuntu unattended-upgrades via cloud-init bootcmd for fast startup (~3min vs ~12min). Deploys OAuth credentials (`client_secret.json`, `youtube_token.json`) via cloud-init write_files for automatic YouTube upload + email notification after each video. Writes instance metadata for dashboard display. Default user is `user` (not root). Port forwarding for SSH (22) and dashboard (8080).
- **youtube_upload.py** — Uploads enhanced video to YouTube, copies title/description from original (adds "Enhanced 4K" suffix), sends email notification to juerg@davaz.com via Gmail API with old + new video links. Requires `client_secret.json` and `youtube_token.json` (OAuth2 with youtube.upload + gmail.send scopes). Token auto-refreshes. Used in the queue pipeline: upscale → upload → notify → delete .mkv to free disk.
- **check_enhanced.py** — Checks via YouTube Data API which of the 226 Da Vaz videos already have an "(Enhanced 4K)" version uploaded. Searches the channel for enhanced titles, matches them back to originals, and outputs a summary with GPU requirements (RTX 4090 for SD, RTX 5090 for HD). Saves results to `enhanced_status.json`.
- **close_enhanced_issues.py** — Auto-closes GitHub issues for videos that have been uploaded as "Enhanced 4K" to YouTube. Queries YouTube API for enhanced videos (two passes: keyword search + date-ordered for recent uploads), matches them to original video IDs, finds open GitHub issues in zdavatz/old2new, and closes them with a comment linking to the new video. Supports dry-run (default) and `--close` mode. Fuzzy title matching handles: HTML entities (`&#39;`), emoji (full Unicode range), possessives (`GIRL'S` → `GIRLS`), digit spacing (`171,2%` → `1712`), and space-insensitive comparison for emoji-heavy titles.
- **fetch_missing_videos.py** — Fetches actual resolution (width × height) for all non-enhanced videos via yt-dlp `--dump-json`. Determines GPU requirement per video based on megapixels (≤1.6 MP → RTX 4090, >1.6 MP → RTX 5090). Saves complete list to `not_enhanced.json` with youtube_id, title, duration, resolution, scale, and GPU recommendation.
- **not_enhanced.json** — Machine-readable list of all Da Vaz videos without Enhanced 4K version. Each entry has: youtube_id, title, duration_seconds, definition (hd/sd), width, height, megapixels, scale (2x/4x), gpu (RTX 4090/RTX 5090). Generated by `fetch_missing_videos.py`.
- **not_enhanced_rtx4090.json** — SD videos needing RTX 4090 (4x upscale). 72 videos, 24.8h total. Split by YouTube definition (sd), not by megapixels.
- **not_enhanced_rtx5090.json** — HD videos needing RTX 5090 (2x upscale). 143 videos, 33.5h total. Split by YouTube definition (hd), not by megapixels. Includes 960x720 and 720x1280 videos that are HD-defined despite low resolution.
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
- CPU cores and disk speed directly impact upscaling fps: 4 vCPUs + 624 MB/s disk = 2.6 fps vs 16 vCPUs + 1207 MB/s NVMe = 7.0 fps (same RTX 4090). The I/O pipeline needs 8 read + 8 write workers to keep GPU fed. Always request 16+ vCPUs and NVMe >=1000 MB/s.
- CPU single-core speed matters: Xeon Phi (1.4GHz, 272 cores) was 4x slower than EPYC (2.25GHz, 32 cores) with same RTX 5090 GPU because cv2.imread/imwrite bottlenecks on per-core speed. Prefer machines with >2GHz per-core.
- RTX 5090 (Blackwell, sm_120) needs PyTorch 2.6+ with CUDA 12.8. The `pytorch:2.1.0-cuda12.1` Docker image must be upgraded: `pip install torch torchvision --index-url https://download.pytorch.org/whl/cu128`. Also patch basicsr: `sed -i 's/functional_tensor/functional/' .../degradations.py` and suppress tile spam: `sed -i "s/print(f'.*Tile/pass  # /" .../realesrgan/utils.py`. PyTorch 2.10 tested — no speedup over 2.7 for Real-ESRGAN (0.51 vs 0.50 fps).
- **Docker images** for cloud deployment — **always use the slim image as default** for new setups on vast.ai, TensorDock, RunPod, and Packet.ai. All deps pre-installed = no pip install, no patching, no ffmpeg download. Saves 5-8 min setup per instance.
  - `ghcr.io/zdavatz/realesrgan-benchmark:latest` — **slim ~4.5GB, USE THIS** (PyTorch 2.10 + CUDA 12.8 + Real-ESRGAN + ffmpeg + gcc). Default for all new deployments.
  - `ghcr.io/zdavatz/realesrgan-benchmark-full:latest` — full ~8GB (+ TensorRT + ONNX Runtime + g++). Only for optimization benchmarks.
  - `ghcr.io/zdavatz/realesrgan-ncnn-vulkan:latest` — ncnn-vulkan build (reference only, doesn't work on cloud)
  - Built from `nvidia/cuda:12.8.0-runtime-ubuntu24.04` base (not `base` — base lacks CUDA runtime libs, PyTorch falls back to CPU)
- Processing is resumable: each step checks for existing output before re-running
- The `realesrgan-x4plus` model is used for both 2x and 4x upscaling (general-purpose, best for real-world content)
- GFPGAN is DISABLED — it hallucinates facial features and changes how people look. Not suitable for documentary footage. Real-ESRGAN alone provides good upscaling.
- Video reassembly uses libx264 with CRF 18 (visually lossless) and copies original audio stream
- Frame extraction uses parallel ffmpeg workers (up to 16) when multiple CPUs are available. Always enable parallel extraction on new instances.
- Auto-tiling for VRAM management: RTX 4090 (24GB) and RTX 5090 (32GB) both safe up to 1.6 MP without tiling. Above 1.6 MP, tile=512 is used automatically. L40S/A6000 (48GB) up to 2.0 MP, 80GB+ up to 4.0 MP. Tiling is **faster** than no-tile for high-res: at 1920x1200 (2.3 MP), tile=512 = 0.56 fps vs no-tile = 0.13 fps — even on 96GB GPUs where VRAM isn't the bottleneck. RealESRGAN processes at 4x internally, so one huge 7680x4800 image is slower than 12 small tiles.
- GPU power limit matters for Real-ESRGAN: RTX Pro 6000 WS "Max-Q" (300W) = 0.44 fps vs RTX 5090 (575W) = 0.56 fps vs RTX Pro 6000 S Server (600W) = 0.62 fps at same 3090 MHz clock. Pre-flight check warns about <400W for >30GB VRAM GPUs.
- **ncnn-vulkan does NOT work on cloud GPUs**: tested both pre-built binary (2022, doesn't know Blackwell) and self-compiled from source. Vulkan ICD fails in standard Docker containers (`vkCreateInstance failed -9`). Even with `NVIDIA_DRIVER_CAPABILITIES=all` in a custom Docker image (`ghcr.io/zdavatz/realesrgan-ncnn-vulkan`), ncnn-vulkan runs at 0.005 fps (202s/frame) vs PyTorch/CUDA at 0.62 fps — ncnn falls back to CPU computation despite Vulkan detecting the GPU. **Always use PyTorch/CUDA for cloud upscaling.**
- Two GPU profiles for the davaz.com collection: **SD-4x** (RTX 4090, 24GB, ≤1.6 MP, 7.0 fps, $0.50/hr with 16 vCPUs) and **HD-2x** (RTX 5090, 32GB, 0.5-1.7 fps depending on resolution, $0.69/hr). The script auto-detects tiling need per video.
- **Datacenter GPUs are NOT suitable** (except GH200): RTX Pro 6000 S Server 600W = 0.6 fps ($3.41/hr, 5x teurer pro Frame als 5090), B200 179GB = 0.57 fps ($3.13/hr), H100 PCIe 80GB = 0.46 fps ($1.90/hr, nur 147W/350W draw), L40S 48GB = 0.3 fps, RTX Pro 6000 WS Max-Q 300W = 0.44 fps, A100 80GB = 0.07 fps. No-tile is always slower than tile=512 regardless of VRAM. Multi-GPU bringt nichts — Real-ESRGAN nutzt nur 1 GPU.
- **GH200 Grace Hopper is the FASTEST single-GPU tested**: 0.74 fps at $2.26/hr (RunCrate). The integrated ARM Grace CPU + NVLink eliminates the PCIe CPU↔GPU bottleneck. ARM64 — x86 Docker images don't work, must install deps directly. Not cost-effective vs RTX 5090 ($0.30-0.76/hr at 0.51 fps).
- **Multi-GPU scales linearly for multiple videos**: 4x RTX 5090 = 1.47 fps combined (4.1x speedup) when processing **different videos** on each GPU ($1.50/hr on vast.ai Sichuan). Each GPU runs its own `enhance_gpu.py` process via `CUDA_VISIBLE_DEVICES`. Uses `multiprocessing.set_start_method('spawn')` (CUDA requires spawn, not fork). Dashboard supports multi-GPU: per-GPU logs, combined fps, temp/util/VRAM.
- **Multi-GPU does NOT help for single large videos**: Splitting one HD video (1920x1200) across 4 GPUs gives only ~0.3 fps combined (vs ~0.4 fps single GPU) due to I/O bottleneck — 4 GPUs writing 28MB PNGs simultaneously saturates disk I/O regardless of NVMe speed. Tested on Taiwan (slow disk) and Alberta (fast NVMe >3000 MB/s) — same result. **Always use 1 GPU per video** for HD content, multiple single-GPU instances for parallel processing.
- **RTX 5090 optimization benchmarks** (1920x1200, 2x): tile=512 is optimal (0.45 fps), tile=256/384/768 slightly slower. FP16 is 67% faster than FP32 (already used). tile_pad=0 gives 7% speedup but risks tile-boundary artifacts. torch.compile() not compatible with Real-ESRGAN tiling. cudnn.benchmark + TF32 have no effect. GPU only draws 208W of 575W at 1920x1200 — Real-ESRGAN is framework/CPU-limited, not GPU-limited. Per-resolution fps: SD 640x480 = 3.27 fps (340W), HD 960x720 = 1.47 fps (281W), HD 1920x1200 = 0.44 fps (208W).
- Web dashboard (`status_server.py`) shows progress bars, input/output filenames, side-by-side frame comparison, system specs (GPU/CPU/RAM/disk), instance metadata (cost, location, provider), and cost remaining estimate. On TensorDock, served via nginx reverse proxy (port 8080 → status_server.py on 8081) for reliability. On vast.ai, use `bore.pub` tunnel for HTTP access (direct ports often blocked by host firewall).
- Instance metadata stored in `~/instance_meta.json` (label, location, cost_per_hr, provider, instance_id) — displayed in dashboard header.

## Cloud GPU Deployment

- **vast.ai**: Use slim Docker image `ghcr.io/zdavatz/realesrgan-benchmark:latest` — all deps pre-installed, boots in ~1-4min (vs ~8min pip install on TensorDock). For RTX 5090/Blackwell, use `pytorch/pytorch:2.7.0-cuda12.8-cudnn9-runtime` with onstart script. SSH access via `vastai` CLI. Cheapest option (~$0.27-0.54/hr for RTX 4090). Request >=700GB disk for SD videos, >=2TB for long HD films (1920x1200 @ 2x needs ~1.8TB). Use `vast_batch.sh` for automated batch processing. API key stored in `~/.zshrc` as `VAST_API_KEY`. When choosing instances: check CPU clock speed (>2GHz), vCPUs >=16, disk space, and PCIe gen — not just GPU and price.
- **TensorDock**: SSH VMs via API (`dashboard.tensordock.com/api/v2`). Auth: `Authorization: Bearer $TENSORDOCK_API_KEY`. Ubuntu 24.04 bare-metal (no Docker — direct pip install). Default SSH user is `user` (not root). API key stored in `~/.bashrc` as `TENSORDOCK_API_KEY`. Organization: old2new. Cloud-init `bootcmd` disables `unattended-upgrades` and `apt-daily` timers to prevent Ubuntu from wasting 5-10 min on dist-upgrade at first boot.
  - **Pip-based setup** (not Docker — slim Docker image fails on hosts with CUDA <12.8, e.g. Orlando has 12.7). Cloud-init installs: `python3-pip` + `xz-utils` from apt, then pip installs PyTorch (cu121 for Ada/Ampere, cu128 for Blackwell), realesrgan, yt-dlp, etc. Static ffmpeg 7.x downloads in parallel with pip.
  - **Ubuntu 24.04 typing_extensions fix**: Must `pip install --ignore-installed typing_extensions` before `pip install torch` — Ubuntu's dpkg-installed version blocks pip from upgrading it, causing torch install to fail with "Cannot uninstall typing_extensions, RECORD file not found".
  - `tensordock_batch.sh` auto-calculates disk size via `yt-dlp --dump-json` (exact resolution, 2.5x PNG compression, 20% safety margin). Auto-detects tiling risk: HD videos (>1.6 MP) auto-switch to RTX 5090, refuses to launch on RTX 4090 (tiling = 8x slower).
  - Port forwarding maps internal 22→random and 8080→random external ports. Disk resize requires stop→modify→start (GPU may detach — always create with correct size from the start).
  - RTX 5090 support: auto-switches or override with `GPU_MODEL=geforcertx5090-pcie-32gb`. Auto-detects Blackwell arch (sm_120+) and installs PyTorch with CUDA 12.8.
  - **Proven profile SD-4x**: RTX 4090, Ottawa/Orlando, 650-700GB, 2.6-2.9 fps, $0.41-0.50/hr. Queue multiple videos with frame cleanup between jobs.
  - **Proven profile HD-2x**: RTX 5090, Chubbuck Idaho, 1700-3000GB, ~$0.70-0.80/hr. Only location with 5090 + large storage. ~1700GB for 1h HD, ~3000GB for 2h HD.
- **RunPod**: NOT WORKING as of 2026-03-18. Pods show "RUNNING" but never actually start (uptime stays 0, no ports assigned). Tested with RTX Pro 6000, RTX 5090, multiple datacenters (EU-RO-1, US-KS-2), various images (pytorch, nvidia/cuda, ubuntu:22.04), with/without network volumes, REST and GraphQL APIs. All pods stuck indefinitely. Platform-level issue — not a configuration problem. `runpod_launch.sh` script exists but is unusable until RunPod fixes this. API key stored in `~/.bashrc` as `RUNPOD_API_KEY`.
- **Google Cloud**: Use `gcp_setup.sh` for automated setup. Image: `pytorch-2-7-cu128-ubuntu-2204-nvidia-570`, machine: `g2-standard-4` + L4 GPU. Requires GPUS_ALL_REGIONS quota increase for new projects.
- **Packet.ai**: GPU aggregator (by hosted.ai). API: `https://dash.packet.ai/api/v1/inventory`. Auth: `Authorization: Bearer $PACKET_API_KEY`. Lists offers from multiple providers (voltage-park, shadeform, hyperstack, etc.). GPUs: B300 SXM6 (262GB, ~$3.45-10/hr), H100 SXM5 (80GB, ~$0.92-2.88/hr), RTX Pro 6000 (96GB, ~$0.83-1.96/hr). Requires $50 minimum wallet balance, 25-min minimum runtime. **DEPLOYMENT API BROKEN as of 2026-03-18**: POST /deployments returns INTERNAL_ERROR for all offers, all regions, all provider values. Offers list correctly via GET /offers but cannot be deployed. Bug report sent to hello@packet.ai. API key stored in `~/.bashrc` as `PACKET_API_KEY`. SSH key ID: `69bacf06d469cdd843eb487c`.
- **Lambda Labs**: API: `https://cloud.lambdalabs.com/api/v1`. Auth: `Authorization: Bearer $LAMBDA_API_KEY`. **NOT SUITABLE**: (1) Zero capacity — all instance types perpetually sold out (all `regions_with_capacity_available` empty as of 2026-03-18). (2) No consumer GPUs — only datacenter GPUs (A10, A100, H100, B200, GH200), no RTX 4090/5090. (3) Expensive — $1.48-$6.08/hr for single GPU vs $0.34-0.69/hr on vast.ai/TensorDock. Datacenter GPUs are proven unsuitable for Real-ESRGAN (see benchmarks). API key stored in `~/.bashrc` as `LAMBDA_API_KEY`.
- **Hyperstack**: GPU cloud by NexGenCloud. API: `https://infrahub-api.nexgencloud.com/v1`. Auth header: `api_key: $HYPERSTACK_API_KEY`. Bare-metal VMs with Docker support. Only datacenter GPUs (H100, A100, L40, A6000 — no RTX 4090/5090). H100 PCIe benchmarked: 0.46 fps at $1.90/hr (147W/350W draw) = not cost-effective vs RTX 5090. Datacenters: CANADA-1, NORWAY-1, US-1. SSH keys: `zenogentoo-ca` (CANADA-1), `zenogentoo` (NORWAY-1). Use `Ubuntu Server 22.04 LTS R570 CUDA 12.8 with Docker` image + `docker run --gpus all ghcr.io/zdavatz/realesrgan-benchmark:latest`.
- **RunCrate**: GPU aggregator (runcrate.ai). No REST API — dashboard only (app.runcrate.ai). Custom Docker images supported. Tested: RTX Pro 6000 S (0.62 fps, works with slim Docker image), GH200 Grace Hopper (0.74 fps — fastest GPU tested, ARM64 so no x86 Docker). GPUs: RTX 4090 $0.36/hr, RTX 5090 $0.55/hr, GH200 $2.26/hr, B200 $3.40/hr. $10 free credits for new users.
- API keys stored in `~/.bashrc` as `TENSORDOCK_API_KEY`, `RUNPOD_API_KEY`, `PACKET_API_KEY`, `LAMBDA_API_KEY`, `HYPERSTACK_API_KEY`, and `RUNCRATE_API_KEY`, in `~/.zshrc` as `VAST_API_KEY`
- Google Cloud projects: old2new-490311 (zdavatz@ywesee.com), old2new-davaz (juerg@davaz.com)

## Cloud Python Dependency Fixes

The `realesrgan` package has version conflicts on many cloud images:
- **Ubuntu 24.04 typing_extensions**: `pip install --ignore-installed typing_extensions` BEFORE installing torch — Ubuntu's dpkg version blocks pip upgrade
- `numpy==1.26.4` required (numpy 2.x breaks basicsr)
- `torchvision==0.15.2` and `basicsr==1.4.2` needed if torchvision is too new (missing `functional_tensor`)
- Must uninstall `opencv-python` AND `opencv-contrib-python` before installing `opencv-python-headless<4.11` (4.11+ requires numpy>=2)
- On cloud images, install `libgl1` and `libglib2.0-0` system packages
- PyTorch Docker image ships ffmpeg 4.3 which can't merge webm — install static ffmpeg 7.x from johnvansickle.com (replace `/opt/conda/bin/ffmpeg`)
- **Prefer slim Docker image** (`ghcr.io/zdavatz/realesrgan-benchmark:latest`) over pip install — avoids all dependency conflicts

## Dependencies

Local (Homebrew): `yt-dlp`, `ffmpeg`, `bc`
Cloud (pip/apt): `realesrgan`, `yt-dlp`, `numpy<2`, `torchvision==0.15.2`, `basicsr==1.4.2`, `opencv-python-headless`, `ffmpeg` (static binary on GCP)
Dashboard: `bore` (for vast.ai HTTP tunneling, installed from github.com/ekzhang/bore)
Status check: `google-api-python-client`, `google-auth-oauthlib` (for YouTube API in check_enhanced.py/fetch_missing_videos.py)
