#!/bin/bash
set -e
export DEBIAN_FRONTEND=noninteractive

exec > >(tee -a /root/enhance.log) 2>&1
echo "=== ncnn-vulkan Build & Benchmark started at $(date) ==="

# Step 1: System packages
echo ""
echo "=== Step 1: System packages ==="
apt-get update -qq
apt-get install -y -qq build-essential cmake git curl unzip \
    libvulkan-dev vulkan-tools libgomp1 \
    python3 python3-pip > /dev/null 2>&1
pip install -q opencv-python-headless numpy 2>&1 | tail -1
echo "System packages installed."

# Step 2: GPU info
echo ""
echo "=== Step 2: GPU info ==="
nvidia-smi --query-gpu=name,memory.total,power.limit,clocks.max.graphics,compute_cap --format=csv
echo ""
# Vulkan check
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/nvidia_icd.json vulkaninfo --summary 2>&1 | head -20 || echo "Vulkan summary failed"

# Step 3: Build ncnn from source
echo ""
echo "=== Step 3: Build ncnn ==="
cd /root
git clone --depth 1 https://github.com/Tencent/ncnn.git
cd ncnn
git submodule update --init
mkdir build && cd build
cmake -DCMAKE_BUILD_TYPE=Release \
      -DNCNN_VULKAN=ON \
      -DNCNN_BUILD_EXAMPLES=OFF \
      -DNCNN_BUILD_TESTS=OFF \
      -DNCNN_BUILD_BENCHMARK=OFF \
      ..
make -j$(nproc)
make install
echo "ncnn built successfully."

# Step 4: Build Real-ESRGAN-ncnn-vulkan from source
echo ""
echo "=== Step 4: Build Real-ESRGAN-ncnn-vulkan ==="
cd /root
git clone --depth 1 https://github.com/xinntao/Real-ESRGAN-ncnn-vulkan.git
cd Real-ESRGAN-ncnn-vulkan/src
# Download models
mkdir -p ../models
cd ../models
curl -sLO https://github.com/xinntao/Real-ESRGAN/releases/download/v0.2.5.0/realesrgan-ncnn-vulkan-20220424-ubuntu.zip
unzip -o realesrgan-ncnn-vulkan-20220424-ubuntu.zip 'models/*' 2>/dev/null || true
# Move models up if nested
[ -d models ] && mv models/* . && rmdir models 2>/dev/null || true
cd ..

# Build
mkdir -p build && cd build
cmake -DCMAKE_BUILD_TYPE=Release \
      -Dncnn_DIR=/root/ncnn/build/install/lib/cmake/ncnn \
      ../src
make -j$(nproc)
echo "Real-ESRGAN-ncnn-vulkan built successfully."
ls -la realesrgan-ncnn-vulkan

# Step 5: Create test frame (1920x1200)
echo ""
echo "=== Step 5: Create test frame ==="
python3 -c "
import numpy as np, cv2
img = np.random.randint(0, 255, (1200, 1920, 3), dtype=np.uint8)
cv2.imwrite('/root/test_frame.png', img)
print('Test frame: 1920x1200')
"

# Step 6: Benchmark ncnn-vulkan
echo ""
echo "=== Step 6: Benchmark ncnn-vulkan ==="
cd /root/Real-ESRGAN-ncnn-vulkan/build

# Copy models next to binary
cp /root/Real-ESRGAN-ncnn-vulkan/models/*.param . 2>/dev/null || true
cp /root/Real-ESRGAN-ncnn-vulkan/models/*.bin . 2>/dev/null || true

echo "--- tile=512, scale=2 ---"
time VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/nvidia_icd.json \
    ./realesrgan-ncnn-vulkan -i /root/test_frame.png -o /root/out_512.png \
    -s 2 -n realesrgan-x4plus -t 512 2>&1

echo ""
echo "--- tile=0 (no tile), scale=2 ---"
time VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/nvidia_icd.json \
    ./realesrgan-ncnn-vulkan -i /root/test_frame.png -o /root/out_notile.png \
    -s 2 -n realesrgan-x4plus -t 0 2>&1

echo ""
echo "--- tile=768, scale=2 ---"
time VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/nvidia_icd.json \
    ./realesrgan-ncnn-vulkan -i /root/test_frame.png -o /root/out_768.png \
    -s 2 -n realesrgan-x4plus -t 768 2>&1

# Step 7: PyTorch benchmark for comparison
echo ""
echo "=== Step 7: PyTorch benchmark (same frame, same GPU) ==="
pip install -q torch torchvision --index-url https://download.pytorch.org/whl/cu128 2>&1 | tail -2
pip install -q realesrgan "numpy==1.26.4" "basicsr==1.4.2" 2>&1 | tail -2
pip uninstall -y opencv-python 2>/dev/null || true
pip install -q "opencv-python-headless==4.10.0.84" 2>&1 | tail -1
pip install -q "numpy==1.26.4" 2>&1 | tail -1

# Patch basicsr
DEGRADATIONS_FILE=$(python3 -c "import importlib.util; print(importlib.util.find_spec('basicsr').submodule_search_locations[0])" 2>/dev/null)/data/degradations.py
[ -f "$DEGRADATIONS_FILE" ] && sed -i 's/from torchvision.transforms.functional_tensor import rgb_to_grayscale/from torchvision.transforms.functional import rgb_to_grayscale/' "$DEGRADATIONS_FILE" 2>/dev/null

python3 -c "
import torch, time, cv2, numpy as np
from realesrgan import RealESRGANer
from basicsr.archs.rrdbnet_arch import RRDBNet

img = cv2.imread('/root/test_frame.png', cv2.IMREAD_UNCHANGED)
model = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
upsampler = RealESRGANer(scale=4, model_path='https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth', model=model, tile=512, tile_pad=10, pre_pad=0, half=True, gpu_id=0)

# Warmup
upsampler.enhance(img, outscale=2)

# Benchmark
times = []
for i in range(3):
    t0 = time.time()
    upsampler.enhance(img, outscale=2)
    times.append(time.time() - t0)
avg = sum(times)/len(times)
print(f'PyTorch tile=512: {avg:.2f}s/frame = {1/avg:.2f} fps')
"

echo ""
echo "=== ALL BENCHMARKS DONE at $(date) ==="
