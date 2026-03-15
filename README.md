# old2new

Improve video quality of old Da Vaz movies using [Real-ESRGAN](https://github.com/xinntao/Real-ESRGAN) AI upscaling.

## Requirements

- macOS (Apple Silicon)
- [yt-dlp](https://github.com/yt-dlp/yt-dlp) — `brew install yt-dlp`
- [ffmpeg](https://ffmpeg.org/) — `brew install ffmpeg`
- bc — `brew install bc` (usually pre-installed)

Real-ESRGAN is downloaded automatically on first run.

## Usage

```bash
./enhance.sh <youtube-url>
```

The script will:
1. Download the video via yt-dlp
2. Detect your machine specs (chip, GPU cores, RAM)
3. Benchmark one frame at 2x and 4x to estimate processing time
4. Present enhancement options with time estimates:
   - **Option 1**: Minimal enhance (2x upscale)
   - **Option 2**: Maximum enhance (4x upscale)
5. Extract frames, upscale with Real-ESRGAN, and reassemble with original audio

### Example

```bash
./enhance.sh https://www.youtube.com/watch?v=rX4ADnOa3G4
```

### Long-running jobs

For long videos, run in the background:

```bash
# Non-interactive mode: pass scale as second argument
# (skips the menu)
nohup ./enhance.sh 'https://www.youtube.com/watch?v=xyz' > enhance.log 2>&1 &

# Monitor progress
tail -f enhance.log
```

### Output

Enhanced videos are saved to `jobs/<video-id>/enhanced_<scale>x_<width>x<height>.mkv`.

The process is resumable — if interrupted, re-run the same command and it will skip already-completed steps (download, frame extraction, upscaled frames).

## Performance Reference

| Chip | GPU Cores | ~Time per frame | 11 min video (~16k frames) |
|------|-----------|-----------------|---------------------------|
| M5 (base) | 10 | ~10s | ~45h |
| M3 Ultra | 76 | ~1.5-2s | ~7-9h |

## License

GPL-3.0
