//! Shared file-staleness tracker.
//!
//! Records the mtime of files when the agent reads them, then verifies that
//! mtime hasn't changed when an edit is attempted. Prevents the model from
//! overwriting external changes it didn't see.
//!
//! The architecture doc calls this "staleness detection".
//!
//! Usage:
//!     let tracker = Arc::new(FileTracker::new());
//!     // After read_file:
//!     tracker.mark_read(&path);
//!     // Before edit_file:
//!     tracker.check_fresh(&path)?;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// Result of a freshness check.
#[derive(Debug, Clone, PartialEq)]
pub enum Freshness {
    /// File has been read by the agent and is unchanged since then.
    Fresh,
    /// File has never been read by the agent in this session.
    NeverRead,
    /// File was read, but has been modified externally since.
    Stale {
        last_read: SystemTime,
        current: SystemTime,
    },
}

/// Thread-safe map of canonical file path → last-known mtime.
#[derive(Debug, Default)]
pub struct FileTracker {
    seen: Mutex<HashMap<PathBuf, SystemTime>>,
}

impl FileTracker {
    pub fn new() -> Self {
        Self {
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Record that the agent just read this file. Stores its current mtime.
    /// Silently ignores files that don't exist or whose mtime can't be read.
    pub fn mark_read(&self, path: &Path) {
        if let Some((key, mtime)) = key_and_mtime(path) {
            self.seen.lock().unwrap().insert(key, mtime);
        }
    }

    /// Forget tracking for a file (e.g. after the agent overwrote it itself,
    /// so the next edit shouldn't be flagged stale).
    pub fn mark_written(&self, path: &Path) {
        // After our own write, refresh mtime so subsequent edits see "fresh".
        if let Some((key, mtime)) = key_and_mtime(path) {
            self.seen.lock().unwrap().insert(key, mtime);
        }
    }

    /// Check whether a file is "fresh" relative to what the agent last saw.
    pub fn check(&self, path: &Path) -> Freshness {
        let (key, current) = match key_and_mtime(path) {
            Some(km) => km,
            None => return Freshness::NeverRead,
        };
        let map = self.seen.lock().unwrap();
        match map.get(&key) {
            None => Freshness::NeverRead,
            Some(&last_read) => {
                if mtime_eq(last_read, current) {
                    Freshness::Fresh
                } else {
                    Freshness::Stale { last_read, current }
                }
            }
        }
    }

    /// Drop all tracking state (e.g. on session reset).
    pub fn clear(&self) {
        self.seen.lock().unwrap().clear();
    }

    /// Number of files currently tracked. Mostly useful in tests.
    pub fn len(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.lock().unwrap().is_empty()
    }
}

/// Resolve a path to a canonical key and return its current mtime.
/// Falls back to the un-canonicalized path if canonicalize fails.
fn key_and_mtime(path: &Path) -> Option<(PathBuf, SystemTime)> {
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    Some((key, mtime))
}

/// Compare mtimes with millisecond tolerance — some filesystems round
/// timestamps and a same-second write should not flag stale.
fn mtime_eq(a: SystemTime, b: SystemTime) -> bool {
    match (
        a.duration_since(SystemTime::UNIX_EPOCH),
        b.duration_since(SystemTime::UNIX_EPOCH),
    ) {
        (Ok(da), Ok(db)) => {
            let am = da.as_millis();
            let bm = db.as_millis();
            am.max(bm) - am.min(bm) <= 1
        }
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::thread::sleep;
    use std::time::Duration;

    fn tmp_file(name: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("file_tracker_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn never_read_when_unmarked() {
        let path = tmp_file("never.txt", "hi");
        let t = FileTracker::new();
        assert_eq!(t.check(&path), Freshness::NeverRead);
    }

    #[test]
    fn fresh_after_mark_read() {
        let path = tmp_file("fresh.txt", "hi");
        let t = FileTracker::new();
        t.mark_read(&path);
        assert_eq!(t.check(&path), Freshness::Fresh);
    }

    #[test]
    fn stale_after_external_write() {
        let path = tmp_file("stale.txt", "v1");
        let t = FileTracker::new();
        t.mark_read(&path);

        // Wait long enough for mtime to change on most filesystems.
        sleep(Duration::from_millis(20));
        let mut f = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.write_all(b"v2 changed").unwrap();
        f.sync_all().unwrap();

        match t.check(&path) {
            Freshness::Stale { .. } => {}
            other => panic!("expected Stale, got {:?}", other),
        }
    }

    #[test]
    fn fresh_again_after_mark_written() {
        let path = tmp_file("rewrite.txt", "v1");
        let t = FileTracker::new();
        t.mark_read(&path);
        sleep(Duration::from_millis(20));
        fs::write(&path, b"v2 by agent").unwrap();
        // Without mark_written, this would now be Stale.
        t.mark_written(&path);
        assert_eq!(t.check(&path), Freshness::Fresh);
    }

    #[test]
    fn clear_drops_all_state() {
        let path = tmp_file("clr.txt", "x");
        let t = FileTracker::new();
        t.mark_read(&path);
        assert_eq!(t.len(), 1);
        t.clear();
        assert!(t.is_empty());
    }
}
