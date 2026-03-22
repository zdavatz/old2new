#!/bin/bash
# Deploy videos to cloud GPU instances for Real-ESRGAN upscaling
#
# Usage:
#   ./deploy.sh <video_id> [video_id2] ...          # auto: find running instance or propose new
#   ./deploy.sh new <video_id> [video_id2] ...       # search for a new instance
#   ./deploy.sh --instance <ID> <video_id> ...       # add to existing instance
#   ./deploy.sh --plan <video_id> [video_id2] ...    # analyze only, no deploy
#   ./deploy.sh --plan --vastai <video_id> ...       # search vast.ai only
#   ./deploy.sh --plan --tensordock <video_id> ...   # search TensorDock only
#   ./deploy.sh destroy <instance_id>                # destroy an instance
#
# Options:
#   --single    prefer single GPU instance
#   --multi     prefer multi GPU instance
#   --plan      analyze and show recommendations without deploying
#   --vastai    search only vast.ai
#   --tensordock search only TensorDock
#   --instance <ID>  deploy to existing vast.ai instance

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
JSON_DIR="$SCRIPT_DIR/json"

# ============================================================
# Parse global options
# ============================================================
MODE="auto"           # auto | new | plan | instance
PROVIDER="both"       # both | vastai | tensordock
GPU_PREF=""           # "" | single | multi
INSTANCE_ID=""
VIDEO_IDS=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        new)
            MODE="new"; shift ;;
        destroy)
            if [[ -n "${2:-}" ]]; then
                echo "Destroying instance $2..."
                vastai destroy instance "$2" 2>/dev/null
                echo "Instance $2 destroyed."
                exit 0
            else
                echo "Usage: $0 destroy <instance_id>"
                exit 1
            fi
            ;;
        --plan)
            MODE="plan"; shift ;;
        --vastai)
            PROVIDER="vastai"; shift ;;
        --tensordock)
            PROVIDER="tensordock"; shift ;;
        --single)
            GPU_PREF="single"; shift ;;
        --multi)
            GPU_PREF="multi"; shift ;;
        --instance)
            MODE="instance"; INSTANCE_ID="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,/^$/p' "$0" | grep '^#' | sed 's/^# \?//'; exit 0 ;;
        *)
            VIDEO_IDS+=("$1"); shift ;;
    esac
done

