use anyhow::Result;
use comfy_table::{
    modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL, Cell, Color as TColor, Table,
};
use takka_core::Config;
use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::terminal::{Clear, ClearType};
use crossterm::ExecutableCommand;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, MultiSelect, Select};
use ratatui::style::{Color as RColor, Modifier as RModifier, Style as RStyle};
use ratatui::text::{Line as RLine, Span as RSpan};
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use termcolor::{Color, ColorSpec, StandardStream, WriteColor};

use crate::cli::{ConfigAction, OutputFormat, SkillAction};
use crate::confirmation::{prompt_confirmation, ConfirmationResponse};
use crate::error::CommandError;
use crate::widgets::capabilities;
use crate::widgets::{components, inline};

static ALWAYS_CONFIRM: AtomicBool = AtomicBool::new(false);
static COLOR_ENABLED: AtomicBool = AtomicBool::new(true);

/// True while `Commands::run`'s streaming renderer thread is alive (REPL
/// mode). The question handler only does the consume/re-arm spinner dance
/// when this is set; in TUI mode the progress indicator lives in the app
/// frame instead.
pub(crate) static SPINNER_ACTIVE: AtomicBool = AtomicBool::new(false);

/// True while the full-screen TUI owns the terminal. The question (and
/// confirmation) handler checks this to leave / re-enter the alternate
/// screen around its prompt so dialoguer renders on a clean primary
/// screen instead of on top of a frozen TUI frame.
pub(crate) static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Set by the interactive handlers after re-entering the alternate
/// screen; the TUI main loop does a full repaint (`terminal::clear`)
/// before its next draw so dialoguer's artifacts are never left behind
/// (ratatui only diffs against its last frame).
pub(crate) static TUI_NEEDS_REDRAW: AtomicBool = AtomicBool::new(false);

/// Serializes writes to the terminal between the streaming renderer
/// thread (spawned by `Commands::run`) / the TUI main loop and the
/// `question` tool's interactive prompt.
///
/// While the question handler holds this gate the renderer parks instead
/// of touching the terminal, so its `MoveUp`/`Clear` spinner redraws
/// can't land on top of the dialoguer option list — and on Windows the
/// two writers racing over cursor control is what looks like a flickering
/// dim lamp.
pub(crate) static RENDER_GATE: Mutex<()> = Mutex::new(());

// ── Interactive tool handlers ───────────────────────────────

/// Handles `question` tool calls from the agent during interactive mode.
///
/// Renders each question with a styled prompt, presents options via
/// `dialoguer::Select` / `MultiSelect` when choices exist, and falls back
/// to free-text `Input` otherwise. The handler is synchronous (matches
/// the `Tool::execute` contract) and reads directly from stdin.
struct CliQuestionHandler;

impl takka_core::QuestionHandler for CliQuestionHandler {
    fn handle(
        &self,
        questions: &[takka_core::QuestionPrompt],
    ) -> Vec<takka_core::QuestionAnswer> {
        // Pause the streaming renderer for the whole prompt sequence.
        // The spinner thread spawned by `Commands::run` redraws the
        // "Thinking…" line every 80ms with MoveUp+Clear+print; if it kept
        // writing here its cursor moves would land on top of the
        // dialoguer list (messy options), then erase the answer badge
        // after submission — and on Windows the two writers racing over
        // the console is what looks like a flickering dim lamp.
        let _render_gate = RENDER_GATE.lock().unwrap();
        let tui_active = TUI_ACTIVE.load(Ordering::Relaxed);
        if tui_active {
            // Drop the alternate screen so the prompt renders on the
            // primary screen instead of over a frozen TUI frame.
            let _ =
                crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        }
        if SPINNER_ACTIVE.load(Ordering::Relaxed) {
            consume_spinner_line();
        }

        // The REPL / TUI run with crossterm raw mode enabled. dialoguer's
        // own key handling enables raw mode again through the `console`
        // crate, which on Windows re-issues SetConsoleMode calls that
        // force a repaint (the "flip"). Flip to cooked mode for the
        // duration of the prompts, then restore raw mode afterwards so
        // the REPL keeps working (same pattern as `prompt_confirmation`).
        let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
        if was_raw {
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
        }

        /// Restores raw mode on scope exit (incl. early returns from the
        /// match arms below). The cursor is re-hidden for the REPL (which
        /// hides it in raw mode) but left visible for the TUI.
        struct RawModeGuard(bool, bool);
        impl Drop for RawModeGuard {
            fn drop(&mut self) {
                if self.0 {
                    if self.1 {
                        let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Hide);
                    } else {
                        let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
                    }
                    let _ = crossterm::terminal::enable_raw_mode();
                }
            }
        }
        let _raw_guard = RawModeGuard(was_raw, !tui_active);

        /// Re-enters the alternate screen when the TUI is active and
        /// flags a full repaint so the next `Terminal::draw` rewrites
        /// everything (ratatui would otherwise only diff against its
        /// pre-question frame and leave the screen half-blank).
        struct TuiScreenGuard(bool);
        impl Drop for TuiScreenGuard {
            fn drop(&mut self) {
                if self.0 {
                    let _ = crossterm::execute!(
                        std::io::stdout(),
                        crossterm::terminal::EnterAlternateScreen
                    );
                    TUI_NEEDS_REDRAW.store(true, Ordering::Relaxed);
                }
            }
        }
        let _tui_guard = TuiScreenGuard(tui_active);

        let mut answers = Vec::with_capacity(questions.len());

        for q in questions {
            // Render a visual separator + question header.
            inline::print_blank();

            let header = q.header.as_deref().unwrap_or("Question");
            inline::print_line(&components::section_header(
                "❓",
                header,
                RColor::Rgb(241, 196, 15),
            ));
            inline::print_blank();

            // Print the question text.
            inline::print_line(&RLine::from(vec![
                RSpan::styled("  ", RStyle::default()),
                RSpan::raw(&q.question),
            ]));
            inline::print_blank();

            let answer = if !q.options.is_empty() {
                // Agent provided pre-defined choices.
                if q.multiple {
                    // Multi-select.
                    let defaults: Vec<bool> = vec![false; q.options.len()];
                    let selection = MultiSelect::with_theme(&ColorfulTheme::default())
                        .with_prompt("Select (space to toggle, enter to confirm)")
                        .items(&q.options)
                        .defaults(&defaults)
                        .interact();

                    match selection {
                        Ok(indices) => {
                            let chosen: Vec<String> =
                                indices.iter().map(|&i| q.options[i].clone()).collect();

                            if chosen.is_empty() && q.custom {
                                // Nothing selected but custom allowed.
                                free_text_fallback(&q.question)
                            } else if chosen.is_empty() {
                                // Nothing selected and no custom — skip.
                                skip_answer(&q.question)
                            } else {
                                render_answer(&chosen.join(", ").to_string());
                                vec![takka_core::QuestionAnswer {
                                    question: q.question.clone(),
                                    answer: chosen,
                                    skipped: false,
                                }]
                            }
                        }
                        Err(_) => skip_answer(&q.question),
                    }
                } else {
                    // Single-select.
                    let mut items: Vec<String> = q.options.clone();
                    if q.custom {
                        items.push("✏️  Custom (type your own)".to_string());
                    }
                    items.push("⏭  Skip".to_string());

                    let selection = Select::with_theme(&ColorfulTheme::default())
                        .with_prompt("Choose")
                        .items(&items)
                        .default(0)
                        .interact();

                    match selection {
                        Ok(idx) => {
                            // Last item is always "Skip".
                            let skip_idx = items.len() - 1;
                            if idx == skip_idx {
                                skip_answer(&q.question)
                            } else if q.custom && idx == items.len() - 2 {
                                // "Custom" was selected.
                                free_text_fallback(&q.question)
                            } else {
                                let chosen = q.options[idx].clone();
                                render_answer(&chosen);
                                vec![takka_core::QuestionAnswer {
                                    question: q.question.clone(),
                                    answer: vec![chosen],
                                    skipped: false,
                                }]
                            }
                        }
                        Err(_) => skip_answer(&q.question),
                    }
                }
            } else {
                // No options — free-text input.
                free_text_fallback(&q.question)
            };

            answers.extend(answer);
        }

        // Re-arm a transient spinner line under the cursor so the
        // renderer's invariant ("spinner is the last printed line, cursor
        // directly below it") still holds when it wakes up. Its next
        // tick replaces this placeholder with the real progress line.
        if SPINNER_ACTIVE.load(Ordering::Relaxed) {
            commit_spinner_placeholder();
        }

        answers
    }
}

/// Render a free-text input prompt for the given question.
fn free_text_fallback(question: &str) -> Vec<takka_core::QuestionAnswer> {
    let result = Input::<String>::with_theme(&ColorfulTheme::default())
        .with_prompt("Your answer (enter to submit, Ctrl+C to skip)")
        .allow_empty(true)
        .interact();

    match result {
        Ok(text) => {
            let text = text.trim().to_string();
            if text.is_empty() {
                skip_answer(question)
            } else {
                render_answer(&text);
                vec![takka_core::QuestionAnswer {
                    question: question.to_string(),
                    answer: vec![text],
                    skipped: false,
                }]
            }
        }
        Err(_) => skip_answer(question),
    }
}

/// Produce a single skipped answer.
fn skip_answer(question: &str) -> Vec<takka_core::QuestionAnswer> {
    inline::print_blank();
    inline::print_line(&components::warning_badge("Skipped."));
    inline::print_blank();
    vec![takka_core::QuestionAnswer {
        question: question.to_string(),
        answer: vec![],
        skipped: true,
    }]
}

/// Echo the chosen answer back to the user.
fn render_answer(text: &str) {
    inline::print_blank();
    inline::print_line(&components::success_badge(&format!("Answered: {}", text)));
    inline::print_blank();
}

/// Erase the transient spinner line that `Commands::run`'s renderer
/// thread keeps on screen. The renderer's invariant is that the spinner
/// is the last printed line with the cursor directly below it, so one
/// `MoveUp` lands on it.
pub(crate) fn consume_spinner_line() {
    if !inline::is_stdout_tty() {
        return;
    }
    let mut stdout = std::io::stdout();
    let _ = stdout.execute(MoveUp(1));
    let _ = stdout.execute(MoveToColumn(0));
    let _ = stdout.execute(Clear(ClearType::FromCursorDown));
}

/// Print a fresh "Thinking…" line under the current cursor. Used by the
/// question handler to restore the renderer's "spinner is the last line"
/// invariant after a prompt; the renderer replaces it with the real
/// progress line on its next tick.
pub(crate) fn commit_spinner_placeholder() {
    if !inline::is_stdout_tty() {
        return;
    }
    let mut p = crate::widgets::progress::ProgressState::new();
    p.start();
    p.set_message("Thinking…".to_string());
    inline::print_line(&crate::widgets::spinner::compact_progress_line(&p, 18));
}

/// Renders todo list changes inline. Fires after every `todowrite` call.
/// Shows a compact progress summary so the user sees task progress even
/// in non-interactive `takka run` mode.
struct CliTodoRenderer;

impl takka_core::TodoChangeHandler for CliTodoRenderer {
    fn on_change(&self, todos: &[takka_core::TodoItem]) {
        if todos.is_empty() {
            return;
        }

        let total = todos.len();
        let completed = todos
            .iter()
            .filter(|t| t.status == takka_core::TodoStatus::Completed)
            .count();
        let in_progress = todos
            .iter()
            .filter(|t| t.status == takka_core::TodoStatus::InProgress)
            .count();
        let pct = if total > 0 {
            (completed as f64 / total as f64 * 100.0) as u32
        } else {
            0
        };

        // Build a compact status bar.
        //    📋 Tasks: 3/7 (43%)  ● 1 active  ○ 3 pending
        let mut spans: Vec<RSpan<'static>> = vec![
            RSpan::styled("  📋 ", RStyle::default()),
            RSpan::styled(
                format!("Tasks: {}/{} ({}%)", completed, total, pct),
                RStyle::default()
                    .fg(RColor::Rgb(241, 196, 15))
                    .add_modifier(RModifier::BOLD),
            ),
        ];

        if in_progress > 0 {
            spans.push(RSpan::styled(
                format!("  ● {} active", in_progress),
                RStyle::default().fg(RColor::Rgb(135, 206, 250)),
            ));
        }

        let pending = total - completed - in_progress;
        if pending > 0 {
            spans.push(RSpan::styled(
                format!("  ○ {} pending", pending),
                RStyle::default().fg(RColor::Rgb(120, 120, 140)),
            ));
        }

        // Render individual items (compact, truncated).
        let mut item_lines: Vec<RLine<'static>> = Vec::new();
        for todo in todos {
            let (icon, color) = match todo.status {
                takka_core::TodoStatus::Completed => ("✓", RColor::Rgb(46, 204, 113)),
                takka_core::TodoStatus::InProgress => ("●", RColor::Rgb(135, 206, 250)),
                takka_core::TodoStatus::Pending => ("○", RColor::Rgb(120, 120, 140)),
                takka_core::TodoStatus::Cancelled => ("✗", RColor::Rgb(120, 120, 140)),
            };

            let priority_marker = match todo.priority {
                takka_core::TodoPriority::High => " ❗",
                takka_core::TodoPriority::Low => " ↓",
                _ => "",
            };

            let mut content = todo.content.clone();
            if content.len() > 80 {
                content.truncate(77);
                content.push_str("...");
            }

            item_lines.push(RLine::from(vec![
                RSpan::raw("    "),
                RSpan::styled(
                    format!("{} ", icon),
                    RStyle::default().fg(color).add_modifier(RModifier::BOLD),
                ),
                RSpan::styled(content, RStyle::default().fg(RColor::Rgb(200, 200, 210))),
                RSpan::styled(
                    priority_marker.to_string(),
                    RStyle::default()
                        .fg(RColor::Rgb(231, 76, 60))
                        .add_modifier(RModifier::DIM),
                ),
            ]));
        }

        inline::print_blank();
        inline::print_line(&RLine::from(spans));
        for line in &item_lines {
            inline::print_line(line);
        }
        inline::print_blank();
    }
}

/// Provider presets for quick setup
struct ProviderPreset {
    name: &'static str,
    provider_type: &'static str,
    api_base: &'static str,
    models: &'static [(&'static str, &'static str)],
}

const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        name: "openai",
        provider_type: "openai-compatible",
        api_base: "https://api.openai.com/v1",
        models: &[
            ("gpt-4o", "GPT-4o"),
            ("gpt-4o-mini", "GPT-4o Mini"),
            ("gpt-4-turbo", "GPT-4 Turbo"),
        ],
    },
    ProviderPreset {
        name: "anthropic",
        provider_type: "openai-compatible",
        api_base: "https://api.anthropic.com/v1",
        models: &[
            ("claude-sonnet-4-20250514", "Claude Sonnet 4"),
            ("claude-3-5-sonnet-20241022", "Claude 3.5 Sonnet"),
        ],
    },
    ProviderPreset {
        name: "zai",
        provider_type: "openai-compatible",
        api_base: "https://api.z.ai/v1",
        models: &[("glm-4.7", "GLM-4.7"), ("glm-4", "GLM-4")],
    },
    ProviderPreset {
        name: "custom",
        provider_type: "openai-compatible",
        api_base: "",
        models: &[],
    },
];

/// How user text typed while a run is active is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueMode {
    /// Pesan selama run aktif diantrikan lokal; dikirim sebagai Run
    /// berikutnya.
    #[default]
    FollowUp,
    /// Pesan selama run aktif dikirim langsung sebagai steer ke engine.
    Interrupt,
}

impl QueueMode {
    /// CLI-facing name (matches the `--queue-mode` values).
    pub fn as_str(&self) -> &'static str {
        match self {
            QueueMode::FollowUp => "follow-up",
            QueueMode::Interrupt => "interrupt",
        }
    }

    /// One-line behavioral description used by `/queue` output.
    pub fn description(&self) -> &'static str {
        match self {
            QueueMode::FollowUp => "text typed during a run is queued and sent as the next run",
            QueueMode::Interrupt => {
                "text typed during a run is sent as a steer at the next tool boundary"
            }
        }
    }
}

/// Safety valve for the client-issued drain loop: each drained follow-up
/// goes through the full `run()` path, which drains again on completion. In
/// practice the queue is empty by the time nested drains run; the cap only
/// guards against a pathological submit-during-drained-run loop (possible
/// once input-during-run is wired into the TUI).
const MAX_FOLLOW_UP_DRAIN_DEPTH: usize = 8;

