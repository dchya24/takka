//! Agent session checkpointing (P1-3).
//!
//! Memory persistence stores *messages*; an `AgentSession` stores the
//! whole recoverable run: identity, lifecycle state, model, message
//! history, and reserved slots for plan/task state. The
//! [`SessionStore`] persists them as versioned JSON documents — one
//! file per session — so a crashed run can be inspected and resumed on
//! a fresh orchestrator.
//!
//! Contract:
//! - `save` is atomic (temp file + rename): a crash mid-write never
//!   corrupts the previous checkpoint.
//! - Session ids are validated before touching the filesystem.
//! - Format version lives in the document so future migrations can
//!   branch on it.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::memory::Message;
use crate::AgenticError;

/// Bump when the on-disk format changes.
pub const SESSION_FORMAT_VERSION: u32 = 1;

/// A recoverable snapshot of one agent session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    /// On-disk format version.
    pub version: u32,
    pub session_id: String,
    /// Wire name of the orchestrator state at checkpoint time
    /// (`OrchestratorState::as_str`) — e.g. `executing_tools`,
    /// `completed`, `failed`.
    pub state: String,
    /// Model the session was running with.
    pub model: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Full conversation history (user/assistant/tool turns).
    pub messages: Vec<Message>,
    /// Skill active for the session, if any.
    #[serde(default)]
    pub active_skill: Option<String>,
    /// Plan snapshot (P2 wiring), when a planner produced one.
    #[serde(default)]
    pub plan: Option<serde_json::Value>,
    /// Task-state snapshot — todowrite items (P2 wiring).
    #[serde(default)]
    pub task_state: Option<serde_json::Value>,
    /// How many checkpoints have been written for this session.
    pub checkpoint_count: u32,
}

impl AgentSession {
    /// Start a fresh session snapshot for the given model.
    pub fn new(session_id: impl Into<String>, model: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            version: SESSION_FORMAT_VERSION,
            session_id: session_id.into(),
            state: "created".to_string(),
            model: model.into(),
            created_at: now,
            updated_at: now,
            messages: Vec::new(),
            active_skill: None,
            plan: None,
            task_state: None,
            checkpoint_count: 0,
        }
    }

    /// Record a checkpoint: refresh state + history, bump the counter.
    pub fn checkpoint(&mut self, state: &str, messages: Vec<Message>) {
        self.state = state.to_string();
        self.messages = messages;
        self.updated_at = Utc::now();
        self.checkpoint_count += 1;
    }
}

/// Lightweight listing entry — no message payloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub state: String,
    pub model: String,
    pub message_count: usize,
    pub checkpoint_count: u32,
    pub updated_at: DateTime<Utc>,
}

