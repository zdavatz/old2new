#!/usr/bin/env bash
#
# enhance.sh — Cloud GPU video enhancement pipeline using Real-ESRGAN.
# Replaces enhance_gpu.py with a Bash orchestrator that calls upscale.py.
#
# Usage: ./enhance.sh <youtube-url> <scale> [--job-name <title>] [--gpu N]
#   scale: 2 or 4
#   --job-name: custom directory name under ~/jobs/ (default: video ID)
#   --gpu: GPU index for CUDA_VISIBLE_DEVICES (default: all GPUs)
#
# Requires: nvidia-smi, python3, ffmpeg, ffprobe, yt-dlp, deno
# Python packages: torch, realesrgan, basicsr, cv2, numpy
#
# Each phase is resumable — checks for existing output before re-running.

set -euo pipefail

export PATH="/opt/venv/bin:/usr/local/bin:/usr/bin:/bin:$PATH"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Timing accumulators (seconds)
TIMING_DOWNLOAD=0
TIMING_EXTRACTION=0
TIMING_UPSCALING=0
TIMING_REASSEMBLY=0

# ============================================================
# Phase 1: Parse Arguments
# ============================================================
parse_args() {
    if [[ $# -lt 2 ]]; then
        echo "Usage: $0 <youtube-url> <scale> [--job-name <title>]"
        echo "  scale: 2 or 4"
        echo "  --job-name: custom directory name under ~/jobs/ (default: video ID)"
        exit 1
    fi

    URL="$1"
    SCALE="$2"
    shift 2

    if [[ "$SCALE" != "2" && "$SCALE" != "4" ]]; then
        echo "ERROR: scale must be 2 or 4, got: $SCALE"
        exit 1
    fi

    JOB_NAME=""
    GPU_ID=""
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --job-name)
                JOB_NAME="$2"
                shift 2
                ;;
            --gpu)
                GPU_ID="$2"
                export CUDA_VISIBLE_DEVICES="$GPU_ID"
                shift 2
                ;;
            *)
                echo "Unknown argument: $1"
                exit 1
                ;;
        esac
    done

    # Extract video ID from URL
    VIDEO_ID=$(echo "$URL" | grep -oP '[?&]v=\K[^&]+' || true)
    if [[ -z "$VIDEO_ID" ]]; then
        VIDEO_ID=$(echo "$URL" | grep -oP '/([^/?]+)$' | tr -d '/' || true)
    fi
    if [[ -z "$VIDEO_ID" ]]; then
        echo "ERROR: Could not extract video ID from URL: $URL"
        exit 1
    fi

    # Default job name to video ID
    if [[ -z "$JOB_NAME" ]]; then
        JOB_NAME="$VIDEO_ID"
    fi

    JOBS_DIR="$HOME/jobs"
    WORKDIR="$JOBS_DIR/$JOB_NAME"
    FRAMES_IN="$WORKDIR/frames_in"
    FRAMES_OUT="$WORKDIR/frames_out"
    INPUT="$WORKDIR/$JOB_NAME.mkv"

    mkdir -p "$FRAMES_IN" "$FRAMES_OUT"

    # Backwards compat: use original.mkv if it exists
    if [[ ! -f "$INPUT" && -f "$WORKDIR/original.mkv" ]]; then
        INPUT="$WORKDIR/original.mkv"
    fi

    # Auto-detect cookies
    COOKIES_OPT=""
    if [[ -f "$HOME/cookies.txt" ]]; then
        COOKIES_OPT="--cookies $HOME/cookies.txt"
        echo "Using cookies: $HOME/cookies.txt"
    fi

    # yt-dlp JS challenge solver (required since ~March 2026)
    YTDLP_RC="--remote-components ejs:github"

    echo "Video ID:  $VIDEO_ID"
    echo "Scale:     ${SCALE}x"
    echo "Job name:  $JOB_NAME"
    echo "Work dir:  $WORKDIR"
    echo
}