pub struct Commands {
    config: Config,
    // orchestrator removed (P1-1 DoD: no Orchestrator::new in CLI —
    // all agent runs go through the takka-runtime daemon).
    connection: Option<crate::client::AgentConnection>,
    runtime_config_path: Option<String>,
    runtime_mode: crate::client::RuntimeMode,
    /// Explicit daemon discovery file for `attach_daemon` (default:
    /// [`crate::client::daemon_info_path`]). Test seam for hermetic e2e
    /// smokes; production builds leave it `None`. Unix-only (daemon mode
    /// is not supported on Windows); `allow(dead_code)` keeps non-unix
    /// builds warning-clean under `-D warnings`.
    #[cfg_attr(not(unix), allow(dead_code))]
    daemon_info_path_override: Option<std::path::PathBuf>,
    color_enabled: bool,
    debug_enabled: bool,
    permission_mode: takka_core::PermissionMode,
    /// Path to the `AGENT.md` discovered by walk-up at orchestrator init,
    /// or `None` when no project instructions were found.
    agent_md_path: Option<std::path::PathBuf>,
    /// `true` when at least one persistent memory file (user-global or
    /// project-local) was loaded and folded into the system prompt.
    memory_md_loaded: bool,
    /// Image attachments queued by the `/image <path>` slash command,
    /// to ride along with the next user turn. Drained by `run()` when
    /// the next message is sent.
    pending_attachments: Vec<takka_core::Attachment>,
    /// `true` when running in interactive REPL mode. Controls whether
    /// the `question` tool handler is registered (stdin-based prompts)
    /// vs returning skip-all fallback (non-interactive `takka run`).
    interactive_mode: bool,
    /// Shared skill index for the skill tool, populated at orchestrator init.
    skill_index: Option<std::sync::Arc<std::sync::RwLock<takka_core::SkillIndex>>>,
    /// Skill currently active for THIS session (per-instance state
    /// replacing the removed process-global, Fase D).
    active_skill: Option<String>,
    /// How text typed during an active run is handled: queued locally as a
    /// follow-up (default) or sent immediately as a steer (interrupt).
    queue_mode: QueueMode,
    /// Text submitted while a run was active in FollowUp mode. Drained
    /// after the run's terminal event: each item is sent as the next run
    /// (client-issued; see `drain_follow_ups`).
    pending_follow_ups: std::collections::VecDeque<String>,
    /// Current nesting depth of `drain_follow_ups` (drained runs drain
    /// again on completion; capped at [`MAX_FOLLOW_UP_DRAIN_DEPTH`]).
    drain_depth: usize,
    /// Test-only taps (cfg(test)): e2e tests prove the client-issued drain
    /// contract (runs == queue items + 1; the drained run emits its own
    /// SessionStarted) by counting client-issued Runs and capturing the
    /// lifecycle events of drained runs — which travel through `run()`'s
    /// renderer path and are invisible to per-call callbacks.
    #[cfg(test)]
    test_runs: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    test_events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl Default for Commands {
    fn default() -> Self {
        Self::new(Config::fallback())
    }
}

impl Commands {
    /// Create Commands without initializing the orchestrator (for config/status/examples)
    pub fn new(config: Config) -> Self {
        Self {
            config,

            connection: None,
            runtime_config_path: None,
            runtime_mode: crate::client::RuntimeMode::Embedded,
            daemon_info_path_override: None,
            color_enabled: true,
            debug_enabled: false,
            permission_mode: takka_core::PermissionMode::Default,
            agent_md_path: None,
            memory_md_loaded: false,
            pending_attachments: Vec::new(),
            interactive_mode: false,
            skill_index: None,
            active_skill: None,
            queue_mode: QueueMode::default(),
            pending_follow_ups: std::collections::VecDeque::new(),
            drain_depth: 0,
            #[cfg(test)]
            test_runs: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            test_events: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    pub fn with_runtime_config_path(mut self, path: Option<String>) -> Self {
        self.runtime_config_path = path;
        self
    }

    pub fn with_runtime_mode(mut self, mode: crate::client::RuntimeMode) -> Self {
        self.runtime_mode = mode;
        self
    }

    /// Point `attach_daemon` at an explicit discovery file (test seam for
    /// hermetic daemon smokes; unix-only — the non-unix build never reads
    /// the field, hence the field-level `allow(dead_code)` above).
    #[cfg(test)]
    pub(crate) fn with_daemon_info_path(mut self, path: std::path::PathBuf) -> Self {
        self.daemon_info_path_override = Some(path);
        self
    }

    /// `takka attach` (Fase 3a): with `--shutdown`, ask the running
    /// daemon to stop (no session handshake — see
    /// [`crate::client::shutdown_daemon_at`]); otherwise connect to the
    /// already-running daemon (never auto-spawn — an attach that starts
    /// its own daemon would be a run in disguise) and keep the
    /// connection for the regular REPL (main.rs calls `interactive::run`
    /// with this same `Commands`, and `ensure_connection` reuses the
    /// established connection).
    pub async fn attach_daemon(&mut self, shutdown: bool) -> Result<()> {
        self.attach_daemon_inner(shutdown).await
    }

    /// Unix implementation: shutdown or connect to the live daemon.
    /// On non-unix this variant does not exist; the stub below returns
    /// the actionable error.
    #[cfg(unix)]
    async fn attach_daemon_inner(&mut self, shutdown: bool) -> Result<()> {
        if shutdown {
            let info_path = self
                .daemon_info_path_override
                .clone()
                .unwrap_or_else(crate::client::daemon_info_path);
            crate::client::shutdown_daemon_at(&info_path).await?;
            inline::print_blank();
            inline::print_line(&components::success_badge("Daemon shutdown requested."));
            inline::print_blank();
            return Ok(());
        }
        if self.connection.is_some() {
            return Ok(()); // already attached
        }
        let overrides = takka_core::runtime::protocol::InitOverrides {
            config_path: self.runtime_config_path.clone(),
            permission_mode: Some(self.permission_mode),
            model: self.config.active_model().map(|model| model.model.clone()),
            system_prompt: self.config.system_prompt.clone(),
        };
        let connection = match &self.daemon_info_path_override {
            Some(path) => {
                crate::client::AgentConnection::connect_daemon_at(
                    path,
                    overrides,
                    crate::client::CLI_CAPABILITIES,
                    false,
                )
                .await?
            }
            None => {
                crate::client::AgentConnection::connect_daemon(
                    overrides,
                    crate::client::CLI_CAPABILITIES,
                    false,
                )
                .await?
            }
        };
        self.connection = Some(connection);
        Ok(())
    }

    /// Non-unix stub: daemon mode is Unix-only. This is the ONLY
    /// `attach_daemon` body compiled on Windows — the unix variant above
    /// (with its `connect_daemon_at` / `shutdown_daemon_at` references)
    /// does not exist there, so no unix-only symbol leaks into the
    /// Windows build under `-D warnings`.
    #[cfg(not(unix))]
    async fn attach_daemon_inner(&mut self, _shutdown: bool) -> Result<()> {
        Err(anyhow::anyhow!(crate::client::DAEMON_NOT_SUPPORTED_MESSAGE))
    }

    async fn ensure_connection(&mut self) -> Result<()> {
        if self.connection.is_some() {
            return Ok(());
        }
        let overrides = takka_core::runtime::protocol::InitOverrides {
            config_path: self.runtime_config_path.clone(),
            permission_mode: Some(self.permission_mode),
            model: self.config.active_model().map(|model| model.model.clone()),
            system_prompt: self.config.system_prompt.clone(),
        };
        self.connection = Some(
            crate::client::AgentConnection::connect(
                self.runtime_mode,
                overrides,
                crate::client::CLI_CAPABILITIES,
            )
            .await?,
        );
        Ok(())
    }

    pub fn with_color(mut self, enabled: bool) -> Self {
        self.color_enabled = enabled;
        COLOR_ENABLED.store(enabled, Ordering::Relaxed);
        self
    }

    /// Get a reference to the config
    pub(crate) fn get_config(&self) -> &Config {
        &self.config
    }

    pub fn with_interactive_mode(mut self, enabled: bool) -> Self {
        self.interactive_mode = enabled;
        self
    }

    pub fn with_debug(mut self, enabled: bool) -> Self {
        self.debug_enabled = enabled;
        self
    }

    /// Attach the input watcher's shared state so the spinner ticker
    /// can render the live input buffer below the progress line.
    pub fn with_permission_mode(mut self, mode: takka_core::PermissionMode) -> Self {
        self.permission_mode = mode;
        self
    }

    /// Select how text typed during an active run is handled.
    pub fn with_queue_mode(mut self, mode: QueueMode) -> Self {
        self.queue_mode = mode;
        self
    }

    /// One-time session metadata init: filesystem-derived state that
    /// was previously set inside the removed `ensure_orchestrator`.
    fn init_session_metadata(&mut self) {
        if self.agent_md_path.is_some() || self.memory_md_loaded {
            return; // already initialized
        }
        let cwd = std::env::current_dir().unwrap_or_default();
        self.agent_md_path = takka_core::load_project_instructions(&cwd).map(|(path, _)| path);
        self.memory_md_loaded = takka_core::assemble_memory_section(&cwd).is_some();
    }

    // ── Status ──────────────────────────────────────────────

    pub fn status(&self) -> Result<()> {
        let config_path = Config::config_path();
        let config_exists = config_path.exists();

        println!();
        print_info("Takka CLI Status");
        println!("  Config file: {}", config_path.display());
        if config_exists {
            print_success("  Config file: exists");
        } else {
            print_warning("  Config file: not found");
        }

        if self.config.providers.is_empty() {
            print_warning("  No providers configured");
        } else {
            for (i, p) in self.config.providers.iter().enumerate() {
                println!();
                print_info(&format!("Provider #{}: {}", i + 1, p.name));
                println!("    Type:     {}", p.provider_type);
                println!("    API Base: {}", p.api_base);
                if p.api_key.is_empty() {
                    print_warning("    API Key:  ✗ not set");
                } else {
                    print_success("    API Key:  ✓ configured");
                }
                if !p.models.is_empty() {
                    println!("    Models:");
                    for m in &p.models {
                        let display = m.display_name.as_deref().unwrap_or(&m.model);
                        println!("      • {} ({})", display, m.model);
                    }
                }
            }
        }
        println!();
        Ok(())
    }

    // ── Model info (for REPL prompt) ────────────────────────

    pub fn model_info(&self) -> (String, String, String) {
        if let Some(p) = self.config.active_provider() {
            let model = p
                .models
                .first()
                .map(|m| m.display_name.as_deref().unwrap_or(&m.model))
                .unwrap_or("unknown")
                .to_string();
            (p.name.clone(), model, p.api_base.clone())
        } else {
            ("none".into(), "none".into(), "-".into())
        }
    }

    // ── Inline config display (for REPL /config) ──────────

    pub fn config_show_inline(&self) {
        inline::print_blank();
        let key_w = 10;
        if let Some(p) = self.config.active_provider() {
            inline::print_line(&components::kv_line(
                "Provider",
                &format!("{} ({})", p.name, p.provider_type),
                key_w,
                RColor::Reset,
            ));
            inline::print_line(&components::kv_line(
                "API Base",
                &p.api_base,
                key_w,
                RColor::Reset,
            ));
            if p.api_key.is_empty() {
                inline::print_line(&components::kv_line(
                    "API Key",
                    "not set",
                    key_w,
                    RColor::Red,
                ));
            } else {
                let masked = format!(
                    "{}...{}",
                    &p.api_key[..4.min(p.api_key.len())],
                    &p.api_key[p.api_key.len().saturating_sub(4)..]
                );
                inline::print_line(&components::kv_line(
                    "API Key",
                    &masked,
                    key_w,
                    RColor::Green,
                ));
            }
            if let Some(m) = p.models.first() {
                inline::print_line(&components::kv_line(
                    "Model",
                    &format!(
                        "{} (temp: {}, max_tokens: {})",
                        m.model, m.temperature, m.max_tokens
                    ),
                    key_w,
                    RColor::Reset,
                ));
            }
        } else {
            inline::print_line(&components::warning_badge("No provider configured."));
        }
        inline::print_line(&components::kv_line(
            "Config",
            &Config::config_path().display().to_string(),
            key_w,
            RColor::Reset,
        ));
        inline::print_blank();
    }

    // ── List tools (for REPL /tools) ────────────────────────

    /// Path to the `AGENT.md` that was loaded into the system prompt for
    /// this session, or `None` when no project instructions were found.
    /// Set during the first `ensure_orchestrator()` call.
    pub fn agent_md_path(&self) -> Option<&std::path::Path> {
        self.agent_md_path.as_deref()
    }

    /// `true` when at least one persistent memory file (user-global or
    /// project-local) was loaded into the system prompt.
    pub fn memory_md_loaded(&self) -> bool {
        self.memory_md_loaded
    }

    /// Resolve the active model's capabilities (vision / tools /
    /// streaming). Reads the per-model override first, then falls back
    /// to the built-in lookup.
    pub fn active_model_capabilities(&self) -> takka_core::ModelCapabilities {
        if let Some(model) = self.config.active_model() {
            return model.effective_capabilities();
        }
        takka_core::ModelCapabilities::default()
    }

    /// Drain the queue of `/image`-attached payloads. The queue is
    /// per-command-run — once attached to a turn, the buffer empties.
    pub fn drain_pending_attachments(&mut self) -> Vec<takka_core::Attachment> {
        std::mem::take(&mut self.pending_attachments)
    }

    /// Number of images queued for the next turn (for status display).
    #[allow(dead_code)]
    pub fn pending_attachment_count(&self) -> usize {
        self.pending_attachment_count_inner()
    }

    fn pending_attachment_count_inner(&self) -> usize {
        self.pending_attachments.len()
    }

    /// Queue an already-loaded attachment for the next turn.
    /// Used by the TUI `/image` handler that loads the file itself.
    pub fn queue_attachment(&mut self, att: takka_core::Attachment) {
        self.pending_attachments.push(att);
    }

    /// Read-only reference to the current configuration.
    /// Used by TUI commands that need provider/model lists.
    pub fn config_ref(&self) -> &Config {
        &self.config
    }

    /// Render the `/image <path>` slash command: load the file, validate
    /// it (size cap + MIME), and queue it for the next user turn. The
    /// next call to `run()` will attach all queued images to the
    /// outgoing message.
    pub fn attach_image_inline(&mut self, path: &str) {
        let path = path.trim();
        if path.is_empty() {
            inline::print_blank();
            inline::print_line(&components::warning_badge("Usage: /image <path>"));
            inline::print_blank();
            return;
        }

        // Pre-flight capability check so the user sees the failure now,
        // not on the next turn.
        let caps = self.active_model_capabilities();
        if !caps.vision {
            inline::print_blank();
            inline::print_line(&components::error_badge(
                "Active model does not support image input.",
            ));
            inline::print_line(&RLine::from(vec![
                RSpan::raw("  Switch with "),
                RSpan::styled("/models", RStyle::default().add_modifier(RModifier::BOLD)),
                RSpan::raw(" to a vision-capable model first."),
            ]));
            inline::print_blank();
            return;
        }

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
                let bytes = att.size_bytes;
                let mime = att.mime_type.clone();
                let source = format!("{}", att.source);
                self.pending_attachments.push(att);
                inline::print_blank();
                inline::print_line(&components::success_badge(&format!(
                    "Attached: {} ({} bytes, {})",
                    source,
                    bytes,
                    if mime.is_empty() {
                        "remote"
                    } else {
                        mime.as_str()
                    }
                )));
                inline::print_line(&RLine::from(vec![
                    RSpan::raw("  "),
                    RSpan::styled(
                        format!(
                            "{} image(s) queued for next turn.",
                            self.pending_attachments.len()
                        ),
                        RStyle::default().add_modifier(RModifier::DIM),
                    ),
                ]));
                inline::print_blank();
            }
            Err(e) => {
                inline::print_blank();
                inline::print_line(&components::error_badge(&format!(
                    "Failed to attach: {}",
                    e
                )));
                inline::print_blank();
            }
        }
    }

    /// Restart the agent session in-place: drops the conversation memory,
    /// clears any pending cancel flag, drops accumulated event handlers,
    /// and resets cumulative cost. The provider, tool registry, system
    /// prompt, AGENT.md and persistent memory section all stay loaded —
    /// the user does NOT pay the re-init cost (or another wizard prompt).
    ///
    /// No-op when the orchestrator hasn't been initialized yet.
    pub async fn restart_session(&mut self) -> Result<()> {
        self.ensure_connection().await?;
        let runtime = self.connection.as_mut().unwrap();
        let event = runtime
            .send_and_wait(takka_core::runtime::protocol::Request::ResetSession)
            .await?;
        match event.event {
            takka_core::Event::SessionReset => Ok(()),
            takka_core::Event::Error { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!("unexpected reset response: {other:?}")),
        }
    }

    pub async fn list_tools(&mut self) {
        if let Err(e) = self.ensure_connection().await {
            inline::print_line(&components::error_badge(&e.to_string()));
            return;
        }
        let runtime = self.connection.as_mut().unwrap();
        let reply = match runtime
            .send_and_wait(takka_core::runtime::protocol::Request::ListTools)
            .await
        {
            Ok(reply) => reply,
            Err(e) => {
                inline::print_line(&components::error_badge(&e.to_string()));
                return;
            }
        };

        if let takka_core::Event::ToolList { tools } = reply.event {
            inline::print_blank();
            inline::print_line(&components::section_header(
                "🔧",
                &format!("Available Tools ({})", tools.len()),
                RColor::Cyan,
            ));
            inline::print_blank();
            for t in &tools {
                inline::print_line(&RLine::from(vec![
                    RSpan::raw("  "),
                    RSpan::styled(
                        t.name.clone(),
                        RStyle::default().add_modifier(RModifier::BOLD),
                    ),
                ]));
                inline::print_line(&RLine::from(vec![
                    RSpan::raw("    "),
                    RSpan::raw(t.description.clone()),
                ]));
            }
            inline::print_blank();
        }
    }

    // ── Switch model ────────────────────────────────────────

    /// Switch to a model by name.
    ///
    /// Accepts either a provider-qualified id `"provider/model"` (matched
    /// exactly, the form the REPL model dropdown generates) or a plain /
    /// partial name (matched with the legacy contains() search).
    ///
    /// The qualified form is important: identical model ids or display
    /// names can exist in several providers (e.g. "GLM-4.7" in two
    /// providers, or an aliased id like "sm/deepseek-v4-flash" displayed
    /// as "kimi-k2.6"). A plain contains() search would silently pick the
    /// *first* provider in config order instead of the one the user chose.
    ///
    /// Returns (provider_name, model_name) if switched, or error string.
    pub fn switch_model(&mut self, name: &str) -> Result<(String, String), String> {
        let (pi, mi) = Self::find_model_index(&self.config, name)
            .ok_or_else(|| format!("Model '{}' not found", name))?;

        let (active_provider, active_model) = Self::reorder_config(&mut self.config, pi, mi);

        // Persist. Under test builds this refuses to touch the real config
        // path (see persist_config) so a stray test can never clobber the
        // developer's actual ~/.config/agentic/config.json.
        self.persist_config()?;

        // Reset orchestrator so it reinitializes with new model

        self.connection = None;

        Ok((active_provider, active_model))
    }

