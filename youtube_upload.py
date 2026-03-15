#!/usr/bin/env python3
"""
Upload enhanced video to YouTube, copying title/description from original.

Usage:
    python3 youtube_upload.py <video_id> <enhanced_file> [--client-secret FILE] [--token FILE]

First run requires browser auth. Token is saved and reused for subsequent uploads.
The token file can be copied to remote servers for headless uploads.
"""

import os
import sys
import json
import argparse

def get_authenticated_service(client_secret_file, token_file):
    """Authenticate with YouTube API, reusing token if available."""
    from google_auth_oauthlib.flow import InstalledAppFlow
    from google.auth.transport.requests import Request
    from google.oauth2.credentials import Credentials
    from googleapiclient.discovery import build

    SCOPES = [
        "https://www.googleapis.com/auth/youtube.upload",
        "https://www.googleapis.com/auth/youtube.readonly",
        "https://www.googleapis.com/auth/youtube",
        "https://www.googleapis.com/auth/gmail.send",
    ]

    creds = None
    if os.path.exists(token_file):
        creds = Credentials.from_authorized_user_file(token_file, SCOPES)

    if not creds or not creds.valid:
        if creds and creds.expired and creds.refresh_token:
            creds.refresh(Request())
        else:
            flow = InstalledAppFlow.from_client_secrets_file(client_secret_file, SCOPES)
            creds = flow.run_local_server(port=0)
        with open(token_file, "w") as f:
            f.write(creds.to_json())
        print(f"Token saved to {token_file}")

    return build("youtube", "v3", credentials=creds)


def get_video_details(youtube, video_id):
    """Fetch title, description, tags, category from existing YouTube video."""
    response = youtube.videos().list(
        part="snippet,status",
        id=video_id
    ).execute()

    if not response.get("items"):
        print(f"ERROR: Video {video_id} not found on YouTube")
        sys.exit(1)

    snippet = response["items"][0]["snippet"]
    status = response["items"][0]["status"]

    return {
        "title": snippet.get("title", ""),
        "description": snippet.get("description", ""),
        "tags": snippet.get("tags", []),
        "categoryId": snippet.get("categoryId", "22"),
        "privacyStatus": status.get("privacyStatus", "public"),
    }


def upload_video(youtube, filepath, metadata):
    """Upload video to YouTube with given metadata."""
    from googleapiclient.http import MediaFileUpload

    title = metadata["title"]
    if "(Enhanced" not in title:
        title = f"{title} (Enhanced 4K)"

    body = {
        "snippet": {
            "title": title,
            "description": metadata["description"],
            "tags": metadata.get("tags", []),
            "categoryId": metadata.get("categoryId", "22"),
        },
        "status": {
            "privacyStatus": "public",
            "selfDeclaredMadeForKids": False,
        },
    }

    filesize = os.path.getsize(filepath) / (1024 * 1024)
    print(f"Uploading: {os.path.basename(filepath)} ({filesize:.0f} MB)")
    print(f"Title: {title}")
    print(f"Privacy: public")
    print()

    media = MediaFileUpload(
        filepath,
        mimetype="video/x-matroska",
        resumable=True,
        chunksize=10 * 1024 * 1024,  # 10MB chunks
    )

    request = youtube.videos().insert(
        part="snippet,status",
        body=body,
        media_body=media,
    )

    response = None
    while response is None:
        status, response = request.next_chunk()
        if status:
            pct = int(status.progress() * 100)
            print(f"  Uploading: {pct}%")

    video_id = response["id"]
    print(f"\nUpload complete!")
    print(f"  Video ID: {video_id}")
    print(f"  URL: https://www.youtube.com/watch?v={video_id}")
    return video_id


def send_notification_email(creds, to_email, subject, body_text):
    """Send email notification via Gmail API using existing OAuth credentials."""
    import base64
    from email.mime.text import MIMEText
    from googleapiclient.discovery import build

    try:
        gmail = build("gmail", "v1", credentials=creds)
        message = MIMEText(body_text)
        message["to"] = to_email
        message["subject"] = subject
        raw = base64.urlsafe_b64encode(message.as_bytes()).decode()
        gmail.users().messages().send(
            userId="me", body={"raw": raw}
        ).execute()
        print(f"Email sent to {to_email}")
    except Exception as e:
        print(f"Warning: Could not send email: {e}")


def main():
    parser = argparse.ArgumentParser(description="Upload enhanced video to YouTube")
    parser.add_argument("video_id", help="Original YouTube video ID (to copy title/description)")
    parser.add_argument("enhanced_file", help="Path to enhanced video file")
    parser.add_argument("--client-secret", default="client_secret.json", help="OAuth client secret file")
    parser.add_argument("--token", default="youtube_token.json", help="OAuth token file (saved after first auth)")
    parser.add_argument("--title-suffix", default="(Enhanced 4K)", help="Suffix added to title")
    parser.add_argument("--notify", default="juerg@davaz.com", help="Email to notify after upload")
    args = parser.parse_args()

    if not os.path.exists(args.enhanced_file):
        print(f"ERROR: File not found: {args.enhanced_file}")
        sys.exit(1)

    if not os.path.exists(args.client_secret):
        print(f"ERROR: Client secret not found: {args.client_secret}")
        print("Download from Google Cloud Console → Credentials → OAuth 2.0 Client IDs")
        sys.exit(1)

    # Authenticate
    print("Authenticating with YouTube...")
    youtube = get_authenticated_service(args.client_secret, args.token)

    # Get original video details
    print(f"Fetching details for original video {args.video_id}...")
    metadata = get_video_details(youtube, args.video_id)
    print(f"  Original title: {metadata['title']}")
    print(f"  Description: {metadata['description'][:100]}..." if len(metadata['description']) > 100 else f"  Description: {metadata['description']}")
    print()

    # Upload
    new_video_id = upload_video(youtube, args.enhanced_file, metadata)

    # Send email notification
    if args.notify and new_video_id:
        from google.oauth2.credentials import Credentials
        creds = Credentials.from_authorized_user_file(args.token)
        title = metadata["title"]
        enhanced_url = f"https://www.youtube.com/watch?v={new_video_id}"
        original_url = f"https://www.youtube.com/watch?v={args.video_id}"

        subject = f"[completed] {title} (Enhanced 4K)"
        body = f"""Video Enhancement Completed

Title: {title} (Enhanced 4K)
Original: {original_url}
Enhanced: {enhanced_url}
Scale: 4x
Model: Real-ESRGAN x4plus

Checklist:
- [x] Upscaling completed
- [x] Uploaded to YouTube
- [ ] Quality reviewed
- [ ] Old video deleted from YouTube

---
Generated by old2new
https://github.com/zdavatz/old2new
"""
        send_notification_email(creds, args.notify, subject, body)


if __name__ == "__main__":
    main()
