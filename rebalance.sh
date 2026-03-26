#!/bin/bash
# rebalance.sh — Redistribute remaining upscale work across all GPUs
# Usage: ./rebalance.sh [NUM_GPUS]
# Uses Rust rebalance_calc for fast frame counting (10-100x faster than Python).

set -uo pipefail
export PATH="/opt/venv/bin:/opt/conda/bin:/usr/local/bin:/usr/bin:/bin:$PATH"
cd /root

NUM_GPUS="${1:-}"
if [[ -z "$NUM_GPUS" ]]; then
    NUM_GPUS=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | wc -l)
fi

echo "=== Rebalance: $NUM_GPUS GPUs ==="

# Phase A: Kill all upscaling processes (resumable — max 1 frame lost per GPU)
echo ""
echo "=== Phase A: Stopping current processes ==="
pkill -9 -f 'upscale.py|enhance.sh|multi_gpu_queue' 2>/dev/null
sleep 2
echo "  Killed all upscale/enhance/queue processes"

# Phase B+C: Count frames and distribute (Rust — fast!)
echo ""
echo "=== Phase B+C: Counting frames and distributing (Rust) ==="

CALC_BIN=""
if [[ -x /root/rebalance_calc ]]; then
    CALC_BIN=/root/rebalance_calc
elif [[ -x /usr/local/bin/rebalance_calc ]]; then
    CALC_BIN=/usr/local/bin/rebalance_calc
fi

if [[ -n "$CALC_BIN" ]]; then
    # Fast path: Rust binary
    GPU_PLAN=$("$CALC_BIN" "$NUM_GPUS" 2>&1)
    CALC_STDERR=$(echo "$GPU_PLAN" | grep -v "^GPU:")
    CALC_STDOUT=$(echo "$GPU_PLAN" | grep "^GPU:")

    # Show inventory (stderr from rebalance_calc)
    echo "$CALC_STDERR"

    if [[ -z "$CALC_STDOUT" ]] || echo "$CALC_STDOUT" | grep -q "NOTHING"; then
        echo "  Nothing to rebalance — all frames done!"
        echo "  Restarting queue for reassembly..."
        ./restart.sh "$NUM_GPUS"
        exit 0
    fi

    # Phase D: Launch upscale.py per GPU from Rust plan
    echo ""
    echo "=== Phase D: Starting upscaling ==="

    echo "$CALC_STDOUT" | while IFS= read -r line; do
        gpu=$(echo "$line" | grep -oP 'GPU:\K\d+')
        tasks=$(echo "$line" | sed 's/^GPU:[0-9]* //')

        (
            for task in $tasks; do
                IFS=':' read -r vid start end scale <<< "$task"
                fi_dir="/root/jobs/$vid/frames_in"
                fo_dir="/root/jobs/$vid/frames_out"
                echo "GPU $gpu: upscaling $vid frames $start-$end (scale ${scale}x)"
                CUDA_VISIBLE_DEVICES=$gpu python3 /root/upscale.py "$fi_dir" "$fo_dir" "$scale" --start "$start" --end "$end"
            done
        ) >> "/root/gpu${gpu}.log" 2>&1 &
        echo "  GPU $gpu started (PID $!)"
    done
else
    # Fallback: Python (slow but works)
    echo "  WARNING: rebalance_calc not found, using slow Python fallback"

    for job_dir in /root/jobs/*/; do
        [ -d "$job_dir" ] || continue
        vid=$(basename "$job_dir")
        fi_dir="$job_dir/frames_in"
        fo_dir="$job_dir/frames_out"
        [ -d "$fi_dir" ] || continue

        count_in=$(find "$fi_dir" -maxdepth 1 -name "frame_*.png" 2>/dev/null | wc -l)
        [[ "$count_in" -eq 0 ]] && continue

        actual_remaining=$(python3 -c "
import glob, os
fi = set(os.path.basename(f) for f in glob.glob('$fi_dir/frame_*.png'))
fo = set(os.path.basename(f) for f in glob.glob('$fo_dir/frame_*.png'))
print(len(fi - fo))
" 2>/dev/null)

        [[ "$actual_remaining" -eq 0 ]] && continue

        scale=4
        for jf in /root/json/${vid}.json.processing.* /root/json/${vid}.json /root/json_done/${vid}.json; do
            if [ -f "$jf" ]; then
                scale=$(python3 -c "import json; print(json.load(open('$jf')).get('scale',4))" 2>/dev/null)
                break
            fi
        done

        echo "  $vid: $actual_remaining remaining, scale=${scale}x"
        # Simple: give each GPU the whole video, upscale.py skips done frames
        CUDA_VISIBLE_DEVICES=0 python3 /root/upscale.py "$fi_dir" "$fo_dir" "$scale" >> /root/gpu0.log 2>&1 &
        echo "  Started on GPU 0 (fallback mode)"
        break  # Only handle first video in fallback
    done
fi

# Phase E: Start queue for reassembly + auto-destroy
echo ""
echo "=== Phase E: Starting queue ==="

# Restore .processing files to .json
for f in /root/json/*.processing.*; do
    [ -f "$f" ] || continue
    base=$(echo "$f" | sed 's/\.processing\.[0-9]*//')
    vid=$(basename "$base" .json)
    if [ -f "/root/json_done/${vid}.json" ]; then
        rm -f "$f"
        continue
    fi
    mv "$f" "$base"
done

# Start status server if not running
if ! pgrep -x status_server > /dev/null 2>&1; then
    nohup ./status_server >> /root/status_server.log 2>&1 &
fi

# Start queue
nohup ./multi_gpu_queue.sh "$NUM_GPUS" >> /root/enhance.log 2>&1 &
echo "  Queue started ($NUM_GPUS GPUs)"
echo ""
echo "=== Rebalance complete ==="