    /// Save the config to disk.
    ///
    /// In test builds this is a hard error: [`Config::save`] writes to the
    /// real user config path and a test must never reach it. This guard
    /// exists because a unit test once called `switch_model` and silently
    /// overwrote a developer's real `~/.config/agentic/config.json`.
    fn persist_config(&self) -> Result<(), String> {
        #[cfg(test)]
        {
            Err("refusing to save config under test (would overwrite the real ~/.config/agentic/config.json)"
                .to_string())
        }
        #[cfg(not(test))]
        {
            self.config
                .save()
                .map_err(|e| format!("Failed to save config: {}", e))
        }
    }

    /// Locate the (provider_idx, model_idx) for `name`.
    ///
    /// Tries an exact provider-qualified match (`"provider/model"`) first
    /// — the form the REPL model dropdown generates — then falls back to a
    /// loose contains() search for plain / partial names (manual typing).
    fn find_model_index(config: &Config, name: &str) -> Option<(usize, usize)> {
        if let Some((provider_name, model_name)) = name.split_once('/') {
            if let Some(found) = Self::find_model_exact(config, provider_name, model_name) {
                return Some(found);
            }
            // Fall through to the loose search (e.g. the user typed an id
            // that merely contains '/' but isn't a valid provider).
        }

        let name_lower = name.to_lowercase();
        for (pi, provider) in config.providers.iter().enumerate() {
            for (mi, model) in provider.models.iter().enumerate() {
                let display = model.display_name.as_deref().unwrap_or(&model.model);
                if model.model.to_lowercase().contains(&name_lower)
                    || display.to_lowercase().contains(&name_lower)
                {
                    return Some((pi, mi));
                }
            }
        }
        None
    }

    /// Exact provider + model-id lookup (case-insensitive).
    fn find_model_exact(
        config: &Config,
        provider_name: &str,
        model_name: &str,
    ) -> Option<(usize, usize)> {
        for (pi, provider) in config.providers.iter().enumerate() {
            if !provider.name.eq_ignore_ascii_case(provider_name) {
                continue;
            }
            for (mi, model) in provider.models.iter().enumerate() {
                if model.model.eq_ignore_ascii_case(model_name) {
                    return Some((pi, mi));
                }
            }
        }
        None
    }

    /// Reorder config so (pi, mi) becomes the active provider/model.
    /// Returns the resulting (provider, model) display names.
    fn reorder_config(config: &mut Config, pi: usize, mi: usize) -> (String, String) {
        // Reorder: move selected provider to front, selected model to front
        let mut providers = config.providers.clone();
        let mut provider = providers.remove(pi);
        let model = provider.models.remove(mi);
        provider.models.insert(0, model);
        providers.insert(0, provider);
        config.providers = providers;

        let active_provider = config.providers[0].name.clone();
        let active_model = config.providers[0].models[0]
            .display_name
            .clone()
            .unwrap_or_else(|| config.providers[0].models[0].model.clone());

        (active_provider, active_model)
    }

    /// Interactive model picker using ratatui full-screen TUI.
    /// Returns (provider_name, model_name) if switched, None if cancelled.
    /// Display available models inline (non-modal).
    pub fn list_models_inline(&self) {
        inline::print_blank();
        inline::print_line(&components::section_header(
            "🤖",
            "Available Models",
            RColor::Cyan,
        ));
        inline::print_blank();

        let active_provider = self.config.active_provider().map(|p| p.name.clone());
        let active_model = self.config.active_model().map(|m| m.model.clone());

        for provider in &self.config.providers {
            let provider_style = RStyle::default()
                .fg(RColor::Rgb(255, 215, 0))
                .add_modifier(RModifier::BOLD);

            inline::print_line(&RLine::from(vec![
                RSpan::raw("  "),
                RSpan::styled(format!("📡 {}", provider.name), provider_style),
            ]));

            for model in &provider.models {
                let is_active = active_provider.as_deref() == Some(&provider.name)
                    && active_model.as_deref() == Some(&model.model);

                let display = model.display_name.as_deref().unwrap_or(&model.model);
                let caps = model.effective_capabilities();

                let mut spans = vec![RSpan::raw("    ")];

                if is_active {
                    spans.push(RSpan::styled("● ", RStyle::default().fg(RColor::Green)));
                } else {
                    spans.push(RSpan::raw("  "));
                }

                spans.push(RSpan::styled(
                    display.to_string(),
                    RStyle::default().fg(RColor::Rgb(180, 180, 200)),
                ));

                if caps.vision {
                    spans.push(RSpan::styled(
                        "  👁",
                        RStyle::default().fg(RColor::Rgb(135, 206, 250)),
                    ));
                }

                spans.push(RSpan::styled(
                    format!("  ({})", model.model),
                    RStyle::default()
                        .fg(RColor::Rgb(100, 100, 120))
                        .add_modifier(RModifier::DIM),
                ));

                inline::print_line(&RLine::from(spans));
            }
            inline::print_blank();
        }

        inline::print_line(&RLine::from(vec![
            RSpan::styled("💡 ", RStyle::default()),
            RSpan::styled("Tip: ", RStyle::default().add_modifier(RModifier::DIM)),
            RSpan::raw("Use "),
            RSpan::styled(
                "/models <name>",
                RStyle::default()
                    .fg(RColor::Rgb(255, 215, 0))
                    .add_modifier(RModifier::BOLD),
            ),
            RSpan::raw(" to switch. Type "),
            RSpan::styled("/models ", RStyle::default().fg(RColor::Rgb(255, 215, 0))),
            RSpan::raw("then press "),
            RSpan::styled(
                "Tab",
                RStyle::default()
                    .fg(RColor::Rgb(135, 206, 250))
                    .add_modifier(RModifier::BOLD),
            ),
            RSpan::raw(" for completion"),
        ]));
        inline::print_blank();
    }

    /// Interactive model picker using dialoguer (fuzzy searchable).
    pub fn pick_model_interactive_inline(&mut self) -> Option<(String, String)> {
        use dialoguer::{theme::ColorfulTheme, FuzzySelect};

        let active_provider = self.config.active_provider().map(|p| p.name.clone());
        let active_model = self.config.active_model().map(|m| m.model.clone());

        // Build list of (display_string, provider_name, model_name)
        let mut items: Vec<(String, String, String)> = Vec::new();
        let mut default_idx = 0;

        for provider in &self.config.providers {
            for model in &provider.models {
                let display_name = model.display_name.as_deref().unwrap_or(&model.model);
                let caps = model.effective_capabilities();
                let vision_icon = if caps.vision { " 👁" } else { "" };

                let is_active = active_provider.as_deref() == Some(&provider.name)
                    && active_model.as_deref() == Some(&model.model);

                if is_active {
                    default_idx = items.len();
                }

                let display = format!(
                    "{}{} [{}]{}",
                    display_name,
                    vision_icon,
                    provider.name,
                    if is_active { " ●" } else { "" }
                );

                items.push((display, provider.name.clone(), model.model.clone()));
            }
        }

        if items.is_empty() {
            inline::print_blank();
            inline::print_line(&components::warning_badge("No models configured."));
            inline::print_blank();
            return None;
        }

        inline::print_blank();

        let selection = FuzzySelect::with_theme(&ColorfulTheme::default())
            .with_prompt(
                "🤖 Select Model (type to filter, ↑↓ to navigate, Enter to select, Esc to cancel)",
            )
            .items(&items.iter().map(|(d, _, _)| d.as_str()).collect::<Vec<_>>())
            .default(default_idx)
            .interact_opt();

        match selection {
            Ok(Some(idx)) => {
                let (_, provider, model) = &items[idx];
                // Provider-qualified so identical ids / display names in
                // different providers switch to the exact entry chosen.
                self.switch_model(&format!("{}/{}", provider, model)).ok()
            }
            Ok(None) | Err(_) => None,
        }
    }

    // ── MCP status (for REPL /mcp) ──────────────────────────

    pub fn show_mcp_status(&self) {
        inline::print_blank();

        if self.config.mcp_servers.is_empty() {
            inline::print_line(&components::warning_badge("No MCP servers configured."));
            inline::print_line(&RLine::from(vec![
                RSpan::raw("  Add servers in your config file: "),
                RSpan::raw(Config::config_path().display().to_string()),
            ]));
            inline::print_blank();
            return;
        }

        inline::print_line(&components::section_header(
            "📡",
            &format!("MCP Servers ({})", self.config.mcp_servers.len()),
            RColor::Cyan,
        ));
        inline::print_blank();

        let bold = RStyle::default().add_modifier(RModifier::BOLD);
        for (name, srv) in &self.config.mcp_servers {
            inline::print_line(&RLine::from(vec![
                RSpan::raw("  "),
                RSpan::styled(name.clone(), bold),
            ]));
            if let Some(cmd) = &srv.command {
                inline::print_line(&RLine::from(format!("    Command: {}", cmd)));
            }
            if let Some(args) = &srv.args {
                if !args.is_empty() {
                    inline::print_line(&RLine::from(format!("    Args:    {}", args.join(" "))));
                }
            }
            if let Some(url) = &srv.url {
                inline::print_line(&RLine::from(format!("    URL:     {}", url)));
            }
            inline::print_blank();
        }
    }

    // ── Memory search (for REPL /search) ────────────────────

    /// Render a `/search <query>` result against the current orchestrator's
    /// conversation memory. No-op (with a hint) if the orchestrator hasn't
    /// been initialized yet — i.e. before the first turn.
    pub async fn search_memory_inline(&mut self, query: &str) {
        let query = query.trim();
        if query.is_empty() {
            inline::print_blank();
            inline::print_line(&components::warning_badge("Search query is empty."));
            inline::print_blank();
            return;
        }
        if let Err(error) = self.ensure_connection().await {
            inline::print_line(&components::error_badge(&error.to_string()));
            return;
        }
        let response = self
            .connection
            .as_mut()
            .unwrap()
            .send_and_wait(takka_core::runtime::protocol::Request::SearchMemory {
                query: query.to_string(),
            })
            .await;
        match response {
            Ok(event) => {
                if let takka_core::Event::MemorySearchResult { query, matches } = event.event {
                    inline::print_blank();
                    inline::print_line(&components::section_header(
                        "🔍",
                        &format!("Memory search: \"{}\" — {} match(es)", query, matches.len()),
                        RColor::Cyan,
                    ));
                    inline::print_blank();
                    for (index, hit) in matches.iter().enumerate() {
                        inline::print_line(&RLine::from(format!("  [{}] {}", index + 1, hit.role)));
                        inline::print_line(&RLine::from(format!("      {}", hit.content)));
                    }
                    inline::print_blank();
                }
            }
            Err(error) => inline::print_line(&components::error_badge(&error.to_string())),
        }
    }

    // ── Plan (for REPL /plan) ───────────────────────────

    /// Plan a goal through `takka_core::PlannerAgent`: ask the model
    /// for a step-by-step plan, render it for the operator, and ask for
    /// approval before executing the steps.
    ///
    /// This replaces the older 'just-prefix-the-prompt' fallback. The
    /// orchestrator and tool registry are reused from the current
    /// session so plan steps run with the same safety + permissions as
    /// regular turns.
    pub async fn plan_inline(&mut self, goal: &str) -> anyhow::Result<()> {
        let goal = goal.trim();
        if goal.is_empty() {
            inline::print_blank();
            inline::print_line(&components::warning_badge("Plan goal is empty."));
            inline::print_blank();
            return Ok(());
        }

        self.plan_and_execute(goal, true).await
    }

    /// `takka run --plan <task>`: one-shot plan-then-execute without
    /// entering interactive mode. Respects the `require_approval` config
    /// setting (defaults to true).
    pub async fn plan_run(&mut self, task: &str) -> anyhow::Result<()> {
        let task = task.trim();
        if task.is_empty() {
            inline::print_blank();
            inline::print_line(&components::warning_badge("Task is empty."));
            inline::print_blank();
            return Ok(());
        }

        // Use require_approval from config (default: true)
        self.plan_and_execute(task, self.config.agent.planner.require_approval)
            .await
    }

