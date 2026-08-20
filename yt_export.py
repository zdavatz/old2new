#!/usr/bin/env python3
"""yt_export.py — export our own YouTube uploads via Google's Data Portability API.

Why this exists: YouTube now 403s a download once it exceeds a per-IP byte
allowance (see README "YouTube 403 bot-detection & the PO-token provider"), which
makes pulling our own videos back out with yt-dlp unreliable. The Data Portability
API is the supported way for an account to export its own uploads — no bot
detection, no PO token, no byte cap — and it returns the *original* uploaded file
rather than a transcode. Feed the result to li_push and posting to LinkedIn/X
never has to touch YouTube's download path again.

Deliberately dependency-free (stdlib only), like the other OAuth tools here, so it
runs on a bare machine without a pip install.

Usage:
    ./yt_export.py --auth                  # one-time: grant the export scopes
    ./yt_export.py                         # initiate + poll + download
    ./yt_export.py --initiate              # just start the job
    ./yt_export.py --poll                  # poll the saved job until COMPLETE
    ./yt_export.py --download              # download a COMPLETE job's archives
    ./yt_export.py --reset                 # release one-time access for a re-export

Setup (once, in the Google Cloud console — see README):
    1. Enable the "Data Portability API" in the project.
    2. Create an OAuth client of type "Desktop app", download the JSON to
       client_secret_dataportability.json in this directory.
    3. Add the account that owns the channel as a test user on the consent screen.

Note the export is authorized by whichever Google account you log in as, and
covers that account's own uploads — so sign in as the channel owner.
"""

import argparse
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

HERE = os.path.dirname(os.path.abspath(__file__))
CLIENT_SECRET = os.path.join(HERE, "client_secret_dataportability.json")
TOKEN_FILE = os.path.join(HERE, "dataportability_token.json")
STATE_FILE = os.path.join(HERE, "yt_export_state.json")
DEFAULT_OUTDIR = os.path.join(HERE, "exports")

API = "https://dataportability.googleapis.com/v1"
AUTH_URI = "https://accounts.google.com/o/oauth2/v2/auth"
TOKEN_URI = "https://oauth2.googleapis.com/token"

# 8091 and 8092 are taken by tk_push and li_push respectively.
CALLBACK_PORT = 8093
REDIRECT_URI = f"http://localhost:{CALLBACK_PORT}/"

# Resource groups holding uploaded video media. Public is the useful one for us —
# the Enhanced 4K shorts are public — but unlisted/private are available too.
DEFAULT_RESOURCES = ["youtube.public_videos"]

# Terminal states, per the archive job state enum.
DONE_STATES = {"COMPLETE", "FAILED", "CANCELLED", "EXPIRED"}


def scope_for(resource):
    """Resource groups map 1:1 onto OAuth scopes."""
    return f"https://www.googleapis.com/auth/dataportability.{resource}"


def read_json(path):
    with open(path) as fh:
        return json.load(fh)


def write_json(path, data):
    with open(path, "w") as fh:
        json.dump(data, fh, indent=2)
    # Tokens are credentials; keep them off other accounts on the machine.
    os.chmod(path, 0o600)


def load_client():
    if not os.path.exists(CLIENT_SECRET):
        sys.exit(
            f"ERROR: {os.path.basename(CLIENT_SECRET)} not found.\n"
            "Create an OAuth client of type 'Desktop app' in the Google Cloud\n"
            "console, enable the Data Portability API, and save the JSON there."
        )
    data = read_json(CLIENT_SECRET)
    # Desktop clients land under "installed"; accept "web" too so a re-used client works.
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


def api_call(path, token, payload=None):
    """GET when payload is None, otherwise POST JSON."""
    url = f"{API}/{path}"
    data = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request(url, data=data, method="POST" if data else "GET")
    req.add_header("Authorization", f"Bearer {token}")
    if data:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req) as resp:
            raw = resp.read()
            return json.loads(raw) if raw else {}
    except urllib.error.HTTPError as err:
        detail = err.read().decode(errors="replace")
        sys.exit(f"ERROR: {err.code} from {path}\n{detail}")


# --------------------------------------------------------------------------- auth


