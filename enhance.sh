#!/usr/bin/env bash
#
# enhance.sh — Cloud GPU video enhancement pipeline using Real-ESRGAN.
# Replaces enhance_gpu.py with a Bash orchestrator that calls upscale.py.
#
# Usage: ./enhance.sh <youtube-url> <scale> [--job-name <title>] [--gpu N]
#        ./enhance.sh done <json-path>   — clean up after manual upload
#   scale: 2 or 4
#   --job-name: custom directory name under ~/jobs/ (default: video ID)
#   --gpu: GPU index for CUDA_VISIBLE_DEVICES (default: all GPUs)
#
# Requires: nvidia-smi, python3, ffmpeg, ffprobe, yt-dlp, deno
# Python packages: torch, realesrgan, basicsr, cv2, numpy
#
# Each phase is resumable — checks for existing output before re-running.

set -euo pipefail

export PATH="/opt/venv/bin:/opt/conda/bin:/usr/local/bin:/usr/bin:/bin:$PATH"
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
import json, sys
meta = {
    'video_id': sys.argv[1],
    'scale': int(sys.argv[2]),
    'title': sys.argv[3],
    'display_title': sys.argv[4],
    'started_at': sys.argv[5]
}
with open(sys.argv[6], 'w') as f:
    json.dump(meta, f, indent=2)
" "$VIDEO_ID" "$SCALE" "$JOB_NAME" "$DISPLAY_TITLE" "$STARTED_AT" "$META_FILE"
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
    # Peak disk = max of two phases:
    # 1. Extraction: all input frames exist (frames * in_sz)
    # 2. Upscaling: output frames grow while input frames are deleted (rolling 10 kept)
    #    Peak during upscaling: all output frames + 10 input frames
    # The larger of the two phases determines disk need
    all_in_gb = frames * in_sz / 2.5 / 1024
    all_out_gb = frames * out_sz / 2.5 / 1024
    peak_gb = max(all_in_gb, all_out_gb + 10 * in_sz / 2.5 / 1024)
    est_gb = peak_gb * 1.1 + 5  # 10% margin + 5 GB for MKV/overhead
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

    # Detect available GPUs for segment splitting
    local AVAIL_GPUS
    if [[ -n "$GPU_ID" ]]; then
        # Single GPU mode (called from multi_gpu_queue.sh)
        AVAIL_GPUS=1
    else
        AVAIL_GPUS=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | wc -l)
    fi

    local TOTAL_FRAMES
    TOTAL_FRAMES=$(ls "$FRAMES_IN"/frame_*.png 2>/dev/null | wc -l)

    if [[ "$AVAIL_GPUS" -gt 1 && "$TOTAL_FRAMES" -gt 1000 ]]; then
        # Multi-GPU segment splitting: divide frames across GPUs
        echo "Segment splitting across $AVAIL_GPUS GPUs ($TOTAL_FRAMES frames)"
        local PER_GPU=$(( (TOTAL_FRAMES + AVAIL_GPUS - 1) / AVAIL_GPUS ))
        local PIDS=()

        for ((g=0; g<AVAIL_GPUS; g++)); do
            local START=$((g * PER_GPU))
            local END=$(( (g + 1) * PER_GPU ))
            [[ $END -gt $TOTAL_FRAMES ]] && END=$TOTAL_FRAMES
            [[ $START -ge $TOTAL_FRAMES ]] && continue

            local GPU_LOG="$HOME/gpu${g}.log"
            echo "  GPU $g: frames $START-$END ($(( END - START )) frames) → $GPU_LOG"
            CUDA_VISIBLE_DEVICES=$g python3 "$SCRIPT_DIR/upscale.py" \
                "$FRAMES_IN" "$FRAMES_OUT" "$SCALE" \
                --start "$START" --end "$END" >> "$GPU_LOG" 2>&1 &
            PIDS+=($!)
        done

        # Wait for all GPUs to finish
        local FAILED=0
        for pid in "${PIDS[@]}"; do
            if ! wait "$pid"; then
                FAILED=$((FAILED + 1))
            fi
        done
        if [[ $FAILED -gt 0 ]]; then
            echo "WARNING: $FAILED GPU segment(s) failed"
        fi
    else
        # Single GPU mode
        python3 "$SCRIPT_DIR/upscale.py" "$FRAMES_IN" "$FRAMES_OUT" "$SCALE"
    fi

    local UP_END
    UP_END=$(date +%s)
    TIMING_UPSCALING=$((UP_END - UP_START))
    echo "Upscaling complete in $(python3 -c "print(round($TIMING_UPSCALING / 3600, 1))")h (${TIMING_UPSCALING}s)"
    echo
}

