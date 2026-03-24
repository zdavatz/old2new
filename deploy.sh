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
#   ./deploy.sh update <instance_id>                 # redeploy scripts + binaries, restart queue
#   ./deploy.sh update-server <instance_id>          # replace status_server + youtube_upload only (no queue restart)
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
        update-server)
            if [[ -z "${2:-}" ]]; then
                echo "Usage: $0 update-server <instance_id>"
                exit 1
            fi
            UPD_ID="$2"
            echo "=== Updating server binaries on $UPD_ID (no queue restart) ==="
            UPD_URL=$(vastai ssh-url "$UPD_ID" 2>/dev/null)
            UPD_HOST=$(echo "$UPD_URL" | sed 's|ssh://root@||' | cut -d: -f1)
            UPD_PORT=$(echo "$UPD_URL" | sed 's|ssh://root@||' | cut -d: -f2)
            if [[ -z "$UPD_HOST" ]]; then
                echo "ERROR: Could not get SSH URL for instance $UPD_ID"
                exit 1
            fi
            UPD_SCP="scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -P $UPD_PORT"
            UPD_SSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 root@$UPD_HOST -p $UPD_PORT"

            # Upload binaries
            for bin in status_server_rs/target/release/status_server youtube_upload_rs/target/release/youtube_upload; do
                if [[ -f "$SCRIPT_DIR/$bin" ]]; then
                    bname=$(basename "$bin")
                    $UPD_SCP "$SCRIPT_DIR/$bin" root@"$UPD_HOST":/root/${bname}.new 2>/dev/null
                    echo "  $bname uploaded"
                fi
            done

            # Also upload scripts (no harm, they're only used on next queue start)
            $UPD_SCP "$SCRIPT_DIR/enhance.sh" "$SCRIPT_DIR/upscale.py" "$SCRIPT_DIR/multi_gpu_queue.sh" root@"$UPD_HOST":/root/ 2>/dev/null
            echo "  Scripts uploaded"

            # Replace binaries and restart only status_server
            $UPD_SSH 'kill $(pgrep -x status_server) 2>/dev/null
sleep 1
mv -f /root/status_server.new /root/status_server 2>/dev/null && echo "status_server replaced" || echo "no status_server.new"
mv -f /root/youtube_upload.new /root/youtube_upload 2>/dev/null && echo "youtube_upload replaced" || echo "no youtube_upload.new"
chmod +x /root/status_server /root/youtube_upload /root/enhance.sh /root/multi_gpu_queue.sh /root/upscale.py 2>/dev/null
cd /root && nohup ./status_server >> /root/status_server.log 2>&1 &
sleep 1
if pgrep -x status_server > /dev/null; then echo "status_server restarted"; else echo "ERROR: status_server failed to start"; fi' 2>/dev/null

            echo ""
            echo "Instance $UPD_ID: server binaries updated (queue still running)."
            echo "SSH: ssh -p $UPD_PORT root@$UPD_HOST"
            exit 0
            ;;
        update)
            if [[ -z "${2:-}" ]]; then
                echo "Usage: $0 update <instance_id>"
                exit 1
            fi
            UPD_ID="$2"
            echo "=== Updating instance $UPD_ID ==="
            UPD_URL=$(vastai ssh-url "$UPD_ID" 2>/dev/null)
            UPD_HOST=$(echo "$UPD_URL" | sed 's|ssh://root@||' | cut -d: -f1)
            UPD_PORT=$(echo "$UPD_URL" | sed 's|ssh://root@||' | cut -d: -f2)
            if [[ -z "$UPD_HOST" ]]; then
                echo "ERROR: Could not get SSH URL for instance $UPD_ID"
                exit 1
            fi
            UPD_SCP="scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -P $UPD_PORT"
            UPD_SSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 root@$UPD_HOST -p $UPD_PORT"

            # Step 1: Upload everything first (while old processes still run)
            $UPD_SCP "$SCRIPT_DIR/enhance.sh" "$SCRIPT_DIR/upscale.py" "$SCRIPT_DIR/multi_gpu_queue.sh" "$SCRIPT_DIR/restart.sh" root@"$UPD_HOST":/root/ 2>/dev/null
            echo "  Scripts uploaded"
            for bin in status_server_rs/target/release/status_server youtube_upload_rs/target/release/youtube_upload; do
                if [[ -f "$SCRIPT_DIR/$bin" ]]; then
                    bname=$(basename "$bin")
                    $UPD_SCP "$SCRIPT_DIR/$bin" root@"$UPD_HOST":/root/${bname}.new
                    echo "  $bname uploaded"
                fi
            done

            echo ""
            echo "  Restarting..."
            $UPD_SSH './restart.sh'
            exit 0
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
# Peak disk = max of extraction (all input) or upscaling (all output + 10 rolling input)
all_in = frames * input_sz / 1024
all_out = frames * output_sz / 1024
disk_gb = max(all_in, all_out + 10 * input_sz / 1024) * 1.1 + 5
# Estimated upscaling time based on resolution → fps benchmarks
mp_val = w * h / 1e6
if gpu == 'RTX 5090':
    if mp_val > 1.6: est_fps = 0.5   # HD tiled
    elif mp_val > 0.6: est_fps = 1.5  # HD no-tile
    else: est_fps = 3.0               # SD
