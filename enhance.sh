#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ESRGAN="$SCRIPT_DIR/realesrgan/realesrgan-ncnn-vulkan"
MODEL_DIR="$SCRIPT_DIR/realesrgan/models"
ESRGAN_URL="https://github.com/xinntao/Real-ESRGAN/releases/download/v0.2.5.0/realesrgan-ncnn-vulkan-20220424-macos.zip"

# --- Install Real-ESRGAN if missing ---
if [ ! -x "$ESRGAN" ]; then
    echo "Real-ESRGAN not found. Downloading..."
    curl -L -o "$SCRIPT_DIR/realesrgan-macos.zip" "$ESRGAN_URL"
    unzip -o "$SCRIPT_DIR/realesrgan-macos.zip" -d "$SCRIPT_DIR/realesrgan"
    chmod +x "$ESRGAN"
    rm -f "$SCRIPT_DIR/realesrgan-macos.zip"
    echo "Real-ESRGAN installed."
    echo ""
fi

# --- Check dependencies ---
for cmd in yt-dlp ffmpeg ffprobe bc; do
    if ! command -v "$cmd" &>/dev/null; then
        echo "Error: $cmd is required but not installed."
        echo "Install with: brew install $cmd"
        exit 1
    fi
done

# --- Usage ---
if [ -z "$1" ]; then
    echo "Usage: ./enhance.sh \"<youtube-url-or-file>\""
    echo ""
    echo "Examples:"
    echo "  ./enhance.sh \"https://www.youtube.com/watch?v=xyz123\""
    echo "  ./enhance.sh /path/to/video.mp4"
    echo ""
    echo "NOTE: YouTube URLs must be quoted to prevent the shell from interpreting '?' as a glob."
    exit 1
fi

ARG="$1"

echo "=== Davaz Video Enhancement ==="
echo ""

# --- Detect machine specs ---
CHIP=$(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo "Unknown")
GPU_CORES=$(system_profiler SPDisplaysDataType 2>/dev/null | grep "Total Number of Cores" | awk '{print $NF}')
RAM=$(sysctl -n hw.memsize 2>/dev/null | awk '{printf "%.0f", $1/1073741824}')

echo "Machine: $CHIP"
echo "GPU Cores: ${GPU_CORES:-unknown}"
echo "RAM: ${RAM:-unknown} GB"
echo ""

# --- Determine input: local file or YouTube URL ---
if [ -f "$ARG" ]; then
    # Local file: use filename (without extension) as job ID
    VIDEO_ID=$(basename "$ARG" | sed 's/\.[^.]*$//')
    WORKDIR="$SCRIPT_DIR/jobs/$VIDEO_ID"
    mkdir -p "$WORKDIR"
    INPUT="$WORKDIR/original.mkv"
    if [ -f "$INPUT" ]; then
        echo "Video already copied to workspace."
    else
        echo "Copying local file to workspace..."
        EXT="${ARG##*.}"
        if [ "$EXT" = "mkv" ]; then
            cp "$ARG" "$INPUT"
        else
            echo "Converting to MKV..."
            ffmpeg -i "$ARG" -c copy "$INPUT"
        fi
    fi
else
    # YouTube URL: extract video ID and download
    URL="$ARG"
    VIDEO_ID=$(echo "$URL" | sed -n 's/.*[?&]v=\([^&]*\).*/\1/p')
    if [ -z "$VIDEO_ID" ]; then
        VIDEO_ID=$(echo "$URL" | sed -n 's|.*/\([^/?]*\).*|\1|p')
    fi
    if [ -z "$VIDEO_ID" ]; then
        echo "Error: Could not extract video ID from URL"
        exit 1
    fi

    WORKDIR="$SCRIPT_DIR/jobs/$VIDEO_ID"
    mkdir -p "$WORKDIR"
    INPUT="$WORKDIR/original.mkv"

    if [ -f "$INPUT" ]; then
        echo "Video already downloaded."
    else
        echo "Downloading video..."
        yt-dlp -o "$WORKDIR/original.%(ext)s" --merge-output-format mkv "$URL"
        if [ ! -f "$INPUT" ]; then
            DOWNLOADED=$(ls "$WORKDIR"/original.* 2>/dev/null | head -1)
            if [ -n "$DOWNLOADED" ]; then
                mv "$DOWNLOADED" "$INPUT"
            else
                echo "Error: Download failed"
                exit 1
            fi
        fi
    fi
fi

# --- Get video info ---
eval "$(ffprobe -v quiet -print_format flat -show_streams -select_streams v:0 "$INPUT" 2>/dev/null | grep -E 'width|height|r_frame_rate' | sed 's/\./_/g')"
SRC_W="${streams_stream_0_width}"
SRC_H="${streams_stream_0_height}"
FPS_FRAC="${streams_stream_0_r_frame_rate}"
FPS=$(echo "$FPS_FRAC" | bc -l | xargs printf "%.0f")
DURATION=$(ffprobe -v quiet -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 "$INPUT" | xargs printf "%.0f")
TOTAL_FRAMES=$(( DURATION * FPS ))

