#!/bin/bash
set -e
export DEBIAN_FRONTEND=noninteractive

exec > >(tee -a /root/benchmark.log) 2>&1
echo "=== RTX 5090 Optimization Benchmark started at $(date) ==="

# Setup
echo ""
echo "=== Setup ==="
apt-get update -qq && apt-get install -y -qq xz-utils curl > /dev/null 2>&1
pip install -q realesrgan "numpy==1.26.4" "basicsr==1.4.2" 2>&1 | tail -2
pip uninstall -y opencv-python opencv-contrib-python 2>/dev/null || true
pip install -q "opencv-python-headless==4.10.0.84" 2>&1 | tail -1
pip install -q "numpy==1.26.4" 2>&1 | tail -1

DEGRADATIONS_FILE=$(python3 -c "import importlib.util; print(importlib.util.find_spec('basicsr').submodule_search_locations[0])" 2>/dev/null)/data/degradations.py
[ -f "$DEGRADATIONS_FILE" ] && sed -i 's/from torchvision.transforms.functional_tensor import rgb_to_grayscale/from torchvision.transforms.functional import rgb_to_grayscale/' "$DEGRADATIONS_FILE"

echo "Setup done."

# GPU Info
echo ""
echo "=== GPU Info ==="
nvidia-smi --query-gpu=name,memory.total,power.limit,clocks.max.graphics,compute_cap --format=csv

# Run all benchmarks
echo ""
echo "=== Running Benchmarks ==="
python3 -u -c "
import torch, time, cv2, numpy as np, sys, os

print(f'PyTorch: {torch.__version__}, CUDA: {torch.cuda.is_available()}')
print(f'GPU: {torch.cuda.get_device_name(0)}')
print(f'VRAM: {torch.cuda.get_device_properties(0).total_memory / 1024**3:.0f} GB')
print()

# Create test frame 1920x1200
img = np.random.randint(0, 255, (1200, 1920, 3), dtype=np.uint8)
print('Test frame: 1920x1200 (2.3 MP)')
print()

from realesrgan import RealESRGANer
from basicsr.archs.rrdbnet_arch import RRDBNet

model = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)

def benchmark(label, upsampler, rounds=3):
    # warmup
    upsampler.enhance(img, outscale=2)
    torch.cuda.synchronize()
    times = []
    for i in range(rounds):
        torch.cuda.synchronize()
        t0 = time.time()
        upsampler.enhance(img, outscale=2)
        torch.cuda.synchronize()
        t1 = time.time()
        times.append(t1 - t0)
    avg = sum(times) / len(times)
    best = min(times)
    import subprocess
    r = subprocess.run(['nvidia-smi','--query-gpu=power.draw','--format=csv,noheader,nounits'], capture_output=True, text=True)
    power = r.stdout.strip()
    print(f'{label:40} avg={avg:.2f}s ({1/avg:.2f} fps)  best={best:.2f}s ({1/best:.2f} fps)  power={power}W')
    sys.stdout.flush()
    return avg

print('=' * 80)
print('TEST 1: Tile size comparison (FP16)')
print('=' * 80)
for tile in [256, 384, 512, 768]:
    up = RealESRGANer(scale=4, model_path='https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth',
        model=model, tile=tile, tile_pad=10, pre_pad=0, half=True, gpu_id=0)
    benchmark(f'tile={tile} (FP16)', up)

print()
print('=' * 80)
print('TEST 2: FP32 vs FP16')
print('=' * 80)
up_fp16 = RealESRGANer(scale=4, model_path='https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth',
    model=model, tile=512, tile_pad=10, pre_pad=0, half=True, gpu_id=0)
benchmark('tile=512 FP16', up_fp16)

model_fp32 = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
up_fp32 = RealESRGANer(scale=4, model_path='https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth',
    model=model_fp32, tile=512, tile_pad=10, pre_pad=0, half=False, gpu_id=0)
benchmark('tile=512 FP32', up_fp32)

print()
print('=' * 80)
print('TEST 3: tile_pad comparison')
print('=' * 80)
for pad in [0, 5, 10, 20]:
    model_pad = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
    up_pad = RealESRGANer(scale=4, model_path='https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth',
        model=model_pad, tile=512, tile_pad=pad, pre_pad=0, half=True, gpu_id=0)
    benchmark(f'tile=512 pad={pad}', up_pad)

print()
print('=' * 80)
print('TEST 4: torch.compile() optimization')
print('=' * 80)
try:
    model_compile = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
    up_compile = RealESRGANer(scale=4, model_path='https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth',
        model=model_compile, tile=512, tile_pad=10, pre_pad=0, half=True, gpu_id=0)
    # Compile the model
    up_compile.model = torch.compile(up_compile.model, mode='reduce-overhead')
    print('torch.compile(reduce-overhead) applied')
    benchmark('tile=512 FP16 + torch.compile', up_compile, rounds=5)
except Exception as e:
    print(f'torch.compile failed: {e}')

print()
try:
    model_compile2 = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
    up_compile2 = RealESRGANer(scale=4, model_path='https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth',
        model=model_compile2, tile=512, tile_pad=10, pre_pad=0, half=True, gpu_id=0)
    up_compile2.model = torch.compile(up_compile2.model, mode='max-autotune')
    print('torch.compile(max-autotune) applied — may take a few minutes to compile...')
    benchmark('tile=512 FP16 + torch.compile(autotune)', up_compile2, rounds=5)
except Exception as e:
    print(f'torch.compile(max-autotune) failed: {e}')

print()
print('=' * 80)
print('TEST 5: torch.backends optimizations')
print('=' * 80)
torch.backends.cudnn.benchmark = True
torch.backends.cuda.matmul.allow_tf32 = True
torch.backends.cudnn.allow_tf32 = True
print('Enabled: cudnn.benchmark=True, TF32=True')
model_tf32 = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
up_tf32 = RealESRGANer(scale=4, model_path='https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth',
    model=model_tf32, tile=512, tile_pad=10, pre_pad=0, half=True, gpu_id=0)
benchmark('tile=512 FP16 + cudnn.bench + TF32', up_tf32)

print()
print('=' * 80)
print('TEST 6: Different input resolutions (tile=512)')
print('=' * 80)
for res_name, w, h in [('SD 640x480', 640, 480), ('HD 960x720', 960, 720), ('HD 1280x720', 1280, 720), ('HD 1920x1080', 1920, 1080), ('HD 1920x1200', 1920, 1200)]:
    img_test = np.random.randint(0, 255, (h, w, 3), dtype=np.uint8)
    model_res = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
    up_res = RealESRGANer(scale=4, model_path='https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth',
        model=model_res, tile=512, tile_pad=10, pre_pad=0, half=True, gpu_id=0)
    # Override img for this test
    _img_backup = img
    img = img_test
    benchmark(f'{res_name} tile=512 FP16', up_res)
    img = _img_backup

print()
print('=' * 80)
print('ALL BENCHMARKS DONE')
print('=' * 80)
"

echo ""
echo "=== Benchmark complete at $(date) ==="