# ============================================================
# Phase 2: Write job_meta.json (resume-safe)
# ============================================================
write_job_meta() {
    local META_FILE="$WORKDIR/job_meta.json"
    if [[ -f "$META_FILE" ]]; then
        echo "job_meta.json already exists (resume)."
        return
    fi

    # Read display_title from ~/json/{VIDEO_ID}.json (or .processing.* variant)
    local DISPLAY_TITLE=""
    local JSON_FILE=""
    for jf in "$HOME/json/${VIDEO_ID}.json" "$HOME/json/${VIDEO_ID}.json.processing."*; do
        if [[ -f "$jf" ]]; then
            JSON_FILE="$jf"
            break
        fi
    done
    if [[ -n "$JSON_FILE" ]]; then
        DISPLAY_TITLE=$(python3 -c "import json; print(json.load(open('$JSON_FILE')).get('title',''))" 2>/dev/null)
    fi
    # Fallback: replace underscores with spaces
    if [[ -z "$DISPLAY_TITLE" ]]; then
        DISPLAY_TITLE=$(echo "$JOB_NAME" | sed 's/[_-]/ /g')
    fi

    local STARTED_AT
    STARTED_AT=$(date -u +"%Y-%m-%dT%H:%M:%S")

    python3 -c "
import json
meta = {
    'video_id': '$VIDEO_ID',
    'scale': $SCALE,
    'title': '$JOB_NAME',
    'display_title': '$DISPLAY_TITLE',
    'started_at': '$STARTED_AT'
}
with open('$META_FILE', 'w') as f:
    json.dump(meta, f, indent=2)
"
    echo "Wrote $META_FILE"
}

