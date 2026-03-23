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
PIDS=$(pgrep -f 'status_server|multi_gpu_queue|enhance.sh|enhance_gpu.py|upscale.py|korea_single' 2>/dev/null | grep -v "^$$\$" || true)
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
mv -f /root/status_server.new /root/status_server 2>/dev/null && echo "  status_server replaced" || echo "  status_server: no .new file"
mv -f /root/youtube_upload.new /root/youtube_upload 2>/dev/null && echo "  youtube_upload replaced" || echo "  youtube_upload: no .new file"
chmod +x /root/status_server /root/youtube_upload /root/enhance.sh /root/multi_gpu_queue.sh 2>/dev/null

echo ""
echo "=== Restoring queue ==="
for f in /root/json/*.processing.*; do
    [ -f "$f" ] || continue
    base=$(echo "$f" | sed 's/\.processing\.[0-9]*//')
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
echo "=== Done ==="
