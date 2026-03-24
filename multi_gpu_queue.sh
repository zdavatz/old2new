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

export PATH="/opt/venv/bin:/opt/conda/bin:/usr/local/bin:/usr/bin:/bin:$PATH"
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
# Prioritizes videos with most existing frames_out (resume after restart)
pick_next_video() {
    flock "$LOCK_FILE" bash -c '
        QUEUE_DIR="'"$QUEUE_DIR"'"
        JOBS_DIR="$HOME/jobs"
        best=""
        best_count=0
        for f in "$QUEUE_DIR"/*.json; do
            [ -f "$f" ] || continue
            vid=$(basename "$f" .json)
            count=$(ls "$JOBS_DIR/$vid/frames_out/" 2>/dev/null | wc -l)
            if [ "$count" -gt "$best_count" ]; then
                best_count=$count
                best=$(basename "$f")
            fi
        done
        if [ -n "$best" ] && [ "$best_count" -gt 0 ]; then
            echo "$best"
        else
            # No in-progress videos, pick first available
            for f in "$QUEUE_DIR"/*.json; do
                [ -f "$f" ] || continue
                basename "$f"
                break
            done
        fi
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
                basename "$f"
                break
            done
        ')

        if [[ -z "$json_file" ]]; then
            # No video available — wait and poll instead of exiting
            # Videos may be added to the queue later
            sleep 30
            continue
        fi

        local processing_path="$QUEUE_DIR/${json_file}.processing.${gpu}"
        local video_id="${json_file%.json}"

        # Read video info from JSON
        local vid scale title
        vid=$(python3 -c "import json; d=json.load(open('$processing_path')); print(d.get('video_id',''))" 2>/dev/null)
        scale=$(python3 -c "import json; d=json.load(open('$processing_path')); print(d.get('scale',4))" 2>/dev/null)
        title=$(python3 -c "import json; d=json.load(open('$processing_path')); print(d.get('title',''))" 2>/dev/null)

        # Use video_id as job name (safe for directories — no slashes, emojis, etc.)
        if [[ -z "$vid" ]]; then
            vid="$video_id"
        fi

        echo "[GPU $gpu] Starting: $title ($vid) ($(date +%H:%M))"

        # Check if this is the only video and multiple GPUs are free → segment splitting
        local remaining_queue
        remaining_queue=$(ls "$QUEUE_DIR"/*.json 2>/dev/null | wc -l)
        local other_processing
        other_processing=$(ls "$QUEUE_DIR"/*.processing.* 2>/dev/null | grep -v "processing.${gpu}" | wc -l)
        local use_segment_split=0
        if [[ "$remaining_queue" -eq 0 && "$other_processing" -eq 0 && "$NUM_GPUS" -gt 1 ]]; then
            use_segment_split=1
            echo "[GPU $gpu] Only video in queue — segment splitting across $NUM_GPUS GPUs"
        fi

        # Retry on OOM-kill (exit > 128)
        local max_retries=3 retry=0
        local success=0
        while true; do
            if [[ "$use_segment_split" -eq 1 ]]; then
                # No --gpu flag → enhance.sh detects all GPUs and splits segments
                "$SCRIPT_DIR/enhance.sh" "https://www.youtube.com/watch?v=$vid" "$scale" \
                    --job-name "$vid" >> "$logfile" 2>&1
            else
                "$SCRIPT_DIR/enhance.sh" "https://www.youtube.com/watch?v=$vid" "$scale" \
                    --job-name "$vid" --gpu "$gpu" >> "$logfile" 2>&1
            fi
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
