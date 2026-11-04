//! Session management — auto-save, list, resume conversations.
//!
//! Sessions are stored as JSON files in `~/.config/agentic/sessions/`.
//! Each session captures the full conversation history plus metadata
//! (model, provider, working directory, timestamps, cost).
//!
//! Inspired by opencode's session system but using simple JSON files
//! instead of SQLite for zero-dependency portability.

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

// ── Data types ──────────────────────────────────────────────

/// One message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMessage {
    pub role: String,
    pub content: String,
    pub timestamp: String, // RFC 3339
}

/// Metadata for a session (stored alongside messages).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub directory: String,
    pub provider: String,
    pub model: String,
    pub messages: Vec<SessionMessage>,
    pub created_at: String,
    pub updated_at: String,
    pub cost: f64,
    pub tokens_input: u32,
    pub tokens_output: u32,
    /// Tokens read from prompt cache.
    #[serde(default)]
    pub cache_read_tokens: u32,
    /// Tokens created in prompt cache (written to cache).
    #[serde(default)]
    pub cache_creation_tokens: u32,
}

/// Summary used for listing sessions (no message content).
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub directory: String,
    pub provider: String,
    pub model: String,
    pub message_count: usize,
    pub updated_at: String,
    pub created_at: String,
}

// ── Storage helpers ─────────────────────────────────────────

/// Root directory for session files.
fn sessions_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".config")
        .join("agentic")
        .join("sessions")
}

/// Ensure the sessions directory exists.
fn ensure_dir() -> Result<()> {
    let dir = sessions_dir();
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
    }
    Ok(())
}

/// Generate a short unique session ID: `ses_<timestamp>_<random>`.
fn generate_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let rand: u16 = (ts & 0xFFFF) as u16 ^ std::process::id() as u16;
    format!("ses_{:x}_{:04x}", ts, rand.wrapping_add(1))
}

/// File path for a session ID.
fn session_path(id: &str) -> PathBuf {
    sessions_dir().join(format!("{}.json", id))
}

// ── Public API ──────────────────────────────────────────────

/// Create a new empty session.
pub fn create(directory: &str, provider: &str, model: &str) -> Session {
    let now = Local::now().to_rfc3339();
    Session {
        id: generate_id(),
        title: String::new(),
        directory: directory.to_string(),
        provider: provider.to_string(),
        model: model.to_string(),
        messages: Vec::new(),
        created_at: now.clone(),
        updated_at: now,
        cost: 0.0,
        tokens_input: 0,
        tokens_output: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    }
}

/// Auto-generate a title from the first user message.
fn auto_title(content: &str) -> String {
    // Take first line, trim to 60 chars
    let first_line = content.lines().next().unwrap_or(content);
    let title = first_line.trim();
    if title.len() > 60 {
        // Truncate on a char boundary, leaving room for the 3-byte '…'
        // so the total stays within 61 bytes (60 content + ellipsis).
        let mut end = 58.min(title.len());
        while end > 0 && !title.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &title[..end])
    } else {
        title.to_string()
    }
}

/// Save (upsert) a session to disk.
pub fn save(session: &Session) -> Result<()> {
    ensure_dir()?;
    let json = serde_json::to_string_pretty(session)?;
    fs::write(session_path(&session.id), json)?;
    Ok(())
}

/// Load a session by ID.
pub fn load(id: &str) -> Result<Session> {
    let path = session_path(id);
    let content = fs::read_to_string(&path)?;
    let session: Session = serde_json::from_str(&content)?;
    Ok(session)
}

/// Delete a session by ID.
pub fn delete(id: &str) -> Result<()> {
    let path = session_path(id);
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

/// List all sessions sorted by most recently updated first.
pub fn list() -> Result<Vec<SessionSummary>> {
    ensure_dir()?;
    let dir = sessions_dir();
    let mut summaries = Vec::new();

    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(session) = serde_json::from_str::<Session>(&content) {
                summaries.push(SessionSummary {
                    id: session.id.clone(),
                    title: if session.title.is_empty() {
                        "Untitled".to_string()
                    } else {
                        session.title
                    },
                    directory: session.directory,
                    provider: session.provider,
                    model: session.model,
                    message_count: session.messages.len(),
                    updated_at: session.updated_at,
                    created_at: session.created_at,
                });
            }
        }
    }

    // Sort by updated_at descending (most recent first)
    summaries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(summaries)
}

/// Add a message to a session and auto-save.
pub fn push_message(session: &mut Session, role: &str, content: &str) {
    let msg = SessionMessage {
        role: role.to_string(),
        content: content.to_string(),
        timestamp: Local::now().to_rfc3339(),
    };

    // Auto-title from first user message
    if session.title.is_empty() && role == "user" {
        session.title = auto_title(content);
    }

    session.messages.push(msg);
    session.updated_at = Local::now().to_rfc3339();
}

/// Update cost/token counters.
pub fn update_stats(session: &mut Session, cost: f64, tokens_input: u32, tokens_output: u32) {
    session.cost += cost;
    session.tokens_input += tokens_input;
    session.tokens_output += tokens_output;
    session.updated_at = Local::now().to_rfc3339();
}

/// Update cache token counters.
pub fn update_cache_stats(
    session: &mut Session,
    cache_read_tokens: u32,
    cache_creation_tokens: u32,
) {
    session.cache_read_tokens += cache_read_tokens;
    session.cache_creation_tokens += cache_creation_tokens;
    session.updated_at = Local::now().to_rfc3339();
}

/// Find the most recent session for a given directory.
pub fn find_latest_for_dir(directory: &str) -> Result<Option<Session>> {
    let summaries = list()?;
    let dir_path = Path::new(directory);

    for summary in summaries {
        if Path::new(&summary.directory) == dir_path {
            if let Ok(session) = load(&summary.id) {
                return Ok(Some(session));
            }
        }
    }
    Ok(None)
}

/// Parse an RFC 3339 timestamp into a human-readable relative time.
pub fn format_relative_time(rfc3339: &str) -> String {
    let parsed: Option<DateTime<Local>> = DateTime::parse_from_rfc3339(rfc3339)
        .ok()
        .map(|dt| dt.with_timezone(&Local));

    match parsed {
        Some(dt) => {
            let now = Local::now();
            let diff = now.signed_duration_since(dt);
            if diff.num_seconds() < 60 {
                "just now".to_string()
            } else if diff.num_minutes() < 60 {
                format!("{}m ago", diff.num_minutes())
            } else if diff.num_hours() < 24 {
                format!("{}h ago", diff.num_hours())
            } else if diff.num_days() < 7 {
                format!("{}d ago", diff.num_days())
            } else {
                dt.format("%Y-%m-%d %H:%M").to_string()
            }
        }
        None => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_title_short() {
        assert_eq!(auto_title("hello world"), "hello world");
    }

    #[test]
    fn test_auto_title_long() {
        let long = "a".repeat(100);
        let title = auto_title(&long);
        assert!(title.len() <= 61); // 60 + '…'
        assert!(title.ends_with('…'));
    }

    #[test]
    fn test_auto_title_multiline() {
        assert_eq!(auto_title("first line\nsecond line"), "first line");
    }

    #[test]
    fn test_generate_id_format() {
        let id = generate_id();
        assert!(id.starts_with("ses_"));
        assert!(id.len() > 10);
    }
}
