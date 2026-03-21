#!/bin/bash
export PATH="/opt/venv/bin:/usr/local/bin:/usr/bin:/bin:$PATH"
echo "=== Sichuan v2: 16 HD videos on 4x RTX 5090 at $(date) ==="

# Start status server
python3 /root/status_server.py &

# Write instance metadata
cat > /root/instance_meta.json << 'EOF'
{
  "label": "davaz-hd-sichuan-4x5090-v2",
  "location": "Sichuan, CN",
  "cost_per_hr": 1.49,
  "provider": "vast.ai",
  "instance_id": "33281125"
}
EOF

# Write video queue for dashboard
cat > /root/video_queue.json << 'QEOF'
[
  {"id": "q_UgL0Pbet8", "scale": 2, "title": "077a_Republic_of_Georgia_WHERE_GOD_LANDED_-_RGAOEsub", "display_title": "077a Republic of Georgia WHERE GOD LANDED", "duration": 3872},
  {"id": "l8szkLe2eiM", "scale": 2, "title": "077b_Republik_Georgien_FUSSTRITTE_GOTTES_-_RGAODsub", "display_title": "077b Republik Georgien FUSSTRITTE GOTTES", "duration": 3876},
  {"id": "tljAVZCj6lw", "scale": 2, "title": "BLUEPRINTS_of_LIFE", "display_title": "BLUEPRINTS of LIFE", "duration": 3602},
  {"id": "N_Ui88q-gy8", "scale": 2, "title": "072a_Iran_CHADOR_CONDOM_COFFEESHOP_-_FarsiEsub", "display_title": "072a Iran CHADOR CONDOM COFFEESHOP", "duration": 3377},
  {"id": "opavvOpVpUM", "scale": 2, "title": "072b_Iran_KOPFTUCH_PARISER_KAFFEEHAUS_-_FarsiDsub", "display_title": "072b Iran KOPFTUCH PARISER KAFFEEHAUS", "duration": 3376},
  {"id": "9lZDEOnRgSU", "scale": 2, "title": "054b_HUMUS_for_HAMAS_-_ArabicHsub", "display_title": "054b HUMUS for HAMAS - ArabicHsub", "duration": 3200},
  {"id": "G9Whw4gJCeY", "scale": 2, "title": "054a_HUMUS_for_HAMAS_Gaza_Strip_-_ArabicEsub", "display_title": "054a HUMUS for HAMAS Gaza Strip", "duration": 3132},
  {"id": "u9LHBYxyj5w", "scale": 2, "title": "VERGANGENHEIT_als_VERMACHTNIS_work_in_progress_1.0", "display_title": "VERGANGENHEIT als VERMACHTNIS", "duration": 2701},
  {"id": "rXUD-jxOCuM", "scale": 2, "title": "051a_MISHA_goes_to_SCHOOL_-_REsub", "display_title": "051a MISHA goes to SCHOOL", "duration": 2368},
  {"id": "4_gaU85Zzog", "scale": 2, "title": "075a_MAOs_BARBERSHOP_-_chineseEsub", "display_title": "075a MAOs BARBERSHOP", "duration": 2072},
  {"id": "v27m8UT4w0M", "scale": 2, "title": "074c_ZAVESHCHANIE_EVGENIYU_BURAKU_-_R", "display_title": "074c ZAVESHCHANIE EVGENIYU BURAKU", "duration": 2059},
  {"id": "e1e-_J0PzTY", "scale": 2, "title": "Museum_Rundgang_Werni_2024", "display_title": "Museum Rundgang Werni 2024", "duration": 2014},
  {"id": "oUnnIVKwxv0", "scale": 2, "title": "052d_Itzhak_Frey_Malchik_-_Russian_sub", "display_title": "052d Itzhak Frey Malchik - Russian sub", "duration": 1940},
  {"id": "zPTk4BzdBu8", "scale": 2, "title": "052b_Itzhak_Frey_Son_-_DEsub", "display_title": "052b Itzhak Frey Son - DEsub", "duration": 1940},
  {"id": "wxJ7SGgb42c", "scale": 2, "title": "052a_Itzhak_Frey_Sohn_-_D", "display_title": "052a Itzhak Frey Sohn - D", "duration": 1940},
  {"id": "cgHVbxV64VU", "scale": 2, "title": "068_SEX_on_the_STEPS_-_Yellow_Mountains", "display_title": "068 SEX on the STEPS - Yellow Mountains", "duration": 1900}
]
QEOF

