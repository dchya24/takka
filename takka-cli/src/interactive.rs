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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::cli::SkillAction;
use crate::commands::{Commands, QueueMode, TimelineStyle};
use crate::input_buffer::InputBuffer;
use crate::input_renderer::{self, PromptMetadata};
use crate::tui::dropdown::{Dropdown, DropdownType};
use crate::widgets::components;
use crate::widgets::inline;
use crate::widgets::spinner;
use crate::widgets::toast::{Toast, ToastManager};

// ── Session statistics ──────────────────────────────────────

#[derive(Clone)]
struct SessionStats {
    messages_sent: Arc<AtomicU32>,
    tool_calls: Arc<AtomicU32>,
    total_input_tokens: Arc<AtomicU32>,
    total_output_tokens: Arc<AtomicU32>,
    total_cache_read_tokens: Arc<AtomicU32>,
    total_cache_creation_tokens: Arc<AtomicU32>,
    session_start: Instant,
}

impl SessionStats {
    fn new() -> Self {
        Self {
            messages_sent: Arc::new(AtomicU32::new(0)),
            tool_calls: Arc::new(AtomicU32::new(0)),
            total_input_tokens: Arc::new(AtomicU32::new(0)),
            total_output_tokens: Arc::new(AtomicU32::new(0)),
            total_cache_read_tokens: Arc::new(AtomicU32::new(0)),
            total_cache_creation_tokens: Arc::new(AtomicU32::new(0)),
            session_start: Instant::now(),
        }
    }

    fn increment_messages(&self) {
        self.messages_sent.fetch_add(1, Ordering::Relaxed);
    }

    fn reset(&self) {
        self.messages_sent.store(0, Ordering::Relaxed);
        self.tool_calls.store(0, Ordering::Relaxed);
        self.total_input_tokens.store(0, Ordering::Relaxed);
        self.total_output_tokens.store(0, Ordering::Relaxed);
        self.total_cache_read_tokens.store(0, Ordering::Relaxed);
        self.total_cache_creation_tokens.store(0, Ordering::Relaxed);
    }

    fn add_tool_calls(&self, n: u32) {
        if n > 0 {
            self.tool_calls.fetch_add(n, Ordering::Relaxed);
        }
    }

    fn add_input_tokens(&self, n: u32) {
        self.total_input_tokens.fetch_add(n, Ordering::Relaxed);
    }

    fn add_output_tokens(&self, n: u32) {
        self.total_output_tokens.fetch_add(n, Ordering::Relaxed);
    }

    fn add_cache_read_tokens(&self, n: u32) {
        self.total_cache_read_tokens.fetch_add(n, Ordering::Relaxed);
    }

    fn add_cache_creation_tokens(&self, n: u32) {
        self.total_cache_creation_tokens
            .fetch_add(n, Ordering::Relaxed);
    }

    fn total_cache_read_tokens(&self) -> u32 {
        self.total_cache_read_tokens.load(Ordering::Relaxed)
    }

    fn total_cache_creation_tokens(&self) -> u32 {
        self.total_cache_creation_tokens.load(Ordering::Relaxed)
    }

    fn cache_hit_ratio(&self) -> f64 {
        let read = self.total_cache_read_tokens() as f64;
        let created = self.total_cache_creation_tokens() as f64;
        let total = read + created;
        if total > 0.0 {
            read / total
        } else {
            0.0
        }
    }

    fn messages_sent(&self) -> u32 {
        self.messages_sent.load(Ordering::Relaxed)
    }

    #[allow(dead_code)]
    fn tool_calls(&self) -> u32 {
        self.tool_calls.load(Ordering::Relaxed)
    }

    fn total_input_tokens(&self) -> u32 {
        self.total_input_tokens.load(Ordering::Relaxed)
    }

    fn total_output_tokens(&self) -> u32 {
        self.total_output_tokens.load(Ordering::Relaxed)
    }

    fn elapsed_secs(&self) -> u64 {
        self.session_start.elapsed().as_secs()
    }

    fn elapsed_str(&self) -> String {
        let secs = self.elapsed_secs();
        if secs < 60 {
            format!("{}s", secs)
        } else {
            format!("{}m {}s", secs / 60, secs % 60)
        }
    }

