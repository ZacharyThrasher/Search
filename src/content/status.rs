//! The indexer's progress, as a small JSON file the popup (tray tooltip) and
//! the CLI read. Written by `searchi` every couple of seconds, replaced
//! atomically; anything older than `STALE_AFTER` means the indexer is not running.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const STALE_AFTER: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Progress {
    /// "starting", "waiting for the helper", "listing", "indexing", "idle".
    pub phase: String,
    /// Why crawling is paused ("you are active", "on battery"), if it is.
    pub paused: String,
    /// Files with text in the index.
    pub indexed: u64,
    /// Files looked at and left out (binary, too big, empty, failed).
    pub skipped: u64,
    pub failed: u64,
    /// Files the crawl has found that need work, so far, and how many are done.
    pub queued: u64,
    pub done: u64,
    /// Files waiting for the live-update debounce.
    pub pending_events: u64,
    /// Passages (documents) in the index and its size on disk.
    pub passages: u64,
    pub index_bytes: u64,
    /// Seconds spent crawling since the indexer started.
    pub crawl_secs: u64,
    /// Unix milliseconds of this update, and of the indexer's start.
    pub updated_ms: u64,
    pub started_ms: u64,
    pub pid: u32,
    /// Last problem worth showing, if any.
    pub note: String,
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

pub fn path(dir: &Path) -> PathBuf {
    dir.join("status.json")
}

pub fn write(dir: &Path, p: &Progress) -> std::io::Result<()> {
    let tmp = dir.join("status.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(p).map_err(std::io::Error::other)?)?;
    std::fs::rename(&tmp, path(dir))
}

/// The latest progress, or `None` if there is no file or it cannot be read.
pub fn read(dir: &Path) -> Option<Progress> {
    serde_json::from_slice(&std::fs::read(path(dir)).ok()?).ok()
}

fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl Progress {
    pub fn is_running(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.updated_ms) < STALE_AFTER.as_millis() as u64
    }

    /// Everything not yet done among the files found so far.
    pub fn remaining(&self) -> u64 {
        self.queued.saturating_sub(self.done)
    }

    /// One line for a tooltip or a status command.
    pub fn summary(&self, now_ms: u64) -> String {
        if !self.is_running(now_ms) {
            return if self.indexed > 0 {
                format!("Content indexer not running ({} files indexed)", commas(self.indexed))
            } else {
                "Content indexer not running".into()
            };
        }
        let files = commas(self.indexed);
        match self.phase.as_str() {
            "idle" => format!("Content: {files} files indexed"),
            "indexing" | "listing" if !self.paused.is_empty() => {
                format!("Content: {files} files indexed, {} to go (paused: {})", commas(self.remaining()), self.paused)
            }
            "listing" => format!("Content: {files} files indexed, scanning for more"),
            "indexing" => format!("Content: {files} files indexed, {} to go", commas(self.remaining())),
            other => format!("Content: {other}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_summarises() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Progress { phase: "indexing".into(), indexed: 12_345, queued: 20_000, done: 15_000, updated_ms: now_ms(), ..Default::default() };
        write(dir.path(), &p).unwrap();
        let back = read(dir.path()).unwrap();
        assert_eq!(back, p);
        assert_eq!(back.summary(now_ms()), "Content: 12,345 files indexed, 5,000 to go");
        p.paused = "you are active".into();
        assert!(p.summary(now_ms()).ends_with("(paused: you are active)"));
        p.phase = "idle".into();
        assert_eq!(p.summary(now_ms()), "Content: 12,345 files indexed");
        // Old updates mean the indexer is gone.
        assert!(p.summary(now_ms() + 60_000).starts_with("Content indexer not running (12,345"));
    }

    #[test]
    fn missing_or_damaged_files_read_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path()).is_none());
        std::fs::write(path(dir.path()), b"{not json").unwrap();
        assert!(read(dir.path()).is_none());
        // Unknown and missing fields are fine.
        std::fs::write(path(dir.path()), br#"{"phase":"idle","extra":1}"#).unwrap();
        assert_eq!(read(dir.path()).unwrap().phase, "idle");
    }
}