    /// Shared planner logic: create a plan, optionally ask for approval,
    /// render live progress, and execute.
    ///
    /// `ask_approval`: when true, render the plan and prompt with
    /// dialoguer. When false, skip the prompt (auto-approve).
    async fn plan_and_execute(&mut self, goal: &str, ask_approval: bool) -> anyhow::Result<()> {
        inline::print_blank();
        inline::print_line(&components::section_header(
            "🗺️",
            &format!("Planning: {}", goal),
            RColor::Cyan,
        ));
        inline::print_blank();

        if let Err(e) = self.ensure_connection().await {
            inline::print_line(&components::error_badge(&e.to_string()));
            return Ok(());
        }
        let runtime = self.connection.as_mut().unwrap();

        // Full planner lifecycle on the daemon: LLM plan creation, the
        // approval gate (blocking dialoguer prompt is safe here — the
        // daemon is parked waiting for our ConfirmResponse), then live
        // execution progress.
        runtime
            .send(takka_core::runtime::protocol::Request::Plan {
                goal: goal.to_string(),
                require_approval: ask_approval,
            })
            .await?;

        loop {
            let event = runtime.next_event().await?.event;
            match event {
                takka_core::Event::PlanApprovalRequest { steps, .. } => {
                    let dim = RStyle::default().add_modifier(RModifier::DIM);
                    let bold = RStyle::default().add_modifier(RModifier::BOLD);
                    for (i, step) in steps.iter().enumerate() {
                        inline::print_line(&RLine::from(vec![
                            RSpan::styled(format!("  {:>2}. ", i + 1), dim),
                            RSpan::styled(step.description.clone(), bold),
                        ]));
                    }
                    inline::print_blank();

                    let proceed = if ask_approval {
                        Confirm::with_theme(&ColorfulTheme::default())
                            .with_prompt("Execute this plan?")
                            .default(false)
                            .interact()
                            .unwrap_or(false)
                    } else {
                        inline::print_line(&RLine::from(vec![RSpan::styled(
                            "  Auto-approved (require_approval = false)\n".to_string(),
                            RStyle::default().fg(RColor::DarkGray),
                        )]));
                        true
                    };

                    runtime
                        .send(takka_core::runtime::protocol::Request::ConfirmResponse {
                            request_id: None,
                            approved: proceed,
                        })
                        .await?;

                    if !proceed {
                        inline::print_line(&components::warning_badge(
                            "Plan rejected. Nothing executed.",
                        ));
                        inline::print_blank();
                        return Ok(());
                    }
                }
                takka_core::Event::PlanProgress {
                    step_description,
                    step_status,
                    steps_total,
                    steps_completed,
                    steps_failed,
                    ..
                } => {
                    let icon = match step_status.as_str() {
                        "in_progress" => "▶",
                        "completed" => "✅",
                        "failed" => "❌",
                        _ => "⏳",
                    };
                    let fraction = if steps_total > 0 {
                        steps_completed as f32 / steps_total as f32
                    } else {
                        0.0
                    };
                    let bar = components::labeled_bar(
                        "progress",
                        fraction,
                        20,
                        RColor::Rgb(46, 204, 113),
                        RColor::Rgb(60, 60, 80),
                    );
                    inline::print_line(&RLine::from(vec![
                        RSpan::raw(format!("  {} ", icon)),
                        RSpan::styled(
                            step_description.clone(),
                            RStyle::default().add_modifier(RModifier::DIM),
                        ),
                    ]));
                    inline::print_line(&bar);
                    inline::print_line(&RLine::from(vec![
                        RSpan::raw("     "),
                        RSpan::styled(
                            format!("{} done / {} failed", steps_completed, steps_failed),
                            RStyle::default().fg(RColor::DarkGray),
                        ),
                    ]));
                    inline::print_blank();
                }
                takka_core::Event::Done { result } => {
                    inline::print_line(&components::success_badge(&result));
                    inline::print_blank();
                    return Ok(());
                }
                takka_core::Event::Error { message } => {
                    inline::print_line(&components::error_badge(&message));
                    inline::print_blank();
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    pub fn examples(&self) {
        let yellow_comment = RStyle::default().fg(RColor::Yellow);

        inline::print_blank();
        inline::print_line(&components::section_header(
            "📖",
            "Takka CLI — Usage Examples",
            RColor::Cyan,
        ));
        inline::print_blank();

        let groups: &[(&str, &[&str])] = &[
            (
                "# Run a single task",
                &[
                    "takka run \"list all Rust files\"",
                    "takka run \"create hello.txt with 'hello world'\"",
                    "takka run \"explain the codebase structure\"",
                ],
            ),
            ("# Interactive mode", &["takka interactive", "takka i"]),
            (
                "# Config management",
                &[
                    "takka config init                    # Default config",
                    "takka config init --interactive      # Guided wizard",
                    "takka config init --provider openai  # Quick setup",
                    "takka config show",
                    "takka config show --format table",
                    "takka config edit",
                    "takka config validate",
                    "takka config backup",
                    "takka config export                  # Masked secrets",
                ],
            ),
            ("# Status & info", &["takka status", "takka version"]),
        ];

        for (heading, lines) in groups {
            inline::print_line(&RLine::from(vec![
                RSpan::raw("  "),
                RSpan::styled((*heading).to_string(), yellow_comment),
            ]));
            for cmd in *lines {
                inline::print_line(&RLine::from(format!("  {}", cmd)));
            }
            inline::print_blank();
        }
    }

    // ── Run task ────────────────────────────────────────────

    pub async fn run(&mut self, task: &str) -> Result<()> {
        use crate::widgets::{components, inline, markdown as md_widget, progress, spinner};

        // When --mode plan is active, route through the planner agent
        // instead of the regular orchestrator loop. This creates a real
        // plan, shows it, and executes approved steps.
        if self.permission_mode == takka_core::PermissionMode::Plan {
            return self.plan_run(task).await;
        }

        // Expand @file references and extract any image attachments
        // (`@photo.png`) for the vision channel. Pending attachments
        // queued by `/image` are drained here too.
        let expanded = crate::file_ref::expand_with_attachments(task);
        let mut attachments = self.drain_pending_attachments();
        attachments.extend(expanded.attachments);
        let task = &expanded.text;

        if !attachments.is_empty() {
            // Pre-flight capability check so we fail fast with a clear,
            // user-actionable error instead of waiting on a provider 4xx.
            let caps = self.active_model_capabilities();
            if !caps.vision {
                inline::print_blank();
                inline::print_line(&components::error_badge(
                    "Active model does not support image input.",
                ));
                inline::print_line(&RLine::from(vec![
                    RSpan::raw("  Switch with "),
                    RSpan::styled("/models", RStyle::default().add_modifier(RModifier::BOLD)),
                    RSpan::raw(" to a vision-capable model (e.g. gpt-4o, claude-3-5-sonnet)."),
                ]));
                inline::print_blank();
                return Ok(());
            }
        }

        self.ensure_connection().await?;
        #[cfg(test)]
        self.test_runs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Runtime events are forwarded to the existing single-writer renderer.
        let show_tool_calls = self.config.output.show_tool_calls;
        let (event_tx, event_rx) = std::sync::mpsc::channel::<takka_core::Event>();

        // ── Live rendering strategy (single writer) ───────────
        //
        // ALL terminal output during the run flows through ONE task
        // (the `renderer` below). This fixes two bugs the old two-writer
        // design suffered from:
        //
        //   1. Corruption during tool use: the spinner ticker and the
        //      on_chunk callback wrote to stdout from different tasks and
        //      raced on cursor position, so streamed-text fragments
        //      (e.g. a command tail like `-maxdepth 4 …`) leaked into the
        //      "Thinking…" spinner line.
        //   2. Duplicate final text: the end-of-run markdown re-render
        //      miscounted visual lines (it ignored line wrapping and the
        //      tool-panel lines interleaved between streamed turns), so
        //      streamed plaintext was left behind after the re-render.
        //
        // Now chunks are forwarded to a channel and printed by the SAME
        // task that ticks the spinner and renders events, so the three
        // can never interleave. A state machine maintains the invariant
        // "the spinner is always the last printed line, cursor directly
        // below it"; streamed text replaces the spinner in place, and a
        // tool panel always ends by re-arming a fresh spinner line.
        //
        // At the end of the run, the accumulated text block is rendered
        // as styled markdown (instead of leaving raw plaintext on screen),
        // giving the same polished output the old markdown re-render aimed
        // for — but without line-counting bugs, because the renderer tracks
        // visual lines accurately for the ONE block it replaces.

        // Chunk channel: on_chunk forwards deltas here instead of writing
        // to stdout directly (keeps all writes in the renderer task).
        // Chunk channel (std::sync::mpsc for cross-thread use).
        let (chunk_tx, chunk_rx) = std::sync::mpsc::channel::<String>();

        let progress = std::sync::Arc::new(std::sync::Mutex::new({
            let mut p = progress::ProgressState::new();
            p.start();
            p.set_message("Thinking…".to_string());
            p
        }));
        let stop_flag = std::sync::Arc::new(AtomicBool::new(false));

        // Mark the spinner renderer as alive before spawning it: the
        // question handler reads this to decide whether it must consume
        // / re-arm the transient spinner line around its prompt.
        SPINNER_ACTIVE.store(true, Ordering::Relaxed);

        // Renderer state machine.
        //
        // Strategy: NEVER write streamed chunks to stdout.  Only the
        // spinner line and tool panels touch the terminal.  At the end
        // of the run, the spinner is cleared and the accumulated text
        // is rendered as styled markdown in one shot.  This eliminates
        // ALL cursor-position bugs (line-counting, MoveUp overshoot,
        // trailing-newline ambiguity) because there is nothing to
        // "replace" — the markdown is the first and only text output.
        struct RenderFinal {
            /// Accumulated text from all streaming blocks.
            block_text: String,
        }

        let r_progress = progress.clone();
        let r_stop = stop_flag.clone();
        let render_panels = show_tool_calls;
        let renderer = std::thread::Builder::new()
            .name("takka-renderer".into())
            .spawn(move || {
                use crossterm::cursor::{MoveToColumn, MoveUp};
                use crossterm::terminal::{Clear, ClearType};
                use crossterm::ExecutableCommand;

                let block_text = std::sync::Mutex::new(String::new());

                // ── spinner cursor helpers ─────────────────────────────
                let commit_spinner = || {
                    let line = {
                        let mut p = r_progress.lock().unwrap();
                        p.tick();
                        spinner::compact_progress_line(&p, 18)
                    };
                    inline::print_line(&line);
                };
                let redraw_spinner = || {
                    let line = {
                        let mut p = r_progress.lock().unwrap();
                        p.tick();
                        spinner::compact_progress_line(&p, 18)
                    };
                    // Overwrite in place (the line is fixed-width) instead
                    // of Clear(CurrentLine) + repaint. The erase-then-draw
                    // cycle makes the line blink on Windows conhost (the
                    // "dim lamp flicker") — a plain overwrite of the same
                    // width is seamless everywhere.
                    let mut s = std::io::stdout();
                    let _ = s.execute(MoveUp(1));
                    let _ = s.execute(MoveToColumn(0));
                    inline::print_line(&line);
                };
                let consume_spinner = || {
                    let mut s = std::io::stdout();
                    let _ = s.execute(MoveUp(1));
                    let _ = s.execute(MoveToColumn(0));
                    let _ = s.execute(Clear(ClearType::FromCursorDown));
                };

                // Print the initial spinner.
                commit_spinner();

                // Wrap the receivers in a Mutex so we can share them
                // (they are !Sync but we only access from this thread).
                let chunk_rx = std::sync::Mutex::new(chunk_rx);
                let event_rx = std::sync::Mutex::new(event_rx);
                let mut chunks_done = false;
                let mut events_done = false;

                loop {
                    if chunks_done && events_done {
                        break;
                    }

                    // Sleep 80ms between spinner ticks.
                    std::thread::sleep(std::time::Duration::from_millis(80));

                    // Hold the render gate for the whole terminal-touching
                    // phase. While the `question` handler is prompting the
                    // user it owns this gate and we park here without
                    // writing, so the dialoguer list never races the
                    // spinner's MoveUp/Clear redraws.
                    let _render_gate = RENDER_GATE.lock().unwrap();

                    if !r_stop.load(Ordering::Relaxed) {
                        redraw_spinner();
                    }

                    // Drain all available chunks (non-blocking).
                    if !chunks_done {
                        loop {
                            match chunk_rx.lock().unwrap().try_recv() {
                                Ok(chunk) => {
                                    if !chunk.is_empty() {
                                        block_text.lock().unwrap().push_str(&chunk);
                                    }
                                }
                                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                    chunks_done = true;
                                    break;
                                }
                            }
                        }
                    }

                    // Drain all available events (non-blocking).
                    if !events_done {
                        loop {
                            match event_rx.lock().unwrap().try_recv() {
                                Ok(event) => {
                                    consume_spinner();
                                    if render_panels {
                                        render_event(&event);
                                    }
                                    commit_spinner();
                                }
                                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                    events_done = true;
                                    break;
                                }
                            }
                        }
                    }
                }

                RenderFinal {
                    block_text: block_text.into_inner().unwrap(),
                }
            })
            .expect("failed to spawn renderer thread");

        let runtime = self
            .connection
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Runtime not initialized"))?;
        let result = runtime
            .run(
                task.to_string(),
                attachments,
                |chunk| {
                    if !chunk.is_empty() {
                        let _ = chunk_tx.send(chunk.to_string());
                    }
                },
                {
                    // Test tap: drained follow-up runs travel through this
                    // path, so e2e tests observe their lifecycle here.
                    #[cfg(test)]
                    let test_events = std::sync::Arc::clone(&self.test_events);
                    // `move` (needed for the test tap) would move the
                    // original sender; forward through a clone instead so
                    // `drop(event_tx)` below still releases the renderer.
                    let event_tx = event_tx.clone();
                    move |event| {
                        #[cfg(test)]
                        test_events.lock().unwrap().push(format!("{event:?}"));
                        if !matches!(event, takka_core::Event::Thinking { .. }) {
                            let _ = event_tx.send(event);
                        }
                    }
                },
                |event| match &event.event {
                    takka_core::Event::ConfirmationRequest { description, .. } => {
                        let approved = Confirm::with_theme(&ColorfulTheme::default())
                            .with_prompt(description)
                            .default(false)
                            .interact()
                            .unwrap_or(false);
                        Some(takka_core::runtime::protocol::Request::ConfirmResponse {
                            request_id: event.request_id.clone(),
                            approved,
                        })
                    }
                    takka_core::Event::QuestionRequest { questions } => {
                        let answers =
                            takka_core::QuestionHandler::handle(&CliQuestionHandler, questions);
                        Some(takka_core::runtime::protocol::Request::QuestionResponse {
                            request_id: event.request_id.clone(),
                            answers,
                        })
                    }
                    _ => None,
                },
            )
            .await;

        drop(chunk_tx);
        drop(event_tx);
        stop_flag.store(true, Ordering::Relaxed);

        let render_final = renderer.join().expect("renderer thread panicked");
        // The question handler only does its spinner consume/re-arm dance
        // while this flag is set, so it never touches the screen in modes
        // where no transient spinner exists (e.g. the full-screen TUI).
        SPINNER_ACTIVE.store(false, Ordering::Relaxed);
        progress.lock().unwrap().stop();

        // Cursor cleanup + final result.  The renderer always runs in
        // "spinner mode" (we never wrote chunks to stdout), so cleanup
        // is the same in every case: erase the spinner line, then render
        // the accumulated text as styled markdown.
        let mut s = std::io::stdout();
        {
            use crossterm::cursor::{MoveToColumn, MoveUp};
            use crossterm::terminal::{Clear, ClearType};
            use crossterm::ExecutableCommand;
            let _ = s.execute(MoveUp(1));
            let _ = s.execute(MoveToColumn(0));
            let _ = s.execute(Clear(ClearType::FromCursorDown));
        }
        // Flush buffered writes so inline::print_* output below is not
        // reordered with the terminal-control sequences written above.
        let _ = std::io::Write::flush(&mut s);

        match result {
            Ok(final_result) => {
                let source = if final_result.is_empty() {
                    &render_final.block_text
                } else {
                    &final_result
                };
                if !source.is_empty() {
                    let parsed = md_widget::MarkdownContent::parse(source);
                    inline::print_lines(&parsed.lines);
                }
                inline::print_blank();
            }
            Err(e) => {
                inline::print_line(&components::error_badge(&e.to_string()));
                inline::print_blank();
            }
        }

        // After the run's terminal event (Done or Error), drain queued
        // follow-ups: text submitted mid-run in FollowUp mode is sent as
        // the next run (client-issued, depth-guarded loop). Both outcome
        // branches above converge here.
        self.drain_follow_ups().await?;

        Ok(())
    }
    /// Run task with separate callbacks for streaming chunks and runtime
    /// events (tool calls + results). Used by the TUI to surface tool
    /// activity in the message log alongside the streaming response.
    ///
    /// `on_chunk` receives token deltas as the model generates them.
    /// `on_event` receives `takka_core::Event` for every tool call and
    /// result emitted by the orchestrator.
    ///
    /// Both callbacks must be `Send + Sync + 'static` because the event
    /// stream is dispatched on blocking-pool threads.
    pub async fn run_with_callbacks<C, E>(
        &mut self,
        task: &str,
        mut on_chunk: C,
        on_event: E,
    ) -> Result<String>
    where
        C: FnMut(&str),
        E: Fn(takka_core::Event) + Send + Sync + 'static,
    {
        let expanded = crate::file_ref::expand_with_attachments(task);
        self.ensure_connection().await?;
        #[cfg(test)]
        self.test_runs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let runtime = self
            .connection
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Runtime not initialized"))?;

        let mut event_callback = Some(on_event);
        let result = runtime
            .run(
                expanded.text,
                expanded.attachments,
                |chunk| on_chunk(chunk),
                |event| {
                    if let Some(callback) = event_callback.as_mut() {
                        callback(event);
                    }
                },
                |event| match &event.event {
                    takka_core::Event::ConfirmationRequest { description, .. } => {
                        let approved = prompt_confirmation(&takka_core::ConfirmationRequest {
                            action: "runtime".to_string(),
                            description: description.clone(),
                            risk_level: takka_core::RiskLevel::Medium,
                            risk_score: 0.5,
                            reason: description.clone(),
                            timestamp: chrono::Utc::now(),
                            preview_diff: None,
                        })
                        .map(|response| {
                            matches!(
                                response,
                                ConfirmationResponse::Yes | ConfirmationResponse::Always
                            )
                        })
                        .unwrap_or(false);
                        Some(takka_core::runtime::protocol::Request::ConfirmResponse {
                            request_id: event.request_id.clone(),
                            approved,
                        })
                    }
                    takka_core::Event::QuestionRequest { questions } => {
                        let answers =
                            takka_core::QuestionHandler::handle(&CliQuestionHandler, questions);
                        Some(takka_core::runtime::protocol::Request::QuestionResponse {
                            request_id: event.request_id.clone(),
                            answers,
                        })
                    }
                    _ => None,
                },
            )
            .await;

        // Same drain-after-Done contract as `run()`: after the run's
        // terminal event, queued follow-ups are sent as the next run(s).
        // Fires for both outcomes (Ok/Err) — a failed session also has a
        // terminal event, and queued text should still reach the engine.
        self.drain_follow_ups().await?;

        result
    }

    // ── Follow-up queue (QueueMode) ───────────────────────

    /// Active queue mode (how text typed during a run is handled).
    pub fn queue_mode_getter(&self) -> QueueMode {
        self.queue_mode
    }

    /// Switch the queue mode at runtime (the `/queue` slash command).
    pub fn set_queue_mode(&mut self, mode: QueueMode) {
        self.queue_mode = mode;
    }

    /// Text queued in FollowUp mode awaiting the next drain.
    pub fn pending_follow_ups(&self) -> &std::collections::VecDeque<String> {
        &self.pending_follow_ups
    }

    /// Programmatic entrypoint for user text typed while a run may be
    /// active (unit-tested now; the TUI wires it up in a later phase —
    /// inline REPL input-during-run is out of scope).
    ///
    /// Returns `Ok(true)` when the text was accepted (steered or queued).
    ///
    /// - Interrupt mode: fire-and-forget steer to the engine (injected at
    ///   the next tool boundary; requires an active run, else the engine
    ///   answers `Error { "no active run" }` on the event stream).
    /// - FollowUp mode: pushed onto the local queue; sent as the next run
    ///   once the current one reaches its terminal event.
    pub(crate) async fn submit_user_text(&mut self, text: String) -> Result<bool> {
        match self.queue_mode {
            QueueMode::Interrupt => {
                let conn = self
                    .connection
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("not connected"))?;
                conn.steer(text).await?;
                Ok(true)
            }
            QueueMode::FollowUp => {
                self.pending_follow_ups.push_back(text);
                Ok(true)
            }
        }
    }

    /// After a run's terminal event: drain the follow-up queue — send each
    /// queued item as the next run (client-issued, loop, depth-guarded).
    ///
    /// Drained items go through the full `run()` path, which drains again
    /// on completion; `drain_depth` keeps that nesting bounded (see
    /// [`MAX_FOLLOW_UP_DRAIN_DEPTH`]). Leftover items past the cap stay
    /// queued for the next drain.
    ///
    /// Returns a boxed future: `run → drain → run` is recursive at the
    /// type level, and async fns refuse unboxed recursion (the runtime
    /// bound is the depth guard, not the type).
    fn drain_follow_ups(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            if self.drain_depth >= MAX_FOLLOW_UP_DRAIN_DEPTH {
                return Ok(());
            }
            self.drain_depth += 1;
            let result = self.drain_follow_ups_loop().await;
            self.drain_depth -= 1;
            result
        })
    }

    async fn drain_follow_ups_loop(&mut self) -> Result<()> {
        while let Some(next) = self.pending_follow_ups.pop_front() {
            self.run(&next).await?;
        }
        Ok(())
    }

    // ── Config dispatch ─────────────────────────────────────

    pub fn config(&self, action: &ConfigAction) -> Result<()> {
        match action {
            ConfigAction::Show { format } => self.config_show(*format)?,
            ConfigAction::Init {
                interactive,
                provider,
            } => self.config_init(*interactive, provider.as_deref())?,
            ConfigAction::Edit => self.config_edit()?,
            ConfigAction::Validate { verbose } => self.config_validate(*verbose)?,
            ConfigAction::Reset { force } => self.config_reset(*force)?,
            ConfigAction::Path => self.config_path()?,
            ConfigAction::Backup => self.config_backup()?,
            ConfigAction::Restore { file } => self.config_restore(file)?,
            ConfigAction::Export => self.config_export()?,
            ConfigAction::Import { file } => self.config_import(file)?,
        }
        Ok(())
    }

    // ── Skill command dispatch ────────────────────────────────

