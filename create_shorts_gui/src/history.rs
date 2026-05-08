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
