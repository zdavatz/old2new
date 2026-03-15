#!/bin/bash
set -e

# --- Usage ---
if [ -z "$1" ]; then
    echo "Usage: ./gcp_setup.sh <youtube-url> [project-id] [zone]"
    echo ""
    echo "Example: ./gcp_setup.sh \"https://www.youtube.com/watch?v=xyz123\""
    echo "         ./gcp_setup.sh \"https://www.youtube.com/watch?v=xyz123\" old2new-davaz us-central1-a"
    echo ""
    echo "Prerequisites:"
    echo "  - gcloud CLI installed (brew install --cask google-cloud-sdk)"
    echo "  - Authenticated (gcloud auth login)"
    echo "  - Project with billing enabled and GPUS_ALL_REGIONS quota >= 1"
    exit 1
fi

URL="$1"
PROJECT="${2:-old2new-davaz}"
ZONE="${3:-us-central1-a}"
INSTANCE="old2new-gpu"
MACHINE_TYPE="g2-standard-4"
GPU_TYPE="nvidia-l4"
IMAGE="pytorch-2-7-cu128-ubuntu-2204-nvidia-570-v20260305"
DISK_SIZE="200GB"

# Find gcloud
GCLOUD=$(which gcloud 2>/dev/null || echo "/opt/homebrew/share/google-cloud-sdk/bin/gcloud")
if [ ! -x "$GCLOUD" ]; then
    echo "Error: gcloud CLI not found. Install with: brew install --cask google-cloud-sdk"
    exit 1
fi

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
nohup python3 -u ~/enhance_gpu.py \"$URL\" 4 > ~/enhance.log 2>&1 &
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
