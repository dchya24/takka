//! Interactive REPL mode using custom ratatui input widget
//!
//! Provides an interactive CLI with:
//! - `/` command completion dropdown (auto-activates on `/`)
//! - `@` file path completion dropdown (auto-activates on `@`)
//! - Syntax highlighting for `/` (yellow) and `@` (blue)
//! - In-memory input history with ↑/↓ navigation
//! - Session statistics, conversation history, save/load
//!
//! Uses crossterm raw mode for key capture and ratatui inline rendering
//! (no alternate screen). Replaces the previous reedline-based input.
//!
//! Layout (mechanical split, no behaviour change): this file holds the
//! shared state (stats, render state, slash table) and the `run` entry.
//! The REPL loop and its slash-command dispatch live in `repl`, dropdown
//! handling in `dropdown`, and all terminal output in `render`.

mod dropdown;
mod render;
mod repl;

pub(crate) use dropdown::*;
pub(crate) use render::*;
pub(crate) use repl::*;

use anyhow::Result;
use crossterm::{
    cursor::{MoveToColumn, MoveUp},
    event::{self, Event, KeyCode, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType},
    ExecutableCommand,
};
use ratatui::{
    style::{Color, Modifier, Style as RStyle},
    text::{Line, Span as RSpan},
};
use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::cli::SkillAction;
use crate::commands::{Commands, QueueMode, TimelineStyle};
use crate::input_buffer::InputBuffer;
use crate::input_renderer::{self, PromptMetadata};
use crate::tui::dropdown::{Dropdown, DropdownType};
use crate::widgets::budget::budget_chip;
use crate::widgets::components;
use crate::widgets::inline;
use crate::widgets::spinner;
use crate::widgets::toast::{Toast, ToastManager};

// ── Session statistics ──────────────────────────────────────

#[derive(Clone)]
pub(crate) struct SessionStats {
    pub(crate) messages_sent: Arc<AtomicU32>,
    pub(crate) tool_calls: Arc<AtomicU32>,
    pub(crate) total_input_tokens: Arc<AtomicU32>,
    pub(crate) total_output_tokens: Arc<AtomicU32>,
    pub(crate) total_cache_read_tokens: Arc<AtomicU32>,
    pub(crate) total_cache_creation_tokens: Arc<AtomicU32>,
    /// Spend of the current run, in USD (H-06). The ceiling bounds one run,
    /// so the budget chip reads this, not a session total.
    pub(crate) run_cost_usd: Arc<AtomicU64>,
    pub(crate) session_start: Instant,
}

impl SessionStats {
    pub(crate) fn new() -> Self {
        Self {
            messages_sent: Arc::new(AtomicU32::new(0)),
            tool_calls: Arc::new(AtomicU32::new(0)),
            total_input_tokens: Arc::new(AtomicU32::new(0)),
            total_output_tokens: Arc::new(AtomicU32::new(0)),
            total_cache_read_tokens: Arc::new(AtomicU32::new(0)),
            total_cache_creation_tokens: Arc::new(AtomicU32::new(0)),
            run_cost_usd: Arc::new(AtomicU64::new(0)),
            session_start: Instant::now(),
        }
    }