if [[ ${#VIDEO_IDS[@]} -eq 0 ]]; then
    echo "Usage: $0 [new|--plan|--instance ID] [--single|--multi] [--vastai|--tensordock] <video_id> ..."
    echo "Run '$0 --help' for details."
    exit 1
fi

# ============================================================
# Phase 1: Analyze videos from JSON files
# ============================================================
echo "=== Analyzing ${#VIDEO_IDS[@]} videos ==="
echo ""

TOTAL_DISK_GB=0
TOTAL_DURATION=0
NEEDS_5090=0
VIDEO_COUNT=${#VIDEO_IDS[@]}
VIDEOS=()
RTX4090_VIDS=()
RTX5090_VIDS=()

for vid in "${VIDEO_IDS[@]}"; do
    json_file="$JSON_DIR/${vid}.json"
    if [[ ! -f "$json_file" ]]; then
        echo "ERROR: No JSON file for $vid — run: ./fetch_video_json.sh $vid"
        exit 1
    fi

    info=$(python3 -c "
import json
d = json.load(open('$json_file'))
video_id = d.get('video_id', '$vid')
w = d.get('width', 0)
h = d.get('height', 0)
dur = d.get('duration_seconds', 0)
fps = d.get('fps', 25)
mp = d.get('megapixels', 0)
scale = d.get('scale', 4)
gpu = d.get('gpu', 'RTX 4090')
title = d.get('title', '$vid')
frames = int(dur * fps)
input_sz = w * h * 3 / 2.5 / 1024 / 1024
output_sz = w * scale * h * scale * 3 / 2.5 / 1024 / 1024
disk_gb = (frames * input_sz + frames * output_sz) / 1024 * 1.2 + 5
print(f'{video_id}|{w}|{h}|{dur}|{mp}|{scale}|{gpu}|{disk_gb:.0f}|{title}')
")

    IFS='|' read -r v_id v_w v_h v_dur v_mp v_scale v_gpu v_disk v_title <<< "$info"
    VIDEOS+=("$info")
    TOTAL_DISK_GB=$((TOTAL_DISK_GB + v_disk))
    TOTAL_DURATION=$((TOTAL_DURATION + v_dur))

    if [[ "$v_gpu" == "RTX 5090" ]]; then
        NEEDS_5090=1
        RTX5090_VIDS+=("$vid")
    else
        RTX4090_VIDS+=("$vid")
    fi

    printf "  %-50s %sx%s  %4ss  %sx  %-9s ~%sGB\n" "$v_title" "$v_w" "$v_h" "$v_dur" "$v_scale" "$v_gpu" "$v_disk"
done

echo ""
TOTAL_HOURS=$(python3 -c "print(f'{$TOTAL_DURATION/3600:.1f}')")
echo "Total: $VIDEO_COUNT videos, ${TOTAL_HOURS}h duration, ~${TOTAL_DISK_GB}GB disk"

# ============================================================
# Phase 2: Determine instance requirements
# ============================================================

# Split into GPU groups if mixed
if [[ ${#RTX4090_VIDS[@]} -gt 0 && ${#RTX5090_VIDS[@]} -gt 0 ]]; then
    echo ""
    echo "=== Mixed GPU requirements ==="
    echo "  RTX 4090: ${#RTX4090_VIDS[@]} videos (${RTX4090_VIDS[*]})"
    echo "  RTX 5090: ${#RTX5090_VIDS[@]} videos (${RTX5090_VIDS[*]})"
    echo ""
    echo "Recommendation: deploy separately"
    echo "  ./deploy.sh ${RTX4090_VIDS[*]}"
    echo "  ./deploy.sh ${RTX5090_VIDS[*]}"
    if [[ "$MODE" == "plan" ]]; then
        exit 0
    fi
    echo ""
    read -p "Deploy all to RTX 5090 (works but slower for SD)? [y/N] " confirm
    if [[ "$confirm" != "y" && "$confirm" != "Y" ]]; then
        exit 0
    fi
    NEEDS_5090=1
fi

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
if [[ "$GPU_PREF" == "single" ]]; then
    NUM_GPUS=1
elif [[ "$GPU_PREF" == "multi" ]]; then
    NUM_GPUS=4
elif [[ $VIDEO_COUNT -le 2 ]]; then
    NUM_GPUS=1
else
    NUM_GPUS=4
fi

# Warn if preference doesn't match
if [[ "$GPU_PREF" == "single" && $VIDEO_COUNT -gt 3 ]]; then
    echo ""
    echo "NOTE: $VIDEO_COUNT videos on single GPU will be slower than multi GPU."
fi
if [[ "$GPU_PREF" == "multi" && $VIDEO_COUNT -le 1 ]]; then
    echo ""
    echo "NOTE: 1 video on multi GPU — extra GPUs will be idle."
fi

# Disk calculation
if [[ $NUM_GPUS -eq 1 ]]; then
    # Largest single video × 2 for safety
    MAX_SINGLE=$(printf '%s\n' "${VIDEOS[@]}" | sort -t'|' -k8 -rn | head -1 | cut -d'|' -f8)
    DISK_GB=$((MAX_SINGLE * 2))
else
    # 4 largest concurrent
    TOP4_DISK=$(printf '%s\n' "${VIDEOS[@]}" | sort -t'|' -k8 -rn | head -4 | awk -F'|' '{sum+=$8} END {print int(sum)}')
    DISK_GB=$((TOP4_DISK + 100))
fi
[[ $DISK_GB -lt 500 ]] && DISK_GB=500

echo ""
echo "=== Recommended Setup ==="
echo "  GPU:  ${NUM_GPUS}x $GPU_LABEL"
echo "  CPU:  >= ${MIN_CPU_GHZ} GHz (ideal 5+ for RTX 5090 HD)"
echo "  RAM:  >= ${MIN_RAM_GB} GB"
echo "  Disk: >= ${DISK_GB} GB"
echo ""

# ============================================================
# Phase 3: Search providers
# ============================================================

search_vastai() {
    echo "=== vast.ai ==="
    local results
    results=$(vastai search offers "num_gpus>=${NUM_GPUS} gpu_name=${GPU_NAME} disk_space>=${DISK_GB} cpu_ghz>=${MIN_CPU_GHZ} verified=true" -o 'dph' 2>/dev/null | head -6)
    if [[ -z "$results" || $(echo "$results" | wc -l) -le 1 ]]; then
        echo "  No matching instances found"
    else
        echo "$results"
    fi
    echo ""
}

search_tensordock() {
    echo "=== TensorDock ==="
    # TensorDock API — may return 404 if API is down
    local gpu_model
    if [[ "$GPU_NAME" == "RTX_5090" ]]; then
        gpu_model="geforcertx5090-pcie-32gb"
    else
        gpu_model="geforcertx4090-pcie-24gb"
    fi
    local result
    result=$(curl -s "https://dashboard.tensordock.com/api/v2/gpu-cloud/deploy-options?gpu_model=$gpu_model&gpu_count=$NUM_GPUS&min_storage=$DISK_GB" \
        -H "Authorization: Bearer ${TENSORDOCK_API_KEY:-}" 2>/dev/null)
    if echo "$result" | python3 -c "import json,sys; d=json.load(sys.stdin); locs=d.get('locations',[]); print(f'  {len(locs)} locations available') if locs else print('  No locations available')" 2>/dev/null; then
        echo "$result" | python3 -c "
import json, sys
d = json.load(sys.stdin)
for loc in d.get('locations', [])[:5]:
    name = loc.get('location', '?')
    price = loc.get('price_per_hour', 0)
    print(f'  {name}: \${price:.2f}/hr')
" 2>/dev/null
    else
        echo "  API unavailable (returned 404)"
    fi
    echo ""
}

search_running_instances() {
    echo "=== Running vast.ai instances ==="
    local instances
    instances=$(vastai show instances --raw 2>/dev/null | python3 -c "
import json, sys
data = json.load(sys.stdin)
for inst in data:
    label = inst.get('label', '?')
    gpu = inst.get('gpu_name', '?')
    num_gpus = inst.get('num_gpus', 1)
    dph = inst.get('dph_total', 0) or 0
    disk_total = inst.get('disk_space', 0) or 0
    disk_used = inst.get('disk_usage', 0) or 0
    disk_free = disk_total - disk_used
    cpu_ghz = inst.get('cpu_ghz', 0) or 0
    ram = inst.get('total_ram', 0) or 0
    iid = inst.get('id', '')
    status = inst.get('actual_status', '?')
    if status != 'running':
        continue
    # Check if this instance can handle the videos
    gpu_ok = '${GPU_NAME}'.replace('_',' ') in gpu or '${GPU_NAME}'.replace('_','') in gpu.replace(' ','')
    cpu_ok = cpu_ghz >= float('${MIN_CPU_GHZ}')
    ram_ok = ram >= ${MIN_RAM_GB}
    disk_ok = disk_free >= ${DISK_GB} * 0.5  # need at least half the required disk free
    fit = 'MATCH' if gpu_ok and cpu_ok and ram_ok and disk_ok else 'no fit'
    reason = []
    if not gpu_ok: reason.append(f'GPU:{gpu}')
    if not cpu_ok: reason.append(f'CPU:{cpu_ghz:.1f}GHz')
    if not ram_ok: reason.append(f'RAM:{ram:.0f}GB')
    if not disk_ok: reason.append(f'Disk:{disk_free:.0f}GB free')
    reason_str = ' (' + ', '.join(reason) + ')' if reason else ''
    print(f'  {iid:>10} {label:<30} {num_gpus}x {gpu:<12} {cpu_ghz:.1f}GHz {ram:.0f}GB RAM {disk_free:.0f}GB free \${dph:.2f}/hr [{fit}{reason_str}]')
" 2>/dev/null)
    if [[ -z "$instances" ]]; then
        echo "  No running instances"
    else
        echo "$instances"
    fi
    echo ""
}

if [[ "$MODE" == "plan" || "$MODE" == "auto" ]]; then
    # Show running instances first
    search_running_instances
fi

if [[ "$PROVIDER" == "both" || "$PROVIDER" == "vastai" ]]; then
    search_vastai
fi
if [[ "$PROVIDER" == "both" || "$PROVIDER" == "tensordock" ]]; then
    search_tensordock
fi

# Plan mode: stop here
if [[ "$MODE" == "plan" ]]; then
    echo "=== Plan mode — no deployment ==="
    echo "To deploy: ./deploy.sh new ${VIDEO_IDS[*]}"
    echo "Or add to existing: ./deploy.sh --instance <ID> ${VIDEO_IDS[*]}"
    exit 0
fi

# ============================================================
# Phase 4: Auto mode — try to find running instance
# ============================================================
if [[ "$MODE" == "auto" ]]; then
    echo "=== Looking for matching running instance ==="
    MATCH_ID=$(vastai show instances --raw 2>/dev/null | python3 -c "
import json, sys
data = json.load(sys.stdin)
for inst in data:
    if inst.get('actual_status') != 'running': continue
    gpu = inst.get('gpu_name', '')
    cpu_ghz = inst.get('cpu_ghz', 0) or 0
    ram = inst.get('total_ram', 0) or 0
    disk_free = (inst.get('disk_space', 0) or 0) - (inst.get('disk_usage', 0) or 0)
    gpu_ok = '${GPU_NAME}'.replace('_',' ') in gpu or '${GPU_NAME}'.replace('_','') in gpu.replace(' ','')
    if gpu_ok and cpu_ghz >= float('${MIN_CPU_GHZ}') and ram >= ${MIN_RAM_GB} and disk_free >= ${DISK_GB} * 0.3:
        print(inst.get('id', ''))
        break
" 2>/dev/null)

    if [[ -n "$MATCH_ID" ]]; then
        echo "Found matching instance: $MATCH_ID"
        MODE="instance"
        INSTANCE_ID="$MATCH_ID"
    else
        echo "No matching running instance found."
        echo ""
        read -p "Search for a new instance? [Y/n] " confirm
        if [[ "$confirm" == "n" || "$confirm" == "N" ]]; then
            exit 0
        fi
        MODE="new"
    fi
fi

# ============================================================
# Phase 5: Deploy to existing instance
# ============================================================
if [[ "$MODE" == "instance" ]]; then
    echo ""
    echo "=== Deploying to instance $INSTANCE_ID ==="

    SSH_URL=$(vastai ssh-url "$INSTANCE_ID" 2>/dev/null)
    SSH_HOST=$(echo "$SSH_URL" | sed 's|ssh://root@||' | cut -d: -f1)
    SSH_PORT=$(echo "$SSH_URL" | sed 's|ssh://root@||' | cut -d: -f2)

    if [[ -z "$SSH_HOST" ]]; then
        echo "ERROR: Could not get SSH URL for instance $INSTANCE_ID"
        exit 1
    fi

    # Validate instance fits the videos
    echo "Validating instance..."
    INSTANCE_CHECK=$(ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 root@"$SSH_HOST" -p "$SSH_PORT" '
        gpu=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1)
        cpu_mhz=$(grep "cpu MHz" /proc/cpuinfo | head -1 | awk -F: "{print \$2}" | xargs)
        ram_gb=$(free -g | grep Mem | awk "{print \$2}")
        disk_free_gb=$(df -BG / | tail -1 | awk "{print \$4}" | tr -d "G")
        echo "$gpu|$cpu_mhz|$ram_gb|$disk_free_gb"
    ' 2>/dev/null)

    IFS='|' read -r inst_gpu inst_cpu inst_ram inst_disk <<< "$INSTANCE_CHECK"
    inst_cpu_ghz=$(python3 -c "print(f'{float(\"${inst_cpu:-0}\") / 1000:.1f}')" 2>/dev/null)

    echo "  GPU: $inst_gpu"
    echo "  CPU: ${inst_cpu_ghz} GHz"
    echo "  RAM: ${inst_ram} GB"
    echo "  Disk free: ${inst_disk} GB"

    # Check fit
    FITS=1
    if [[ "$NEEDS_5090" -eq 1 ]] && ! echo "$inst_gpu" | grep -qi "5090"; then
        echo "  ERROR: Video needs RTX 5090 but instance has $inst_gpu"
        FITS=0
    fi
    if python3 -c "exit(0 if float('${inst_cpu_ghz:-0}') < float('$MIN_CPU_GHZ') else 1)" 2>/dev/null; then
        echo "  WARNING: CPU ${inst_cpu_ghz} GHz < recommended ${MIN_CPU_GHZ} GHz"
    fi
    if [[ "${inst_disk:-0}" -lt "$((DISK_GB / 3))" ]]; then
        echo "  ERROR: Only ${inst_disk}GB free, need at least $((DISK_GB / 3))GB"
        FITS=0
    fi
    if [[ "$FITS" -eq 0 ]]; then
        echo "  Instance does not fit. Aborting."
        exit 1
    fi

    SSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 root@$SSH_HOST -p $SSH_PORT"
    SCP="scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -P $SSH_PORT"

    # Copy JSON files to instance queue
    $SSH 'mkdir -p /root/json /root/json_done' 2>/dev/null
    for vid in "${VIDEO_IDS[@]}"; do
        $SCP "$JSON_DIR/${vid}.json" root@"$SSH_HOST":/root/json/ 2>/dev/null
    done
    echo "  Deployed ${#VIDEO_IDS[@]} JSON files to /root/json/"
    echo ""
    echo "Videos added to queue. They will be picked up by the running worker."
    echo "Dashboard: http://${SSH_HOST}:$((SSH_PORT + 1))/"
    exit 0
fi

# ============================================================
# Phase 6: Create new instance (MODE=new)
# ============================================================
echo ""

# Get vast.ai offers
SEARCH_RESULTS=$(vastai search offers "num_gpus>=${NUM_GPUS} gpu_name=${GPU_NAME} disk_space>=${DISK_GB} cpu_ghz>=${MIN_CPU_GHZ} verified=true" -o 'dph' 2>/dev/null | head -8)

if [[ -z "$SEARCH_RESULTS" || $(echo "$SEARCH_RESULTS" | wc -l) -le 1 ]]; then
    echo "No matching instances found on vast.ai!"
    exit 1
fi

# Show numbered list of offers
echo "=== Available instances (need ${DISK_GB} GB disk) ==="
echo "$SEARCH_RESULTS" | awk -v need="$DISK_GB" 'NR==1 {next} {
    id=$1; gpu=$4; pcie=$5; cpu=$6; vcpu=$7; ram=$8; disk=$9; price=$10; net_up=$16; net_down=$17; loc=$NF
    printf "[%d] %-20s %sx %s  CPU: %s GHz  RAM: %s GB  Disk: %s GB (need %s GB)  Net: %s/%s Mbps  $%s/hr  (%s)\n", NR-1, loc, $3, gpu, cpu, ram, disk, need, net_up, net_down, price, id
}'
echo ""

NUM_OFFERS=$(echo "$SEARCH_RESULTS" | wc -l)
NUM_OFFERS=$((NUM_OFFERS - 1))  # minus header

read -p "Select instance [1-${NUM_OFFERS}], or 'n' to abort: " choice
if [[ "$choice" == "n" || "$choice" == "N" || -z "$choice" ]]; then
    echo "Aborted."
    exit 0
fi

# Validate choice
if ! [[ "$choice" =~ ^[0-9]+$ ]] || [[ "$choice" -lt 1 || "$choice" -gt "$NUM_OFFERS" ]]; then
    echo "Invalid choice: $choice"
    exit 1
fi

SELECTED_ROW=$((choice + 1))  # +1 for header
OFFER_ID=$(echo "$SEARCH_RESULTS" | awk "NR==$SELECTED_ROW {print \$1}")
OFFER_PRICE=$(echo "$SEARCH_RESULTS" | awk "NR==$SELECTED_ROW {print \$10}")
OFFER_LOCATION=$(echo "$SEARCH_RESULTS" | awk "NR==$SELECTED_ROW {print \$NF}")

echo ""
echo "Selected: ID=$OFFER_ID, \$${OFFER_PRICE}/hr, $OFFER_LOCATION"
read -p "Create instance and deploy $VIDEO_COUNT videos? [y/N] " confirm
if [[ "$confirm" != "y" && "$confirm" != "Y" ]]; then
    echo "Aborted."
    exit 0
fi

# Cleanup on abort: destroy the instance if it was created
INSTANCE_ID=""
cleanup_on_abort() {
    echo ""
    echo "Aborted!"
    if [[ -n "$INSTANCE_ID" ]]; then
        echo "Destroying instance $INSTANCE_ID..."
        vastai destroy instance "$INSTANCE_ID" 2>/dev/null
        echo "Instance destroyed."
    fi
    exit 1
}
trap cleanup_on_abort INT TERM

echo ""
echo "=== Creating instance ==="
CREATE_RESULT=$(vastai create instance "$OFFER_ID" \
    --image ghcr.io/zdavatz/realesrgan-benchmark:latest \
    --disk "$DISK_GB" \
    --label "davaz-${GPU_NAME,,}-${VIDEO_COUNT}vid" \
    --ssh --direct 2>&1)

echo "$CREATE_RESULT"
# Parse instance ID — vastai outputs "Started. {'new_contract': 12345, ...}" (Python dict, not JSON)
INSTANCE_ID=$(echo "$CREATE_RESULT" | grep -oP "new_contract['\"]?:\s*(\K[0-9]+)" 2>/dev/null)

if [[ -z "$INSTANCE_ID" ]]; then
    echo "ERROR: Failed to create instance"
    echo "Output was: $CREATE_RESULT"
    exit 1
fi

echo "Instance ID: $INSTANCE_ID"
echo ""
echo "=== Waiting for instance to start ==="
SSH_HOST=""
SSH_PORT=""
for i in $(seq 1 30); do
    sleep 10
    STATUS=$(vastai show instance "$INSTANCE_ID" 2>/dev/null | tail -1 | awk '{print $3}')
    STATUS_MSG=$(vastai show instance "$INSTANCE_ID" --raw 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin).get('status_msg',''))" 2>/dev/null)
    SSH_URL=$(vastai ssh-url "$INSTANCE_ID" 2>/dev/null)
    if [[ "$STATUS" == "running" && -n "$SSH_URL" ]]; then
        SSH_HOST=$(echo "$SSH_URL" | sed 's|ssh://root@||' | cut -d: -f1)
        SSH_PORT=$(echo "$SSH_URL" | sed 's|ssh://root@||' | cut -d: -f2)
        if ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 root@"$SSH_HOST" -p "$SSH_PORT" 'echo OK' >/dev/null 2>&1; then
            echo "  Instance ready! SSH: $SSH_URL"
            break
        fi
    fi
    # Get network speed on first check
    if [[ $i -eq 1 ]]; then
        DL_SPEED=$(vastai show instance "$INSTANCE_ID" --raw 2>/dev/null | python3 -c "import json,sys; d=json.load(sys.stdin); print(f'{d.get(\"inet_down\",0):.0f}')" 2>/dev/null)
        echo "  Network: ${DL_SPEED:-?} Mbps download (image ~4.5 GB)"
    fi
    echo "  [$i/30] ${STATUS:-loading}: ${STATUS_MSG:-waiting...}"
done

if [[ -z "$SSH_HOST" ]]; then
    echo ""
    echo "ERROR: Instance $INSTANCE_ID did not start within 5 minutes"
    read -p "Destroy instance? [Y/n] " destroy_confirm
    if [[ "$destroy_confirm" != "n" && "$destroy_confirm" != "N" ]]; then
        vastai destroy instance "$INSTANCE_ID" 2>/dev/null
        echo "Instance $INSTANCE_ID destroyed."
        INSTANCE_ID=""  # prevent trap from destroying again
    else
        echo "Instance kept running: vastai show instance $INSTANCE_ID"
    fi
    exit 1
fi

# ============================================================
# Phase 7: Deploy everything
# ============================================================
echo ""
echo "=== Deploying ==="
SSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 root@$SSH_HOST -p $SSH_PORT"
SCP="scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -P $SSH_PORT"

# Scripts
$SCP "$SCRIPT_DIR/enhance.sh" "$SCRIPT_DIR/upscale.py" "$SCRIPT_DIR/multi_gpu_queue.sh" root@"$SSH_HOST":/root/ 2>/dev/null
echo "  Scripts deployed"

# Rust binaries
for bin in status_server_rs/target/release/status_server youtube_upload_rs/target/release/youtube_upload; do
    if [[ -f "$SCRIPT_DIR/$bin" ]]; then
        $SCP "$SCRIPT_DIR/$bin" root@"$SSH_HOST":/root/ 2>/dev/null
        echo "  $(basename $bin) deployed"
    fi
done

# OAuth credentials
for src in "/tmp/client_secret.json" "$HOME/client_secret.json"; do
    if [[ -f "$src" && -f "$(dirname "$src")/youtube_token.json" ]]; then
        $SCP "$src" "$(dirname "$src")/youtube_token.json" root@"$SSH_HOST":/root/ 2>/dev/null
        echo "  Credentials deployed"
        break
    fi
done

# JSON queue
$SSH 'mkdir -p /root/json /root/json_done' 2>/dev/null
for vid in "${VIDEO_IDS[@]}"; do
    $SCP "$JSON_DIR/${vid}.json" root@"$SSH_HOST":/root/json/ 2>/dev/null
done
echo "  Queue: $VIDEO_COUNT JSON files"

# Instance metadata
$SSH "cat > /root/instance_meta.json << EOF
{\"label\": \"davaz-${GPU_NAME,,}-${VIDEO_COUNT}vid\", \"location\": \"$OFFER_LOCATION\", \"cost_per_hr\": $OFFER_PRICE, \"provider\": \"vast.ai\", \"instance_id\": \"$INSTANCE_ID\"}
EOF" 2>/dev/null

$SSH 'chmod +x /root/enhance.sh /root/multi_gpu_queue.sh /root/status_server /root/youtube_upload 2>/dev/null' 2>/dev/null

# ============================================================
# Phase 8: Start processing
# ============================================================
# Get the vast.ai proxy host for dashboard URL (port+1 only works via proxy)
PROXY_HOST=$(vastai show instance "$INSTANCE_ID" 2>/dev/null | tail -1 | awk '{print $10}')
PROXY_PORT=$(vastai show instance "$INSTANCE_ID" 2>/dev/null | tail -1 | awk '{print $11}')
DASHBOARD_PORT=$((PROXY_PORT + 1))
if [[ "$PROXY_HOST" == ssh*.vast.ai ]]; then
    DASHBOARD_URL="http://${PROXY_HOST}:${DASHBOARD_PORT}/"
else
    DASHBOARD_URL="http://${SSH_HOST}:$((SSH_PORT + 1))/ (may need SSH tunnel)"
fi

echo ""
echo "============================================="
echo "DEPLOYED!"
echo "============================================="
echo "Instance:  $INSTANCE_ID"
echo "SSH:       ssh -p $SSH_PORT root@$SSH_HOST"
echo "Dashboard: $DASHBOARD_URL"
echo "Videos:    $VIDEO_COUNT"
echo "GPUs:      ${NUM_GPUS}x $GPU_LABEL"
echo "Cost:      \$${OFFER_PRICE}/hr"
echo "============================================="
echo ""

echo "=== Starting processing ==="
if [[ $NUM_GPUS -gt 1 ]]; then
    # Use ssh -f to fork into background and not block
    ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -f root@"$SSH_HOST" -p "$SSH_PORT" \
        'sudo bash -c "cd /root && nohup ./multi_gpu_queue.sh >> /root/enhance.log 2>&1 &"' 2>/dev/null
    echo "Started multi_gpu_queue.sh on $NUM_GPUS GPUs"
else
    vid="${VIDEOS[0]}"
    IFS='|' read -r v_id v_w v_h v_dur v_mp v_scale v_gpu v_disk v_title <<< "$vid"
    ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -f root@"$SSH_HOST" -p "$SSH_PORT" \
        "sudo bash -c 'cd /root && nohup ./enhance.sh \"https://www.youtube.com/watch?v=$v_id\" $v_scale --job-name \"$v_title\" >> /root/enhance.log 2>&1 &'" 2>/dev/null
    echo "Started enhance.sh for $v_title"
fi
echo "Done."
