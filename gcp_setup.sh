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
    else
        echo "Reusing existing instance."
    fi
fi

# --- Create instance if needed ---
if [ -z "$EXISTING" ] || [ "$CONFIRM" = "y" ] || [ "$CONFIRM" = "Y" ]; then
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

# --- Setup function ---
echo ""
echo "Installing dependencies..."
"$GCLOUD" compute ssh "$INSTANCE" --project="$PROJECT" --zone="$ZONE" --command='
set -e

# Install Python deps (uninstall opencv-python first to avoid libGL conflict)
pip uninstall -y opencv-python -q 2>/dev/null
pip install realesrgan yt-dlp "numpy<2" "torchvision==0.15.2" "basicsr==1.4.2" opencv-python-headless -q 2>&1 | tail -1

# Install static ffmpeg (apt version has broken deps on DL images)
if ! command -v ffprobe &>/dev/null; then
    wget -q https://johnvansickle.com/ffmpeg/releases/ffmpeg-release-amd64-static.tar.xz -O /tmp/ffmpeg.tar.xz
    tar xf /tmp/ffmpeg.tar.xz -C /tmp
    sudo cp /tmp/ffmpeg-*-amd64-static/ffmpeg /usr/local/bin/
    sudo cp /tmp/ffmpeg-*-amd64-static/ffprobe /usr/local/bin/
    rm -rf /tmp/ffmpeg*
fi

# Verify
python3 -c "from basicsr.archs.rrdbnet_arch import RRDBNet; print(\"Python deps OK\")"
ffprobe -version 2>&1 | head -1
nvidia-smi --query-gpu=name,memory.total --format=csv,noheader
echo "SETUP DONE"
'

# --- Upload and run script ---
echo ""
echo "Downloading enhance script..."
"$GCLOUD" compute ssh "$INSTANCE" --project="$PROJECT" --zone="$ZONE" --command='
wget -q https://raw.githubusercontent.com/zdavatz/old2new/main/enhance_gpu.py -O ~/enhance_gpu.py
echo "Script downloaded"
'

echo ""
echo "Starting enhancement..."
"$GCLOUD" compute ssh "$INSTANCE" --project="$PROJECT" --zone="$ZONE" --command="
export PATH=/usr/local/bin:\$HOME/.local/bin:\$PATH
nohup python3 -u ~/enhance_gpu.py \"$URL\" 4 > ~/enhance.log 2>&1 &
echo \"Enhancement started (PID: \$!)\"
"

echo ""
echo "=== Setup Complete ==="
echo ""
echo "Monitor progress:"
echo "  $GCLOUD compute ssh $INSTANCE --project=$PROJECT --zone=$ZONE --command='tail -20 ~/enhance.log'"
echo ""
echo "Check frame count:"
echo "  $GCLOUD compute ssh $INSTANCE --project=$PROJECT --zone=$ZONE --command='ls ~/jobs/*/frames_out/ | wc -l'"
echo ""
echo "Download result when done:"
echo "  $GCLOUD compute scp $INSTANCE:~/jobs/*/enhanced_*.mkv . --project=$PROJECT --zone=$ZONE"
echo ""
echo "DELETE instance when done to stop billing:"
echo "  $GCLOUD compute instances delete $INSTANCE --project=$PROJECT --zone=$ZONE"