# ============================================================
# Phase 8b: Check for frame gaps (missing frames from kills/restarts)
# ============================================================
check_frame_gaps() {
    echo "Checking for frame gaps..."

    local GAPS
    GAPS=$(python3 -c "
import glob, os, sys

frames_out = sorted(glob.glob('$FRAMES_OUT/frame_*.png'))
if not frames_out:
    print('0')
    sys.exit(0)

# Extract frame numbers
nums = []
for f in frames_out:
    name = os.path.basename(f)
    # frame_00000001.png → 1
    num = int(name.replace('frame_', '').replace('.png', ''))
    nums.append(num)

nums.sort()
# Check for gaps (should be consecutive: 1,2,3,...,N)
gaps = []
for i in range(len(nums) - 1):
    if nums[i+1] - nums[i] > 1:
        for g in range(nums[i] + 1, nums[i+1]):
            gaps.append(g)

print(len(gaps))
if gaps:
    # Print gap frame numbers for re-upscaling
    for g in gaps[:50]:  # show max 50
        print(g)
" 2>/dev/null)

    local GAP_COUNT
    GAP_COUNT=$(echo "$GAPS" | head -1)

    if [[ "$GAP_COUNT" -eq 0 ]]; then
        echo "  No gaps — all frames consecutive."
        echo
        return
    fi

    echo "  WARNING: $GAP_COUNT missing frame(s) detected!"
    echo "  Re-upscaling missing frames..."

    # Re-upscale missing frames from frames_in (if they exist)
    python3 -c "
import glob, os, sys, cv2
sys.path.insert(0, '$SCRIPT_DIR')

frames_in = '$FRAMES_IN'
frames_out = '$FRAMES_OUT'
scale = $SCALE

# Find gaps
out_files = sorted(glob.glob(os.path.join(frames_out, 'frame_*.png')))
out_nums = set()
for f in out_files:
    num = int(os.path.basename(f).replace('frame_', '').replace('.png', ''))
    out_nums.add(num)

if not out_nums:
    sys.exit(0)

max_num = max(out_nums)
gaps = [n for n in range(1, max_num + 1) if n not in out_nums]

if not gaps:
    print('No gaps')
    sys.exit(0)

print(f'Re-upscaling {len(gaps)} missing frames...')

# Load model
import torch
from basicsr.archs.rrdbnet_arch import RRDBNet
from realesrgan import RealESRGANer
import logging
logging.getLogger('basicsr').setLevel(logging.WARNING)
logging.getLogger('realesrgan').setLevel(logging.WARNING)

model = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
tile = 512 if torch.cuda.is_available() else 0
upsampler = RealESRGANer(
    scale=4, model_path='https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth',
    model=model, tile=tile, tile_pad=10, pre_pad=0, half=True,
    gpu_id=0 if torch.cuda.is_available() else None)

fixed = 0
for num in gaps:
    in_path = os.path.join(frames_in, f'frame_{num:08d}.png')
    out_path = os.path.join(frames_out, f'frame_{num:08d}.png')
    if not os.path.exists(in_path):
        print(f'  Frame {num}: input missing, cannot fix')
        continue
    img = cv2.imread(in_path, cv2.IMREAD_UNCHANGED)
    if img is None:
        print(f'  Frame {num}: cannot read input')
        continue
    output, _ = upsampler.enhance(img, outscale=scale)
    tmp = out_path.rsplit('.', 1)[0] + '.tmp.png'
    cv2.imwrite(tmp, output)
    os.rename(tmp, out_path)
    fixed += 1
    print(f'  Frame {num}: fixed')

print(f'Fixed {fixed}/{len(gaps)} missing frames')
" 2>/dev/null

    echo "  Frame gap check complete."
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
    # Try Rust binary first (check $HOME and PATH), fall back to Python
    local UPLOAD_BIN=""
    if [[ -x "$HOME/youtube_upload" ]]; then
        UPLOAD_BIN="$HOME/youtube_upload"
    elif command -v youtube_upload &>/dev/null; then
        UPLOAD_BIN="youtube_upload"
    fi

    if [[ -n "$UPLOAD_BIN" ]]; then
        if "$UPLOAD_BIN" --video-id="$VIDEO_ID" "$OUTPUT" \
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
# Done: clean up after manual upload
# ============================================================
do_done() {
    local JSON_PATH="$1"
    if [[ ! -f "$JSON_PATH" ]]; then
        echo "ERROR: File not found: $JSON_PATH"
        exit 1
    fi

    local VID
    VID=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['video_id'])" "$JSON_PATH" 2>/dev/null)
    if [[ -z "$VID" ]]; then
        # Fall back to filename
        VID=$(basename "$JSON_PATH" .json)
    fi

    local JOBDIR="$HOME/jobs/$VID"
    if [[ -d "$JOBDIR" ]]; then
        local SIZE
        SIZE=$(du -sh "$JOBDIR" | cut -f1)
        rm -rf "$JOBDIR"
        echo "Deleted $JOBDIR ($SIZE freed)"
    else
        echo "No job directory found at $JOBDIR"
    fi

    # Move JSON to done
    mkdir -p "$HOME/json_done"
    if [[ -f "$HOME/json/${VID}.json" ]]; then
        mv "$HOME/json/${VID}.json" "$HOME/json_done/"
        echo "Moved ${VID}.json to json_done/"
    fi

    echo "Done: $VID cleaned up"
}

# ============================================================
# Main
# ============================================================
if [[ "${1:-}" == "done" ]]; then
    if [[ -z "${2:-}" ]]; then
        echo "Usage: $0 done <json-path>"
        echo "  e.g. $0 done json/8SvgnUHDdTU.json"
        exit 1
    fi
    do_done "$2"
    exit 0
fi

main() {
    parse_args "$@"
    write_job_meta

    # Fast resume: if frames already exist, skip download/extract entirely
    local EXISTING_OUT EXISTING_IN
    EXISTING_OUT=$(find "$FRAMES_OUT" -maxdepth 1 -name "frame_*.png" 2>/dev/null | wc -l)
    EXISTING_IN=$(find "$FRAMES_IN" -maxdepth 1 -name "frame_*.png" 2>/dev/null | wc -l)
    if [[ "$EXISTING_IN" -gt 0 || "$EXISTING_OUT" -gt 0 ]]; then
        echo "Resuming: $EXISTING_IN input frames, $EXISTING_OUT output frames"
        # Still need video info for reassembly (fps, resolution)
        if [[ -f "$INPUT" ]]; then
            update_job_meta
        fi
    else
        preflight_checks
        pre_download_disk_check
        download_video
        update_job_meta
        extract_frames
    fi

    upscale_frames
    check_frame_gaps
    reassemble_video
    write_timing
    print_summary
    upload_video
}

main "$@"
