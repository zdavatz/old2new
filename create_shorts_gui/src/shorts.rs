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

/// Where the fetched shorts list is cached so the preview modal can show
/// something instantly while a fresh fetch runs in the background.
fn cache_path() -> std::path::PathBuf {
    crate::settings::config_dir().join("shorts_cache.json")
}

/// Load the previously-cached shorts list (newest-first), if any. Returns an
/// empty vec on a missing/corrupt cache.
pub fn load_cache() -> Vec<PdfRow> {
    match std::fs::read_to_string(cache_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Persist the fetched shorts list for instant display next time.
pub fn save_cache(rows: &[PdfRow]) {
    if let Ok(s) = serde_json::to_string(rows) {
        let _ = std::fs::write(cache_path(), s);
    }
}

/// Build the rows for the PDF: the `n` newest channel uploads, or — if the
/// channel can't be reached — the `n` newest history entries. Returns the
/// rows plus a short human note about which source was used.
pub fn latest_rows(settings: &Settings, n: usize) -> (Vec<PdfRow>, String) {
    rows_from(settings, Some(n))
}

/// Build the rows for the PDF from the **full** history: every channel upload
/// (no count limit), or — if the channel can't be reached — every history
/// entry. Used by the preview modal so the user can pick which shorts go in.
pub fn all_rows(settings: &Settings) -> (Vec<PdfRow>, String) {
    rows_from(settings, None)
}

/// Number of newest videos re-fetched on an incremental refresh — enough to
/// catch anything uploaded since the last open without paginating the whole
/// channel. `--playlist-end` makes yt-dlp stop after this many, so it's fast.
const REFRESH_HEAD: usize = 25;

/// Incremental refresh for the preview modal. With a non-empty `cached` list
/// we fetch only the newest `REFRESH_HEAD` videos and merge any that aren't
/// already cached to the front — much faster than re-scanning the whole
/// channel. With no cache we do the one-time full fetch. If the quick head
/// fetch fails we keep showing the cache unchanged.
pub fn refresh(settings: &Settings, cached: &[PdfRow]) -> (Vec<PdfRow>, String) {
    if cached.is_empty() {
        return all_rows(settings);
    }
    match fetch_channel_latest(settings, Some(REFRESH_HEAD)) {
        Ok(head) if !head.is_empty() => {
            let head_urls: std::collections::HashSet<String> =
                head.iter().map(|r| r.url.trim().to_string()).collect();
            // Freshly-fetched newest first (also picks up edited titles), then
            // the older cached entries that weren't in the head fetch.
            let mut merged = head;
            for c in cached {
                if !head_urls.contains(c.url.trim()) {
                    merged.push(c.clone());
                }
            }
            let new_count = merged.len().saturating_sub(cached.len());
            let note = if new_count > 0 {
                format!("{} from @gozipa (+{} new)", merged.len(), new_count)
            } else {
                format!("{} from @gozipa", merged.len())
            };
            (merged, note)
        }
        _ => (cached.to_vec(), format!("{} cached (channel unreachable)", cached.len())),
    }
}

/// Shared implementation: `limit = Some(n)` for the newest n, `None` for all.
fn rows_from(settings: &Settings, limit: Option<usize>) -> (Vec<PdfRow>, String) {
    match fetch_channel_latest(settings, limit) {
        Ok(rows) if !rows.is_empty() => {
            let note = format!("{} from @gozipa", rows.len());
            (rows, note)
        }
        Ok(_) | Err(_) => {
            let mut iter: Box<dyn Iterator<Item = _>> = Box::new(
                history::load_all()
                    .into_iter()
                    .filter(|e| !e.url.trim().is_empty()),
            );
            if let Some(n) = limit {
                iter = Box::new(iter.take(n));
            }
            let rows: Vec<PdfRow> = iter
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

/// One fast yt-dlp `--flat-playlist` call returning the newest videos as
/// `id \t title \t duration` lines. `limit = Some(n)` caps to the newest n via
/// `--playlist-end`; `None` fetches the whole channel. No per-video
/// description fetch — that would be N extra network round-trips.
fn fetch_channel_latest(settings: &Settings, limit: Option<usize>) -> Result<Vec<PdfRow>, String> {
    let mut cmd = Command::new("yt-dlp");
    cmd.args(["--flat-playlist"]);
    if let Some(n) = limit {
        cmd.arg("--playlist-end").arg(n.to_string());
    }
    cmd.args([
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
