#!/usr/bin/env python3
"""gmail_send.py — send mail through the Gmail API with the MIME we actually wrote.

Why this exists: composing through a high-level "body + htmlBody" interface lets
Gmail's compose pipeline rewrite the message on its way out — most visibly, every
link gets wrapped in a `https://www.google.com/url?q=…&source=gmail&ust=…`
redirect, which lands in the *sent* MIME, not just in the reading view. Handing
`users.messages.send` a finished `raw` MIME skips that: what you encode is what
is delivered, so a YouTube link stays a YouTube link.

Stdlib only, like the other tools here.

Usage:
    ./gmail_send.py --auth                       # one-time consent
    ./gmail_send.py --to a@b.com --subject "..." --text body.txt [--html body.html]
    ./gmail_send.py ... --dry-run                # print the MIME, send nothing

Setup (once, in the Google Cloud console):
    1. Enable the "Gmail API" in the project.
    2. Reuse the existing Desktop OAuth client (client_secret_dataportability.json
       is found automatically) and add the scope
       https://www.googleapis.com/auth/gmail.send
    3. Keep yourself on the consent screen's test users / internal audience.
"""

import argparse
import base64
import http.server
import json
import os
import secrets
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import webbrowser
from email.message import EmailMessage

HERE = os.path.dirname(os.path.abspath(__file__))
TOKEN_FILE = os.path.join(HERE, "gmail_token.json")
# The same Desktop client works for any scope; prefer a Gmail-specific file if
# one exists, else fall back to the clients already sitting in this directory.
CLIENT_CANDIDATES = [
    "client_secret_gmail.json",
    "client_secret_dataportability.json",
    "client_secret.json",
]

AUTH_URI = "https://accounts.google.com/o/oauth2/v2/auth"
TOKEN_URI = "https://oauth2.googleapis.com/token"
SEND_URI = "https://gmail.googleapis.com/gmail/v1/users/me/messages/send"
SCOPE = "https://www.googleapis.com/auth/gmail.send"

# 8091 tk_push, 8092 li_push, 8093 yt_export.
CALLBACK_PORT = 8094
REDIRECT_URI = f"http://localhost:{CALLBACK_PORT}"


def client_secret_path(explicit=None):
    if explicit:
        return explicit
    for name in CLIENT_CANDIDATES:
        p = os.path.join(HERE, name)
        if os.path.exists(p):
            return p
    sys.exit(
        "ERROR: no OAuth client JSON found (looked for: "
        + ", ".join(CLIENT_CANDIDATES)
        + ").\nCreate a Desktop-app client in the Google Cloud console."
    )


def load_client(explicit=None):
    with open(client_secret_path(explicit)) as fh:
        data = json.load(fh)
    cfg = data.get("installed") or data.get("web")
    if not cfg:
        sys.exit("ERROR: client secret JSON has neither an 'installed' nor a 'web' section.")
    return cfg["client_id"], cfg["client_secret"]


def post_form(url, fields):
    body = urllib.parse.urlencode(fields).encode()
    req = urllib.request.Request(url, data=body, method="POST")
    req.add_header("Content-Type", "application/x-www-form-urlencoded")
    with urllib.request.urlopen(req) as resp:
        return json.load(resp)


class _CallbackHandler(http.server.BaseHTTPRequestHandler):
    result = {}

    def do_GET(self):  # noqa: N802
        params = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
        _CallbackHandler.result = {k: v[0] for k, v in params.items()}
        ok = "code" in _CallbackHandler.result
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.end_headers()
        msg = "Authorized. You can close this tab." if ok else "Authorization failed."
        self.wfile.write(f"<html><body><h3>{msg}</h3></body></html>".encode())

    def log_message(self, *args):
        pass