echo ""
echo "Video: ${SRC_W}x${SRC_H} @ ${FPS}fps, ${DURATION}s (~${TOTAL_FRAMES} frames)"

# --- Check disk space ---
INPUT_FRAME_MB=$(echo "$SRC_W * $SRC_H * 3 / 1024 / 1024 / 3" | bc -l)
OUTPUT_4X_FRAME_MB=$(echo "$SRC_W * 4 * $SRC_H * 4 * 3 / 1024 / 1024 / 3" | bc -l)
EST_INPUT_GB=$(echo "$TOTAL_FRAMES * $INPUT_FRAME_MB / 1024" | bc -l | xargs printf "%.0f")
EST_OUTPUT_GB=$(echo "$TOTAL_FRAMES * $OUTPUT_4X_FRAME_MB / 1024" | bc -l | xargs printf "%.0f")
EST_TOTAL_GB=$(( EST_INPUT_GB + EST_OUTPUT_GB + 5 ))
AVAIL_GB=$(df -g "$WORKDIR" 2>/dev/null | tail -1 | awk '{print $4}' || df -BG "$WORKDIR" | tail -1 | awk '{print $4}' | tr -d 'G')

echo "Estimated disk needed: ~${EST_TOTAL_GB} GB (input: ${EST_INPUT_GB} GB + output: ${EST_OUTPUT_GB} GB)"
echo "Available disk space:  ${AVAIL_GB} GB"

if [ "$EST_TOTAL_GB" -gt "$AVAIL_GB" ] 2>/dev/null; then
    echo ""
    echo "WARNING: May not have enough disk space!"
    echo "Need ~${EST_TOTAL_GB} GB but only ${AVAIL_GB} GB available."
    read -p "Continue anyway? [y/N] " DISK_CONFIRM
    if [ "$DISK_CONFIRM" != "y" ] && [ "$DISK_CONFIRM" != "Y" ]; then
        exit 1
    fi
fi
echo ""

# --- Benchmark: upscale one frame at 2x and 4x ---
echo "Benchmarking your GPU (one frame each)..."
mkdir -p "$WORKDIR/bench_in" "$WORKDIR/bench_out"

# Extract a single frame for benchmarking
ffmpeg -v quiet -i "$INPUT" -frames:v 1 -y "$WORKDIR/bench_in/bench.png"

# Benchmark 2x
TIME_2X=$( { time "$ESRGAN" -i "$WORKDIR/bench_in" -o "$WORKDIR/bench_out" -s 2 -n realesrgan-x4plus -m "$MODEL_DIR" -f png 2>/dev/null; } 2>&1 | grep real | awk '{print $2}' )
# Parse time (handles both 0m10.123s and 1m30.456s formats)
MIN_2X=$(echo "$TIME_2X" | sed 's/m.*//')
SEC_2X=$(echo "$TIME_2X" | sed 's/.*m//' | sed 's/s//')
SECS_2X=$(echo "$MIN_2X * 60 + $SEC_2X" | bc)

# Clean bench output for 4x run
rm -f "$WORKDIR/bench_out/"*

# Benchmark 4x
TIME_4X=$( { time "$ESRGAN" -i "$WORKDIR/bench_in" -o "$WORKDIR/bench_out" -s 4 -n realesrgan-x4plus -m "$MODEL_DIR" -f png 2>/dev/null; } 2>&1 | grep real | awk '{print $2}' )
MIN_4X=$(echo "$TIME_4X" | sed 's/m.*//')
SEC_4X=$(echo "$TIME_4X" | sed 's/.*m//' | sed 's/s//')
SECS_4X=$(echo "$MIN_4X * 60 + $SEC_4X" | bc)

# Clean up benchmark files
rm -rf "$WORKDIR/bench_in" "$WORKDIR/bench_out"

# --- Calculate estimates ---
TOTAL_SECS_2X=$(echo "$TOTAL_FRAMES * $SECS_2X" | bc | xargs printf "%.0f")
TOTAL_SECS_4X=$(echo "$TOTAL_FRAMES * $SECS_4X" | bc | xargs printf "%.0f")

format_time() {
    local secs=$1
    local h=$(( secs / 3600 ))
    local m=$(( (secs % 3600) / 60 ))
    if [ "$h" -gt 0 ]; then
        echo "${h}h ${m}m"
    else
        echo "${m}m"
    fi
}

EST_2X=$(format_time "$TOTAL_SECS_2X")
EST_4X=$(format_time "$TOTAL_SECS_4X")

