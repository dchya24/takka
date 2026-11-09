//! Main TUI application state and event loop
//!
//! Handles:
//! - Input editing with cursor support
//! - `@` file search dropdown (type @ anywhere to search files)
//! - `/` command dropdown (type / at start to see commands)
//! - Arrow key navigation for both dropdowns
//! - Tab/Enter to accept, Esc to dismiss
//! - History navigation with ↑/↓

/// If the agent runs longer than this without sending any message back
/// through the channel, `process_messages` auto-resets the loading state
/// and surfaces a warning. Prevents permanent "stuck on thinking" when
/// the spawned task panics or the channel silently drops.
const LOADING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};

use super::dropdown::{Dropdown, DropdownType};
use super::ui;
use crate::commands::Commands;
use crate::session::{self, Session, SessionSummary};
use crate::widgets::progress::ProgressState;

/// Messages from async tasks to the UI
pub enum AppMessage {
    /// Streaming chunk from LLM
    StreamChunk(String),
    /// Task completed
    TaskComplete(String),
    /// Error occurred
    Error(String),
    /// Progress update
    Progress(String),
    /// Agent invoked a tool. Renders as a yellow panel before the result.
    ToolCall {
        name: String,
        arguments: serde_json::Value,
    },
    /// Agent received a tool result. Renders as a green/red notification
    /// + optional unified-diff body when the tool is edit_file/write_file.
    ToolResult {
        name: String,
        output: serde_json::Value,
        is_error: bool,
    },
    /// LLM's thinking/explanation before tool execution.
    Thought(String),
    /// Live output delta from a streaming tool.
    ToolDelta { name: String, delta: String },
    /// Provider-reported token usage for one model call.
    Usage { input: u32, output: u32 },
    /// Plan progress update from the planner agent.
    PlanProgress {
        goal: String,
        current_step: String,
        step_status: String,
        total: usize,
        completed: usize,
        failed: usize,
        pending: usize,
    },
}

/// Which text buffer the dropdown engine currently reads and writes.
///
/// Idle keys edit `input` (cursor tracked in `cursor_pos`); mid-run keys
/// edit `mid_run_input` (story 2.4, cursor always at the end). One
/// dropdown engine serves both: every call derives the target from
/// `is_loading` and views/writes the matching buffer.
#[derive(Clone, Copy)]
enum InputTarget {
    Idle,
    MidRun,
}

/// Find the byte position of the `@` trigger that the cursor is currently
/// inside, in the given buffer view.
///
/// Returns Some(byte_pos) if:
/// - There's an `@` at the start of the buffer or after whitespace
/// - There's no whitespace between the `@` and the cursor
/// - The cursor is at or after the `@`
fn find_at_trigger(text: &str, cursor: usize) -> Option<usize> {
    let before_cursor = &text[..cursor];

    // Walk backwards from cursor looking for `@`
    for (i, c) in before_cursor.char_indices().rev() {
        match c {
            '@' => {
                // `@` must be at start of buffer or preceded by whitespace
                let at_start = i == 0;
                let after_space = i > 0 && text[..i].ends_with(char::is_whitespace);
                if at_start || after_space {
                    // Check no whitespace between @ and cursor
                    let after_at = &before_cursor[i + 1..];
                    if !after_at.contains(char::is_whitespace) {
                        return Some(i);
                    }
                }
                // If `@` is not valid trigger position, stop searching
                // (we hit a non-whitespace char that isn't `@`)
                return None;
            }
            w if w.is_whitespace() => {
                // Hit whitespace going backwards — stop
                return None;
            }
            _ => {
                // Regular character, keep going back
                continue;
            }
        }
    }
    None
}

/// Application state
pub struct App {
    /// Input buffer
    pub input: String,
    /// Story 2.4: the TUI mid-run typed buffer (the run's steering
    /// contract routes it; this is only the TUI's echo).
    pub mid_run_input: String,
    /// Cursor position (byte offset) in input
    pub cursor_pos: usize,
    /// Output/conversation history
    pub messages: Vec<Message>,
    /// Current streaming response (being built)
    pub current_response: String,
    /// Is currently processing a task
    pub is_loading: bool,
    /// Progress state for animations
    pub progress: ProgressState,
    /// Dropdown state (None when hidden)
    pub dropdown: Option<Dropdown>,
    /// Scroll offset for messages
    pub scroll_offset: usize,
    /// Should quit
    pub should_quit: bool,
    /// Commands handler (shared with spawned tasks via Arc<Mutex>)
    commands: Option<Arc<Mutex<Commands>>>,
    /// Message channel receiver
    rx: Option<mpsc::UnboundedReceiver<AppMessage>>,
    /// Message channel sender (for async tasks)
    tx: mpsc::UnboundedSender<AppMessage>,
    /// Last tick for animations
    last_tick: Instant,
    /// Input history (submitted inputs)
    pub history: Vec<String>,
    /// History index (-1 = not browsing history)
    pub history_index: i32,
    /// Saved input when browsing history
    pub saved_input: String,
    // ── Session management ──
    /// Current session
    pub session: Session,
    /// Session view overlay (for `/sessions` list)
    pub session_view: Option<SessionView>,
    /// Session statistics
    pub stats: SessionStats,
    /// Image attachment display name (for status bar indicator)
    pub image_attachment: Option<String>,
    /// When the current loading state started (None when not loading).
    /// Used by the watchdog to auto-reset if the agent hangs silently.
    loading_started: Option<Instant>,
}

/// Session statistics for status display
#[derive(Clone, Debug, Default)]
pub struct SessionStats {
    pub messages_sent: u32,
    pub tool_calls: u32,
    pub tokens_input: u32,
    pub tokens_output: u32,
}

impl SessionStats {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn format_tokens(&self, n: u32) -> String {
        if n >= 1_000_000 {
            format!("{:.1}M", n as f64 / 1_000_000.0)
        } else if n >= 1_000 {
            format!("{:.1}K", n as f64 / 1_000.0)
        } else {
            format!("{}", n)
        }
    }
}

/// Session list view for `/sessions` overlay
#[derive(Clone, Debug)]
pub struct SessionView {
    pub summaries: Vec<SessionSummary>,
    pub selected: usize,
    pub filter: String,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
    pub timestamp: chrono::DateTime<chrono::Local>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Error,
    /// Tool invocation: rendered as a panel with the tool name and args.
    Tool,
    /// Tool result (success path): rendered as a green notification with
    /// optional unified-diff body for edit_file/write_file outputs.
    ToolResult,
    /// Tool result (error / blocked / skipped): rendered as a red
    /// notification with the body always shown.
    ToolError,
    /// Live output delta from a streaming tool (run_command/run_script).
    /// Rendered as indented DIM lines under the tool's panel.
    ToolActivity,
}

impl App {
    pub fn new(commands: Arc<Mutex<Commands>>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let cwd = std::env::current_dir()
            .unwrap_or_default()
            .display()
            .to_string();
        let (provider, model, _) = {
            let cmds = commands
                .try_lock()
                .expect("Commands lock not available at init");
            cmds.model_info()
        };
        let session = session::create(&cwd, &provider, &model);
        Self {
            input: String::new(),
            mid_run_input: String::new(),
            cursor_pos: 0,
            messages: vec![Message {
                role: MessageRole::System,
                content: "Welcome to Takka TUI! Type a message or use /help for commands.".into(),
                timestamp: chrono::Local::now(),
            }],
            current_response: String::new(),
            is_loading: false,
            progress: ProgressState::new(),
            dropdown: None,
            scroll_offset: 0,
            should_quit: false,
            commands: Some(commands),
            rx: Some(rx),
            tx,
            last_tick: Instant::now(),
            history: Vec::new(),
            history_index: -1,
            saved_input: String::new(),
            session,
            session_view: None,
            stats: SessionStats::default(),
            image_attachment: None,
            loading_started: None,
        }
    }

    /// Get model info from commands
    pub fn model_info(&self) -> (String, String, String) {
        match &self.commands {
            Some(cmds_arc) => {
                if let Ok(cmds) = cmds_arc.try_lock() {
                    cmds.model_info()
                } else {
                    ("none".into(), "none".into(), "-".into())
                }
            }
            None => ("none".into(), "none".into(), "-".into()),
        }
    }