def auth_flow(client_secret=None):
    client_id, secret = load_client(client_secret)
    state = secrets.token_urlsafe(16)
    params = {
        "client_id": client_id,
        "redirect_uri": REDIRECT_URI,
        "response_type": "code",
        "scope": SCOPE,
        "state": state,
        "access_type": "offline",
        "prompt": "consent",
    }
    url = f"{AUTH_URI}?{urllib.parse.urlencode(params)}"
    server = http.server.HTTPServer(("localhost", CALLBACK_PORT), _CallbackHandler)
    threading.Thread(target=server.handle_request, daemon=True).start()
    print("Opening browser for Google consent (scope: gmail.send)...")
    print(f"If nothing opens, visit:\n{url}\n")
    webbrowser.open(url)
    deadline = time.time() + 300
    while not _CallbackHandler.result and time.time() < deadline:
        time.sleep(0.5)
    server.server_close()
    res = _CallbackHandler.result
    if "code" not in res:
        sys.exit(f"ERROR: no authorization code received ({res.get('error', 'timed out')})")
    if res.get("state") != state:
        sys.exit("ERROR: OAuth state mismatch — aborting.")
    token = post_form(TOKEN_URI, {
        "code": res["code"],
        "client_id": client_id,
        "client_secret": secret,
        "redirect_uri": REDIRECT_URI,
        "grant_type": "authorization_code",
    })
    token["obtained_at"] = int(time.time())
    with open(TOKEN_FILE, "w") as fh:
        json.dump(token, fh, indent=2)
    os.chmod(TOKEN_FILE, 0o600)
    print(f"Token saved to {os.path.basename(TOKEN_FILE)}")


def access_token(client_secret=None):
    if not os.path.exists(TOKEN_FILE):
        sys.exit("ERROR: not authorized yet — run --auth first.")
    with open(TOKEN_FILE) as fh:
        token = json.load(fh)
    if time.time() < token.get("obtained_at", 0) + token.get("expires_in", 3600) - 60:
        return token["access_token"]
    if "refresh_token" not in token:
        sys.exit("ERROR: token expired and no refresh_token stored — run --auth again.")
    client_id, secret = load_client(client_secret)
    refreshed = post_form(TOKEN_URI, {
        "refresh_token": token["refresh_token"],
        "client_id": client_id,
        "client_secret": secret,
        "grant_type": "refresh_token",
    })
    token.update(refreshed)
    token["obtained_at"] = int(time.time())
    with open(TOKEN_FILE, "w") as fh:
        json.dump(token, fh, indent=2)
    return token["access_token"]


def build_mime(to, subject, text, html=None, sender=None, cc=None):
    """Assemble the message ourselves so nothing rewrites it later."""
    msg = EmailMessage()
    msg["To"] = ", ".join(to)
    if cc:
        msg["Cc"] = ", ".join(cc)
    if sender:
        msg["From"] = sender
    msg["Subject"] = subject
    msg.set_content(text)
    if html:
        msg.add_alternative(html, subtype="html")
    return msg


def send(msg, token):
    raw = base64.urlsafe_b64encode(msg.as_bytes()).decode()
    req = urllib.request.Request(
        SEND_URI, data=json.dumps({"raw": raw}).encode(), method="POST"
    )
    req.add_header("Authorization", f"Bearer {token}")
    req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req) as resp:
            return json.load(resp)
    except urllib.error.HTTPError as err:
        sys.exit(f"ERROR: {err.code} from Gmail\n{err.read().decode(errors='replace')}")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--auth", action="store_true", help="run the OAuth consent flow")
    ap.add_argument("--to", nargs="+", help="recipient address(es)")
    ap.add_argument("--cc", nargs="+", help="cc address(es)")
    ap.add_argument("--from", dest="sender", help="From header (defaults to the account)")
    ap.add_argument("--subject", default="")
    ap.add_argument("--text", help="file holding the plain-text body ('-' for stdin)")
    ap.add_argument("--html", help="file holding the HTML body")
    ap.add_argument("--client-secret", help="override the OAuth client JSON")
    ap.add_argument("--dry-run", action="store_true",
                    help="print the MIME that would be sent, and send nothing")
    args = ap.parse_args()

    if args.auth:
        auth_flow(args.client_secret)
        return

    if not args.to or not args.text:
        ap.error("--to and --text are required (or use --auth)")

    text = sys.stdin.read() if args.text == "-" else open(args.text, encoding="utf-8").read()
    html = open(args.html, encoding="utf-8").read() if args.html else None
    msg = build_mime(args.to, args.subject, text, html, args.sender, args.cc)

    if args.dry_run:
        print(msg.as_string())
        return

    result = send(msg, access_token(args.client_secret))
    print(f"Sent. Message id: {result.get('id')}")


if __name__ == "__main__":
    main()