else:  # RTX 4090
    if mp_val > 1.6: est_fps = 0.3   # tiled (slow)
    else: est_fps = 2.5              # SD no-tile
est_hours = frames / est_fps / 3600 if est_fps > 0 else 0
print(f'{video_id}|{w}|{h}|{dur}|{mp}|{scale}|{gpu}|{disk_gb:.0f}|{title}|{est_hours:.1f}')
")

    IFS='|' read -r v_id v_w v_h v_dur v_mp v_scale v_gpu v_disk v_title v_hours <<< "$info"
    VIDEOS+=("$info")
    TOTAL_DISK_GB=$((TOTAL_DISK_GB + v_disk))
    TOTAL_DURATION=$((TOTAL_DURATION + v_dur))

    if [[ "$v_gpu" == "RTX 5090" ]]; then
        NEEDS_5090=1
        RTX5090_VIDS+=("$vid")
    else
        RTX4090_VIDS+=("$vid")
    fi

    printf "  %-45s %sx%s  %4ss  %sx  %-9s ~%sGB  ~%sh\n" "$v_title" "$v_w" "$v_h" "$v_dur" "$v_scale" "$v_gpu" "$v_disk" "$v_hours"
done

echo ""
TOTAL_HOURS=$(python3 -c "print(f'{$TOTAL_DURATION/3600:.1f}')")
echo "Total: $VIDEO_COUNT videos, ${TOTAL_HOURS}h duration, ~${TOTAL_DISK_GB}GB disk"

