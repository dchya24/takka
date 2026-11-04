//! Disk persistence for `Memory` sessions.
//!
//! Sessions are stored as JSON under `~/.config/agentic/sessions/` by
//! default; the directory can be overridden per-call (for tests) or
//! globally via `MemoryConfig::persist_dir`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::store::Memory;

impl Memory {
    /// Get the default persist directory.
    pub(super) fn persist_dir(&self) -> PathBuf {
        if let Some(ref dir) = self.config.persist_dir {
            PathBuf::from(dir)
        } else {
            default_persist_dir()
        }
    }

    /// Save this memory to disk.
    pub fn persist(&self) -> io::Result<PathBuf> {
        let dir = self.persist_dir();
        fs::create_dir_all(&dir)?;

        let filename = format!("{}.json", self.session.id);
        let path = dir.join(filename);

        let json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;

        fs::write(&path, json)?;
        Ok(path)
    }

    /// Load a session from disk by session ID.
    pub fn load(session_id: &str) -> io::Result<Self> {
        Self::load_from_dir(session_id, None)
    }

    /// Load a session from a specific directory.
    pub fn load_from_dir(session_id: &str, dir: Option<&Path>) -> io::Result<Self> {
        let dir = dir.map(PathBuf::from).unwrap_or_else(default_persist_dir);

        let path = dir.join(format!("{}.json", session_id));
        let content = fs::read_to_string(&path)?;

        let mut memory: Memory = serde_json::from_str(&content)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // Rebuild pinned_ids from messages
        memory.pinned_ids = memory
            .messages
            .iter()
            .filter(|m| m.pinned)
            .map(|m| m.id.clone())
            .collect();

        Ok(memory)
    }

    /// List all saved session IDs in the persist directory.
    pub fn list_sessions() -> io::Result<Vec<String>> {
        Self::list_sessions_from_dir(None)
    }

    /// List all saved session IDs from a specific directory.
    pub fn list_sessions_from_dir(dir: Option<&Path>) -> io::Result<Vec<String>> {
        let dir = dir.map(PathBuf::from).unwrap_or_else(default_persist_dir);

        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions: Vec<String> = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                if let Some(stem) = path.file_stem() {
                    sessions.push(stem.to_string_lossy().to_string());
                }
            }
        }

        sessions.sort();
        Ok(sessions)
    }

    /// Delete a saved session file.
    pub fn delete_session(session_id: &str) -> io::Result<()> {
        Self::delete_session_in_dir(session_id, None)
    }

    /// Delete a saved session file from a specific directory.
    pub fn delete_session_in_dir(session_id: &str, dir: Option<&Path>) -> io::Result<()> {
        let dir = dir.map(PathBuf::from).unwrap_or_else(default_persist_dir);
        let path = dir.join(format!("{}.json", session_id));
        if path.exists() {
            fs::remove_file(path)
        } else {
            Ok(())
        }
    }
}

fn default_persist_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".config")
        .join("agentic")
        .join("sessions")
}
