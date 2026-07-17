//! Persistent upload history. One JSON object per line in
//! `<config_dir>/uploads.jsonl` — append-only so a corrupted line
//! never breaks the rest of the file. Loaded fresh each time the
//! History modal opens; we don't keep the list in memory between
//! views.

use crate::settings::config_dir;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UploadEntry {
    pub timestamp: String,
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub start: String,
    #[serde(default)]
    pub end: String,
    #[serde(default)]
    pub privacy: String,
    /// Full form snapshot at upload time (cut-out times, fades, end card,
    /// overlay, …) so the entry can be loaded back for re-editing without
    /// retyping anything. Lines written before this field existed parse as
    /// `None` and restore only source/start/end/title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form: Option<crate::FormState>,
}

pub fn history_path() -> PathBuf {
    config_dir().join("uploads.jsonl")
}

pub fn append(entry: &UploadEntry) -> std::io::Result<()> {
    let path = history_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let line = serde_json::to_string(entry).unwrap_or_default();
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{}", line)
}

/// Returns entries newest-first. Lines that fail to parse are
/// silently skipped — the file is append-only so a partial write
/// at the tail shouldn't take the whole history with it.
pub fn load_all() -> Vec<UploadEntry> {
    let path = history_path();
    let Ok(file) = std::fs::File::open(&path) else { return Vec::new() };
    let reader = BufReader::new(file);
    let mut entries: Vec<UploadEntry> = reader
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<UploadEntry>(&l).ok())
        .collect();
    entries.reverse();
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_form_snapshot_round_trips_and_old_lines_parse() {
        // Lines written before the `form` field existed must keep parsing.
        let old = r#"{"timestamp":"t","url":"u","title":"x","start":"0:01","end":"0:02"}"#;
        let e: UploadEntry = serde_json::from_str(old).expect("old line parses");
        assert!(e.form.is_none());

        // Every FormState field has a serde default, so `{}` builds one.
        let mut form: crate::FormState = serde_json::from_str("{}").unwrap();
        form.cut_middle = true;
        form.cuts = vec![crate::CutRange { from: "0:52.9".into(), till: "1:51.3".into() }];
        let entry = UploadEntry {
            timestamp: "t".into(),
            url: "u".into(),
            title: String::new(),
            source: "src".into(),
            start: "0:40.8".into(),
            end: "6:29.3".into(),
            privacy: "public".into(),
            form: Some(form),
        };
        let line = serde_json::to_string(&entry).unwrap();
        let back: UploadEntry = serde_json::from_str(&line).unwrap();
        let f = back.form.expect("form snapshot survives the round trip");
        assert!(f.cut_middle);
        assert_eq!(f.cuts.len(), 1);
        assert_eq!(f.cuts[0].from, "0:52.9");
        assert_eq!(f.cuts[0].till, "1:51.3");
    }
}