    pub fn skill_command(&self, action: &SkillAction) -> Result<()> {
        match action {
            SkillAction::List => self.skill_list()?,
            SkillAction::Info { name } => self.skill_info(name)?,
            SkillAction::Create { name, global } => self.skill_create(name, *global)?,
        }
        Ok(())
    }

    fn skill_list(&self) -> Result<()> {
        let discovery_config: takka_core::DiscoveryConfig =
            takka_core::DiscoveryConfig::from(&self.config.skills);
        let index = takka_core::discover_skills(&discovery_config);

        if index.is_empty() {
            println!();
            println!("  No skills found.");
            println!();
            println!("  Create one:  takka skill create <name>");
            println!();
            return Ok(());
        }

        println!();
        println!("  Skills");

        let mut skills: Vec<_> = index.all().into_iter().collect();
        skills.sort_by(|a, b| a.name().cmp(b.name()));

        for skill in &skills {
            println!();
            println!("  📦 {}", skill.name());
            println!("     {}", skill.description());
            println!("     Path: {}", skill.dir.display());
        }

        if !index.blocked().is_empty() {
            println!();
            println!("  Blocked:");
            for name in index.blocked() {
                println!("     ✗ {}", name);
            }
        }

        println!();
        Ok(())
    }

    fn skill_info(&self, name: &str) -> Result<()> {
        let discovery_config: takka_core::DiscoveryConfig =
            takka_core::DiscoveryConfig::from(&self.config.skills);
        let index = takka_core::discover_skills(&discovery_config);

        let skill = match index.get(name) {
            Some(s) => s,
            None => {
                eprintln!("✗ Skill '{}' not found.", name);
                std::process::exit(1);
            }
        };

        println!();
        println!("  📦 {} — {}", skill.name(), skill.description());
        println!("     Path: {}", skill.dir.display());
        println!("     SKILL.md size: {} bytes", skill.content.len());
        println!();

        // Show preview (first 20 lines of body)
        let preview_lines: Vec<&str> = skill.body.lines().take(20).collect();
        if !preview_lines.is_empty() {
            println!("  Preview:");
            for line in &preview_lines {
                println!("    {}", line);
            }
            if skill.body.lines().count() > 20 {
                println!("    ... ({} more lines)", skill.body.lines().count() - 20);
            }
        }

        // List other files in the skill directory
        if let Ok(entries) = std::fs::read_dir(&skill.dir) {
            let files: Vec<_> = entries
                .flatten()
                .filter(|e| e.path().is_file())
                .map(|e| e.path())
                .filter(|p| p.file_name().and_then(|n| n.to_str()) != Some("SKILL.md"))
                .collect();
            if !files.is_empty() {
                println!();
                println!("  Referenced files:");
                for f in &files {
                    println!(
                        "     📄 {}",
                        f.file_name().unwrap_or_default().to_string_lossy()
                    );
                }
            }
        }

        println!();
        Ok(())
    }

    fn skill_create(&self, name: &str, global: bool) -> Result<()> {
        // Validate name
        let name_re = regex::Regex::new(r"^[a-z0-9-]{1,64}$").unwrap();
        if !name_re.is_match(name) {
            eprintln!(
                "✗ Invalid skill name '{}': must be 1-64 chars, lowercase a-z, 0-9, hyphens only",
                name
            );
            std::process::exit(1);
        }

        let base_dir = if global {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            std::path::PathBuf::from(home)
                .join(".config")
                .join("agentic")
                .join("skills")
        } else {
            // Project-local: .agentic/skills/ in cwd (or walk-up)
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let mut dir = find_or_create_dir(&cwd, ".agentic");
            dir.push("skills");
            dir
        };

        let skill_dir = base_dir.join(name);
        if skill_dir.exists() {
            eprintln!(
                "✗ Skill '{}' already exists at {}",
                name,
                skill_dir.display()
            );
            std::process::exit(1);
        }

        std::fs::create_dir_all(&skill_dir)
            .map_err(|e| anyhow::anyhow!("Failed to create skill directory: {}", e))?;

        let template = format!(
            r"---
name: {name}
description: What this skill does and when to use it.
---

# {name}

## Setup
(optional setup instructions)

## Usage
Instructions the agent follows when this skill is loaded.
"
        );

        std::fs::write(skill_dir.join("SKILL.md"), &template)
            .map_err(|e| anyhow::anyhow!("Failed to write SKILL.md: {}", e))?;

        println!();
        println!("  ✅ Skill '{}' created at:", name);
        println!("     {}", skill_dir.display());
        println!();
        println!("  Edit SKILL.md to add instructions, then run:");
        println!("     takka skill list");
        println!();

        Ok(())
    }

    /// Name of the skill active for this session, if any.
    pub fn active_skill_name(&self) -> Option<String> {
        self.active_skill.clone()
    }

    /// Available skills for this session (name, description) from the
    /// discovered index — per-instance replacement for the removed
    /// global `list_skills`.
    pub fn available_skills(&self) -> Vec<(String, String)> {
        match &self.skill_index {
            Some(index) => index
                .read()
                .unwrap()
                .all()
                .into_iter()
                .map(|s| (s.name().to_string(), s.description().to_string()))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Load a skill by name and inject its instructions into the orchestrator's
    /// conversation context as a system message. The next LLM call will see the
    /// skill instructions alongside the user's message.
    ///
    /// Returns the skill body content on success.
    pub async fn load_and_activate_skill(&mut self, name: &str) -> Result<String, String> {
        if let Err(e) = self.ensure_connection().await {
            return Err(e.to_string());
        }
        let runtime = self.connection.as_mut().unwrap();
        let reply = runtime
            .send_and_wait(takka_core::runtime::protocol::Request::SkillActivate {
                name: name.to_string(),
            })
            .await
            .map_err(|e| e.to_string())?;

        match reply.event {
            takka_core::Event::SkillActivatedResult {
                skill,
                activated,
                message,
                content,
            } => {
                self.active_skill = if activated { Some(skill) } else { None };
                match message {
                    Some(m) => Err(m),
                    None => Ok(content),
                }
            }
            other => Err(format!("unexpected runtime reply: {other:?}")),
        }
    }

    // ── Config show (json or table) ─────────────────────────

    fn config_show(&self, format: OutputFormat) -> Result<()> {
        match format {
            OutputFormat::Json => {
                let json = serde_json::to_string_pretty(&self.config)
                    .map_err(|e| CommandError::Config(e.to_string()))?;
                println!("{}", json);
            }
            OutputFormat::Table => {
                self.config_show_table()?;
            }
        }
        Ok(())
    }

    fn config_show_table(&self) -> Result<()> {
        println!();

        // Providers table
        let mut table = Table::new();
        table
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec![
                Cell::new("Provider").fg(TColor::Cyan),
                Cell::new("Type").fg(TColor::Cyan),
                Cell::new("API Base").fg(TColor::Cyan),
                Cell::new("Key").fg(TColor::Cyan),
                Cell::new("Models").fg(TColor::Cyan),
            ]);

        for p in &self.config.providers {
            let key_status = if p.api_key.is_empty() {
                Cell::new("✗").fg(TColor::Red)
            } else if p.api_key.starts_with('$') {
                Cell::new(format!("env:{}", p.api_key)).fg(TColor::Yellow)
            } else {
                Cell::new(format!(
                    "{}...{}",
                    &p.api_key[..4.min(p.api_key.len())],
                    &p.api_key[p.api_key.len().saturating_sub(4)..]
                ))
                .fg(TColor::Green)
            };

            let models: String = p
                .models
                .iter()
                .map(|m| m.display_name.as_deref().unwrap_or(&m.model))
                .collect::<Vec<_>>()
                .join(", ");

            table.add_row(vec![
                Cell::new(&p.name),
                Cell::new(&p.provider_type),
                Cell::new(&p.api_base),
                key_status,
                Cell::new(if models.is_empty() { "none" } else { &models }),
            ]);
        }

        inline::print_line(&RLine::from(vec![
            RSpan::raw("  "),
            RSpan::styled(
                "Providers:",
                RStyle::default().add_modifier(RModifier::BOLD),
            ),
        ]));
        println!("{table}");

        // Safety table
        let mut safety_table = Table::new();
        safety_table
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec![
                Cell::new("Setting").fg(TColor::Cyan),
                Cell::new("Value").fg(TColor::Cyan),
            ]);

        safety_table.add_row(vec![
            Cell::new("Auto-approve low risk"),
            Cell::new(if self.config.safety.auto_approve_low_risk {
                "yes"
            } else {
                "no"
            }),
        ]);

        let blocked = if self.config.safety.blocked_commands.is_empty() {
            "none".to_string()
        } else {
            self.config.safety.blocked_commands.join(", ")
        };
        safety_table.add_row(vec![Cell::new("Blocked commands"), Cell::new(blocked)]);

        inline::print_line(&RLine::from(vec![
            RSpan::raw("  "),
            RSpan::styled("Safety:", RStyle::default().add_modifier(RModifier::BOLD)),
        ]));
        println!("{safety_table}");

        // Output table
        let mut output_table = Table::new();
        output_table
            .load_preset(UTF8_FULL)
            .apply_modifier(UTF8_ROUND_CORNERS)
            .set_header(vec![
                Cell::new("Setting").fg(TColor::Cyan),
                Cell::new("Value").fg(TColor::Cyan),
            ]);

        output_table.add_row(vec![
            Cell::new("Color"),
            Cell::new(if self.config.output.color {
                "yes"
            } else {
                "no"
            }),
        ]);
        output_table.add_row(vec![
            Cell::new("Stream"),
            Cell::new(if self.config.output.stream {
                "yes"
            } else {
                "no"
            }),
        ]);
        output_table.add_row(vec![
            Cell::new("Show thoughts"),
            Cell::new(if self.config.output.show_thoughts {
                "yes"
            } else {
                "no"
            }),
        ]);
        output_table.add_row(vec![
            Cell::new("Show tool calls"),
            Cell::new(if self.config.output.show_tool_calls {
                "yes"
            } else {
                "no"
            }),
        ]);

        inline::print_line(&RLine::from(vec![
            RSpan::raw("  "),
            RSpan::styled("Output:", RStyle::default().add_modifier(RModifier::BOLD)),
        ]));
        println!("{output_table}");

        // MCP servers
        if !self.config.mcp_servers.is_empty() {
            let mut mcp_table = Table::new();
            mcp_table
                .load_preset(UTF8_FULL)
                .apply_modifier(UTF8_ROUND_CORNERS)
                .set_header(vec![
                    Cell::new("Name").fg(TColor::Cyan),
                    Cell::new("Command").fg(TColor::Cyan),
                ]);
            for (name, srv) in &self.config.mcp_servers {
                mcp_table.add_row(vec![
                    Cell::new(name),
                    Cell::new(srv.command.as_deref().unwrap_or("")),
                ]);
            }
            inline::print_line(&RLine::from(vec![
                RSpan::raw("  "),
                RSpan::styled(
                    "MCP Servers:",
                    RStyle::default().add_modifier(RModifier::BOLD),
                ),
            ]));
            println!("{mcp_table}");
        }

        Ok(())
    }

    // ── Config init (default / --interactive / --provider) ──