class _CallbackHandler(http.server.BaseHTTPRequestHandler):
    """Catches the single OAuth redirect and hands the code back to the main thread."""

    result = {}

    def do_GET(self):  # noqa: N802 - name fixed by BaseHTTPRequestHandler
        query = urllib.parse.urlparse(self.path).query
        params = urllib.parse.parse_qs(query)
        _CallbackHandler.result = {k: v[0] for k, v in params.items()}
        ok = "code" in _CallbackHandler.result
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.end_headers()
        msg = "Authorized. You can close this tab." if ok else "Authorization failed."
        self.wfile.write(f"<html><body><h3>{msg}</h3></body></html>".encode())

    def log_message(self, *args):
        pass  # keep the console clean


def auth_flow(resources):
    client_id, client_secret = load_client()
    scopes = " ".join(scope_for(r) for r in resources)
    state = secrets.token_urlsafe(16)

    params = {
        "client_id": client_id,
        "redirect_uri": REDIRECT_URI,
        "response_type": "code",
        "scope": scopes,
        "state": state,
        # A refresh token only comes back with offline access, and only reliably
        # when consent is forced.
        "access_type": "offline",
        "prompt": "consent",
    }
    url = f"{AUTH_URI}?{urllib.parse.urlencode(params)}"

    server = http.server.HTTPServer(("localhost", CALLBACK_PORT), _CallbackHandler)
    threading.Thread(target=server.handle_request, daemon=True).start()

    print("Opening browser for Google consent...")
    print("Sign in as the account that owns the channel you want to export.")
    print(f"If nothing opens, visit:\n{url}\n")
    webbrowser.open(url)

    deadline = time.time() + 300
    while not _CallbackHandler.result and time.time() < deadline:
        time.sleep(0.5)
    server.server_close()

    result = _CallbackHandler.result
    if "code" not in result:
        sys.exit(f"ERROR: no authorization code received ({result.get('error', 'timed out')})")
    if result.get("state") != state:
        sys.exit("ERROR: OAuth state mismatch — aborting.")

    token = post_form(
        TOKEN_URI,
        {
            "code": result["code"],
            "client_id": client_id,
            "client_secret": client_secret,
            "redirect_uri": REDIRECT_URI,
            "grant_type": "authorization_code",
        },
    )
    token["obtained_at"] = int(time.time())
    token["scopes"] = resources
    write_json(TOKEN_FILE, token)
    print(f"Token saved to {os.path.basename(TOKEN_FILE)}")
    print("NB: the archive must be initiated within 24h of this authorization.")


def access_token():
    if not os.path.exists(TOKEN_FILE):
        sys.exit("ERROR: not authorized yet — run --auth first.")
    token = read_json(TOKEN_FILE)

    fresh_until = token.get("obtained_at", 0) + token.get("expires_in", 3600) - 60
    if time.time() < fresh_until:
        return token["access_token"]

    if "refresh_token" not in token:
        sys.exit("ERROR: token expired and no refresh_token stored — run --auth again.")
    client_id, client_secret = load_client()
    refreshed = post_form(
        TOKEN_URI,
        {
            "refresh_token": token["refresh_token"],
            "client_id": client_id,
            "client_secret": client_secret,
            "grant_type": "refresh_token",
        },
    )
    # A refresh response omits the refresh_token; keep the one we already have.
    token.update(refreshed)
    token["obtained_at"] = int(time.time())
    write_json(TOKEN_FILE, token)
    return token["access_token"]


# ------------------------------------------------------------------------- export


def load_state():
    return read_json(STATE_FILE) if os.path.exists(STATE_FILE) else {}


def initiate(resources):
    token = access_token()
    resp = api_call("portabilityArchive:initiate", token, {"resources": resources})
    job_id = resp.get("archiveJobId")
    if not job_id:
        sys.exit(f"ERROR: no archiveJobId in response: {resp}")
    state = load_state()
    state.update(
        {
            "archive_job_id": job_id,
            "access_type": resp.get("accessType", ""),
            "resources": resources,
            "initiated_at": int(time.time()),
        }
    )
    write_json(STATE_FILE, state)
    print(f"Archive job started: {job_id}")
    print(f"Access type: {resp.get('accessType', 'unknown')}")
    return job_id


def job_state(job_id):
    token = access_token()
    return api_call(f"archiveJobs/{job_id}/portabilityArchiveState", token)


