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
# Kill by PID files first (reliable)
for pidfile in /root/status_server.pid /root/gpu_scheduler.pid /root/preparer.pid /root/youtube_upload.pid; do
    if [[ -f "$pidfile" ]]; then
        PID=$(cat "$pidfile")
        kill -9 "$PID" 2>/dev/null && echo "  Killed $(basename $pidfile .pid) (PID $PID)"
        rm -f "$pidfile"
    fi
done
# Also kill any remaining processes not tracked by PID files
PIDS=$(pgrep -f 'status_server|gpu_scheduler|preparer|multi_gpu_queue|enhance.sh|/root/enhance|enhance_gpu.py|upscale.py|youtube_upload|korea_single' 2>/dev/null | grep -v "^$$\$" || true)
if [[ -n "$PIDS" ]]; then
    echo "$PIDS" | xargs kill -9 2>/dev/null
    echo "  Killed remaining: $PIDS"
fi
sleep 3

# Verify
REMAINING=$(pgrep -f 'status_server|multi_gpu_queue|enhance|upscale|preparer|youtube_upload' 2>/dev/null | wc -l)
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
chmod +x /root/status_server /root/youtube_upload /root/enhance /root/enhance.sh /root/frame_gap_check /root/rebalance_calc /root/gpu_scheduler /root/multi_gpu_queue.sh 2>/dev/null

echo ""
echo "=== Restoring queue ==="
for f in /root/json/*.processing*; do
    [ -f "$f" ] || continue
    base=$(echo "$f" | sed 's/\.processing.*//')
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
echo $! > /root/status_server.pid
sleep 2
if kill -0 $(cat /root/status_server.pid) 2>/dev/null; then
    echo "  OK (PID $(cat /root/status_server.pid))"
else
    echo "  FAILED — check /root/status_server.log"
fi

echo ""
echo "=== Starting queue ==="
if [[ -z "$NUM_GPUS" ]]; then
    NUM_GPUS=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | wc -l)
fi
# Start preparer (download + extract, no GPU needed)
echo ""
echo "=== Starting preparer ==="
if [[ -x /root/preparer ]]; then
    nohup ./preparer >> /root/preparer.log 2>&1 &
    echo $! > /root/preparer.pid
    sleep 1
    if kill -0 $(cat /root/preparer.pid) 2>/dev/null; then
        echo "  OK (PID $(cat /root/preparer.pid))"
    else
        echo "  FAILED — check /root/preparer.log"
    fi
else
    echo "  preparer binary not found — download/extract must be done manually"
fi

# Start gpu_scheduler (upscale + reassemble)
if [[ -x /root/gpu_scheduler ]]; then
    nohup ./gpu_scheduler "$NUM_GPUS" >> /root/scheduler.log 2>&1 &
    echo $! > /root/gpu_scheduler.pid
    sleep 2
    if kill -0 $(cat /root/gpu_scheduler.pid) 2>/dev/null; then
        echo "  OK — $NUM_GPUS GPUs via gpu_scheduler (PID $(cat /root/gpu_scheduler.pid))"
    else
        echo "  gpu_scheduler FAILED, falling back to multi_gpu_queue.sh"
        nohup ./multi_gpu_queue.sh "$NUM_GPUS" >> /root/enhance.log 2>&1 &
        sleep 2
        echo "  Fallback OK (PID $(pgrep -f multi_gpu_queue | head -1))"
    fi
else
    nohup ./multi_gpu_queue.sh "$NUM_GPUS" >> /root/enhance.log 2>&1 &
    sleep 2
    if pgrep -f multi_gpu_queue > /dev/null; then
        echo "  OK — $NUM_GPUS GPUs (PID $(pgrep -f multi_gpu_queue | head -1))"
    else
        echo "  FAILED — check /root/enhance.log"
    fi
fi

echo ""
echo "=== Starting youtube_upload --watch ==="
if [[ -x /root/youtube_upload ]]; then
    nohup ./youtube_upload --watch >> /root/upload.log 2>&1 &
    echo $! > /root/youtube_upload.pid
    sleep 1
    if kill -0 $(cat /root/youtube_upload.pid) 2>/dev/null; then
        echo "  OK (PID $(cat /root/youtube_upload.pid))"
    else
        echo "  FAILED — check /root/upload.log"
    fi
else
    echo "  youtube_upload binary not found — upload must be done manually"
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
