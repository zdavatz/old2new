#!/usr/bin/env bash
#
# runpod_launch.sh — Launch RunPod GPU pod for video enhancement
#
# Usage:
#   ./runpod_launch.sh launch <VIDEO_ID> [DISK_GB]  — Launch pod for a specific video
#   ./runpod_launch.sh test [VIDEO_ID]               — Launch pod with default test video
#   ./runpod_launch.sh status                        — Show status of all pods
#   ./runpod_launch.sh ssh [POD_NUM]                 — SSH into a pod
#   ./runpod_launch.sh download [OUTPUT_DIR]         — Download completed videos
#   ./runpod_launch.sh destroy                       — Destroy all pods (keeps network volume)
#   ./runpod_launch.sh destroy-all                   — Destroy pods AND network volume
#   ./runpod_launch.sh list                          — List all videos from not_enhanced.json
#
# Flow: Creates a network volume first (pre-allocated, fast pod start),
# then launches pod with that volume attached. Credentials are embedded
# in the startup script (no SCP needed).
#
# Requirements:
#   - RunPod API key: export RUNPOD_API_KEY in ~/.bashrc
#   - SSH key added to RunPod account settings (https://console.runpod.io/user/settings)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
STATE_DIR="$SCRIPT_DIR/.runpod_batch"
OUTPUT_DIR="${OUTPUT_DIR:-$SCRIPT_DIR/enhanced_videos}"

# RunPod API
RP_API="https://rest.runpod.io/v1"
RP_KEY="${RUNPOD_API_KEY:-}"

# GPU config
GPU_TYPE="${GPU_TYPE:-NVIDIA RTX PRO 6000 Blackwell Server Edition}"
GPU_DISPLAY="RTX Pro 6000 (96GB)"

# Docker image — lightweight Ubuntu with NVIDIA drivers (no PyTorch needed for ncnn-vulkan)
DOCKER_IMAGE="nvidia/cuda:12.8.1-base-ubuntu22.04"

# Preferred datacenter (EU-RO-1 = Romania, closest to CH)
# Override with: DATACENTER=US-KS-2 ./runpod_launch.sh ...
DATACENTER="${DATACENTER:-EU-RO-1}"

# SSH user (RunPod pods run as root)
SSH_USER="root"

# ---------- Helper functions ----------

log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*"
}

ensure_state_dir() {
    mkdir -p "$STATE_DIR"
}

rp_api() {
    local method="$1"
    local endpoint="$2"
    shift 2
    curl -s -X "$method" "${RP_API}${endpoint}" \
        -H "Authorization: Bearer $RP_KEY" \
        -H "Content-Type: application/json" \
        "$@"
}

check_api_key() {
    if [[ -z "$RP_KEY" ]]; then
        echo "ERROR: RUNPOD_API_KEY not set."
        echo "Add to ~/.bashrc: export RUNPOD_API_KEY=\"your-key\""
        exit 1
    fi
}