# ============================================================
# Phase 3: Pre-flight Checks
# ============================================================
preflight_checks() {
    echo "============================================================"
    echo "PRE-FLIGHT CHECK"
    echo "============================================================"
    local ERRORS=()
    local WARNINGS=()

    # --- GPU ---
    echo
    echo "[GPU]"
    if command -v nvidia-smi &>/dev/null; then
        local GPU_INFO
        GPU_INFO=$(nvidia-smi --query-gpu=name,memory.total,driver_version,compute_cap --format=csv,noheader,nounits 2>/dev/null || true)
        if [[ -n "$GPU_INFO" ]]; then
            local GPU_NAME GPU_VRAM_MB GPU_DRIVER GPU_COMPUTE
            IFS=',' read -r GPU_NAME GPU_VRAM_MB GPU_DRIVER GPU_COMPUTE <<< "$GPU_INFO"
            GPU_NAME=$(echo "$GPU_NAME" | xargs)
            GPU_VRAM_MB=$(echo "$GPU_VRAM_MB" | xargs)
            GPU_DRIVER=$(echo "$GPU_DRIVER" | xargs)
            GPU_COMPUTE=$(echo "$GPU_COMPUTE" | xargs)
            local GPU_VRAM_GB
            GPU_VRAM_GB=$(python3 -c "print(int($GPU_VRAM_MB // 1024))")
            echo "  GPU:      $GPU_NAME"
            echo "  VRAM:     ${GPU_VRAM_GB} GB"
            echo "  Driver:   $GPU_DRIVER"
            echo "  Compute:  sm_${GPU_COMPUTE//.}"

            # Power and clock
            local PW_INFO
            PW_INFO=$(nvidia-smi --query-gpu=power.limit,clocks.max.graphics --format=csv,noheader,nounits 2>/dev/null || true)
            if [[ -n "$PW_INFO" ]]; then
                local POWER_LIMIT MAX_CLOCK
                IFS=',' read -r POWER_LIMIT MAX_CLOCK <<< "$PW_INFO"
                POWER_LIMIT=$(echo "$POWER_LIMIT" | xargs)
                MAX_CLOCK=$(echo "$MAX_CLOCK" | xargs)
                echo "  Power:    ${POWER_LIMIT}W"
                echo "  MaxClock: ${MAX_CLOCK} MHz"
                # Warn about power-limited GPUs
                local PW_INT
                PW_INT=$(printf "%.0f" "$POWER_LIMIT")
                if [[ "$PW_INT" -lt 400 ]] && [[ "$GPU_VRAM_MB" -gt 30000 ]]; then
                    WARNINGS+=("Low power limit (${POWER_LIMIT}W) for a ${GPU_VRAM_GB}GB GPU -- may be Max-Q/throttled")
                fi
            fi
        else
            ERRORS+=("nvidia-smi failed -- no GPU detected")
        fi
    else
        ERRORS+=("nvidia-smi not found -- no NVIDIA GPU available")
    fi

    # --- PyTorch ---
    echo
    echo "[PyTorch]"
    if python3 -c "import torch" 2>/dev/null; then
        local PT_VER CUDA_VER CUDA_AVAIL
        PT_VER=$(python3 -c "import torch; print(torch.__version__)")
        CUDA_VER=$(python3 -c "import torch; print(torch.version.cuda or 'none')")
        CUDA_AVAIL=$(python3 -c "import torch; print(torch.cuda.is_available())")
        echo "  PyTorch:  $PT_VER"
        echo "  CUDA:     $CUDA_VER"
        echo "  GPU OK:   $CUDA_AVAIL"
        if [[ "$CUDA_AVAIL" != "True" ]]; then
            ERRORS+=("CUDA not available -- torch.cuda.is_available() returned False")
        fi
    else
        ERRORS+=("PyTorch not installed -- pip install torch")
    fi

    # --- CPU ---
    echo
    echo "[CPU]"
    local CPU_MODEL CPU_MHZ CPU_CORES
    CPU_MODEL=$(grep -m1 "model name" /proc/cpuinfo 2>/dev/null | cut -d: -f2 | xargs || echo "unknown")
    CPU_MHZ=$(grep -m1 "cpu MHz" /proc/cpuinfo 2>/dev/null | cut -d: -f2 | xargs || echo "0")
    CPU_CORES=$(nproc 2>/dev/null || echo "1")
    echo "  Model:    $CPU_MODEL"
    echo "  Cores:    $CPU_CORES"
    echo "  MHz:      $CPU_MHZ"
    if [[ "$CPU_CORES" -lt 8 ]]; then
        WARNINGS+=("Only $CPU_CORES CPU cores -- I/O pipeline needs 16+ for full GPU utilization.")
    fi

    # --- RAM ---
    echo
    echo "[RAM]"
    local RAM_TOTAL_GB RAM_AVAIL_GB
    RAM_TOTAL_GB=$(free -g | awk '/^Mem:/ {print $2}')
    RAM_AVAIL_GB=$(free -g | awk '/^Mem:/ {print $7}')
    echo "  Total:    ${RAM_TOTAL_GB} GB"
    echo "  Avail:    ${RAM_AVAIL_GB} GB"
    if [[ "$RAM_AVAIL_GB" -lt 4 ]]; then
        WARNINGS+=("Low available RAM (${RAM_AVAIL_GB} GB). May cause issues with large frames.")
    fi

    # --- Disk ---
    echo
    echo "[Disk]"
    local DISK_TOTAL DISK_FREE
    DISK_TOTAL=$(df -BG "$HOME" | awk 'NR==2 {print $2}' | tr -d 'G')
    DISK_FREE=$(df -BG "$HOME" | awk 'NR==2 {print $4}' | tr -d 'G')
    echo "  Total:    ${DISK_TOTAL} GB"
    echo "  Free:     ${DISK_FREE} GB"

    # Disk I/O benchmark
    echo -n "  Write:    "
    dd if=/dev/zero of=/tmp/disk_bench_test bs=1M count=100 oflag=direct 2>&1 | tail -1 | grep -oP '[\d.]+ [MG]B/s' || echo "unknown"
    rm -f /tmp/disk_bench_test

    # --- Software ---
    echo
    echo "[Software]"
    for cmd in ffmpeg ffprobe yt-dlp python3 deno; do
        if command -v "$cmd" &>/dev/null; then
            local ver=""
            case "$cmd" in
                ffmpeg)   ver=$(ffmpeg -version 2>/dev/null | head -1) ;;
                ffprobe)  ver="OK" ;;
                yt-dlp)   ver=$(yt-dlp --version 2>/dev/null) ;;
                python3)  ver=$(python3 --version 2>/dev/null) ;;
                deno)     ver=$(deno --version 2>/dev/null | head -1) ;;
            esac
            echo "  $cmd:  $ver"
        else
            if [[ "$cmd" == "deno" ]]; then
                WARNINGS+=("deno not found -- needed for yt-dlp JS challenge solving")
            else
                ERRORS+=("$cmd not found")
            fi
        fi
    done

    # Python packages
    for pkg in numpy cv2 basicsr realesrgan; do
        if python3 -c "import $pkg" 2>/dev/null; then
            local pkg_ver
            pkg_ver=$(python3 -c "import $pkg; print(getattr($pkg, '__version__', 'OK'))" 2>/dev/null || echo "OK")
            echo "  $pkg:  $pkg_ver"
        else
            ERRORS+=("Python package $pkg not installed")
        fi
    done

    # --- PCIe ---
    local PCIE_INFO
    PCIE_INFO=$(nvidia-smi --query-gpu=pcie.link.gen.current,pcie.link.width.current --format=csv,noheader 2>/dev/null || true)
    if [[ -n "$PCIE_INFO" ]]; then
        echo
        echo "[PCIe]"
        local PCIE_GEN PCIE_WIDTH
        IFS=',' read -r PCIE_GEN PCIE_WIDTH <<< "$PCIE_INFO"
        echo "  Gen:      $(echo "$PCIE_GEN" | xargs)"
        echo "  Width:    x$(echo "$PCIE_WIDTH" | xargs)"
    fi

    # --- Summary ---
    echo
    echo "============================================================"
    if [[ ${#ERRORS[@]} -gt 0 ]]; then
        echo "ERRORS (${#ERRORS[@]}):"
        for e in "${ERRORS[@]}"; do
            echo "  x $e"
        done
    fi
    if [[ ${#WARNINGS[@]} -gt 0 ]]; then
        echo "WARNINGS (${#WARNINGS[@]}):"
        for w in "${WARNINGS[@]}"; do
            echo "  ! $w"
        done
    fi
    if [[ ${#ERRORS[@]} -eq 0 && ${#WARNINGS[@]} -eq 0 ]]; then
        echo "ALL CHECKS PASSED"
    elif [[ ${#ERRORS[@]} -eq 0 ]]; then
        echo "CHECKS PASSED (with warnings)"
    fi
    echo "============================================================"
    echo

    if [[ ${#ERRORS[@]} -gt 0 ]]; then
        echo "Fix the errors above before running enhancement."
        exit 1
    fi
}

# ============================================================
# Phase 4: Pre-download Disk Check
# ============================================================
pre_download_disk_check() {
    # Skip if video already downloaded
    if [[ -f "$INPUT" ]]; then
        return
    fi

    echo "Fetching video info for disk estimate..."
    local JSON_OUT
    # shellcheck disable=SC2086
    JSON_OUT=$(yt-dlp $YTDLP_RC --dump-json --no-download $COOKIES_OPT "$URL" 2>/dev/null || true)

    if [[ -z "$JSON_OUT" ]]; then
        echo "  Warning: Could not fetch video info, skipping pre-download disk check."
        echo
        return
    fi

    local PRE_INFO
    PRE_INFO=$(echo "$JSON_OUT" | python3 -c "
import sys, json
info = json.load(sys.stdin)
w = info.get('width', 0) or 0
h = info.get('height', 0) or 0
dur = info.get('duration', 0) or 0
fps = info.get('fps', 25) or 25
print(f'{w} {h} {dur} {fps}')
")
    local PRE_W PRE_H PRE_DUR PRE_FPS
    read -r PRE_W PRE_H PRE_DUR PRE_FPS <<< "$PRE_INFO"

    if [[ "$PRE_W" -eq 0 || "$PRE_H" -eq 0 ]]; then
        echo "  Warning: Incomplete video info, skipping disk check."
        echo
        return
    fi

    # Calculate disk estimate using Python for float math
    local DISK_INFO
    DISK_INFO=$(python3 -c "
import os, glob
w, h, dur, fps, scale = $PRE_W, $PRE_H, float('$PRE_DUR'), float('$PRE_FPS'), $SCALE
frames = int(dur * fps)
in_sz = (w * h * 3) / (1024 * 1024)
out_sz = (w * scale * h * scale * 3) / (1024 * 1024)
# Account for existing frames on resume
fi_count = len(glob.glob('$FRAMES_IN/frame_*.png'))
fo_count = len(glob.glob('$FRAMES_OUT/frame_*.png'))
if fo_count > 0:
    reclaimable = (fi_count * in_sz / 2.5) / 1024
    remain_in = max(0, frames - fi_count) * in_sz / 2.5 / 1024
    remain_out = max(0, frames - fo_count) * out_sz / 2.5 / 1024
    est_gb = max(remain_in + remain_out - reclaimable, 0) * 1.1 + 2
else:
    est_gb = (frames * in_sz / 2.5 + frames * out_sz / 2.5) / 1024
    est_gb = est_gb * 1.1 + 5
st = os.statvfs('$WORKDIR')
avail = (st.f_frsize * st.f_bavail) / (1024**3)
print(f'{est_gb:.0f} {avail:.0f} {frames}')
")
    local EST_GB AVAIL_GB PRE_FRAMES
    read -r EST_GB AVAIL_GB PRE_FRAMES <<< "$DISK_INFO"

    echo "  Video:    ${PRE_W}x${PRE_H} @ ${PRE_FPS}fps, ${PRE_DUR}s (~${PRE_FRAMES} frames)"
    echo "  Disk est: ~${EST_GB} GB needed, ${AVAIL_GB} GB available"

    if [[ "$EST_GB" -gt "$AVAIL_GB" ]]; then
        echo
        echo "  ERROR: Not enough disk space!"
        echo "  Need ~${EST_GB} GB but only ${AVAIL_GB} GB available."
        echo "  Resize disk to at least $((EST_GB * 120 / 100)) GB or use a larger instance."
        exit 1
    else
        local HEADROOM=$((AVAIL_GB - EST_GB))
        echo "  Disk OK:  ${HEADROOM} GB headroom"
    fi
    echo
}

# ============================================================
# Phase 5: Download
# ============================================================
download_video() {
    if [[ -f "$INPUT" ]]; then
        echo "Video already downloaded: $INPUT"
        echo
        return
    fi

    echo "Downloading video..."
    local DL_START
    DL_START=$(date +%s)

    # shellcheck disable=SC2086
    yt-dlp $YTDLP_RC -o "$WORKDIR/$JOB_NAME.%(ext)s" --merge-output-format mkv $COOKIES_OPT "$URL"

    # If expected file doesn't exist, find what yt-dlp produced
    if [[ ! -f "$INPUT" ]]; then
        local FOUND
        FOUND=$(find "$WORKDIR" -maxdepth 1 -name "*.mkv" ! -name "*enhanced*" ! -name "*_${SCALE}x*" -print -quit 2>/dev/null || true)
        if [[ -z "$FOUND" ]]; then
            FOUND=$(find "$WORKDIR" -maxdepth 1 -type f \( -name "*.mp4" -o -name "*.webm" \) -print -quit 2>/dev/null || true)
        fi
        if [[ -n "$FOUND" ]]; then
            mv "$FOUND" "$INPUT"
        else
            echo "ERROR: Download failed -- no video file produced"
            exit 1
        fi
    fi

    local DL_END DL_SIZE_MB
    DL_END=$(date +%s)
    TIMING_DOWNLOAD=$((DL_END - DL_START))
    DL_SIZE_MB=$(du -m "$INPUT" | cut -f1)
    echo "Downloaded ${DL_SIZE_MB} MB in ${TIMING_DOWNLOAD}s"
    echo
}

# ============================================================
# Phase 6: Update job_meta.json with ffprobe info
# ============================================================
update_job_meta() {
    local META_FILE="$WORKDIR/job_meta.json"

    # Get video properties via ffprobe
    local PROBE_OUT
    PROBE_OUT=$(ffprobe -v quiet -select_streams v:0 \
        -show_entries stream=width,height,r_frame_rate \
        -show_entries format=duration \
        -of default=noprint_wrappers=1 "$INPUT" 2>/dev/null)

    # Parse probe output
    SRC_W=$(echo "$PROBE_OUT" | grep "^width=" | head -1 | cut -d= -f2)
    SRC_H=$(echo "$PROBE_OUT" | grep "^height=" | head -1 | cut -d= -f2)
    DURATION=$(echo "$PROBE_OUT" | grep "^duration=" | head -1 | cut -d= -f2)
    FPS_FRAC=$(echo "$PROBE_OUT" | grep "^r_frame_rate=" | head -1 | cut -d= -f2)

    # Calculate numeric fps
    local FPS_NUM FPS_DEN
    FPS_NUM=$(echo "$FPS_FRAC" | cut -d/ -f1)
    FPS_DEN=$(echo "$FPS_FRAC" | cut -d/ -f2)
    if [[ -n "$FPS_DEN" && "$FPS_DEN" != "0" && "$FPS_DEN" != "$FPS_NUM" ]]; then
        FPS=$(python3 -c "print(round($FPS_NUM / $FPS_DEN, 2))")
        FPS_INT=$(python3 -c "print(int($FPS_NUM // $FPS_DEN))")
    else
        FPS="$FPS_NUM"
        FPS_INT="$FPS_NUM"
    fi

    TOTAL_FRAMES=$(python3 -c "print(int(float('$DURATION') * float('$FPS')))")

    echo "Video: ${SRC_W}x${SRC_H} @ ${FPS}fps, $(printf '%.0f' "$DURATION")s ($TOTAL_FRAMES frames)"

    # Update meta file with video info
    python3 -c "
import json, os
meta_path = '$META_FILE'
if os.path.exists(meta_path):
    with open(meta_path) as f:
        meta = json.load(f)
else:
    meta = {}
meta['width'] = $SRC_W
meta['height'] = $SRC_H
meta['fps'] = float('$FPS')
meta['duration_seconds'] = float('$DURATION')
meta['total_frames'] = $TOTAL_FRAMES
with open(meta_path, 'w') as f:
    json.dump(meta, f, indent=2)
"
    echo "Updated $META_FILE with video info."
    echo
}

# ============================================================
# Phase 7: Extract Frames
# ============================================================
extract_frames() {
    local EXISTING_COUNT
    EXISTING_COUNT=$(find "$FRAMES_IN" -maxdepth 1 -name "frame_*.png" 2>/dev/null | wc -l)

    if [[ "$EXISTING_COUNT" -gt 0 ]]; then
        echo "Frames already extracted: $EXISTING_COUNT"
        return
    fi

    echo "Extracting ~$TOTAL_FRAMES frames..."
    local EX_START
    EX_START=$(date +%s)

    ffmpeg -i "$INPUT" -qscale:v 2 "$FRAMES_IN/frame_%08d.png" \
        -loglevel warning -stats

    local EX_END EXTRACTED
    EX_END=$(date +%s)
    TIMING_EXTRACTION=$((EX_END - EX_START))
    EXTRACTED=$(find "$FRAMES_IN" -maxdepth 1 -name "frame_*.png" | wc -l)
    echo "Extracted $EXTRACTED frames in ${TIMING_EXTRACTION}s"
    echo
}

# ============================================================
# Phase 8: Upscale (calls upscale.py)
# ============================================================
upscale_frames() {
    echo "Upscaling frames (${SCALE}x)..."
    local UP_START
    UP_START=$(date +%s)

    python3 "$SCRIPT_DIR/upscale.py" "$FRAMES_IN" "$FRAMES_OUT" "$SCALE"

    local UP_END
    UP_END=$(date +%s)
    TIMING_UPSCALING=$((UP_END - UP_START))
    echo "Upscaling complete in $(python3 -c "print(round($TIMING_UPSCALING / 3600, 1))")h (${TIMING_UPSCALING}s)"
    echo
}

# ============================================================
# Phase 9: Reassemble
# ============================================================
reassemble_video() {
    OUTPUT="$WORKDIR/${JOB_NAME}_${SCALE}x.mkv"

    if [[ -f "$OUTPUT" ]]; then
        echo "Output already exists: $OUTPUT"
        return
    fi

    echo "Reassembling video at ${FPS_INT}fps..."
    local RE_START
    RE_START=$(date +%s)

    # Check if source has audio
    local HAS_AUDIO
    HAS_AUDIO=$(ffprobe -v quiet -select_streams a -show_entries stream=codec_type \
        -of csv=p=0 "$INPUT" 2>/dev/null || true)

    if [[ -n "$HAS_AUDIO" ]]; then
        ffmpeg -framerate "$FPS_INT" -i "$FRAMES_OUT/frame_%08d.png" \
            -i "$INPUT" -map 0:v -map 1:a \
            -c:v libx264 -crf 18 -preset medium -pix_fmt yuv420p \
            -c:a copy -y "$OUTPUT" \
            -loglevel warning -stats
    else
        ffmpeg -framerate "$FPS_INT" -i "$FRAMES_OUT/frame_%08d.png" \
            -c:v libx264 -crf 18 -preset medium -pix_fmt yuv420p \
            -y "$OUTPUT" \
            -loglevel warning -stats
    fi

    local RE_END
    RE_END=$(date +%s)
    TIMING_REASSEMBLY=$((RE_END - RE_START))
    echo "Reassembly complete in ${TIMING_REASSEMBLY}s"
    echo
}

# ============================================================
# Phase 10: Write timing.json
# ============================================================
write_timing() {
    local TIMING_FILE="$WORKDIR/timing.json"
    python3 -c "
import json
timing = {
    'download': $TIMING_DOWNLOAD,
    'extraction': $TIMING_EXTRACTION,
    'upscaling': $TIMING_UPSCALING,
    'reassembly': $TIMING_REASSEMBLY
}
with open('$TIMING_FILE', 'w') as f:
    json.dump(timing, f, indent=2)
"
    echo "Wrote $TIMING_FILE"
}

# ============================================================
# Phase 11: Print Summary
# ============================================================
print_summary() {
    local DL=$TIMING_DOWNLOAD
    local EX=$TIMING_EXTRACTION
    local UP=$TIMING_UPSCALING
    local RE=$TIMING_REASSEMBLY
    local TOTAL_TIME=$((DL + EX + UP + RE))
    local OVERHEAD=$((DL + EX + RE))

    echo
    echo "============================================================"
    echo "TIMING BREAKDOWN"
    echo "============================================================"
    echo "  Download:    ${DL}s ($(python3 -c "print(round($DL / 60, 1))")m)"
    echo "  Extraction:  ${EX}s ($(python3 -c "print(round($EX / 60, 1))")m)"
    echo "  Upscaling:   ${UP}s ($(python3 -c "print(round($UP / 3600, 1))")h)"
    echo "  Reassembly:  ${RE}s ($(python3 -c "print(round($RE / 60, 1))")m)"
    echo "  Total:       ${TOTAL_TIME}s ($(python3 -c "print(round($TOTAL_TIME / 3600, 1))")h)"
    if [[ "$TOTAL_TIME" -gt 0 ]]; then
        local OVERHEAD_PCT
        OVERHEAD_PCT=$(python3 -c "print(int($OVERHEAD * 100 // $TOTAL_TIME))")
        echo "  Overhead:    ${OVERHEAD}s ($(python3 -c "print(int($OVERHEAD // 60))")m, ${OVERHEAD_PCT}% of total)"
    fi

    OUTPUT="$WORKDIR/${JOB_NAME}_${SCALE}x.mkv"
    if [[ -f "$OUTPUT" ]]; then
        local SIZE_MB
        SIZE_MB=$(du -m "$OUTPUT" | cut -f1)
        echo
        echo "Done! Output: $OUTPUT (${SIZE_MB} MB)"
    fi
}

# ============================================================
# Phase 12: Upload (if credentials exist)
# ============================================================
upload_video() {
    OUTPUT="$WORKDIR/${JOB_NAME}_${SCALE}x.mkv"

    if [[ ! -f "$HOME/client_secret.json" ]]; then
        echo
        echo "No ~/client_secret.json found, skipping upload."
        return
    fi

    if [[ ! -f "$OUTPUT" ]]; then
        echo
        echo "No output file found, skipping upload."
        return
    fi

    echo
    echo "Uploading to YouTube..."

    local UPLOAD_OK=0
    # Try Rust binary first, fall back to Python
    if command -v youtube_upload &>/dev/null; then
        if youtube_upload --video-id="$VIDEO_ID" "$OUTPUT" \
            --client-secret "$HOME/client_secret.json" \
            --token "$HOME/youtube_token.json"; then
            UPLOAD_OK=1
        fi
    elif [[ -f "$SCRIPT_DIR/youtube_upload.py" ]]; then
        if python3 "$SCRIPT_DIR/youtube_upload.py" --video-id="$VIDEO_ID" "$OUTPUT"; then
            UPLOAD_OK=1
        fi
    else
        echo "No upload tool found, skipping upload."
        return
    fi

    if [[ "$UPLOAD_OK" -eq 1 ]]; then
        echo "Upload successful. Cleaning up work directory..."
        rm -rf "$WORKDIR"
        # Move JSON from queue to done
        mkdir -p "$HOME/json_done"
        if [[ -f "$HOME/json/${VIDEO_ID}.json" ]]; then
            mv "$HOME/json/${VIDEO_ID}.json" "$HOME/json_done/"
            echo "Moved ${VIDEO_ID}.json to json_done/"
        fi
    else
        echo "Upload failed. Keeping work directory for retry."
    fi
}

# ============================================================
# Main
# ============================================================
main() {
    parse_args "$@"
    write_job_meta
    preflight_checks
    pre_download_disk_check
    download_video
    update_job_meta
    extract_frames
    upscale_frames
    reassemble_video
    write_timing
    print_summary
    upload_video
}

main "$@"