# Write work queue for flock-based processing
cat > /root/video_work_queue.txt << 'WQEOF'
q_UgL0Pbet8|2|077a_Republic_of_Georgia_WHERE_GOD_LANDED_-_RGAOEsub
l8szkLe2eiM|2|077b_Republik_Georgien_FUSSTRITTE_GOTTES_-_RGAODsub
tljAVZCj6lw|2|BLUEPRINTS_of_LIFE
N_Ui88q-gy8|2|072a_Iran_CHADOR_CONDOM_COFFEESHOP_-_FarsiEsub
opavvOpVpUM|2|072b_Iran_KOPFTUCH_PARISER_KAFFEEHAUS_-_FarsiDsub
9lZDEOnRgSU|2|054b_HUMUS_for_HAMAS_-_ArabicHsub
G9Whw4gJCeY|2|054a_HUMUS_for_HAMAS_Gaza_Strip_-_ArabicEsub
u9LHBYxyj5w|2|VERGANGENHEIT_als_VERMACHTNIS_work_in_progress_1.0
rXUD-jxOCuM|2|051a_MISHA_goes_to_SCHOOL_-_REsub
4_gaU85Zzog|2|075a_MAOs_BARBERSHOP_-_chineseEsub
v27m8UT4w0M|2|074c_ZAVESHCHANIE_EVGENIYU_BURAKU_-_R
e1e-_J0PzTY|2|Museum_Rundgang_Werni_2024
oUnnIVKwxv0|2|052d_Itzhak_Frey_Malchik_-_Russian_sub
zPTk4BzdBu8|2|052b_Itzhak_Frey_Son_-_DEsub
wxJ7SGgb42c|2|052a_Itzhak_Frey_Sohn_-_D
cgHVbxV64VU|2|068_SEX_on_the_STEPS_-_Yellow_Mountains
WQEOF

process_video() {
    local gpu=$1 vid=$2 scale=$3 title=$4
    export CUDA_VISIBLE_DEVICES=$gpu
    echo "[GPU $gpu] Starting: $title ($(date +%H:%M))"
    python3 /root/enhance_gpu.py "https://www.youtube.com/watch?v=$vid" "$scale" --job-name "$title" >> /root/gpu${gpu}.log 2>&1
    local exit_code=$?
    if [[ $exit_code -eq 0 ]]; then
        echo "[GPU $gpu] SUCCESS: $title"
        ENHANCED="/root/jobs/$title/${title}_${scale}x.mkv"
        if [[ -f "$ENHANCED" && -f "/root/client_secret.json" ]]; then
            echo "[GPU $gpu] Uploading $title..."
            if python3 /root/youtube_upload.py --video-id="$vid" "$ENHANCED" \
                --client-secret /root/client_secret.json \
                --token /root/youtube_token.json; then
                echo "[GPU $gpu] Uploaded + cleaned: $title"
                touch "/root/jobs/$title/.uploaded"
                rm -rf "/root/jobs/$title"
            else
                echo "[GPU $gpu] Upload FAILED: $title"
            fi
        fi
        return 0
    else
        echo "[GPU $gpu] FAILED (exit $exit_code): $title"
        return $exit_code
    fi
}

gpu_worker() {
    local gpu=$1
    local pidfile="/root/gpu${gpu}.worker.pid"

    echo $BASHPID > "$pidfile"
    sleep 1
    if [[ "$(cat "$pidfile" 2>/dev/null)" != "$BASHPID" ]]; then
        echo "[GPU $gpu] Lost race — aborting"
        return
    fi

    echo "[GPU $gpu] Worker started (PID $BASHPID)"

    while true; do
        local line
        line=$(flock /root/queue.lock bash -c 'head -1 /root/video_work_queue.txt 2>/dev/null && sed -i "1d" /root/video_work_queue.txt')
        if [[ -z "$line" ]]; then break; fi

        local vid=$(echo "$line" | cut -d'|' -f1)
        local scale=$(echo "$line" | cut -d'|' -f2)
        local title=$(echo "$line" | cut -d'|' -f3)

        # Retry on OOM-kill (exit > 128)
        local max_retries=3 retry=0
        while true; do
            process_video "$gpu" "$vid" "$scale" "$title"
            local result=$?
            if [[ $result -eq 0 ]]; then break; fi
            if [[ $result -gt 128 ]]; then
                retry=$((retry + 1))
                if [[ $retry -ge $max_retries ]]; then
                    echo "[GPU $gpu] GIVING UP after $max_retries retries: $title"
                    break
                fi
                echo "[GPU $gpu] Killed, waiting 60s, retry $retry/$max_retries: $title"
                sleep 60
            else
                echo "[GPU $gpu] Skipping $title (exit $result)"
                break
            fi
        done
    done
    echo "[GPU $gpu] Queue empty. Done at $(date)."
    rm -f "$pidfile"
}

# Start 4 workers — one per GPU
gpu_worker 0 &
gpu_worker 1 &
gpu_worker 2 &
gpu_worker 3 &
wait

echo "=== All 16 videos done at $(date) ==="
