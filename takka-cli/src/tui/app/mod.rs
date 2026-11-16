//! Main TUI application state and event loop
//!
//! Handles:
//! - Input editing with cursor support
//! - `@` file search dropdown (type @ anywhere to search files)
//! - `/` command dropdown (type / at start to see commands)
//! - Arrow key navigation for both dropdowns
//! - Tab/Enter to accept, Esc to dismiss
//! - History navigation with ↑/↓
//!
//! Layout (mechanical split, no behaviour change): this file is the entry
//! point (state, message/`AppMessage` types, construction, the terminal run
//! loop and key dispatch). Session lifecycle lives in `sessions`, input
//! editing/dropdowns/submission in `input`, slash-command handlers in
//! `slash`, and the async message pump in `messages`.

mod input;
mod messages;
mod sessions;
mod slash;

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

/// If the agent runs longer than this without sending any message back
/// through the channel, `process_messages` auto-resets the loading state
/// and surfaces a warning. Prevents permanent "stuck on thinking" when
/// the spawned task panics or the channel silently drops.
const LOADING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

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
pub(crate) enum InputTarget {
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
pub(crate) fn find_at_trigger(text: &str, cursor: usize) -> Option<usize> {
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
    pub(crate) commands: Option<Arc<Mutex<Commands>>>,
    /// Message channel receiver
    pub(crate) rx: Option<mpsc::UnboundedReceiver<AppMessage>>,
    /// Message channel sender (for async tasks)
    pub(crate) tx: mpsc::UnboundedSender<AppMessage>,
    /// Last tick for animations
    pub(crate) last_tick: Instant,
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
    pub(crate) loading_started: Option<Instant>,
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
            let _render_gate = takka_core::sync::lock(&crate::commands::RENDER_GATE);

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
pub(crate) async fn handle_key_event(app: &mut App, key: crossterm::event::KeyEvent) {
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
            } else if !app.is_loading {
                // Mid-run cursor keys only move the hidden idle buffer's
                // cursor (visible nowhere) — keep them idle-only.
                app.move_cursor_left();
            }
        }
        KeyCode::Right => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.scroll_down();
            } else if !app.is_loading {
                // See the Left arm: idle-only cursor movement.
                app.move_cursor_right();
            }
        }
        KeyCode::Home if !app.is_loading => app.move_cursor_start(),
        KeyCode::End if !app.is_loading => app.move_cursor_end(),
        KeyCode::Up => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.scroll_up();
            } else if !app.is_loading {
                // History browsing edits the idle buffer — meaning while a
                // run is loading that buffer isn't even the pane's content
                // (story 3.1 echoes mid_run_input instead). Mid-run a
                // plain Up/Down must not silently rewrite the hidden idle
                // buffer; it is a no-op (the dropdown engine is NOT active:
                // no dropdown open or the interception block above ran).
                app.history_up();
            }
        }
        KeyCode::Down => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.scroll_down();
            } else if !app.is_loading {
                // See the Up arm: no hidden-idle-buffer history edits mid-run.
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

    /// Plain Up/Down mid-run must NOT browse history: history edits the
    /// idle buffer, which the loading pane no longer renders (story 3.1
    /// echo) — the mutation would be silent. Idle still navigates history.
    #[tokio::test(flavor = "multi_thread")]
    async fn tui_mid_run_history_keys_leave_buffers_alone() {
        let (mut app, _commands) = test_app();
        app.input = "pre-run prompt".into();
        app.cursor_pos = "pre-run prompt".len();
        app.history = vec!["older entry".to_string()];
        app.is_loading = true;
        type_mid_run(&mut app, "steering text");

        handle_key_event(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)).await;
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)).await;
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)).await;
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)).await;
        handle_key_event(&mut app, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)).await;
        handle_key_event(&mut app, KeyEvent::new(KeyCode::End, KeyModifiers::NONE)).await;

        // Nothing changed: neither buffer, nor history state.
        assert_eq!(app.input, "pre-run prompt");
        assert_eq!(app.cursor_pos, "pre-run prompt".len());
        assert_eq!(app.history_index, -1);
        assert_eq!(app.mid_run_input, "steering text");
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
