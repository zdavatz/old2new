#!/usr/bin/env python3
"""Export Real-ESRGAN model as TorchScript + ONNX for Rust upscaler."""
from basicsr.archs.rrdbnet_arch import RRDBNet
import torch, site, os

model = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
wp = os.path.join(site.getsitepackages()[0], 'weights', 'RealESRGAN_x4plus.pth')
ln = torch.load(wp, map_location='cpu', weights_only=False)
model.load_state_dict(ln.get('params_ema', ln.get('params', ln)), strict=True)
model.eval()

# TorchScript export
traced = torch.jit.trace(model, torch.randn(1, 3, 64, 64))
traced.save('/root/RealESRGAN_x4plus.pt')
print('TorchScript: %.1f MB' % (os.path.getsize('/root/RealESRGAN_x4plus.pt') / 1024 / 1024))

# ONNX export (legacy mode for full weight embedding)
dummy = torch.randn(1, 3, 64, 64)
torch.onnx.export(
    model, dummy, '/root/RealESRGAN_x4plus.onnx',
    input_names=['input'], output_names=['output'],
    dynamic_axes={'input': {2: 'height', 3: 'width'}, 'output': {2: 'height', 3: 'width'}},
    opset_version=17, dynamo=False,
)
print('ONNX: %.1f MB' % (os.path.getsize('/root/RealESRGAN_x4plus.onnx') / 1024 / 1024))