    fn config_init(&self, interactive: bool, provider_name: Option<&str>) -> Result<()> {
        let config_path = Config::config_path();

        if config_path.exists() {
            let overwrite = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt(format!(
                    "Config file already exists at {}. Overwrite?",
                    config_path.display()
                ))
                .default(false)
                .interact()
                .unwrap_or(false);
            if !overwrite {
                println!("Aborted.");
                return Ok(());
            }
        }

        let new_config = if interactive {
            self.config_init_wizard()?
        } else if let Some(name) = provider_name {
            self.config_init_provider(name)?
        } else {
            // Default: create fallback config
            print_info("Creating default config (OpenAI-compatible)...");
            Config::fallback()
        };

        // Ensure directory exists
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CommandError::Config(format!("Failed to create directory: {}", e)))?;
        }

        let content = serde_json::to_string_pretty(&new_config)
            .map_err(|e| CommandError::Config(e.to_string()))?;

        std::fs::write(&config_path, content)
            .map_err(|e| CommandError::Config(format!("Failed to write config: {}", e)))?;

        print_success(&format!(
            "Config file created at: {}",
            config_path.display()
        ));

        // Next steps guidance
        if new_config.providers.iter().any(|p| p.api_key.is_empty()) {
            println!();
            print_info("Next steps:");
            println!("  1. Set your API key:");
            println!("     takka config edit");
            println!("     # or set env var: OPENAI_API_KEY=sk-...");
            println!("  2. Verify: takka config validate");
            println!("  3. Try it: takka run \"list files\"");
        } else {
            println!();
            print_info("You're all set! Try: takka run \"hello world\"");
        }

        Ok(())
    }

    /// Full interactive wizard using dialoguer
    fn config_init_wizard(&self) -> Result<Config> {
        inline::print_blank();
        inline::print_line(&components::banner_title(
            "🤖 Takka Config Wizard",
            RColor::Cyan,
            RColor::Magenta,
        ));
        inline::print_blank();

        // Step 1: Choose provider
        let provider_names: Vec<&str> = PROVIDER_PRESETS.iter().map(|p| p.name).collect();
        let provider_idx = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Step 1: Choose your LLM provider")
            .items(&provider_names)
            .default(0)
            .interact()
            .map_err(|e| anyhow::anyhow!("Failed to read input: {}", e))?;

        let preset = &PROVIDER_PRESETS[provider_idx];

        // Step 2: API base URL
        let default_base = if preset.api_base.is_empty() {
            String::new()
        } else {
            preset.api_base.to_string()
        };
        let api_base: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Step 2: API base URL")
            .default(default_base)
            .interact()
            .map_err(|e| anyhow::anyhow!("Failed to read input: {}", e))?;

        // Step 3: API key
        let env_key = if provider_idx == 0 {
            "OPENAI_API_KEY"
        } else if provider_idx == 1 {
            "ANTHROPIC_API_KEY"
        } else {
            "API_KEY"
        };

        let env_value = std::env::var(env_key).unwrap_or_default();
        let api_key_default = if env_value.is_empty() {
            String::new()
        } else {
            env_value
        };

        let api_key: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt(format!("Step 3: API key (or env:${})", env_key))
            .default(api_key_default)
            .interact()
            .map_err(|e| anyhow::anyhow!("Failed to read input: {}", e))?;

        // Resolve if user typed env reference
        let resolved_key = api_key
            .strip_prefix('$')
            .and_then(|rest| std::env::var(rest).ok())
            .unwrap_or(api_key);

        // Step 4: Choose model
        let model = if preset.models.is_empty() {
            // Custom provider — ask for model name
            let model_name: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("Step 4: Model name (e.g. gpt-4o)")
                .interact()
                .map_err(|e| anyhow::anyhow!("Failed to read input: {}", e))?;
            takka_core::ModelConfig {
                model: model_name,
                display_name: None,
                temperature: 0.7,
                max_tokens: 8192,
                capabilities: None,
            }
        } else {
            let model_labels: Vec<String> = preset
                .models
                .iter()
                .map(|(id, name)| format!("{} ({})", name, id))
                .collect();

            let model_idx = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("Step 4: Choose default model")
                .items(&model_labels)
                .default(0)
                .interact()
                .map_err(|e| anyhow::anyhow!("Failed to read input: {}", e))?;

            takka_core::ModelConfig {
                model: preset.models[model_idx].0.to_string(),
                display_name: Some(preset.models[model_idx].1.to_string()),
                temperature: 0.7,
                max_tokens: 8192,
                capabilities: None,
            }
        };

        // Step 5: Safety options
        let auto_approve = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Step 5: Auto-approve low-risk tool calls?")
            .default(true)
            .interact()
            .unwrap_or(true);

        // Build the config
        let config = Config {
            providers: vec![takka_core::ProviderConfig {
                name: preset.name.to_string(),
                provider_type: preset.provider_type.to_string(),
                api_base,
                api_key: resolved_key.clone(),
                models: vec![model],
                cache: takka_core::CacheConfig::default(),
            }],
            safety: takka_core::SafetyConfig {
                auto_approve_low_risk: auto_approve,
                blocked_commands: vec![
                    "rm -rf /".to_string(),
                    "mkfs".to_string(),
                    "dd if=".to_string(),
                ],
                allowed_domains: vec![],
                block_ip_urls: false,
            },
            output: takka_core::OutputConfig {
                color: true,
                stream: true,
                show_thoughts: true,
                show_tool_calls: true,
            },
            mcp_servers: std::collections::HashMap::new(),
            system_prompt: None,
            agent: takka_core::AgentLoopConfig::default(),
            skills: takka_core::SkillsConfig::default(),
        };

        // Summary
        println!();
        print_success("Configuration summary:");
        println!("  Provider: {}", config.providers[0].name);
        println!("  API Base: {}", config.providers[0].api_base);
        println!("  Model:    {}", config.providers[0].models[0].model);
        println!(
            "  API Key:  {}",
            if resolved_key.is_empty() {
                "(empty)".to_string()
            } else {
                format!(
                    "{}...{}",
                    &resolved_key[..4.min(resolved_key.len())],
                    &resolved_key[resolved_key.len().saturating_sub(4)..]
                )
            }
        );

        Ok(config)
    }

    /// Quick provider preset setup
    fn config_init_provider(&self, name: &str) -> Result<Config> {
        let preset = PROVIDER_PRESETS
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| {
                let available: Vec<&str> = PROVIDER_PRESETS.iter().map(|p| p.name).collect();
                CommandError::not_found_with_suggestion(
                    format!("provider '{}'", name),
                    format!("available: {}", available.join(", ")),
                )
            })?;

        if preset.name == "custom" {
            // Redirect to interactive wizard for custom
            return self.config_init_wizard();
        }

        // Try to get API key from env
        let env_var = format!("{}_API_KEY", preset.name.to_uppercase());
        let api_key = std::env::var(&env_var)
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .unwrap_or_default();

        let default_model = preset
            .models
            .first()
            .map(|(id, display)| takka_core::ModelConfig {
                model: id.to_string(),
                display_name: Some(display.to_string()),
                temperature: 0.7,
                max_tokens: 8192,
                capabilities: None,
            })
            .unwrap_or(takka_core::ModelConfig {
                model: "gpt-4o".to_string(),
                display_name: Some("GPT-4o".to_string()),
                temperature: 0.7,
                max_tokens: 8192,
                capabilities: None,
            });

        print_info(&format!(
            "Creating config for {} ({})",
            preset.name, preset.api_base
        ));

        let config = Config {
            providers: vec![takka_core::ProviderConfig {
                name: preset.name.to_string(),
                provider_type: preset.provider_type.to_string(),
                api_base: preset.api_base.to_string(),
                api_key,
                models: vec![default_model],
                cache: takka_core::CacheConfig::default(),
            }],
            safety: takka_core::SafetyConfig::default(),
            output: takka_core::OutputConfig::default(),
            mcp_servers: std::collections::HashMap::new(),
            system_prompt: None,
            agent: takka_core::AgentLoopConfig::default(),
            skills: takka_core::SkillsConfig::default(),
        };

        Ok(config)
    }

    // ── Config edit ─────────────────────────────────────────

    fn config_edit(&self) -> Result<()> {
        let config_path = Config::config_path();

        if !config_path.exists() {
            return Err(anyhow::anyhow!(
                "Config file not found. Run 'takka config init' to create one."
            ));
        }

        let editor = std::env::var("EDITOR")
            .or_else(|_| std::env::var("VISUAL"))
            .unwrap_or_else(|_| {
                if cfg!(windows) {
                    "notepad".to_string()
                } else if cfg!(target_os = "macos") {
                    "open".to_string()
                } else {
                    "nano".to_string()
                }
            });

        print_info(&format!("Opening {}...", config_path.display()));

        let status = ProcessCommand::new(&editor)
            .arg(&config_path)
            .status()
            .map_err(|e| anyhow::anyhow!("Failed to open editor: {}", e))?;

        if status.success() {
            print_success("Config file updated.");
        } else {
            print_warning("Editor exited with non-zero status.");
        }

        Ok(())
    }

    // ── Config validate ─────────────────────────────────────

    fn config_validate(&self, verbose: bool) -> Result<()> {
        print_info("Validating configuration...");
        println!();

        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        // Check providers
        if self.config.providers.is_empty() {
            errors.push("No providers configured".to_string());
        }

        for (i, provider) in self.config.providers.iter().enumerate() {
            let label = if provider.name.is_empty() {
                format!("Provider #{}", i + 1)
            } else {
                format!("Provider '{}'", provider.name)
            };

            if verbose {
                println!("  Checking {}...", label);
            }

            if provider.name.is_empty() {
                errors.push(format!("{}: name is empty", label));
            }
            if provider.provider_type.is_empty() {
                errors.push(format!("{}: type is empty", label));
            }
            if provider.api_base.is_empty() {
                errors.push(format!("{}: API base URL is empty", label));
            }
            if provider.api_key.is_empty() {
                warnings.push(format!("{}: API key is empty", label));
            }
            if provider.models.is_empty() {
                warnings.push(format!("{}: No models configured", label));
            }

            if verbose {
                println!("    API Base: {}", provider.api_base);
                println!(
                    "    API Key:  {}",
                    if provider.api_key.is_empty() {
                        "(empty)".to_string()
                    } else {
                        format!(
                            "{}...{}",
                            &provider.api_key[..4.min(provider.api_key.len())],
                            &provider.api_key[provider.api_key.len().saturating_sub(4)..]
                        )
                    }
                );
                println!("    Models:   {}", provider.models.len());
                for m in &provider.models {
                    println!(
                        "      • {} (temp: {}, max_tokens: {})",
                        m.model, m.temperature, m.max_tokens
                    );
                }
            }
        }

        // Check safety
        if self.config.safety.blocked_commands.is_empty() {
            warnings.push(
                "No blocked commands configured. Consider adding dangerous commands.".to_string(),
            );
        }

        if verbose {
            println!("  Checking safety config...");
            println!(
                "    Auto-approve low risk: {}",
                self.config.safety.auto_approve_low_risk
            );
            println!(
                "    Blocked commands: {}",
                self.config.safety.blocked_commands.join(", ")
            );
        }

        // Print results
        println!();
        if errors.is_empty() && warnings.is_empty() {
            print_success("Configuration is valid!");
            print_info(&format!("Config file: {}", Config::config_path().display()));
        } else {
            for error in &errors {
                print_error(error, self.color_enabled);
            }
            for warning in &warnings {
                print_warning(warning);
            }

            if !errors.is_empty() {
                return Err(anyhow::anyhow!(
                    "Configuration validation failed with {} error(s)",
                    errors.len()
                ));
            }
            print_warning(&format!(
                "Configuration valid with {} warning(s)",
                warnings.len()
            ));
        }

        Ok(())
    }

    // ── Config reset ────────────────────────────────────────

    fn config_reset(&self, force: bool) -> Result<()> {
        let config_path = Config::config_path();

        if config_path.exists() {
            if !force {
                let confirm = Confirm::with_theme(&ColorfulTheme::default())
                    .with_prompt(format!(
                        "This will reset your config at {}. Continue?",
                        config_path.display()
                    ))
                    .default(false)
                    .interact()
                    .unwrap_or(false);
                if !confirm {
                    println!("Aborted.");
                    return Ok(());
                }
            }

            std::fs::remove_file(&config_path)
                .map_err(|e| CommandError::Config(format!("Failed to remove config: {}", e)))?;
        }

        let default_config = Config::fallback();

        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CommandError::Config(format!("Failed to create directory: {}", e)))?;
        }

        let content = serde_json::to_string_pretty(&default_config)
            .map_err(|e| CommandError::Config(e.to_string()))?;

        std::fs::write(&config_path, content)
            .map_err(|e| CommandError::Config(format!("Failed to write config: {}", e)))?;

        print_success(&format!(
            "Default config created at: {}",
            config_path.display()
        ));
        print_info("Remember to set your API key in the config file or via environment variables.");
        Ok(())
    }

    // ── Config path ─────────────────────────────────────────

    fn config_path(&self) -> Result<()> {
        println!("{}", Config::config_path().display());
        Ok(())
    }

    // ── Config backup ───────────────────────────────────────

    fn config_backup(&self) -> Result<()> {
        let config_path = Config::config_path();

        if !config_path.exists() {
            return Err(anyhow::anyhow!(
                "No config file found at: {}",
                config_path.display()
            ));
        }

        let backup_dir = config_path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("backups");

        std::fs::create_dir_all(&backup_dir)
            .map_err(|e| CommandError::Config(format!("Failed to create backup dir: {}", e)))?;

        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let backup_file = backup_dir.join(format!("config_{}.json", timestamp));

        std::fs::copy(&config_path, &backup_file)
            .map_err(|e| CommandError::Config(format!("Failed to create backup: {}", e)))?;

        print_success(&format!("Backup created at: {}", backup_file.display()));
        Ok(())
    }

    // ── Config restore ──────────────────────────────────────

    fn config_restore(&self, file: &str) -> Result<()> {
        let source = std::path::PathBuf::from(file);

        if !source.exists() {
            return Err(anyhow::anyhow!("Backup file not found: {}", file));
        }

        let config_path = Config::config_path();

        if config_path.exists() {
            let backup_dir = config_path
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .join("backups");
            std::fs::create_dir_all(&backup_dir).ok();
            let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
            let pre_restore = backup_dir.join(format!("pre_restore_{}.json", ts));
            std::fs::copy(&config_path, &pre_restore).ok();
            print_info(&format!(
                "Current config backed up to: {}",
                pre_restore.display()
            ));
        }

        std::fs::copy(&source, &config_path)
            .map_err(|e| CommandError::Config(format!("Failed to restore config: {}", e)))?;

        print_success(&format!("Config restored from: {}", file));
        Ok(())
    }

    // ── Config export ───────────────────────────────────────

    fn config_export(&self) -> Result<()> {
        let config_path = Config::config_path();

        if !config_path.exists() {
            return Err(anyhow::anyhow!(
                "No config file found. Run 'takka config init' first."
            ));
        }

        let content = std::fs::read_to_string(&config_path)
            .map_err(|e| CommandError::Config(format!("Failed to read config: {}", e)))?;

        let mut json: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| CommandError::Config(format!("Invalid JSON: {}", e)))?;

        if let Some(providers) = json.get_mut("providers").and_then(|p| p.as_array_mut()) {
            for provider in providers.iter_mut() {
                if let Some(api_key) = provider.get_mut("api_key") {
                    if let Some(key) = api_key.as_str() {
                        if key.len() > 8 {
                            *api_key = serde_json::Value::String(format!(
                                "{}...{}",
                                &key[..4],
                                &key[key.len() - 4..]
                            ));
                        } else {
                            *api_key = serde_json::Value::String("****".to_string());
                        }
                    }
                }
            }
        }

        let exported =
            serde_json::to_string_pretty(&json).map_err(|e| CommandError::Config(e.to_string()))?;

        println!("{}", exported);
        print_info("API keys have been masked for safe sharing.");
        Ok(())
    }

    // ── Config import ───────────────────────────────────────

    fn config_import(&self, file: &str) -> Result<()> {
        let source = std::path::PathBuf::from(file);

        if !source.exists() {
            return Err(anyhow::anyhow!("Import file not found: {}", file));
        }

        let content = std::fs::read_to_string(&source)
            .map_err(|e| CommandError::Config(format!("Failed to read import file: {}", e)))?;

        let _: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| CommandError::Config(format!("Invalid JSON in import file: {}", e)))?;

        let config_path = Config::config_path();

        if config_path.exists() {
            let backup_dir = config_path
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .join("backups");
            std::fs::create_dir_all(&backup_dir).ok();
            let ts = chrono::Local::now().format("%Y%m%d_%H%M%S");
            let pre_import = backup_dir.join(format!("pre_import_{}.json", ts));
            std::fs::copy(&config_path, &pre_import).ok();
            print_info(&format!(
                "Current config backed up to: {}",
                pre_import.display()
            ));
        }

        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| CommandError::Config(format!("Failed to create directory: {}", e)))?;
        }

        std::fs::write(&config_path, &content)
            .map_err(|e| CommandError::Config(format!("Failed to write config: {}", e)))?;

        print_success(&format!("Config imported from: {}", file));
        print_info("Run 'takka config validate' to verify the imported config.");
        Ok(())
    }
}

// ── Inline event rendering helper (used by run()'s ticker task) ───────────
//
// Free function so it can be called from inside the spawned tokio task
// without moving a closure (and therefore without capturing &self).

