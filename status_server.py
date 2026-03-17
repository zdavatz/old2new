#!/usr/bin/env python3
"""Lightweight status web server for vast.ai enhancement instances."""
import http.server
import json
import os
import glob
import time
from datetime import datetime, timedelta

JOBS_DIR = os.path.expanduser("~/jobs")
QUEUE_FILE = os.path.expanduser("~/video_queue.json")
INSTANCE_META = os.path.expanduser("~/instance_meta.json")
PORT = 8080


def get_system_specs():
    """Gather GPU, CPU, disk, and memory specs from the system."""
    import subprocess
    specs = {}

    # GPU info via nvidia-smi
    try:
        result = subprocess.run(
            ["nvidia-smi", "--query-gpu=name,memory.total,memory.used,memory.free,temperature.gpu,utilization.gpu,driver_version,pci.bus_id",
             "--format=csv,noheader,nounits"],
            capture_output=True, text=True, timeout=5
        )
        if result.returncode == 0:
            parts = [p.strip() for p in result.stdout.strip().split(",")]
            if len(parts) >= 8:
                specs["gpu"] = {
                    "name": parts[0],
                    "vram_total_mb": int(parts[1]),
                    "vram_used_mb": int(parts[2]),
                    "vram_free_mb": int(parts[3]),
                    "temp_c": int(parts[4]),
                    "util_pct": int(parts[5]),
                    "driver": parts[6],
                    "pci_bus": parts[7],
                }
    except Exception:
        pass

    # CPU info
    try:
        cpu_name = ""
        cpu_cores = 0
        with open("/proc/cpuinfo") as f:
            for line in f:
                if line.startswith("model name") and not cpu_name:
                    cpu_name = line.split(":", 1)[1].strip()
                if line.startswith("processor"):
                    cpu_cores += 1
        specs["cpu"] = {"name": cpu_name, "cores": cpu_cores}
    except Exception:
        pass

    # CPU utilization from /proc/stat (snapshot)
    try:
        with open("/proc/loadavg") as f:
            parts = f.read().split()
            specs.setdefault("cpu", {})["load_1m"] = float(parts[0])
            specs.setdefault("cpu", {})["load_5m"] = float(parts[1])
            specs.setdefault("cpu", {})["load_15m"] = float(parts[2])
    except Exception:
        pass

    # Memory info
    try:
        mem = {}
        with open("/proc/meminfo") as f:
            for line in f:
                parts = line.split()
                if parts[0] == "MemTotal:":
                    mem["total_mb"] = int(parts[1]) // 1024
                elif parts[0] == "MemAvailable:":
                    mem["available_mb"] = int(parts[1]) // 1024
                elif parts[0] == "MemFree:":
                    mem["free_mb"] = int(parts[1]) // 1024
        if "total_mb" in mem:
            mem["used_mb"] = mem["total_mb"] - mem.get("available_mb", mem.get("free_mb", 0))
        specs["memory"] = mem
    except Exception:
        pass

    # Disk info
    try:
        st = os.statvfs("/")
        total_gb = (st.f_blocks * st.f_frsize) / (1024**3)
        free_gb = (st.f_bavail * st.f_frsize) / (1024**3)
        used_gb = total_gb - free_gb
        specs["disk"] = {
            "total_gb": round(total_gb, 1),
            "used_gb": round(used_gb, 1),
            "free_gb": round(free_gb, 1),
            "util_pct": round(used_gb / total_gb * 100, 1) if total_gb > 0 else 0,
        }
    except Exception:
        pass

    # CUDA version
    try:
        result = subprocess.run(["nvcc", "--version"], capture_output=True, text=True, timeout=5)
        if result.returncode == 0:
            import re
            m = re.search(r"release ([\d.]+)", result.stdout)
            if m:
                specs.setdefault("gpu", {})["cuda"] = m.group(1)
    except Exception:
        pass

    return specs

class StatusHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        pass  # suppress access logs

    def do_GET(self):
        if self.path == "/api/status":
            self.send_json(self.get_status())
        elif self.path == "/":
            self.send_html(self.render_page())
        elif self.path.startswith("/compare/"):
            # /compare/TITLE shows side-by-side frame comparison
            title = self.path.split("/compare/", 1)[1].split("?")[0]
            frame = self.path.split("frame=")[1] if "frame=" in self.path else None
            self.send_html(self.render_compare(title, frame))
        elif self.path.startswith("/frames/"):
            # /frames/TITLE/in/frame_00000001.png or /frames/TITLE/out/frame_00000001.png
            self.serve_frame()
        elif self.path.startswith("/download/"):
            # /download/TITLE serves the enhanced video file
            self.serve_video()
        else:
            self.send_error(404)

    def serve_video(self):
        """Serve enhanced video file for download."""
        title = self.path.split("/download/", 1)[1]
        job_dir = os.path.join(JOBS_DIR, title)
        # Find the enhanced video file (try both naming schemes)
        filepath = None
        for pattern in [f"{title}_*x.mkv", "enhanced_*x.mkv", f"{title}_*x.mp4", "*.mkv"]:
            matches = glob.glob(os.path.join(job_dir, pattern))
            # Exclude original input
            matches = [m for m in matches if "enhanced" in os.path.basename(m) or "_2x" in os.path.basename(m) or "_4x" in os.path.basename(m)]
            if matches:
                filepath = matches[0]
                break
        if not filepath or not os.path.exists(filepath):
            self.send_error(404, "Enhanced video not found")
            return
        filename = os.path.basename(filepath)
        filesize = os.path.getsize(filepath)
        self.send_response(200)
        self.send_header("Content-Type", "video/x-matroska")
        self.send_header("Content-Length", filesize)
        self.send_header("Content-Disposition", f'attachment; filename="{filename}"')
        self.end_headers()
        with open(filepath, "rb") as f:
            while True:
                chunk = f.read(1024 * 1024)  # 1MB chunks
                if not chunk:
                    break
                self.wfile.write(chunk)

    def serve_frame(self):
        """Serve frame images from jobs directory."""
        import mimetypes
        parts = self.path.split("/frames/", 1)[1].split("/", 2)
        if len(parts) < 3:
            self.send_error(404)
            return
        title, direction, filename = parts[0], parts[1], parts[2]
        if direction == "in":
            filepath = os.path.join(JOBS_DIR, title, "frames_in", filename)
        elif direction == "out":
            filepath = os.path.join(JOBS_DIR, title, "frames_out", filename)
        else:
            self.send_error(404)
            return
        if not os.path.exists(filepath):
            self.send_error(404)
            return
        mime = mimetypes.guess_type(filepath)[0] or "application/octet-stream"
        with open(filepath, "rb") as f:
            data = f.read()
        self.send_response(200)
        self.send_header("Content-Type", mime)
        self.send_header("Content-Length", len(data))
        self.send_header("Cache-Control", "max-age=3600")
        self.end_headers()
        self.wfile.write(data)

    def send_json(self, data):
        body = json.dumps(data).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", len(body))
        self.end_headers()
        self.wfile.write(body)

    def send_html(self, html):
        body = html.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", len(body))
        self.end_headers()
        self.wfile.write(body)

    def get_instance_meta(self):
        """Read instance metadata (cost, location, label) from file."""
        if os.path.exists(INSTANCE_META):
            try:
                with open(INSTANCE_META) as f:
                    return json.load(f)
            except Exception:
                pass
        return {}

    def get_status(self):
        queue = []
        if os.path.exists(QUEUE_FILE):
            with open(QUEUE_FILE) as f:
                queue = json.load(f)

        videos = []
        for entry in queue:
            vid = entry["id"]
            title = entry["title"]  # used as directory name
            display_title = entry.get("display_title", title).replace('_', ' ')
            scale = entry["scale"]
            duration = entry["duration"]
            job_dir = os.path.join(JOBS_DIR, title)

            status = "queued"
            progress = 0
            total_frames = 0
            done_frames = 0
            eta = ""
            # Check both new naming (title_4x.mkv) and old naming (enhanced_4x.mkv)
            enhanced_file = os.path.join(job_dir, f"{title}_{scale}x.mkv")
            if not os.path.exists(enhanced_file):
                enhanced_file = os.path.join(job_dir, f"enhanced_{scale}x.mkv")

            if os.path.exists(enhanced_file):
                status = "done"
                progress = 100
                size_mb = os.path.getsize(enhanced_file) / (1024*1024)
                eta = f"{size_mb:.0f} MB"
            elif os.path.isdir(os.path.join(job_dir, "frames_out")):
                status = "upscaling"
                frames_in = glob.glob(os.path.join(job_dir, "frames_in", "frame_*.png"))
                frames_out = glob.glob(os.path.join(job_dir, "frames_out", "frame_*.png"))
                total_frames = len(frames_in)
                done_frames = len(frames_out)
                if total_frames > 0:
                    progress = round(done_frames / total_frames * 100, 1)
            elif os.path.isdir(os.path.join(job_dir, "frames_in")):
                status = "extracting"
                frames_in = glob.glob(os.path.join(job_dir, "frames_in", "frame_*.png"))
                total_frames = len(frames_in)
                if total_frames > 0:
                    status = "upscaling"
            elif os.path.exists(os.path.join(job_dir, "original.mkv")):
                status = "downloaded"

            # Detect input resolution via ffprobe
            resolution = ""
            input_file = os.path.join(job_dir, f"{title}.mkv")
            if not os.path.exists(input_file):
                for ext in ["mkv", "mp4", "webm"]:
                    candidates = [c for c in glob.glob(os.path.join(job_dir, f"*.{ext}")) if f"_{scale}x" not in c]
                    if candidates:
                        input_file = candidates[0]
                        break
            if os.path.exists(input_file):
                try:
                    import subprocess
                    r = subprocess.run(
                        ["ffprobe", "-v", "quiet", "-select_streams", "v:0",
                         "-show_entries", "stream=width,height",
                         "-of", "csv=p=0:s=x", input_file],
                        capture_output=True, text=True, timeout=5)
                    if r.returncode == 0 and "x" in r.stdout:
                        resolution = r.stdout.strip()
                except Exception:
                    pass

            videos.append({
                "id": vid,
                "title": title,
                "display_title": display_title,
                "scale": scale,
                "duration": duration,
                "status": status,
                "progress": progress,
                "total_frames": total_frames,
                "done_frames": done_frames,
                "eta": eta,
                "resolution": resolution,
            })

        # Read last lines of enhance.log
        log_tail = ""
        log_path = os.path.expanduser("~/enhance.log")
        if os.path.exists(log_path):
            with open(log_path, "rb") as f:
                f.seek(0, 2)
                size = f.tell()
                f.seek(max(0, size - 4096))
                log_tail = f.read().decode("utf-8", errors="replace")
                log_tail = "\n".join(log_tail.split("\n")[-30:])

        total = len(videos)
        done = sum(1 for v in videos if v["status"] == "done")
        active = [v for v in videos if v["status"] == "upscaling"]

        # Aggregate frame stats (estimate frames for queued videos from duration)
        total_frames_all = 0
        done_frames_all = 0
        est_fps_default = 25  # assume 25fps for queued videos without frames
        for v in videos:
            if v["total_frames"] > 0:
                total_frames_all += v["total_frames"]
            else:
                # Estimate from duration (queued/not-yet-extracted)
                total_frames_all += int(v["duration"] * est_fps_default)
            done_frames_all += v["done_frames"]
        overall_pct = round(done_frames_all / total_frames_all * 100, 1) if total_frames_all > 0 else 0

        # Extract speed info from log
        import re
        fps = 0
        eta_str = ""
        dl_speed = ""
        dl_progress = ""
        if log_tail:
            # Download speed from speed test
            dl_speed_match = re.findall(r'Download speed:\s*([\d.]+)\s*Mbps', log_tail)
            if dl_speed_match:
                dl_speed = f"{dl_speed_match[-1]} Mbps"

            # Video download progress (yt-dlp output)
            dl_pct_matches = re.findall(r'\[download\]\s+([\d.]+)%\s+of\s+[\d.]+\w+\s+at\s+([\d.]+\w+/s)', log_tail)
            if dl_pct_matches:
                dl_progress = f"{dl_pct_matches[-1][0]}% at {dl_pct_matches[-1][1]}"

            # Video download completed
            dl_done_match = re.findall(r'Downloaded\s+([\d.]+)\s*MB\s+in\s+([\d.]+)s\s+\(([\d.]+)\s*Mbps\)', log_tail)
            if dl_done_match:
                dl_speed = f"{dl_done_match[-1][2]} Mbps (actual)"
                dl_progress = ""

            fps_matches = re.findall(r'([\d.]+)\s+fps', log_tail)
            if fps_matches:
                fps = float(fps_matches[-1])
                remaining = total_frames_all - done_frames_all
                if fps > 0:
                    eta_secs = remaining / fps
                    eta_h = int(eta_secs // 3600)
                    eta_m = int((eta_secs % 3600) // 60)
                    if eta_h >= 24:
                        eta_d = eta_h // 24
                        eta_h_rem = eta_h % 24
                        eta_str = f"{eta_d}d {eta_h_rem}h {eta_m}m"
                    elif eta_h > 0:
                        eta_str = f"{eta_h}h {eta_m}m"
                    else:
                        eta_str = f"{eta_m}m"

        return {
            "total": total,
            "done": done,
            "active": active[0]["title"] if active else None,
            "videos": videos,
            "log_tail": log_tail,
            "timestamp": datetime.now().isoformat(),
            "total_frames": total_frames_all,
            "done_frames": done_frames_all,
            "overall_pct": overall_pct,
            "fps": fps,
            "eta": eta_str,
            "dl_speed": dl_speed,
            "dl_progress": dl_progress,
            "system": get_system_specs(),
            "instance": self.get_instance_meta(),
        }

    def render_page(self):
        return """<!DOCTYPE html>
<html><head>
<meta charset="utf-8">
<title>Da Vaz Video Enhancement</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="refresh" content="30">
<style>
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
         background: #0f172a; color: #e2e8f0; padding: 20px; }
  h1 { font-size: 1.5rem; margin-bottom: 4px; color: #f8fafc; }
  .subtitle { color: #94a3b8; margin-bottom: 20px; font-size: 0.9rem; }
  .summary { display: flex; gap: 16px; margin-bottom: 24px; flex-wrap: wrap; }
  .card { background: #1e293b; border-radius: 8px; padding: 16px 20px; min-width: 140px; }
  .card .num { font-size: 2rem; font-weight: 700; color: #38bdf8; }
  .card .label { color: #94a3b8; font-size: 0.85rem; }
  table { width: 100%; border-collapse: collapse; background: #1e293b; border-radius: 8px; overflow: hidden; }
  th { text-align: left; padding: 10px 12px; background: #334155; color: #94a3b8;
       font-size: 0.8rem; text-transform: uppercase; letter-spacing: 0.05em; }
  td { padding: 8px 12px; border-top: 1px solid #334155; font-size: 0.9rem; }
  tr:hover { background: #1e3a5f; }
  .bar-bg { background: #334155; border-radius: 4px; height: 20px; position: relative; overflow: hidden; min-width: 120px; }
  .bar-fg { height: 100%; border-radius: 4px; transition: width 0.5s; }
  .bar-text { position: absolute; top: 0; left: 8px; line-height: 20px; font-size: 0.75rem; font-weight: 600; }
  .status { display: inline-block; padding: 2px 8px; border-radius: 4px; font-size: 0.75rem; font-weight: 600; }
  .status-done { background: #065f46; color: #6ee7b7; }
  .status-upscaling { background: #1e3a5f; color: #38bdf8; }
  .status-extracting { background: #713f12; color: #fbbf24; }
  .status-downloaded { background: #3b0764; color: #c084fc; }
  .status-queued { background: #334155; color: #94a3b8; }
  .log { background: #0f172a; border: 1px solid #334155; border-radius: 8px; padding: 12px;
         font-family: monospace; font-size: 0.75rem; max-height: 300px; overflow-y: auto;
         white-space: pre-wrap; color: #94a3b8; margin-top: 20px; }
  .title-col { max-width: 350px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  a { color: #38bdf8; text-decoration: none; }
  a:hover { text-decoration: underline; }
  .instance-bar { background: #1e293b; border-radius: 8px; padding: 10px 18px; margin-bottom: 16px;
         display: flex; gap: 24px; flex-wrap: wrap; font-size: 0.85rem; }
  .instance-bar .item { display: flex; gap: 6px; }
  .instance-bar .label { color: #94a3b8; }
  .instance-bar .value { color: #e2e8f0; font-weight: 500; }
  .specs { display: grid; grid-template-columns: repeat(auto-fit, minmax(280px, 1fr)); gap: 12px; margin-bottom: 24px; }
  .spec-card { background: #1e293b; border-radius: 8px; padding: 14px 18px; }
  .spec-card h3 { font-size: 0.8rem; text-transform: uppercase; letter-spacing: 0.05em; color: #94a3b8; margin-bottom: 8px; }
  .spec-row { display: flex; justify-content: space-between; font-size: 0.85rem; padding: 2px 0; }
  .spec-row .label { color: #94a3b8; }
  .spec-row .value { color: #e2e8f0; font-weight: 500; }
  .spec-row .value.hot { color: #fb923c; }
  .spec-row .value.ok { color: #6ee7b7; }
</style>
</head><body>
<h1 id="page-title">Da Vaz Video Enhancement</h1>
<p class="subtitle" id="page-subtitle">Real-ESRGAN AI Upscaling &mdash; auto-refreshes every 30s</p>
<div id="app">Loading...</div>
<script>
async function update() {
  try {
    const r = await fetch('/api/status');
    const d = await r.json();
    const app = document.getElementById('app');
    const active = d.videos.filter(v => v.status === 'upscaling');
    const done = d.videos.filter(v => v.status === 'done');
    const queued = d.videos.filter(v => v.status === 'queued');
    const other = d.videos.filter(v => !['done','upscaling','queued'].includes(v.status));

    const fpsStr = d.fps > 0 ? d.fps.toFixed(1) + ' fps' : '—';
    const etaStr = d.eta || '—';
    const framesStr = d.done_frames + ' / ' + d.total_frames;
    const dlStr = d.dl_progress || d.dl_speed || '—';

    // Instance metadata bar
    let h = '';
    if (d.instance && Object.keys(d.instance).length > 0) {
      const i = d.instance;
      let items = '';
      if (i.label) items += `<div class="item"><span class="label">Instance:</span><span class="value">${i.label}</span></div>`;
      if (i.location) items += `<div class="item"><span class="label">Location:</span><span class="value">${i.location}</span></div>`;
      if (i.cost_per_hr) items += `<div class="item"><span class="label">Cost:</span><span class="value">$${i.cost_per_hr}/hr</span></div>`;
      if (i.provider) items += `<div class="item"><span class="label">Provider:</span><span class="value">${i.provider}</span></div>`;
      if (i.instance_id) items += `<div class="item"><span class="label">ID:</span><span class="value">${i.instance_id}</span></div>`;
      h += `<div class="instance-bar">${items}</div>`;
    }

    // Calculate cost estimates
    const costPerHr = d.instance && d.instance.cost_per_hr ? parseFloat(d.instance.cost_per_hr) : 0;
    let costRemaining = '—';
    let costTotal = '—';
    if (costPerHr > 0 && d.fps > 0) {
      // Remaining frames for ALL videos (not just active)
      const totalRemaining = d.total_frames - d.done_frames;
      const remainingHrs = totalRemaining / d.fps / 3600;
      costRemaining = '$' + (remainingHrs * costPerHr).toFixed(1);
      // Estimate total from all video durations
      const totalDurationSecs = d.videos.reduce((sum, v) => sum + (v.duration || 0), 0);
      // Rough: duration * fps_guess * cost_per_frame_hr
      // Better: use actual frames if known, else estimate
      if (d.total_frames > 0) {
        const totalHrs = d.total_frames / d.fps / 3600;
        costTotal = '$' + (totalHrs * costPerHr).toFixed(1);
      }
    }

    h += `<div class="summary">
      <div class="card"><div class="num">${d.total}</div><div class="label">Total Videos</div></div>
      <div class="card"><div class="num" style="color:#6ee7b7">${done.length}</div><div class="label">Completed</div></div>
      <div class="card"><div class="num" style="color:#38bdf8">${active.length}</div><div class="label">Upscaling Now</div></div>
      <div class="card"><div class="num" style="color:#94a3b8">${queued.length}</div><div class="label">Queued</div></div>
      <div class="card"><div class="num" style="font-size:1.2rem">${framesStr}</div><div class="label">Frames (${d.overall_pct}%)</div></div>
      <div class="card"><div class="num">${fpsStr}</div><div class="label">GPU Speed</div></div>
      <div class="card"><div class="num">${etaStr}</div><div class="label">ETA</div></div>
      <div class="card"><div class="num" style="color:#fbbf24">${costRemaining}</div><div class="label">Cost Remaining</div></div>
      <div class="card"><div class="num" style="font-size:1rem">${dlStr}</div><div class="label">Download</div></div>
    </div>`;

    // System specs panel
    if (d.system) {
      const s = d.system;
      h += '<div class="specs">';
      if (s.gpu) {
        const g = s.gpu;
        const vramPct = g.vram_total_mb > 0 ? ((g.vram_used_mb / g.vram_total_mb) * 100).toFixed(0) : 0;
        const tempClass = g.temp_c >= 80 ? 'hot' : 'ok';
        h += `<div class="spec-card"><h3>GPU</h3>
          <div class="spec-row"><span class="label">Model</span><span class="value">${g.name}</span></div>
          <div class="spec-row"><span class="label">VRAM</span><span class="value">${(g.vram_total_mb/1024).toFixed(1)} GB (${(g.vram_used_mb/1024).toFixed(1)} GB used, ${vramPct}%)</span></div>
          <div class="spec-row"><span class="label">Temperature</span><span class="value ${tempClass}">${g.temp_c}°C</span></div>
          <div class="spec-row"><span class="label">Utilization</span><span class="value">${g.util_pct}%</span></div>
          <div class="spec-row"><span class="label">Driver</span><span class="value">${g.driver}</span></div>
          ${g.cuda ? '<div class="spec-row"><span class="label">CUDA</span><span class="value">'+g.cuda+'</span></div>' : ''}
          <div class="spec-row"><span class="label">PCIe Bus</span><span class="value">${g.pci_bus}</span></div>
        </div>`;
      }
      if (s.cpu) {
        const c = s.cpu;
        const loadStr = c.load_1m !== undefined ? c.load_1m.toFixed(1)+' / '+c.load_5m.toFixed(1)+' / '+c.load_15m.toFixed(1) : '—';
        h += `<div class="spec-card"><h3>CPU</h3>
          <div class="spec-row"><span class="label">Model</span><span class="value">${c.name}</span></div>
          <div class="spec-row"><span class="label">Cores</span><span class="value">${c.cores}</span></div>
          <div class="spec-row"><span class="label">Load (1/5/15m)</span><span class="value">${loadStr}</span></div>
        </div>`;
      }
      if (s.memory) {
        const m = s.memory;
        const usedPct = m.total_mb > 0 ? ((m.used_mb / m.total_mb) * 100).toFixed(0) : 0;
        h += `<div class="spec-card"><h3>Memory</h3>
          <div class="spec-row"><span class="label">Total</span><span class="value">${(m.total_mb/1024).toFixed(1)} GB</span></div>
          <div class="spec-row"><span class="label">Used</span><span class="value">${(m.used_mb/1024).toFixed(1)} GB (${usedPct}%)</span></div>
          <div class="spec-row"><span class="label">Available</span><span class="value">${((m.available_mb||m.free_mb)/1024).toFixed(1)} GB</span></div>
        </div>`;
      }
      if (s.disk) {
        const dk = s.disk;
        h += `<div class="spec-card"><h3>Disk</h3>
          <div class="spec-row"><span class="label">Total</span><span class="value">${dk.total_gb} GB</span></div>
          <div class="spec-row"><span class="label">Used</span><span class="value">${dk.used_gb} GB (${dk.util_pct}%)</span></div>
          <div class="spec-row"><span class="label">Free</span><span class="value">${dk.free_gb} GB</span></div>
        </div>`;
      }
      h += '</div>';
    }

    h += `<table><thead><tr>
      <th>#</th><th>Title</th><th>Resolution</th><th>Input</th><th>Output</th><th>Duration</th><th>Status</th><th>Progress</th><th>Compare</th>
    </tr></thead><tbody>`;

    const order = [...active, ...other, ...done, ...queued];
    order.forEach((v, i) => {
      const dur = v.duration >= 3600
        ? `${Math.floor(v.duration/3600)}:${String(Math.floor((v.duration%3600)/60)).padStart(2,'0')}:${String(v.duration%60).padStart(2,'0')}`
        : `${Math.floor(v.duration/60)}:${String(v.duration%60).padStart(2,'0')}`;
      const barColor = v.status === 'done' ? '#6ee7b7' : '#38bdf8';
      const ytUrl = 'https://www.youtube.com/watch?v=' + v.id;
      const compareLink = v.done_frames > 0 ? `<a href="/compare/${v.title}">view</a>` : '';
      const inputName = v.title + '.mkv';
      const outputName = v.title + '_' + v.scale + 'x.mkv';
      const outputCell = v.status === 'done'
        ? `<a href="/download/${v.title}" style="color:#6ee7b7">${outputName}</a>`
        : outputName;
      h += `<tr>
        <td>${i+1}</td>
        <td class="title-col"><a href="${ytUrl}" target="_blank">${v.display_title || v.title.replace(/_/g, ' ')}</a></td>
        <td style="font-size:0.75rem;color:#94a3b8">${v.resolution || '—'}</td>
        <td style="font-size:0.75rem;color:#94a3b8">${inputName}</td>
        <td style="font-size:0.75rem">${outputCell}</td>
        <td>${dur}</td>
        <td><span class="status status-${v.status}">${v.status}</span></td>
        <td><div class="bar-bg"><div class="bar-fg" style="width:${v.progress}%;background:${barColor}"></div>
            <span class="bar-text">${v.status==='done' ? v.eta : v.progress > 0 ? v.done_frames+'/'+v.total_frames : ''}</span></div></td>
        <td>${compareLink}</td>
      </tr>`;
    });
    h += '</tbody></table>';

    if (d.log_tail) {
      h += '<div class="log">' + d.log_tail.replace(/</g,'&lt;') + '</div>';
    }
    h += '<p style="margin-top:12px;color:#64748b;font-size:0.75rem">Updated: ' + d.timestamp + '</p>';
    app.innerHTML = h;

    // Update page title, h1, and subtitle with video name, location, and cost
    const activeVideo = d.videos.find(v => v.status === 'upscaling') || d.videos[0];
    const videoName = activeVideo ? (activeVideo.display_title || activeVideo.title.replace(/_/g, ' ')) : '';
    const location = (d.instance && d.instance.location) ? d.instance.location : '';
    const titleParts = [];
    if (videoName) titleParts.push(videoName);
    if (location) titleParts.push(location);
    document.title = titleParts.join(' — ') || 'Video Enhancement';

    // Update h1 with video name
    document.getElementById('page-title').textContent = videoName || 'Video Enhancement';

    // Update subtitle with cost estimate (reuse costPerHr from above)
    let subtitleParts = ['Real-ESRGAN AI Upscaling'];
    if (location) subtitleParts.push(location);
    if (costPerHr > 0 && etaStr !== '—') {
      // Parse eta string (e.g. "5h 30m" or "45m")
      let etaHours = 0;
      const hMatch = etaStr.match(/(\d+)h/);
      const mMatch = etaStr.match(/(\d+)m/);
      if (hMatch) etaHours += parseInt(hMatch[1]);
      if (mMatch) etaHours += parseInt(mMatch[1]) / 60;
      if (etaHours > 0) {
        const estCost = (etaHours * costPerHr).toFixed(1);
        subtitleParts.push('$' + costPerHr.toFixed(2) + '/hr');
        subtitleParts.push('~$' + estCost + ' remaining');
      }
    }
    document.getElementById('page-subtitle').textContent = subtitleParts.join(' — ');
  } catch(e) {
    document.getElementById('app').innerHTML = '<p>Error loading status: ' + e + '</p>';
  }
}
update();
setInterval(update, 30000);
</script>
</body></html>"""

    def render_compare(self, title, frame_param=None):
        """Render side-by-side comparison of original vs enhanced frames."""
        # Look up display title from video_queue.json
        display_title = title.replace('_', ' ')
        if os.path.exists(QUEUE_FILE):
            try:
                with open(QUEUE_FILE) as f:
                    for entry in json.load(f):
                        if entry.get("title") == title or entry.get("id") == title:
                            display_title = entry.get("display_title", entry.get("title", title)).replace('_', ' ')
                            break
            except Exception:
                pass
        job_dir = os.path.join(JOBS_DIR, title)
        frames_in = sorted(glob.glob(os.path.join(job_dir, "frames_in", "frame_*.png")))
        frames_out = sorted(glob.glob(os.path.join(job_dir, "frames_out", "frame_*.png")))

        # Find frames that exist in both in and out
        out_names = {os.path.basename(f) for f in frames_out}
        available = [f for f in frames_in if os.path.basename(f) in out_names]

        if not available:
            return "<html><body><h1>No frames available for comparison yet.</h1><p><a href='/'>Back</a></p></body></html>"

        # Pick frame to show
        total = len(available)
        if frame_param and frame_param.isdigit():
            idx = max(0, min(int(frame_param), total - 1))
        else:
            # Default: show a frame from ~20% into the video (more interesting than frame 1)
            idx = min(total // 5, total - 1)

        frame_name = os.path.basename(available[idx])
        in_url = f"/frames/{title}/in/{frame_name}"
        out_url = f"/frames/{title}/out/{frame_name}"

        # Navigation
        prev_idx = max(0, idx - 1)
        next_idx = min(total - 1, idx + 1)
        jump_10 = min(total - 1, idx + 10)
        back_10 = max(0, idx - 10)

        return f"""<!DOCTYPE html>
<html><head>
<meta charset="utf-8">
<title>Compare: {display_title}</title>
<style>
  * {{ box-sizing: border-box; margin: 0; padding: 0; }}
  body {{ font-family: -apple-system, sans-serif; background: #0f172a; color: #e2e8f0; padding: 20px; }}
  h1 {{ font-size: 1.3rem; margin-bottom: 4px; }}
  .nav {{ margin: 12px 0; display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }}
  .nav a, .nav span {{ padding: 6px 12px; background: #1e293b; border-radius: 4px; color: #38bdf8;
                       text-decoration: none; font-size: 0.85rem; }}
  .nav a:hover {{ background: #334155; }}
  .nav .current {{ background: #334155; color: #f8fafc; }}
  .compare {{ display: flex; gap: 12px; margin-top: 12px; }}
  .compare .panel {{ flex: 1; min-width: 0; }}
  .compare .panel h2 {{ font-size: 0.9rem; color: #94a3b8; margin-bottom: 6px; }}
  .compare img {{ width: 100%; height: auto; border-radius: 4px; border: 1px solid #334155; cursor: pointer; }}
  .compare img:hover {{ border-color: #38bdf8; }}
  a.back {{ color: #38bdf8; text-decoration: none; font-size: 0.85rem; }}
</style>
</head><body>
<a class="back" href="/">&larr; Back to dashboard</a>
<h1>{display_title}</h1>
<div class="nav">
  <a href="/compare/{title}?frame=0">First</a>
  <a href="/compare/{title}?frame={back_10}">&laquo; -10</a>
  <a href="/compare/{title}?frame={prev_idx}">&lsaquo; Prev</a>
  <span class="current">Frame {idx + 1} / {total}</span>
  <a href="/compare/{title}?frame={next_idx}">Next &rsaquo;</a>
  <a href="/compare/{title}?frame={jump_10}">+10 &raquo;</a>
  <a href="/compare/{title}?frame={total - 1}">Last</a>
</div>
<div class="compare">
  <div class="panel">
    <h2>Original ({frame_name})</h2>
    <a href="{in_url}" target="_blank"><img src="{in_url}" alt="Original"></a>
  </div>
  <div class="panel">
    <h2>Enhanced ({frame_name})</h2>
    <a href="{out_url}" target="_blank"><img src="{out_url}" alt="Enhanced"></a>
  </div>
</div>
</body></html>"""


if __name__ == "__main__":
    server = http.server.HTTPServer(("0.0.0.0", PORT), StatusHandler)
    print(f"Status server running on port {PORT}")
    server.serve_forever()