# Look up video in not_enhanced.json by youtube_id
lookup_video() {
    local video_id="$1"
    python3 -c "
import json, sys
with open('$SCRIPT_DIR/not_enhanced.json') as f:
    data = json.load(f)
for v in data['videos']:
    if v['youtube_id'] == '$video_id':
        title = v['title'].replace(' ', '_').replace('/', '_')
        print(f\"{v['youtube_id']}\t{title}\t{v['duration_seconds']}\t{v['width']}\t{v['height']}\t{v['scale']}\")
        sys.exit(0)
print('NOT_FOUND', file=sys.stderr)
sys.exit(1)
"
}

# Estimate disk GB needed for a video
estimate_disk_gb() {
    local width="$1" height="$2" duration="$3" scale="$4"
    python3 -c "
import math
w, h, dur, sc = $width, $height, $duration, $scale
fps = 25
total_frames = dur * fps
input_bytes = w * h * 3
output_bytes = (w * sc) * (h * sc) * 3
input_gb = total_frames * input_bytes / 2.5 / (1024**3)
output_gb = total_frames * output_bytes / 2.5 / (1024**3)
video_gb = (input_gb + output_gb) * 1.2 + 50
video_gb = math.ceil(video_gb / 50) * 50
print(int(video_gb))
"
}

# Create or reuse a network volume
# Network volumes are pre-allocated and persist across pods → fast pod start
ensure_network_volume() {
    local name="$1"
    local size_gb="$2"
    local datacenter="$3"

    # Check for existing volume in state dir
    if [[ -f "$STATE_DIR/volume.id" ]]; then
        local existing_id
        existing_id=$(cat "$STATE_DIR/volume.id")
        # Verify it still exists
        local vol_info
        vol_info=$(rp_api GET "/networkvolumes/$existing_id" 2>/dev/null || echo '{"error":"not found"}')
        if echo "$vol_info" | python3 -c "import json,sys; d=json.load(sys.stdin); sys.exit(0 if 'error' not in d else 1)" 2>/dev/null; then
            local existing_size
            existing_size=$(echo "$vol_info" | python3 -c "import json,sys; print(json.load(sys.stdin).get('size',0))" 2>/dev/null)
            if [[ "$existing_size" -ge "$size_gb" ]]; then
                log "Reusing existing network volume: $existing_id (${existing_size}GB)" >&2
                echo "$existing_id"
                return
            else
                log "Existing volume too small (${existing_size}GB < ${size_gb}GB) — creating new one" >&2
            fi
        else
            log "Previous volume $existing_id no longer exists — creating new one" >&2
            rm -f "$STATE_DIR/volume.id"
        fi
    fi

    log "Creating network volume: ${name} (${size_gb}GB in ${datacenter})..." >&2
    local result
    result=$(rp_api POST /networkvolumes -d "$(python3 -c "
import json
print(json.dumps({'name': '$name', 'size': $size_gb, 'dataCenterId': '$datacenter'}))
")")

    local vol_id
    vol_id=$(echo "$result" | python3 -c "
import json, sys
d = json.load(sys.stdin)
print(d.get('id', ''))
" 2>/dev/null || echo "")

    if [[ -z "$vol_id" ]]; then
        log "ERROR: Failed to create network volume" >&2
        log "Response: $result" >&2
        exit 1
    fi

    echo "$vol_id" > "$STATE_DIR/volume.id"
    echo "$datacenter" > "$STATE_DIR/volume.dc"
    log "Network volume created: $vol_id" >&2
    echo "$vol_id"
}

# Wait for pod to be ready with SSH access
# Returns: "ip port" or empty string
wait_for_pod() {
    local pod_id="$1"
    local max_wait="${2:-300}"  # 5 min default

    local waited=0
    while [[ $waited -lt $max_wait ]]; do
        sleep 10
        waited=$((waited + 10))
        local status_json
        status_json=$(rp_api GET "/pods/$pod_id")
        local pod_info
        pod_info=$(echo "$status_json" | python3 -c "
import json, sys
d = json.load(sys.stdin)
status = d.get('desiredStatus', '')
runtime = d.get('runtime', {}) or {}
ports = runtime.get('ports', []) or []
uptime = runtime.get('uptimeInSeconds', 0) or 0
ip = port = ''
for p in ports:
    if p.get('privatePort') == 22:
        ip = p.get('ip', '')
        port = str(p.get('publicPort', ''))
print(f'{status}\t{ip}\t{port}\t{uptime}')
" 2>/dev/null || echo "UNKNOWN\t\t\t0")

        local pstatus pip pport puptime
        IFS=$'\t' read -r pstatus pip pport puptime <<< "$pod_info"

        if [[ -n "$pip" && -n "$pport" && "$pip" != "None" && "$pip" != "" ]]; then
            log "Pod ready: ${pip}:${pport} (uptime: ${puptime}s)" >&2
            echo "$pip $pport"
            return
        fi

        # Show progress on stderr (not captured by subshell)
        local mins=$((waited / 60))
        local secs=$((waited % 60))
        printf "\r  Waiting... %dm%02ds (status: %s)   " "$mins" "$secs" "$pstatus" >&2
    done
    printf "\n" >&2
}

# Generate the startup script that runs inside the pod
generate_startup_script() {
    local vid="$1" title="$2" duration="$3" scale="$4"
    local display_title="${title//_/ }"
    local instance_label="${5:-davaz-enhance}"
    local cost_per_hr="${6:-1.69}"

    # Base64-encode OAuth credentials for embedding
    local client_secret_b64="" youtube_token_b64=""
    if [[ -f "$SCRIPT_DIR/client_secret.json" ]]; then
        client_secret_b64=$(base64 -w0 "$SCRIPT_DIR/client_secret.json")
    fi
    if [[ -f "$SCRIPT_DIR/youtube_token.json" ]]; then
        youtube_token_b64=$(base64 -w0 "$SCRIPT_DIR/youtube_token.json")
    fi

    cat << 'SETUP_HEADER'
#!/bin/bash
set -e

echo "=== Setup started at $(date) ==="

# Install system packages (lean — no PyTorch, no pip dependency hell)
apt-get update -qq && apt-get install -y --no-install-recommends \
    ffmpeg nginx python3 python3-pip curl unzip \
    libvulkan1 vulkan-tools 2>&1

# === Vulkan check ===
echo ""
echo "[Vulkan]"
if vulkaninfo --summary 2>/dev/null | grep -q "GPU"; then
    vulkaninfo --summary 2>&1 | grep -E "GPU|driver|apiVersion" || true
    echo "Vulkan: OK"
    VULKAN_OK=1
else
    echo "WARNING: Vulkan not available!"
    echo "Trying to fix with NVIDIA ICD..."
    # Create ICD manifest if missing
    mkdir -p /etc/vulkan/icd.d
    cat > /etc/vulkan/icd.d/nvidia_icd.json << 'ICDEOF'
{
    "file_format_version": "1.0.0",
    "ICD": {
        "library_path": "libGLX_nvidia.so.0",
        "api_version": "1.3.0"
    }
}
ICDEOF
    # Also try the lib path directly
    ldconfig 2>/dev/null || true
    if vulkaninfo --summary 2>/dev/null | grep -q "GPU"; then
        vulkaninfo --summary 2>&1 | grep -E "GPU|driver|apiVersion" || true
        echo "Vulkan: OK (after ICD fix)"
        VULKAN_OK=1
    else
        echo "FATAL: Vulkan still not available. Cannot run ncnn-vulkan."
        echo "GPU info:"
        nvidia-smi 2>&1 | head -5 || true
        echo "Vulkan debug:"
        VK_LOADER_DEBUG=all vulkaninfo --summary 2>&1 | tail -20 || true
        exit 1
    fi
fi

# Configure nginx as reverse proxy to status server
cat > /etc/nginx/sites-available/default << 'NGINXCONF'
server {
    listen 8080 default_server;
    server_name _;
    location / {
        proxy_pass http://127.0.0.1:8081;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_read_timeout 300;
        proxy_connect_timeout 5;
    }
}
NGINXCONF
systemctl enable nginx
systemctl restart nginx
echo "nginx configured on port 8080 -> status server on 8081"

# Use /workspace (network volume) for jobs
mkdir -p /workspace/jobs
ln -sf /workspace/jobs /root/jobs
echo "Jobs directory: /workspace/jobs (symlinked from /root/jobs)"

# Download Real-ESRGAN ncnn-vulkan binary (44MB — vs 12GB PyTorch image)
echo "Downloading Real-ESRGAN ncnn-vulkan..."
curl -sL "https://github.com/xinntao/Real-ESRGAN/releases/download/v0.2.5.0/realesrgan-ncnn-vulkan-20220424-ubuntu.zip" -o /tmp/realesrgan.zip
cd /tmp && unzip -o realesrgan.zip -d /opt/realesrgan
chmod +x /opt/realesrgan/realesrgan-ncnn-vulkan
ln -sf /opt/realesrgan/realesrgan-ncnn-vulkan /usr/local/bin/realesrgan-ncnn-vulkan
echo "Real-ESRGAN ncnn-vulkan installed."

# Verify it can see the GPU
echo "[Real-ESRGAN GPU test]"
/opt/realesrgan/realesrgan-ncnn-vulkan -i /dev/null -o /dev/null 2>&1 | head -5 || true

# Install minimal Python deps (yt-dlp for download, Google API for upload)
pip install --break-system-packages -q yt-dlp google-api-python-client google-auth-oauthlib google-auth-httplib2 2>&1 || true

# Install static ffmpeg 7.x if system version is old
if ! ffmpeg -version 2>/dev/null | grep -q "7\."; then
    echo "Installing static ffmpeg 7.x..."
    curl -sL https://johnvansickle.com/ffmpeg/releases/ffmpeg-release-amd64-static.tar.xz | tar xJ -C /tmp/
    cp /tmp/ffmpeg-*-amd64-static/ffmpeg /usr/local/bin/ffmpeg
    cp /tmp/ffmpeg-*-amd64-static/ffprobe /usr/local/bin/ffprobe
fi

# Download youtube_upload.py
curl -sL "https://raw.githubusercontent.com/zdavatz/old2new/main/youtube_upload.py" -o /root/youtube_upload.py
echo "Dependencies installed."

SETUP_HEADER

    # Deploy OAuth credentials (embedded as base64 — no SCP timing issues)
    if [[ -n "$client_secret_b64" && -n "$youtube_token_b64" ]]; then
        cat << CREDSEOF
# Deploy YouTube OAuth credentials (embedded)
echo "$client_secret_b64" | base64 -d > /root/client_secret.json
echo "$youtube_token_b64" | base64 -d > /root/youtube_token.json
chmod 600 /root/client_secret.json /root/youtube_token.json
echo "YouTube OAuth credentials deployed."

CREDSEOF
    else
        echo "# No YouTube OAuth credentials available"
        echo ""
    fi

    # Write video queue JSON for status server
    cat << QUEUEEOF
cat > /root/video_queue.json << 'QUEUEJSON'
[
  {"id": "$vid", "scale": $scale, "title": "$title", "display_title": "$display_title", "duration": $duration}
]
QUEUEJSON

QUEUEEOF

    # Write instance metadata for dashboard
    cat << METAEOF
cat > /root/instance_meta.json << 'METAJSON'
{
  "label": "$instance_label",
  "location": "RunPod $DATACENTER",
  "cost_per_hr": $cost_per_hr,
  "provider": "runpod",
  "instance_id": "pending"
}
METAJSON

METAEOF

    # Download and start status server
    cat << 'STATUSEOF'
curl -sL "https://raw.githubusercontent.com/zdavatz/old2new/main/status_server.py?$(date +%s)" -o /root/status_server.py
sed -i "s/PORT = 8080/PORT = 8081/" /root/status_server.py
python3 /root/status_server.py &
echo "Status server started on port 8081 (nginx proxy on 8080)"

STATUSEOF

    # Video processing — ncnn-vulkan pipeline (no Python/PyTorch)
    cat << VIDEOEOF
echo ""
echo "=========================================="
echo "Processing: $display_title ($vid, scale=${scale}x)"
echo "Started at: \$(date)"
echo "=========================================="

JOB_DIR="/root/jobs/$title"
mkdir -p "\$JOB_DIR/frames_in" "\$JOB_DIR/frames_out"
URL="https://www.youtube.com/watch?v=$vid"

# Step 1: Download video
echo "[1/4] Downloading video..."
if [[ ! -f "\$JOB_DIR/$title.mkv" ]]; then
    yt-dlp -f "bestvideo[ext=mp4]+bestaudio[ext=m4a]/bestvideo+bestaudio/best" \\
        --merge-output-format mkv \\
        -o "\$JOB_DIR/$title.mkv" "\$URL"
fi
echo "Download complete: \$(du -h "\$JOB_DIR/$title.mkv" | cut -f1)"

# Get video info
FPS=\$(ffprobe -v error -select_streams v:0 -show_entries stream=r_frame_rate -of csv=p=0 "\$JOB_DIR/$title.mkv" | head -1)
FPS_NUM=\$(echo "\$FPS" | cut -d/ -f1)
FPS_DEN=\$(echo "\$FPS" | cut -d/ -f2)
FPS_FLOAT=\$(python3 -c "print(f'{\${FPS_NUM}/\${FPS_DEN:-1}:.3f}')")
echo "Video FPS: \$FPS_FLOAT (\$FPS)"

# Step 2: Extract frames (parallel ffmpeg)
echo "[2/4] Extracting frames..."
FRAME_COUNT=\$(ls "\$JOB_DIR/frames_in/" 2>/dev/null | wc -l)
if [[ \$FRAME_COUNT -eq 0 ]]; then
    ffmpeg -i "\$JOB_DIR/$title.mkv" -qscale:v 1 -qmin 1 -qmax 1 -vsync 0 \\
        "\$JOB_DIR/frames_in/frame_%08d.png" 2>&1 | tail -3
    FRAME_COUNT=\$(ls "\$JOB_DIR/frames_in/" | wc -l)
fi
echo "Extracted \$FRAME_COUNT frames"

# Step 3: Upscale with ncnn-vulkan
echo "[3/4] Upscaling with Real-ESRGAN ncnn-vulkan (${scale}x)..."
UPSCALED_COUNT=\$(ls "\$JOB_DIR/frames_out/" 2>/dev/null | wc -l)
if [[ \$UPSCALED_COUNT -lt \$FRAME_COUNT ]]; then
    START_TIME=\$(date +%s)
    realesrgan-ncnn-vulkan \\
        -i "\$JOB_DIR/frames_in" \\
        -o "\$JOB_DIR/frames_out" \\
        -s $scale \\
        -n realesrgan-x4plus \\
        -m /opt/realesrgan/models \\
        -f png \\
        -g 0 \\
        -j 4:4:4
    END_TIME=\$(date +%s)
    ELAPSED=\$(( END_TIME - START_TIME ))
    UPSCALED_COUNT=\$(ls "\$JOB_DIR/frames_out/" | wc -l)
    if [[ \$ELAPSED -gt 0 ]]; then
        FPS_ACTUAL=\$(python3 -c "print(f'{\$UPSCALED_COUNT/\$ELAPSED:.2f}')")
        echo "Upscaled \$UPSCALED_COUNT frames in \${ELAPSED}s (\${FPS_ACTUAL} fps)"
    fi
else
    echo "Frames already upscaled (\$UPSCALED_COUNT frames)"
fi

# Step 4: Reassemble video
ENHANCED_FILE="\$JOB_DIR/${title}_${scale}x.mkv"
echo "[4/4] Reassembling video..."
if [[ ! -f "\$ENHANCED_FILE" ]]; then
    ffmpeg -framerate "\$FPS" -i "\$JOB_DIR/frames_out/frame_%08d.png" \\
        -i "\$JOB_DIR/$title.mkv" \\
        -map 0:v -map 1:a? \\
        -c:v libx264 -crf 18 -preset medium -pix_fmt yuv420p \\
        -c:a copy \\
        "\$ENHANCED_FILE" 2>&1 | tail -3
    echo "Enhanced video: \$(du -h "\$ENHANCED_FILE" | cut -f1)"
fi

# Upload to YouTube
if [[ -f "\$ENHANCED_FILE" && -f "/root/client_secret.json" && -f "/root/youtube_token.json" ]]; then
    echo "Uploading to YouTube..."
    if python3 /root/youtube_upload.py "$vid" "\$ENHANCED_FILE" \\
        --client-secret /root/client_secret.json \\
        --token /root/youtube_token.json; then
        echo "YouTube upload + email notification done at \$(date)"
        touch "\$JOB_DIR/.uploaded"
        rm -rf "\$JOB_DIR"
        echo "Cleaned up job dir (uploaded to YouTube)"
    else
        echo "WARNING: YouTube upload failed — keeping ALL files"
    fi
else
    echo "Skipping YouTube upload (missing credentials or file) — keeping files"
fi

echo "$vid $title" >> /root/completed.txt

echo ""
echo "=========================================="
echo "DONE at \$(date)"
echo "=========================================="
VIDEOEOF
}

# ---------- Commands ----------

cmd_launch() {
    local video_id="$1"
    local override_disk="${2:-}"

    ensure_state_dir
    check_api_key

    # Lookup video
    local vid_info
    vid_info=$(lookup_video "$video_id") || {
        echo "ERROR: Video ID '$video_id' not found in not_enhanced.json"
        echo "Run './runpod_launch.sh list' to see available videos."
        exit 1
    }

    local vid title duration width height scale
    IFS=$'\t' read -r vid title duration width height scale <<< "$vid_info"

    local h=$((duration / 3600))
    local m=$(( (duration % 3600) / 60 ))
    local dur_str
    if [[ $h -gt 0 ]]; then dur_str="${h}h ${m}m"; else dur_str="${m}m"; fi
    local display_title="${title//_/ }"
    local mpixels
    mpixels=$(python3 -c "print(f'{$width * $height / 1000000:.2f}')")

    log "=== RunPod Launch ==="
    log "Video:      $display_title"
    log "ID:         $vid"
    log "Resolution: ${width}x${height} (${mpixels} MP)"
    log "Duration:   $dur_str"
    log "Scale:      ${scale}x"
    log "GPU:        $GPU_DISPLAY (96GB VRAM — no tiling needed)"
    log "Datacenter: $DATACENTER"
    log ""

    # Estimate disk
    local disk_gb
    if [[ -n "$override_disk" ]]; then
        disk_gb="$override_disk"
        log "Disk:       ${disk_gb}GB (manual override)"
    else
        disk_gb=$(estimate_disk_gb "$width" "$height" "$duration" "$scale")
        log "Disk:       ${disk_gb}GB (estimated)"
    fi
    log ""

    # Step 1: Create or reuse network volume (pre-allocated = fast pod start)
    local volume_id
    volume_id=$(ensure_network_volume "davaz-enhance" "$disk_gb" "$DATACENTER")
    log ""

    # Step 2: Generate startup script with embedded credentials
    local setup_file="$STATE_DIR/setup.sh"
    generate_startup_script "$vid" "$title" "$duration" "$scale" "davaz-$title" "1.69" > "$setup_file"

    local startup_b64
    startup_b64=$(base64 -w0 "$setup_file")

    # Step 3: Create pod with network volume attached
    log "Creating RunPod pod..."
    local result
    result=$(rp_api POST /pods -d "$(python3 -c "
import json
data = {
    'name': 'davaz-${title}',
    'gpuTypeIds': ['$GPU_TYPE'],
    'gpuCount': 1,
    'imageName': '$DOCKER_IMAGE',
    'containerDiskInGb': 50,
    'volumeInGb': 0,
    'networkVolumeId': '$volume_id',
    'volumeMountPath': '/workspace',
    'ports': ['22/tcp', '8080/http'],
    'cloudType': 'SECURE',
    'dockerStartCmd': ['bash', '-c', 'echo $startup_b64 | base64 -d > /root/setup.sh && chmod +x /root/setup.sh && bash /root/setup.sh > /root/enhance.log 2>&1 & sleep infinity'],
}
print(json.dumps(data))
")")

    local pod_id
    pod_id=$(echo "$result" | python3 -c "
import json, sys
data = json.load(sys.stdin)
pid = data.get('id', '')
if not pid:
    # Check for error
    err = data.get('error', '')
    if err:
        print(f'ERROR:{err}', file=sys.stderr)
print(pid)
" 2>/dev/null || echo "")

    if [[ -z "$pod_id" ]]; then
        log "ERROR: Failed to create pod"
        log "Response: $result"
        exit 1
    fi

    echo "$pod_id" > "$STATE_DIR/pod_0.id"
    echo "$vid" > "$STATE_DIR/pod_0.vid"
    echo "$title" > "$STATE_DIR/pod_0.title"

    log "Pod created: ID=$pod_id"
    log ""

    # Step 4: Wait for pod to be ready
    log "Waiting for pod to start (network volume = faster)..."
    local ssh_info
    ssh_info=$(wait_for_pod "$pod_id" 300)

    if [[ -n "$ssh_info" ]]; then
        local pod_ip pod_port
        read -r pod_ip pod_port <<< "$ssh_info"
        log ""
        log "Pod is running!"
        log "  SSH:       ssh -p $pod_port root@$pod_ip"
        log "  SSH (alt): ssh $pod_id@ssh.runpod.io"
        log "  Logs:      ssh -p $pod_port root@$pod_ip tail -f /root/enhance.log"
    else
        log "Pod still starting — check status later"
        log "  SSH (alt): ssh $pod_id@ssh.runpod.io"
    fi

    echo ""
    log "Next steps:"
    log "  ./runpod_launch.sh status       — check progress + dashboard URL"
    log "  ./runpod_launch.sh ssh           — SSH into pod"
    log "  ./runpod_launch.sh download      — download enhanced video"
    log "  ./runpod_launch.sh destroy       — destroy pod (keeps volume for reuse)"
    log "  ./runpod_launch.sh destroy-all   — destroy pod AND volume"
}

cmd_test() {
    local video_id="${1:-tljAVZCj6lw}"  # Default: BLUEPRINTS of LIFE
    cmd_launch "$video_id"
}

cmd_status() {
    ensure_state_dir
    check_api_key

    echo "=== Da Vaz Video Enhancement — RunPod Status ==="
    echo ""

    # Show network volume info
    if [[ -f "$STATE_DIR/volume.id" ]]; then
        local vol_id
        vol_id=$(cat "$STATE_DIR/volume.id")
        local vol_info
        vol_info=$(rp_api GET "/networkvolumes/$vol_id" 2>/dev/null || echo '{}')
        echo "$vol_info" | python3 -c "
import json, sys
d = json.load(sys.stdin)
if d and 'error' not in d:
    size = d.get('size', '?')
    name = d.get('name', '?')
    dc = d.get('dataCenterId', '?')
    print(f'  Network Volume: {name} ({size}GB in {dc})')
    print(f'    ID: ${vol_id}')
    print()
" 2>/dev/null
    fi

    local pods
    pods=$(rp_api GET /pods)

    echo "$pods" | python3 -c "
import json, sys
data = json.load(sys.stdin)
pods = data if isinstance(data, list) else data.get('pods', data.get('data', []))
if not pods:
    print('No pods running.')
    sys.exit(0)
for pod in pods:
    pid = pod.get('id', '?')
    name = pod.get('name', '?')
    status = pod.get('desiredStatus', '?')
    gpu = pod.get('gpuTypeId', '?')
    runtime = pod.get('runtime', {}) or {}
    ports = runtime.get('ports', []) or []
    uptime = runtime.get('uptimeInSeconds', 0) or 0
    cost = pod.get('costPerHr', '?')
    vcpu = pod.get('vcpuCount', '?')
    mem = pod.get('memoryInGb', '?')

    ssh_ip = ssh_port = web_url = ''
    for p in ports:
        if p.get('privatePort') == 22:
            ssh_ip = p.get('ip', '')
            ssh_port = str(p.get('publicPort', ''))
        if p.get('privatePort') == 8080:
            web_ip = p.get('ip', '')
            web_port = str(p.get('publicPort', ''))
            web_url = f'http://{web_ip}:{web_port}'

    hours = uptime // 3600
    mins = (uptime % 3600) // 60
    total_cost = cost * uptime / 3600 if isinstance(cost, (int, float)) and uptime else 0
    print(f'  Pod: {name} [{status}]')
    print(f'    ID:        {pid}')
    print(f'    GPU:       {gpu}')
    print(f'    CPU/RAM:   {vcpu} vCPUs / {mem}GB')
    print(f'    Cost:      \${cost}/hr (total: \${total_cost:.2f})')
    print(f'    Uptime:    {hours}h {mins}m')
    if ssh_ip and ssh_port:
        print(f'    SSH:       ssh -p {ssh_port} root@{ssh_ip}')
    if web_url:
        print(f'    Dashboard: {web_url}')
    print(f'    SSH (alt): ssh {pid}@ssh.runpod.io')
    print(f'    Logs:      ssh {pid}@ssh.runpod.io tail -f /root/enhance.log')
    print()
" 2>/dev/null

    # Show tracked pods
    if ls "$STATE_DIR"/pod_*.id &>/dev/null; then
        echo "--- Tracked Pods ---"
        for f in "$STATE_DIR"/pod_*.id; do
            local label
            label=$(basename "$f" .id)
            local title_file="${f%.id}.title"
            local title_str=""
            [[ -f "$title_file" ]] && title_str=" ($(cat "$title_file" | tr '_' ' '))"
            echo "  $label: $(cat "$f")$title_str"
        done
        echo ""
    fi
}

cmd_ssh() {
    local pod_num="${1:-0}"
    check_api_key

    local id_file="$STATE_DIR/pod_${pod_num}.id"
    if [[ ! -f "$id_file" ]]; then
        echo "ERROR: No pod $pod_num found. Run 'launch' first."
        exit 1
    fi

    local pod_id
    pod_id=$(cat "$id_file")

    local pod_info
    pod_info=$(rp_api GET "/pods/$pod_id")

    local ssh_info
    ssh_info=$(echo "$pod_info" | python3 -c "
import json, sys
d = json.load(sys.stdin)
runtime = d.get('runtime', {}) or {}
ports = runtime.get('ports', []) or []
for p in ports:
    if p.get('privatePort') == 22:
        print(f\"{p.get('ip', '')} {p.get('publicPort', '')}\")
        sys.exit(0)
print('PROXY')
" 2>/dev/null)

    local ip port
    read -r ip port <<< "$ssh_info"

    if [[ "$ip" == "PROXY" || -z "$ip" || -z "$port" ]]; then
        log "Using proxied SSH..."
        exec ssh "${pod_id}@ssh.runpod.io"
    else
        log "Connecting to pod $pod_num at $ip:$port..."
        exec ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -p "$port" "${SSH_USER}@${ip}"
    fi
}

cmd_download() {
    local out_dir="${1:-$OUTPUT_DIR}"
    mkdir -p "$out_dir"
    check_api_key

    for id_file in "$STATE_DIR"/pod_*.id; do
        [[ ! -f "$id_file" ]] && continue
        local pod_id
        pod_id=$(cat "$id_file")
        local title_file="${id_file%.id}.title"
        local title_str="unknown"
        [[ -f "$title_file" ]] && title_str=$(cat "$title_file")

        local pod_info
        pod_info=$(rp_api GET "/pods/$pod_id")

        local ssh_info
        ssh_info=$(echo "$pod_info" | python3 -c "
import json, sys
d = json.load(sys.stdin)
runtime = d.get('runtime', {}) or {}
ports = runtime.get('ports', []) or []
for p in ports:
    if p.get('privatePort') == 22:
        print(f\"{p.get('ip', '')} {p.get('publicPort', '')}\")
        sys.exit(0)
print('')
" 2>/dev/null)

        local ip port
        read -r ip port <<< "$ssh_info"

        if [[ -z "$ip" || -z "$port" || "$ip" == "None" ]]; then
            log "Pod $pod_id not reachable — skipping"
            continue
        fi

        log "Downloading from pod $pod_id ($title_str)..."

        local completed
        completed=$(ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
            -p "$port" "${SSH_USER}@${ip}" \
            'find ~/jobs -name "*_*x.mkv" -type f 2>/dev/null' || true)

        if [[ -z "$completed" ]]; then
            log "  No completed videos found"
            continue
        fi

        echo "$completed" | while read -r remote_path; do
            local filename
            filename=$(basename "$remote_path")
            if [[ -f "$out_dir/$filename" ]]; then
                log "  Already downloaded: $filename"
                continue
            fi
            log "  Downloading: $filename"
            scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
                -P "$port" "${SSH_USER}@${ip}:${remote_path}" "$out_dir/" || \
                log "  FAILED: $filename"
        done
    done

    log "Downloads saved to: $out_dir"
}

cmd_destroy() {
    check_api_key

    local destroyed=0
    for id_file in "$STATE_DIR"/pod_*.id; do
        [[ ! -f "$id_file" ]] && continue
        local pod_id
        pod_id=$(cat "$id_file")
        log "Destroying pod $pod_id..."
        rp_api DELETE "/pods/$pod_id" >/dev/null 2>&1 || log "  Failed to destroy $pod_id"
        rm -f "$id_file"
        destroyed=$((destroyed + 1))
    done

    if [[ $destroyed -eq 0 ]]; then
        echo "No tracked pods to destroy."
    else
        log "$destroyed pod(s) destroyed."
        log "Network volume kept for reuse. Use 'destroy-all' to also delete volume."
    fi

    rm -f "$STATE_DIR"/pod_*.id "$STATE_DIR"/pod_*.vid "$STATE_DIR"/pod_*.title
}

cmd_destroy_all() {
    cmd_destroy

    # Also destroy network volume
    if [[ -f "$STATE_DIR/volume.id" ]]; then
        local vol_id
        vol_id=$(cat "$STATE_DIR/volume.id")
        log "Destroying network volume $vol_id..."
        rp_api DELETE "/networkvolumes/$vol_id" >/dev/null 2>&1 || log "  Failed to destroy volume"
        rm -f "$STATE_DIR/volume.id" "$STATE_DIR/volume.dc"
        log "Network volume destroyed."
    else
        log "No network volume to destroy."
    fi
}

cmd_list() {
    if [[ ! -f "$SCRIPT_DIR/not_enhanced.json" ]]; then
        echo "ERROR: not_enhanced.json not found. Run fetch_missing_videos.py first."
        exit 1
    fi

    python3 -c "
import json
with open('$SCRIPT_DIR/not_enhanced.json') as f:
    data = json.load(f)

print(f'=== Da Vaz Videos Not Enhanced ({data[\"total\"]} videos) ===')
print(f'    RTX 4090: {data[\"summary\"][\"rtx_4090\"]}  |  RTX 5090: {data[\"summary\"][\"rtx_5090\"]}')
print()
print(f'{\"VIDEO_ID\":<14}  {\"LENGTH\":>7}  {\"RES\":>10}  {\"MP\":>5}  {\"SCALE\":>5}  {\"GPU\":>10}  TITLE')
print(f'{\"----------\":<14}  {\"------\":>7}  {\"---\":>10}  {\"--\":>5}  {\"-----\":>5}  {\"---\":>10}  -----')

for v in data['videos']:
    dur = v['duration_seconds']
    h = dur // 3600
    m = (dur % 3600) // 60
    s = dur % 60
    dur_str = f'{h}:{m:02d}:{s:02d}' if h > 0 else f'{m}:{s:02d}'
    res = f\"{v['width']}x{v['height']}\"
    mp = f\"{v['megapixels']:.1f}\"
    print(f\"{v['youtube_id']:<14}  {dur_str:>7}  {res:>10}  {mp:>5}  {v['scale']:>4}x  {v['gpu']:>10}  {v['title']}\")
"
}

# ---------- Main ----------

case "${1:-help}" in
    launch)
        if [[ -z "${2:-}" ]]; then
            echo "Usage: $0 launch <VIDEO_ID> [DISK_GB]"
            echo "Run '$0 list' to see available video IDs."
            exit 1
        fi
        cmd_launch "$2" "${3:-}"
        ;;
    test)       cmd_test "${2:-}" ;;
    status)     cmd_status ;;
    ssh)        cmd_ssh "${2:-0}" ;;
    download)   cmd_download "${2:-}" ;;
    destroy)    cmd_destroy ;;
    destroy-all) cmd_destroy_all ;;
    list)       cmd_list ;;
    help|*)
        echo "Usage: $0 <command> [args]"
        echo ""
        echo "Commands:"
        echo "  launch <VID> [DISK]  Launch pod for a video (disk GB auto-estimated)"
        echo "  test [VIDEO_ID]      Launch pod for BLUEPRINTS of LIFE (or specified video)"
        echo "  status               Show pod status, dashboard URL, costs"
        echo "  ssh [N]              SSH into pod N (default: 0)"
        echo "  download [DIR]       Download completed videos"
        echo "  destroy              Destroy pods (keeps network volume for reuse)"
        echo "  destroy-all          Destroy pods AND network volume"
        echo "  list                 List all videos from not_enhanced.json"
        echo ""
        echo "Environment:"
        echo "  RUNPOD_API_KEY       RunPod API key (required)"
        echo "  GPU_TYPE             GPU type (default: NVIDIA RTX PRO 6000 Blackwell Server Edition)"
        echo "  DATACENTER           RunPod datacenter (default: EU-RO-1 = Romania)"
        echo "  DISK_GB              Override disk size in GB"
        ;;
esac
