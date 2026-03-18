#!/usr/bin/env python3
"""
Fetch all non-enhanced Da Vaz videos with their actual resolution.
Uses YouTube API for titles/duration and yt-dlp for actual resolution.
Writes results to not_enhanced.json.
"""

import os
import sys
import json
import subprocess
import concurrent.futures
import re


def get_youtube_service():
    from google.auth.transport.requests import Request
    from google.oauth2.credentials import Credentials
    from googleapiclient.discovery import build

    SCOPES = ["https://www.googleapis.com/auth/youtube.readonly"]
    token_file = "youtube_token.json"
    creds = Credentials.from_authorized_user_file(token_file, SCOPES)
    if not creds.valid:
        if creds.expired and creds.refresh_token:
            creds.refresh(Request())
            with open(token_file, "w") as f:
                f.write(creds.to_json())
    return build("youtube", "v3", credentials=creds)


def get_channel_enhanced_titles(youtube):
    """Get all Enhanced 4K video titles from the channel."""
    channels = youtube.channels().list(part="id", mine=True).execute()
    channel_id = channels["items"][0]["id"]

    enhanced_titles = set()
    page_token = None
    while True:
        results = youtube.search().list(
            part="snippet", channelId=channel_id,
            q="Enhanced", type="video", maxResults=50,
            pageToken=page_token,
        ).execute()
        for item in results.get("items", []):
            title = item["snippet"]["title"]
            if "(Enhanced" in title:
                enhanced_titles.add(title.lower())
        page_token = results.get("nextPageToken")
        if not page_token:
            break
    return enhanced_titles


def get_video_details_batch(youtube, video_ids):
    """Fetch video details (no fileDetails - requires owner)."""
    results = {}
    for i in range(0, len(video_ids), 50):
        batch = video_ids[i:i+50]
        response = youtube.videos().list(
            part="snippet,contentDetails",
            id=",".join(batch),
        ).execute()
        for item in response.get("items", []):
            vid = item["id"]
            results[vid] = {
                "title": item["snippet"].get("title", ""),
                "definition": item["contentDetails"].get("definition", ""),
                "duration_iso": item["contentDetails"].get("duration", ""),
            }
        print(f"  YouTube API: {min(i+50, len(video_ids))}/{len(video_ids)}")
    return results


YTDLP = os.path.join(os.path.dirname(os.path.abspath(__file__)), "venv", "bin", "yt-dlp")

def get_resolution_ytdlp(video_id):
    """Get actual resolution via yt-dlp --dump-json (no download)."""
    try:
        result = subprocess.run(
            [YTDLP, "--dump-json", "--no-download", f"https://www.youtube.com/watch?v={video_id}"],
            capture_output=True, text=True, timeout=30,
        )
        if result.returncode == 0:
            data = json.loads(result.stdout)
            return video_id, data.get("width"), data.get("height")
    except Exception as e:
        pass
    return video_id, None, None


def parse_duration_iso(iso):
    """Convert ISO 8601 duration (PT1H2M3S) to seconds."""
    m = re.match(r'PT(?:(\d+)H)?(?:(\d+)M)?(?:(\d+)S)?', iso)
    if not m:
        return 0
    return int(m.group(1) or 0) * 3600 + int(m.group(2) or 0) * 60 + int(m.group(3) or 0)


def main():
    with open("enhanced_status.json") as f:
        status = json.load(f)

    all_missing = status["missing_sd"] + status["missing_hd"]
    all_missing_ids = [v["id"] for v in all_missing]
    print(f"Total non-enhanced videos: {len(all_missing_ids)}")

    youtube = get_youtube_service()

    print("Fetching enhanced titles from channel...")
    enhanced_titles = get_channel_enhanced_titles(youtube)
    print(f"Found {len(enhanced_titles)} enhanced videos")

    print("Fetching video details from YouTube API...")
    details = get_video_details_batch(youtube, all_missing_ids)

    # Get actual resolution via yt-dlp in parallel
    print(f"Fetching resolution via yt-dlp for {len(all_missing_ids)} videos (10 parallel)...")
    resolutions = {}
    done = 0
    with concurrent.futures.ThreadPoolExecutor(max_workers=10) as executor:
        futures = {executor.submit(get_resolution_ytdlp, vid): vid for vid in all_missing_ids}
        for future in concurrent.futures.as_completed(futures):
            vid, w, h = future.result()
            resolutions[vid] = (w, h)
            done += 1
            if done % 20 == 0:
                print(f"  yt-dlp: {done}/{len(all_missing_ids)}")
    print(f"  yt-dlp: {done}/{len(all_missing_ids)} done")

    # Build output
    videos = []
    no_resolution = 0
    for vid in all_missing_ids:
        if vid not in details:
            continue
        d = details[vid]
        duration_s = parse_duration_iso(d["duration_iso"])

        w, h = resolutions.get(vid, (None, None))
        if w and h:
            megapixels = round((w * h) / 1_000_000, 2)
            if d["definition"] == "sd":
                scale = 4
            else:
                scale = 2
            # GPU based on input megapixels
            # RTX 4090 (24GB) safe up to 1.6 MP, RTX 5090 (32GB) for larger
            if megapixels <= 1.6:
                gpu = "RTX 4090"
            else:
                gpu = "RTX 5090"
        else:
            w = h = None
            megapixels = None
            scale = 4 if d["definition"] == "sd" else 2
            gpu = "RTX 4090" if d["definition"] == "sd" else "RTX 5090"
            no_resolution += 1

        entry = {
            "youtube_id": vid,
            "title": d["title"],
            "duration_seconds": duration_s,
            "definition": d["definition"],
            "width": w,
            "height": h,
            "megapixels": megapixels,
            "scale": scale,
            "gpu": gpu,
        }
        videos.append(entry)

    videos.sort(key=lambda x: x["duration_seconds"], reverse=True)

    output = {
        "description": "Da Vaz videos without Enhanced 4K version",
        "generated": "2026-03-18",
        "total": len(videos),
        "summary": {
            "rtx_4090": sum(1 for v in videos if v["gpu"] == "RTX 4090"),
            "rtx_5090": sum(1 for v in videos if v["gpu"] == "RTX 5090"),
        },
        "videos": videos,
    }

    with open("not_enhanced.json", "w") as f:
        json.dump(output, f, indent=2, ensure_ascii=False)

    print(f"\nWritten {len(videos)} videos to not_enhanced.json")
    if no_resolution:
        print(f"  ({no_resolution} videos without resolution data)")
    print(f"  RTX 4090: {output['summary']['rtx_4090']}")
    print(f"  RTX 5090: {output['summary']['rtx_5090']}")


if __name__ == "__main__":
    main()