/// Directory-backed checkpoint store: `<root>/<session_id>.json`.
#[derive(Clone)]
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Session ids must be plain identifiers (uuid-shaped): letters,
    /// digits, and dashes only — never path separators.
    fn validate_id(id: &str) -> Result<(), AgenticError> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(AgenticError::Config(format!("invalid session id: {id:?}")));
        }
        Ok(())
    }

    fn path_for(&self, id: &str) -> Result<PathBuf, AgenticError> {
        Self::validate_id(id)?;
        Ok(self.root.join(format!("{id}.json")))
    }

    /// Persist a session atomically: write `<id>.json.tmp`, then rename
    /// over the previous checkpoint.
    pub fn save(&self, session: &AgentSession) -> Result<PathBuf, AgenticError> {
        std::fs::create_dir_all(&self.root)?;
        let path = self.path_for(&session.session_id)?;
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(session)?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// Load a session by id. Unknown ids are a `Config` error.
    pub fn load(&self, id: &str) -> Result<AgentSession, AgenticError> {
        let path = self.path_for(id)?;
        if !path.exists() {
            return Err(AgenticError::Config(format!("session not found: {id}")));
        }
        let raw = std::fs::read_to_string(&path)?;
        let session: AgentSession = serde_json::from_str(&raw)?;
        if session.version > SESSION_FORMAT_VERSION {
            return Err(AgenticError::Config(format!(
                "session {id} was written by a newer format (v{} > v{SESSION_FORMAT_VERSION})",
                session.version
            )));
        }
        Ok(session)
    }

    /// List all stored sessions, newest first.
    pub fn list(&self) -> Result<Vec<SessionSummary>, AgenticError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let raw = match std::fs::read_to_string(&path) {
                Ok(raw) => raw,
                // Skip unreadable/corrupt files rather than failing the
                // whole listing.
                Err(_) => continue,
            };
            let session: AgentSession = match serde_json::from_str(&raw) {
                Ok(s) => s,
                Err(_) => continue,
            };
            out.push(SessionSummary {
                session_id: session.session_id,
                state: session.state,
                model: session.model,
                message_count: session.messages.len(),
                checkpoint_count: session.checkpoint_count,
                updated_at: session.updated_at,
            });
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        Ok(out)
    }

    /// Delete a session. Returns whether anything was removed.
    pub fn delete(&self, id: &str) -> Result<bool, AgenticError> {
        let path = self.path_for(id)?;
        if path.exists() {
            std::fs::remove_file(&path)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_session(id: &str) -> AgentSession {
        let mut s = AgentSession::new(id, "test-model");
        s.checkpoint(
            "executing_tools",
            vec![Message::user("hello"), Message::assistant("hi")],
        );
        s
    }

    #[test]
    fn save_load_roundtrip_preserves_fields() {
        let dir = std::env::temp_dir().join(format!("sess_rt_{}", uuid::Uuid::new_v4()));
        let store = SessionStore::new(&dir);

        let session = sample_session("roundtrip-1");
        store.save(&session).unwrap();

        let loaded = store.load("roundtrip-1").unwrap();
        assert_eq!(loaded.version, SESSION_FORMAT_VERSION);
        assert_eq!(loaded.session_id, "roundtrip-1");
        assert_eq!(loaded.state, "executing_tools");
        assert_eq!(loaded.model, "test-model");
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].content, "hello");
        assert_eq!(loaded.checkpoint_count, 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn checkpoint_bumps_counter_and_updates_state() {
        let mut s = AgentSession::new("s", "m");
        assert_eq!(s.checkpoint_count, 0);
        s.checkpoint("waiting_for_model", vec![]);
        assert_eq!(s.checkpoint_count, 1);
        assert_eq!(s.state, "waiting_for_model");
        s.checkpoint("completed", vec![Message::user("x")]);
        assert_eq!(s.checkpoint_count, 2);
        assert_eq!(s.state, "completed");
        assert_eq!(s.messages.len(), 1);
    }

    #[test]
    fn list_is_newest_first_and_skips_corrupt_files() {
        let dir = std::env::temp_dir().join(format!("sess_ls_{}", uuid::Uuid::new_v4()));
        let store = SessionStore::new(&dir);

        let mut older = sample_session("older");
        older.updated_at -= chrono::Duration::seconds(30);
        store.save(&older).unwrap();
        store.save(&sample_session("newer")).unwrap();

        // Corrupt + foreign files are ignored.
        std::fs::write(dir.join("broken.json"), "{not json").unwrap();
        std::fs::write(dir.join("notes.txt"), "unrelated").unwrap();

        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].session_id, "newer");
        assert_eq!(list[1].session_id, "older");
        assert_eq!(list[0].message_count, 2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn delete_removes_and_reports_missing() {
        let dir = std::env::temp_dir().join(format!("sess_dl_{}", uuid::Uuid::new_v4()));
        let store = SessionStore::new(&dir);
        store.save(&sample_session("gone")).unwrap();

        assert!(store.delete("gone").unwrap());
        assert!(!store.delete("gone").unwrap());
        assert!(store.load("gone").is_err());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_missing_is_config_error() {
        let dir = std::env::temp_dir().join(format!("sess_ms_{}", uuid::Uuid::new_v4()));
        let store = SessionStore::new(&dir);
        let err = store.load("nope").unwrap_err();
        assert!(err.to_string().contains("not found"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_traversal_is_rejected() {
        let dir = std::env::temp_dir().join(format!("sess_pt_{}", uuid::Uuid::new_v4()));
        let store = SessionStore::new(&dir);
        assert!(store.load("../escape").is_err());
        assert!(store.load("a/b").is_err());
        assert!(store.load("").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_save_leaves_no_tmp_files() {
        let dir = std::env::temp_dir().join(format!("sess_tm_{}", uuid::Uuid::new_v4()));
        let store = SessionStore::new(&dir);
        store.save(&sample_session("atomic")).unwrap();
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, vec!["atomic.json".to_string()]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