    fn format_tokens(&self, n: u32) -> String {
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

const SLASH_COMMANDS: &[(&str, &[&str], &str)] = &[
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
struct ConversationEntry {
    role: String,
    content: String,
    timestamp: chrono::DateTime<chrono::Local>,
}

// ── Render state ───────────────────────────────────────────
//
/// Encapsulates the mutable rendering state used by the REPL
/// loop so that cursor-position tracking and line-count
/// calculations are kept together instead of being scattered
/// across local variables.
struct RenderState {
    /// How many terminal lines the previous render covered
    /// (prompt + dropdown).  Used by MoveUp(N) to target the
    /// top of the input area before clearing.
    pub prev_lines: usize,
    /// Whether the status bar (+toasts) needs to be reprinted.
    pub needs_status_bar: bool,
}

impl RenderState {
    fn new() -> Self {
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
    fn clear_input_area(&self, stdout: &mut Stdout) -> io::Result<()> {
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
    fn reset(&mut self) {
        self.prev_lines = 0;
    }

    /// Called when the prompt + dropdown have been rendered.
    /// `lines` is the total number of lines emitted.
    fn after_render(&mut self, lines: usize) {
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
    struct RawModeGuard;
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

// ── Inner REPL loop ─────────────────────────────────────────
//
// Rendering architecture (MoveUp tracking, encapsulated in RenderState):
//
//   On each keystroke:
//     1. RenderState::clear_input_area() — MoveUp(N) + Clear
//     2. render_prompt_line() → returns line count N
//     3. render_dropdown_lines() → returns line count D
//     4. RenderState::after_render() — stores N+D in prev_lines
//
//   Key invariant: **every** render path MUST leave the cursor on a
//   new line below the rendered content so that MoveUp(N) can
//   correctly return to the top of the input area on the next
//   keystroke.  `render_prompt_line` uses `print_line` (which appends
//   `\r\n`) for the last line, guaranteeing this.
//
//   N counts *physical* terminal rows: long input wraps onto extra
//   rows, so render_prompt_line / render_dropdown_lines return the
//   sum of wrapped row counts (see input_renderer::physical_lines),
//   not the number of logical print_line calls.
//
//   Terminal resize: clears render state + reprints status bar.
//
//   We avoid DECSC/DECRC (SavePosition/RestorePosition) because they
//   are unreliable when the terminal scrolls.  MoveUp tracking is
//   deterministic regardless of scroll state.

/// Main REPL loop. Internal; args grouped per call site.
#[allow(clippy::too_many_arguments)]
async fn repl_loop(
    buffer: &mut InputBuffer,
    dropdown: &mut Option<Dropdown>,
    commands: &mut Commands,
    conversation: &mut Vec<ConversationEntry>,
    current_session: &mut crate::session::Session,
    stats: &SessionStats,
    model_info: &mut ModelInfo,
    toast_manager: &mut ToastManager,
) -> Result<()> {
    let mut render = RenderState::new();
    let mut stdout = io::stdout();

    loop {
        // ── Clean up expired toasts ──
        toast_manager.cleanup();

        // ── Print status bar once per prompt cycle ──
        if render.needs_status_bar {
            print_prompt_status_bar(stats);

            // Render active toasts
            if toast_manager.has_toasts() {
                let toast_lines = toast_manager.render();
                for line in toast_lines {
                    inline::print_line(&line);
                }
            }

            render.needs_status_bar = false;
        }

        // ── Clear previous input area ──
        render.clear_input_area(&mut stdout)?;

        // ── Render prompt line first ──
        let meta = PromptMetadata::new(model_info.provider.clone(), model_info.model.clone());
        let prompt_lines = input_renderer::render_prompt_line(&meta, buffer, dropdown.is_some());
        render.prev_lines = prompt_lines;

        // ── Render dropdown below input (if active) ──
        if let Some(ref dd) = dropdown {
            if !dd.is_empty() {
                let dd_lines = input_renderer::render_dropdown_lines(dd);
                render.prev_lines += dd_lines;
            }
        }

        // ── Wait for key event ──
        let event = event::read()?;

        match event {
            Event::Key(key) => {
                if !crate::keyboard::should_process_key_kind(key.kind) {
                    continue;
                }

                // Safety net: an empty dropdown renders nothing but would
                // still swallow Enter / Tab / Up / Down below. update_dropdown
                // already closes empty dropdowns; this guards any path that
                // re-creates one (e.g. after accepting an item).
                if dropdown.as_ref().is_some_and(|dd| dd.is_empty()) {
                    *dropdown = None;
                }

                // If dropdown is open, handle dropdown-specific keys first
                if dropdown.is_some() {
                    match (key.modifiers, key.code) {
                        (KeyModifiers::NONE, KeyCode::Up) => {
                            if let Some(ref mut dd) = dropdown {
                                dd.select_prev();
                            }
                            continue;
                        }
                        (KeyModifiers::NONE, KeyCode::Down) => {
                            if let Some(ref mut dd) = dropdown {
                                dd.select_next();
                            }
                            continue;
                        }
                        (KeyModifiers::NONE, KeyCode::Tab)
                        | (KeyModifiers::NONE, KeyCode::Enter) => {
                            if let Some(_auto_cmd) = accept_dropdown(buffer, dropdown) {
                                // Auto-submit: skill / model selected, execute immediately.
                                // Clear the rendered prompt + dropdown first so the
                                // dropdown disappears the moment the item is accepted;
                                // reset() also prevents the next cycle's MoveUp from
                                // erasing the command output printed below.
                                render.clear_input_area(&mut stdout)?;
                                render.reset();
                                let input = buffer.submit();
                                let dir_name = std::env::current_dir()
                                    .unwrap_or_default()
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("?")
                                    .to_string();
                                inline::print_line(&Line::from(vec![
                                    RSpan::styled(
                                        format!("{}> ", dir_name),
                                        RStyle::default().add_modifier(Modifier::DIM),
                                    ),
                                    RSpan::styled(
                                        input.clone(),
                                        RStyle::default().fg(Color::Rgb(220, 220, 230)),
                                    ),
                                ]));
                                let should_break = handle_input(
                                    &input,
                                    commands,
                                    conversation,
                                    current_session,
                                    stats,
                                    model_info,
                                    toast_manager,
                                )
                                .await;
                                *model_info = get_model_info(commands);
                                render.needs_status_bar = true;
                                if should_break {
                                    return Ok(());
                                }
                                continue;
                            }
                            // Re-evaluate dropdown: if user selected `/skill` the input
                            // is now "/skill " which should trigger the skill dropdown.
                            update_dropdown(buffer, dropdown);
                            continue;
                        }
                        (KeyModifiers::NONE, KeyCode::Esc) => {
                            *dropdown = None;
                            continue;
                        }
                        // Any other key: close dropdown and fall through
                        _ => {
                            *dropdown = None;
                        }
                    }
                }

                // Handle normal input keys
                match (key.modifiers, key.code) {
                    // ── Submit ──
                    (KeyModifiers::NONE, KeyCode::Enter) => {
                        // Clear input area (input + dropdown below)
                        render.clear_input_area(&mut stdout)?;
                        render.reset();
                        *dropdown = None;

                        let input = buffer.submit();
                        if input.is_empty() {
                            inline::print_blank();
                            render.reset();
                            continue;
                        }

                        // Echo the user's input as a permanent line
                        let dir_name = std::env::current_dir()
                            .unwrap_or_default()
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("?")
                            .to_string();
                        inline::print_line(&Line::from(vec![
                            RSpan::styled(
                                format!("{}> ", dir_name),
                                RStyle::default().add_modifier(Modifier::DIM),
                            ),
                            RSpan::styled(
                                input.clone(),
                                RStyle::default().fg(Color::Rgb(220, 220, 230)),
                            ),
                        ]));

                        // Handle input
                        let should_break = handle_input(
                            &input,
                            commands,
                            conversation,
                            current_session,
                            stats,
                            model_info,
                            toast_manager,
                        )
                        .await;

                        // Refresh model_info in case provider changed
                        *model_info = get_model_info(commands);
                        render.needs_status_bar = true;

                        if should_break {
                            return Ok(());
                        }
                    }

                    // ── Exit ──
                    (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
                        render.clear_input_area(&mut stdout)?;
                        return Ok(());
                    }

                    // ── Cancel ──
                    (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                        render.clear_input_area(&mut stdout)?;
                        render.reset();
                        *dropdown = None;
                        buffer.clear();
                        inline::print_blank();
                        inline::print_line(&components::info_badge("Use /quit or Ctrl+D to exit."));
                        inline::print_blank();
                        render.needs_status_bar = true;
                    }

                    // ── Character input (normal + shift/capslock) ──
                    (KeyModifiers::NONE, KeyCode::Char(c))
                    | (KeyModifiers::SHIFT, KeyCode::Char(c)) => {
                        buffer.insert_char(c);
                        buffer.reset_history_browse();
                        update_dropdown(buffer, dropdown);
                    }

                    // ── Shift+Enter for multi-line ──
                    (KeyModifiers::SHIFT, KeyCode::Enter) => {
                        buffer.insert_line_break();
                        buffer.reset_history_browse();
                    }

                    // ── Backspace ──
                    (KeyModifiers::NONE, KeyCode::Backspace) => {
                        // If at start of a line in multi-line mode, merge with previous line
                        if buffer.is_multiline()
                            && buffer.cursor() > 0
                            && buffer.text().as_bytes().get(buffer.cursor() - 1) == Some(&b'\n')
                        {
                            buffer.merge_with_previous_line();
                        } else {
                            buffer.delete_backward();
                        }
                        buffer.reset_history_browse();
                        update_dropdown(buffer, dropdown);
                    }

                    // ── Ctrl+Backspace / Ctrl+W ──
                    (KeyModifiers::CONTROL, KeyCode::Backspace)
                    | (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
                        buffer.delete_word_backward();
                        buffer.reset_history_browse();
                        update_dropdown(buffer, dropdown);
                    }

                    // ── Delete ──
                    (KeyModifiers::NONE, KeyCode::Delete) => {
                        buffer.delete_forward();
                        buffer.reset_history_browse();
                        update_dropdown(buffer, dropdown);
                    }

                    // ── Cursor movement ──
                    (KeyModifiers::NONE, KeyCode::Left) => {
                        buffer.cursor_left();
                    }
                    (KeyModifiers::NONE, KeyCode::Right) => {
                        buffer.cursor_right();
                    }
                    (KeyModifiers::CONTROL, KeyCode::Left) => {
                        buffer.cursor_word_left();
                    }
                    (KeyModifiers::CONTROL, KeyCode::Right) => {
                        buffer.cursor_word_right();
                    }
                    (KeyModifiers::NONE, KeyCode::Home) => {
                        buffer.cursor_home();
                    }
                    (KeyModifiers::NONE, KeyCode::End) => {
                        buffer.cursor_end();
                    }

                    // ── History navigation (only when no dropdown and not multi-line) ──
                    (KeyModifiers::NONE, KeyCode::Up) => {
                        if buffer.is_multiline() {
                            buffer.cursor_up();
                        } else {
                            buffer.history_up();
                        }
                    }
                    (KeyModifiers::NONE, KeyCode::Down) => {
                        if buffer.is_multiline() {
                            buffer.cursor_down();
                        } else {
                            buffer.history_down();
                        }
                    }

                    // ── Ctrl+L — clear screen ──
                    (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
                        render.reset();
                        crossterm::execute!(
                            std::io::stdout(),
                            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                            crossterm::cursor::MoveTo(0, 0)
                        )
                        .ok();
                        print_banner(model_info, stats);
                        render.needs_status_bar = true;
                    }

                    // Ignore other key combinations
                    _ => {}
                }
            }
            Event::Resize(_, _) => {
                // Terminal was resized — the previous rendered line
                // count is now stale.  Force a status bar reprint so
                // the user gets a clean visual anchor.
                render.reset();
                render.needs_status_bar = true;
            }
            _ => {}
        }
    }
}

// ── Dropdown trigger & accept logic ─────────────────────────

/// Discover skills and return (name, description) pairs for the skill dropdown.
fn discover_skill_pairs() -> Vec<(String, String)> {
    let _config_path = takka_core::Config::config_path();
    let config = takka_core::Config::load().unwrap_or_default();
    let discovery_config: takka_core::DiscoveryConfig =
        takka_core::DiscoveryConfig::from(&config.skills);
    let index = takka_core::discover_skills(&discovery_config);
    let mut skills: Vec<_> = index.all().into_iter().collect();
    skills.sort_by(|a, b| a.name().cmp(b.name()));
    skills
        .into_iter()
        .map(|s| (s.name().to_string(), s.description().to_string()))
        .collect()
}

/// Update dropdown state based on current input and cursor position.
///
/// An empty dropdown is closed immediately: it renders nothing, yet if it
/// stayed open it would intercept Enter / Up / Down and swallow those keys.
/// This happens in practice right after accepting a model — the input
/// becomes `/models <id> ` (trailing space) whose query `<id> ` matches no
/// model, so the next Enter, which should submit the `/models <id>`
/// command, would instead be consumed by the empty dropdown and the model
/// would never get selected.
fn update_dropdown(buffer: &InputBuffer, dropdown: &mut Option<Dropdown>) {
    let text = buffer.text();
    let cursor = buffer.cursor();

    let mut next: Option<Dropdown> = None;

    // 1) Check for `/` command trigger
    if text.starts_with('/') {
        let before_cursor = &text[..cursor];
        if !before_cursor.contains(' ') {
            let query = &text[1..cursor];
            // If the query exactly matches a skill command, open skills dropdown directly
            if query == "skills" || query == "skill" || query == "sk" {
                let skill_pairs = discover_skill_pairs();
                next = Some(Dropdown::new_skill(String::new(), skill_pairs));
            } else {
                next = Some(Dropdown::new(DropdownType::Command, query.to_string()));
            }
        } else if let Some(space_pos) = before_cursor.find(' ') {
            let cmd = &text[1..space_pos];
            // 1b) Check for `/models <partial>` model trigger
            if cmd == "models" || cmd == "m" {
                let query = &text[space_pos + 1..cursor];
                next = Some(Dropdown::new(DropdownType::Model, query.to_string()));
            }
            // 1c) Check for `/skill <partial>` skill trigger
            else if cmd == "skill" || cmd == "sk" {
                let query = &text[space_pos + 1..cursor];
                let skill_pairs = discover_skill_pairs();
                next = Some(Dropdown::new_skill(query.to_string(), skill_pairs));
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

    // 3) Commit, closing empty dropdowns so they cannot swallow keys.
    *dropdown = next.filter(|dd| !dd.is_empty());
}

/// Find the byte position of the `@` trigger that the cursor is inside.
fn find_at_trigger(text: &str, cursor: usize) -> Option<usize> {
    let before_cursor = &text[..cursor];

    for (i, c) in before_cursor.char_indices().rev() {
        match c {
            '@' => {
                let at_start = i == 0;
                let after_space = i > 0 && text[..i].ends_with(char::is_whitespace);
                if at_start || after_space {
                    let after_at = &before_cursor[i + 1..];
                    if !after_at.contains(char::is_whitespace) {
                        return Some(i);
                    }
                }
                return None;
            }
            w if w.is_whitespace() => return None,
            _ => continue,
        }
    }
    None
}

/// Accept the currently selected dropdown item and insert into buffer.
///
/// Returns `Some(command)` when the input should be auto-submitted
/// (e.g. after selecting a skill from the skills dropdown), or `None`
/// when the user needs to press Enter again to confirm.
fn accept_dropdown(buffer: &mut InputBuffer, dropdown: &mut Option<Dropdown>) -> Option<String> {
    let dd = dropdown.take()?;

    let selected_text = dd.selected_item()?.to_string();

    let is_dir = selected_text.ends_with('/');

    match dd.dropdown_type {
        DropdownType::Command => {
            let cmd = selected_text.trim_start_matches('/');
            if cmd == "skill" || cmd == "sk" {
                // Skills dropdown is triggered separately — just fill /skill
                buffer.set_text("/skill ".to_string());
            } else {
                buffer.set_text(format!("/{} ", selected_text));
            }
        }
        DropdownType::File => {
            if let Some(at_pos) = find_at_trigger(buffer.text(), buffer.cursor()) {
                let suffix = if is_dir { "" } else { " " };
                let replacement = format!("{}{}", selected_text, suffix);
                buffer.replace_range(at_pos + 1, &replacement);
            }
        }
        DropdownType::Skill => {
            let skill_name = dd
                .get_skill_name(&selected_text)
                .unwrap_or_else(|| selected_text.clone());
            let cmd = format!("/skill {}", skill_name);
            buffer.set_text(cmd.clone());
            return Some(cmd);
        }
        DropdownType::Model => {
            // Auto-submit like the skill dropdown: selecting a model from
            // `/models <partial>` is always the whole command, so switch
            // to it immediately on Enter/Tab.
            //
            // Use the exact (provider, model id) carried by the dropdown
            // and emit a provider-qualified command. A plain id would be
            // re-resolved with a contains() search and could silently hit
            // a different provider that happens to share the display name
            // / id (e.g. "GLM-4.7" in two providers, or an aliased id).
            let (provider, model_id) = dd
                .selected_model_meta()
                .unwrap_or_else(|| (String::new(), selected_text.clone()));
            let cmd = if provider.is_empty() {
                format!("/models {}", model_id)
            } else {
                format!("/models {}/{}", provider, model_id)
            };
            buffer.set_text(cmd.clone());
            return Some(cmd);
        }
    }

    // For directories, re-trigger dropdown to show contents
    if is_dir {
        let mut new_dd = None;
        update_dropdown(buffer, &mut new_dd);
        *dropdown = new_dd;
    }
    None
}

// ── Input handling ──────────────────────────────────────────

/// Handle submitted input. Returns true if the loop should break (quit).
async fn handle_input(
    input: &str,
    commands: &mut Commands,
    conversation: &mut Vec<ConversationEntry>,
    current_session: &mut crate::session::Session,
    stats: &SessionStats,
    model_info: &ModelInfo,
    toast_manager: &mut ToastManager,
) -> bool {
    // Handle slash commands
    if input.starts_with('/') {
        if let Some(action) = handle_slash_command(input) {
            return handle_repl_action(
                &action,
                commands,
                conversation,
                current_session,
                stats,
                model_info,
                toast_manager,
            )
            .await;
        }
        return false;
    }

    // Handle plain text shortcuts
    match input.to_lowercase().as_str() {
        "exit" | "quit" | "q" => return true,
        "help" | "h" => print_help(),
        "new" | "n" => {
            if !current_session.messages.is_empty() {
                let _ = crate::session::save(current_session);
            }
            let cwd = std::env::current_dir()
                .unwrap_or_default()
                .display()
                .to_string();
            *current_session =
                crate::session::create(&cwd, &model_info.provider, &model_info.model);
            conversation.clear();
            stats.reset();
            if let Err(e) = commands.restart_session().await {
                tracing::warn!(error = %e, "session reset through runtime failed");
            }

            crossterm::execute!(
                std::io::stdout(),
                crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                crossterm::cursor::MoveTo(0, 0)
            )
            .ok();
            print_banner(model_info, stats);
            inline::print_blank();
            inline::print_line(&components::success_badge("New session started."));
            inline::print_blank();
            print_status_bar(model_info, stats);

            // Show toast notification
            toast_manager.add(Toast::success("New session started"));
        }
        _ => {
            process_message(
                input,
                commands,
                conversation,
                current_session,
                stats,
                model_info,
            )
            .await;
        }
    }

    false
}

// ── REPL actions ────────────────────────────────────────────

enum ReplAction {
    Quit,
    NewSession,
    Config,
    History,
    Tools,
    Stats,
    Provider(String),
    Models,
    ModelsSwitch(String),
    Sessions,
    SessionsResume(String),
    Mcp,
    Plan(String),
    Skills,
    SkillsLoad(String),
    Search(String),
    Image(String),
    QueueShow,
    QueueSet(QueueMode),
    ThoughtsToggle,
    TimelineShow,
    TimelineSet(TimelineStyle),
}

fn handle_slash_command(input: &str) -> Option<ReplAction> {
    let parts: Vec<&str> = input.splitn(2, ' ').collect();
    let cmd = parts[0];
    let arg = parts
        .get(1)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    match cmd {
        "/quit" | "/q" | "/exit" => Some(ReplAction::Quit),
        "/help" | "/h" => {
            print_help();
            None
        }
        "/new" | "/n" | "/clear" | "/cls" => Some(ReplAction::NewSession),
        "/config" | "/cfg" => Some(ReplAction::Config),
        "/history" | "/hist" => Some(ReplAction::History),
        "/tools" | "/t" => Some(ReplAction::Tools),
        "/stats" => Some(ReplAction::Stats),
        "/mcp" => Some(ReplAction::Mcp),
        "/provider" if !arg.is_empty() => Some(ReplAction::Provider(arg)),
        "/provider" => {
            inline::print_blank();
            inline::print_line(&components::warning_badge("Usage: /provider <name>"));
            inline::print_blank();
            None
        }
        "/models" | "/m" if !arg.is_empty() => Some(ReplAction::ModelsSwitch(arg)),
        "/models" | "/m" => Some(ReplAction::Models),
        "/sessions" | "/ss" if !arg.is_empty() => Some(ReplAction::SessionsResume(arg)),
        "/sessions" | "/ss" => Some(ReplAction::Sessions),
        "/plan" if !arg.is_empty() => Some(ReplAction::Plan(arg)),
        "/plan" => {
            inline::print_blank();
            inline::print_line(&components::warning_badge("Usage: /plan <goal>"));
            inline::print_blank();
            None
        }
        "/skills" | "/skill" | "/sk" if !arg.is_empty() => Some(ReplAction::SkillsLoad(arg)),
        "/skills" => Some(ReplAction::Skills),
        "/skill" | "/sk" => Some(ReplAction::Skills),
        "/search" | "/find" if !arg.is_empty() => Some(ReplAction::Search(arg)),
        "/search" | "/find" => {
            inline::print_blank();
            inline::print_line(&components::warning_badge(
                "Usage: /search <query>  (case-insensitive substring match over conversation memory)",
            ));
            inline::print_blank();
            None
        }
        "/image" | "/img" if !arg.is_empty() => Some(ReplAction::Image(arg)),
        "/image" | "/img" => {
            inline::print_blank();
            inline::print_line(&components::warning_badge(
                "Usage: /image <path | data: url | http(s) url>",
            ));
            inline::print_blank();
            None
        }
        "/queue" if !arg.is_empty() => match arg.as_str() {
            "interrupt" => Some(ReplAction::QueueSet(QueueMode::Interrupt)),
            "follow-up" => Some(ReplAction::QueueSet(QueueMode::FollowUp)),
            _ => {
                inline::print_blank();
                inline::print_line(&components::warning_badge(
                    "Usage: /queue [follow-up | interrupt]",
                ));
                inline::print_blank();
                None
            }
        },
        "/queue" => Some(ReplAction::QueueShow),
        "/thoughts" | "/thinking" => Some(ReplAction::ThoughtsToggle),
        "/timeline" if !arg.is_empty() => match arg.as_str() {
            "detail" | "detailed" | "full" => {
                Some(ReplAction::TimelineSet(TimelineStyle::Detailed))
            }
            "compact" | "short" | "min" => Some(ReplAction::TimelineSet(TimelineStyle::Compact)),
            _ => {
                inline::print_blank();
                inline::print_line(&components::warning_badge(
                    "Usage: /timeline [detailed | compact]",
                ));
                inline::print_blank();
                None
            }
        },
        "/timeline" => Some(ReplAction::TimelineShow),
        _ => {
            inline::print_blank();
            inline::print_line(&components::error_badge(&format!(
                "Unknown command: {}",
                cmd
            )));
            inline::print_line(&Line::from(vec![
                RSpan::raw("  Type "),
                RSpan::styled(
                    "/help",
                    RStyle::default()
                        .fg(Color::Rgb(255, 215, 0))
                        .add_modifier(Modifier::BOLD),
                ),
                RSpan::raw(" for available commands."),
            ]));
            inline::print_blank();
            None
        }
    }
}

/// Handle a ReplAction. Returns true if the loop should break (quit).
async fn handle_repl_action(
    action: &ReplAction,
    commands: &mut Commands,
    conversation: &mut Vec<ConversationEntry>,
    current_session: &mut crate::session::Session,
    stats: &SessionStats,
    model_info: &ModelInfo,
    toast_manager: &mut ToastManager,
) -> bool {
    match action {
        ReplAction::Quit => return true,

        ReplAction::NewSession => {
            if !current_session.messages.is_empty() {
                if let Err(e) = crate::session::save(current_session) {
                    inline::print_line(&components::warning_badge(&format!(
                        "Could not auto-save session: {}",
                        e
                    )));
                }
            }
            let cwd = std::env::current_dir()
                .unwrap_or_default()
                .display()
                .to_string();
            *current_session =
                crate::session::create(&cwd, &model_info.provider, &model_info.model);
            conversation.clear();
            stats.reset();
            if let Err(e) = commands.restart_session().await {
                tracing::warn!(error = %e, "session reset through runtime failed");
            }

            crossterm::execute!(
                std::io::stdout(),
                crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                crossterm::cursor::MoveTo(0, 0)
            )
            .ok();
            print_banner(model_info, stats);
            inline::print_blank();
            inline::print_line(&components::success_badge("New session started."));
            inline::print_blank();
            print_status_bar(model_info, stats);

            // Show toast notification
            toast_manager.add(Toast::success("New session started"));
        }

        ReplAction::Config => {
            commands.config_show_inline();
        }

        ReplAction::History => {
            show_history(conversation);
        }

        ReplAction::Tools => {
            commands.list_tools().await;
        }

        ReplAction::Stats => {
            show_stats(stats, model_info);
        }

        ReplAction::Sessions => {
            show_sessions();
        }

        ReplAction::SessionsResume(id) => {
            match crate::session::load(id) {
                Ok(loaded) => {
                    if !current_session.messages.is_empty() {
                        let _ = crate::session::save(current_session);
                    }
                    conversation.clear();
                    for msg in &loaded.messages {
                        conversation.push(ConversationEntry {
                            role: msg.role.clone(),
                            content: msg.content.clone(),
                            timestamp: chrono::DateTime::parse_from_rfc3339(&msg.timestamp)
                                .map(|dt| dt.with_timezone(&chrono::Local))
                                .unwrap_or_else(|_| chrono::Local::now()),
                        });
                    }
                    *current_session = loaded;
                    if let Err(e) = commands.restart_session().await {
                        tracing::warn!(error = %e, "session reset through runtime failed");
                    }
                    stats.reset();

                    crossterm::execute!(
                        std::io::stdout(),
                        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                        crossterm::cursor::MoveTo(0, 0)
                    )
                    .ok();
                    print_banner(model_info, stats);
                    inline::print_blank();
                    inline::print_line(&components::success_badge(&format!(
                        "Resumed: {} ({} messages)",
                        current_session.title,
                        current_session.messages.len()
                    )));
                    inline::print_blank();
                    print_status_bar(model_info, stats);

                    // Show toast notification
                    toast_manager.add(Toast::success(format!(
                        "Session resumed: {}",
                        current_session.title
                    )));
                }
                Err(e) => {
                    inline::print_blank();
                    inline::print_line(&components::error_badge(&format!(
                        "Failed to load session: {}",
                        e
                    )));
                    inline::print_blank();

                    // Show toast notification
                    toast_manager.add(Toast::error(format!("Failed to load session: {}", e)));
                }
            }
        }

        ReplAction::Provider(name) => {
            inline::print_blank();
            inline::print_line(&components::warning_badge(
                "Provider switching not yet supported in REPL.",
            ));
            inline::print_line(&Line::from(vec![
                RSpan::raw("  Use: "),
                RSpan::styled(
                    "takka config edit",
                    RStyle::default().add_modifier(Modifier::BOLD),
                ),
                RSpan::raw(" to change providers."),
            ]));
            inline::print_blank();
            let _ = name;
        }

        ReplAction::Models => {
            // dialoguer needs cooked mode (it controls the terminal itself)
            disable_raw_mode().ok();
            let result = commands.pick_model_interactive_inline();
            enable_raw_mode().ok();
            if let Some((provider, model)) = result {
                inline::print_blank();
                inline::print_line(&components::success_badge(&format!(
                    "Switched to {} / {}",
                    provider, model
                )));
                inline::print_blank();
                print_status_bar(model_info, stats);

                // Show toast notification
                toast_manager.add(Toast::success(format!(
                    "Switched to {} / {}",
                    provider, model
                )));
            }
        }

        ReplAction::ModelsSwitch(name) => {
            match commands.switch_model(name) {
                Ok((provider, model)) => {
                    inline::print_blank();
                    inline::print_line(&components::success_badge(&format!(
                        "Switched to {} / {}",
                        provider, model
                    )));
                    inline::print_blank();

                    // Show toast notification
                    toast_manager.add(Toast::success(format!(
                        "Switched to {} / {}",
                        provider, model
                    )));
                }
                Err(e) => {
                    inline::print_blank();
                    inline::print_line(&components::error_badge(&e.to_string()));
                    inline::print_line(&Line::from(vec![
                        RSpan::raw("  Use "),
                        RSpan::styled("/models", RStyle::default().add_modifier(Modifier::BOLD)),
                        RSpan::raw(" to see available models."),
                    ]));
                    inline::print_blank();

                    // Show toast notification
                    toast_manager.add(Toast::error(e.to_string()));
                }
            }
        }

        ReplAction::Mcp => {
            commands.show_mcp_status();
        }

        ReplAction::Plan(goal) => {
            conversation.push(ConversationEntry {
                role: "user".into(),
                content: format!("[plan] {}", goal),
                timestamp: chrono::Local::now(),
            });
            stats.increment_messages();

            print_turn_separator(model_info);
            let start = Instant::now();
            if let Err(e) = commands.plan_inline(goal).await {
                inline::print_blank();
                inline::print_line(&components::error_badge(&e.to_string()));
                inline::print_blank();
            } else {
                let elapsed = start.elapsed();
                conversation.push(ConversationEntry {
                    role: "assistant".into(),
                    content: format!("(plan executed in {:.1}s)", elapsed.as_secs_f64()),
                    timestamp: chrono::Local::now(),
                });
                print_response_summary(stats);
            }
        }

        ReplAction::Skills => {
            commands.skill_command(&SkillAction::List).ok();
        }

        ReplAction::SkillsLoad(name) => {
            use ratatui::style::{Color, Modifier, Style};
            use ratatui::text::{Line, Span};

            inline::print_blank();
            inline::print_line(&components::section_header(
                "⚡",
                &format!("Activating skill: {}", name),
                Color::Rgb(255, 215, 0),
            ));
            inline::print_blank();

            // Load & activate: inject instructions into orchestrator context
            match commands.load_and_activate_skill(name).await {
                Ok(body) => {
                    // Success — show activation badge + preview
                    inline::print_line(&Line::from(vec![
                        Span::styled("  ✅ ", Style::default().fg(Color::Rgb(46, 204, 113))),
                        Span::styled(
                            format!(
                                "Skill '{}' activated — instructions injected into agent context.",
                                name
                            ),
                            Style::default()
                                .fg(Color::Rgb(180, 180, 200))
                                .add_modifier(Modifier::DIM),
                        ),
                    ]));
                    inline::print_blank();
                    inline::print_line(&Line::from(vec![Span::styled(
                        "  📖 Preview:",
                        Style::default()
                            .fg(Color::Rgb(241, 196, 15))
                            .add_modifier(Modifier::BOLD),
                    )]));
                    let preview: Vec<&str> = body.lines().take(5).collect();
                    for line in &preview {
                        inline::print_line(&Line::from(vec![
                            Span::raw("     "),
                            Span::styled(*line, Style::default().fg(Color::Rgb(180, 180, 200))),
                        ]));
                    }
                    if body.lines().count() > 5 {
                        inline::print_line(&Line::from(vec![
                            Span::raw("     "),
                            Span::styled("...", Style::default().fg(Color::Rgb(100, 100, 120))),
                        ]));
                    }
                    inline::print_blank();
                    inline::print_line(&Line::from(vec![
                        Span::raw("  💡 "),
                        Span::styled(
                            "Now send a message — the skill instructions will be included as context.",
                            Style::default()
                                .fg(Color::Rgb(100, 100, 120))
                                .add_modifier(Modifier::DIM),
                        ),
                    ]));
                }
                Err(e) => {
                    inline::print_line(&components::warning_badge(&format!(
                        "Failed to activate skill '{}': {}",
                        name, e
                    )));
                }
            }
            inline::print_blank();
        }

        ReplAction::Search(query) => {
            commands.search_memory_inline(query).await;
        }

        ReplAction::Image(path) => {
            commands.attach_image_inline(path);
        }

        ReplAction::QueueShow => {
            show_queue_status(commands);
        }

        ReplAction::QueueSet(mode) => {
            commands.set_queue_mode(*mode);
            inline::print_blank();
            inline::print_line(&components::success_badge(&format!(
                "Queue mode set to {} — {}",
                mode.as_str(),
                mode.description()
            )));
            show_queue_status(commands);
        }

        ReplAction::TimelineShow => {
            show_timeline_status(commands);
        }

        ReplAction::TimelineSet(style) => {
            commands.set_timeline_style(*style);
            inline::print_blank();
            inline::print_line(&components::success_badge(&format!(
                "Timeline set to {} — {}",
                style.as_str(),
                style.description()
            )));
            inline::print_blank();
        }

        ReplAction::ThoughtsToggle => {
            let enabled = !commands.show_thoughts();
            commands.set_show_thoughts(enabled);
            inline::print_blank();
            inline::print_line(&components::success_badge(if enabled {
                "Reasoning shown — the model's pre-tool thinking is printed DIM."
            } else {
                "Reasoning hidden — only answers and tool activity are shown."
            }));
            inline::print_blank();
        }
    }

    false
}

/// `/timeline` display: current detail level and what it means.
fn show_timeline_status(commands: &Commands) {
    let style = commands.timeline_style();
    inline::print_blank();
    inline::print_line(&Line::from(vec![
        RSpan::raw("Tool timeline: "),
        RSpan::styled(
            style.as_str(),
            RStyle::default()
                .fg(Color::Rgb(64, 224, 208))
                .add_modifier(Modifier::BOLD),
        ),
        RSpan::styled(format!(" — {}", style.description()), RStyle::default()),
    ]));
    inline::print_line(&Line::from(RSpan::styled(
        "  /timeline compact  one row per tool   ·   /timeline detailed  rows + nested detail",
        RStyle::default().add_modifier(Modifier::DIM),
    )));
    inline::print_blank();
}

/// `/queue` display: active mode + pending follow-up queue (index +
/// first 80 characters of each entry). Only reachable while the REPL is
/// idle; input-during-run wiring comes with the TUI phase.
fn show_queue_status(commands: &Commands) {
    let mode = commands.queue_mode_getter();
    inline::print_blank();
    inline::print_line(&Line::from(vec![
        RSpan::raw("Queue mode: "),
        RSpan::styled(
            mode.as_str(),
            RStyle::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        RSpan::raw(format!(" — {}", mode.description())),
    ]));

    let pending = commands.pending_follow_ups();
    if pending.is_empty() {
        inline::print_line(&Line::from(RSpan::styled(
            "Pending follow-ups: (none)",
            RStyle::default().add_modifier(Modifier::DIM),
        )));
    } else {
        inline::print_line(&Line::from(format!(
            "Pending follow-ups: {}",
            pending.len()
        )));
        for (i, text) in pending.iter().enumerate() {
            let preview: String = text.chars().take(80).collect();
            let ellipsis = if text.chars().count() > 80 { "…" } else { "" };
            inline::print_line(&Line::from(vec![
                RSpan::styled(
                    format!("  {}. ", i + 1),
                    RStyle::default().fg(Color::DarkGray),
                ),
                RSpan::raw(format!("{preview}{ellipsis}")),
            ]));
        }
    }
    inline::print_blank();
}

// ── Message processing ────────────────────────────────────

/// Process a single user message through the agent.
async fn process_message(
    input: &str,
    commands: &mut Commands,
    conversation: &mut Vec<ConversationEntry>,
    current_session: &mut crate::session::Session,
    stats: &SessionStats,
    model_info: &ModelInfo,
) {
    conversation.push(ConversationEntry {
        role: "user".into(),
        content: input.to_string(),
        timestamp: chrono::Local::now(),
    });
    stats.increment_messages();

    crate::session::push_message(current_session, "user", input);

    print_turn_separator(model_info);
    inline::print_line(&components::dotted_separator(Color::Rgb(60, 60, 80)));
    inline::print_blank();

    let start = Instant::now();
    // Ctrl+C cancellation is handled by the signal handler in main.rs,
    // which sets the global CANCEL_FLAG.  The orchestrator checks this
    // flag at every turn boundary and returns gracefully.
    let result = commands.run(input).await;

    if let Err(e) = result {
        inline::print_blank();
        inline::print_line(&components::error_badge(&e.to_string()));
        inline::print_blank();
    } else {
        let elapsed = start.elapsed();
        // Real provider usage when the runtime reported it, otherwise the
        // ~4-chars-per-token estimate.
        let run = commands.last_run_stats();
        let measured = run.tokens_are_measured();
        let (input_tokens, output_tokens) = run.tokens();
        stats.add_input_tokens(input_tokens);
        stats.add_output_tokens(output_tokens);
        stats.add_cache_read_tokens(run.cache_read_tokens);
        stats.add_cache_creation_tokens(run.cache_creation_tokens);
        stats.add_tool_calls(run.tool_calls);

        conversation.push(ConversationEntry {
            role: "assistant".into(),
            content: format!("(response in {:.1}s)", elapsed.as_secs_f64()),
            timestamp: chrono::Local::now(),
        });

        crate::session::push_message(
            current_session,
            "assistant",
            &format!("(response in {:.1}s)", elapsed.as_secs_f64()),
        );
        let _ = crate::session::save(current_session);

        // One-line run summary: outcome, wall time, tools, this run's tokens.
        let done_line = spinner::run_summary_line(
            elapsed.as_millis(),
            input_tokens,
            output_tokens,
            run.tool_calls,
            measured,
        );
        inline::print_line(&done_line);
        inline::print_blank();

        print_response_summary(stats);
    }
}

// ── Model info ──────────────────────────────────────────────

struct ModelInfo {
    provider: String,
    model: String,
    api_base: String,
    agent_md_name: Option<String>,
    memory_md_loaded: bool,
    vision_capable: bool,
    active_skill: Option<String>,
    /// Session skill list (per-instance, Fase D) for the banner.
    available_skills: Vec<(String, String)>,
}

fn get_model_info(commands: &Commands) -> ModelInfo {
    let (provider, model, api_base) = commands.model_info();
    let agent_md_name = commands
        .agent_md_path()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string());
    ModelInfo {
        provider,
        model,
        api_base,
        agent_md_name,
        memory_md_loaded: commands.memory_md_loaded(),
        vision_capable: commands.active_model_capabilities().vision,
        active_skill: commands.active_skill_name(),
        available_skills: commands.available_skills(),
    }
}

// ── Print helpers ───────────────────────────────────────────

fn print_banner(model_info: &ModelInfo, stats: &SessionStats) {
    let cwd = std::env::current_dir()
        .unwrap_or_default()
        .display()
        .to_string();

    inline::print_blank();

    // TAKKA wordmark, drawn with block glyphs. Each row gets its own
    // left-to-right gradient so the block reads as a diagonal sweep.
    const LOGO_ROWS: [&str; 3] = [
        "  █████  ▄▀▀▄  █  █  █  █  ▄▀▀▄",
        "    █    █▄▄█  ███   ███   █▄▄█",
        "    █    █  █  █  █  █  █  █  █",
    ];
    let logo_from = Color::Rgb(255, 105, 180);
    let logo_to = Color::Rgb(64, 224, 208);
    for row in LOGO_ROWS.iter() {
        let title = components::banner_title(row, logo_from, logo_to);
        inline::print_line(&title);
    }
    inline::print_blank();

    let info_lines = vec![
        Line::from(vec![
            RSpan::styled("📂 ", RStyle::default()),
            RSpan::styled("cwd  ", RStyle::default().add_modifier(Modifier::DIM)),
            RSpan::styled(cwd, RStyle::default().fg(Color::Rgb(180, 180, 200))),
        ]),
        Line::from(vec![
            RSpan::styled("⚡ ", RStyle::default()),
            RSpan::styled("model", RStyle::default().add_modifier(Modifier::DIM)),
            RSpan::raw("  "),
            RSpan::styled(
                model_info.provider.clone(),
                RStyle::default()
                    .fg(Color::Rgb(64, 224, 208))
                    .add_modifier(Modifier::BOLD),
            ),
            RSpan::styled(" / ", RStyle::default().add_modifier(Modifier::DIM)),
            RSpan::styled(
                model_info.model.clone(),
                RStyle::default()
                    .fg(Color::Rgb(255, 215, 0))
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            RSpan::styled("💡 ", RStyle::default()),
            RSpan::styled("tip  ", RStyle::default().add_modifier(Modifier::DIM)),
            RSpan::styled("type ", RStyle::default()),
            RSpan::styled(
                "/help",
                RStyle::default()
                    .fg(Color::Rgb(255, 215, 0))
                    .add_modifier(Modifier::BOLD),
            ),
            RSpan::styled(" for commands, ", RStyle::default()),
            RSpan::styled(
                "@",
                RStyle::default()
                    .fg(Color::Rgb(135, 206, 250))
                    .add_modifier(Modifier::BOLD),
            ),
            RSpan::styled(" to reference files", RStyle::default()),
        ]),
    ];

    let mut info_lines = info_lines;
    if model_info.agent_md_name.is_some()
        || model_info.memory_md_loaded
        || model_info.active_skill.is_some()
    {
        let mut spans: Vec<RSpan<'static>> = vec![
            RSpan::styled("🔗 ", RStyle::default()),
            RSpan::styled("ctx  ", RStyle::default().add_modifier(Modifier::DIM)),
        ];
        let mut first = true;
        if let Some(ref name) = model_info.agent_md_name {
            spans.push(RSpan::styled(
                format!("📄 {}", name),
                RStyle::default()
                    .fg(Color::Rgb(176, 196, 222))
                    .add_modifier(Modifier::BOLD),
            ));
            first = false;
        }
        if model_info.memory_md_loaded {
            if !first {
                spans.push(RSpan::styled(
                    "  ·  ",
                    RStyle::default().add_modifier(Modifier::DIM),
                ));
            }
            spans.push(RSpan::styled(
                "🧠 memory.md",
                RStyle::default()
                    .fg(Color::Rgb(176, 196, 222))
                    .add_modifier(Modifier::BOLD),
            ));
            first = false;
        }
        if let Some(ref skill) = model_info.active_skill {
            if !first {
                spans.push(RSpan::styled(
                    "  ·  ",
                    RStyle::default().add_modifier(Modifier::DIM),
                ));
            }
            spans.push(RSpan::styled(
                format!("⚡ skill:{}", skill),
                RStyle::default()
                    .fg(Color::Rgb(241, 196, 15))
                    .add_modifier(Modifier::BOLD),
            ));
        }
        info_lines.push(Line::from(spans));
    }

    let skills = &model_info.available_skills;
    if !skills.is_empty() {
        const MAX_SKILL_NAMES: usize = 5;
        let mut skill_names: Vec<String> = skills
            .iter()
            .take(MAX_SKILL_NAMES)
            .map(|(name, _)| name.clone())
            .collect();
        let remaining = skills.len().saturating_sub(MAX_SKILL_NAMES);
        if remaining > 0 {
            skill_names.push(format!("+{} more", remaining));
        }
        let skill_str = skill_names.join(" · ");
        info_lines.push(Line::from(vec![
            RSpan::styled("📦 ", RStyle::default()),
            RSpan::styled("skills", RStyle::default().add_modifier(Modifier::DIM)),
            RSpan::raw(" "),
            RSpan::styled(skill_str, RStyle::default().fg(Color::Rgb(241, 196, 15))),
        ]));
    }

    let panel_lines = components::panel(
        "Welcome",
        &info_lines,
        components::BoxStyle::Rounded,
        Color::Rgb(100, 100, 140),
    );
    inline::print_lines(&panel_lines);
    inline::print_blank();

    print_status_bar(model_info, stats);
}

fn print_turn_separator(model_info: &ModelInfo) {
    let git_branch = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let b = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if b.is_empty() || b == "HEAD" {
                    None
                } else {
                    Some(b)
                }
            } else {
                None
            }
        });

    let mut spans = vec![
        RSpan::raw("  "),
        RSpan::styled(
            format!("⚡ {}/{}", model_info.provider, model_info.model),
            RStyle::default().fg(Color::Rgb(255, 215, 0)),
        ),
    ];

    if let Some(ref branch) = git_branch {
        spans.push(RSpan::styled(
            format!(" 📌 {}", branch),
            RStyle::default()
                .fg(Color::Rgb(135, 206, 250))
                .add_modifier(Modifier::DIM),
        ));
    }

    inline::print_line(&Line::from(spans));
}

fn print_status_bar(model_info: &ModelInfo, stats: &SessionStats) {
    let in_tok = stats.format_tokens(stats.total_input_tokens());
    let out_tok = stats.format_tokens(stats.total_output_tokens());

    let sep = RSpan::styled("  │  ", RStyle::default().fg(Color::Rgb(60, 60, 80)));

    inline::print_line(&Line::from(vec![
        RSpan::raw("  "),
        RSpan::styled(
            format!("⚡ {}", model_info.provider),
            RStyle::default().fg(Color::Rgb(255, 215, 0)),
        ),
        RSpan::styled(
            format!("/{}", model_info.model),
            RStyle::default()
                .fg(Color::Rgb(241, 196, 15))
                .add_modifier(Modifier::DIM),
        ),
        if model_info.vision_capable {
            RSpan::styled("  👁", RStyle::default().fg(Color::Rgb(135, 206, 250)))
        } else {
            RSpan::raw("")
        },
        sep.clone(),
        RSpan::styled(
            format!("📊 {} ↑ / {} ↓", in_tok, out_tok),
            RStyle::default().fg(Color::Rgb(186, 85, 211)),
        ),
    ]));

    let has_agent_md = model_info.agent_md_name.is_some();
    let has_skill = model_info.active_skill.is_some();
    if has_agent_md || model_info.memory_md_loaded || has_skill {
        let mut chips: Vec<RSpan<'static>> = vec![RSpan::raw("  ")];
        if let Some(ref name) = model_info.agent_md_name {
            chips.push(RSpan::styled(
                format!("📄 {}", name),
                RStyle::default().fg(Color::Rgb(176, 196, 222)),
            ));
        }
        if model_info.memory_md_loaded {
            if has_agent_md {
                chips.push(RSpan::styled(
                    "  ·  ",
                    RStyle::default().fg(Color::Rgb(60, 60, 80)),
                ));
            }
            chips.push(RSpan::styled(
                "🧠 memory.md",
                RStyle::default().fg(Color::Rgb(176, 196, 222)),
            ));
        }
        if let Some(ref skill) = model_info.active_skill {
            if has_agent_md || model_info.memory_md_loaded {
                chips.push(RSpan::styled(
                    "  ·  ",
                    RStyle::default().fg(Color::Rgb(60, 60, 80)),
                ));
            }
            chips.push(RSpan::styled(
                format!("⚡ skill:{}", skill),
                RStyle::default()
                    .fg(Color::Rgb(241, 196, 15))
                    .add_modifier(Modifier::BOLD),
            ));
        }
        inline::print_line(&Line::from(chips));
    }

    inline::print_line(&components::dashed_separator(Color::Rgb(60, 60, 80)));
    inline::print_blank();
}

fn print_prompt_status_bar(stats: &SessionStats) {
    let in_tok = stats.format_tokens(stats.total_input_tokens());
    let out_tok = stats.format_tokens(stats.total_output_tokens());
    let total_tok = stats.total_input_tokens() + stats.total_output_tokens();
    let total_formatted = stats.format_tokens(total_tok);

    let sep = RSpan::styled(" \u{2502} ", RStyle::default().fg(Color::Rgb(60, 60, 80)));

    let cwd = std::env::current_dir()
        .unwrap_or_default()
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_string();

    let mut spans: Vec<RSpan<'static>> = vec![
        RSpan::raw("  "),
        RSpan::styled(
            format!("\u{1f4c2} {}", cwd),
            RStyle::default().fg(Color::Rgb(180, 180, 200)),
        ),
    ];

    spans.push(sep.clone());
    spans.push(RSpan::styled(
        format!(
            "\u{1f4ca} \u{2191}{} \u{2193}{} ({})",
            in_tok, out_tok, total_formatted
        ),
        RStyle::default().fg(Color::Rgb(186, 85, 211)),
    ));

    // Add cache hit ratio if available
    let cache_ratio = stats.cache_hit_ratio();
    if cache_ratio > 0.0 {
        spans.push(sep.clone());
        spans.push(RSpan::styled(
            format!("\u{1f4e6} {:.0}%", cache_ratio * 100.0),
            RStyle::default().fg(Color::Rgb(46, 204, 113)),
        ));
    }

    inline::print_line(&Line::from(spans));
}

fn print_response_summary(stats: &SessionStats) {
    let in_tok = stats.format_tokens(stats.total_input_tokens());
    let out_tok = stats.format_tokens(stats.total_output_tokens());

    let sep = RSpan::styled(" │ ", RStyle::default().fg(Color::Rgb(60, 60, 80)));

    // The per-run summary above already carries the "done" outcome; this line
    // is the running session total (tokens are estimates, ~4 chars/token).
    inline::print_line(&Line::from(vec![
        RSpan::raw("  "),
        RSpan::styled("📊 session", RStyle::default().add_modifier(Modifier::DIM)),
        sep.clone(),
        RSpan::styled(
            format!("↑{} ↓{}", in_tok, out_tok),
            RStyle::default().fg(Color::Rgb(186, 85, 211)),
        ),
    ]));
    inline::print_blank();
}

fn print_help() {
    let help_md = r#"## 📖 Commands

**Slash commands:**
- `/help`              Show this help
- `/new`               Start a new session (clears conversation)
- `/config`            Show current configuration
- `/history`           Show conversation history
- `/tools`             List available tools
- `/stats`             Show session statistics
- `/thoughts`          Toggle inline model reasoning (thinking blocks)
- `/timeline`          Show/set tool timeline detail (`detailed` | `compact`)
- `/mcp`               Show MCP server status
- `/skills`            List all indexed skills
- `/skills <name>`     Load and display a skill
- `/skill`             Open skill selection dropdown
- `/skill <name>`      Load and activate a skill
- `/sessions`          List previous sessions
- `/sessions <id>`     Resume a previous session
- `/plan <goal>`       Create a plan for a goal
- `/search <query>`    Search conversation memory (case-insensitive)
- `/image <path>`      Attach image for next turn (path | data: | http(s) URL)
- `/queue`             Show queue mode + pending follow-ups
- `/queue <mode>`      Switch queue mode: follow-up (default) | interrupt
- `/provider <name>`   Switch provider (not yet supported)
- `/models`            Pick model interactively
- `/models <name>`     Switch to model by name (supports auto-complete)
- `/quit`              Exit interactive mode

**Shortcuts:**
- `help`, `h`          Show help
- `new`, `n`           New session
- `exit`, `q`          Exit

**Completion & Hints:**
- `/` → Dropdown with command list + descriptions
- `@` → Dropdown with file list + icons
- Tab/Enter → Accept dropdown selection
- ↑/↓ → Navigate dropdown or history
- Esc → Close dropdown

**Tips:**
- Type any text to send as a task to the AI agent
- Use `/skill <name>` or `/skills <name>` to load a skill before starting a task
- Ctrl+C to cancel, Ctrl+D to exit
"#;

    inline::print_blank();
    inline::print_line(&components::section_header(
        "📖",
        "Help",
        Color::Rgb(64, 224, 208),
    ));
    inline::print_blank();

    let md = crate::widgets::markdown::MarkdownContent::parse(help_md);
    inline::print_lines(&md.lines);
    inline::print_blank();
}

fn show_stats(stats: &SessionStats, model_info: &ModelInfo) {
    let cwd = std::env::current_dir()
        .unwrap_or_default()
        .display()
        .to_string();

    let in_tok = stats.total_input_tokens();
    let out_tok = stats.total_output_tokens();
    let total_tok = in_tok + out_tok;

    inline::print_blank();
    inline::print_line(&components::section_header(
        "📊",
        "Session Statistics",
        Color::Rgb(64, 224, 208),
    ));
    inline::print_blank();

    inline::print_line(&components::subsection_header(
        "Session",
        Color::Rgb(255, 215, 0),
    ));
    inline::print_line(&components::kv_line(
        "Duration",
        &stats.elapsed_str(),
        12,
        Color::Rgb(46, 204, 113),
    ));
    inline::print_line(&components::kv_line(
        "Messages",
        &format!("{}", stats.messages_sent()),
        12,
        Color::Rgb(255, 215, 0),
    ));
    inline::print_line(&components::kv_line(
        "Tool calls",
        &format!("{}", stats.tool_calls()),
        12,
        Color::Rgb(135, 206, 250),
    ));
    inline::print_blank();

    inline::print_line(&components::subsection_header(
        "Model",
        Color::Rgb(255, 215, 0),
    ));
    inline::print_line(&components::kv_badge(
        "Provider",
        &model_info.provider,
        12,
        Color::Rgb(255, 255, 255),
        Color::Rgb(155, 89, 182),
    ));
    inline::print_line(&components::kv_badge(
        "Model",
        &model_info.model,
        12,
        Color::Rgb(255, 255, 255),
        Color::Rgb(52, 152, 219),
    ));
    inline::print_line(&components::kv_line(
        "API Base",
        &model_info.api_base,
        12,
        Color::Rgb(180, 180, 200),
    ));
    inline::print_blank();

    inline::print_line(&components::subsection_header(
        "Token Usage",
        Color::Rgb(255, 215, 0),
    ));

    if total_tok > 0 {
        let in_ratio = in_tok as f32 / total_tok as f32;
        let out_ratio = out_tok as f32 / total_tok as f32;
        inline::print_line(&components::labeled_bar(
            "Input",
            in_ratio,
            30,
            Color::Rgb(46, 204, 113),
            Color::Rgb(50, 50, 60),
        ));
        inline::print_line(&components::labeled_bar(
            "Output",
            out_ratio,
            30,
            Color::Rgb(231, 76, 60),
            Color::Rgb(50, 50, 60),
        ));
    } else {
        inline::print_line(&Line::from(vec![
            RSpan::styled(
                "  Input:        ",
                RStyle::default().add_modifier(Modifier::DIM),
            ),
            RSpan::styled(
                "— no data yet —",
                RStyle::default().add_modifier(Modifier::DIM),
            ),
        ]));
    }
    inline::print_line(&Line::from(vec![
        RSpan::styled(
            "  Total:        ",
            RStyle::default().add_modifier(Modifier::DIM),
        ),
        RSpan::styled(
            format!("{} tokens", stats.format_tokens(total_tok)),
            RStyle::default()
                .fg(Color::Rgb(255, 215, 0))
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    inline::print_blank();

    let cache_read = stats.total_cache_read_tokens();
    let cache_created = stats.total_cache_creation_tokens();
    if cache_read > 0 || cache_created > 0 {
        inline::print_line(&components::subsection_header(
            "Prompt Cache",
            Color::Rgb(46, 204, 113),
        ));
        inline::print_line(&components::kv_line(
            "Cache read",
            &format!("{} tokens", stats.format_tokens(cache_read)),
            14,
            Color::Rgb(46, 204, 113),
        ));
        inline::print_line(&components::kv_line(
            "Cache created",
            &format!("{} tokens", stats.format_tokens(cache_created)),
            14,
            Color::Rgb(52, 152, 219),
        ));
        let ratio = stats.cache_hit_ratio();
        inline::print_line(&components::kv_line(
            "Hit ratio",
            &format!("{:.0}%", ratio * 100.0),
            14,
            Color::Rgb(241, 196, 15),
        ));
        inline::print_blank();
    }

    inline::print_line(&components::subsection_header(
        "Environment",
        Color::Rgb(255, 215, 0),
    ));
    inline::print_line(&components::kv_line(
        "Working dir",
        &cwd,
        12,
        Color::Rgb(180, 180, 200),
    ));
    inline::print_blank();
    inline::print_line(&components::dashed_separator(Color::Rgb(60, 60, 80)));
    inline::print_blank();
}

fn show_history(conversation: &[ConversationEntry]) {
    inline::print_blank();
    if conversation.is_empty() {
        inline::print_line(&components::warning_badge(
            "No messages in this session yet.",
        ));
        inline::print_blank();
        return;
    }

    inline::print_line(&components::section_header(
        "📜",
        &format!("Conversation History ({} messages)", conversation.len()),
        Color::Rgb(64, 224, 208),
    ));
    inline::print_blank();

    for (i, entry) in conversation.iter().enumerate() {
        let time = entry.timestamp.format("%H:%M:%S");
        let (icon, badge_bg) = match entry.role.as_str() {
            "user" => ("👤", Color::Rgb(52, 152, 219)),
            "assistant" => ("🤖", Color::Rgb(46, 204, 113)),
            _ => ("💬", Color::Rgb(241, 196, 15)),
        };
        let content_preview = if entry.content.len() > 120 {
            format!("{}...", &entry.content[..117])
        } else {
            entry.content.clone()
        };
        inline::print_line(&Line::from(vec![
            RSpan::raw("  "),
            RSpan::styled(
                format!(" {} ", icon),
                RStyle::default()
                    .fg(Color::Rgb(255, 255, 255))
                    .bg(badge_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            RSpan::raw(" "),
            RSpan::styled(
                format!("#{:02}", i + 1),
                RStyle::default()
                    .fg(Color::Rgb(180, 180, 200))
                    .add_modifier(Modifier::BOLD),
            ),
            RSpan::raw(" "),
            RSpan::styled(
                format!("[{}]", time),
                RStyle::default().add_modifier(Modifier::DIM),
            ),
            RSpan::raw("  "),
            RSpan::raw(content_preview),
        ]));
    }
    inline::print_blank();
}

fn print_goodbye(stats: &SessionStats) {
    let in_tok = stats.format_tokens(stats.total_input_tokens());
    let out_tok = stats.format_tokens(stats.total_output_tokens());

    inline::print_blank();

    let cache_read = stats.total_cache_read_tokens();
    let cache_created = stats.total_cache_creation_tokens();
    let has_cache = cache_read > 0 || cache_created > 0;

    let mut summary_lines = vec![
        Line::from(vec![
            RSpan::styled("💬 ", RStyle::default()),
            RSpan::styled("Messages ", RStyle::default().add_modifier(Modifier::DIM)),
            RSpan::styled(
                format!("{}", stats.messages_sent()),
                RStyle::default()
                    .fg(Color::Rgb(135, 206, 250))
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            RSpan::styled("⏱ ", RStyle::default()),
            RSpan::styled("Duration ", RStyle::default().add_modifier(Modifier::DIM)),
            RSpan::styled(
                stats.elapsed_str(),
                RStyle::default()
                    .fg(Color::Rgb(46, 204, 113))
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            RSpan::styled("📊 ", RStyle::default()),
            RSpan::styled("Tokens   ", RStyle::default().add_modifier(Modifier::DIM)),
            RSpan::styled(
                format!("{} ↑", in_tok),
                RStyle::default().fg(Color::Rgb(46, 204, 113)),
            ),
            RSpan::raw(" / "),
            RSpan::styled(
                format!("{} ↓", out_tok),
                RStyle::default().fg(Color::Rgb(231, 76, 60)),
            ),
        ]),
    ];

    if has_cache {
        summary_lines.push(Line::from(vec![
            RSpan::styled("📦 ", RStyle::default()),
            RSpan::styled("Cache    ", RStyle::default().add_modifier(Modifier::DIM)),
            RSpan::styled(
                format!("💰 {} rd", stats.format_tokens(cache_read)),
                RStyle::default()
                    .fg(Color::Rgb(46, 204, 113))
                    .add_modifier(Modifier::BOLD),
            ),
            RSpan::raw(" / "),
            RSpan::styled(
                format!("✏️ {} cr", stats.format_tokens(cache_created)),
                RStyle::default()
                    .fg(Color::Rgb(52, 152, 219))
                    .add_modifier(Modifier::BOLD),
            ),
            RSpan::raw("  "),
            RSpan::styled(
                format!("({:.0}% hit)", stats.cache_hit_ratio() * 100.0),
                RStyle::default().fg(Color::Rgb(241, 196, 15)),
            ),
        ]));
    }

    let panel_lines = components::panel(
        "Session Summary",
        &summary_lines,
        components::BoxStyle::Rounded,
        Color::Rgb(100, 100, 140),
    );
    inline::print_lines(&panel_lines);
    inline::print_blank();

    let goodbye = components::gradient_text(
        "  👋 See you next time!",
        Color::Rgb(255, 105, 180),
        Color::Rgb(64, 224, 208),
    );
    inline::print_line(&goodbye);
    inline::print_blank();
}

fn show_sessions() {
    inline::print_blank();

    let sessions = match crate::session::list() {
        Ok(s) => s,
        Err(e) => {
            inline::print_line(&components::error_badge(&format!(
                "Failed to list sessions: {}",
                e
            )));
            inline::print_blank();
            return;
        }
    };

    if sessions.is_empty() {
        inline::print_line(&components::warning_badge("No previous sessions found."));
        inline::print_blank();
        return;
    }

    inline::print_line(&components::section_header(
        "📜",
        &format!("Sessions ({})", sessions.len()),
        Color::Rgb(64, 224, 208),
    ));
    inline::print_blank();

    let bold = RStyle::default().add_modifier(Modifier::BOLD);
    let dim = RStyle::default().add_modifier(Modifier::DIM);

    for (i, s) in sessions.iter().enumerate().take(20) {
        let time = crate::session::format_relative_time(&s.updated_at);

        let title = if s.title.is_empty() {
            "Untitled"
        } else {
            &s.title
        };

        inline::print_line(&Line::from(vec![
            RSpan::styled(format!("  {:2}. ", i + 1), dim),
            RSpan::styled(title.to_string(), bold),
            RSpan::styled(format!("  {} msgs", s.message_count), dim),
            RSpan::raw("  "),
            RSpan::styled(time, RStyle::default().fg(Color::Rgb(135, 206, 250))),
        ]));
        inline::print_line(&Line::from(vec![
            RSpan::styled("      ", RStyle::default()),
            RSpan::styled(s.id.to_string(), dim),
            RSpan::styled(
                format!(" · {} · {}/{}", s.directory, s.provider, s.model),
                dim,
            ),
        ]));
        inline::print_blank();
    }

    if sessions.len() > 20 {
        inline::print_line(&Line::from(vec![RSpan::styled(
            format!("  ... and {} more", sessions.len() - 20),
            dim,
        )]));
        inline::print_blank();
    }

    inline::print_line(&Line::from(vec![
        RSpan::styled("💡 ", RStyle::default()),
        RSpan::styled("Tip: ", RStyle::default().add_modifier(Modifier::DIM)),
        RSpan::raw("Use "),
        RSpan::styled(
            "/sessions <id>",
            RStyle::default()
                .fg(Color::Rgb(255, 215, 0))
                .add_modifier(Modifier::BOLD),
        ),
        RSpan::raw(" to resume a session"),
    ]));
    inline::print_blank();
}

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
