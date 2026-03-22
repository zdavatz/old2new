#!/bin/bash
# Multi-GPU queue processor — reads queue from ~/json/, runs enhance.sh per GPU
#
# Usage: ./multi_gpu_queue.sh [NUM_GPUS]
#   NUM_GPUS: number of GPUs to use (default: auto-detect from nvidia-smi)
#
# Queue source: ~/json/*.json files (one per video)
# Each GPU worker atomically picks the next JSON file via flock
# After successful upload: JSON moves to ~/json_done/
# OOM-kill recovery: retries same video up to 3 times
# PID-file per GPU: ~/gpu{N}.worker.pid

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
QUEUE_DIR="$HOME/json"
DONE_DIR="$HOME/json_done"
LOCK_FILE="$HOME/queue.lock"

mkdir -p "$DONE_DIR"

# Auto-detect GPU count
if [[ ${1:-} =~ ^[0-9]+$ ]]; then
    NUM_GPUS=$1
else
    NUM_GPUS=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | wc -l)
    if [[ "$NUM_GPUS" -eq 0 ]]; then
        echo "ERROR: No GPUs detected"
        exit 1
    fi
fi

echo "=== Multi-GPU queue started at $(date) ==="
echo "GPUs: $NUM_GPUS | Queue: $QUEUE_DIR/ | Done: $DONE_DIR/"

# Start status server if not running
if ! pgrep -f status_server > /dev/null 2>&1; then
    if [[ -x "$HOME/status_server" ]]; then
        nohup "$HOME/status_server" >> "$HOME/status_server.log" 2>&1 &
        echo "Status server started"
    elif [[ -f "$HOME/status_server.py" ]]; then
        nohup python3 "$HOME/status_server.py" >> "$HOME/status_server.log" 2>&1 &
        echo "Status server (Python) started"
    fi
fi

# Pick next video from json/ queue (atomic via flock)
pick_next_video() {
    flock "$LOCK_FILE" bash -c '
        QUEUE_DIR="'"$QUEUE_DIR"'"
        DONE_DIR="'"$DONE_DIR"'"
        for f in "$QUEUE_DIR"/*.json; do
            [ -f "$f" ] || continue
            basename "$f"
            break
        done
    '
}

# GPU worker loop
gpu_worker() {
    local gpu=$1
    local pidfile="$HOME/gpu${gpu}.worker.pid"
    local logfile="$HOME/gpu${gpu}.log"

    # PID-file locking
    echo $BASHPID > "$pidfile"
    sleep 1
    if [[ "$(cat "$pidfile" 2>/dev/null)" != "$BASHPID" ]]; then
        echo "[GPU $gpu] Lost race — aborting"
        return
    fi

    echo "[GPU $gpu] Worker started (PID $BASHPID)"

    while true; do
        # Atomically pick next JSON file
        local json_file
        json_file=$(flock "$LOCK_FILE" bash -c '
            for f in "'"$QUEUE_DIR"'"/*.json; do
                [ -f "$f" ] || continue
                mv "$f" "$f.processing.'"$gpu"'"
                echo "$(basename "$f" .json).processing.'"$gpu"'"
                break
            done
        ')

        if [[ -z "$json_file" ]]; then
            echo "[GPU $gpu] No more videos in queue. Done at $(date)."
            break
        fi

        local processing_path="$QUEUE_DIR/$json_file"
        local video_id="${json_file%.processing.*}"

        # Read video info from JSON
        local vid scale title
        vid=$(python3 -c "import json; d=json.load(open('$processing_path')); print(d.get('video_id',''))" 2>/dev/null)
        scale=$(python3 -c "import json; d=json.load(open('$processing_path')); print(d.get('scale',4))" 2>/dev/null)
        title=$(python3 -c "import json; d=json.load(open('$processing_path')); print(d.get('title','').replace(' ','_'))" 2>/dev/null)

        # Fallback title
        if [[ -z "$title" ]]; then
            title="$video_id"
        fi

        echo "[GPU $gpu] Starting: $title ($(date +%H:%M))"

        # Retry on OOM-kill (exit > 128)
        local max_retries=3 retry=0
        local success=0
        while true; do
            "$SCRIPT_DIR/enhance.sh" "https://www.youtube.com/watch?v=$vid" "$scale" \
                --job-name "$title" --gpu "$gpu" >> "$logfile" 2>&1
            local exit_code=$?

            if [[ $exit_code -eq 0 ]]; then
                echo "[GPU $gpu] SUCCESS: $title"
                # Move JSON to done
                mv "$processing_path" "$DONE_DIR/${video_id}.json"
                success=1
                break
            elif [[ $exit_code -gt 128 ]]; then
                retry=$((retry + 1))
                if [[ $retry -ge $max_retries ]]; then
                    echo "[GPU $gpu] GIVING UP after $max_retries retries: $title"
                    break
                fi
                echo "[GPU $gpu] Killed (exit $exit_code), waiting 60s, retry $retry/$max_retries: $title"
                sleep 60
            else
                echo "[GPU $gpu] FAILED (exit $exit_code): $title"
                break
            fi
        done

        # If failed, move JSON back to queue for later retry
        if [[ $success -eq 0 && -f "$processing_path" ]]; then
            mv "$processing_path" "$QUEUE_DIR/${video_id}.json"
        fi
    done

    rm -f "$pidfile"
}

# Start one worker per GPU
for ((gpu=0; gpu<NUM_GPUS; gpu++)); do
    gpu_worker "$gpu" &
done

wait
echo "=== All videos done at $(date) ==="
