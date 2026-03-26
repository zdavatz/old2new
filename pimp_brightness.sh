#!/usr/bin/env bash
# pimp_brightness.sh — Download YouTube video, adjust brightness of a time segment, re-upload
#
# Usage:
#   ./pimp_brightness.sh <youtube-url> <start> <end> <percent> [--title "Custom Title"]
#
# Examples:
#   ./pimp_brightness.sh "https://youtube.com/watch?v=J1VEC3Us21Q" 0:33 1:48 -25
#   ./pimp_brightness.sh "https://youtube.com/watch?v=J1VEC3Us21Q" 0:33 1:48 -25 --suffix v1
#   ./pimp_brightness.sh "https://youtube.com/watch?v=J1VEC3Us21Q" 0:33 1:48 -25 --title "Full Custom Title"
#   ./pimp_brightness.sh "https://youtube.com/watch?v=J1VEC3Us21Q" 5:00 8:30 +30
#
# Pipeline:
#   1. Download video from YouTube (yt-dlp)
#   2. Split into 3 segments (before/affected/after)
#   3. Re-encode ONLY the affected segment with gamma correction
#   4. Concatenate back together (stream-copy = instant)
#   5. Upload to YouTube with --title (or auto "Enhanced 4K" suffix)
#   6. Send email notification to juerg@davaz.com
#
# Gamma mapping: -25% = gamma 0.75 (darker), +30% = gamma 1.30 (brighter)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

usage() {
    echo "Usage: $0 <youtube-url> <start> <end> <percent> [--title \"Custom Title\"]"
    echo ""
    echo "  youtube-url: YouTube video URL"
    echo "  start/end:   timestamp like 0:33 or 1:48:05 or 90 (seconds)"
    echo "  percent:     -25 (darker) or +30 (brighter)"
    echo "  --suffix:    appended after 'Enhanced 4K' in title (e.g. v1 → 'Enhanced 4K v1')"
    echo "  --title:     fully custom YouTube upload title (overrides --suffix)"
    echo ""
    echo "Examples:"
    echo "  $0 \"https://youtube.com/watch?v=ABC123\" 0:33 1:48 -25"
    echo "  $0 \"https://youtube.com/watch?v=ABC123\" 0:33 1:48 -25 --suffix v1"
    echo "  $0 \"https://youtube.com/watch?v=ABC123\" 0:33 1:48 -25 --title \"Full Custom Title\""
    exit 1
}

[ $# -lt 4 ] && usage

URL="$1"
START="$2"
END="$3"
PERCENT="$4"
shift 4

CUSTOM_TITLE=""
SUFFIX=""
while [ $# -gt 0 ]; do
    case "$1" in
        --title) CUSTOM_TITLE="$2"; shift 2 ;;
        --suffix) SUFFIX="$2"; shift 2 ;;
        *) echo "Unknown option: $1"; usage ;;
    esac
done

# Extract video ID from URL
VIDEO_ID=$(echo "$URL" | sed -n 's/.*[?&]v=\([^&]*\).*/\1/p')
if [ -z "$VIDEO_ID" ]; then
    VIDEO_ID=$(echo "$URL" | sed -n 's|.*/\([^/?]*\)$|\1|p')
fi
[ -z "$VIDEO_ID" ] && echo "Error: could not extract video ID from $URL" && exit 1

# Convert timestamp to seconds
to_seconds() {
    local ts="$1"
    if [[ "$ts" =~ ^[0-9]+(\.[0-9]+)?$ ]]; then
        echo "$ts"
    elif [[ "$ts" =~ ^([0-9]+):([0-9]+)$ ]]; then
        echo "$(( ${BASH_REMATCH[1]} * 60 + ${BASH_REMATCH[2]} ))"
    elif [[ "$ts" =~ ^([0-9]+):([0-9]+):([0-9]+)$ ]]; then
        echo "$(( ${BASH_REMATCH[1]} * 3600 + ${BASH_REMATCH[2]} * 60 + ${BASH_REMATCH[3]} ))"
    else
        echo "Error: invalid timestamp '$ts'" >&2; exit 1
    fi
}

START_S=$(to_seconds "$START")
END_S=$(to_seconds "$END")
DURATION=$(echo "$END_S - $START_S" | bc)

if (( $(echo "$DURATION <= 0" | bc -l) )); then
    echo "Error: end ($END) must be after start ($START)"
    exit 1
fi

# Calculate gamma from percent: -25 -> 0.75, +30 -> 1.30
PERCENT_NUM="${PERCENT#+}"  # strip leading + for bc
GAMMA=$(echo "1 + ($PERCENT_NUM / 100)" | bc -l)
if (( $(echo "$GAMMA <= 0" | bc -l) )); then
    echo "Error: percent $PERCENT would result in gamma $GAMMA (must be > 0)"
    exit 1
fi

# Setup work directory
WORKDIR="$HOME/jobs/${VIDEO_ID}_brightness"
mkdir -p "$WORKDIR"

