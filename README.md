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
```

**Important**: Always quote the URL to prevent the shell from interpreting `?` as a glob character.

The script will:
1. Download the video via yt-dlp
2. Detect your machine specs (chip, GPU cores, RAM)
3. Benchmark one frame at 2x and 4x to estimate processing time
4. Present enhancement options with time estimates:
   - **Option 1**: Minimal enhance (2x upscale)
   - **Option 2**: Maximum enhance (4x upscale)
5. Extract frames, upscale with Real-ESRGAN, and reassemble with original audio

### Cloud GPU (vast.ai / RunPod)

For faster processing, use `enhance_gpu.py` on a cloud GPU instance with CUDA + PyTorch:

```bash
# On a cloud instance with CUDA:
pip install realesrgan yt-dlp "numpy<2"
apt-get install -y ffmpeg
python3 enhance_gpu.py
```

The Python script uses Real-ESRGAN via PyTorch/CUDA (no Vulkan needed).

### Example

```bash
./enhance.sh "https://www.youtube.com/watch?v=rX4ADnOa3G4"
```

### Long-running jobs

For long videos, run in the background:

```bash
nohup ./enhance.sh 'https://www.youtube.com/watch?v=xyz' > enhance.log 2>&1 &

# Monitor progress
tail -f enhance.log
```

### Output

Enhanced videos are saved to `jobs/<video-id>/enhanced_<scale>x_<width>x<height>.mkv`.

The process is resumable — if interrupted, re-run the same command and it will skip already-completed steps (download, frame extraction, upscaled frames).

## Performance Reference

| Hardware | Per frame | 11 min video (~16k frames) | Cost |
|----------|-----------|---------------------------|------|
| M5 (10 GPU cores) | ~10s | ~45h | — |
| M3 Ultra (76 GPU cores) | ~1.5-2s | ~7-9h | — |
| RTX 4090 (vast.ai, CUDA) | ~3s | ~13h | ~$2.85 |
| H200 NVL (RunPod, CUDA) | ~1-1.5s | ~5-6h | ~$17-20 |

### Cloud GPU Providers

- **[vast.ai](https://vast.ai)**: Cheapest option. RTX 4090 at ~$0.20-0.28/hr. CLI: `pip install vastai`
- **[RunPod](https://runpod.io)**: More GPU variety, higher availability for premium GPUs. RTX 4090 at ~$0.34-0.59/hr. CLI: `pip install runpod`

**Note**: On cloud instances, use the Python/CUDA approach (`enhance_gpu.py`) instead of the ncnn-vulkan binary, as Vulkan drivers are often not available in Docker containers.

## License

GPL-3.0
