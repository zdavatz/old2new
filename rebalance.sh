#!/bin/bash
# rebalance.sh — Redistribute remaining upscale work across all GPUs
# Usage: ./rebalance.sh [NUM_GPUS]
# Runs on the remote instance. Kills running upscale processes, counts
# remaining frames per video, distributes evenly across GPUs, then
# restarts queue for reassembly/upload.

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

# Phase B: Inventory remaining work per video
echo ""
echo "=== Phase B: Counting remaining frames ==="

WORK=()  # array of "video_id|frames_in_dir|frames_out_dir|remaining|scale"
TOTAL_REMAINING=0

for job_dir in /root/jobs/*/; do
    [ -d "$job_dir" ] || continue
    vid=$(basename "$job_dir")
    fi_dir="$job_dir/frames_in"
    fo_dir="$job_dir/frames_out"

    [ -d "$fi_dir" ] || continue

    count_in=$(find "$fi_dir" -maxdepth 1 -name "frame_*.png" 2>/dev/null | wc -l)
    count_out=$(find "$fo_dir" -maxdepth 1 -name "frame_*.png" 2>/dev/null | wc -l)
    remaining=$((count_in))  # upscale.py skips done frames, so all frames_in are candidates

    # Skip if no input frames left
    [[ "$count_in" -eq 0 ]] && continue

    # Get scale from json
    scale=4
    for jf in /root/json/${vid}.json.processing.* /root/json/${vid}.json /root/json_done/${vid}.json; do
        if [ -f "$jf" ]; then
            scale=$(python3 -c "import json; print(json.load(open('$jf')).get('scale',4))" 2>/dev/null)
            break
        fi
    done

    # Actual remaining = frames without output
    actual_remaining=$(python3 -c "
import glob, os
fi = set(os.path.basename(f) for f in glob.glob('$fi_dir/frame_*.png'))
fo = set(os.path.basename(f) for f in glob.glob('$fo_dir/frame_*.png'))
print(len(fi - fo))
" 2>/dev/null)

    [[ "$actual_remaining" -eq 0 ]] && continue

    WORK+=("$vid|$fi_dir|$fo_dir|$actual_remaining|$scale|$count_in")
    TOTAL_REMAINING=$((TOTAL_REMAINING + actual_remaining))
    printf "  %-40s %6d remaining (%d/%d done) scale=%sx\n" "$vid" "$actual_remaining" "$count_out" "$((count_in + count_out))" "$scale"
done

if [[ ${#WORK[@]} -eq 0 ]]; then
    echo "  Nothing to rebalance — all frames done!"
    echo "  Restarting queue for reassembly..."
    ./restart.sh "$NUM_GPUS"
    exit 0
fi

echo ""
echo "  Total: $TOTAL_REMAINING frames across ${#WORK[@]} videos on $NUM_GPUS GPUs"
TARGET_PER_GPU=$(( (TOTAL_REMAINING + NUM_GPUS - 1) / NUM_GPUS ))
echo "  Target: ~$TARGET_PER_GPU frames per GPU"

# Phase C: Distribute work across GPUs
echo ""
echo "=== Phase C: Distributing segments ==="

# Sort videos by remaining (largest first)
IFS=$'\n' SORTED=($(printf '%s\n' "${WORK[@]}" | sort -t'|' -k4 -rn))
unset IFS

# Build per-GPU task lists
declare -a GPU_TASKS  # GPU_TASKS[g] = "vid1:start:end:scale vid2:start:end:scale ..."
for ((g=0; g<NUM_GPUS; g++)); do
    GPU_TASKS[$g]=""
done

gpu_loads=()
for ((g=0; g<NUM_GPUS; g++)); do
    gpu_loads[$g]=0
done

for work_item in "${SORTED[@]}"; do
    IFS='|' read -r vid fi_dir fo_dir remaining scale total_in <<< "$work_item"

    if [[ "$remaining" -le "$TARGET_PER_GPU" ]]; then
        # Small enough for one GPU — assign to least loaded
        min_gpu=0
        min_load=${gpu_loads[0]}
        for ((g=1; g<NUM_GPUS; g++)); do
            if [[ ${gpu_loads[$g]} -lt $min_load ]]; then
                min_gpu=$g
                min_load=${gpu_loads[$g]}
            fi
        done
        GPU_TASKS[$min_gpu]="${GPU_TASKS[$min_gpu]} ${vid}:0:${total_in}:${scale}"
        gpu_loads[$min_gpu]=$((gpu_loads[$min_gpu] + remaining))
    else
        # Large video — split remaining (todo) frames across GPUs
        # Find the first and last undone frame indices to avoid assigning already-done ranges
        local todo_range
        todo_range=$(python3 -c "
import glob, os
fi = sorted(glob.glob('$fi_dir/frame_*.png'))
fo = set(os.path.basename(f) for f in glob.glob('$fo_dir/frame_*.png'))
todo_idx = [i for i, f in enumerate(fi) if os.path.basename(f) not in fo]
if todo_idx:
    print(f'{todo_idx[0]} {todo_idx[-1]+1} {len(todo_idx)}')
else:
    print('0 0 0')
" 2>/dev/null)
        local todo_start todo_end todo_count
        read -r todo_start todo_end todo_count <<< "$todo_range"

        if [[ "$todo_count" -gt 0 ]]; then
            local todo_per_gpu=$(( (todo_end - todo_start + NUM_GPUS - 1) / NUM_GPUS ))
            for ((g=0; g<NUM_GPUS; g++)); do
                start=$(( todo_start + g * todo_per_gpu ))
                end=$(( todo_start + (g + 1) * todo_per_gpu ))
                [[ $end -gt $todo_end ]] && end=$todo_end
                [[ $start -ge $todo_end ]] && continue
                seg_size=$((end - start))
                GPU_TASKS[$g]="${GPU_TASKS[$g]} ${vid}:${start}:${end}:${scale}"
                gpu_loads[$g]=$((gpu_loads[$g] + seg_size))
            done
        fi
    fi
done

# Show assignment
for ((g=0; g<NUM_GPUS; g++)); do
    echo "  GPU $g: ${gpu_loads[$g]} frames — ${GPU_TASKS[$g]}"
done

# Phase D: Launch upscale.py per GPU
echo ""
echo "=== Phase D: Starting upscaling ==="

for ((g=0; g<NUM_GPUS; g++)); do
    tasks="${GPU_TASKS[$g]}"
    [[ -z "$tasks" ]] && continue

    (
        for task in $tasks; do
            IFS=':' read -r vid start end scale <<< "$task"
            fi_dir="/root/jobs/$vid/frames_in"
            fo_dir="/root/jobs/$vid/frames_out"
            echo "GPU $g: upscaling $vid frames $start-$end (scale ${scale}x)"
            CUDA_VISIBLE_DEVICES=$g python3 /root/upscale.py "$fi_dir" "$fo_dir" "$scale" --start "$start" --end "$end"
        done
    ) >> "/root/gpu${g}.log" 2>&1 &
    echo "  GPU $g started (PID $!)"
done

echo ""
echo "=== Phase D2: Starting queue (handles reassembly + dynamic joining) ==="
# Don't wait for segments — start multi_gpu_queue.sh immediately.
# Workers will: 1) find no queued JSON, 2) see processing files,
# 3) dynamic-join the remaining video when their segment finishes.
# When all upscaling done, workers pick up the video for reassembly + upload.

# Restore .processing files to .json so queue can manage them
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
    pkill -x status_server 2>/dev/null
    nohup ./status_server >> /root/status_server.log 2>&1 &
fi

# Start queue — workers will wait for upscale segments to finish,
# then pick up the video for reassembly
nohup ./multi_gpu_queue.sh "$NUM_GPUS" >> /root/enhance.log 2>&1 &
echo "  Queue started ($NUM_GPUS GPUs) — workers will dynamic-join when segments finish"
echo ""
echo "=== Rebalance complete ==="
