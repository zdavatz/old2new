//! Source the "latest shorts" list for the PDF export. Primary source is
//! the @gozipa YouTube channel (the Enhanced 4K shorts channel) fetched
//! live via yt-dlp; if that fails (offline, yt-dlp missing) we fall back to
//! the app's own upload history so the button still produces something.

use crate::history;
use crate::pdf::PdfRow;
use crate::settings::Settings;
use std::process::Command;

/// The channel whose newest uploads are "the latest shorts created".
pub const SHORTS_CHANNEL: &str = "https://www.youtube.com/@gozipa/videos";

/// Build the rows for the PDF: the `n` newest channel uploads, or — if the
/// channel can't be reached — the `n` newest history entries. Returns the
/// rows plus a short human note about which source was used.
pub fn latest_rows(settings: &Settings, n: usize) -> (Vec<PdfRow>, String) {
    match fetch_channel_latest(settings, n) {
        Ok(rows) if !rows.is_empty() => {
            let note = format!("{} from @gozipa", rows.len());
            (rows, note)
        }
        Ok(_) | Err(_) => {
            let rows: Vec<PdfRow> = history::load_all()
                .into_iter()
                .filter(|e| !e.url.trim().is_empty())
                .take(n)
                .map(|e| {
                    let mut meta: Vec<String> = Vec::new();
                    if !e.start.trim().is_empty() || !e.end.trim().is_empty() {
                        meta.push(format!("{}\u{2013}{}", e.start.trim(), e.end.trim()));
                    }
                    if !e.privacy.trim().is_empty() {
                        meta.push(e.privacy.trim().to_string());
                    }
                    if !e.timestamp.trim().is_empty() {
                        meta.push(e.timestamp.trim().to_string());
                    }
                    PdfRow {
                        title: e.title.trim().to_string(),
                        url: e.url.trim().to_string(),
                        meta: meta.join("   \u{00b7}   "),
                    }
                })
                .collect();
            let note = format!("{} from upload history (channel unavailable)", rows.len());
            (rows, note)
        }
    }
}

/// One fast yt-dlp `--flat-playlist` call returning the `n` newest videos
/// as `id \t title \t duration` lines. No per-video description fetch — that
/// would be `n` extra network round-trips and JS-challenge solves.
fn fetch_channel_latest(settings: &Settings, n: usize) -> Result<Vec<PdfRow>, String> {
    let mut cmd = Command::new("yt-dlp");
    cmd.args([
        "--flat-playlist",
        "--playlist-end",
        &n.to_string(),
        "--print",
        "%(id)s\t%(title)s\t%(duration)s",
    ]);
    if !settings.cookies_browser.trim().is_empty() {
        cmd.arg("--cookies-from-browser").arg(settings.cookies_browser.trim());
    }
    cmd.arg(SHORTS_CHANNEL);

    let out = cmd.output().map_err(|e| format!("yt-dlp: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
        return Err(format!("yt-dlp failed: {last}"));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let rows: Vec<PdfRow> = stdout
        .lines()
        .filter_map(|line| {
            let mut f = line.splitn(3, '\t');
            let id = f.next()?.trim();
            let title = f.next().unwrap_or("").trim();
            let dur = f.next().unwrap_or("").trim();
            if id.is_empty() {
                return None;
            }
            Some(PdfRow {
                title: title.to_string(),
                url: format!("https://www.youtube.com/watch?v={id}"),
                meta: fmt_duration(dur),
            })
        })
        .collect();
    Ok(rows)
}

/// yt-dlp prints duration as whole seconds (or "NA"); render "H:MM:SS" /
/// "M:SS". Anything unparseable yields an empty meta line.
fn fmt_duration(secs: &str) -> String {
    let Ok(total) = secs.parse::<f64>() else {
        return String::new();
    };
    let total = total.round() as i64;
    if total <= 0 {
        return String::new();
    }
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}