/// Render a two-line transient area:
///   Line 1: spinner progress
fn render_event(event: &takka_core::Event) {
    use crate::widgets::{components, diff as diff_widget, inline, tool_call};

    // Compact rendering by default: success notifications show only the
    // headline (tool name + numeric summary). Errors always include their
    // message body so users can debug without flipping a flag. The full
    // raw output is still in memory for the model.
    //
    // Special case: edit_file / write_file results carry a `diff` field
    // (unified-diff string) and `lines_added` / `lines_removed` counts.
    // We render those through `widgets::diff` so the user sees a real
    // colored diff inline instead of the raw JSON blob.
    const MAX_TOOL_OUTPUT_LINES: usize = 12;
    match event {
        takka_core::Event::ToolCall {
            tool_name,
            arguments,
        } => {
            inline::print_line(&tool_call::render_call_compact(tool_name, arguments));
        }
        takka_core::Event::ToolStarted {
            tool_name, arguments, ..
        } => {
            // Running header: "  ⟳ run_command" — live progress begins.
            let line = RLine::from(vec![RSpan::raw("  "), RSpan::styled(
                format!("\u{27F3} {}", tool_name),
                RStyle::default().fg(RColor::Indexed(247)).add_modifier(RModifier::BOLD),
            )]);
            inline::print_line(&line);
            let _ = arguments;
        }
        takka_core::Event::ToolDelta {
            delta, tool_name, ..
        } => {
            let lines = render_tool_delta(tool_name, delta);
            for line in lines {
                inline::print_line(&line);
            }
        }
        takka_core::Event::ToolOutput {
            tool_name,
            output,
            duration_ms,
            truncated,
            ..
        } => {
            // Heuristic: orchestrator records denied/skipped/error outcomes
            // as plain string output with these prefixes. We render those
            // with the red error accent.
            let body = match output {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let is_error = body.starts_with("Tool error")
                || body.starts_with("Blocked:")
                || body.starts_with("Skipped:");

            // Try to extract a unified diff from the embedded JSON. The
            // orchestrator wraps tool results as Value::String containing
            // pretty-printed JSON; we parse it back to look for our
            // structured fields. If we find a non-empty diff, render it
            // through the diff widget; the headline notification still
            // appears so the user gets the summary line.
            let parsed: Option<serde_json::Value> = if !is_error {
                serde_json::from_str(&body).ok()
            } else {
                None
            };

            let diff_text = parsed
                .as_ref()
                .and_then(|v| v.get("diff"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());

            // Compact result line: → ✓ summary  or  → ✗ error
            inline::print_line(&tool_call::render_result_compact(output, is_error));

            // Enriched metadata: duration + truncation notice.
            if *duration_ms > 0 {
                inline::print_line(&RLine::from(RSpan::styled(
                    format!("   ({:.1}s)", *duration_ms as f64 / 1000.0),
                    RStyle::default().add_modifier(RModifier::DIM),
                )));
            }
            if *truncated {
                inline::print_line(&RLine::from(RSpan::styled(
                    "   \u{2026} output dipotong (hasil penuh tersimpan di konteks model)",
                    RStyle::default().add_modifier(RModifier::DIM),
                )));
            }

            // If there's a diff, render it inline without borders.
            if let Some(diff) = diff_text {
                inline::print_line(&diff_widget::summary_line(diff));
                let diff_lines = diff_widget::render(diff);
                let max_diff_lines = 20;
                if diff_lines.len() > max_diff_lines {
                    inline::print_lines(&diff_lines[..max_diff_lines]);
                    let remaining = diff_lines.len() - max_diff_lines;
                    inline::print_line(&ratatui::text::Line::from(ratatui::text::Span::styled(
                        format!("    … {} more diff line(s) hidden", remaining),
                        ratatui::style::Style::default()
                            .add_modifier(ratatui::style::Modifier::DIM),
                    )));
                } else {
                    inline::print_lines(&diff_lines);
                }
            }
            let _ = tool_name;
        }
        takka_core::Event::Thinking { content }
            // Display the LLM's thinking/reasoning before tool execution
            // with DIM styling so it's visually distinct from the final response.
            if !content.is_empty() => {
                inline::print_line(&components::thinking_header(true));
                // Print thinking content with DIM style
                for line in content.lines() {
                    inline::print_line(&RLine::from(RSpan::styled(
                        format!("  {}", line),
                        RStyle::default()
                            .fg(RColor::Indexed(242))
                            .add_modifier(RModifier::DIM),
                    )));
                }
                inline::print_line(&components::thinking_header(false));
            }
        takka_core::Event::Error { message } => {
            inline::print_line(&components::error_badge(message));
        }
        takka_core::Event::System { message } => {
            inline::print_line(&components::info_badge(message));
        }
        // ConfirmationRequest / Completed are surfaced via other
        // channels (the confirmation prompt, the final markdown).
        _ => {}
    }
}

/// Render a live tool-output delta as indented DIM lines. Pure — no
/// terminal access — so it can be unit-tested without capturing stdout.
fn render_tool_delta(_tool_name: &str, delta: &str) -> Vec<RLine<'static>> {
    let mut lines = Vec::new();
    for line in delta.lines() {
        if line.trim().is_empty() {
            continue;
        }
        lines.push(RLine::from(RSpan::styled(
            format!("    {}", line),
            RStyle::default()
                .fg(RColor::Indexed(244))
                .add_modifier(RModifier::DIM),
        )));
    }
    lines
}

// ── Print helpers (shared widgets) ──────────────────────────
//  - CLI and TUI share one styling vocabulary
//  - Color/TTY decisions live in one place (`widgets::capabilities`)
fn print_success(text: &str) {
    inline::print_line(&components::success_badge(text));
}

fn print_warning(text: &str) {
    inline::print_line(&components::warning_badge(text));
}

fn print_error(text: &str, _color_enabled: bool) {
    // Errors go to stderr in plain styled text. We keep a simple format here
    // because `inline::print_line` writes to stdout; for stderr we render
    // directly with optional ANSI based on capabilities.
    if capabilities::should_use_color() {
        let mut stderr = StandardStream::stderr(termcolor::ColorChoice::Always);
        let _ = stderr.set_color(
            ColorSpec::new()
                .set_fg(Some(Color::White))
                .set_bg(Some(Color::Red)),
        );
        eprint!("  ✗ {} ", text);
        let _ = stderr.reset();
        eprintln!();
    } else {
        eprintln!("  ✗ {}", text);
    }
}

fn print_info(text: &str) {
    inline::print_line(&components::info_badge(text));
}

/// Extract up to `max_snippets` short windows around occurrences of
/// `query` in `content`. Each window is bounded by `max_chars`, breaks
/// on UTF-8 boundaries, and is prefixed/suffixed with an ellipsis when
/// truncated.
///
/// The match itself is preserved verbatim; we don't attempt to highlight
/// the matched substring inside the snippet (that would require a richer
/// span builder than the rest of the inline renderer expects).
fn extract_match_snippets(
    content: &str,
    query: &str,
    max_chars: usize,
    max_snippets: usize,
) -> Vec<String> {
    if query.is_empty() {
        return vec![truncate_one_line(content, max_chars)];
    }

    let lower_content = content.to_lowercase();
    let lower_query = query.to_lowercase();

    let mut out = Vec::with_capacity(max_snippets);
    let mut search_from = 0usize;
    let context = max_chars / 2;

    while out.len() < max_snippets {
        let Some(rel) = lower_content[search_from..].find(&lower_query) else {
            break;
        };
        let match_start = search_from + rel;
        let match_end = match_start + lower_query.len();

        // Window: pad `context` chars before/after the match, snapped to
        // UTF-8 boundaries.
        let start = floor_char_boundary(content, match_start.saturating_sub(context));
        let end = ceil_char_boundary(content, (match_end + context).min(content.len()));
        let mut snippet = String::new();
        if start > 0 {
            snippet.push('…');
        }
        snippet.push_str(content[start..end].replace('\n', " ").trim());
        if end < content.len() {
            snippet.push('…');
        }
        out.push(snippet);

        // Advance past this match so we don't loop on the same hit.
        search_from = match_end;
    }

    if out.is_empty() {
        out.push(truncate_one_line(content, max_chars));
    }
    out
}

fn truncate_one_line(s: &str, max_chars: usize) -> String {
    let one = s.replace('\n', " ");
    if one.chars().count() <= max_chars {
        return one;
    }
    let cut = floor_char_boundary(&one, max_chars);
    format!("{}…", &one[..cut])
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    let len = s.len();
    if i >= len {
        return len;
    }
    while i < len && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Walk up from `start` looking for `target` directory, creating it if not found.
/// If none exists up to the filesystem root, falls back to `start`.
fn find_or_create_dir(start: &std::path::Path, target: &str) -> std::path::PathBuf {
    let mut current: Option<&std::path::Path> = Some(start);
    while let Some(dir) = current {
        let candidate = dir.join(target);
        if candidate.is_dir() {
            return candidate;
        }
        current = dir.parent();
    }
    // Create in cwd
    let fallback = start.join(target);
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

#[cfg(test)]
mod search_snippet_tests {
    use super::*;

    #[test]
    fn extract_finds_substring_case_insensitive() {
        let s = "Hello WORLD, this is a test. The world is round.";
        let snips = extract_match_snippets(s, "world", 40, 5);
        assert_eq!(snips.len(), 2);
        assert!(snips[0].to_lowercase().contains("world"));
        assert!(snips[1].to_lowercase().contains("world"));
    }

    #[test]
    fn extract_respects_max_snippets() {
        let s = "foo ".repeat(50);
        let snips = extract_match_snippets(&s, "foo", 40, 3);
        assert_eq!(snips.len(), 3);
    }

    #[test]
    fn extract_falls_back_to_truncated_line_when_no_match() {
        let s = "some content with no needle";
        let snips = extract_match_snippets(s, "needle", 10, 2);
        // "needle" *is* in s; ensure positive case still works.
        assert_eq!(snips.len(), 1);
        assert!(snips[0].to_lowercase().contains("needle"));

        let snips = extract_match_snippets("unrelated text", "missing", 8, 2);
        assert_eq!(snips.len(), 1);
        // Truncated to ~8 chars + ellipsis.
        assert!(snips[0].chars().count() <= 9);
    }

    #[test]
    fn extract_handles_utf8_boundaries() {
        let s = "你好世界 你好世界";
        let snips = extract_match_snippets(s, "世界", 6, 5);
        assert!(!snips.is_empty());
        // Must not panic and must contain the query.
        for snip in &snips {
            assert!(snip.contains("世界"));
        }
    }
}

// ── QueueMode / follow-up queue unit tests ──────────────

#[cfg(test)]
mod queue_mode_tests {
    use super::*;

    #[test]
    fn queue_mode_default_is_follow_up() {
        assert_eq!(QueueMode::default(), QueueMode::FollowUp);
    }

    #[test]
    fn queue_mode_labels_match_cli_flag_values() {
        // `/queue <mode>` and `--queue-mode <MODE>` must agree on names.
        assert_eq!(QueueMode::FollowUp.as_str(), "follow-up");
        assert_eq!(QueueMode::Interrupt.as_str(), "interrupt");
    }

    #[test]
    fn set_queue_mode_switches_mode() {
        let mut commands = Commands::new(Config::fallback());
        assert_eq!(commands.queue_mode_getter(), QueueMode::FollowUp);
        commands.set_queue_mode(QueueMode::Interrupt);
        assert_eq!(commands.queue_mode_getter(), QueueMode::Interrupt);
        commands.set_queue_mode(QueueMode::FollowUp);
        assert_eq!(commands.queue_mode_getter(), QueueMode::FollowUp);
    }

    #[tokio::test]
    async fn submit_user_text_follow_up_queues_without_connection() {
        // FollowUp is a purely local queue: no engine connection needed.
        let mut commands = Commands::new(Config::fallback());
        commands
            .submit_user_text("queued".to_string())
            .await
            .expect("follow-up submit should always succeed");
        let queued: Vec<_> = commands.pending_follow_ups().iter().cloned().collect();
        assert_eq!(queued, vec!["queued".to_string()]);
    }

    #[tokio::test]
    async fn submit_user_text_follow_up_preserves_fifo_order() {
        let mut commands = Commands::new(Config::fallback());
        commands
            .submit_user_text("first".to_string())
            .await
            .unwrap();
        commands
            .submit_user_text("second".to_string())
            .await
            .unwrap();
        let queued: Vec<_> = commands.pending_follow_ups().iter().cloned().collect();
        assert_eq!(queued, vec!["first".to_string(), "second".to_string()]);
    }

    #[tokio::test]
    async fn submit_user_text_interrupt_requires_a_connection() {
        // Interrupt submits a steer to the engine — impossible unconnected.
        let mut commands = Commands::new(Config::fallback()).with_queue_mode(QueueMode::Interrupt);
        let err = commands
            .submit_user_text("hello".to_string())
            .await
            .expect_err("interrupt submit without connection must fail");
        assert!(err.to_string().contains("not connected"));
        // …and nothing was silently queued either.
        assert!(commands.pending_follow_ups().is_empty());
    }

    #[tokio::test]
    async fn drain_follow_ups_with_empty_queue_is_a_noop() {
        let mut commands = Commands::new(Config::fallback());
        commands
            .drain_follow_ups()
            .await
            .expect("empty drain should be a noop");
        assert_eq!(commands.test_runs.load(Ordering::Relaxed), 0);
    }
}

// ── End-to-end smoke tests ─────────────────────────────────
//
// These tests exercise the full `Commands::run` pipeline against a
// scripted mock provider so refactors in the CLI wiring are caught
// before they reach a release. The orchestrator integration tests in
// takka-core cover the loop itself; here we validate that the CLI
// layer (orchestrator init, tool registration, event plumbing, output
// rendering) works end to end.

#[cfg(test)]
mod e2e_tests {
    use super::*;
    use crate::client::RuntimeMode;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    /// Write a minimal config file pinning the spawned `takka-runtime`
    /// daemon to a provider endpoint that can never connect.
    ///
    /// The daemon loads its own config (`RuntimeEngine::initialize`), so a
    /// test must pin it explicitly — otherwise it reads the ambient
    /// `~/.config/agentic/config.json` and these tests would call a real
    /// LLM provider with the developer's credentials (and fail on CI, where
    /// no config exists). `http://127.0.0.1:9` (discard port) fails fast
    /// with a connection error and never leaves the machine, keeping the
    /// tests hermetic.
    fn hermetic_runtime_config() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "agentic-e2e-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(
            &path,
            r#"{
  "providers": [
    {
      "name": "hermetic-test",
      "type": "openai-compatible",
      "api_base": "http://127.0.0.1:9",
      "api_key": "unused",
      "models": [
        { "model": "hermetic-model", "temperature": 0.0, "max_tokens": 64 }
      ]
    }
  ]
}"#,
        )
        .expect("write hermetic runtime config");
        path
    }

    /// `run_with_callbacks` must stream the session lifecycle
    /// events through to the caller's callback, in BOTH connection modes.
    /// The embedded engine runs as a thread inside this process; the
    /// spawned mode launches the real `takka-runtime` subprocess over
    /// the wire protocol. The pinned unreachable provider makes the
    /// session fail deterministically in both cases.
    ///
    /// Tool-call / model-response semantics are covered by
    /// `takka-core/tests/runtime_engine.rs` (in-process engine tests).
    async fn smoke_lifecycle(mode: RuntimeMode) {
        let fixture = hermetic_runtime_config();

        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(mode)
            .with_permission_mode(takka_core::PermissionMode::Yolo);

        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            commands.run_with_callbacks(
                "list files",
                |_| {},
                move |evt| {
                    captured.lock().unwrap().push(format!("{evt:?}"));
                },
            ),
        )
        .await
        .expect("run_with_callbacks hung: no terminal event within 60s");
        let _ = std::fs::remove_file(&fixture);

        // The terminal value depends on how the engine ends a failed
        // session (Done or Error) — only the streamed lifecycle is asserted.
        let _ = result;
        let logged = events.lock().unwrap();
        let all: String = logged.join(" ");
        assert!(
            all.contains("SessionStarted"),
            "Expected SessionStarted lifecycle event, got: {all}"
        );
        assert!(
            all.contains("SessionFailed"),
            "Expected SessionFailed after provider failure, got: {all}"
        );
    }

    #[tokio::test]
    async fn smoke_lifecycle_embedded() {
        smoke_lifecycle(RuntimeMode::Embedded).await;
    }

    #[tokio::test]
    async fn smoke_lifecycle_spawned() {
        smoke_lifecycle(RuntimeMode::Spawn).await;
    }

    /// Regression test for the frozen-"Thinking…" deadlock: `Commands::run`
    /// (the inline renderer-thread path) used to leave `chunk_tx` alive in
    /// its own stack frame while blocking on `renderer.join()`. The renderer
    /// waits for `Disconnected` on the chunk channel, which only happens when
    /// *every* `Sender` is dropped — so it never exited and the spinner
    /// stayed on screen forever.
    ///
    /// Driven through both connection modes with an unreachable provider so
    /// the renderer must still join cleanly on the session-failure path.
    /// Multi-thread runtime + timeout: without the fix this test fails with
    /// a timeout instead of hanging the whole test binary.
    async fn smoke_no_deadlock(mode: RuntimeMode) {
        let fixture = hermetic_runtime_config();

        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(mode)
            .with_permission_mode(takka_core::PermissionMode::Yolo);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            commands.run("who are you?"),
        )
        .await
        .expect("Commands::run deadlocked: renderer.join() never returned");
        let _ = std::fs::remove_file(&fixture);

        // `run` renders session failures as an error badge and returns Ok —
        // the regression here is returning at all, not the outcome.
        assert!(result.is_ok(), "run should complete: {:?}", result.err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_no_deadlock_embedded() {
        smoke_no_deadlock(RuntimeMode::Embedded).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_no_deadlock_spawned() {
        smoke_no_deadlock(RuntimeMode::Spawn).await;
    }

    /// Follow-up queue flow (both transports), per the steer-queue Task 3
    /// brief: first run against the hermetic unreachable provider, then
    /// `submit_user_text` in FollowUp mode queues locally, and
    /// `drain_follow_ups` issues the queued text as a NEW client-issued
    /// run — its SessionStarted arrives again, and total client-issued
    /// Runs == queue items + 1.
    ///
    /// The drained run travels through `Commands::run` (the renderer
    /// path), so its lifecycle is observed through the cfg(test) event tap
    /// rather than the caller callback used for the first run.
    async fn smoke_follow_up_queue(mode: RuntimeMode) {
        let fixture = hermetic_runtime_config();

        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(mode)
            .with_permission_mode(takka_core::PermissionMode::Yolo);
        assert_eq!(commands.queue_mode_getter(), QueueMode::FollowUp);

        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();

        let first = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            commands.run_with_callbacks(
                "first run",
                |_| {},
                move |evt| {
                    captured.lock().unwrap().push(format!("{evt:?}"));
                },
            ),
        )
        .await
        .expect("first run hung: no terminal event within 60s");
        // The provider is unreachable; the session fails deterministically.
        // The outcome value itself is not the assertion here.
        let _ = first;

        // FollowUp mode: text submitted once the run is over queues
        // locally instead of steering.
        commands
            .submit_user_text("second run".to_string())
            .await
            .expect("submit_user_text should queue in FollowUp mode");
        assert_eq!(commands.pending_follow_ups().len(), 1);

        // Drain after Done: the queued text is sent as the next run.
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            commands.drain_follow_ups(),
        )
        .await
        .expect("drain hung")
        .expect("drain failed");

        let _ = std::fs::remove_file(&fixture);

        // Client-issued guarantee: exactly one Run per queue item, on top
        // of the first one.
        assert_eq!(
            commands.test_runs.load(Ordering::Relaxed),
            2,
            "expected first run + 1 drained follow-up run"
        );

        // The drained run started its own session (SessionStarted again).
        let drained: String = commands.test_events.lock().unwrap().join(" ");
        assert!(
            drained.contains("SessionStarted"),
            "drained follow-up run should emit SessionStarted, got: {drained}"
        );

        // …and the first run streamed its lifecycle to the caller callback.
        let all: String = events.lock().unwrap().join(" ");
        assert!(
            all.contains("SessionStarted"),
            "Expected SessionStarted on first run, got: {all}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_follow_up_queue_embedded() {
        smoke_follow_up_queue(RuntimeMode::Embedded).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_follow_up_queue_spawned() {
        smoke_follow_up_queue(RuntimeMode::Spawn).await;
    }

    /// Interrupt mode (both transports): text submitted outside an active
    /// run (the only case this task wires) is sent fire-and-forget as a
    /// steer — never queued locally. The engine's strict gating answers
    /// `Error { "no active run" }` on the event stream (Task 2); the stray
    /// reply is skipped by the next run's request-id filter.
    async fn smoke_interrupt_mode_does_not_queue(mode: RuntimeMode) {
        let fixture = hermetic_runtime_config();

        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(mode)
            .with_permission_mode(takka_core::PermissionMode::Yolo)
            .with_queue_mode(QueueMode::Interrupt);

        // Establish the connection with a first run (hermetic fixture).
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            commands.run_with_callbacks(
                "first run",
                |_| {},
                move |evt| {
                    captured.lock().unwrap().push(format!("{evt:?}"));
                },
            ),
        )
        .await
        .expect("first run hung");

        // Interrupt mode: fire-and-forget steer, local queue untouched.
        commands
            .submit_user_text("steer with no active run".to_string())
            .await
            .expect("submit_user_text should send the steer");
        assert!(
            commands.pending_follow_ups().is_empty(),
            "interrupt mode must not queue locally"
        );

        let _ = std::fs::remove_file(&fixture);
    }

    #[tokio::test]
    async fn smoke_interrupt_mode_does_not_queue_embedded() {
        smoke_interrupt_mode_does_not_queue(RuntimeMode::Embedded).await;
    }

    #[tokio::test]
    async fn smoke_interrupt_mode_does_not_queue_spawned() {
        smoke_interrupt_mode_does_not_queue(RuntimeMode::Spawn).await;
    }
}

// ── Daemon-mode e2e smoke (Fase 3a, unix only) ───────────
//
// The daemon mode of the `--runtime` matrix gets dedicated
// AgentConnection-level smokes instead of extending the Commands-level
// mode matrix: the broker's discovery file lives at a process-default
// path, and a Commands-driven smoke would either touch the developer's
// real ~/.config/agentic or need a process-global HOME override (racy
// under parallel cargo test). `AgentConnection::connect_daemon_at` is
// the same discovery + spawn + token-handshake path the mode flag and
// `attach` use, pointed at a tempdir fixture.
#[cfg(all(test, unix))]
mod daemon_smoke_tests {
    use super::*;
    use crate::client::{AgentConnection, CLI_CAPABILITIES};
    use takka_core::runtime::protocol::InitOverrides;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    /// Bound for every poll so a regression can never hang the suite.
    const POLL_TIMEOUT: Duration = Duration::from_secs(10);