def poll(job_id, interval, max_wait):
    """Poll until the job reaches a terminal state, returning the final response.

    Google says an archive can take minutes, hours, or up to seven days, and asks
    that state be checked no more than every five minutes. We start tighter than
    that because small archives really do finish quickly, then back off.
    """
    print(f"Polling job {job_id} (Ctrl-C to stop; state is saved and resumable)...")
    started = time.time()
    wait = min(30, interval)
    while True:
        resp = job_state(job_id)
        state = resp.get("state", "UNKNOWN")
        elapsed = int(time.time() - started)
        print(f"  [{elapsed // 60}m{elapsed % 60:02d}s] state={state}")
        if state in DONE_STATES:
            if state == "COMPLETE":
                st = load_state()
                st["urls"] = resp.get("urls", [])
                st["export_time"] = resp.get("exportTime", "")
                st["completed_at"] = int(time.time())
                write_json(STATE_FILE, st)
            return resp
        if max_wait and time.time() - started > max_wait:
            print(f"Giving up after {max_wait}s — the job is still running.")
            print(f"Resume later with: ./yt_export.py --poll --job-id {job_id}")
            return resp
        time.sleep(wait)
        wait = min(interval, wait * 2)  # ease off toward the requested interval


def download(urls, outdir):
    """Fetch each signed URL into outdir.

    The signed URLs expire six hours after the job completes; if one has gone
    stale the caller should re-poll to mint fresh ones rather than retrying here.
    """
    os.makedirs(outdir, exist_ok=True)
    written = []
    for idx, url in enumerate(urls, 1):
        name = os.path.basename(urllib.parse.urlparse(url).path) or f"archive_{idx}"
        dest = os.path.join(outdir, name)
        print(f"[{idx}/{len(urls)}] {name}")
        try:
            with urllib.request.urlopen(url) as resp, open(dest, "wb") as out:
                total = int(resp.headers.get("Content-Length", 0))
                done = 0
                while chunk := resp.read(1 << 20):
                    out.write(chunk)
                    done += len(chunk)
                    if total:
                        pct = done * 100 // total
                        print(f"\r      {pct:3d}%  {done / 1e6:.1f}/{total / 1e6:.1f} MB",
                              end="", flush=True)
                print()
        except urllib.error.HTTPError as err:
            if err.code in (400, 403):
                print(f"      signed URL rejected ({err.code}) — it likely expired.")
                print("      Re-run with --poll to mint fresh URLs, then --download.")
                return written
            raise
        written.append(dest)
    return written


def reset_authorization():
    """Release one-time access so a later export can be initiated.

    With ACCESS_TYPE_ONE_TIME the granted resources stay 'exhausted' until this is
    called, and Google calls it automatically 14 days after the first initiate.
    """
    token = access_token()
    api_call("authorization:reset", token, {})
    print("Authorization reset — the scopes are released and a new export can start.")
    print("You will need --auth again before the next export.")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--auth", action="store_true", help="run the OAuth consent flow")
    ap.add_argument("--initiate", action="store_true", help="start an archive job")
    ap.add_argument("--poll", action="store_true", help="poll a job until it completes")
    ap.add_argument("--download", action="store_true", help="download a completed job")
    ap.add_argument("--reset", action="store_true", help="release one-time access")
    ap.add_argument("--job-id", help="job id (defaults to the saved one)")
    ap.add_argument("--resources", nargs="+", default=DEFAULT_RESOURCES,
                    help=f"resource groups to export (default: {' '.join(DEFAULT_RESOURCES)})")
    ap.add_argument("--outdir", default=DEFAULT_OUTDIR, help="where to write archives")
    ap.add_argument("--poll-interval", type=int, default=300,
                    help="seconds between state checks once backed off (default 300)")
    ap.add_argument("--max-wait", type=int, default=0,
                    help="stop polling after N seconds (0 = wait indefinitely)")
    args = ap.parse_args()

    if args.auth:
        auth_flow(args.resources)
        return

    if args.reset:
        reset_authorization()
        return

    saved = load_state()
    job_id = args.job_id or saved.get("archive_job_id")

    # No explicit step selected: run the whole pipeline.
    full_run = not (args.initiate or args.poll or args.download)

    if args.initiate or full_run:
        job_id = initiate(args.resources)

    if args.poll or full_run:
        if not job_id:
            sys.exit("ERROR: no job id — run --initiate first.")
        resp = poll(job_id, args.poll_interval, args.max_wait)
        if resp.get("state") != "COMPLETE":
            return

    if args.download or full_run:
        urls = load_state().get("urls", [])
        if not urls:
            sys.exit("ERROR: no download URLs stored — run --poll until COMPLETE first.")
        files = download(urls, args.outdir)
        print(f"\nDownloaded {len(files)} file(s) to {args.outdir}")
        if load_state().get("access_type") == "ACCESS_TYPE_ONE_TIME":
            print("This was one-time access; run --reset before starting another export.")


if __name__ == "__main__":
    main()
