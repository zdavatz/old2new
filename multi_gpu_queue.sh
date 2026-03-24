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
            # No video available — check if we can help another GPU via dynamic joining
            if [[ "$NUM_GPUS" -gt 1 ]]; then
                local other_active
                other_active=$(ls "$QUEUE_DIR"/*.processing.* 2>/dev/null | wc -l)
                if [[ "$other_active" -ge 1 ]]; then
                    # Find a video with remaining frames to help with
                    local active_file active_vid job_dir frames_in frames_out remaining
                    for af in "$QUEUE_DIR"/*.processing.*; do
                        [ -f "$af" ] || continue
                        active_vid=$(basename "$af" | sed 's/\.json\.processing\..*//')
                        job_dir="$HOME/jobs/$active_vid"
                        frames_in="$job_dir/frames_in"
                        frames_out="$job_dir/frames_out"
                        local total_in=$(ls "$frames_in"/frame_*.png 2>/dev/null | wc -l)
                        local total_out=$(ls "$frames_out"/frame_*.png 2>/dev/null | wc -l)
                        remaining=$((total_in - total_out))
                        if [[ "$remaining" -gt 500 ]]; then
                            active_file="$af"
                            break
                        fi
                    done

                    if [[ -n "${active_file:-}" && "$remaining" -gt 500 ]]; then
                        local scale
                        scale=$(python3 -c "import json; print(json.load(open('$active_file')).get('scale',4))" 2>/dev/null)

                        # Count how many GPUs are free (including us)
                        local busy_gpus free_gpus
                        busy_gpus=$(ls "$QUEUE_DIR"/*.processing.* 2>/dev/null | wc -l)
                        free_gpus=$((NUM_GPUS - busy_gpus))
                        [[ "$free_gpus" -lt 1 ]] && free_gpus=1

                        # Calculate our segment from the remaining frames
                        # We take the LAST portion — the original GPU continues from the front
                        local total_in
                        total_in=$(ls "$frames_in"/frame_*.png 2>/dev/null | wc -l)
                        local seg_size=$(( remaining / (free_gpus + 1) ))  # +1 for the original GPU
                        local our_start=$(( total_in - seg_size ))
                        [[ "$our_start" -lt 0 ]] && our_start=0

                        echo "[GPU $gpu] Joining upscale for $active_vid: frames $our_start-$total_in ($seg_size frames, $free_gpus free GPUs)"

                        CUDA_VISIBLE_DEVICES=$gpu python3 "$SCRIPT_DIR/upscale.py" \
                            "$frames_in" "$frames_out" "$scale" \
                            --start "$our_start" --end "$total_in" >> "$logfile" 2>&1

                        echo "[GPU $gpu] Done helping with $active_vid"
                        continue
                    fi
                fi
            fi

            # Check if ALL GPUs are idle — auto-destroy if queue completely empty
            local all_processing
            all_processing=$(ls "$QUEUE_DIR"/*.processing.* 2>/dev/null | wc -l)
            local all_queued
            all_queued=$(ls "$QUEUE_DIR"/*.json 2>/dev/null | wc -l)
            # Verify last upload had email sent (check all gpu logs)
            local last_email_ok=0
            if [[ -d "$DONE_DIR" ]] && ls "$DONE_DIR"/*.json >/dev/null 2>&1; then
                local last_success
                last_success=$(grep -l "Email sent" "$HOME"/gpu*.log 2>/dev/null | head -1)
                [[ -n "$last_success" ]] && last_email_ok=1
            fi

            if [[ "$all_processing" -eq 0 && "$all_queued" -eq 0 && "$last_email_ok" -eq 1 && -f "$HOME/instance_meta.json" ]]; then
                # Queue completely empty, no GPUs working, last email confirmed — auto-destroy after 10 min grace period
                if [[ -z "${IDLE_SINCE:-}" ]]; then
                    IDLE_SINCE=$(date +%s)
                    echo "[GPU $gpu] Queue empty, all GPUs idle. Auto-destroy in 10 min (add videos to cancel)."
                fi
                local idle_secs=$(( $(date +%s) - IDLE_SINCE ))
                if [[ "$idle_secs" -ge 600 ]]; then
                    echo "[GPU $gpu] Auto-destroying instance after 10 min idle..."
                    local inst_id
                    inst_id=$(python3 -c "import json; print(json.load(open('$HOME/instance_meta.json')).get('instance_id',''))" 2>/dev/null)
                    local api_key
                    api_key=$(cat "$HOME/.vast_api_key" 2>/dev/null)
                    if [[ -n "$inst_id" && -n "$api_key" ]]; then
                        curl -s -X PUT "https://console.vast.ai/api/v0/instances/${inst_id}/" \
                            -H "Authorization: Bearer ${api_key}" \
                            -d '{"state": "stopped"}' >/dev/null 2>&1
                        echo "[GPU $gpu] Instance $inst_id stopped via API."
                    else
                        echo "[GPU $gpu] Cannot auto-destroy: missing instance_id or API key."
                    fi
                    exit 0
                fi
            else
                IDLE_SINCE=""  # Reset if new work appeared
            fi

            # Normal polling — wait for new videos
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