# GPU balance warning: check if videos have very different runtimes
if [[ $VIDEO_COUNT -gt 1 ]]; then
    BALANCE=$(printf '%s\n' "${VIDEOS[@]}" | awk -F'|' '{print $10}' | python3 -c "
import sys
times = [float(l.strip()) for l in sys.stdin if l.strip()]
if times:
    mn, mx = min(times), max(times)
    if mx > 0 and mn > 0 and mx / mn > 2:
        print(f'WARNING: GPU imbalance! Shortest: {mn:.1f}h, longest: {mx:.1f}h ({mx/mn:.1f}x difference)')
        print(f'  {int((mx - mn) * 60)}min of idle GPU time per finished video. Consider grouping similar-length videos.')
    else:
        print(f'GPU balance OK: {mn:.1f}h - {mx:.1f}h ({mx/mn:.1f}x)')
")
    if [[ -n "$BALANCE" ]]; then
        echo "$BALANCE"
    fi
fi

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
# NOTE: vast.ai cpu_ghz is BOOST CLOCK, not actual running speed.
# Xeon 8481C listed as 3.8 GHz boost but runs at 2.7 GHz actual.
# We search for higher boost values to ensure adequate actual speed.
# Post-deploy SSH check verifies actual MHz from /proc/cpuinfo.
if [[ "$NEEDS_5090" -eq 1 ]]; then
    GPU_NAME="RTX_5090"
    GPU_LABEL="RTX 5090"
    MIN_CPU_GHZ="3.0"          # actual minimum (checked post-deploy via SSH)
    SEARCH_CPU_GHZ="4.0"      # search filter (boost clock, ~30% higher than actual)
    MIN_RAM_GB=128
    MIN_VCPUS=32               # need enough cores for cv2 I/O with large PNGs
else
    GPU_NAME="RTX_4090"
    GPU_LABEL="RTX 4090"
    MIN_CPU_GHZ="2.0"
    SEARCH_CPU_GHZ="2.5"
    MIN_RAM_GB=32
    MIN_VCPUS=16
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

# Disk calculation — concurrent videos each need their own disk budget
# Largest video determines minimum (even on multi-GPU, one video can fill the disk)
MAX_SINGLE=$(printf '%s\n' "${VIDEOS[@]}" | sort -t'|' -k8 -rn | head -1 | cut -d'|' -f8)
if [[ $NUM_GPUS -eq 1 ]]; then
    DISK_GB=$((MAX_SINGLE + 100))  # single video + headroom
else
    # N largest concurrent (N = min of GPUs, videos)
    CONCURRENT=$(( NUM_GPUS < VIDEO_COUNT ? NUM_GPUS : VIDEO_COUNT ))
    TOP_N_DISK=$(printf '%s\n' "${VIDEOS[@]}" | sort -t'|' -k8 -rn | head -"$CONCURRENT" | awk -F'|' '{sum+=$8} END {print int(sum)}')
    DISK_GB=$((TOP_N_DISK + 100))
fi
[[ $DISK_GB -lt 500 ]] && DISK_GB=500
# Show warning if largest video needs >60% of total disk
if [[ $((MAX_SINGLE * 100 / DISK_GB)) -gt 60 ]]; then
    echo "  NOTE: Largest video needs ~${MAX_SINGLE}GB — may not fit on shared instances"
fi

echo ""
echo "=== Recommended Setup ==="
echo "  GPU:  ${NUM_GPUS}x $GPU_LABEL"
echo "  CPU:  >= ${MIN_CPU_GHZ} GHz actual (search: >= ${SEARCH_CPU_GHZ} GHz boost)"
echo "  vCPUs: >= ${MIN_VCPUS}"
echo "  RAM:  >= ${MIN_RAM_GB} GB"
echo "  Disk: >= ${DISK_GB} GB"
echo ""

# Load blacklisted machine IDs (slow CPUs etc)
BLACKLIST_FILE="$SCRIPT_DIR/blacklist_machines.txt"
BLACKLISTED=""
if [[ -f "$BLACKLIST_FILE" ]]; then
    BLACKLISTED=$(grep -v '^\s*#' "$BLACKLIST_FILE" | awk '{print $1}' | tr '\n' ',' | sed 's/,$//')
    if [[ -n "$BLACKLISTED" ]]; then
        echo "  Blacklisted machines: $BLACKLISTED"
    fi
fi

# Load cached machines (Docker image + benchmark scores)
CACHE_FILE="$SCRIPT_DIR/cached_machines.txt"
CACHED=""
CACHED_BENCH=""  # machine_id:score pairs
if [[ -f "$CACHE_FILE" ]]; then
    CACHED=$(grep -v '^\s*#' "$CACHE_FILE" | awk '{print $1}' | tr '\n' ',' | sed 's/,$//')
    CACHED_BENCH=$(grep -v '^\s*#' "$CACHE_FILE" | awk '{print $1":"$2}' | tr '\n' ',' | sed 's/,$//')
fi

# ============================================================
# Phase 3: Search providers
# ============================================================

search_vastai() {
    echo "=== vast.ai ==="
    local raw
    raw=$(vastai search offers "num_gpus>=${NUM_GPUS} gpu_name=${GPU_NAME} disk_space>=${DISK_GB} cpu_ghz>=${SEARCH_CPU_GHZ} cpu_cores>=${MIN_VCPUS} verified=true" -o 'dph' --raw 2>/dev/null)
    local formatted
    formatted=$(echo "$raw" | BLACKLISTED="$BLACKLISTED" CACHED="$CACHED" CACHED_BENCH="$CACHED_BENCH" python3 -c "
import json, sys, os
blacklisted = set(int(x) for x in os.environ.get('BLACKLISTED','').split(',') if x.strip())
cached = set(int(x) for x in os.environ.get('CACHED','').split(',') if x.strip())
bench_scores = {}
for pair in os.environ.get('CACHED_BENCH','').split(','):
    if ':' in pair:
        mid, score = pair.split(':', 1)
        if mid.strip() and score.strip():
            try: bench_scores[int(mid)] = int(score)
            except: pass
CPU_YEAR = {
    '14900': 2023, '13900': 2022, '12900': 2021, '12700': 2021,
    '9950X': 2024, '9900X': 2024, '7950X': 2022, '7900X': 2022, '7900 ': 2022, '5950X': 2020, '5900X': 2020,
    '7960X': 2023, '7970X': 2023, '7975WX': 2023, '7980X': 2023, '5975WX': 2022,
    'EPYC 97': 2024, 'EPYC 96': 2024, 'EPYC 93': 2022, 'EPYC 77': 2020,
    '8592': 2024, '8490': 2023, '8481': 2023, '8380': 2021, '6530': 2024, '6430': 2023,
    '285K': 2024, 'Ultra 9': 2024, 'Ultra 7': 2024, 'Ultra 5': 2024,
}
def cpu_year(name):
    for key, yr in CPU_YEAR.items():
        if key in name: return str(yr)
    return '  ?'
data = [d for d in json.load(sys.stdin) if d.get('machine_id') not in blacklisted][:5]
if not data:
    sys.exit(1)
hdr = f\"{'Location':<18s} {'ID':<11s} {'GPU':<12s} {'CPU':<26s} {'Yr':>4s} {'GHz':>4s} {'vCPU':>5s} {'Disk':>6s} {'IO MB/s':>8s} {'Net D/U':>10s} {'$/hr':>7s} {'Bench':>6s}\"
print(hdr)
print('-' * len(hdr))
for d in data:
    loc = d.get('geolocation', '?')
    if len(loc) > 17: loc = loc[:17]
    num = d.get('num_gpus', 1)
    gpu = d.get('gpu_name', '?')
    cpu_ghz = d.get('cpu_ghz', 0) or 0
    cpu_name = d.get('cpu_name', '?')
    yr = cpu_year(cpu_name)
    for rm in ['Intel(R) ', 'AMD ', '(R)', '(TM)', ' Processor', '-Core']:
        cpu_name = cpu_name.replace(rm, '')
    if len(cpu_name) > 25: cpu_name = cpu_name[:25]
    vcpu = int(d.get('cpu_cores_effective', 0) or 0)
    disk = int(d.get('disk_space', 0) or 0)
    disk_bw = int(d.get('disk_bw', 0) or 0)
    price = d.get('dph_total', 0) or 0
    oid = str(d.get('id', '?'))
    mid = d.get('machine_id', 0)
    inet_down = int(d.get('inet_down', 0) or 0)
    inet_up = int(d.get('inet_up', 0) or 0)
    slow = '*' if disk_bw > 0 and disk_bw < 1000 else ''
    bs = bench_scores.get(mid, 0)
    bench = f'{bs/1000000:.1f}M' if bs > 0 else ('  img' if mid in cached else '    -')
    print(f'{loc:<18s} {oid:<11s} {num}x {gpu:<9s} {cpu_name:<26s} {yr:>4s} {cpu_ghz:>4.1f} {vcpu:>5d} {disk:>5d}G {disk_bw:>7d}{slow} {inet_down:>5d}/{inet_up:<4d} {price:>7.4f} {bench:>6s}')
" 2>/dev/null)
    if [[ -z "$formatted" ]]; then
        echo "  No matching instances found"
    else
        echo "$formatted"
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
    vcpus = inst.get('cpu_cores_effective', 0) or inst.get('vcpus', 0) or 0
    ram = inst.get('total_ram', 0) or 0
    iid = inst.get('id', '')
    status = inst.get('actual_status', '?')
    if status != 'running':
        continue
    # Check if this instance can handle the videos
    # NOTE: cpu_ghz from vast.ai is BOOST clock, not actual — use SEARCH_CPU_GHZ threshold
    gpu_ok = '${GPU_NAME}'.replace('_',' ') in gpu or '${GPU_NAME}'.replace('_','') in gpu.replace(' ','')
    cpu_ok = cpu_ghz >= float('${SEARCH_CPU_GHZ}')
    vcpu_ok = vcpus >= ${MIN_VCPUS}
    ram_ok = ram >= ${MIN_RAM_GB}
    disk_ok = disk_free >= ${DISK_GB} * 0.5  # need at least half the required disk free
    fit = 'MATCH' if gpu_ok and cpu_ok and vcpu_ok and ram_ok and disk_ok else 'no fit'
    reason = []
    if not gpu_ok: reason.append(f'GPU:{gpu}')
    if not cpu_ok: reason.append(f'CPU:{cpu_ghz:.1f}GHz boost<${SEARCH_CPU_GHZ}')
    if not vcpu_ok: reason.append(f'vCPUs:{vcpus:.0f}<${MIN_VCPUS}')
    if not ram_ok: reason.append(f'RAM:{ram:.0f}GB')
    if not disk_ok: reason.append(f'Disk:{disk_free:.0f}GB free')
    reason_str = ' (' + ', '.join(reason) + ')' if reason else ''
    print(f'  {iid:>10} {label:<30} {num_gpus}x {gpu:<12} {cpu_ghz:.1f}GHz {int(vcpus)}vCPU {ram:.0f}GB RAM {disk_free:.0f}GB free \${dph:.2f}/hr [{fit}{reason_str}]')
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
    vcpus = inst.get('cpu_cores_effective', 0) or inst.get('vcpus', 0) or 0
    ram = inst.get('total_ram', 0) or 0
    disk_free = (inst.get('disk_space', 0) or 0) - (inst.get('disk_usage', 0) or 0)
    gpu_ok = '${GPU_NAME}'.replace('_',' ') in gpu or '${GPU_NAME}'.replace('_','') in gpu.replace(' ','')
    if gpu_ok and cpu_ghz >= float('${SEARCH_CPU_GHZ}') and vcpus >= ${MIN_VCPUS} and ram >= ${MIN_RAM_GB} and disk_free >= ${DISK_GB} * 0.3:
        print(inst.get('id', ''))
        break
" 2>/dev/null)

    if [[ -n "$MATCH_ID" ]]; then
        echo "Found matching instance: $MATCH_ID"
        MODE="instance"
        INSTANCE_ID="$MATCH_ID"
    else
        echo "No matching running instance found — searching for new instance..."
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

# Get vast.ai offers (use --raw for cpu_name access)
SEARCH_RAW=$(vastai search offers "num_gpus>=${NUM_GPUS} gpu_name=${GPU_NAME} disk_space>=${DISK_GB} cpu_ghz>=${SEARCH_CPU_GHZ} cpu_cores>=${MIN_VCPUS} verified=true" -o 'dph' --raw 2>/dev/null)

OFFER_LIST=$(echo "$SEARCH_RAW" | BLACKLISTED="$BLACKLISTED" CACHED="$CACHED" CACHED_BENCH="$CACHED_BENCH" python3 -c "
import json, sys, os, re
blacklisted = set(int(x) for x in os.environ.get('BLACKLISTED','').split(',') if x.strip())
cached = set(int(x) for x in os.environ.get('CACHED','').split(',') if x.strip())
bench_scores = {}
for pair in os.environ.get('CACHED_BENCH','').split(','):
    if ':' in pair:
        mid, score = pair.split(':', 1)
        if mid.strip() and score.strip():
            try: bench_scores[int(mid)] = int(score)
            except: pass
CPU_YEAR = {
    '14900': 2023, '13900': 2022, '12900': 2021, '12700': 2021, '11900': 2021,
    '9950X': 2024, '9900X': 2024, '7950X': 2022, '7900X': 2022, '7900 ': 2022, '5950X': 2020, '5900X': 2020, '3950X': 2019,
    '7960X': 2023, '7970X': 2023, '7975WX': 2023, '7980X': 2023, '5975WX': 2022, '3970X': 2019, '3990X': 2020,
    'EPYC 97': 2024, 'EPYC 96': 2024, 'EPYC 95': 2024, 'EPYC 93': 2022, 'EPYC 77': 2020, 'EPYC 75': 2019,
    '8592': 2024, '8490': 2023, '8481': 2023, '8380': 2021, '8375': 2021, '8358': 2021, '8272': 2019,
    '6530': 2024, '6448': 2024, '6430': 2023, '6348': 2021, '6258': 2019,
    'W9-3595': 2024, 'W9-3545': 2024, 'W7-3465': 2023, 'W7-3455': 2023,
    '285K': 2024, 'Ultra 9': 2024, 'Ultra 7': 2024, 'Ultra 5': 2024,
}
def cpu_year(name):
    for key, yr in CPU_YEAR.items():
        if key in name: return str(yr)
    return '  ?'
data = [d for d in json.load(sys.stdin) if d.get('machine_id') not in blacklisted][:7]
if not data:
    sys.exit(1)
hdr = f\"{'#':>3s} {'Location':<18s} {'GPU':<12s} {'CPU':<26s} {'Yr':>4s} {'GHz':>4s} {'vCPU':>5s} {'Disk':>6s} {'IO MB/s':>8s} {'Net D/U':>10s} {'$/hr':>7s} {'ID':>11s} {'Bench':>6s}\"
print(hdr)
print('-' * len(hdr))
for i, d in enumerate(data):
    loc = d.get('geolocation', '?')
    if len(loc) > 17: loc = loc[:17]
    num = d.get('num_gpus', 1)
    gpu = d.get('gpu_name', '?')
    cpu_ghz = d.get('cpu_ghz', 0) or 0
    cpu_name = d.get('cpu_name', '?')
    yr = cpu_year(cpu_name)
    for rm in ['Intel(R) ', 'AMD ', '(R)', '(TM)', ' Processor', '-Core']:
        cpu_name = cpu_name.replace(rm, '')
    cpu_name = cpu_name.strip()
    if len(cpu_name) > 25: cpu_name = cpu_name[:25]
    vcpu = int(d.get('cpu_cores_effective', 0) or 0)
    disk = int(d.get('disk_space', 0) or 0)
    disk_bw = int(d.get('disk_bw', 0) or 0)
    inet_down = int(d.get('inet_down', 0) or 0)
    inet_up = int(d.get('inet_up', 0) or 0)
    price = d.get('dph_total', 0) or 0
    oid = str(d.get('id', '?'))
    mid = d.get('machine_id', 0)
    slow = '*' if disk_bw > 0 and disk_bw < 1000 else ''
    bs = bench_scores.get(mid, 0)
    bench = f'{bs/1000000:.1f}M' if bs > 0 else ('  img' if mid in cached else '    -')
    print(f'[{i+1:>1}] {loc:<18s} {num}x {gpu:<9s} {cpu_name:<26s} {yr:>4s} {cpu_ghz:>4.1f} {vcpu:>5d} {disk:>5d}G {disk_bw:>7d}{slow} {inet_down:>5d}/{inet_up:<4d} {price:>7.4f} {oid:>11s} {bench:>6s}')
" 2>/dev/null)

if [[ -z "$OFFER_LIST" ]]; then
    echo "No matching instances found on vast.ai!"
    exit 1
fi

# Show numbered list of offers
echo "=== Available instances (need ${DISK_GB} GB disk) ==="
echo "$OFFER_LIST"
echo ""

NUM_OFFERS=$(echo "$OFFER_LIST" | wc -l)

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

# Extract offer details from raw JSON by index
SELECTED_IDX=$((choice - 1))
read -r OFFER_ID OFFER_PRICE OFFER_LOCATION <<< "$(echo "$SEARCH_RAW" | BLACKLISTED="$BLACKLISTED" python3 -c "
import json, sys, os
blacklisted = set(int(x) for x in os.environ.get('BLACKLISTED','').split(',') if x.strip())
data = [d for d in json.load(sys.stdin) if d.get('machine_id') not in blacklisted][:7]
d = data[$SELECTED_IDX]
print(d.get('id',''), f\"{d.get('dph_total',0):.4f}\", d.get('geolocation','?'))
" 2>/dev/null)"

echo ""
echo "Selected: ID=$OFFER_ID, \$${OFFER_PRICE}/hr, $OFFER_LOCATION"
read -p "Create instance and deploy $VIDEO_COUNT videos? [y/N] " confirm
if [[ "$confirm" != "y" && "$confirm" != "Y" ]]; then
    echo "Aborted."
    exit 0
fi

# Get machine_id from the selected offer (needed for blacklist before instance is destroyed)
SELECTED_MACHINE_ID=$(echo "$SEARCH_RAW" | BLACKLISTED="$BLACKLISTED" python3 -c "
import json, sys, os
blacklisted = set(int(x) for x in os.environ.get('BLACKLISTED','').split(',') if x.strip())
data = [d for d in json.load(sys.stdin) if d.get('machine_id') not in blacklisted][:7]
print(data[$SELECTED_IDX].get('machine_id',''))
" 2>/dev/null)

# Cleanup on abort: blacklist + destroy the instance if it was created
INSTANCE_ID=""
cleanup_on_abort() {
    echo ""
    echo "Aborted!"
    if [[ -n "$INSTANCE_ID" ]]; then
        # Blacklist machine if instance never became usable (SSH never connected)
        if [[ -z "$SSH_HOST" && -n "$SELECTED_MACHINE_ID" ]]; then
            echo "${SELECTED_MACHINE_ID}  # ${OFFER_LOCATION:-?} — aborted, never started" >> "$BLACKLIST_FILE"
            echo "  Blacklisted machine $SELECTED_MACHINE_ID"
        fi
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
for i in $(seq 1 60); do
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
    # Detect if image is cached or being pulled
    if [[ $i -eq 2 ]]; then
        if echo "$STATUS_MSG" | grep -qi "pulling\|pull"; then
            echo "  Docker image: pulling (not cached on this host)"
        elif [[ "$STATUS" == "running" || "$STATUS_MSG" == *"starting"* || "$STATUS_MSG" == *"success"* ]]; then
            echo "  Docker image: cached (fast start)"
        fi
    fi
    echo "  [$i/30] ${STATUS:-loading}: ${STATUS_MSG:-waiting...}"
done

if [[ -z "$SSH_HOST" ]]; then
    echo ""
    echo "ERROR: Instance $INSTANCE_ID did not start within 10 minutes"
    read -p "Destroy and blacklist machine? [Y/n] " destroy_confirm
    if [[ "$destroy_confirm" != "n" && "$destroy_confirm" != "N" ]]; then
        # Blacklist this machine — it can't start properly
        if [[ -n "$SELECTED_MACHINE_ID" ]]; then
            echo "${SELECTED_MACHINE_ID}  # ${OFFER_LOCATION} — stuck loading, never started" >> "$BLACKLIST_FILE"
            echo "  Blacklisted machine $SELECTED_MACHINE_ID"
        fi
        vastai destroy instance "$INSTANCE_ID" 2>/dev/null
        echo "Instance $INSTANCE_ID destroyed."
        INSTANCE_ID=""  # prevent trap from destroying again
    else
        echo "Instance kept running: vastai show instance $INSTANCE_ID"
    fi
    exit 1
fi

# ============================================================
# Phase 7: Verify actual CPU speed (vast.ai reports boost clock, not actual)
# ============================================================
echo ""
echo "=== Verifying instance hardware ==="
SSH="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 root@$SSH_HOST -p $SSH_PORT"
SCP="scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -P $SSH_PORT"

ACTUAL_HW=$($SSH '
    cpu_model=$(grep "model name" /proc/cpuinfo | head -1 | awk -F: "{print \$2}" | xargs)
    gpu=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1)
    gpu_count=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | wc -l)
    ram_gb=$(free -g | grep Mem | awk "{print \$2}")
    disk_free_gb=$(df -BG / | tail -1 | awk "{print \$4}" | tr -d "G")
    # Single-core CPU benchmark (~2s): hash loop, correlates with cv2 imread/imwrite
    bench_score=$(python3 -c "
import time, hashlib
start = time.time()
h = b\"bench\"
for _ in range(500000):
    h = hashlib.sha256(h).digest()
elapsed = time.time() - start
score = 500000 / elapsed  # hashes/sec, higher=faster
# Reference: Ryzen 9950X ~1.8M, 7950X ~1.5M, Xeon 8481C@2.7GHz ~0.8M, throttled@1.5GHz ~0.4M
print(f\"{score:.0f}\")
" 2>/dev/null)
    echo "$cpu_model|$gpu|$gpu_count|$ram_gb|$disk_free_gb|$bench_score"
' 2>/dev/null)

IFS='|' read -r actual_cpu_model actual_gpu actual_gpu_count actual_ram actual_disk bench_score <<< "$ACTUAL_HW"

# Benchmark thresholds (hashes/sec): >=1.2M=good, 0.8-1.2M=ok, <0.8M=too slow
bench_rating="GOOD"
if python3 -c "exit(0 if float('${bench_score:-0}') < 800000 else 1)" 2>/dev/null; then
    bench_rating="TOO SLOW"
elif python3 -c "exit(0 if float('${bench_score:-0}') < 1200000 else 1)" 2>/dev/null; then
    bench_rating="OK"
fi
bench_display=$(python3 -c "print(f'{float(\"${bench_score:-0}\") / 1000000:.2f}M')" 2>/dev/null)

echo "  CPU: $actual_cpu_model"
echo "  Benchmark: ${bench_display} hashes/sec (${bench_rating}) — Ryzen 9950X=1.8M, 7950X=1.5M"
echo "  GPU: ${actual_gpu_count}x $actual_gpu"
echo "  RAM: ${actual_ram} GB"
echo "  Disk free: ${actual_disk} GB"

if [[ "$bench_rating" == "TOO SLOW" ]]; then
    echo ""
    echo "  *** CPU BENCHMARK TOO SLOW: ${bench_display} hashes/sec ***"
    echo "  CPU is throttled or misconfigured (power-saving mode?)."
    echo "  RTX 5090 HD videos will be very slow (~0.1 fps instead of ~0.5 fps)."
    echo ""
    read -p "  Continue anyway, or destroy instance? [c=continue / D=destroy] " cpu_choice
    if [[ "$cpu_choice" != "c" && "$cpu_choice" != "C" ]]; then
        # Save slow benchmark score to cache — will show in future listings
        MACHINE_ID=$(vastai show instance "$INSTANCE_ID" --raw 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin).get('machine_id',''))" 2>/dev/null)
        if [[ -n "$MACHINE_ID" ]]; then
            sed -i "/^${MACHINE_ID} /d" "$CACHE_FILE" 2>/dev/null
            echo "${MACHINE_ID}  ${bench_score:-0}  # ${OFFER_LOCATION} — ${actual_cpu_model} — SLOW" >> "$CACHE_FILE"
            echo "  Saved benchmark ${bench_display} for machine $MACHINE_ID (visible in future listings)"
        fi
        echo "  Destroying instance $INSTANCE_ID..."
        vastai destroy instance "$INSTANCE_ID" 2>/dev/null
        echo "  Instance destroyed."
        INSTANCE_ID=""
        exit 1
    fi
    echo "  Continuing with slow CPU..."
fi

# ============================================================
# Phase 8: Deploy everything
# ============================================================
echo ""
echo "=== Deploying ==="

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
# Phase 9: Start processing
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
# Use actual GPU count from machine (auto-detected in Phase 7), not search NUM_GPUS
ACTUAL_GPUS="${actual_gpu_count:-$NUM_GPUS}"
ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -f root@"$SSH_HOST" -p "$SSH_PORT" \
    "sudo bash -c 'cd /root && nohup ./multi_gpu_queue.sh $ACTUAL_GPUS >> /root/enhance.log 2>&1 &'" 2>/dev/null
echo "Started multi_gpu_queue.sh on $ACTUAL_GPUS GPU(s)"

# Save machine_id + benchmark score to cache (has our Docker image now)
DEPLOY_MACHINE_ID=$(vastai show instance "$INSTANCE_ID" --raw 2>/dev/null | python3 -c "import json,sys; print(json.load(sys.stdin).get('machine_id',''))" 2>/dev/null)
if [[ -n "$DEPLOY_MACHINE_ID" ]]; then
    # Remove old entry for this machine (if any) and add with current benchmark
    sed -i "/^${DEPLOY_MACHINE_ID} /d" "$CACHE_FILE" 2>/dev/null
    echo "${DEPLOY_MACHINE_ID}  ${bench_score:-0}  # ${OFFER_LOCATION} — ${NUM_GPUS}x ${GPU_LABEL} — ${actual_cpu_model}" >> "$CACHE_FILE"
fi

# Test dashboard HTTP proxy
sleep 3
if curl -s --connect-timeout 5 "$DASHBOARD_URL" >/dev/null 2>&1; then
    echo "Dashboard: OK ($DASHBOARD_URL)"
else
    echo ""
    echo "WARNING: Dashboard not reachable via vast.ai proxy!"
    echo "  SSH tunnel: ssh -p $SSH_PORT -L 8080:localhost:8080 root@$SSH_HOST"
    echo "  Then open: http://localhost:8080"
    echo ""
    read -p "Continue with SSH tunnel, or destroy instance? [c=continue / D=destroy] " dash_choice
    if [[ "$dash_choice" != "c" && "$dash_choice" != "C" ]]; then
        echo "  Destroying instance $INSTANCE_ID..."
        vastai destroy instance "$INSTANCE_ID" 2>/dev/null
        echo "  Instance destroyed. Try a different host."
        INSTANCE_ID=""
        exit 1
    fi
fi

echo "Done."
