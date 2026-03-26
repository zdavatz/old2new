#!/usr/bin/env python3
"""Export Real-ESRGAN model as TorchScript for Rust upscaler."""
from basicsr.archs.rrdbnet_arch import RRDBNet
import torch, site, os

model = RRDBNet(num_in_ch=3, num_out_ch=3, num_feat=64, num_block=23, num_grow_ch=32, scale=4)
wp = os.path.join(site.getsitepackages()[0], 'weights', 'RealESRGAN_x4plus.pth')
ln = torch.load(wp, map_location='cpu', weights_only=False)
model.load_state_dict(ln.get('params_ema', ln.get('params', ln)), strict=True)
model.eval()
traced = torch.jit.trace(model, torch.randn(1, 3, 64, 64))
traced.save('/root/RealESRGAN_x4plus.pt')
print(f'TorchScript model: {os.path.getsize("/root/RealESRGAN_x4plus.pt") / 1024 / 1024:.1f} MB')
