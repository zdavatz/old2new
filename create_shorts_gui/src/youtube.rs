//! Direct YouTube Data API v3 client. Uses the resumable upload
//! protocol so we can stream progress to the GUI without holding the
//! whole file in memory and without dragging in `google-youtube3`.

use serde::Serialize;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Serialize)]
pub struct VideoSnippet<'a> {
    pub title: &'a str,
    pub description: &'a str,
    #[serde(rename = "categoryId")]
    pub category_id: &'a str,
}

#[derive(Serialize)]
pub struct VideoStatus<'a> {
    #[serde(rename = "privacyStatus")]
    pub privacy_status: &'a str,
    #[serde(rename = "selfDeclaredMadeForKids")]
    pub self_declared_made_for_kids: bool,
}

#[derive(Serialize)]
pub struct VideoBody<'a> {
    pub snippet: VideoSnippet<'a>,
    pub status: VideoStatus<'a>,
}

const RESUMABLE_INIT: &str =
    "https://www.googleapis.com/upload/youtube/v3/videos?uploadType=resumable&part=snippet,status";

const CHUNK: usize = 4 * 1024 * 1024;

pub fn upload_video(
    access_token: &str,
    file: &Path,
    body: &VideoBody<'_>,
    cancel: &AtomicBool,
    mut on_progress: impl FnMut(u64, u64),
) -> Result<String, String> {
    let total = std::fs::metadata(file)
        .map_err(|e| format!("stat {}: {}", file.display(), e))?
        .len();

    // Bounded per-request timeouts so a stalled connection surfaces an error
    // within ~2 min instead of reqwest's default (none). Each request here is
    // either tiny (init) or a single 4 MB chunk, so 120 s tolerates very slow
    // links (~280 kbps) while capping how long a Cancel can lag behind an
    // in-flight PUT on a dead connection. connect_timeout fails fast on a
    // captive portal / unplugged network.
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| format!("client build: {}", e))?;

    if cancel.load(Ordering::SeqCst) {
        return Err("cancelled by user".to_string());
    }

    let init_resp = client
        .post(RESUMABLE_INIT)
        .bearer_auth(access_token)
        .header("Content-Type", "application/json; charset=UTF-8")
        .header("X-Upload-Content-Type", "video/*")
        .header("X-Upload-Content-Length", total.to_string())
        .json(body)
        .send()
        .map_err(|e| format!("init request: {}", e))?;

    let status = init_resp.status();
    if !status.is_success() {
        let body = init_resp.text().unwrap_or_default();
        return Err(format!("init {}: {}", status, body));
    }
    let upload_url = init_resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .ok_or("init response missing Location header")?
        .to_string();

    let mut f = File::open(file).map_err(|e| format!("open {}: {}", file.display(), e))?;
    let mut offset: u64 = 0;
    let mut buf = vec![0u8; CHUNK];

    loop {
        // Stop between chunks when the user cancels. The in-flight 4 MB PUT
        // (if any) finishes first; the next iteration never starts.
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled by user".to_string());
        }
        let want = ((total - offset) as usize).min(CHUNK);
        f.seek(SeekFrom::Start(offset))
            .map_err(|e| format!("seek: {}", e))?;
        let mut filled = 0;
        while filled < want {
            let n = f
                .read(&mut buf[filled..want])
                .map_err(|e| format!("read: {}", e))?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }

        let end = offset + filled as u64 - 1;
        let range = format!("bytes {}-{}/{}", offset, end, total);

        let resp = client
            .put(&upload_url)
            .header("Content-Length", filled.to_string())
            .header("Content-Range", range.clone())
            .body(buf[..filled].to_vec())
            .send()
            .map_err(|e| format!("chunk PUT: {}", e))?;

        let status = resp.status();
        if status.as_u16() == 308 {
            offset += filled as u64;
            on_progress(offset, total);
            continue;
        }
        if status.is_success() {
            offset += filled as u64;
            on_progress(offset, total);
            let body: serde_json::Value = resp
                .json()
                .map_err(|e| format!("final response parse: {}", e))?;
            let id = body
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("response missing id: {}", body))?
                .to_string();
            return Ok(id);
        }
        let body = resp.text().unwrap_or_default();
        return Err(format!("chunk PUT {}: {}", status, body));
    }

    Err("upload ended without final response".to_string())
}