    /// Save current session to disk
    fn save_session(&mut self) {
        // Push accumulated messages to session before saving
        for msg in &self.messages {
            let role = match msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                _ => continue,
            };
            // Check if message already exists in session (avoid duplicates)
            let content_preview = if msg.content.len() > 50 {
                &msg.content[..50]
            } else {
                &msg.content
            };
            let already_exists = self.session.messages.iter().any(|m| {
                m.content.len() >= 50 && m.content.starts_with(content_preview)
                    || m.content == msg.content
            });
            if !already_exists {
                session::push_message(&mut self.session, role, &msg.content);
            }
        }
        if let Err(e) = session::save(&self.session) {
            self.messages.push(Message {
                role: MessageRole::System,
                content: format!("⚠ Could not save session: {}", e),
                timestamp: chrono::Local::now(),
            });
        }
    }

    /// Start a new session (reset all state)
    async fn new_session(&mut self) {
        // Save current session first
        if !self.session.messages.is_empty() {
            self.save_session();
        }

        // Reset all state
        let (provider, model, _) = self.model_info();
        let cwd = std::env::current_dir()
            .unwrap_or_default()
            .display()
            .to_string();
        self.session = session::create(&cwd, &provider, &model);
        self.messages.clear();
        self.messages.push(Message {
            role: MessageRole::System,
            content: "New session started.".into(),
            timestamp: chrono::Local::now(),
        });
        self.current_response.clear();
        self.scroll_offset = 0;
        self.stats.reset();
        self.image_attachment = None;

        // Restart the orchestrator session through the runtime client.
        if let Some(cmds_arc) = &self.commands {
            if let Ok(mut cmds) = cmds_arc.try_lock() {
                if let Err(e) = cmds.restart_session().await {
                    self.messages.push(Message {
                        role: MessageRole::System,
                        content: format!("⚠ Session reset failed: {}", e),
                        timestamp: chrono::Local::now(),
                    });
                }
            }
        }
    }

    /// Resume a session by loading its history
    async fn resume_session(&mut self, session_id: &str) {
        match session::load(session_id) {
            Ok(loaded) => {
                // Save current session first
                if !self.session.messages.is_empty() {
                    self.save_session();
                }

                // Restore loaded session
                self.messages.clear();
                for msg in &loaded.messages {
                    let role = match msg.role.as_str() {
                        "user" => MessageRole::User,
                        "assistant" => MessageRole::Assistant,
                        "system" => MessageRole::System,
                        _ => MessageRole::System,
                    };
                    let timestamp = chrono::DateTime::parse_from_rfc3339(&msg.timestamp)
                        .map(|dt| dt.with_timezone(&chrono::Local))
                        .unwrap_or_else(|_| chrono::Local::now());
                    self.messages.push(Message {
                        role,
                        content: msg.content.clone(),
                        timestamp,
                    });
                }
                self.session = loaded;
                self.session_view = None;
                self.scroll_offset = 0;
                self.stats.reset();

                self.messages.push(Message {
                    role: MessageRole::System,
                    content: format!(
                        "Resumed session: {} ({} messages)",
                        self.session.title,
                        self.session.messages.len()
                    ),
                    timestamp: chrono::Local::now(),
                });

                // Restart the orchestrator session through the runtime.
                if let Some(cmds_arc) = &self.commands {
                    if let Ok(mut cmds) = cmds_arc.try_lock() {
                        if let Err(e) = cmds.restart_session().await {
                            self.messages.push(Message {
                                role: MessageRole::Error,
                                content: format!("⚠ Session reset failed: {}", e),
                                timestamp: chrono::Local::now(),
                            });
                        }
                    }
                }
            }
            Err(e) => {
                self.messages.push(Message {
                    role: MessageRole::Error,
                    content: format!("Failed to load session: {}", e),
                    timestamp: chrono::Local::now(),
                });
            }
        }
    }

    /// Switch model by name (partial match)
    async fn switch_model(&mut self, name: &str) {
        match &self.commands {
            Some(cmds_arc) => {
                let mut cmds = cmds_arc.lock().await;
                match cmds.switch_model(name) {
                    Ok((provider, model)) => {
                        // Update session with new model
                        self.session.provider = provider.clone();
                        self.session.model = model.clone();
                        self.messages.push(Message {
                            role: MessageRole::System,
                            content: format!("Switched to {} / {}", provider, model),
                            timestamp: chrono::Local::now(),
                        });
                    }
                    Err(e) => {
                        self.messages.push(Message {
                            role: MessageRole::Error,
                            content: format!("Failed to switch model: {}", e),
                            timestamp: chrono::Local::now(),
                        });
                    }
                }
            }
            None => {
                self.messages.push(Message {
                    role: MessageRole::Error,
                    content: "Commands not initialized".into(),
                    timestamp: chrono::Local::now(),
                });
            }
        }
    }

    // ── G-06: /search <query> ─────────────────────────────────

    /// Search conversation history for matching messages
    fn handle_search(&mut self, query: &str) {
        let query = query.trim();
        if query.is_empty() {
            self.messages.push(Message {
                role: MessageRole::System,
                content: "Usage: `/search <query>` — Search conversation history.".into(),
                timestamp: chrono::Local::now(),
            });
            return;
        }

        let query_lower = query.to_lowercase();
        let mut hits: Vec<(usize, &str, &str)> = Vec::new(); // (turn_index, role, snippet)

        for (i, msg) in self.messages.iter().enumerate() {
            let content_lower = msg.content.to_lowercase();
            if content_lower.contains(&query_lower) {
                let role_label = match msg.role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::System => "system",
                    MessageRole::Error => "error",
                    MessageRole::Tool => "tool",
                    MessageRole::ToolResult => "tool-result",
                    MessageRole::ToolError => "tool-error",
                    MessageRole::ToolActivity => "tool-activity",
                };
                hits.push((i, role_label, &msg.content));
            }
        }

        if hits.is_empty() {
            self.messages.push(Message {
                role: MessageRole::System,
                content: format!("No matches found for \"{}\"", query),
                timestamp: chrono::Local::now(),
            });
            return;
        }

        // Format top results (max 5) with surrounding context
        let mut result_text = format!(
            "**Search Results for \"{}\"** — {} match(es)\n\n",
            query,
            hits.len()
        );
        for (idx, (turn, role, content)) in hits.iter().take(5).enumerate() {
            // Extract a snippet around the match
            let content_lower = content.to_lowercase();
            let match_pos = content_lower.find(&query_lower).unwrap_or(0);
            let start = content
                .char_indices()
                .take_while(|(i, _)| *i < match_pos.saturating_sub(60))
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            let end_pos = match_pos + query.len() + 80;
            let end = content
                .char_indices()
                .take_while(|(i, _)| *i < end_pos)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(content.len());
            let snippet = &content[start..end.min(content.len())];
            let prefix = if start > 0 { "..." } else { "" };
            let suffix = if end < content.len() { "..." } else { "" };

            result_text.push_str(&format!(
                "{}. **Turn {}** ({})\n   `{}{}{}`\n\n",
                idx + 1,
                turn + 1,
                role,
                prefix,
                snippet.trim(),
                suffix
            ));
        }

        if hits.len() > 5 {
            result_text.push_str(&format!("...and {} more matches\n", hits.len() - 5));
        }

        self.messages.push(Message {
            role: MessageRole::System,
            content: result_text,
            timestamp: chrono::Local::now(),
        });
    }

    // ── G-07: /image <path> ────────────────────────────────────

    /// Attach an image for the next message (vision models)
    async fn handle_image(&mut self, path: &str) {
        let path = path.trim();
        if path.is_empty() {
            // Clear any pending attachment
            if let Some(cmds_arc) = &self.commands {
                let mut cmds = cmds_arc.lock().await;
                let count = cmds.pending_attachment_count();
                if count > 0 {
                    cmds.drain_pending_attachments();
                    self.image_attachment = None;
                    self.messages.push(Message {
                        role: MessageRole::System,
                        content: "Cleared pending image attachment.".into(),
                        timestamp: chrono::Local::now(),
                    });
                } else {
                    self.messages.push(Message {
                        role: MessageRole::System,
                        content: "Usage: `/image <path>` — Attach an image (for vision models)."
                            .into(),
                        timestamp: chrono::Local::now(),
                    });
                }
            }
            return;
        }

        // Use commands to validate and queue the image
        if let Some(cmds_arc) = &self.commands {
            let mut cmds = cmds_arc.lock().await;
            // Check vision capability first
            let caps = cmds.active_model_capabilities();
            if !caps.vision {
                self.messages.push(Message {
                    role: MessageRole::Error,
                    content: "Active model does not support image input.\nSwitch with `/models` to a vision-capable model first.".into(),
                    timestamp: chrono::Local::now(),
                });
                return;
            }

            // Load the image
            let limits = takka_core::AttachmentLimits::default();
            let result = if path.starts_with("http://")
                || path.starts_with("https://")
                || path.starts_with("data:")
            {
                takka_core::attachments::load_image_from_url(path, limits)
            } else {
                takka_core::attachments::load_image_from_path(path, limits)
            };

            match result {
                Ok(att) => {
                    let source = format!("{}", att.source);
                    let bytes = att.size_bytes;
                    let mime = att.mime_type.clone();
                    // Store filename for display in status bar
                    let display_name = std::path::Path::new(path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.to_string());
                    self.image_attachment = Some(display_name);
                    cmds.queue_attachment(att);
                    self.messages.push(Message {
                        role: MessageRole::System,
                        content: format!(
                            "📷 Attached: {} ({} bytes, {})\nImage will be sent with your next message.",
                            source,
                            bytes,
                            if mime.is_empty() { "unknown" } else { mime.as_str() }
                        ),
                        timestamp: chrono::Local::now(),
                    });
                }
                Err(e) => {
                    self.messages.push(Message {
                        role: MessageRole::Error,
                        content: format!("Failed to attach image: {}", e),
                        timestamp: chrono::Local::now(),
                    });
                }
            }
        } else {
            self.messages.push(Message {
                role: MessageRole::Error,
                content: "Commands not initialized.".into(),
                timestamp: chrono::Local::now(),
            });
        }
    }

    // ── G-08: /provider <name> ─────────────────────────────────

    /// List available providers
    async fn handle_provider_list(&mut self) {
        let providers_text = match &self.commands {
            Some(cmds_arc) => {
                let cmds = cmds_arc.lock().await;
                let config = &cmds.config_ref();
                let mut lines = String::from("**Providers:**\n\n");
                let active = config.active_provider().map(|p| p.name.clone());
                for provider in &config.providers {
                    let is_active = active.as_deref() == Some(&provider.name);
                    let marker = if is_active { " ●" } else { "" };
                    let model_count = provider.models.len();
                    lines.push_str(&format!(
                        "- **{}** ({} model(s)){}\n",
                        provider.name, model_count, marker
                    ));
                }
                lines.push_str("\nUse `/provider <name>` to switch provider.");
                lines
            }
            None => "Commands not initialized.".into(),
        };
        self.messages.push(Message {
            role: MessageRole::System,
            content: providers_text,
            timestamp: chrono::Local::now(),
        });
    }

    /// Switch to a different provider
    async fn handle_provider_switch(&mut self, name: &str) {
        // Find the default model of the target provider and switch to it
        let result = match &self.commands {
            Some(cmds_arc) => match cmds_arc.try_lock() {
                Ok(cmds) => {
                    let config = cmds.config_ref();
                    let provider = config
                        .providers
                        .iter()
                        .find(|p| p.name.to_lowercase() == name.to_lowercase());
                    match provider {
                        Some(p) => {
                            let default_model = p
                                .models
                                .first()
                                .map(|m| m.display_name.as_deref().unwrap_or(&m.model))
                                .unwrap_or(name)
                                .to_string();
                            Some(default_model)
                        }
                        None => None,
                    }
                }
                Err(_) => None,
            },
            None => None,
        };

        match result {
            Some(model_name) => {
                self.switch_model(&model_name).await;
            }
            None => {
                self.messages.push(Message {
                    role: MessageRole::Error,
                    content: format!(
                        "Provider '{}' not found. Use `/provider` to see available providers.",
                        name
                    ),
                    timestamp: chrono::Local::now(),
                });
            }
        }
    }

    // ── G-09: /mcp ─────────────────────────────────────────────

    /// Show MCP server status
    async fn handle_mcp_status(&mut self) {
        let status_text = match &self.commands {
            Some(cmds_arc) => {
                let cmds = cmds_arc.lock().await;
                let config = cmds.config_ref();
                if config.mcp_servers.is_empty() {
                    "No MCP servers configured.\nAdd servers in your config file.".to_string()
                } else {
                    let mut lines = format!("**MCP Servers ({})**\n\n", config.mcp_servers.len());
                    for (name, srv) in &config.mcp_servers {
                        let has_command = srv.command.is_some();
                        let has_url = srv.url.is_some();
                        let status = if has_command || has_url {
                            "✓ configured"
                        } else {
                            "✗ incomplete"
                        };
                        lines.push_str(&format!("- **{}** — {}", name, status));
                        if let Some(ref cmd) = srv.command {
                            lines.push_str(&format!("\n  Command: `{}`", cmd));
                            if let Some(ref args) = srv.args {
                                if !args.is_empty() {
                                    lines.push_str(&format!(" {}", args.join(" ")));
                                }
                            }
                        }
                        if let Some(ref url) = srv.url {
                            lines.push_str(&format!("\n  URL: `{}`", url));
                        }
                        lines.push('\n');
                    }
                    lines
                }
            }
            None => "Commands not initialized.".into(),
        };
        self.messages.push(Message {
            role: MessageRole::System,
            content: status_text,
            timestamp: chrono::Local::now(),
        });
    }

    // ── Skills ─────────────────────────────────────────────────

    /// List all indexed skills
    async fn handle_skill_list(&mut self) {
        match &self.commands {
            Some(cmds_arc) => {
                let cmds = cmds_arc.lock().await;
                let discovery_config: takka_core::DiscoveryConfig =
                    takka_core::DiscoveryConfig::from(&cmds.get_config().skills);
                let index = takka_core::discover_skills(&discovery_config);

                if index.is_empty() {
                    self.messages.push(Message {
                        role: MessageRole::System,
                        content: "No skills found.\n\nCreate one: `takka skill create <name>`"
                            .into(),
                        timestamp: chrono::Local::now(),
                    });
                    return;
                }

                let mut skills: Vec<_> = index.all().into_iter().collect();
                skills.sort_by(|a, b| a.name().cmp(b.name()));

                let mut lines = format!("**Indexed Skills ({})**\n\n", skills.len());
                for skill in &skills {
                    lines.push_str(&format!(
                        "- **{}** — {}\n",
                        skill.name(),
                        skill.description()
                    ));
                    lines.push_str(&format!("  *Path: `{}`*\n", skill.dir.display()));
                }

                if !index.blocked().is_empty() {
                    lines.push_str("\n**Blocked:**\n");
                    for name in index.blocked() {
                        lines.push_str(&format!("- ✗ {}\n", name));
                    }
                }

                self.messages.push(Message {
                    role: MessageRole::System,
                    content: lines,
                    timestamp: chrono::Local::now(),
                });
            }
            None => {
                self.messages.push(Message {
                    role: MessageRole::Error,
                    content: "Commands not initialized.".into(),
                    timestamp: chrono::Local::now(),
                });
            }
        }
    }

    /// Load, display, and activate a skill
    async fn handle_skill_load(&mut self, name: &str) {
        match &self.commands {
            Some(cmds_arc) => {
                let mut cmds = cmds_arc.lock().await;
                match cmds.load_and_activate_skill(name).await {
                    Ok(body) => {
                        let mut lines = format!("✅ **Skill '{}' activated** — instructions injected into agent context.\n\n", name);

                        // Preview first 10 lines
                        lines.push_str("📖 Preview:\n\n");
                        let preview: Vec<&str> = body.lines().take(10).collect();
                        for line in &preview {
                            lines.push_str(&format!("{}\n", line));
                        }
                        if body.lines().count() > 10 {
                            lines.push_str(&format!(
                                "... ({} more lines)\n",
                                body.lines().count() - 10
                            ));
                        }

                        lines.push_str("\n💡 *Now send a message — the skill instructions will be included as context.*");

                        self.messages.push(Message {
                            role: MessageRole::System,
                            content: lines,
                            timestamp: chrono::Local::now(),
                        });
                    }
                    Err(e) => {
                        self.messages.push(Message {
                            role: MessageRole::Error,
                            content: format!("Failed to activate skill '{}': {}", name, e),
                            timestamp: chrono::Local::now(),
                        });
                    }
                }
            }
            None => {
                self.messages.push(Message {
                    role: MessageRole::Error,
                    content: "Commands not initialized.".into(),
                    timestamp: chrono::Local::now(),
                });
            }
        }
    }

    // ── G-10: /plan <goal> ────────────────────────────────────

    /// Generate and display a structured plan for the given goal.
    /// Sends the goal as a special planning request to the LLM with a
    /// planning-focused system prompt, streams the response normally.
    async fn handle_plan(&mut self, goal: &str) {
        let goal = goal.trim();
        if goal.is_empty() {
            self.messages.push(Message {
                role: MessageRole::System,
                content: "Usage: `/plan <goal>` — Generate a structured plan.".into(),
                timestamp: chrono::Local::now(),
            });
            return;
        }

        // Add plan request message
        self.messages.push(Message {
            role: MessageRole::User,
            content: format!("/plan {}", goal),
            timestamp: chrono::Local::now(),
        });
        self.stats.messages_sent += 1;

        // Start loading
        self.is_loading = true;
        self.progress.start();
        self.loading_started = Some(Instant::now());
        self.current_response.clear();

        // Clone Arc<Mutex<Commands>> for the spawned task
        let commands_arc = match &self.commands {
            Some(c) => c.clone(),
            None => {
                self.is_loading = false;
                self.messages.push(Message {
                    role: MessageRole::Error,
                    content: "Commands not initialized.".into(),
                    timestamp: chrono::Local::now(),
                });
                return;
            }
        };

        let tx = self.tx.clone();
        let plan_goal = goal.to_string();

        tokio::spawn(async move {
            // Prepend a planning system instruction to guide the response
            let plan_prompt = format!(
                "You are a planning assistant. The user will describe a goal.\
                \nRespond with a structured plan:\
                \n1. **Understanding** — Briefly restate the goal\
                \n2. **Approach** — High-level strategy\
                \n3. **Steps** — Numbered, actionable steps\
                \n4. **Considerations** — Risks, edge cases, dependencies\
                \nKeep it concise and practical.\
                \n\nGoal: {}",
                plan_goal
            );

            let event_tx = tx.clone();
            let mut commands = commands_arc.lock().await;
            let result = commands
                .run_with_callbacks(
                    &plan_prompt,
                    |chunk| {
                        let _ = tx.send(AppMessage::StreamChunk(chunk.to_string()));
                    },
                    move |event| match event {
                        takka_core::Event::ToolCall {
                            tool_name,
                            arguments,
                        } => {
                            let _ = event_tx.send(AppMessage::ToolCall {
                                name: tool_name,
                                arguments,
                            });
                        }
                        takka_core::Event::ToolStarted { .. } => {
                            // ToolCall (just above) already surfaces the
                            // running marker — ToolStart is a no-op here.
                        }
                        takka_core::Event::ToolDelta {
                            tool_name, delta, ..
                        } => {
                            let _ = event_tx.send(AppMessage::ToolDelta {
                                name: tool_name,
                                delta,
                            });
                        }
                        takka_core::Event::ToolOutput {
                            tool_name, output, ..
                        } => {
                            let body = match &output {
                                serde_json::Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            let is_error = body.starts_with("Tool error")
                                || body.starts_with("Blocked:")
                                || body.starts_with("Skipped:");
                            let _ = event_tx.send(AppMessage::ToolResult {
                                name: tool_name,
                                output,
                                is_error,
                            });
                        }
                        takka_core::Event::Error { message } => {
                            let _ = event_tx.send(AppMessage::Error(message));
                        }
                        takka_core::Event::Usage {
                            prompt_tokens,
                            completion_tokens,
                            ..
                        } => {
                            let _ = event_tx.send(AppMessage::Usage {
                                input: prompt_tokens,
                                output: completion_tokens,
                            });
                        }
                        takka_core::Event::Thinking { .. } => {
                            // Skip — text was already streamed via on_chunk.
                            // A separate Thought message would duplicate content.
                        }
                        _ => {}
                    },
                )
                .await;
            tracing::debug!(
                ok = result.is_ok(),
                "handle_plan: run_with_callbacks completed"
            );
            match result {
                Ok(response) => {
                    tracing::debug!(len = response.len(), "handle_plan: sending TaskComplete");
                    if let Err(e) = tx.send(AppMessage::TaskComplete(response)) {
                        tracing::warn!(error = %e, "handle_plan: failed to send TaskComplete");
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "handle_plan: sending Error");
                    if let Err(send_err) = tx.send(AppMessage::Error(e.to_string())) {
                        tracing::warn!(error = %send_err, "handle_plan: failed to send Error");
                    }
                }
            }
            // Lock is dropped here; commands_arc stays alive for future use.
        });
    }

    // ── G-11: Context indicator state ─────────────────────────

    /// Check for context files (AGENT.md, memory.md) and return
    /// a formatted indicator string for the header bar.
    pub fn context_indicators(&self) -> String {
        let mut indicators = Vec::new();

        if let Some(cmds_arc) = &self.commands {
            if let Ok(cmds) = cmds_arc.try_lock() {
                if cmds.agent_md_path().is_some() {
                    indicators.push("📄 AGENT.md");
                }
                if cmds.memory_md_loaded() {
                    indicators.push("🧠 memory.md");
                }
            }
        }

        if indicators.is_empty() {
            String::new()
        } else {
            format!(" {} ", indicators.join(" "))
        }
    }

    /// Open session list view
    fn open_sessions(&mut self) {
        match session::list() {
            Ok(summaries) => {
                self.session_view = Some(SessionView {
                    summaries,
                    selected: 0,
                    filter: String::new(),
                });
            }
            Err(e) => {
                self.messages.push(Message {
                    role: MessageRole::Error,
                    content: format!("Failed to list sessions: {}", e),
                    timestamp: chrono::Local::now(),
                });
            }
        }
    }

    /// Handle key events in session view mode
    async fn handle_session_view_key(&mut self, key: crossterm::event::KeyEvent) {
        let view = match &mut self.session_view {
            Some(v) => v,
            None => return,
        };

        match key.code {
            KeyCode::Up if view.selected > 0 => {
                view.selected -= 1;
            }
            KeyCode::Down if view.selected + 1 < view.summaries.len() => {
                view.selected += 1;
            }
            KeyCode::Enter => {
                if let Some(summary) = view.summaries.get(view.selected) {
                    let id = summary.id.clone();
                    let _ = view; // Release borrow before resuming
                    self.resume_session(&id).await;
                }
            }
            KeyCode::Esc => {
                self.session_view = None;
            }
            _ => {}
        }
    }

    // ── Input editing ────────────────────────────────────────

    /// Insert character at cursor position and update dropdown
    pub fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor_pos, c);
        self.cursor_pos += c.len_utf8();
        self.update_dropdown();
    }

    /// Delete character before cursor
    pub fn delete_char(&mut self) {
        if self.cursor_pos > 0 {
            let prev = self.input[..self.cursor_pos]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.input.remove(prev);
            self.cursor_pos = prev;
            self.update_dropdown();
        }
    }

    /// Delete character at cursor (Delete key)
    #[allow(dead_code)]
    pub fn delete_char_forward(&mut self) {
        if self.cursor_pos < self.input.len() {
            self.input.remove(self.cursor_pos);
            // cursor_pos stays the same
            self.update_dropdown();
        }
    }

    /// Move cursor left
    pub fn move_cursor_left(&mut self) {
        if self.cursor_pos > 0 {
            self.cursor_pos = self.input[..self.cursor_pos]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    /// Move cursor right
    pub fn move_cursor_right(&mut self) {
        if self.cursor_pos < self.input.len() {
            self.cursor_pos = self.input[self.cursor_pos..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor_pos + i)
                .unwrap_or(self.input.len());
        }
    }

    /// Move cursor to start
    pub fn move_cursor_start(&mut self) {
        self.cursor_pos = 0;
    }

    /// Move cursor to end
    pub fn move_cursor_end(&mut self) {
        self.cursor_pos = self.input.len();
    }

    // ── Dropdown: @ file search & / commands ────────────────────

    /// The buffer the dropdown engine (and mid-run editing) targets.
    fn active_target(&self) -> InputTarget {
        if self.is_loading {
            InputTarget::MidRun
        } else {
            InputTarget::Idle
        }
    }

    /// (text, cursor) view of the target buffer. The mid-run buffer has no
    /// stored cursor — it is always at the end (story 2.4 contract).
    fn buffer_view(&self, target: InputTarget) -> (&str, usize) {
        match target {
            InputTarget::Idle => (self.input.as_str(), self.cursor_pos),
            InputTarget::MidRun => (self.mid_run_input.as_str(), self.mid_run_input.len()),
        }
    }

    /// Write (text, cursor) back to the target buffer. For the mid-run
    /// buffer the cursor is always the end, so only the text is stored.
    fn buffer_write_back(&mut self, target: InputTarget, text: String, cursor: usize) {
        match target {
            InputTarget::Idle => {
                self.input = text;
                self.cursor_pos = cursor;
            }
            InputTarget::MidRun => {
                self.mid_run_input = text;
            }
        }
    }

    /// Update dropdown state based on the active input buffer and cursor.
    ///
    /// The active buffer is the idle `input` while idle and the mid-run
    /// `mid_run_input` while a run is loading, so completions work in both
    /// states through the same engine (story 3.1).
    fn update_dropdown(&mut self) {
        let target = self.active_target();
        // Copy the view out so `self` can be borrowed again below.
        let (text, cursor) = {
            let (t, c) = self.buffer_view(target);
            (t.to_string(), c)
        };
        self.dropdown = Self::evaluate_dropdown(&text, cursor, || self.skill_pairs());
    }

    /// Pure dropdown evaluator over a (text, cursor) buffer view.
    ///
    /// Rules (mirrors the inline REPL, the reference contract):
    /// - If the buffer starts with `/` and cursor is in the command part →
    ///   command dropdown
    /// - `/models <partial>` or `/m <partial>` → model dropdown
    /// - `/skill <partial>` or `/sk <partial>` → skill follow-up dropdown:
    ///   re-discovered and re-filtered per keystroke so an accepted
    ///   `/skill `/`/sk ` survives narrowing (the recorded idle drift fix)
    /// - If cursor is right after or within an `@...` sequence → file
    ///   dropdown
    /// - Otherwise → no dropdown
    ///
    /// `skill_pairs` is only invoked when a skill trigger needs the
    /// discovery index, so ordinary typing never pays for discovery. An
    /// empty result closes the dropdown: it renders nothing but would
    /// otherwise intercept Enter / arrows it cannot serve (same guard as
    /// the inline REPL).
    fn evaluate_dropdown(
        text: &str,
        cursor: usize,
        skill_pairs: impl FnOnce() -> Vec<(String, String)>,
    ) -> Option<Dropdown> {
        let mut next: Option<Dropdown> = None;

        // 1) Check for `/` command trigger
        if text.starts_with('/') {
            let before_cursor = &text[..cursor];
            if !before_cursor.contains(' ') {
                // Only show command dropdown if cursor is still in the
                // command part (before any space — the command itself)
                let query = &text[1..cursor];
                next = Some(Dropdown::new(DropdownType::Command, query.to_string()));
            } else if let Some(space_pos) = before_cursor.find(' ') {
                let cmd = &text[1..space_pos];
                // 1b) Check for `/models <partial>` or `/m <partial>` model trigger
                if cmd == "models" || cmd == "m" {
                    let query = &text[space_pos + 1..cursor];
                    next = Some(Dropdown::new(DropdownType::Model, query.to_string()));
                }
                // 1c) Check for `/skill <partial>` or `/sk <partial>` skill
                //     follow-up trigger. Discovery returning nothing keeps
                //     the dropdown closed (no empty trap).
                else if cmd == "skill" || cmd == "sk" {
                    let pairs = skill_pairs();
                    if !pairs.is_empty() {
                        let query = &text[space_pos + 1..cursor];
                        next = Some(Dropdown::new_skill(query.to_string(), pairs));
                    }
                }
            }
        }

        // 2) Check for `@` file trigger
        if next.is_none() {
            if let Some(at_pos) = find_at_trigger(text, cursor) {
                let query = &text[at_pos + 1..cursor];
                next = Some(Dropdown::new(DropdownType::File, query.to_string()));
            }
        }

        // 3) Commit, closing empty dropdowns so they cannot swallow keys
        //    (e.g. a `/models <id> ` query matching no model must not eat
        //    the Enter that submits the command).
        next.filter(|dd| !dd.is_empty())
    }

    /// Accept the currently selected dropdown item and insert it into the
    /// active input buffer: the idle buffer while idle, the mid-run buffer
    /// while a run is loading (the cursor always lands at its end). Enter
    /// with an open dropdown fills, never submits/steers.
    pub fn accept_dropdown(&mut self) {
        let target = self.active_target();

        // Take the dropdown up front: every path below either closes it or
        // re-opens it (directory / skill chaining).
        let Some(dropdown) = self.dropdown.take() else {
            return;
        };
        let Some(selected_text) = dropdown.selected_item().map(|s| s.to_string()) else {
            return;
        };

        let mut is_dir = false;
        // The /skill chain re-opens the dropdown after the fill.
        let mut open_skills = false;

        let (mut buf, mut cursor) = {
            let (t, c) = self.buffer_view(target);
            (t.to_string(), c)
        };

        match dropdown.dropdown_type {
            DropdownType::Command => {
                let cmd = selected_text.trim_start_matches('/');
                if cmd == "skill" || cmd == "sk" {
                    // Fill the command, then open the skills dropdown for
                    // its argument (follow-up typing filters through
                    // update_dropdown's skill trigger).
                    buf = "/skill ".to_string();
                    cursor = buf.len();
                    open_skills = true;
                } else {
                    // Replace entire buffer with /command
                    buf = format!("/{} ", selected_text);
                    cursor = buf.len();
                }
            }
            DropdownType::File => {
                is_dir = selected_text.ends_with('/');
                // Replace from @ to cursor with the selected file path
                if let Some(at_pos) = find_at_trigger(&buf, cursor) {
                    let before_at = buf[..at_pos].to_string();
                    let after_cursor = buf[cursor..].to_string();

                    // Add trailing space for files, keep slash for dirs
                    let suffix = if is_dir {
                        // Directory — keep it for further navigation
                        ""
                    } else {
                        // File — add space after
                        " "
                    };

                    buf = format!("{}@{}{}{}", before_at, selected_text, suffix, after_cursor);
                    cursor = at_pos + 1 + selected_text.len() + suffix.len();
                }
            }
            DropdownType::Skill => {
                // Extract skill name from display and set buffer
                let skill_name = dropdown
                    .get_skill_name(&selected_text)
                    .unwrap_or_else(|| selected_text.clone());
                buf = format!("/skill {}", skill_name);
                cursor = buf.len();
            }
            DropdownType::Model => {
                // Extract model ID from display string (e.g. "gpt-4o 👁 [openai]" → "gpt-4o")
                let model_id = dropdown
                    .get_model_id(&selected_text)
                    .unwrap_or_else(|| selected_text.clone());
                // Replace from after "/models " to cursor with model ID
                let space_pos = buf.find(' ').unwrap_or(buf.len());
                let prefix = buf[..=space_pos].to_string(); // includes the space
                let after_cursor = buf[cursor..].to_string();
                buf = format!("{}{} {}", prefix, model_id, after_cursor);
                cursor = prefix.len() + model_id.len() + 1;
            }
        }

        self.buffer_write_back(target, buf, cursor);

        // After accepting:
        // - The /skill chain: open the skills dropdown for its argument
        // - Directories: re-trigger the dropdown to show contents of the
        //   selected dir
        // - Files/commands/models: close dropdown (already taken)
        if open_skills {
            self.open_skill_dropdown();
        } else if is_dir {
            self.update_dropdown();
        }
    }

    /// Move dropdown selection up
    pub fn dropdown_up(&mut self) {
        if let Some(dropdown) = &mut self.dropdown {
            dropdown.select_prev();
        }
    }

    /// Move dropdown selection down
    pub fn dropdown_down(&mut self) {
        if let Some(dropdown) = &mut self.dropdown {
            dropdown.select_next();
        }
    }

    /// Close dropdown
    pub fn close_dropdown(&mut self) {
        self.dropdown = None;
    }

    /// Discover skill (name, description) pairs from the shared Commands
    /// config. When commands are unavailable or the lock is contended
    /// (mid-run the run task may hold it), discovery runs with the default
    /// configuration.
    fn skill_pairs(&self) -> Vec<(String, String)> {
        let discovery_config = self
            .commands
            .as_ref()
            .and_then(|cmds_arc| cmds_arc.try_lock().ok())
            .map(|cmds| takka_core::DiscoveryConfig::from(&cmds.get_config().skills))
            .unwrap_or_default();
        let index = takka_core::discover_skills(&discovery_config);

        let mut skills: Vec<_> = index.all().into_iter().collect();
        skills.sort_by(|a, b| a.name().cmp(b.name()));
        skills
            .into_iter()
            .map(|s| (s.name().to_string(), s.description().to_string()))
            .collect()
    }

    /// Open a skill selection dropdown populated from the discovery index.
    ///
    /// Guard: with nothing discoverable (or discovery unavailable) the
    /// dropdown stays closed — an empty dropdown would render nothing yet
    /// still intercept Enter / arrows (the "empty dropdown trap").
    fn open_skill_dropdown(&mut self) {
        let pairs = self.skill_pairs();
        self.open_skill_dropdown_with(pairs);
    }

    /// Injected-pairs variant of [`Self::open_skill_dropdown`] so the
    /// guard is unit-testable without ambient discovery.
    fn open_skill_dropdown_with(&mut self, pairs: Vec<(String, String)>) {
        if pairs.is_empty() {
            self.dropdown = None;
            return;
        }
        self.dropdown = Some(Dropdown::new_skill(String::new(), pairs));
    }

    // ── History navigation ──────────────────────────────────

    /// Navigate history up (older entries)
    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_index == -1 {
            self.saved_input = self.input.clone();
        }
        if (self.history_index as usize) < self.history.len() - 1 {
            self.history_index += 1;
            self.input = self.history[self.history.len() - 1 - self.history_index as usize].clone();
            self.cursor_pos = self.input.len();
            self.dropdown = None;
        }
    }

    /// Navigate history down (newer entries)
    pub fn history_down(&mut self) {
        if self.history_index > 0 {
            self.history_index -= 1;
            self.input = self.history[self.history.len() - 1 - self.history_index as usize].clone();
            self.cursor_pos = self.input.len();
        } else if self.history_index == 0 {
            self.history_index = -1;
            self.input = self.saved_input.clone();
            self.cursor_pos = self.input.len();
        }
        self.dropdown = None;
    }

    // ── Scroll ──────────────────────────────────────────────

    /// Scroll messages up
    pub fn scroll_up(&mut self) {
        if self.scroll_offset > 0 {
            self.scroll_offset = self.scroll_offset.saturating_sub(3);
        }
    }

    /// Scroll messages down
    pub fn scroll_down(&mut self) {
        self.scroll_offset += 3;
    }

    // ── Submit ──────────────────────────────────────────────

    /// Submit current input
    /// Mid-run input (story 2.4): while a run is loading, typed text
    /// accumulates in a separate mid-run buffer; Enter submits it through
    /// the run's steering contract (`Commands::steer_text` — Interrupt
    /// steers, FollowUp queues) and the message log surfaces the hand-over.
    pub async fn submit_mid_run(&mut self) {
        let input = self.mid_run_input.trim().to_string();
        if input.is_empty() {
            return;
        }
        self.mid_run_input.clear();
        // The buffer is gone — the dropdown must not survive it.
        self.dropdown = None;

        let Some(commands_arc) = self.commands.clone() else {
            return;
        };
        let mode = commands_arc.lock().await.queue_mode_getter();
        commands_arc.lock().await.steer_text(input.clone()).await;

        let note = match mode {
            crate::commands::QueueMode::Interrupt => {
                format!("\u{21b3} steered in (applies at the next tool boundary): \"{input}\"")
            }
            crate::commands::QueueMode::FollowUp => {
                format!("\u{21b3} queued for the next run: \"{input}\"")
            }
        };
        self.messages.push(Message {
            role: MessageRole::System,
            content: note.clone(),
            timestamp: chrono::Local::now(),
        });
        let _ = self.tx.send(AppMessage::Progress(note));
    }

    pub fn insert_mid_run_char(&mut self, c: char) {
        self.mid_run_input.push(c);
        // Mid-run completions (story 3.1): the same dropdown engine that
        // serves the idle buffer re-evaluates over the mid-run buffer.
        self.update_dropdown();
    }

    /// Delete the last character of the mid-run buffer and re-evaluate the
    /// dropdown — correction parity with the idle buffer, so an accepted
    /// fill can be walked back (and its dropdown restored).
    pub fn delete_mid_run_char(&mut self) {
        self.mid_run_input.pop();
        self.update_dropdown();
    }

    pub async fn submit(&mut self) {
        let input = self.input.trim().to_string();
        if input.is_empty() {
            return;
        }

        // Add to history
        if !self.history.is_empty() || self.history.last() != Some(&input) {
            self.history.push(input.clone());
        }
        self.history_index = -1;

        // Clear input
        self.input.clear();
        self.cursor_pos = 0;
        self.dropdown = None;
        self.image_attachment = None;

        // Handle slash commands
        if input.starts_with('/') {
            self.handle_slash_command(&input).await;
            return;
        }

        // Add user message
        self.messages.push(Message {
            role: MessageRole::User,
            content: input.clone(),
            timestamp: chrono::Local::now(),
        });
        self.stats.messages_sent += 1;

        // Push to session
        session::push_message(&mut self.session, "user", &input);

        // Start loading
        self.is_loading = true;
        self.progress.start();
        self.loading_started = Some(Instant::now());
        self.current_response.clear();

        // Run task in background (clone Arc<Mutex<Commands>>, never take)
        let tx = self.tx.clone();
        let task = input.clone();

        let commands_arc = match &self.commands {
            Some(c) => c.clone(),
            None => {
                self.is_loading = false;
                self.messages.push(Message {
                    role: MessageRole::Error,
                    content: "Commands not initialized.".into(),
                    timestamp: chrono::Local::now(),
                });
                return;
            }
        };

        tokio::spawn(async move {
            let _ = tx.send(AppMessage::Progress("Thinking...".into()));

            // Pipe orchestrator events to the same channel so the
            // message log shows tool calls / results inline. Cloning
            // the sender into the closure is cheap (Arc internally).
            let event_tx = tx.clone();

            let mut commands = commands_arc.lock().await;
            let result = commands
                .run_with_callbacks(
                    &task,
                    |chunk| {
                        let _ = tx.send(AppMessage::StreamChunk(chunk.to_string()));
                    },
                    move |event| match event {
                        takka_core::Event::ToolCall {
                            tool_name,
                            arguments,
                        } => {
                            let _ = event_tx.send(AppMessage::ToolCall {
                                name: tool_name,
                                arguments,
                            });
                        }
                        takka_core::Event::ToolStarted { .. } => {
                            // ToolCall (just above) already surfaces the
                            // running marker — ToolStart is a no-op here.
                        }
                        takka_core::Event::ToolDelta {
                            tool_name, delta, ..
                        } => {
                            let _ = event_tx.send(AppMessage::ToolDelta {
                                name: tool_name,
                                delta,
                            });
                        }
                        takka_core::Event::ToolOutput {
                            tool_name, output, ..
                        } => {
                            let body = match &output {
                                serde_json::Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            let is_error = body.starts_with("Tool error")
                                || body.starts_with("Blocked:")
                                || body.starts_with("Skipped:");
                            let _ = event_tx.send(AppMessage::ToolResult {
                                name: tool_name,
                                output,
                                is_error,
                            });
                        }
                        takka_core::Event::Error { message } => {
                            let _ = event_tx.send(AppMessage::Error(message));
                        }
                        takka_core::Event::Usage {
                            prompt_tokens,
                            completion_tokens,
                            ..
                        } => {
                            let _ = event_tx.send(AppMessage::Usage {
                                input: prompt_tokens,
                                output: completion_tokens,
                            });
                        }
                        takka_core::Event::Thinking { .. } => {
                            // Skip — text was already streamed via on_chunk.
                            // A separate Thought message would duplicate content.
                        }
                        _ => {}
                    },
                )
                .await;
            tracing::debug!(
                ok = result.is_ok(),
                "spawned task: run_with_callbacks completed"
            );
            match result {
                Ok(response) => {
                    tracing::debug!(len = response.len(), "spawned task: sending TaskComplete");
                    if let Err(e) = tx.send(AppMessage::TaskComplete(response)) {
                        tracing::warn!(error = %e, "spawned task: failed to send TaskComplete");
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "spawned task: sending Error");
                    if let Err(send_err) = tx.send(AppMessage::Error(e.to_string())) {
                        tracing::warn!(error = %send_err, "spawned task: failed to send Error");
                    }
                }
            }
            // Lock is dropped here; commands_arc stays alive for future use.
        });
    }

    /// Handle slash commands
    async fn handle_slash_command(&mut self, input: &str) {
        let parts: Vec<&str> = input.splitn(2, ' ').collect();
        let cmd = parts[0];
        let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");

        match cmd {
            "/help" | "/h" => {
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: r#"**Available Commands:**

| Command | Alias | Description |
|---------|-------|-------------|
| `/help` | `/h` | Show this help |
| `/new` | `/n` | Start new session |
| `/clear` | `/cls` | Start new session (alias) |
| `/sessions` | `/ss` | List & resume sessions |
| `/models` | `/m` | Switch model |
| `/provider` | | Switch provider |
| `/search` | `/s`, `/find` | Search conversation history |
| `/image` | `/img` | Attach image |
| `/skills` | `/sk` | List indexed skills |
| `/skills <name>` | `/sk <name>` | Load and display a skill |
| `/skill` | | Open skill selection dropdown |
| `/skill <name>` | | Load and activate a skill |
| `/plan` | `/p` | Generate a structured plan |
| `/config` | `/cfg` | Show configuration |
| `/tools` | `/t` | List available tools |
| `/history` | `/hist` | Show message history |
| `/stats` | | Show statistics |
| `/mcp` | | Show MCP server status |
| `/quit` | `/q` | Exit TUI |

**Tips:**
- Type `/` to see command dropdown
- Type `@` anywhere to browse files
- Use ↑/↓ to navigate history
- Use PageUp/PageDown to scroll
- Press Ctrl+C / Esc to cancel"#
                        .into(),
                    timestamp: chrono::Local::now(),
                });
            }
            "/new" | "/n" => {
                self.new_session().await;
            }
            "/clear" | "/cls" => {
                self.new_session().await;
            }
            "/sessions" | "/ss" if !arg.is_empty() => {
                // Resume specific session by ID or index
                self.resume_session(arg).await;
            }
            "/sessions" | "/ss" => {
                self.open_sessions();
            }
            "/models" | "/m" if !arg.is_empty() => {
                self.switch_model(arg).await;
            }
            "/models" | "/m" => {
                // Show models list - open a message with available models
                let models_text = match &self.commands {
                    Some(cmds_arc) => {
                        if let Ok(cmds) = cmds_arc.try_lock() {
                            let (provider, model, _) = cmds.model_info();
                            format!(
                                "**Current Model:** {} / {}\n\nUse `/models <name>` to switch.",
                                provider, model
                            )
                        } else {
                            "**(busy)**".into()
                        }
                    }
                    None => "Commands not initialized.".into(),
                };
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: models_text,
                    timestamp: chrono::Local::now(),
                });
            }
            "/search" | "/s" | "/find" => {
                self.handle_search(arg);
            }
            "/image" | "/img" => {
                self.handle_image(arg).await;
            }
            "/provider" if !arg.is_empty() => {
                self.handle_provider_switch(arg).await;
            }
            "/provider" => {
                self.handle_provider_list().await;
            }
            "/mcp" => {
                self.handle_mcp_status().await;
            }
            "/skills" | "/sk" | "/skill" if !arg.is_empty() => {
                self.handle_skill_load(arg).await;
            }
            "/skills" | "/sk" => {
                self.handle_skill_list().await;
            }
            "/skill" => {
                self.open_skill_dropdown();
            }
            "/plan" | "/p" if !arg.is_empty() => {
                self.handle_plan(arg).await;
            }
            "/plan" | "/p" => {
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: "Usage: `/plan <goal>` — Generate a structured plan.".into(),
                    timestamp: chrono::Local::now(),
                });
            }
            "/quit" | "/q" | "/exit" => {
                self.should_quit = true;
            }
            "/config" | "/cfg" => {
                let config_text = match &self.commands {
                    Some(cmds_arc) => match cmds_arc.try_lock() {
                        Ok(cmds) => {
                            let cfg = cmds.config_ref();
                            let mut lines = String::from("**Configuration:**\n\n");
                            lines.push_str(&format!(
                                "- Config path: `{}`\n",
                                takka_core::Config::config_path().display()
                            ));
                            if let Some(p) = cfg.active_provider() {
                                lines.push_str(&format!("- Provider: **{}**\n", p.name));
                                lines.push_str(&format!("- API Base: `{}`\n", p.api_base));
                                if p.api_key.is_empty() {
                                    lines.push_str("- API Key: ✗ not set\n");
                                } else {
                                    let masked = format!(
                                        "{}...{}",
                                        &p.api_key[..4.min(p.api_key.len())],
                                        &p.api_key[p.api_key.len().saturating_sub(4)..]
                                    );
                                    lines.push_str(&format!("- API Key: `{}`\n", masked));
                                }
                                if let Some(m) = p.models.first() {
                                    lines.push_str(&format!(
                                        "- Model: `{}` (temp: {}, max_tokens: {})\n",
                                        m.model, m.temperature, m.max_tokens
                                    ));
                                }
                            } else {
                                lines.push_str("- No provider configured.\n");
                            }
                            lines
                        }
                        Err(_) => "**(busy)**".into(),
                    },
                    None => "Commands not initialized.".into(),
                };
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: config_text,
                    timestamp: chrono::Local::now(),
                });
            }
            "/tools" | "/t" => {
                let tools_text = {
                    let registry = takka_core::ToolRegistry::new();
                    for tool in takka_core::tools::builtin_tools() {
                        registry.register(tool);
                    }
                    let tool_list = registry.list();
                    let mut lines = format!("**Available Tools ({})**\n\n", tool_list.len());
                    for t in &tool_list {
                        lines.push_str(&format!("- **{}** — {}\n", t.name, t.description));
                        if !t.parameters.is_empty() {
                            let params: Vec<String> = t
                                .parameters
                                .keys()
                                .map(|p| {
                                    let required = t.required.contains(p);
                                    format!("{}{}", p, if required { "*" } else { "" })
                                })
                                .collect();
                            lines.push_str(&format!("  Params: `{}`\n", params.join(", ")));
                        }
                    }
                    lines
                };
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: tools_text,
                    timestamp: chrono::Local::now(),
                });
            }
            "/history" | "/hist" => {
                let history_text = if self.history.is_empty() {
                    "No command history yet.".to_string()
                } else {
                    self.history
                        .iter()
                        .enumerate()
                        .map(|(i, h)| format!("{}. {}", i + 1, h))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: format!("**Command History:**\n\n{}", history_text),
                    timestamp: chrono::Local::now(),
                });
            }
            "/stats" => {
                let (provider, model, _) = self.model_info();
                let in_tok = self.stats.format_tokens(self.stats.tokens_input);
                let out_tok = self.stats.format_tokens(self.stats.tokens_output);
                self.messages.push(Message {
                    role: MessageRole::System,
                    content: format!(
                        "**Session Statistics:**\n\n- Provider: {}\n- Model: {}\n- Messages sent: {}\n- Tool calls: {}\n- Tokens: {} in / {} out\n- Session ID: {}",
                        provider, model, self.stats.messages_sent, self.stats.tool_calls,
                        in_tok, out_tok, &self.session.id[..self.session.id.len().min(20)]
                    ),
                    timestamp: chrono::Local::now(),
                });
            }
            _ => {
                self.messages.push(Message {
                    role: MessageRole::Error,
                    content: format!(
                        "Unknown command: `{}`\nType `/help` for available commands.",
                        cmd
                    ),
                    timestamp: chrono::Local::now(),
                });
            }
        }
    }

    /// Process pending messages from async tasks
    pub fn process_messages(&mut self) {
        // ── Watchdog: auto-reset loading if the agent hangs silently ──
        if self.is_loading {
            if let Some(started) = self.loading_started {
                if started.elapsed() > LOADING_TIMEOUT {
                    tracing::warn!(
                        elapsed_ms = started.elapsed().as_millis(),
                        "Loading watchdog triggered — no response in {}s",
                        LOADING_TIMEOUT.as_secs()
                    );
                    self.is_loading = false;
                    self.loading_started = None;
                    self.progress.stop();
                    // A mid-run dropdown must not leak into the idle state.
                    self.dropdown = None;
                    self.messages.push(Message {
                        role: MessageRole::Error,
                        content: format!(
                            "⚠ Agent did not respond within {} seconds. \
                             Try again or use Ctrl+C to cancel.",
                            LOADING_TIMEOUT.as_secs()
                        ),
                        timestamp: chrono::Local::now(),
                    });
                }
            }
        }

        if let Some(rx) = &mut self.rx {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    AppMessage::StreamChunk(chunk) => {
                        self.current_response.push_str(&chunk);
                    }
                    AppMessage::TaskComplete(result) => {
                        self.is_loading = false;
                        self.loading_started = None;
                        self.progress.stop();
                        // A mid-run dropdown must not leak into the idle state.
                        self.dropdown = None;
                        self.stats.messages_sent += 1;

                        let content = if self.current_response.is_empty() {
                            result
                        } else {
                            std::mem::take(&mut self.current_response)
                        };

                        self.messages.push(Message {
                            role: MessageRole::Assistant,
                            content,
                            timestamp: chrono::Local::now(),
                        });
                    }
                    AppMessage::Error(err) => {
                        self.is_loading = false;
                        self.loading_started = None;
                        self.progress.stop();
                        // A mid-run dropdown must not leak into the idle state.
                        self.dropdown = None;
                        self.current_response.clear();

                        // "Cancelled by user" (AgenticError::Cancelled —
                        // story 1.4) is a desired outcome: system notice,
                        // not a red error line (same decision as the
                        // inline REPL's cancel notice).
                        if err.contains("Cancelled by user") {
                            self.messages.push(Message {
                                role: MessageRole::System,
                                content: "Run cancelled".to_string(),
                                timestamp: chrono::Local::now(),
                            });
                        } else {
                            self.messages.push(Message {
                                role: MessageRole::Error,
                                content: format!("Error: {}", err),
                                timestamp: chrono::Local::now(),
                            });
                        }
                    }
                    AppMessage::Progress(msg) => {
                        self.progress.set_message(msg);
                    }
                    AppMessage::ToolCall { name, arguments } => {
                        self.stats.tool_calls += 1;
                        // Stash the structured payload as JSON in the
                        // message content so the renderer can decode it.
                        // We use a sentinel envelope so render code can
                        // tell tool messages apart by the role enum.
                        let payload = serde_json::json!({
                            "name": name,
                            "arguments": arguments,
                        });
                        self.messages.push(Message {
                            role: MessageRole::Tool,
                            content: payload.to_string(),
                            timestamp: chrono::Local::now(),
                        });
                    }
                    AppMessage::ToolResult {
                        name,
                        output,
                        is_error,
                    } => {
                        let payload = serde_json::json!({
                            "name": name,
                            "output": output,
                        });
                        self.messages.push(Message {
                            role: if is_error {
                                MessageRole::ToolError
                            } else {
                                MessageRole::ToolResult
                            },
                            content: payload.to_string(),
                            timestamp: chrono::Local::now(),
                        });
                    }
                    AppMessage::Thought(content) => {
                        // Display the LLM's thinking/explanation as an assistant message
                        if !content.is_empty() {
                            self.messages.push(Message {
                                role: MessageRole::Assistant,
                                content,
                                timestamp: chrono::Local::now(),
                            });
                        }
                    }
                    AppMessage::ToolDelta { name, delta } => {
                        if !delta.trim().is_empty() {
                            self.messages.push(Message {
                                role: MessageRole::ToolActivity,
                                content: format!("[{}]\n{}", name, delta),
                                timestamp: chrono::Local::now(),
                            });
                        }
                    }
                    AppMessage::PlanProgress {
                        goal: _,
                        current_step,
                        step_status,
                        total: _,
                        completed: _,
                        failed: _,
                        pending: _,
                    } => {
                        self.progress
                            .set_message(format!("Plan: {} — {}", current_step, step_status,));
                    }
                    AppMessage::Usage { input, output } => {
                        // Real provider numbers — the status bar used to sit at
                        // 0/0 because nothing ever incremented these.
                        self.stats.tokens_input = self.stats.tokens_input.saturating_add(input);
                        self.stats.tokens_output = self.stats.tokens_output.saturating_add(output);
                    }
                }
            }
        }
    }

    /// Tick for animations
    pub fn tick(&mut self) {
        if self.last_tick.elapsed() >= Duration::from_millis(100) {
            self.progress.tick();
            self.last_tick = Instant::now();
        }
    }
}

