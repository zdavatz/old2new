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

- **enhance.sh** — macOS script: YouTube URL or local file → detect hardware → benchmark → check disk → interactive menu → extract frames (ffmpeg) → upscale (Real-ESRGAN ncnn-vulkan) → reassemble (ffmpeg)
- **enhance_gpu.py** — Cloud GPU script: same pipeline but PyTorch/CUDA. Pre-flight check (GPU, CPU, RAM, disk, PCIe, software). Parallel frame extraction (up to 16 ffmpeg workers). Parallel I/O pipeline (threaded pre-read + async write). Auto-tiling based on VRAM. Supports `--job-name`. Uses `~/jobs/<name>/`.
- **gcp_setup.sh** — One-command Google Cloud setup: pre-checks video size → creates L4 GPU instance → installs deps → starts enhancement. Supports `status` with ETA.
- **vast_batch.sh** — vast.ai script. Supports: YouTube URL (single video), `test`, `launch N` (batch 226 videos on N instances). Also: `status`, `download`, `destroy`, `list`. Auto-detects HD → recommends 2x. Web dashboard via bore.pub tunnel.
- **runpod_launch.sh** — RunPod script (BROKEN — pods never start). Commands: `test`, `launch`, `status`, `ssh`, `download`, `destroy`, `destroy-all`, `list`.
- **tensordock_batch.sh** — TensorDock API SSH VMs. Auto-calculates disk from resolution x duration x scale. Auto-detects tiling risk → switches to RTX 5090 for HD. Commands: `test [VIDEO_ID]`, `launch N`, `status`, `ssh N`, `download`, `destroy`, `list`. Cloud-init installs all deps, disables unattended-upgrades (~3min vs ~12min boot). Deploys OAuth credentials via cloud-init write_files for auto YouTube upload + email. Default user `user`. Port forwarding SSH (22) and dashboard (8080).
- **youtube_upload.py** — Uploads enhanced video to YouTube (copies title + "Enhanced 4K" suffix), sends email to juerg@davaz.com via Gmail API. Requires `client_secret.json` + `youtube_token.json` (OAuth2 youtube.upload + gmail.send). For video IDs starting with dash, use `--video-id=-x_aIkSrXFw`. **YouTube API Quota**: 10,000 units/day (resets midnight Pacific = 07:00 UTC). ~150 units/upload → ~66 uploads/day. Monitor at [Google Cloud Console](https://console.cloud.google.com/apis/api/youtube.googleapis.com/quotas). `quotaExceeded` → MKVs stay for retry. `retry_uploads.sh` scans for unuploaded MKVs, scheduled via cron (`5 7 * * *`) or sleep-based timer.
- **youtube_upload_rs/** — Rust replacement for `youtube_upload.py`. Binary deployed as `~/youtube_upload`. Same functionality + appends to `~/upload_log.jsonl` + updates `timing.json`. Direct token refresh via Google OAuth2. Supports `--title` for custom upload titles (e.g. "Enhanced 4K v1" for brightness-corrected re-uploads). Built with `cargo build --release`, deployed via `deploy.sh update`.
- **check_enhanced.py** — Checks YouTube Data API for existing "(Enhanced 4K)" versions of 226 Da Vaz videos. Saves to `enhanced_status.json`.
- **close_enhanced_issues.py** — Auto-closes GitHub issues for uploaded Enhanced 4K videos. Two-pass YouTube search (keyword + date-ordered). Fuzzy title matching (HTML entities, emoji, possessives, digit spacing). Supports dry-run (default) and `--close`.
- **deploy.sh** — Deploys videos to cloud GPU instances. Also: `update-soft <id>` (upload scripts without restart). Reads `json/<video_id>.json`, determines GPU type, disk/CPU/RAM needs. Modes: `<ids>` (auto), `new <ids>`, `--instance <ID> <ids>`, `--plan <ids>`, `update <instance_id>` (redeploy + restart), `destroy <instance_id>`. Options: `--single`/`--multi`, `--vastai`/`--tensordock`. Safety: interactive selection, Ctrl+C auto-destroys, 10-min timeout. CPU verification: vast.ai boost clock != actual (searches `cpu_ghz>=4.0` for 5090, verifies via SSH). Blacklists bad machines to `blacklist_machines.txt`. Shows CPU model, release year, disk I/O, Docker cache status. Time estimates with CPU-adjusted parallel benchmark. Dashboard HTTP proxy test with SSH tunnel fallback.
- **restart.sh** — Restarts all services: kills old processes, replaces `.new` binaries, restores `.processing` queue files, starts status_server + multi_gpu_queue.sh. Usage: `./restart.sh [NUM_GPUS]`.
- **gpu_scheduler** (Rust) — Replaces `multi_gpu_queue.sh`. Single binary that manages the entire GPU pipeline: scans queue (`~/json/*.json`), counts GPUs, distributes videos **proportionally** (2 videos + 4 GPUs = 2 GPUs per video), starts `upscale.py` with `--start/--end` segments + `--gpu-id` in parallel, waits for completion, runs frame-gap-check + reassembly (ffmpeg + brightness matching) + upload per video. **Safety**: upload lock file (`.uploaded`) prevents duplicate uploads. Frames only deleted when more videos in queue; last video keeps frames for manual YouTube verification. Auto-destroy checks for running uploads/ffmpeg and unuploaded MKVs before destroying. **Nearly-done detection**: if video has <2000 frames remaining (<5%), GPU 0 finishes it while other GPUs start next video. Frame count verification gate before reassembly (CRITICAL: prevents corrupt uploads). **IMPORTANT**: always deploy via `deploy.sh update`, never manually start processes alongside existing ones. Usage: `./gpu_scheduler [NUM_GPUS]`.
- **multi_gpu_queue.sh** — Legacy Bash queue processor (fallback if `gpu_scheduler` binary not available). Queue from `~/json/*.json`. Atomic pick via flock. 1 GPU per video only — no proportional distribution.
- **enhance.sh** (cloud version) — Bash orchestrator replacing enhance_gpu.py. Usage: `./enhance.sh <youtube-url> <scale> [--job-name <title>] [--gpu N]`. Also: `./enhance.sh done <json-path>`. Phases: parse args → job_meta.json → pre-flight → download → extract → upscale.py → frame gap check → reassemble → black frame check (optional, `CHECK_BLACK=0`) → upload → cleanup. `--gpu N` sets CUDA_VISIBLE_DEVICES. Upload uses `$HOME/youtube_upload` Rust binary (falls back to Python). `job_meta.json` written via `sys.argv` for special chars. PATH includes `/opt/venv/bin` and `/opt/conda/bin`.
- **upscale.py** — Minimal Python (~100 lines), ONLY Real-ESRGAN upscaling. Usage: `python3 upscale.py <frames_in> <frames_out> <scale> [--tile N] [--gpu-id N]`. Auto-tiling (tile=512 for >1.6 MP), deletes input frames in single-GPU mode (keeps last 10). In segment-split mode (`--end > 0`), inputs NOT deleted (shared dir). `--gpu-id N` creates GPU-specific tmp files (`frame_XXXXX.tmp.gpuN.png`) to avoid collisions when multiple GPUs write to the same frames_out dir. Startup cleanup only removes own GPU's tmp files. **Important**: tmp files must use `.tmp.png` (not `.png.tmp`) — OpenCV determines format from extension.
- **pimp_brightness.sh** — End-to-end brightness correction: download from YouTube → adjust segment → re-upload. Only re-encodes the affected segment (gamma correction), stream-copies the rest. Usage: `./pimp_brightness.sh <youtube-url> <start> <end> <percent> [--suffix v1] [--title "Custom Title"]`. Percent: `-25` = 25% darker, `+30` = 30% brighter. `--suffix v2` replaces any existing "(Enhanced 4K ...)" suffix → "Enhanced 4K v2". Uploads via Rust `youtube_upload` binary, sends email to juerg@davaz.com.
- **check_video_fit.sh** — Pre-deploy validation. Usage: `./check_video_fit.sh <youtube-url> <user>@<host> <port>`. Validates GPU, CPU, RAM, disk.
- **fetch_video_json.sh** — Fetches video metadata via `yt-dlp --dump-json`, saves to `json/<video_id>.json`. Skips existing (safe to re-run).
- **json/** — Per-video JSON metadata. Status server reads for queue display. Title lookup: `job_meta.json` → `json/{id}.json` → directory name.
- **fetch_missing_videos.py** — Fetches resolution for non-enhanced videos via yt-dlp. GPU by megapixels (≤1.6 MP → 4090, >1.6 MP → 5090). Saves to `not_enhanced.json`.
- **not_enhanced.json** — All Da Vaz videos without Enhanced 4K. Fields: youtube_id, title, duration, definition, width, height, megapixels, scale, gpu.
- **not_enhanced_rtx4090.json** — SD videos needing RTX 4090 (4x). 72 videos, 24.8h.
- **not_enhanced_rtx5090.json** — HD videos needing RTX 5090 (2x). 143 videos, 33.5h. Includes 960x720 and 720x1280 (HD-defined despite low res).
- **realesrgan/** — Auto-downloaded binary and models (gitignored). macOS ARM64.
- **jobs/<video_id>/** — Per-video work dirs using video IDs (not titles — special chars break paths). Files: `<id>.mkv` (input), `<id>_4x.mkv` (output), `frames_in/`, `frames_out/` (gitignored).

## Important

- URLs must be quoted (`./enhance.sh "https://..."`) — `?` is a glob in zsh
- `ffprobe -print_format flat` outputs dots in names → convert via `sed 's/\./_/g'` before `eval`
- `enhance_gpu.py` uses `os.path.expanduser("~")` — never hardcode `/root/jobs`
- `vast_batch.sh` embeds all 226 video IDs, durations, definitions, titles (from davaz2 MySQL + YouTube API)
- GCP: broken apt ffmpeg → use static binary from johnvansickle.com. Disk sized before creation, default 500GB SSD quota.
- 4x upscale of HD (1080p) needs ~650GB. Recommend 2x for HD.

## Key Details

- Local: ncnn-vulkan uses Vulkan (Apple Silicon via MoltenVK). Cloud: ncnn-vulkan does NOT work in Docker (no Vulkan driver, falls back to CPU at 0.005 fps). **Always use PyTorch/CUDA for cloud.**
- Pre-flight validates: GPU CUDA arch, PyTorch/CUDA versions, CPU benchmark, RAM, disk + I/O, PCIe, ffmpeg, package versions.
- Pre-download disk check via `yt-dlp --dump-json` (no download) to estimate needs. Aborts if insufficient.
- CPU/disk impact: 4 vCPUs + 624 MB/s = 2.6 fps vs 16 vCPUs + 1207 MB/s NVMe = 7.0 fps (same RTX 4090). Always request 16+ vCPUs and NVMe >=1000 MB/s.
- CPU single-core matters: Xeon Phi 1.4GHz = 4x slower than EPYC 2.25GHz with same GPU. cv2.imread/imwrite bottlenecks on per-core speed.
- **RTX 5090 HD CPU benchmarks** (1920x1200, 2x, tile=512): Xeon Gold 6530 @ 0.9 GHz = ~0.1 fps (GPU idle!), Xeon 8481C @ 2.0 GHz = 0.3-0.4 fps, Ryzen 9 7950X @ 5.9 GHz = 0.5 fps, Threadripper PRO 7975WX @ 8.2 GHz = 0.5 fps, **Ryzen 9 9950X = 0.5 fps/GPU (best)**, Threadripper PRO 9975WX = 0.5 fps/GPU (slower than 9950X despite 64 cores — 4 CCDs = higher Infinity Fabric latency). Core count doesn't matter (16 cores sufficient), single-core speed + memory latency matters. **Min 3 GHz for RTX 5090 HD, ideal 5+ GHz.**
- RTX 5090 (Blackwell, sm_120) needs PyTorch 2.6+ / CUDA 12.8. Patch basicsr: `sed -i 's/functional_tensor/functional/' .../degradations.py`. Suppress tile spam: `sed -i "s/print(f'.*Tile/pass  # /" .../utils.py`. PyTorch 2.10 = no speedup over 2.7.
- **Docker images** — **always use slim image as default**. All deps pre-installed = no pip, no patching. Saves 5-8 min/instance.
  - `ghcr.io/zdavatz/realesrgan-benchmark:latest` — **slim ~4.5GB, USE THIS** (PyTorch 2.10 + CUDA 12.8 + Real-ESRGAN + ffmpeg + gcc). Requires driver >=570.
  - `ghcr.io/zdavatz/realesrgan-benchmark-compat:latest` — **compat ~4.5GB** (PyTorch 2.1 + CUDA 12.1 + pre-built Rust binaries for glibc 2.35). For older drivers (>=530). `deploy.sh` auto-selects based on driver version.
  - `ghcr.io/zdavatz/realesrgan-benchmark-full:latest` — full ~8GB (+ TensorRT + ONNX). Only for benchmarks.
  - `ghcr.io/zdavatz/realesrgan-ncnn-vulkan:latest` — ncnn-vulkan (reference only, doesn't work on cloud)
  - Built from `nvidia/cuda:12.8.0-runtime-ubuntu24.04` (not `base` — base lacks CUDA runtime). Compat from `nvidia/cuda:12.1.1-runtime-ubuntu22.04`.
- Processing is resumable: checks existing output. **Fast resume**: existing frames → skip preflight/download/extraction → straight to upscaling.
- `realesrgan-x4plus` model for both 2x and 4x (general-purpose, best for real-world content)
- GFPGAN DISABLED — hallucinates facial features. Not suitable for documentary footage.
- Reassembly: libx264, CRF 18, `-preset medium`. **Auto brightness matching**: samples 10 frames, compares brightness, applies gamma correction. `-preset slow` abandoned (identical quality at CRF 18, YouTube re-encodes anyway, 2x slower).
- Parallel frame extraction (up to 16 ffmpeg workers).
- **Auto-tiling**: RTX 4090/5090 safe up to 1.6 MP without tiling. Above 1.6 MP → tile=512 (always, regardless of VRAM). L40S/A6000 (48GB) up to 2.0 MP, 80GB+ up to 4.0 MP. Tiling **faster** for high-res: 1920x1200 tile=512 = 0.56 fps vs no-tile = 0.13 fps (RealESRGAN processes at 4x internally). **Do NOT increase tile size** — larger tiles use more VRAM but are slower (see tile size benchmark).
- GPU power matters: RTX Pro 6000 Max-Q 300W = 0.44 fps, RTX 5090 575W = 0.56 fps, RTX Pro 6000 S 600W = 0.62 fps. Pre-flight warns <400W for >30GB VRAM.
- Two GPU profiles: **SD-4x** (RTX 4090, 24GB, ≤1.6 MP, 7.0 fps, $0.50/hr) and **HD-2x** (RTX 5090, 32GB, 0.5-1.7 fps, $0.69/hr).
- **Datacenter GPUs NOT suitable** (except GH200): RTX Pro 6000 S = 0.6 fps/$3.41, B200 = 0.57 fps/$3.13, H100 = 0.46 fps/$1.90, L40S = 0.3 fps, A100 = 0.07 fps. No-tile always slower than tile=512.
- **GH200 Grace Hopper FASTEST single-GPU**: 0.74 fps at $2.26/hr (RunCrate). ARM64 — no x86 Docker. Not cost-effective vs RTX 5090 ($0.30-0.76/hr at 0.51 fps).
- **Multi-GPU scales linearly**: 4x RTX 5090 = 1.47 fps combined ($1.35/hr). Each GPU runs own process via CUDA_VISIBLE_DEVICES.
- **Multi-GPU queue**: shared queue with `flock` for atomic access. Avoids batch-of-N anti-pattern (GPUs idle waiting for slowest).
- **OOM-Kill recovery**: exit code > 128 = signal → retry same video after 60s (not skip). Max 3 retries. Normal failures (exit 1) skip.
- **Resume worker monitor** (`resume_workers.sh`): checks every 30s, uses PID-files per GPU. Do NOT use `/proc/$pid/environ` (race condition).
- **Don't let ffmpeg block GPU**: start next video on free GPU while ffmpeg reassembles on another.
- **Disk estimates use peak, not sum**: `max(input, output)` because upscale.py deletes inputs during upscaling. Warns when single video >60% of total disk.
- **Resume-aware disk check**: accounts for existing frames on resume. Existing frames_in counted as reclaimable.
- **Clean already-upscaled frames_in**: on constrained disks, delete redundant frames_in with corresponding frames_out.
- **Multi-GPU segment splitting**: `enhance.sh` splits frames across GPUs, runs `upscale.py --start N --end M`. Linear speedup. Deploy: `./deploy.sh --multi-one [N] <video_id>`.
- **Dynamic GPU joining**: idle GPU auto-joins remaining video by taking tail segment.
- **Rebalance**: `./deploy.sh rebalance <instance_id>` redistributes frames across all GPUs. Runs via nohup.
- **Multi-GPU disk sizing**: 500GB / 4 GPUs = 125GB each. 1920x1200 2x: max ~5min/video. 960x720 2x: max ~17min.
- **yt-dlp JS challenge** (since ~March 2026): requires `--remote-components ejs:github` for deno JS solver. Without it: "This video is not available". Slim Docker image includes deno.
- **Watchdog**: checks every 60s if enhance_gpu.py is running, restarts if not (resumable).
- **RTX 5090 optimization**: tile=512 optimal, FP16 67% faster than FP32 (already used), tile_pad=0 = 7% but risks artifacts, torch.compile() incompatible. GPU draws only 208W/575W at 1920x1200 — framework/CPU-limited. Per-res fps: SD 640x480 = 3.27 fps (340W), HD 960x720 = 1.47 fps (281W), HD 1920x1200 = 0.44 fps (208W). **Tile size benchmark** (RTX 5090, 1920x1200 2x): tile=512 = 0.4 fps (6.5GB VRAM), tile=768 = 0.3 fps (22GB), tile=1024 = 0.2 fps (29.7GB). Larger tiles use more VRAM but are slower — 4x internal tensors exceed GPU L2 cache. **Do NOT try**: ONNX Runtime CUDA/TensorRT (fails on Blackwell sm_120, TRT recompiles engine per tile size), Python threading/prefetch (GIL blocks, slower than sequential), larger tiles (cache thrashing). Sequential Python tile=512 is the proven optimum — only multi-GPU scaling helps.
- **status_server_rs/** — Rust status server binary `~/status_server` (~1.5 MB). Auto-scans `~/jobs/` + `~/json/`. 7 phases: downloading, extracting, upscaling, assembling, uploading, paused, done. Per-phase progress with fps, frame counts, ffmpeg speed, upload %, download %. Reads titles from `json/{id}.json`. Endpoints: `/` (HTML), `/api/status` (JSON), `/compare/{title}`, `/frames/{title}/{dir}/{file}`, `/download/{title}`. **GLIBC**: cross-compiled binary needs glibc 2.39. Slim image works; compat image has pre-built binaries for glibc 2.35; older images need on-server build.
- **SSH nohup detach**: use `sudo bash -c "cd /root && nohup cmd >> log 2>&1 &"` — `bash -c` wrapper ensures proper detach from SSH session.
- Instance metadata in `~/instance_meta.json` (label, location, cost_per_hr, provider, instance_id) — shown in dashboard.

## Cloud GPU Deployment

- **vast.ai**: Slim Docker image, boots 1-4min. SSH via `vastai` CLI. ~$0.27-0.54/hr RTX 4090. >=700GB for SD, >=2TB for long HD. API key in `~/.zshrc` as `VAST_API_KEY`. **ALWAYS use verified hosts** (`verified=true`) — unverified kill processes after ~1h. CPU boost clock != actual: search `cpu_ghz>=4.0` for 5090. Check vCPUs >=16, disk, PCIe. SSH-Key issue (2026-03-21): some instances reject all keys — reuse instances or try different hosts.
- **Video distribution**: ≤1.6 MP videos → multi-GPU RTX 4090 (faster + cheaper). 1920x1200 → RTX 5090. Pre-extract one video at a time (37 parallel extractions dropped fps from 0.5 to 0.2).
- **TensorDock**: SSH VMs via API (`dashboard.tensordock.com/api/v2`). Auth: `Bearer $TENSORDOCK_API_KEY`. Ubuntu 24.04 bare-metal, pip-based setup (slim Docker fails on CUDA <12.8). User `user`. Cloud-init disables unattended-upgrades. Auto-detects Blackwell → CUDA 12.8. Port forwarding 22→random, 8080→random. Create with correct disk size (resize detaches GPU). **Ubuntu 24.04**: `pip install --ignore-installed typing_extensions` before torch.
  - **Proven SD-4x single**: RTX 4090, Ottawa/Orlando, 650-700GB, 2.6-2.9 fps, $0.41-0.50/hr.
  - **Proven SD-4x multi**: 4x RTX 4090, BC (vast.ai 13428), EPYC 7763 128-core, 251GB RAM, 1646GB NVMe, ~10.5 fps, $1.23/hr.
  - **Proven HD-2x**: RTX 5090, Chubbuck ID, 1700-3000GB, ~$0.70-0.80/hr.
- **Google Cloud**: `gcp_setup.sh`, image `pytorch-2-7-cu128-ubuntu-2204-nvidia-570`, `g2-standard-4` + L4. Needs GPUS_ALL_REGIONS quota.
- **RunPod**: BROKEN as of 2026-03-18 — pods never start. API key in `~/.bashrc` as `RUNPOD_API_KEY`.
- **Packet.ai**: DEPLOYMENT API BROKEN as of 2026-03-18. API key in `~/.bashrc` as `PACKET_API_KEY`.
- **Lambda Labs**: NOT SUITABLE — no capacity, no consumer GPUs, expensive. API key in `~/.bashrc` as `LAMBDA_API_KEY`.
- **Hyperstack**: Only datacenter GPUs (no RTX 4090/5090), not cost-effective. API key in `~/.bashrc` as `HYPERSTACK_API_KEY`.
- **RunCrate**: Dashboard only (app.runcrate.ai). RTX 4090 $0.36/hr, RTX 5090 $0.55/hr, GH200 $2.26/hr. $10 free credits. API key in `~/.bashrc` as `RUNCRATE_API_KEY`.
- Google Cloud projects: old2new-490311 (zdavatz@ywesee.com), old2new-davaz (juerg@davaz.com)

## Cloud Python Dependency Fixes

**Prefer slim Docker image** (`ghcr.io/zdavatz/realesrgan-benchmark:latest`) — avoids all below. For manual installs: `pip install --ignore-installed typing_extensions` (Ubuntu 24.04), `numpy==1.26.4` (2.x breaks basicsr), `torchvision==0.15.2` + `basicsr==1.4.2`, uninstall opencv-python/contrib before `opencv-python-headless<4.11`, install `libgl1` + `libglib2.0-0`, replace ffmpeg with static 7.x from johnvansickle.com.

## Dependencies

Local (Homebrew): `yt-dlp`, `ffmpeg`, `bc`
Cloud (pip/apt): `realesrgan`, `yt-dlp`, `numpy<2`, `torchvision==0.15.2`, `basicsr==1.4.2`, `opencv-python-headless`, `ffmpeg` (static binary on GCP)
Dashboard: `bore` (for vast.ai HTTP tunneling)
Status check: `google-api-python-client`, `google-auth-oauthlib` (YouTube API)