    pub(crate) fn increment_messages(&self) {
        self.messages_sent.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn reset(&self) {
        self.messages_sent.store(0, Ordering::Relaxed);
        self.tool_calls.store(0, Ordering::Relaxed);
        self.total_input_tokens.store(0, Ordering::Relaxed);
        self.total_output_tokens.store(0, Ordering::Relaxed);
        self.total_cache_read_tokens.store(0, Ordering::Relaxed);
        self.total_cache_creation_tokens.store(0, Ordering::Relaxed);
        self.run_cost_usd.store(0, Ordering::Relaxed);
    }

    /// Record this run's spend (H-06). F64 has no atomic, so the bits are
    /// carried in a `u64` — this is display state only.
    pub(crate) fn set_run_cost_usd(&self, cost: f64) {
        self.run_cost_usd
            .store(cost.max(0.0).to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn run_cost_usd(&self) -> f64 {
        f64::from_bits(self.run_cost_usd.load(Ordering::Relaxed))
    }

    pub(crate) fn add_tool_calls(&self, n: u32) {
        if n > 0 {
            self.tool_calls.fetch_add(n, Ordering::Relaxed);
        }
    }

    pub(crate) fn add_input_tokens(&self, n: u32) {
        self.total_input_tokens.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn add_output_tokens(&self, n: u32) {
        self.total_output_tokens.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn add_cache_read_tokens(&self, n: u32) {
        self.total_cache_read_tokens.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn add_cache_creation_tokens(&self, n: u32) {
        self.total_cache_creation_tokens
            .fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn total_cache_read_tokens(&self) -> u32 {
        self.total_cache_read_tokens.load(Ordering::Relaxed)
    }

    pub(crate) fn total_cache_creation_tokens(&self) -> u32 {
        self.total_cache_creation_tokens.load(Ordering::Relaxed)
    }

    pub(crate) fn cache_hit_ratio(&self) -> f64 {
        let read = self.total_cache_read_tokens() as f64;
        let created = self.total_cache_creation_tokens() as f64;
        let total = read + created;
        if total > 0.0 {
            read / total
        } else {
            0.0
        }
    }

    pub(crate) fn messages_sent(&self) -> u32 {
        self.messages_sent.load(Ordering::Relaxed)
    }

    #[allow(dead_code)]
    pub(crate) fn tool_calls(&self) -> u32 {
        self.tool_calls.load(Ordering::Relaxed)
    }

    pub(crate) fn total_input_tokens(&self) -> u32 {
        self.total_input_tokens.load(Ordering::Relaxed)
    }

    pub(crate) fn total_output_tokens(&self) -> u32 {
        self.total_output_tokens.load(Ordering::Relaxed)
    }

    pub(crate) fn elapsed_secs(&self) -> u64 {
        self.session_start.elapsed().as_secs()
    }

    pub(crate) fn elapsed_str(&self) -> String {
        let secs = self.elapsed_secs();
        if secs < 60 {
            format!("{}s", secs)
        } else {
            format!("{}m {}s", secs / 60, secs % 60)
        }
    }

    pub(crate) fn format_tokens(&self, n: u32) -> String {
        if n >= 1_000_000 {
            format!("{:.1}M", n as f64 / 1_000_000.0)
        } else if n >= 1_000 {
            format!("{:.1}K", n as f64 / 1_000.0)
        } else {
            format!("{}", n)
        }
    }
}

// ── Slash command definitions with aliases ──────────────────

pub(crate) const SLASH_COMMANDS: &[(&str, &[&str], &str)] = &[
    ("help", &["h", "?"], "Show help message"),
    ("new", &["n", "clear", "cls"], "Start a new session"),
    ("config", &["cfg"], "Show current configuration"),
    ("history", &["hist"], "Show conversation history"),
    ("tools", &["t"], "List available tools"),
    ("models", &["m"], "List all models from all providers"),
    ("provider", &["prov"], "Switch or show provider"),
    ("sessions", &["ss"], "List and resume previous sessions"),
    ("mcp", &[], "Show MCP server status"),
    ("plan", &["p"], "Create a plan for a goal"),
    ("skills", &[], "List all indexed skills"),
    ("search", &["find"], "Search conversation memory"),
    ("image", &["img"], "Attach an image for the next turn"),
    ("stats", &[], "Show session statistics"),
    ("thoughts", &["thinking"], "Toggle inline model reasoning"),
    (
        "timeline",
        &[],
        "Set tool timeline detail (detailed | compact)",
    ),
    ("quit", &["q", "exit"], "Exit interactive mode"),
];

// ── Conversation entry ──────────────────────────────────────

#[derive(Debug)]
pub(crate) struct ConversationEntry {
    pub(crate) role: String,
    pub(crate) content: String,
    pub(crate) timestamp: chrono::DateTime<chrono::Local>,
}

// ── Render state ───────────────────────────────────────────
//
/// Encapsulates the mutable rendering state used by the REPL
/// loop so that cursor-position tracking and line-count
/// calculations are kept together instead of being scattered
/// across local variables.
pub(crate) struct RenderState {
    /// How many terminal lines the previous render covered
    /// (prompt + dropdown).  Used by MoveUp(N) to target the
    /// top of the input area before clearing.
    pub prev_lines: usize,
    /// Whether the status bar (+toasts) needs to be reprinted.
    pub needs_status_bar: bool,
}

impl RenderState {
    pub(crate) fn new() -> Self {
        Self {
            prev_lines: 0,
            needs_status_bar: true,
        }
    }

    /// Clear the previously rendered input area (prompt +
    /// dropdown).  The caller MUST have ensured that the cursor
    /// sits on a NEW line *below* the rendered area before
    /// calling this — that invariant is maintained by
    /// render_prompt_line / render_dropdown_lines which always
    /// end with `print_line` (includes `\r\n`).
    pub(crate) fn clear_input_area(&self, stdout: &mut Stdout) -> io::Result<()> {
        if self.prev_lines == 0 {
            return Ok(());
        }
        // Move up `prev_lines` → back to top of input area.
        stdout.execute(MoveUp(self.prev_lines as u16))?;
        stdout.execute(MoveToColumn(0))?;
        stdout.execute(Clear(ClearType::FromCursorDown))?;
        stdout.flush()?;
        Ok(())
    }

    /// Reset when the input area is permanently consumed
    /// (Enter submit, Ctrl+C, Ctrl+L).
    pub(crate) fn reset(&mut self) {
        self.prev_lines = 0;
    }

    /// Called when the prompt + dropdown have been rendered.
    /// `lines` is the total number of lines emitted.
    pub(crate) fn after_render(&mut self, lines: usize) {
        self.prev_lines = lines;
    }
}

// ── REPL loop ───────────────────────────────────────────────

pub async fn run(mut commands: Commands) -> Result<()> {
    let stats = SessionStats::new();
    let mut model_info = get_model_info(&commands);

    // Initialize session
    let cwd = std::env::current_dir()
        .unwrap_or_default()
        .display()
        .to_string();
    let mut current_session = crate::session::create(&cwd, &model_info.provider, &model_info.model);

    print_banner(&model_info, &stats);

    let mut buffer = InputBuffer::new();
    let mut dropdown: Option<Dropdown> = None;
    let mut conversation: Vec<ConversationEntry> = Vec::new();

    // Enter raw mode for key capture and hide terminal cursor
    // (cursor position is already rendered visually by render_input)
    enable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), crossterm::cursor::Hide)?;

    // Guard: ensure raw mode is disabled + cursor shown even on panic
    pub(crate) struct RawModeGuard;
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
            let _ = disable_raw_mode();
        }
    }
    let _raw_guard = RawModeGuard;

    let mut toast_manager = ToastManager::new();

    let result = repl_loop(
        &mut buffer,
        &mut dropdown,
        &mut commands,
        &mut conversation,
        &mut current_session,
        &stats,
        &mut model_info,
        &mut toast_manager,
    )
    .await;

    // Explicitly disable (guard also does this on drop)
    drop(_raw_guard);
    crossterm::execute!(std::io::stdout(), crossterm::cursor::Show).ok();
    disable_raw_mode().ok();

    // Auto-save session on exit
    if !current_session.messages.is_empty() {
        let _ = crate::session::save(&current_session);
    }

    print_goodbye(&stats);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    // ── /queue slash command tests ──────────────────────────────

    #[cfg(test)]
    mod slash_queue_tests {
        use super::*;

        #[test]
        fn thoughts_toggles_reasoning_output() {
            assert!(matches!(
                handle_slash_command("/thoughts"),
                Some(ReplAction::ThoughtsToggle)
            ));
            // Alias kept discoverable for people who type "thinking".
            assert!(matches!(
                handle_slash_command("/thinking"),
                Some(ReplAction::ThoughtsToggle)
            ));
        }

        #[test]
        fn set_show_thoughts_overrides_config_for_the_session() {
            let mut commands = Commands::new(takka_core::Config::fallback());
            let config_default = commands.show_thoughts();
            commands.set_show_thoughts(!config_default);
            assert_eq!(commands.show_thoughts(), !config_default);
        }

        #[test]
        fn timeline_command_sets_and_shows_the_style() {
            assert!(matches!(
                handle_slash_command("/timeline"),
                Some(ReplAction::TimelineShow)
            ));
            assert!(matches!(
                handle_slash_command("/timeline compact"),
                Some(ReplAction::TimelineSet(TimelineStyle::Compact))
            ));
            assert!(matches!(
                handle_slash_command("/timeline detailed"),
                Some(ReplAction::TimelineSet(TimelineStyle::Detailed))
            ));
            // Unknown argument prints usage instead of switching modes.
            assert!(handle_slash_command("/timeline nope").is_none());
        }

        #[test]
        fn timeline_style_defaults_to_detailed_and_is_settable() {
            let mut commands = Commands::new(takka_core::Config::fallback());
            assert_eq!(commands.timeline_style(), TimelineStyle::Detailed);
            commands.set_timeline_style(TimelineStyle::Compact);
            assert_eq!(commands.timeline_style(), TimelineStyle::Compact);
        }

        #[test]
        fn queue_without_arg_maps_to_show() {
            assert!(matches!(
                handle_slash_command("/queue"),
                Some(ReplAction::QueueShow)
            ));
        }

        #[test]
        fn queue_interrupt_switches_mode() {
            assert!(matches!(
                handle_slash_command("/queue interrupt"),
                Some(ReplAction::QueueSet(QueueMode::Interrupt))
            ));
        }

        #[test]
        fn queue_follow_up_switches_mode() {
            assert!(matches!(
                handle_slash_command("/queue follow-up"),
                Some(ReplAction::QueueSet(QueueMode::FollowUp))
            ));
        }

        #[test]
        fn queue_unknown_arg_shows_usage_instead() {
            // Usage line is printed inline; no action is produced.
            assert!(handle_slash_command("/queue wat").is_none());
        }

        #[tokio::test]
        async fn queue_end_to_end_switch_and_display() {
            // Runtime side of the slash command: switch + display helpers see
            // the same state the parser produced. Queue in FollowUp mode
            // first, then switch — the queued items stay visible (brief:
            // "ganti mode; isi antrian tampil").
            let mut commands = Commands::new(takka_core::Config::fallback());
            commands
                .submit_user_text("pending text".to_string())
                .await
                .unwrap();
            assert_eq!(commands.pending_follow_ups().len(), 1);
            commands.set_queue_mode(QueueMode::Interrupt);
            assert_eq!(commands.queue_mode_getter(), QueueMode::Interrupt);
            show_queue_status(&commands); // smoke: must not panic
        }
    }
}
