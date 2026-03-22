#!/bin/bash
# Deploy videos to the right cloud GPU instance
#
# Usage: ./deploy.sh <video_id> [video_id2] ...
# Example: ./deploy.sh BR5U-miBmt4 wjAkVoSN8jE yt1tQsqYI1s
#
# The script:
# 1. Reads json/<video_id>.json for each video (resolution, duration, GPU requirement)
# 2. Determines: single-GPU or multi-GPU, RTX 4090 or RTX 5090
# 3. Calculates total disk needed
# 4. Searches vast.ai for matching instances
# 5. Creates instance, deploys scripts + queue, starts processing

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
JSON_DIR="$SCRIPT_DIR/json"

if [ $# -eq 0 ]; then
    echo "Usage: $0 <video_id> [video_id2] ..."
    echo ""
    echo "Reads json/<video_id>.json for each video and deploys to the right instance."
    echo ""
    echo "Examples:"
    echo "  $0 BR5U-miBmt4                    # single HD video → 1x RTX 5090"
    echo "  $0 wjAkVoSN8jE yt1tQsqYI1s        # short HD videos → 4x RTX 4090"
    echo "  $0 c62HSWqoxKo 8wqZivWVLZs         # SD videos → 4x RTX 4090"
    exit 1
fi

# ============================================================
# Phase 1: Analyze videos from JSON files
# ============================================================
echo "=== Analyzing ${#} videos ==="

TOTAL_DISK_GB=0
TOTAL_DURATION=0
MAX_MP=0
NEEDS_5090=0
VIDEO_COUNT=$#
VIDEOS=()

for vid in "$@"; do
    json_file="$JSON_DIR/${vid}.json"
    if [[ ! -f "$json_file" ]]; then
        echo "ERROR: No JSON file for $vid — run: ./fetch_video_json.sh $vid"
        exit 1
    fi

    info=$(python3 -c "
import json
d = json.load(open('$json_file'))
w = d.get('width', 0)
h = d.get('height', 0)
dur = d.get('duration_seconds', 0)
fps = d.get('fps', 25)
mp = d.get('megapixels', 0)
scale = d.get('scale', 4)
gpu = d.get('gpu', 'RTX 4090')
title = d.get('title', '$vid')

# Disk estimate: input frames + output frames with PNG compression
frames = int(dur * fps)
input_sz = w * h * 3 / 2.5 / 1024 / 1024  # MB per frame
output_sz = w * scale * h * scale * 3 / 2.5 / 1024 / 1024
disk_gb = (frames * input_sz + frames * output_sz) / 1024 * 1.2 + 5

print(f'{vid}|{w}|{h}|{dur}|{mp}|{scale}|{gpu}|{disk_gb:.0f}|{title}')
")

    IFS='|' read -r v_id v_w v_h v_dur v_mp v_scale v_gpu v_disk v_title <<< "$info"
    VIDEOS+=("$info")
    TOTAL_DISK_GB=$((TOTAL_DISK_GB + v_disk))
    TOTAL_DURATION=$((TOTAL_DURATION + v_dur))

    # Track if any video needs RTX 5090
    if [[ "$v_gpu" == "RTX 5090" ]]; then
        NEEDS_5090=1
    fi

    # Track max megapixels
    if python3 -c "exit(0 if $v_mp > $MAX_MP else 1)" 2>/dev/null; then
        MAX_MP="$v_mp"
    fi

    printf "  %-50s %sx%s  %ss  %sx  %s  ~%sGB\n" "$v_title" "$v_w" "$v_h" "$v_dur" "$v_scale" "$v_gpu" "$v_disk"
done

echo ""
echo "Total: $VIDEO_COUNT videos, ${TOTAL_DURATION}s duration, ~${TOTAL_DISK_GB}GB disk needed"

# ============================================================
# Phase 2: Determine instance type
# ============================================================

# GPU type
if [[ "$NEEDS_5090" -eq 1 ]]; then
    GPU_NAME="RTX_5090"
    GPU_LABEL="RTX 5090"
    MIN_CPU_GHZ="3.0"
    MIN_RAM_GB=128
else
    GPU_NAME="RTX_4090"
    GPU_LABEL="RTX 4090"
    MIN_CPU_GHZ="2.0"
    MIN_RAM_GB=32
fi

# Single vs Multi GPU
if [[ $VIDEO_COUNT -le 2 ]]; then
    NUM_GPUS=1
    # Single GPU needs full disk for one video at a time
    DISK_GB=$((TOTAL_DISK_GB / VIDEO_COUNT * 2))  # 2x for safety
elif [[ $VIDEO_COUNT -le 8 ]]; then
    NUM_GPUS=4
    # Multi GPU: 4 videos parallel, but sequential overall
    # Need disk for 4 concurrent videos
    # Sort videos by disk, take top 4
    TOP4_DISK=$(printf '%s\n' "${VIDEOS[@]}" | sort -t'|' -k8 -rn | head -4 | awk -F'|' '{sum+=$8} END {print sum}')
    DISK_GB=$((TOP4_DISK + 50))  # 50GB overhead
else
    NUM_GPUS=4
    # Many videos: need disk for 4 concurrent + some buffer
    TOP4_DISK=$(printf '%s\n' "${VIDEOS[@]}" | sort -t'|' -k8 -rn | head -4 | awk -F'|' '{sum+=$8} END {print sum}')
    DISK_GB=$((TOP4_DISK + 100))
fi

# Minimum disk
[[ $DISK_GB -lt 500 ]] && DISK_GB=500

echo ""
echo "=== Recommended Setup ==="
echo "  GPU:  ${NUM_GPUS}x $GPU_LABEL"
echo "  CPU:  >= ${MIN_CPU_GHZ} GHz"
echo "  RAM:  >= ${MIN_RAM_GB} GB"
echo "  Disk: >= ${DISK_GB} GB"
echo ""

# ============================================================
# Phase 3: Search vast.ai for matching instance
# ============================================================
echo "=== Searching vast.ai ==="

SEARCH_RESULTS=$(vastai search offers "num_gpus>=${NUM_GPUS} gpu_name=${GPU_NAME} disk_space>=${DISK_GB} cpu_ghz>=${MIN_CPU_GHZ} verified=true" -o 'dph' 2>/dev/null | head -6)

if [[ -z "$SEARCH_RESULTS" || $(echo "$SEARCH_RESULTS" | wc -l) -le 1 ]]; then
    echo "No matching instances found on vast.ai!"
    echo "Try relaxing requirements or check vast.ai availability."
    exit 1
fi

echo "$SEARCH_RESULTS"
echo ""

# Extract best offer ID (first result after header)
OFFER_ID=$(echo "$SEARCH_RESULTS" | awk 'NR==2 {print $1}')
OFFER_PRICE=$(echo "$SEARCH_RESULTS" | awk 'NR==2 {for(i=1;i<=NF;i++) if($i ~ /^[0-9]+\.[0-9]+$/ && $i < 10) {print $i; exit}}')
OFFER_LOCATION=$(echo "$SEARCH_RESULTS" | awk 'NR==2 {print $NF}')

echo "Best offer: ID=$OFFER_ID, \$${OFFER_PRICE}/hr, $OFFER_LOCATION"
echo ""

# ============================================================
# Phase 4: Confirm and create instance
# ============================================================
read -p "Create instance and deploy $VIDEO_COUNT videos? [y/N] " confirm
if [[ "$confirm" != "y" && "$confirm" != "Y" ]]; then
    echo "Aborted."
    exit 0
fi

echo ""
echo "=== Creating instance ==="
CREATE_RESULT=$(vastai create instance "$OFFER_ID" \
    --image ghcr.io/zdavatz/realesrgan-benchmark:latest \
    --disk "$DISK_GB" \
    --label "davaz-${GPU_NAME,,}-${VIDEO_COUNT}vid" \
    --ssh --direct 2>&1)

echo "$CREATE_RESULT"
INSTANCE_ID=$(echo "$CREATE_RESULT" | python3 -c "import json,sys; print(json.loads(sys.stdin.read()).get('new_contract',''))" 2>/dev/null)

if [[ -z "$INSTANCE_ID" ]]; then
    echo "ERROR: Failed to create instance"
    exit 1
fi

echo "Instance ID: $INSTANCE_ID"
echo ""

# ============================================================
# Phase 5: Wait for SSH
# ============================================================
echo "=== Waiting for instance to start ==="
for i in $(seq 1 30); do
    sleep 10
    STATUS=$(vastai show instance "$INSTANCE_ID" 2>/dev/null | tail -1 | awk '{print $3}')
    SSH_URL=$(vastai ssh-url "$INSTANCE_ID" 2>/dev/null)
    if [[ "$STATUS" == "running" && -n "$SSH_URL" ]]; then
        SSH_HOST=$(echo "$SSH_URL" | sed 's|ssh://root@||' | cut -d: -f1)
        SSH_PORT=$(echo "$SSH_URL" | sed 's|ssh://root@||' | cut -d: -f2)
        # Try SSH
        if ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 root@"$SSH_HOST" -p "$SSH_PORT" 'echo OK' >/dev/null 2>&1; then
            echo "Instance ready! SSH: $SSH_URL"
            break
        fi
    fi
    echo "  Waiting... ($i/30, status: ${STATUS:-loading})"
done

if [[ -z "${SSH_HOST:-}" ]]; then
    echo "ERROR: Instance did not start within 5 minutes"
    echo "Check: vastai show instance $INSTANCE_ID"
    exit 1
fi

# ============================================================
# Phase 6: Deploy scripts, binaries, credentials, queue
# ============================================================
echo ""
echo "=== Deploying ==="
SSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 root@$SSH_HOST -p $SSH_PORT"
SCP="scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -P $SSH_PORT"

# Scripts
$SCP "$SCRIPT_DIR/enhance.sh" "$SCRIPT_DIR/upscale.py" "$SCRIPT_DIR/multi_gpu_queue.sh" root@"$SSH_HOST":/root/ 2>/dev/null
echo "  Scripts deployed"

# Rust binaries
if [[ -f "$SCRIPT_DIR/status_server_rs/target/release/status_server" ]]; then
    $SCP "$SCRIPT_DIR/status_server_rs/target/release/status_server" root@"$SSH_HOST":/root/ 2>/dev/null
    echo "  status_server binary deployed"
fi
if [[ -f "$SCRIPT_DIR/youtube_upload_rs/target/release/youtube_upload" ]]; then
    $SCP "$SCRIPT_DIR/youtube_upload_rs/target/release/youtube_upload" root@"$SSH_HOST":/root/ 2>/dev/null
    echo "  youtube_upload binary deployed"
fi

# OAuth credentials (copy from existing instance or local)
CRED_SOURCE=""
for src in "/tmp/client_secret.json" "$HOME/client_secret.json"; do
    if [[ -f "$src" ]]; then
        CRED_SOURCE="$src"
        break
    fi
done
if [[ -n "$CRED_SOURCE" ]]; then
    $SCP "$CRED_SOURCE" "$(dirname "$CRED_SOURCE")/youtube_token.json" root@"$SSH_HOST":/root/ 2>/dev/null
    echo "  Credentials deployed from $CRED_SOURCE"
else
    echo "  WARNING: No OAuth credentials found — upload will not work"
fi

# JSON queue — copy only the specified videos
$SSH 'mkdir -p /root/json /root/json_done' 2>/dev/null
for vid in "$@"; do
    $SCP "$JSON_DIR/${vid}.json" root@"$SSH_HOST":/root/json/ 2>/dev/null
done
echo "  Queue deployed: $VIDEO_COUNT JSON files"

# Instance metadata
$SSH "cat > /root/instance_meta.json << EOF
{\"label\": \"davaz-${GPU_NAME,,}-${VIDEO_COUNT}vid\", \"location\": \"$OFFER_LOCATION\", \"cost_per_hr\": $OFFER_PRICE, \"provider\": \"vast.ai\", \"instance_id\": \"$INSTANCE_ID\"}
EOF" 2>/dev/null
echo "  Instance metadata written"

# Make scripts executable
$SSH 'chmod +x /root/enhance.sh /root/multi_gpu_queue.sh /root/status_server /root/youtube_upload 2>/dev/null' 2>/dev/null

# ============================================================
# Phase 7: Start processing
# ============================================================
echo ""
echo "=== Starting processing ==="

if [[ $NUM_GPUS -gt 1 ]]; then
    $SSH 'sudo bash -c "cd /root && nohup ./multi_gpu_queue.sh >> /root/enhance.log 2>&1 &"' 2>/dev/null
    echo "Started multi_gpu_queue.sh on $NUM_GPUS GPUs"
else
    # Single GPU: process videos sequentially
    vid="${VIDEOS[0]}"
    IFS='|' read -r v_id v_w v_h v_dur v_mp v_scale v_gpu v_disk v_title <<< "$vid"
    $SSH "sudo bash -c 'cd /root && nohup ./enhance.sh \"https://www.youtube.com/watch?v=$v_id\" $v_scale --job-name \"$v_title\" >> /root/enhance.log 2>&1 &'" 2>/dev/null
    echo "Started enhance.sh for $v_title"
fi

# ============================================================
# Summary
# ============================================================
echo ""
echo "============================================="
echo "DEPLOYED!"
echo "============================================="
echo "Instance:  $INSTANCE_ID"
echo "SSH:       ssh -p $SSH_PORT root@$SSH_HOST"
echo "Dashboard: http://${SSH_HOST}:$((SSH_PORT + 1))/"
echo "Videos:    $VIDEO_COUNT"
echo "GPUs:      ${NUM_GPUS}x $GPU_LABEL"
echo "Cost:      \$${OFFER_PRICE}/hr"
echo "============================================="
