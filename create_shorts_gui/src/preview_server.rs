//! Tiny localhost static-file server for the split preview.
//!
//! The preview page embeds the full original video via a YouTube `<iframe>`.
//! YouTube's embedded player refuses to load from a `file://` page — it needs a
//! valid HTTP origin/referrer and otherwise fails with "Fehler 153
//! (Konfiguration des Videoplayers)". So instead of opening the page as a local
//! file, we serve it (and the local edited clip next to it) over
//! `http://127.0.0.1:<port>` and open that URL. The edited `<video>` needs HTTP
//! Range support (Safari won't play otherwise), which we implement.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static PORT: OnceLock<u16> = OnceLock::new();

/// Start the server once (rooted at `dir`, the segments cache dir) and return
/// its port. Called from the UI thread only, so the `OnceLock` check/init needs
/// no extra locking. The accept loop lives on a detached background thread for
/// the app's lifetime; each connection is handled on its own thread.
pub fn ensure_started(dir: PathBuf) -> std::io::Result<u16> {
    if let Some(p) = PORT.get() {
        return Ok(*p);
    }
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let _ = PORT.set(port);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let d = dir.clone();
            std::thread::spawn(move || {
                let _ = handle(stream, &d);
            });
        }
    });
    Ok(port)
}

fn content_type(name: &str) -> &'static str {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if n.ends_with(".mp4") || n.ends_with(".m4v") {
        "video/mp4"
    } else if n.ends_with(".webm") {
        "video/webm"
    } else if n.ends_with(".mov") {
        "video/quicktime"
    } else {
        "application/octet-stream"
    }
}

/// Parse a `Range: bytes=start-end` value into an inclusive (start, end) byte
/// range clamped to `len`. Handles open-ended (`start-`) and suffix (`-n`)
/// forms. Returns None for a malformed or unsatisfiable range (caller then
/// serves the whole file).
fn parse_range(h: &str, len: u64) -> Option<(u64, u64)> {
    let s = h.trim().strip_prefix("bytes=")?;
    let mut it = s.splitn(2, '-');
    let a = it.next()?.trim();
    let b = it.next().unwrap_or("").trim();
    if len == 0 {
        return None;
    }
    if a.is_empty() {
        // suffix: bytes=-N (last N bytes)
        let n: u64 = b.parse().ok()?;
        let n = n.min(len);
        if n == 0 {
            return None;
        }
        return Some((len - n, len - 1));
    }
    let start: u64 = a.parse().ok()?;
    if start >= len {
        return None;
    }
    let end: u64 = if b.is_empty() { len - 1 } else { b.parse().ok()? };
    let end = end.min(len - 1);
    if start > end {
        return None;
    }
    Some((start, end))
}

fn write_status(stream: &mut TcpStream, code: u16, msg: &str) -> std::io::Result<()> {
    let body = msg.as_bytes();
    let header = format!(
        "HTTP/1.1 {code} {msg}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)
}

fn handle(stream: TcpStream, dir: &Path) -> std::io::Result<()> {
    let mut stream = stream;
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let raw_path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();

    // Consume headers; capture Range.
    let mut range: Option<String> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("range:") {
            // Re-read the original-case value after the colon.
            if let Some(idx) = line.find(':') {
                range = Some(line[idx + 1..].trim().to_string());
            } else {
                range = Some(v.trim().to_string());
            }
        }
    }

    // Map the URL path to a file under `dir`.
    let mut rel = raw_path.trim_start_matches('/').to_string();
    if let Some(q) = rel.find(['?', '#']) {
        rel.truncate(q);
    }
    let rel = urlencoding::decode(&rel).map(|c| c.into_owned()).unwrap_or(rel);
    let rel = if rel.is_empty() { "_preview.html".to_string() } else { rel };
    // Everything we serve is a flat file in the segments dir — reject any path
    // separator or parent ref so a crafted URL can't escape `dir`.
    if rel.contains("..") || rel.contains('/') || rel.contains('\\') {
        return write_status(&mut stream, 400, "Bad Request");
    }
    let file_path = dir.join(&rel);

    let mut f = match std::fs::File::open(&file_path) {
        Ok(f) => f,
        Err(_) => return write_status(&mut stream, 404, "Not Found"),
    };
    let len = f.metadata()?.len();
    let ct = content_type(&rel);

    match range.as_deref().and_then(|r| parse_range(r, len)) {
        Some((start, end)) => {
            let clen = end - start + 1;
            f.seek(SeekFrom::Start(start))?;
            let mut buf = vec![0u8; clen as usize];
            f.read_exact(&mut buf)?;
            let header = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Type: {ct}\r\nAccept-Ranges: bytes\r\n\
Content-Range: bytes {start}-{end}/{len}\r\nContent-Length: {clen}\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(header.as_bytes())?;
            stream.write_all(&buf)?;
        }
        None => {
            let mut buf = Vec::with_capacity(len as usize);
            f.read_to_end(&mut buf)?;
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nAccept-Ranges: bytes\r\n\
Content-Length: {len}\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(header.as_bytes())?;
            stream.write_all(&buf)?;
        }
    }
    Ok(())
}
