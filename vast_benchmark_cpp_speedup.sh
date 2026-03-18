#!/bin/bash
set -e
export DEBIAN_FRONTEND=noninteractive

exec > >(tee -a /root/benchmark.log) 2>&1
echo "=== C++ Speedup Benchmark started at $(date) ==="

# Setup
echo ""
echo "=== Setup ==="
apt-get update -qq && apt-get install -y -qq xz-utils curl gcc g++ > /dev/null 2>&1

# Keep PyTorch from Docker image (2.7.0+cu128) — upgrading breaks torchvision compatibility
python3 -c "import torch; print(f'PyTorch: {torch.__version__}')"
pip install -q realesrgan "numpy==1.26.4" "basicsr==1.4.2" 2>&1 | tail -2
pip uninstall -y opencv-python opencv-contrib-python 2>/dev/null || true
pip install -q "opencv-python-headless==4.10.0.84" 2>&1 | tail -1
pip install -q "numpy==1.26.4" 2>&1 | tail -1

# Install TensorRT and ONNX Runtime (large downloads, ~10 min)
pip install -q torch-tensorrt --extra-index-url https://download.pytorch.org/whl/cu128 2>&1 | tail -3
pip install -q onnxruntime-gpu onnx 2>&1 | tail -3

DEGRADATIONS_FILE=$(python3 -c "import importlib.util; print(importlib.util.find_spec('basicsr').submodule_search_locations[0])" 2>/dev/null)/data/degradations.py
[ -f "$DEGRADATIONS_FILE" ] && sed -i 's/from torchvision.transforms.functional_tensor import rgb_to_grayscale/from torchvision.transforms.functional import rgb_to_grayscale/' "$DEGRADATIONS_FILE"

echo "Setup done."

# GPU Info
echo ""
echo "=== GPU Info ==="
nvidia-smi --query-gpu=name,memory.total,power.limit,clocks.max.graphics,compute_cap --format=csv

# Run benchmarks
echo ""
echo "=== Running C++ Speedup Benchmarks ==="
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

def benchmark(label, upsampler, rounds=5):
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
    print(f'{label:45} avg={avg:.2f}s ({1/avg:.2f} fps)  best={best:.2f}s ({1/best:.2f} fps)  power={power}W')
    sys.stdout.flush()
    return avg

# ============================================================
print('=' * 80)
print('TEST 1: Baseline — PyTorch ' + torch.__version__)
print('=' * 80)
model = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
up = RealESRGANer(scale=4, model_path='https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth',
    model=model, tile=512, tile_pad=10, pre_pad=0, half=True, gpu_id=0)
baseline = benchmark('Baseline tile=512 FP16', up)

# ============================================================
print()
print('=' * 80)
print('TEST 2: TorchScript JIT')
print('=' * 80)
try:
    model_jit = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
    up_jit = RealESRGANer(scale=4, model_path='https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth',
        model=model_jit, tile=512, tile_pad=10, pre_pad=0, half=True, gpu_id=0)

    # Trace the model with a sample input
    sample = torch.rand(1, 3, 512, 512).half().cuda()
    traced = torch.jit.trace(up_jit.model, sample)
    traced = torch.jit.optimize_for_inference(traced)
    up_jit.model = traced
    print('TorchScript traced + optimize_for_inference applied')
    benchmark('TorchScript JIT tile=512 FP16', up_jit)
except Exception as e:
    print(f'TorchScript failed: {e}')

# ============================================================
print()
print('=' * 80)
print('TEST 3: torch.compile() with gcc')
print('=' * 80)
try:
    model_comp = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
    up_comp = RealESRGANer(scale=4, model_path='https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth',
        model=model_comp, tile=512, tile_pad=10, pre_pad=0, half=True, gpu_id=0)
    up_comp.model = torch.compile(up_comp.model, mode='reduce-overhead')
    print('torch.compile(reduce-overhead) applied')
    # Extra warmup rounds for compilation
    for i in range(3):
        up_comp.enhance(img, outscale=2)
    benchmark('torch.compile(reduce-overhead)', up_comp)
