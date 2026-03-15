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

### Cloud GPU (manual setup via vast.ai / RunPod)

For faster processing, use `enhance_gpu.py` on a cloud GPU instance with CUDA + PyTorch:

```bash
# On a cloud instance with CUDA:
pip install realesrgan yt-dlp "numpy<2" "torchvision==0.15.2" "basicsr==1.4.2" opencv-python-headless
apt-get install -y ffmpeg
python3 enhance_gpu.py "<youtube-url>" [scale]
```

### Output

Enhanced videos are saved to `jobs/<video-id>/enhanced_<scale>x.mkv`.

The process is resumable — if interrupted, re-run the same command and it will skip already-completed steps (download, frame extraction, upscaled frames).

## Performance Reference

| Hardware | Per frame | 11 min video (~16k frames) | Cost |
|----------|-----------|---------------------------|------|
| M5 (10 GPU cores) | ~10s | ~45h | — |
| M3 Ultra (76 GPU cores) | ~1.5-2s | ~7-9h | — |
| RTX 4090 (vast.ai, CUDA) | ~1s | ~4.5h | ~$1.25 |
| L4 (Google Cloud, CUDA) | ~2s | ~9h | ~$6.30 |
| H200 NVL (RunPod, CUDA) | ~1-1.5s | ~5-6h | ~$17-20 |

### Cloud GPU Providers

- **[vast.ai](https://vast.ai)**: Cheapest option. RTX 4090 at ~$0.20-0.28/hr. CLI: `pipx install vastai`
- **[RunPod](https://runpod.io)**: More GPU variety but often sold out. RTX 4090 at ~$0.34-0.59/hr. CLI: `pipx install runpod`
- **[Google Cloud](https://cloud.google.com)**: Always available. L4 at ~$0.70/hr. Use `gcp_setup.sh` for one-command setup. Requires GPUS_ALL_REGIONS quota for new projects.

**Note**: On cloud instances, use the Python/CUDA approach (`enhance_gpu.py`) instead of the ncnn-vulkan binary, as Vulkan drivers are often not available in Docker containers.

## License

GPL-3.0