    /// Hermetic provider config (same shape as `hermetic_runtime_config`):
    /// `127.0.0.1:9` fails fast and never leaves the machine.
    const HERMETIC_CONFIG: &str = r#"{
  "providers": [
    {
      "name": "hermetic-test",
      "type": "openai-compatible",
      "api_base": "http://127.0.0.1:9",
      "api_key": "unused",
      "models": [
        { "model": "hermetic-model", "temperature": 0.0, "max_tokens": 64 }
      ]
    }
  ]
}"#;

    struct DaemonFixture {
        dir: PathBuf,
        info_path: PathBuf,
        socket_path: PathBuf,
        config_path: PathBuf,
        child: tokio::process::Child,
    }

    impl DaemonFixture {
        /// Spawn the real `takka-runtime --daemon` binary with explicit
        /// endpoints under a fresh tempdir. The info file is written after
        /// bind, so its existence means the listener is ready.
        async fn spawn(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "agentic-cli-daemon-{}-{}-{}",
                std::process::id(),
                nanos,
                tag
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let config_path = dir.join("config.json");
            std::fs::write(&config_path, HERMETIC_CONFIG).unwrap();
            let socket_path = dir.join("test.sock");
            let info_path = dir.join("daemon.json");
            let binary = crate::client::runtime_binary().expect("runtime binary for daemon smoke");
            let child = tokio::process::Command::new(&binary)
                .arg("--daemon")
                .arg("--socket-path")
                .arg(&socket_path)
                .arg("--info-path")
                .arg(&info_path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true) // a panicking test must not leak the daemon
                .spawn()
                .expect("spawn takka-runtime --daemon");
            let deadline = Instant::now() + POLL_TIMEOUT;
            while !info_path.exists() {
                assert!(
                    Instant::now() < deadline,
                    "daemon info file did not appear within 10s"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Self {
                dir,
                info_path,
                socket_path,
                config_path,
                child,
            }
        }

        /// Init overrides pinning the spawned engine to the fixture's
        /// hermetic provider (the engine reads config from the Init
        /// frame, not from the ambient ~/.config).
        fn init_overrides(&self) -> InitOverrides {
            InitOverrides {
                config_path: Some(self.config_path.to_string_lossy().into_owned()),
                permission_mode: Some(takka_core::PermissionMode::Yolo),
                model: None,
                system_prompt: None,
            }
        }

        async fn connect(&self) -> AgentConnection {
            AgentConnection::connect_daemon_at(
                &self.info_path,
                self.init_overrides(),
                CLI_CAPABILITIES,
                false,
            )
            .await
            .expect("daemon attach failed")
        }

        /// Wait until the broker exited cleanly and unlinked its
        /// endpoints (serve's shutdown cleanup).
        async fn wait_clean_exit(&mut self) {
            let status = tokio::time::timeout(POLL_TIMEOUT, self.child.wait())
                .await
                .expect("daemon did not exit within 10s after shutdown")
                .expect("daemon wait failed");
            assert_eq!(status.code(), Some(0), "daemon exited with {status}");
            let deadline = Instant::now() + POLL_TIMEOUT;
            while self.socket_path.exists() || self.info_path.exists() {
                assert!(
                    Instant::now() < deadline,
                    "daemon endpoints were not unlinked within 10s"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for DaemonFixture {
        fn drop(&mut self) {
            // kill_on_drop(true) reaps the daemon (no-op after a clean
            // exit); files are best-effort so a panicking test never
            // leaks a tempdir.
            let _ = std::fs::remove_file(&self.socket_path);
            let _ = std::fs::remove_file(&self.info_path);
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Full lifecycle: spawn daemon → discovery + token handshake →
    /// run (frontend-issued) → SessionStarted + terminal event →
    /// shutdown stops the broker and unlinks socket + info files.
    #[tokio::test(flavor = "multi_thread")]
    async fn daemon_lifecycle_smoke() {
        let mut fx = DaemonFixture::spawn("lifecycle").await;
        let mut conn = fx.connect().await;

        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let result = tokio::time::timeout(
            Duration::from_secs(60),
            conn.run(
                "list files".to_string(),
                Vec::new(),
                |_| {},
                move |evt| captured.lock().unwrap().push(format!("{evt:?}")),
                |_| None,
            ),
        )
        .await
        .expect("daemon run hung: no terminal event within 60s");

        // The pinned unreachable provider makes the session fail
        // deterministically; only the streamed lifecycle is asserted
        // (terminal value is Done-or-Error by design).
        let _ = result;
        let all = events.lock().unwrap().join(" ");
        assert!(
            all.contains("SessionStarted"),
            "expected SessionStarted over the daemon socket, got: {all}"
        );
        assert!(
            all.contains("SessionFailed"),
            "expected SessionFailed after provider failure, got: {all}"
        );

        conn.shutdown().await.expect("daemon shutdown");
        fx.wait_clean_exit().await;
    }

    /// While connection A is active, a second attach is refused with the
    /// exact `"daemon in use"` error (surfaced through the shared
    /// handshake), and A keeps working.
    #[tokio::test(flavor = "multi_thread")]
    async fn in_use_refusal_smoke() {
        let mut fx = DaemonFixture::spawn("refusal").await;
        let mut a = fx.connect().await;

        let refused = AgentConnection::connect_daemon_at(
            &fx.info_path,
            fx.init_overrides(),
            CLI_CAPABILITIES,
            false,
        )
        .await;
        match refused {
            Err(error) => {
                assert!(
                    error.to_string().contains("daemon in use"),
                    "expected daemon-in-use refusal, got: {error}"
                );
            }
            Ok(_) => panic!("second attach must be refused while A is active"),
        }

        // A is unaffected by B's refused attach.
        let event = a
            .send_and_wait(takka_core::runtime::protocol::Request::ListTools)
            .await
            .expect("ListTools on the active connection");
        assert!(
            matches!(event.event, takka_core::Event::ToolList { .. }),
            "expected ToolList on A, got {:?}",
            event.event
        );

        a.shutdown().await.expect("daemon shutdown");
        fx.wait_clean_exit().await;
    }

    /// A attaches, detaches; B reattaches and the engine adopts the live
    /// session: handshake InitOk followed by SessionAdopted.
    #[tokio::test(flavor = "multi_thread")]
    async fn adopt_smoke() {
        let mut fx = DaemonFixture::spawn("adopt").await;
        let a = fx.connect().await;
        drop(a); // detach — the broker's engine keeps running

        // If A's reader thread has not noticed the EOF yet, the broker
        // still holds the slot and refuses B — poll until adoption.
        let deadline = Instant::now() + POLL_TIMEOUT;
        let mut b = loop {
            match AgentConnection::connect_daemon_at(
                &fx.info_path,
                fx.init_overrides(),
                CLI_CAPABILITIES,
                false,
            )
            .await
            {
                Err(error) if error.to_string().contains("daemon in use") => {
                    assert!(Instant::now() < deadline, "reattach window expired");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                other => break other.expect("reattach failed"),
            }
        };

        // Handshake consumed InitOk (cli-1); the adoption replay is the
        // very next frame, tagged with the same request id.
        let event = tokio::time::timeout(POLL_TIMEOUT, b.next_event())
            .await
            .expect("timed out waiting for SessionAdopted")
            .expect("event stream closed after reattach");
        assert_eq!(event.request_id.as_deref(), Some("cli-1"));
        assert!(
            matches!(event.event, takka_core::Event::SessionAdopted { .. }),
            "expected SessionAdopted, got {:?}",
            event.event
        );

        b.shutdown().await.expect("daemon shutdown");
        fx.wait_clean_exit().await;
    }

    /// Auto-spawn: with no info file at all, connect_daemon_at launches
    /// the runtime binary itself, discovers it, handshakes, and a
    /// shutdown tears the daemon down (endpoints unlinked).
    #[tokio::test(flavor = "multi_thread")]
    async fn autospawn_smoke() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "agentic-cli-autospawn-{}-{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.json");
        std::fs::write(&config_path, HERMETIC_CONFIG).unwrap();
        let info_path = dir.join("daemon.json");

        let mut conn = AgentConnection::connect_daemon_at(
            &info_path,
            InitOverrides {
                config_path: Some(config_path.to_string_lossy().into_owned()),
                permission_mode: Some(takka_core::PermissionMode::Yolo),
                model: None,
                system_prompt: None,
            },
            CLI_CAPABILITIES,
            true,
        )
        .await
        .expect("auto-spawn + connect failed");

        // Sanity: the spawned broker's engine answers on the wire.
        let event = tokio::time::timeout(
            Duration::from_secs(30),
            conn.send_and_wait(takka_core::runtime::protocol::Request::ListTools),
        )
        .await
        .expect("ListTools timed out")
        .expect("ListTools failed");
        assert!(matches!(event.event, takka_core::Event::ToolList { .. }));

        conn.shutdown().await.expect("daemon shutdown");
        // The spawned daemon is detached (not our child); its clean-exit
        // signal is the endpoint cleanup.
        let deadline = Instant::now() + POLL_TIMEOUT;
        while info_path.exists() {
            assert!(
                Instant::now() < deadline,
                "auto-spawned daemon did not exit within 10s after shutdown"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Attach without a running daemon (`spawn_if_missing=false`) fails
    /// with the exact actionable message.
    #[tokio::test]
    async fn attach_without_daemon_is_actionable() {
        let dir = std::env::temp_dir().join(format!(
            "agentic-cli-nodaemon-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let info_path = dir.join("daemon.json");

        let error = match AgentConnection::connect_daemon_at(
            &info_path,
            InitOverrides::default(),
            CLI_CAPABILITIES,
            false,
        )
        .await
        {
            Ok(_) => panic!("attach without daemon must fail"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), crate::client::NO_DAEMON_MESSAGE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Commands-level composition of `takka attach --shutdown` against
    /// a fixture daemon: connect (no auto-spawn) → fire-and-forget
    /// shutdown → the broker exits and unlinks its endpoints.
    #[tokio::test(flavor = "multi_thread")]
    async fn commands_attach_shutdown_stops_daemon() {
        let mut fx = DaemonFixture::spawn("attach-shutdown").await;
        let mut commands = Commands::new(Config::fallback())
            .with_daemon_info_path(fx.info_path.clone())
            .with_runtime_config_path(Some(fx.config_path.to_string_lossy().into_owned()))
            .with_permission_mode(takka_core::PermissionMode::Yolo);

        tokio::time::timeout(POLL_TIMEOUT, commands.attach_daemon(true))
            .await
            .expect("attach --shutdown hung")
            .expect("attach --shutdown failed");
        // Fire-and-forget: wait for the broker to exit and clean up,
        // then attach must fail with the exact actionable message.
        fx.wait_clean_exit().await;
        let error = tokio::time::timeout(POLL_TIMEOUT, commands.attach_daemon(false))
            .await
            .expect("attach hung")
            .expect_err("attach after shutdown must fail");
        assert_eq!(error.to_string(), crate::client::NO_DAEMON_MESSAGE);
    }

    /// Shutdown delivery survives the slot race: A detaches and the
    /// shutdown fires immediately — if the broker's reader has not
    /// processed A's EOF yet, the request is refused ("daemon in use")
    /// and must be retried, not silently lost.
    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_retries_through_busy_slot() {
        let mut fx = DaemonFixture::spawn("shutdown-retry").await;
        let a = fx.connect().await;
        drop(a); // EOF races our shutdown attempt below

        let mut commands =
            Commands::new(Config::fallback()).with_daemon_info_path(fx.info_path.clone());
        tokio::time::timeout(POLL_TIMEOUT, commands.attach_daemon(true))
            .await
            .expect("attach --shutdown hung")
            .expect("shutdown through busy slot failed");
        fx.wait_clean_exit().await;
    }

    /// A shutdown while another client HOLDS the connection is refused
    /// until the retry deadline, then surfaces a busy error (the active
    /// client is never touched).
    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_busy_daemon_errors_after_deadline() {
        let mut fx = DaemonFixture::spawn("shutdown-busy").await;
        let mut a = fx.connect().await; // stays attached

        let mut commands =
            Commands::new(Config::fallback()).with_daemon_info_path(fx.info_path.clone());
        let error = tokio::time::timeout(POLL_TIMEOUT, commands.attach_daemon(true))
            .await
            .expect("attach --shutdown hung")
            .expect_err("shutdown of a busy daemon must fail after the deadline");
        assert!(
            error.to_string().contains("daemon busy"),
            "expected busy error, got: {error}"
        );

        // The active client is unaffected by the refused shutdown.
        let event = a
            .send_and_wait(takka_core::runtime::protocol::Request::ListTools)
            .await
            .expect("ListTools on the active connection");
        assert!(matches!(event.event, takka_core::Event::ToolList { .. }));

        a.shutdown().await.expect("daemon shutdown");
        fx.wait_clean_exit().await;
    }

    /// Commands-level no-daemon path: attach without a running daemon
    /// surfaces the actionable message (never spawns).
    #[tokio::test]
    async fn commands_attach_without_daemon_errors_actionable() {
        let dir = std::env::temp_dir().join(format!(
            "agentic-cli-cmd-nodaemon-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let info_path = dir.join("daemon.json");

        let mut commands = Commands::new(Config::fallback()).with_daemon_info_path(info_path);
        let error = tokio::time::timeout(POLL_TIMEOUT, commands.attach_daemon(false))
            .await
            .expect("attach hung")
            .expect_err("attach without daemon must fail");
        assert_eq!(error.to_string(), crate::client::NO_DAEMON_MESSAGE);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ── switch_model disambiguation ────────────────────────────

#[cfg(test)]
mod switch_model_tests {
    use super::*;
    use takka_core::config::{ModelConfig, ProviderConfig};

    /// Config with the same display name "GLM-4.7" (and id "glm-4.7") in
    /// two providers, plus an aliased model (display "kimi-k2.6", id
    /// "sm/deepseek-v4-flash") duplicated across providers — the exact
    /// situations that made a plain contains() switch pick the wrong one.
    fn ambiguous_config() -> Config {
        let model = |id: &str, display: Option<&str>| ModelConfig {
            model: id.into(),
            display_name: display.map(str::to_string),
            temperature: 0.7,
            max_tokens: 8192,
            capabilities: None,
        };
        let provider = |name: &str, models: Vec<ModelConfig>| ProviderConfig {
            name: name.into(),
            provider_type: "openai".into(),
            api_base: format!("https://{}.test/v1", name.to_lowercase()),
            api_key: String::new(),
            models,
            cache: Default::default(),
        };

        Config {
            providers: vec![
                provider(
                    "Provider A",
                    vec![
                        model("glm-4.7", Some("GLM-4.7")),
                        model("sm/deepseek-v4-flash", Some("kimi-k2.6")),
                    ],
                ),
                provider(
                    "Provider B",
                    vec![
                        model("glm-4.7", Some("GLM-4.7")),
                        model("glm-5", Some("GLM-5")),
                        model("sm/deepseek-v4-flash", Some("sm/deepseek-v4-flash")),
                    ],
                ),
            ],
            safety: Default::default(),
            output: Default::default(),
            mcp_servers: Default::default(),
            system_prompt: None,
            agent: Default::default(),
            skills: Default::default(),
        }
    }

    /// Switch via the pure lookup + reorder helpers. These mirror exactly
    /// what `switch_model` does, minus the config save (which writes to
    /// the real user config path and is deliberately not exercised here).
    fn switch(config: &mut Config, name: &str) -> Result<(String, String), String> {
        let (pi, mi) = Commands::find_model_index(config, name)
            .ok_or_else(|| format!("Model '{name}' not found"))?;
        Ok(Commands::reorder_config(config, pi, mi))
    }

    #[test]
    fn unqualified_switch_hits_first_provider() {
        let mut config = ambiguous_config();
        let (provider, model) = switch(&mut config, "GLM-4.7").unwrap();
        assert_eq!(provider, "Provider A");
        assert_eq!(model, "GLM-4.7");
    }

    #[test]
    fn qualified_switch_disambiguates_duplicate_name() {
        let mut config = ambiguous_config();
        // Same id/display in two providers: the qualified form must pick
        // Provider B even though Provider A comes first in config order.
        let (provider, model) = switch(&mut config, "Provider B/glm-4.7").unwrap();
        assert_eq!(provider, "Provider B");
        assert_eq!(model, "GLM-4.7");
    }

    #[test]
    fn qualified_switch_disambiguates_aliased_id() {
        let mut config = ambiguous_config();
        // "sm/deepseek-v4-flash" exists in both providers; the qualified
        // form must hit Provider B's copy, not Provider A's (aliased as
        // "kimi-k2.6").
        let (provider, model) = switch(&mut config, "Provider B/sm/deepseek-v4-flash").unwrap();
        assert_eq!(provider, "Provider B");
        assert_eq!(model, "sm/deepseek-v4-flash");
    }

    #[test]
    fn qualified_switch_case_insensitive_provider() {
        let mut config = ambiguous_config();
        let (provider, _) = switch(&mut config, "provider b/glm-5").unwrap();
        assert_eq!(provider, "Provider B");
    }

    #[test]
    fn qualified_switch_after_unqualified_reorders_correctly() {
        let mut config = ambiguous_config();
        // Unqualified reorders Provider A to the front...
        let (provider, _) = switch(&mut config, "GLM-4.7").unwrap();
        assert_eq!(provider, "Provider A");

        // ...then a qualified switch still lands on the exact provider.
        let (provider, model) = switch(&mut config, "Provider B/glm-5").unwrap();
        assert_eq!(provider, "Provider B");
        assert_eq!(model, "GLM-5");
    }

    #[test]
    fn qualified_switch_unknown_provider_errors() {
        let mut config = ambiguous_config();
        // "nope/glm-4.7" has no such provider → exact match fails, and the
        // loose fallback searches the whole string which matches nothing.
        let err = switch(&mut config, "nope/glm-4.7").unwrap_err();
        assert!(err.contains("not found"), "unexpected error: {err}");
    }

    #[test]
    fn switch_model_never_persists_in_tests() {
        // Regression guard: switch_model must refuse to save config under
        // test builds — save() targets the real user config path and a test
        // once overwrote a developer's actual ~/.config/agentic/config.json.
        let mut commands = Commands::new(ambiguous_config());
        let err = commands.switch_model("GLM-4.7").unwrap_err();
        assert!(
            err.contains("refusing to save config under test"),
            "expected test-save refusal, got: {err}"
        );
    }
}

#[cfg(test)]
mod render_tool_delta_tests {
    use super::*;

    #[test]
    fn skips_blank_lines() {
        let lines = render_tool_delta("run_command", "a\n\nb\n");
        let rendered: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert_eq!(rendered.len(), 2, "blank lines should be skipped");
        assert!(rendered[0].contains("a"));
        assert!(rendered[1].contains("b"));
    }

    #[test]
    fn indents_every_line() {
        let lines = render_tool_delta("run_command", "cargo: compiling\ncargo: done");
        assert_eq!(lines.len(), 2);
        for l in &lines {
            assert!(l.to_string().contains("    "), "expected 4-space indent");
        }
    }

    #[test]
    fn empty_delta_produces_no_lines() {
        let lines = render_tool_delta("run_command", "");
        assert!(lines.is_empty());
    }
}