except Exception as e:
    print(f'torch.compile failed: {e}')

try:
    model_comp2 = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
    up_comp2 = RealESRGANer(scale=4, model_path='https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth',
        model=model_comp2, tile=512, tile_pad=10, pre_pad=0, half=True, gpu_id=0)
    up_comp2.model = torch.compile(up_comp2.model, mode='max-autotune')
    print('torch.compile(max-autotune) applied — compiling kernels...')
    for i in range(3):
        up_comp2.enhance(img, outscale=2)
    benchmark('torch.compile(max-autotune)', up_comp2)
except Exception as e:
    print(f'torch.compile(max-autotune) failed: {e}')

# ============================================================
print()
print('=' * 80)
print('TEST 4: ONNX Runtime GPU')
print('=' * 80)
try:
    import onnxruntime as ort
    print(f'ONNX Runtime: {ort.__version__}')
    print(f'Providers: {ort.get_available_providers()}')

    # Export model to ONNX
    model_onnx = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
    up_onnx_src = RealESRGANer(scale=4, model_path='https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth',
        model=model_onnx, tile=0, tile_pad=0, pre_pad=0, half=False, gpu_id=0)

    dummy = torch.rand(1, 3, 512, 512).cuda()
    onnx_path = '/tmp/realesrgan.onnx'
    torch.onnx.export(up_onnx_src.model, dummy, onnx_path,
                       input_names=['input'], output_names=['output'],
                       dynamic_axes={'input': {2: 'height', 3: 'width'}, 'output': {2: 'height', 3: 'width'}},
                       opset_version=17)
    print(f'ONNX exported to {onnx_path}')

    # Run with ONNX Runtime CUDA
    sess_opts = ort.SessionOptions()
    sess_opts.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
    sess = ort.InferenceSession(onnx_path, sess_opts, providers=['CUDAExecutionProvider'])

    # Benchmark single tile (512x512) through ONNX
    tile_input = np.random.rand(1, 3, 512, 512).astype(np.float32)
    # warmup
    sess.run(None, {'input': tile_input})
    times = []
    for i in range(10):
        t0 = time.time()
        sess.run(None, {'input': tile_input})
        t1 = time.time()
        times.append(t1 - t0)
    avg_tile = sum(times) / len(times)
    # Estimate per-frame: 12 tiles per frame at 1920x1200
    est_frame = avg_tile * 12
    print(f'ONNX Runtime per tile: {avg_tile*1000:.0f}ms (est. per frame: {est_frame:.2f}s = {1/est_frame:.2f} fps)')
except Exception as e:
    print(f'ONNX Runtime failed: {e}')

# ============================================================
print()
print('=' * 80)
print('TEST 5: TensorRT')
print('=' * 80)
try:
    import torch_tensorrt
    print(f'torch-tensorrt: {torch_tensorrt.__version__}')

    model_trt = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
    up_trt = RealESRGANer(scale=4, model_path='https://github.com/xinntao/Real-ESRGAN/releases/download/v0.1.0/RealESRGAN_x4plus.pth',
        model=model_trt, tile=512, tile_pad=10, pre_pad=0, half=True, gpu_id=0)

    # Compile with TensorRT
    sample = torch.rand(1, 3, 512, 512).half().cuda()
    trt_model = torch_tensorrt.compile(up_trt.model,
        inputs=[torch_tensorrt.Input(shape=[1, 3, 512, 512], dtype=torch.half)],
        enabled_precisions={torch.half},
        truncate_long_and_double=True)
    up_trt.model = trt_model
    print('TensorRT compiled')
    benchmark('TensorRT tile=512 FP16', up_trt)
except Exception as e:
    print(f'TensorRT failed: {e}')

# ============================================================
print()
print('=' * 80)
print('SUMMARY')
print('=' * 80)
print(f'Baseline (PyTorch {torch.__version__}): {1/baseline:.2f} fps')
print('See individual test results above for comparison.')
print()
print('=' * 80)
print('ALL BENCHMARKS DONE')
print('=' * 80)
"

echo ""
echo "=== Benchmark complete at $(date) ==="