echo "=== pimp_brightness.sh ==="
echo "Video ID: $VIDEO_ID"
echo "Segment:  $START ($START_S s) - $END ($END_S s) = ${DURATION}s"
echo "Adjust:   ${PERCENT}% (gamma=$GAMMA)"
echo "Work dir: $WORKDIR"
echo ""

# --- Step 1: Download ---
INPUT="$WORKDIR/${VIDEO_ID}.mp4"
if [ -f "$INPUT" ]; then
    echo "[1/6] Video already downloaded: $INPUT"
else
    echo "[1/6] Downloading video ..."
    yt-dlp --remote-components ejs:github \
        -f "bestvideo[ext=mp4]+bestaudio[ext=m4a]/best[ext=mp4]/best" \
        -o "$INPUT" "$URL"
fi
echo ""

# --- Step 2: Split part 1 (before segment) ---
echo "[2/6] Splitting: 0:00 - $START (stream copy) ..."
ffmpeg -y -loglevel warning -i "$INPUT" -t "$START_S" -c copy "$WORKDIR/part1.mp4"

# --- Step 3: Re-encode affected segment ---
echo "[3/6] Re-encoding: $START - $END with gamma=$GAMMA ..."
ffmpeg -y -loglevel warning -i "$INPUT" -ss "$START_S" -t "$DURATION" \
    -vf "eq=gamma=$GAMMA" -c:v libx264 -crf 18 -preset medium \
    -c:a aac -b:a 192k "$WORKDIR/part2.mp4"

# --- Step 4: Split part 3 (after segment) ---
echo "[4/6] Splitting: $END - end (stream copy) ..."
ffmpeg -y -loglevel warning -i "$INPUT" -ss "$END_S" -c copy "$WORKDIR/part3.mp4"

# --- Step 5: Concatenate ---
OUTPUT="$WORKDIR/${VIDEO_ID}_pimped.mp4"
echo "[5/6] Concatenating ..."
printf "file 'part1.mp4'\nfile 'part2.mp4'\nfile 'part3.mp4'\n" > "$WORKDIR/filelist.txt"
ffmpeg -y -loglevel warning -f concat -safe 0 -i "$WORKDIR/filelist.txt" -c copy "$OUTPUT"

# Verify duration
ORIG_DUR=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$INPUT")
NEW_DUR=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$OUTPUT")
echo "  Duration: ${NEW_DUR}s (original: ${ORIG_DUR}s)"
echo ""

# --- Step 6: Upload ---
echo "[6/6] Uploading to YouTube ..."

# Find youtube_upload binary
UPLOAD_BIN=""
for p in "$HOME/youtube_upload" "$SCRIPT_DIR/youtube_upload_rs/target/release/youtube_upload"; do
    [ -x "$p" ] && UPLOAD_BIN="$p" && break
done
[ -z "$UPLOAD_BIN" ] && echo "Error: youtube_upload binary not found" && exit 1

# Find credentials
CLIENT_SECRET=""
for p in "$HOME/client_secret.json" "$SCRIPT_DIR/client_secret.json"; do
    [ -f "$p" ] && CLIENT_SECRET="$p" && break
done
[ -z "$CLIENT_SECRET" ] && echo "Error: client_secret.json not found" && exit 1

TOKEN=""
for p in "$HOME/youtube_token.json" "$SCRIPT_DIR/youtube_token.json"; do
    [ -f "$p" ] && TOKEN="$p" && break
done
[ -z "$TOKEN" ] && echo "Error: youtube_token.json not found" && exit 1

# Build title: --title overrides everything, --suffix appends to original title
UPLOAD_TITLE=""
if [ -n "$CUSTOM_TITLE" ]; then
    UPLOAD_TITLE="$CUSTOM_TITLE"
elif [ -n "$SUFFIX" ]; then
    # Fetch original title via yt-dlp (fast, no download)
    ORIG_TITLE=$(yt-dlp --remote-components ejs:github --print title "$URL" 2>/dev/null)
    # Strip all existing "(Enhanced 4K ...)" suffixes, then append the new one
    STRIPPED=$(echo "$ORIG_TITLE" | sed -E 's/ *\(Enhanced 4K[^)]*\)//g' | sed 's/ *$//')
    UPLOAD_TITLE="$STRIPPED (Enhanced 4K $SUFFIX)"
    echo "  Upload title: $UPLOAD_TITLE"
fi

UPLOAD_ARGS=(
    --video-id="$VIDEO_ID"
    --client-secret "$CLIENT_SECRET"
    --token "$TOKEN"
    --notify "juerg@davaz.com"
)
[ -n "$UPLOAD_TITLE" ] && UPLOAD_ARGS+=(--title "$UPLOAD_TITLE")

"$UPLOAD_BIN" "${UPLOAD_ARGS[@]}" "$OUTPUT"

# Cleanup temp parts
rm -f "$WORKDIR/part1.mp4" "$WORKDIR/part2.mp4" "$WORKDIR/part3.mp4" "$WORKDIR/filelist.txt"

echo ""
echo "=== Done! ==="