OUT_W_2X=$(( SRC_W * 2 ))
OUT_H_2X=$(( SRC_H * 2 ))
OUT_W_4X=$(( SRC_W * 4 ))
OUT_H_4X=$(( SRC_H * 4 ))

# --- Recommend best option ---
if [ "$SRC_W" -ge 1920 ]; then
    RECOMMENDED=1
else
    RECOMMENDED=2
fi

echo ""
echo "==========================================="
echo "  Enhancement Options"
echo "==========================================="
echo ""
echo "  1) Minimal enhance (2x upscale)"
echo "     ${SRC_W}x${SRC_H} -> ${OUT_W_2X}x${OUT_H_2X}"
echo "     Benchmark: ${SECS_2X}s per frame"
echo "     Estimated time: $EST_2X"
echo ""
echo "  2) Maximum enhance (4x upscale)"
echo "     ${SRC_W}x${SRC_H} -> ${OUT_W_4X}x${OUT_H_4X}"
echo "     Benchmark: ${SECS_4X}s per frame"
echo "     Estimated time: $EST_4X"
echo ""
echo "  3) Cancel"
echo ""
if [ "$RECOMMENDED" -eq 1 ]; then
    echo "  * Recommended: Option 1 (source is already HD)"
else
    echo "  * Recommended: Option 2 (best quality improvement)"
fi
echo "==========================================="
echo ""
read -p "Select option [1/2/3]: " CHOICE

case "$CHOICE" in
    1) SCALE=2; MODEL="realesrgan-x4plus"; OUT_W=$OUT_W_2X; OUT_H=$OUT_H_2X ;;
    2) SCALE=4; MODEL="realesrgan-x4plus"; OUT_W=$OUT_W_4X; OUT_H=$OUT_H_4X ;;
    3) echo "Cancelled."; exit 0 ;;
    *) echo "Invalid choice."; exit 1 ;;
esac

OUTPUT="$WORKDIR/enhanced_${SCALE}x_${OUT_W}x${OUT_H}.mkv"

echo ""
echo "Starting ${SCALE}x enhancement: ${SRC_W}x${SRC_H} -> ${OUT_W}x${OUT_H}"
echo ""

# --- Extract frames ---
FRAMES_IN="$WORKDIR/frames_in"
mkdir -p "$FRAMES_IN"
if [ "$(ls "$FRAMES_IN"/ 2>/dev/null | wc -l)" -gt 0 ]; then
    TOTAL=$(ls "$FRAMES_IN"/ | wc -l)
    echo "Frames already extracted ($TOTAL frames), skipping."
else
    echo "Extracting frames..."
    ffmpeg -i "$INPUT" -qscale:v 2 "$FRAMES_IN/frame_%08d.png" 2>&1 | tail -1
    TOTAL=$(ls "$FRAMES_IN"/ | wc -l)
    echo "Extracted $TOTAL frames."
fi

# --- Upscale frames ---
FRAMES_OUT="$WORKDIR/frames_out_${SCALE}x"
mkdir -p "$FRAMES_OUT"
DONE=$(ls "$FRAMES_OUT"/ 2>/dev/null | wc -l)

echo ""
echo "Upscaling frames with Real-ESRGAN (${SCALE}x)..."
echo "Progress: $DONE / $TOTAL already done"

if [ "$DONE" -lt "$TOTAL" ]; then
    START_TIME=$(date +%s)
    "$ESRGAN" -i "$FRAMES_IN" -o "$FRAMES_OUT" -s "$SCALE" -n "$MODEL" -m "$MODEL_DIR" -f png
    END_TIME=$(date +%s)
    ELAPSED=$(( END_TIME - START_TIME ))
    echo "Upscaling complete in $(format_time $ELAPSED)"
else
    echo "All frames already upscaled, skipping."
fi

# --- Reassemble video ---
echo ""
echo "Reassembling video..."

HAS_AUDIO=$(ffprobe -v quiet -select_streams a -show_entries stream=codec_type -of csv=p=0 "$INPUT" 2>/dev/null | head -1)

if [ -n "$HAS_AUDIO" ]; then
    ffmpeg -framerate "$FPS" -i "$FRAMES_OUT/frame_%08d.png" \
        -i "$INPUT" \
        -map 0:v -map 1:a \
        -c:v libx264 -crf 18 -preset slow -pix_fmt yuv420p \
        -c:a copy \
        -y "$OUTPUT"
else
    ffmpeg -framerate "$FPS" -i "$FRAMES_OUT/frame_%08d.png" \
        -c:v libx264 -crf 18 -preset slow -pix_fmt yuv420p \
        -y "$OUTPUT"
fi

echo ""
echo "=== Done! ==="
echo "Output: $OUTPUT"
ls -lh "$OUTPUT"
