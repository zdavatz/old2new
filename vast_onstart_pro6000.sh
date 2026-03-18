#!/bin/bash
set -e
export DEBIAN_FRONTEND=noninteractive

exec > >(tee -a /root/enhance.log) 2>&1
echo "=== Onstart started at $(date) ==="

# Step 1: System packages (apt)
echo ""
echo "=== Step 1: System packages ==="
apt-get update -qq
apt-get install -y -qq xz-utils curl git > /dev/null 2>&1
echo "System packages installed."

# Step 2: Static ffmpeg 7.x
echo ""
echo "=== Step 2: Static ffmpeg 7.x ==="
curl -sL https://johnvansickle.com/ffmpeg/releases/ffmpeg-release-amd64-static.tar.xz | tar xJ -C /tmp/
cp /tmp/ffmpeg-*-amd64-static/ffmpeg /opt/conda/bin/ffmpeg
cp /tmp/ffmpeg-*-amd64-static/ffprobe /opt/conda/bin/ffprobe
ffmpeg -version 2>&1 | head -1
echo "ffmpeg installed."

# Step 3: Python packages
echo ""
echo "=== Step 3: Python packages ==="

# Detect GPU arch — Blackwell (sm_120+) needs PyTorch with CUDA 12.8
GPU_ARCH=$(nvidia-smi --query-gpu=compute_cap --format=csv,noheader 2>/dev/null | head -1 | tr -d '.')
if [ "${GPU_ARCH:-0}" -ge 120 ]; then
    echo "Blackwell GPU detected (sm_${GPU_ARCH}) — upgrading PyTorch to CUDA 12.8"
    pip install -q torch torchvision --index-url https://download.pytorch.org/whl/cu128 2>&1 | tail -3
fi

python3 -c "import torch; print(f'PyTorch: {torch.__version__}, CUDA: {torch.cuda.is_available()}')"

# Install realesrgan and deps
pip install -q realesrgan yt-dlp "numpy==1.26.4" "basicsr==1.4.2" 2>&1 | tail -3

# Fix opencv: remove full, install headless
pip uninstall -y opencv-python opencv-contrib-python 2>/dev/null || true
pip install -q "opencv-python-headless==4.10.0.84" 2>&1 | tail -2

# Re-pin numpy
pip install -q "numpy==1.26.4" 2>&1 | tail -1

# Google API for YouTube upload + email
pip install -q google-api-python-client google-auth-oauthlib google-auth-httplib2 2>&1 | tail -1

echo "Python packages installed."

# Step 4: Patch basicsr for newer torchvision
echo ""
echo "=== Step 4: Patch basicsr ==="
DEGRADATIONS_FILE=$(python3 -c "import importlib.util; print(importlib.util.find_spec('basicsr').submodule_search_locations[0])" 2>/dev/null)/data/degradations.py
if [ -f "$DEGRADATIONS_FILE" ] && grep -q "functional_tensor" "$DEGRADATIONS_FILE"; then
    sed -i 's/from torchvision.transforms.functional_tensor import rgb_to_grayscale/from torchvision.transforms.functional import rgb_to_grayscale/' "$DEGRADATIONS_FILE"
    echo "Patched basicsr for torchvision compatibility."
else
    echo "basicsr already patched or not needed."
fi

# Step 5: Download latest scripts from GitHub
echo ""
echo "=== Step 5: Download scripts ==="
curl -sL -H "Accept: application/vnd.github.v3.raw" "https://api.github.com/repos/zdavatz/old2new/contents/enhance_gpu.py" -o /root/enhance_gpu.py
curl -sL -H "Accept: application/vnd.github.v3.raw" "https://api.github.com/repos/zdavatz/old2new/contents/youtube_upload.py" -o /root/youtube_upload.py
curl -sL -H "Accept: application/vnd.github.v3.raw" "https://api.github.com/repos/zdavatz/old2new/contents/status_server.py" -o /root/status_server.py
echo "Scripts downloaded."

# Step 6: Verify everything works
echo ""
echo "=== Step 6: Verification ==="
python3 -c "
import torch
print(f'PyTorch: {torch.__version__}')
print(f'CUDA: {torch.cuda.is_available()}, GPU: {torch.cuda.get_device_name(0)}')
print(f'VRAM: {torch.cuda.get_device_properties(0).total_memory / 1024**3:.0f} GB')
import cv2; print(f'OpenCV: {cv2.__version__}')
import numpy as np; print(f'NumPy: {np.__version__}')
from basicsr.archs.rrdbnet_arch import RRDBNet; print('BasicSR: OK')
from realesrgan import RealESRGANer; print('RealESRGAN: OK')
import subprocess
r = subprocess.run(['ffmpeg', '-version'], capture_output=True, text=True)
print(f'ffmpeg: {r.stdout.split(chr(10))[0]}')
r = subprocess.run(['yt-dlp', '--version'], capture_output=True, text=True)
print(f'yt-dlp: {r.stdout.strip()}')
print()
print('ALL CHECKS PASSED')
"

# Step 7: Write video queue and instance metadata
echo ""
echo "=== Step 7: Video queue ==="
cat > /root/video_queue.json << 'QJSON'
[
  {"id": "o_nM2N-03UI", "scale": 2, "title": "064_S_T_INGING_BEAUTY_Kamchatka_-_Russian_Esub", "display_title": "064 S(T)INGING BEAUTY Kamchatka - Russian/Esub", "duration": 6840}
]
QJSON

cat > /root/instance_meta.json << 'MJSON'
{
  "label": "davaz-singing-pro6000",
  "location": "Maryland, US",
  "cost_per_hr": 1.20,
  "provider": "vast.ai",
  "instance_id": "pending"
}
MJSON
echo "Queue and metadata written."

# Step 8: Start status server
echo ""
echo "=== Step 8: Start status server ==="
python3 /root/status_server.py &
echo "Status server started on port 8080"

# Step 9: Start enhancement
echo ""
echo "=== Step 9: Start enhancement ==="
python3 -u /root/enhance_gpu.py "https://www.youtube.com/watch?v=o_nM2N-03UI" 2 --job-name "064_S_T_INGING_BEAUTY_Kamchatka_-_Russian_Esub"

# Step 10: Upload to YouTube after completion
echo ""
echo "=== Step 10: YouTube upload ==="
ENHANCED_FILE=$(ls /root/jobs/064_S_T_INGING_BEAUTY_Kamchatka_-_Russian_Esub/064_S_T_INGING_BEAUTY_Kamchatka_-_Russian_Esub_2x.mkv /root/jobs/064_S_T_INGING_BEAUTY_Kamchatka_-_Russian_Esub/enhanced_2x.mkv 2>/dev/null | head -1)
if [ -n "$ENHANCED_FILE" ] && [ -f "/root/client_secret.json" ] && [ -f "/root/youtube_token.json" ]; then
    python3 /root/youtube_upload.py "o_nM2N-03UI" "$ENHANCED_FILE" \
        --client-secret /root/client_secret.json \
        --token /root/youtube_token.json
    echo "YouTube upload + email done at $(date)"
else
    echo "Skipping upload (missing credentials or enhanced file)"
fi

echo ""
echo "=== ALL DONE at $(date) ==="