// ── Main TUI event loop ──────────────────────────────────────

/// Run the TUI application
pub async fn run_tui(commands: Arc<tokio::sync::Mutex<Commands>>) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    crate::commands::TUI_ACTIVE.store(true, Ordering::Relaxed);

    // Create app
    let mut app = App::new(commands);

    // Main loop
    let tick_rate = Duration::from_millis(50);
    let mut last_tick = Instant::now();

    loop {
        // ── Terminal-touching phase ──
        // Guarded by RENDER_GATE so a concurrent `question` / confirmation
        // prompt owns the screen while it is on display: we park here
        // without drawing or reading keys, so dialoguer's UI is never
        // overwritten and its keystrokes are never stolen.
        let key = {
            let _render_gate = crate::commands::RENDER_GATE.lock().unwrap();

            // After an interactive prompt re-entered the alternate screen,
            // ratatui's diff would only repaint cells that changed vs its
            // pre-question frame — the rest would stay blank. Force a full
            // clear (which also resets the internal buffer) first.
            if crate::commands::TUI_NEEDS_REDRAW.swap(false, Ordering::Relaxed) {
                terminal.clear()?;
                // The operator just spent time reading/answering the
                // prompt — restart the loading watchdog so that elapsed
                // time doesn't count as a hung agent.
                if app.is_loading {
                    app.loading_started = Some(Instant::now());
                }
            }

            // Draw UI
            terminal.draw(|f| ui::draw(f, &mut app))?;

            // Handle events
            let timeout = tick_rate.saturating_sub(last_tick.elapsed());
            if event::poll(timeout)? {
                if let Event::Key(key) = event::read()? {
                    Some(key)
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some(key) = key {
            handle_key_event(&mut app, key).await;
        }

        // Tick for animations
        if last_tick.elapsed() >= tick_rate {
            app.tick();
            app.process_messages();
            last_tick = Instant::now();
        }

        // Check quit
        if app.should_quit {
            // Save session before exiting
            if !app.session.messages.is_empty() {
                app.save_session();
            }
            break;
        }
    }

    // Restore terminal
    crate::commands::TUI_ACTIVE.store(false, Ordering::Relaxed);
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}

/// Handle a single key event
async fn handle_key_event(app: &mut App, key: crossterm::event::KeyEvent) {
    if !crate::keyboard::should_process_key_kind(key.kind) {
        return;
    }

    // ── Session view mode ──
    // When session list is open, all keys go to session view handler
    if app.session_view.is_some() {
        app.handle_session_view_key(key).await;
        return;
    }

    // ── Dropdown-specific key handling ──
    // When a dropdown is open, intercept navigation keys
    if app.dropdown.is_some() {
        match key.code {
            KeyCode::Up => {
                app.dropdown_up();
                return;
            }
            KeyCode::Down => {
                app.dropdown_down();
                return;
            }
            KeyCode::Tab | KeyCode::Enter => {
                app.accept_dropdown();
                return;
            }
            KeyCode::Esc => {
                app.close_dropdown();
                return;
            }
            // All other keys fall through to normal input handling
            // so typing continues to filter the dropdown
            _ => {}
        }
    }

    // ── Normal key handling ──
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if app.is_loading {
                app.is_loading = false;
                app.progress.stop();
                // A mid-run dropdown must not leak into the idle state.
                app.dropdown = None;
                app.messages.push(Message {
                    role: MessageRole::System,
                    content: "Task cancelled.".into(),
                    timestamp: chrono::Local::now(),
                });
            } else {
                app.should_quit = true;
            }
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        KeyCode::Enter if !app.is_loading => {
            app.submit().await;
        }
        KeyCode::Enter if app.is_loading => {
            app.submit_mid_run().await;
        }
        KeyCode::Char(c) if !app.is_loading => {
            app.insert_char(c);
        }
        KeyCode::Char(c) if app.is_loading => {
            app.insert_mid_run_char(c);
        }
        KeyCode::Backspace if !app.is_loading => {
            app.delete_char();
        }
        KeyCode::Backspace if app.is_loading => {
            // Mid-run correction (story 3.1): trim the buffer and let the
            // dropdown engine re-evaluate over it.
            app.delete_mid_run_char();
        }
        KeyCode::Delete if !app.is_loading => {
            app.delete_char_forward();
        }
        KeyCode::Left => {
            // If Shift is held, scroll instead of moving cursor
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.scroll_up();
            } else {
                app.move_cursor_left();
            }
        }
        KeyCode::Right => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.scroll_down();
            } else {
                app.move_cursor_right();
            }
        }
        KeyCode::Home => app.move_cursor_start(),
        KeyCode::End => app.move_cursor_end(),
        KeyCode::Up => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.scroll_up();
            } else {
                app.history_up();
            }
        }
        KeyCode::Down => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.scroll_down();
            } else {
                app.history_down();
            }
        }
        KeyCode::PageUp => app.scroll_up(),
        KeyCode::PageDown => app.scroll_down(),
        KeyCode::Esc if app.dropdown.is_some() => {
            app.close_dropdown();
        }
        _ => {}
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::QueueMode;
    use crossterm::event::KeyEvent;
    use takka_core::Config;

    /// Story 2.4 TUI smoke (FollowUp): mid-run text queues client-side;
    /// the message log gains the `↳ queued for the next run` system line.
    #[tokio::test(flavor = "multi_thread")]
    async fn tui_submit_mid_run_queues_in_followup_mode() {
        let commands = Arc::new(Mutex::new(
            Commands::new(Config::fallback()).with_queue_mode(QueueMode::FollowUp),
        ));
        let mut app = App::new(commands.clone());
        app.is_loading = true;
        app.mid_run_input = "tui queued text".into();

        app.submit_mid_run().await;

        // The queue got the line.
        let queued: Vec<String> = commands.lock().await.pending_follow_ups_snapshot();
        assert_eq!(queued, vec!["tui queued text".to_string()]);
        // The log surfaced the hand-over.
        assert!(
            app.messages.iter().any(|m| m.role == MessageRole::System
                && m.content.contains("tui queued text")
                && m.content.contains("queued for the next run")),
            "the TUI must surface the queue ack: {:?}",
            app.messages
        );
        // The mid-run buffer cleared.
        assert!(app.mid_run_input.is_empty());
    }

    // ── Story 3.1: mid-run completions & idle parity ──

    /// FollowUp queue mode so mid-run submits queue client-side
    /// (observable without a live run).
    fn test_app() -> (App, Arc<Mutex<Commands>>) {
        let commands = Arc::new(Mutex::new(
            Commands::new(Config::fallback()).with_queue_mode(QueueMode::FollowUp),
        ));
        let app = App::new(commands.clone());
        (app, commands)
    }

    fn type_mid_run(app: &mut App, text: &str) {
        for c in text.chars() {
            app.insert_mid_run_char(c);
        }
    }

    fn type_idle(app: &mut App, text: &str) {
        for c in text.chars() {
            app.insert_char(c);
        }
    }

    /// Mid-run `/` opens the command dropdown, filters like idle, and
    /// Tab-accept fills `mid_run_input` — without steering anything.
    #[tokio::test(flavor = "multi_thread")]
    async fn tui_mid_run_command_dropdown_fills_buffer_without_steering() {
        let (mut app, commands) = test_app();
        app.is_loading = true;

        type_mid_run(&mut app, "/mod");

        let dd = app
            .dropdown
            .as_ref()
            .expect("command dropdown must open mid-run");
        assert_eq!(dd.dropdown_type, DropdownType::Command);
        assert_eq!(dd.query, "mod");
        // Same filtering as the idle buffer (const command table).
        assert_eq!(dd.items, vec!["models".to_string()]);

        app.accept_dropdown();

        // The fill landed in the mid-run buffer, dropdown closed.
        assert_eq!(app.mid_run_input, "/models ");
        assert!(app.dropdown.is_none());
        // Nothing was steered: the queue is untouched.
        let queued: Vec<String> = commands.lock().await.pending_follow_ups_snapshot();
        assert!(queued.is_empty(), "accept must fill, not steer: {queued:?}");
    }

    /// Mid-run `@` opens the file dropdown and accept replaces from `@`
    /// to the end of the mid-run buffer.
    #[tokio::test(flavor = "multi_thread")]
    async fn tui_mid_run_file_trigger_accept_replaces_from_at() {
        let (mut app, _commands) = test_app();
        app.is_loading = true;

        type_mid_run(&mut app, "check @");
        let dd = app
            .dropdown
            .as_ref()
            .expect("file dropdown must open on @ mid-run");
        assert_eq!(dd.dropdown_type, DropdownType::File);

        // Pin a deterministic item (file discovery itself is
        // environment-dependent and already covered in dropdown.rs).
        app.dropdown.as_mut().unwrap().items = vec!["src/main.rs".to_string()];
        app.accept_dropdown();

        assert_eq!(app.mid_run_input, "check @src/main.rs ");
        assert!(app.dropdown.is_none());
    }

    /// Dropdown navigation keys (Up/Down/Tab/Esc) work mid-run; Enter with
    /// an open dropdown fills instead of steering; Enter with no dropdown
    /// follows the exact story-2.4 path and the log gains the `↳` line.
    #[tokio::test(flavor = "multi_thread")]
    async fn tui_mid_run_enter_with_dropdown_fills_then_next_enter_steers() {
        let (mut app, commands) = test_app();
        app.is_loading = true;

        type_mid_run(&mut app, "/");
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)).await;
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).await;

        // Second command in the const table is `new` — the fill landed in
        // the mid-run buffer, nothing was steered.
        assert_eq!(app.mid_run_input, "/new ");
        assert!(app.dropdown.is_none());
        let queued: Vec<String> = commands.lock().await.pending_follow_ups_snapshot();
        assert!(
            queued.is_empty(),
            "Enter on an open dropdown must fill: {queued:?}"
        );

        // Enter again (dropdown closed): exact story-2.4 path — the typed
        // text is delivered per queue mode and the log surfaces the hand-over.
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)).await;
        let queued: Vec<String> = commands.lock().await.pending_follow_ups_snapshot();
        assert_eq!(queued, vec!["/new".to_string()]);
        assert!(app.mid_run_input.is_empty());
        assert!(app.messages.iter().any(|m| m.role == MessageRole::System
            && m.content.contains('\u{21b3}')
            && m.content.contains("/new")));
    }

    /// Esc dismisses the dropdown but keeps the mid-run buffer; further
    /// typing re-evaluates; Tab accepts; Up wraps the selection.
    #[tokio::test(flavor = "multi_thread")]
    async fn tui_mid_run_navigation_contract_esc_tab_up() {
        let (mut app, _commands) = test_app();
        app.is_loading = true;

        type_mid_run(&mut app, "/mod");
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).await;
        assert!(app.dropdown.is_none());
        assert_eq!(app.mid_run_input, "/mod", "Esc must keep the buffer");

        // Typing after Esc re-evaluates and re-opens.
        app.insert_mid_run_char('e');
        let dd = app.dropdown.as_ref().expect("typing must re-open");
        assert_eq!(dd.dropdown_type, DropdownType::Command);
        assert_eq!(dd.query, "mode");

        // Tab accepts into the mid-run buffer.
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)).await;
        assert_eq!(app.mid_run_input, "/models ");

        // Up from selection 0 wraps to the last command (const table).
        app.mid_run_input.clear();
        app.dropdown = None;
        type_mid_run(&mut app, "/");
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)).await;
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)).await;
        assert_eq!(app.mid_run_input, "/quit ");
    }

    /// Backspace corrects the mid-run buffer and the dropdown re-evaluates;
    /// the idle buffer is untouched and empty-buffer backspace is safe.
    #[tokio::test(flavor = "multi_thread")]
    async fn tui_mid_run_backspace_corrects_buffer_and_dropdown() {
        let (mut app, _commands) = test_app();
        app.is_loading = true;

        type_mid_run(&mut app, "/mo");
        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        )
        .await;
        assert_eq!(app.mid_run_input, "/m");
        let dd = app.dropdown.as_ref().expect("dropdown must re-evaluate");
        assert_eq!(dd.dropdown_type, DropdownType::Command);
        assert_eq!(dd.query, "m");
        assert_eq!(dd.items, vec!["models".to_string(), "mcp".to_string()]);

        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        )
        .await;
        assert_eq!(app.mid_run_input, "/");
        assert!(app.dropdown.is_some(), "`/` alone still triggers");

        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        )
        .await;
        assert!(app.mid_run_input.is_empty());
        assert!(app.dropdown.is_none(), "no trigger, no dropdown");

        // Backspace on an empty mid-run buffer is a no-op, and the idle
        // buffer is never touched by the mid-run arm.
        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        )
        .await;
        assert!(app.mid_run_input.is_empty());
        assert!(app.input.is_empty());
    }

    /// The model trigger works mid-run with the same construction as idle
    /// (pure static registry); the empty guard keeps a matching-nothing
    /// dropdown closed in both states.
    #[tokio::test(flavor = "multi_thread")]
    async fn tui_mid_run_model_trigger_parity() {
        let (mut app, _commands) = test_app();
        app.is_loading = true;

        type_mid_run(&mut app, "/models gpt");
        match app.dropdown.as_ref() {
            Some(dd) => {
                assert_eq!(dd.dropdown_type, DropdownType::Model);
                assert_eq!(dd.query, "gpt");
                assert!(dd.items.iter().all(|i| i.to_lowercase().contains("gpt")));
            }
            None => { /* no ambient models configured — guard kept it closed */ }
        }

        // A query matching nothing must not open an empty dropdown.
        type_mid_run(&mut app, "zzz-no-match");
        assert!(app.dropdown.is_none(), "empty dropdown must stay closed");
    }

    /// The skill guard: nothing discoverable → the dropdown stays closed;
    /// with skills present it opens on an empty query.
    #[test]
    fn tui_open_skill_dropdown_guard() {
        let (mut app, _commands) = test_app();

        app.open_skill_dropdown_with(Vec::new());
        assert!(app.dropdown.is_none(), "empty sources must stay closed");

        app.open_skill_dropdown_with(vec![("writer".to_string(), "Writes".to_string())]);
        let dd = app.dropdown.as_ref().expect("skills present must open");
        assert_eq!(dd.dropdown_type, DropdownType::Skill);
        assert_eq!(dd.items, vec!["writer — Writes".to_string()]);
    }

    /// Skill follow-up parity with the inline REPL: `/skill `/`/sk `
    /// arguments re-trigger the skill dropdown filtered by the query;
    /// empty/unavailable discovery and no-match queries stay closed.
    #[test]
    fn tui_skill_followup_retrigger_matches_repl() {
        let pairs = vec![
            ("navigation".to_string(), "Find routes".to_string()),
            ("search".to_string(), "Search history".to_string()),
        ];

        // Follow-up on `/skill ` filters by the typed argument.
        let dd = App::evaluate_dropdown("/skill na", 9, || pairs.clone())
            .expect("skill follow-up must re-trigger");
        assert_eq!(dd.dropdown_type, DropdownType::Skill);
        assert_eq!(dd.items, vec!["navigation — Find routes".to_string()]);

        // The `/sk ` alias behaves identically.
        let dd = App::evaluate_dropdown("/sk na", 6, || pairs.clone()).unwrap();
        assert_eq!(dd.dropdown_type, DropdownType::Skill);
        assert_eq!(dd.items, vec!["navigation — Find routes".to_string()]);

        // Right after accept (empty argument) the full list shows.
        let dd = App::evaluate_dropdown("/skill ", 7, || pairs.clone()).unwrap();
        assert_eq!(dd.items.len(), 2);

        // Discovery unavailable / empty → stays closed (no empty trap).
        assert!(App::evaluate_dropdown("/skill na", 9, Vec::new).is_none());
        // Query matching nothing → closed as well.
        assert!(App::evaluate_dropdown("/skill zzz", 10, || pairs.clone()).is_none());
    }

    /// Idle parity: the same triggers open the same dropdowns, the accept
    /// chain fills the idle buffer with the cursor, and the skill chain
    /// now fills `/skill ` like the inline REPL (the recorded drift fix).
    #[test]
    fn tui_idle_parity_and_skill_chain_fill() {
        let (mut app, _commands) = test_app();

        type_idle(&mut app, "/mod");
        let dd = app.dropdown.as_ref().expect("idle command dropdown");
        assert_eq!(dd.dropdown_type, DropdownType::Command);
        assert_eq!(dd.items, vec!["models".to_string()]);
        app.accept_dropdown();
        assert_eq!(app.input, "/models ");
        assert_eq!(app.cursor_pos, app.input.len());

        // A query matching nothing must not open an empty dropdown idle
        // either (same guard as the inline REPL — it would swallow Enter).
        let (mut app, _commands) = test_app();
        type_idle(&mut app, "/zzz");
        assert!(
            app.dropdown.is_none(),
            "empty idle dropdown must stay closed"
        );

        // Selecting `skill` from the command list fills `/skill ` into the
        // idle buffer (filter of `sk` is exactly ["skill"] — const table).
        let (mut app, _commands) = test_app();
        app.input = "/sk".into();
        app.cursor_pos = 3;
        app.dropdown = Some(Dropdown::new(DropdownType::Command, "sk".into()));
        assert_eq!(
            app.dropdown.as_ref().unwrap().items,
            vec!["skill".to_string()]
        );
        app.accept_dropdown();
        assert_eq!(app.input, "/skill ");
        assert_eq!(app.cursor_pos, 7);
        // The reopened dropdown is the skill list when discovery yields
        // skills, closed otherwise (guard) — both valid here.
        if let Some(dd) = app.dropdown.as_ref() {
            assert_eq!(dd.dropdown_type, DropdownType::Skill);
        }
    }
}
