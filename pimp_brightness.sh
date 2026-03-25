#!/usr/bin/env bash
# pimp_brightness.sh — Adjust brightness of a specific time segment efficiently
# Only re-encodes the affected segment, stream-copies the rest (fast!)
#
# Usage:
#   ./pimp_brightness.sh <input.mp4> <start> <end> <percent> [output.mp4]
#
# Examples:
#   ./pimp_brightness.sh video.mp4 0:33 1:48 -25          # 25% darker
#   ./pimp_brightness.sh video.mp4 0:33 1:48 +30          # 30% brighter
#   ./pimp_brightness.sh video.mp4 0:33 1:48 -25 out.mp4  # custom output name
#
# How it works:
#   1. Stream-copy part before start (instant)
#   2. Re-encode only the affected segment with gamma correction (fast)
#   3. Stream-copy part after end (instant)
#   4. Concatenate all 3 parts (instant)
#
# Gamma mapping: -25% = gamma 0.75 (darker), +30% = gamma 1.30 (brighter)

set -euo pipefail

usage() {
    echo "Usage: $0 <input.mp4> <start> <end> <percent> [output.mp4]"
    echo ""
    echo "  start/end:  timestamp like 0:33 or 1:48 or 90 (seconds)"
    echo "  percent:    -25 (darker) or +30 (brighter)"
    echo ""
    echo "Examples:"
    echo "  $0 video.mp4 0:33 1:48 -25"
    echo "  $0 video.mp4 0:33 1:48 +30 corrected.mp4"
    exit 1
}

[ $# -lt 4 ] && usage

INPUT="$1"
START="$2"
END="$3"
PERCENT="$4"
OUTPUT="${5:-}"

[ ! -f "$INPUT" ] && echo "Error: $INPUT not found" && exit 1

# Convert timestamp to seconds (handles 0:33, 1:48:05, or plain seconds)
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
GAMMA=$(echo "1 + ($PERCENT / 100)" | bc -l)
if (( $(echo "$GAMMA <= 0" | bc -l) )); then
    echo "Error: percent $PERCENT would result in gamma $GAMMA (must be > 0)"
    exit 1
fi

# Default output name
if [ -z "$OUTPUT" ]; then
    BASE="${INPUT%.*}"
    EXT="${INPUT##*.}"
    OUTPUT="${BASE}_brightness.${EXT}"
fi

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

echo "Input:    $INPUT"
echo "Segment:  $START ($START_S s) - $END ($END_S s) = ${DURATION}s"
echo "Adjust:   ${PERCENT}% (gamma=$GAMMA)"
echo "Output:   $OUTPUT"
echo ""

# Part 1: before the segment (stream copy = instant)
echo "[1/4] Copying 0:00 - $START ..."
ffmpeg -y -loglevel warning -i "$INPUT" -t "$START_S" -c copy "$TMPDIR/part1.mp4"

# Part 2: the segment (re-encode with gamma)
echo "[2/4] Re-encoding $START - $END with gamma=$GAMMA ..."
ffmpeg -y -loglevel warning -i "$INPUT" -ss "$START_S" -t "$DURATION" \
    -vf "eq=gamma=$GAMMA" -c:v libx264 -crf 18 -preset medium \
    -c:a aac -b:a 192k "$TMPDIR/part2.mp4"

# Part 3: after the segment (stream copy = instant)
echo "[3/4] Copying $END - end ..."
ffmpeg -y -loglevel warning -i "$INPUT" -ss "$END_S" -c copy "$TMPDIR/part3.mp4"

# Concatenate
echo "[4/4] Concatenating ..."
printf "file 'part1.mp4'\nfile 'part2.mp4'\nfile 'part3.mp4'\n" > "$TMPDIR/filelist.txt"
ffmpeg -y -loglevel warning -f concat -safe 0 -i "$TMPDIR/filelist.txt" -c copy "$OUTPUT"

# Verify
ORIG_DUR=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$INPUT")
NEW_DUR=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$OUTPUT")
echo ""
echo "Done! Duration: ${NEW_DUR}s (original: ${ORIG_DUR}s)"
echo "Output: $OUTPUT"
