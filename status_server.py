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
PORT = 8080

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

        # Aggregate frame stats
        total_frames_all = sum(v["total_frames"] for v in videos)
        done_frames_all = sum(v["done_frames"] for v in videos)
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
                    eta_str = f"{eta_h}h {eta_m}m" if eta_h > 0 else f"{eta_m}m"

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
</style>
</head><body>
<h1>Da Vaz Video Enhancement</h1>
<p class="subtitle">Real-ESRGAN AI Upscaling &mdash; auto-refreshes every 30s</p>
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

    let h = `<div class="summary">
      <div class="card"><div class="num">${d.total}</div><div class="label">Total Videos</div></div>
      <div class="card"><div class="num" style="color:#6ee7b7">${done.length}</div><div class="label">Completed</div></div>
      <div class="card"><div class="num" style="color:#38bdf8">${active.length}</div><div class="label">Upscaling Now</div></div>
      <div class="card"><div class="num" style="color:#94a3b8">${queued.length}</div><div class="label">Queued</div></div>
      <div class="card"><div class="num" style="font-size:1.2rem">${framesStr}</div><div class="label">Frames (${d.overall_pct}%)</div></div>
      <div class="card"><div class="num">${fpsStr}</div><div class="label">GPU Speed</div></div>
      <div class="card"><div class="num">${etaStr}</div><div class="label">ETA</div></div>
      <div class="card"><div class="num" style="font-size:1rem">${dlStr}</div><div class="label">Download</div></div>
    </div>`;

    h += `<table><thead><tr>
      <th>#</th><th>Title</th><th>Input</th><th>Output</th><th>Duration</th><th>Status</th><th>Progress</th><th>Compare</th>
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
