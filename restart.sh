#!/bin/bash
# Restart all services on this instance
# Usage: ./restart.sh [NUM_GPUS]  (default: auto-detect)
#
# Kills ALL old processes, replaces .new binaries, restores queue, starts fresh.
# Run this directly on the server after deploy.sh update.

set -uo pipefail
export PATH="/opt/venv/bin:/usr/local/bin:/usr/bin:/bin:$PATH"
cd /root

NUM_GPUS="${1:-}"

echo "=== Killing all processes ==="
# Get ALL PIDs in one shot, kill them all
PIDS=$(pgrep -f 'status_server|multi_gpu_queue|enhance.sh|/root/enhance|enhance_gpu.py|upscale.py|korea_single' 2>/dev/null | grep -v "^$$\$" || true)
if [[ -n "$PIDS" ]]; then
    echo "$PIDS" | xargs kill -9 2>/dev/null
    echo "Killed: $PIDS"
    sleep 3
fi

# Verify
REMAINING=$(pgrep -f 'status_server|multi_gpu_queue|enhance|upscale' 2>/dev/null | wc -l)
echo "Remaining: $REMAINING processes"

echo ""
echo "=== Replacing binaries ==="
# Test .new binaries before replacing — avoid overwriting working binary with glibc-incompatible one
for bin in status_server youtube_upload enhance frame_gap_check rebalance_calc; do
    if [ -f "/root/${bin}.new" ]; then
        if ldd "/root/${bin}.new" 2>&1 | grep -q "not found"; then
            rm -f "/root/${bin}.new"
            echo "  ${bin}: .new binary incompatible (glibc), keeping current"
        else
            mv -f "/root/${bin}.new" "/root/${bin}"
            echo "  ${bin} replaced"
        fi
    else
        echo "  ${bin}: no .new file"
    fi
    # Fallback to Docker image binary if current one has glibc issues
    if ldd "/root/${bin}" 2>&1 | grep -q "not found" && [ -f "/usr/local/bin/${bin}" ]; then
        cp "/usr/local/bin/${bin}" "/root/${bin}"
        echo "  ${bin}: using Docker image binary (glibc compat)"
    fi
done
chmod +x /root/status_server /root/youtube_upload /root/enhance /root/enhance.sh /root/frame_gap_check /root/rebalance_calc /root/multi_gpu_queue.sh 2>/dev/null

echo ""
echo "=== Restoring queue ==="
for f in /root/json/*.processing.*; do
    [ -f "$f" ] || continue
    base=$(echo "$f" | sed 's/\.processing\.[0-9]*//')
    vid=$(basename "$base" .json)
    # Skip if already uploaded (json exists in json_done/)
    if [ -f "/root/json_done/${vid}.json" ]; then
        echo "  Skipping $vid (already uploaded)"
        rm -f "$f"
        continue
    fi
    mv "$f" "$base"
done
echo "  Queue: $(ls /root/json/*.json 2>/dev/null | wc -l) JSON files"

echo ""
echo "=== Starting status_server ==="
nohup ./status_server >> /root/status_server.log 2>&1 &
sleep 2
if pgrep -x status_server > /dev/null; then
    echo "  OK (PID $(pgrep -x status_server))"
else
    echo "  FAILED — check /root/status_server.log"
fi

echo ""
echo "=== Starting queue ==="
if [[ -z "$NUM_GPUS" ]]; then
    NUM_GPUS=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | wc -l)
fi
nohup ./multi_gpu_queue.sh "$NUM_GPUS" >> /root/enhance.log 2>&1 &
sleep 2
if pgrep -f multi_gpu_queue > /dev/null; then
    echo "  OK — $NUM_GPUS GPUs (PID $(pgrep -f multi_gpu_queue | head -1))"
else
    echo "  FAILED — check /root/enhance.log"
fi

echo ""
echo "=== Dashboard ==="
# Read instance meta for label
if [[ -f /root/instance_meta.json ]]; then
    python3 -c "import json; d=json.load(open('/root/instance_meta.json')); print(f'  Instance: {d.get(\"label\",\"?\")} ({d.get(\"location\",\"?\")})')" 2>/dev/null
fi
echo "  Port: 8080 (access via vast.ai SSH port+1 or SSH tunnel)"
echo ""
echo "=== Done ==="
