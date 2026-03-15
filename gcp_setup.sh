#!/bin/bash
set -e

# --- Usage ---
if [ -z "$1" ]; then
    echo "Usage: ./gcp_setup.sh <youtube-url> [scale] [project-id] [zone]"
    echo "       ./gcp_setup.sh status [project-id] [zone]"
    echo ""
    echo "  scale: 2 or 4 (default: 4)"
    echo ""
    echo "Example: ./gcp_setup.sh \"https://www.youtube.com/watch?v=xyz123\""
    echo "         ./gcp_setup.sh \"https://www.youtube.com/watch?v=xyz123\" 2 old2new-davaz"
    echo "         ./gcp_setup.sh status old2new-davaz"
    echo ""
    echo "Prerequisites:"
    echo "  - gcloud CLI installed (brew install --cask google-cloud-sdk)"
    echo "  - Authenticated (gcloud auth login)"
    echo "  - Project with billing enabled and GPUS_ALL_REGIONS quota >= 1"
    exit 1
fi

# Find gcloud early for status command
GCLOUD=$(which gcloud 2>/dev/null || echo "/opt/homebrew/share/google-cloud-sdk/bin/gcloud")

# --- Status command ---
if [ "$1" = "status" ]; then
    PROJECT="${2:-old2new-davaz}"
    ZONE="${3:-us-central1-a}"
    INSTANCE="old2new-gpu"
    echo "=== Status: $PROJECT ==="
    $GCLOUD compute ssh "$INSTANCE" --project="$PROJECT" --zone="$ZONE" --command='
        echo "GPU: $(nvidia-smi --query-gpu=name,utilization.gpu,memory.used --format=csv,noheader 2>/dev/null || echo "N/A")"
        echo ""
        for job_dir in ~/jobs/*/; do
            [ -d "$job_dir" ] || continue
            video_id=$(basename "$job_dir")
            total=$(ls "$job_dir/frames_in/" 2>/dev/null | wc -l)
            done_frames=$(ls "$job_dir/frames_out/" 2>/dev/null | wc -l)
            if [ "$total" -gt 0 ] 2>/dev/null; then
                pct=$((done_frames * 100 / total))
                remaining=$((total - done_frames))
                # Extract fps from log
                fps=$(grep -o "[0-9.]\+ fps" ~/enhance.log 2>/dev/null | tail -1 | awk "{print \$1}")
                if [ -n "$fps" ] && [ "$fps" != "0" ] && [ "$done_frames" -gt 0 ]; then
                    eta_min=$(echo "$remaining / $fps / 60" | bc 2>/dev/null)
                    if [ -n "$eta_min" ] && [ "$eta_min" -gt 60 ] 2>/dev/null; then
                        eta_h=$((eta_min / 60))
                        eta_m=$((eta_min % 60))
                        eta_str="${eta_h}h ${eta_m}m"
                    elif [ -n "$eta_min" ] 2>/dev/null; then
                        eta_str="${eta_min}m"
                    else
                        eta_str="calculating..."
                    fi
                    echo "Video: $video_id"
                    echo "  Frames: $done_frames / $total ($pct%) | ${fps} fps | ETA: $eta_str"
                else
                    echo "Video: $video_id"
                    echo "  Frames: $done_frames / $total ($pct%) | ETA: calculating..."
                fi
            else
                echo "Video: $video_id (extracting frames...)"
            fi
        done
        echo ""
        if ps aux | grep -v grep | grep enhance_gpu.py > /dev/null; then
            echo "Process: RUNNING"
        else
            echo "Process: NOT RUNNING"
        fi
        echo ""
        echo "Last log:"
        tail -3 ~/enhance.log 2>/dev/null || echo "  (no log found)"
        echo ""
        echo "Disk: $(df -h ~ | tail -1 | awk "{print \$3 \" used / \" \$4 \" free\"}")"
    '
    exit 0
fi

URL="$1"
SCALE="${2:-4}"
PROJECT="${3:-old2new-davaz}"
ZONE="${4:-us-central1-a}"
INSTANCE="old2new-gpu"
MACHINE_TYPE="g2-standard-4"
GPU_TYPE="nvidia-l4"
IMAGE="pytorch-2-7-cu128-ubuntu-2204-nvidia-570-v20260305"

if [ ! -x "$GCLOUD" ]; then
    echo "Error: gcloud CLI not found. Install with: brew install --cask google-cloud-sdk"
    exit 1
fi

# --- Estimate disk size from video before creating instance ---
echo "Checking video properties..."
if command -v yt-dlp &>/dev/null; then
    VIDEO_INFO=$(yt-dlp --print "%(duration)s %(width)s %(height)s %(fps)s" "$URL" 2>/dev/null)
    if [ -n "$VIDEO_INFO" ]; then
        VID_DURATION=$(echo "$VIDEO_INFO" | awk '{print $1}')
        VID_WIDTH=$(echo "$VIDEO_INFO" | awk '{print $2}')
        VID_HEIGHT=$(echo "$VIDEO_INFO" | awk '{print $3}')
        VID_FPS=$(echo "$VIDEO_INFO" | awk '{print $4}' | cut -d. -f1)
        VID_FPS=${VID_FPS:-25}
        TOTAL_FRAMES=$(( ${VID_DURATION%.*} * VID_FPS ))
        INPUT_GB=$(echo "$TOTAL_FRAMES * $VID_WIDTH * $VID_HEIGHT * 3 / 3 / 1073741824" | bc)
        OUTPUT_GB=$(echo "$TOTAL_FRAMES * $VID_WIDTH * $SCALE * $VID_HEIGHT * $SCALE * 3 / 3 / 1073741824" | bc)
        NEEDED_GB=$(( INPUT_GB + OUTPUT_GB + 10 ))
        # GCP SSD quota is 500GB max by default
        if [ "$NEEDED_GB" -gt 500 ]; then
            echo ""
            echo "WARNING: This video needs ~${NEEDED_GB}GB disk at ${SCALE}x upscale."
            echo "  Source: ${VID_WIDTH}x${VID_HEIGHT}, ${VID_DURATION}s, ~${TOTAL_FRAMES} frames"
            echo "  Output: $((VID_WIDTH * SCALE))x$((VID_HEIGHT * SCALE))"
            echo "  GCP default SSD quota: 500GB"
            echo ""
            # Suggest 2x if 4x is too large
            if [ "$SCALE" -eq 4 ]; then
                OUTPUT_2X_GB=$(echo "$TOTAL_FRAMES * $VID_WIDTH * 2 * $VID_HEIGHT * 2 * 3 / 3 / 1073741824" | bc)
                NEEDED_2X_GB=$(( INPUT_GB + OUTPUT_2X_GB + 10 ))
                echo "  At 2x upscale: ~${NEEDED_2X_GB}GB needed ($((VID_WIDTH * 2))x$((VID_HEIGHT * 2)))"
                echo ""
                read -p "Switch to 2x upscale? [Y/n] " SWITCH
                if [ "$SWITCH" != "n" ] && [ "$SWITCH" != "N" ]; then
                    SCALE=2
                    NEEDED_GB=$NEEDED_2X_GB
                    echo "Switched to 2x upscale."
                fi
            fi
        fi
        # Cap at 500, minimum 200
        DISK_GB=$(( NEEDED_GB > 500 ? 500 : (NEEDED_GB < 200 ? 200 : NEEDED_GB) ))
        DISK_SIZE="${DISK_GB}GB"
        echo "Video: ${VID_WIDTH}x${VID_HEIGHT} @ ${VID_FPS}fps, ${VID_DURATION}s (~${TOTAL_FRAMES} frames)"
        echo "Scale: ${SCALE}x -> $((VID_WIDTH * SCALE))x$((VID_HEIGHT * SCALE))"
        echo "Estimated disk: ~${NEEDED_GB}GB (using ${DISK_SIZE})"
    else
        echo "Could not fetch video info, using default 200GB disk."
        DISK_SIZE="200GB"
    fi
else
    echo "yt-dlp not installed locally, using default 200GB disk."
    DISK_SIZE="200GB"
fi
echo ""

echo "=== old2new Google Cloud GPU Setup ==="
echo "Project:  $PROJECT"
echo "Zone:     $ZONE"
echo "GPU:      $GPU_TYPE"
echo "Video:    $URL"
echo ""

# --- Check if instance already exists ---
EXISTING=$("$GCLOUD" compute instances describe "$INSTANCE" --project="$PROJECT" --zone="$ZONE" --format="value(status)" 2>/dev/null || echo "")
if [ -n "$EXISTING" ]; then
    echo "Instance '$INSTANCE' already exists (status: $EXISTING)."
    read -p "Delete and recreate? [y/N] " CONFIRM
    if [ "$CONFIRM" = "y" ] || [ "$CONFIRM" = "Y" ]; then
        echo "Deleting existing instance..."
        "$GCLOUD" compute instances delete "$INSTANCE" --project="$PROJECT" --zone="$ZONE" --quiet
        EXISTING=""
    else
        echo "Reusing existing instance."
    fi
fi

# --- Create instance if needed ---
if [ -z "$EXISTING" ]; then
    echo "Creating GPU instance..."
    "$GCLOUD" compute instances create "$INSTANCE" \
        --project="$PROJECT" \
        --zone="$ZONE" \
        --machine-type="$MACHINE_TYPE" \
        --accelerator="type=$GPU_TYPE,count=1" \
        --image="$IMAGE" \
        --image-project=deeplearning-platform-release \
        --boot-disk-size="$DISK_SIZE" \
        --maintenance-policy=TERMINATE \
        --metadata="install-nvidia-driver=True"
    echo "Instance created. Waiting for SSH..."
    sleep 30
fi

SSH_CMD="$GCLOUD compute ssh $INSTANCE --project=$PROJECT --zone=$ZONE"

# --- Install dependencies ---
echo ""
echo "Installing dependencies..."
$SSH_CMD --command='
set -e

# Install system deps for OpenCV and ffmpeg
sudo apt-get update -qq
sudo apt-get install -y -qq libgl1 libglib2.0-0 > /dev/null 2>&1

# Install static ffmpeg (apt version has broken deps on GCP DL images)
if ! command -v ffprobe &>/dev/null; then
    echo "Installing ffmpeg..."
    wget -q https://johnvansickle.com/ffmpeg/releases/ffmpeg-release-amd64-static.tar.xz -O /tmp/ffmpeg.tar.xz
    tar xf /tmp/ffmpeg.tar.xz -C /tmp
    sudo cp /tmp/ffmpeg-*-amd64-static/ffmpeg /usr/local/bin/
    sudo cp /tmp/ffmpeg-*-amd64-static/ffprobe /usr/local/bin/
    rm -rf /tmp/ffmpeg*
fi

# Uninstall conflicting opencv versions, install headless
pip uninstall -y opencv-python opencv-contrib-python 2>/dev/null
pip install opencv-python-headless -q 2>&1 | tail -1

# Install Real-ESRGAN and pinned deps
pip install realesrgan yt-dlp "numpy<2" "torchvision==0.15.2" "basicsr==1.4.2" -q 2>&1 | tail -1

# Verify everything works
echo ""
python3 -c "
import cv2
from basicsr.archs.rrdbnet_arch import RRDBNet
import torch
print(f\"Python deps OK (torch={torch.__version__}, CUDA={torch.cuda.is_available()})\")
"
ffprobe -version 2>&1 | head -1
nvidia-smi --query-gpu=name,memory.total --format=csv,noheader
echo ""
echo "SETUP DONE"
'

# --- Download and run enhancement ---
echo ""
echo "Downloading enhance script..."
$SSH_CMD --command='
wget -q --no-cache "https://raw.githubusercontent.com/zdavatz/old2new/main/enhance_gpu.py?$(date +%s)" -O ~/enhance_gpu.py
# Verify the script does not have hardcoded /root paths
if grep -q "f\"/root/jobs" ~/enhance_gpu.py; then
    echo "WARNING: Fixing hardcoded /root path in enhance_gpu.py"
    sed -i "s|f\"/root/jobs/{VIDEO_ID}\"|os.path.join(os.path.expanduser(\"~\"), \"jobs\", VIDEO_ID)|g" ~/enhance_gpu.py
fi
echo "Script downloaded and verified"
'

echo ""
echo "Starting enhancement..."
$SSH_CMD --command="
export PATH=/usr/local/bin:\$HOME/.local/bin:\$PATH
nohup python3 -u ~/enhance_gpu.py \"$URL\" $SCALE > ~/enhance.log 2>&1 &
sleep 2
if ps aux | grep -v grep | grep enhance_gpu.py > /dev/null; then
    echo \"Enhancement started successfully\"
else
    echo \"ERROR: Enhancement failed to start. Check log:\"
    cat ~/enhance.log
    exit 1
fi
"

echo ""
echo "=== Setup Complete ==="
echo ""
echo "Monitor progress:"
echo "  $SSH_CMD --command='tail -20 ~/enhance.log'"
echo ""
echo "Check frame count:"
echo "  $SSH_CMD --command='ls ~/jobs/*/frames_out/ 2>/dev/null | wc -l'"
echo ""
echo "Download result when done:"
echo "  $GCLOUD compute scp $INSTANCE:~/jobs/*/enhanced_*.mkv . --project=$PROJECT --zone=$ZONE"
echo ""
echo "DELETE instance when done to stop billing:"
echo "  $GCLOUD compute instances delete $INSTANCE --project=$PROJECT --zone=$ZONE"
