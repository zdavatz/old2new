#!/bin/bash
# post_enhance.sh — Wait for enhancement to finish, then upload to YouTube and send email
# Usage: nohup bash post_enhance.sh <video_id> <job_name> > /root/post_enhance.log 2>&1 &

set -e

VIDEO_ID="$1"
JOB_NAME="$2"
SCALE="${3:-2}"

if [[ -z "$VIDEO_ID" || -z "$JOB_NAME" ]]; then
    echo "Usage: bash post_enhance.sh <video_id> <job_name> [scale]"
    echo "  video_id: YouTube video ID (e.g. aefe1fn7Kf0)"
    echo "  job_name: Job directory name under ~/jobs/"
    echo "  scale:    2 or 4 (default: 2)"
    exit 1
fi

ENHANCED="/root/jobs/${JOB_NAME}/${JOB_NAME}_${SCALE}x.mkv"

echo "=== post_enhance.sh ==="
echo "Video ID:  $VIDEO_ID"
echo "Job:       $JOB_NAME"
echo "Scale:     ${SCALE}x"
echo "Expected:  $ENHANCED"
echo "Started:   $(date)"
echo ""

# --- Wait for enhancement to finish ---
echo "Waiting for enhancement to complete..."
while true; do
    if [[ -f "$ENHANCED" ]]; then
        # Check file is not still being written (size stable for 30s)
        SIZE1=$(stat -c%s "$ENHANCED" 2>/dev/null || echo 0)
        sleep 30
        SIZE2=$(stat -c%s "$ENHANCED" 2>/dev/null || echo 0)
        if [[ "$SIZE1" -eq "$SIZE2" && "$SIZE1" -gt 0 ]]; then
            echo "Enhanced video ready: $ENHANCED ($(numfmt --to=iec $SIZE2))"
            break
        fi
        echo "  File still being written... ($SIZE2 bytes)"
    fi

    # Check if enhance_gpu.py is still running
    if ! pgrep -f "enhance_gpu.py" > /dev/null 2>&1; then
        if [[ -f "$ENHANCED" ]]; then
            echo "enhance_gpu.py finished, output found."
            break
        else
            echo "ERROR: enhance_gpu.py exited but no output file found!"
            echo "Check /root/enhance.log for errors."
            exit 1
        fi
    fi

    sleep 60
done

echo ""
echo "=== Uploading to YouTube ==="
echo "Time: $(date)"

# --- Upload to YouTube and send email ---
python3 /root/youtube_upload.py "$VIDEO_ID" "$ENHANCED" \
    --client-secret /root/client_secret.json \
    --token /root/youtube_token.json \
    --notify juerg@davaz.com

echo ""
echo "=== DONE ==="
echo "Time: $(date)"
echo "Instance can now be destroyed."
echo "Run: vastai destroy instance <ID>"
