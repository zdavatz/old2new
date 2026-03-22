#!/bin/bash
# Fetch video metadata from YouTube via yt-dlp and save as individual JSON files
# Usage: ./fetch_video_json.sh <video_id> [video_id2] ...
# Example: ./fetch_video_json.sh BR5U-miBmt4 wjAkVoSN8jE
# Output: json/<video_id>.json per video
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
JSON_DIR="$SCRIPT_DIR/json"
mkdir -p "$JSON_DIR"

if [ $# -eq 0 ]; then
    echo "Usage: $0 <video_id> [video_id2] ..."
    echo "Example: $0 BR5U-miBmt4 wjAkVoSN8jE"
    echo ""
    echo "Fetches video info via yt-dlp and saves as json/<video_id>.json"
    echo "Skips videos that already have a JSON file (use rm json/<id>.json to re-fetch)"
    exit 1
fi

total=$#
count=0
failed=0

for vid in "$@"; do
    outfile="$JSON_DIR/${vid}.json"
    if [ -f "$outfile" ]; then
        count=$((count + 1))
        echo "[$count/$total] SKIP (exists): $vid"
        continue
    fi

    raw=$(yt-dlp --remote-components ejs:github --dump-json --no-download "https://www.youtube.com/watch?v=$vid" 2>/dev/null)
    if [ $? -eq 0 ] && [ -n "$raw" ]; then
        echo "$raw" | python3 -c "
import json, sys
v = json.load(sys.stdin)
w = v.get('width', 0) or 0
h = v.get('height', 0) or 0
mp = w * h / 1_000_000 if w and h else 0
info = {
    'video_id': v.get('id', ''),
    'title': v.get('title', ''),
    'description': v.get('description', ''),
    'channel': v.get('channel', ''),
    'upload_date': v.get('upload_date', ''),
    'duration_seconds': v.get('duration', 0),
    'width': w,
    'height': h,
    'fps': v.get('fps', 25),
    'view_count': v.get('view_count', 0),
    'like_count': v.get('like_count', 0),
    'categories': v.get('categories', []),
    'tags': v.get('tags', []),
    'thumbnail': v.get('thumbnail', ''),
    'webpage_url': v.get('webpage_url', ''),
    'megapixels': round(mp, 2),
    'definition': 'hd' if w >= 1280 or h >= 720 else 'sd',
    'scale': 2 if mp > 1.6 else 4,
    'gpu': 'RTX 5090' if mp > 1.6 else 'RTX 4090',
    'resolution': f'{w}x{h}',
}
with open('$outfile', 'w') as f:
    json.dump(info, f, indent=2, ensure_ascii=False)
print(f'OK: {info[\"title\"][:60]} ({info[\"resolution\"]}, {info[\"duration_seconds\"]}s)')
" 2>/dev/null
        if [ $? -eq 0 ]; then
            count=$((count + 1))
            echo "[$count/$total] $vid"
        else
            failed=$((failed + 1))
            echo "[$count/$total] PARSE FAILED: $vid"
        fi
    else
        failed=$((failed + 1))
        echo "[$count/$total] FETCH FAILED: $vid"
    fi
done

echo ""
echo "=== Done: $count/$total OK, $failed failed ==="
echo "JSON files in: $JSON_DIR/"
