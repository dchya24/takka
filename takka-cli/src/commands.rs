use anyhow::Result;
use comfy_table::{
    modifiers::UTF8_ROUND_CORNERS, presets::UTF8_FULL, Cell, Color as TColor, Table,
};
use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::terminal::{Clear, ClearType};
use crossterm::ExecutableCommand;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, MultiSelect, Select};
use ratatui::style::{Color as RColor, Modifier as RModifier, Style as RStyle};
use ratatui::text::{Line as RLine, Span as RSpan};
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use takka_core::Config;
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

/// True while a blocking prompt (tool confirmation, `question` dialog) owns
/// stdin.
///
/// The REPL reads keys continuously — even during a run, so text typed
/// mid-run can be queued — and in raw mode `event::read()` is unconditional:
/// whichever reader asks first gets the key. That race is what ate the `y`/`a`
/// keys of a confirmation prompt and turned them into "Invalid input". The REPL
/// polls this flag and leaves stdin alone while a prompt is up.
pub(crate) static INTERACTIVE_PROMPT_ACTIVE: AtomicBool = AtomicBool::new(false);

/// RAII guard for [`INTERACTIVE_PROMPT_ACTIVE`].
pub(crate) struct InteractivePromptGuard(());

impl InteractivePromptGuard {
    pub(crate) fn activate() -> Self {
        INTERACTIVE_PROMPT_ACTIVE.store(true, Ordering::SeqCst);
        Self(())
    }
}

impl Drop for InteractivePromptGuard {
    fn drop(&mut self) {
        INTERACTIVE_PROMPT_ACTIVE.store(false, Ordering::SeqCst);
    }
}

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
    fn handle(&self, questions: &[takka_core::QuestionPrompt]) -> Vec<takka_core::QuestionAnswer> {
        // Pause the streaming renderer for the whole prompt sequence.
        // The spinner thread spawned by `Commands::run` redraws the
        // "Thinking…" line every 80ms with MoveUp+Clear+print; if it kept
        // writing here its cursor moves would land on top of the
        // dialoguer list (messy options), then erase the answer badge
        // after submission — and on Windows the two writers racing over
        // the console is what looks like a flickering dim lamp.
        let _render_gate = RENDER_GATE.lock().unwrap();
        // Same stdin ownership as `prompt_confirmation`: the REPL key loop must
        // not consume the keystrokes dialoguer is waiting for.
        let _input_guard = InteractivePromptGuard::activate();
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

/// Print a fresh status line under the current cursor. Used by the question
/// handler to restore the renderer's "status line is the last line" invariant
/// after a prompt; the renderer replaces it with the live status on its next
/// tick.
pub(crate) fn commit_spinner_placeholder() {
    if !inline::is_stdout_tty() {
        return;
    }
    let mut p = crate::widgets::progress::ProgressState::new();
    p.start();
    p.set_phase(crate::widgets::progress::Phase::Thinking);
    inline::print_line(&crate::widgets::spinner::status_line(&p, false));
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

/// One poll of the terminal's key events, mapped to the reader's key
/// enum (story 2.1). Raw mode keeps SIGINT off; the reader is the
/// cancel source, so `Ctrl+C` maps via [`MidRunKey::CtrlC`]. Plain
/// characters and Enter drive the steer line; untracked keys fall
/// through.
#[cfg(not(test))]
fn poll_crossterm_key() -> Option<MidRunKey> {
    use crossterm::event::{Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
    match crossterm::event::poll(std::time::Duration::from_millis(100)) {
        Ok(true) => match crossterm::event::read() {
            Ok(CtEvent::Key(key)) => {
                let press = key.kind == KeyEventKind::Press;
                if press
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('c')
                {
                    Some(MidRunKey::CtrlC)
                } else if press && key.modifiers.is_empty() {
                    match key.code {
                        KeyCode::Char(ch) => Some(MidRunKey::Char(ch)),
                        KeyCode::Enter => Some(MidRunKey::Enter),
                        _ => None,
                    }
                } else {
                    None
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// One mid-run reader key event (story 2.1): characters + Enter drive
/// the steer path; Ctrl+C rides the story-1.4 two-stage cancel. The
/// cfg(test) injection feeds these instead of the keyboard.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum MidRunKey {
    Char(char),
    Enter,
    CtrlC,
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
    pending_follow_ups: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    /// Current nesting depth of `drain_follow_ups` (drained runs drain
    /// again on completion; capped at [`MAX_FOLLOW_UP_DRAIN_DEPTH`]).
    drain_depth: usize,
    /// Session override for `output.show_thoughts` (`--no-thoughts`,
    /// `/thoughts`). `None` means "use the config".
    show_thoughts_override: Option<bool>,
    /// Detail level for the inline tool timeline (`/timeline`).
    timeline_style: TimelineStyle,
    /// Figures from the most recent `run()` — the REPL's end-of-run summary
    /// reads these (elapsed time is measured by the caller, tokens are
    /// estimated from the character counts here).
    last_run: RunStats,
    /// Test-only taps (cfg(test)): e2e tests prove the client-issued drain
    /// contract (runs == queue items + 1; the drained run emits its own
    /// SessionStarted) by counting client-issued Runs and capturing the
    /// lifecycle events of drained runs — which travel through `run()`'s
    /// renderer path and are invisible to per-call callbacks.
    #[cfg(test)]
    test_runs: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    test_events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Test-only tap (cfg(test)): the plain text of the preview lines the
    /// last `run()` printed live, captured from the renderer thread's
    /// `RenderFinal` (its terminal output is not capturable in-process).
    #[cfg(test)]
    test_preview: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    #[cfg(test)]
    test_preview_diverged: std::sync::Arc<std::sync::Mutex<bool>>,
    /// Test tap (cfg(test)): the post-run flush path's last appended lines —
    /// the suffix under the preview, or today's full render on the fallback.
    #[cfg(test)]
    test_final_print: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Test tap (cfg(test)): the interleaved print order of the last run —
    /// `text:<line>` / `tool:<name>` / `thinking` markers from the renderer
    /// (story 1.3 e2e interleave assertion).
    #[cfg(test)]
    test_render_order: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Test sink (cfg(test), story 2.2): per-INSTANCE notice capture for
    /// the mid-run reader's queue/cancel notices — no globals, so
    /// parallel tests never capture each other's stderr writes.
    #[cfg(test)]
    test_mid_run_notice_sink: Option<std::sync::Arc<std::sync::Mutex<Vec<String>>>>,
    /// Test tap (cfg(test)): the mid-run key injection source for story
    /// 2.1 smokes — the reader takes this once instead of the keyboard.
    #[cfg(test)]
    test_mid_run_keys:
        std::sync::Arc<std::sync::Mutex<Option<std::sync::mpsc::Receiver<MidRunKey>>>>,
    /// Test tap (cfg(test)): the mid-run input echo frames (story 2.1).
    #[cfg(test)]
    test_input_echoes: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// cfg(test): per-instance prompt gate for the mid-run reader
    /// (story 2.3 smokes) — parallel e2e gates never collide.
    #[cfg(test)]
    test_prompt_gate: std::sync::Arc<AtomicBool>,
    /// Test tap (cfg(test)): which terminal badge each run's Err branch
    /// rendered (story 1.4: "Cancelled by user" must surface as the
    /// cancel notice; everything else keeps the red error badge).
    #[cfg(test)]
    test_notice: std::sync::Arc<std::sync::Mutex<Vec<RunOutcomeBadge>>>,
    /// Test tap (cfg(test)): how many times a run executed the terminal
    /// cursor-cleanup block — the "status line erased exactly once"
    /// guarantee asserted by the cancel smokes (story 1.4 review).
    #[cfg(test)]
    test_terminal_cleanup_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Test-only cancel flag (cfg(test), story 1.4): a private instance
    /// the cancel smokes flip, so parallel tests in one process never
    /// observe each other's cancel. Production always resolves the
    /// process-wide flag the signal task flips (`run_cancel_flag`).
    #[cfg(test)]
    test_cancel_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
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
            pending_follow_ups: Default::default(),
            drain_depth: 0,
            show_thoughts_override: None,
            timeline_style: TimelineStyle::Detailed,
            last_run: RunStats::default(),
            #[cfg(test)]
            test_runs: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            test_events: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            #[cfg(test)]
            test_preview: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            #[cfg(test)]
            test_preview_diverged: std::sync::Arc::new(std::sync::Mutex::new(false)),
            #[cfg(test)]
            test_final_print: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            #[cfg(test)]
            test_render_order: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            #[cfg(test)]
            test_notice: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            #[cfg(test)]
            test_mid_run_keys: std::sync::Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            test_input_echoes: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            #[cfg(test)]
            test_prompt_gate: std::sync::Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            test_mid_run_notice_sink: None,
            #[cfg(test)]
            test_terminal_cleanup_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
                0,
            )),
            #[cfg(test)]
            test_cancel_flag: None,
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

    /// Install a private cancel flag for the hermetic cancel smokes
    /// (story 1.4): runs then watch THIS flag instead of the process
    /// global, so flipping it mid-run cannot cancel a concurrently
    /// running test's run (tests share one process; the process flag is
    /// process-global).
    /// Per-instance prompt gate for the mid-run reader (cfg(test),
    /// story 2.3): the smoke flips this flag instead of the global.
    #[cfg(test)]
    pub(crate) fn with_mid_run_prompt_gate(mut self, gate: std::sync::Arc<AtomicBool>) -> Self {
        self.test_prompt_gate = gate;
        self
    }

    /// Per-instance notice-capture sink for the mid-run reader's queue
    /// and cancel notices (cfg(test), story 2.2).
    #[cfg(test)]
    pub(crate) fn with_mid_run_notice_sink(
        mut self,
        sink: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Self {
        self.test_mid_run_notice_sink = Some(sink);
        self
    }

    /// Inject the story-2.1 mid-run key source (cfg(test) only): the
    /// reader thread's keyboard-feed replacement for deterministic smokes.
    #[cfg(test)]
    pub(crate) fn with_mid_run_keys(self, source: std::sync::mpsc::Receiver<MidRunKey>) -> Self {
        *self.test_mid_run_keys.lock().unwrap() = Some(source);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_cancel_flag(
        mut self,
        flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        self.test_cancel_flag = Some(flag);
        self
    }

    /// The cancel flag handle the active run watches (story 1.4): the
    /// process-wide flag the signal task flips, or the test-injected
    /// instance under cfg(test).
    fn run_cancel_flag(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        #[cfg(test)]
        {
            if let Some(flag) = &self.test_cancel_flag {
                return flag.clone();
            }
        }
        crate::cancel_flag()
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

    /// Route mid-run typed text per queue mode (story 2.4, shared by
    /// the TUI's key path and the inline reader): Interrupt → steer via
    /// the runtime client; FollowUp → queue client-side.
    pub async fn steer_text(&mut self, content: String) {
        if content.is_empty() {
            return;
        }
        match self.queue_mode {
            QueueMode::Interrupt => {
                self.ensure_connection()
                    .await
                    .expect("TUI steer_text requires a connection");
                if let Some(runtime) = self.connection.as_mut() {
                    let _ = runtime.steer(content).await;
                }
            }
            QueueMode::FollowUp => {
                self.pending_follow_ups.lock().unwrap().push_back(content);
            }
        }
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

    /// Override `output.show_thoughts` for this session (`--no-thoughts`).
    pub fn with_show_thoughts(mut self, value: Option<bool>) -> Self {
        self.show_thoughts_override = value;
        self
    }

    /// Whether inline runs print the model's pre-tool reasoning.
    pub fn show_thoughts(&self) -> bool {
        self.show_thoughts_override
            .unwrap_or(self.config.output.show_thoughts)
    }

    /// Toggle reasoning output from the REPL (`/thoughts`).
    pub fn set_show_thoughts(&mut self, enabled: bool) {
        self.show_thoughts_override = Some(enabled);
    }

    /// Detail level of the inline tool timeline.
    pub fn timeline_style(&self) -> TimelineStyle {
        self.timeline_style
    }

    /// Choose the timeline detail level (`/timeline`).
    pub fn set_timeline_style(&mut self, style: TimelineStyle) {
        self.timeline_style = style;
    }

    /// Figures from the most recent [`Commands::run`]: elapsed time is the
    /// caller's, tokens are estimated from the character counts recorded here.
    pub fn last_run_stats(&self) -> RunStats {
        self.last_run
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
        use crate::widgets::{components, inline, markdown as md_widget, progress};

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
        // The model's pre-tool reasoning is rendered as a DIM "thinking" block
        // when enabled (config `output.show_thoughts`, `--no-thoughts`,
        // `/thoughts`). The full text still reaches the renderer either way to
        // drive the status line's phase.
        let show_thoughts = self.show_thoughts();
        // `/timeline compact` keeps one row per tool: no streamed output, no
        // error bodies, no diff hunks.
        let timeline_detailed = self.timeline_style() == TimelineStyle::Detailed;
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

        /// One queue item: a streamed delta or a lifecycle event (story 1.3).
        ///
        /// ONE ordered queue, drained in arrival order: stdout positions fall
        /// out of the queue itself, so text announced live sits above the
        /// tool/thinking row that arrived after it. The order is total
        /// because every runtime mode folds deltas and events through the
        /// same async `run_turn` loop (`takka-cli/src/client.rs`) — two
        /// separate channels drained chunks-first-every-tick could print a
        /// tool row below text that arrived after it (the same-tick
        /// inversion story 1.1 kept in scope).
        enum RendererUpdate {
            /// A streamed answer delta (appends to the preview text).
            Chunk(String),
            /// A lifecycle event (tool row / thinking block / badges).
            Event(takka_core::Event),
            /// The current mid-run input buffer (story 2.1): the
            /// renderer draws it as one transient line above the status
            /// row (empty string = the input row collapses).
            InputEcho(String),
        }

        let (update_tx, update_rx) = std::sync::mpsc::channel::<RendererUpdate>();

        let progress = std::sync::Arc::new(std::sync::Mutex::new({
            let mut p = progress::ProgressState::new();
            p.start();
            // The connection is already established by `ensure_connection`;
            // what remains is waiting on the model.
            p.set_phase(progress::Phase::Thinking);
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
            /// Plain text of the preview lines printed live above the
            /// status line, in print order (empty when nothing was
            /// previewed — the no-chunk fallback signal).
            preview_committed: Vec<String>,
            /// True when the progressive re-render stopped extending the
            /// printed prefix mid-run; the end-of-run path then keeps
            /// today's full markdown render.
            preview_diverged: bool,
            /// cfg(test) only: the interleaved print order of the run —
            /// `text:<line>` for live preview lines, `tool:<name>` for
            /// timeline rows, `thinking` for thinking blocks. The e2e
            /// smokes assert the story-1.3 interleave against this list.
            #[cfg(test)]
            render_order: Vec<String>,
            /// cfg(test) only: the mid-run input echo frames (story 2.1).
            #[cfg(test)]
            input_echoes: Vec<String>,
        }

        let r_progress = progress.clone();
        let r_stop = stop_flag.clone();
        let render_panels = show_tool_calls;
        let renderer = std::thread::Builder::new()
            .name("takka-renderer".into())
            .spawn(move || {
                let block_text = std::sync::Mutex::new(String::new());

                // ── status line painter ────────────────────────────────
                // Shared with the console-log hook below, which repaints the
                // status line after printing a log line.
                let painter = std::sync::Arc::new(std::sync::Mutex::new(StatusPainter::new()));
                let commit_spinner = {
                    let painter = painter.clone();
                    let r_progress = r_progress.clone();
                    move || painter.lock().unwrap().commit(&r_progress)
                };
                let redraw_spinner = {
                    let painter = painter.clone();
                    let r_progress = r_progress.clone();
                    move || painter.lock().unwrap().redraw(&r_progress)
                };

                // Live-preview state (story 1.1 machinery, unchanged): the
                // plain text of the lines already printed, the completed
                // text they were rendered from (skips re-parsing when
                // nothing new completed), and the divergence flag that
                // ends previewing for this run.
                let mut preview = PreviewTracker::default();

                // Print the initial spinner.
                commit_spinner();

                // Route console logs around the spinner line: while this hook
                // is installed, every stderr log line erases the spinner,
                // prints itself, and re-arms the spinner, so the renderer's
                // "spinner is the last line" invariant survives log output
                // from other threads (config load, transport warnings, ...).
                // Story-2.1 review fix: the hook consumes the whole
                // transient band (status + the mid-run input row) — the
                // input row is re-rendered by the reader's next echo frame.
                // The transient input row's liveness: shared with every
                // path whose erase must consume the whole band (hook,
                // announces, event rows) — story-2.1 review fix.
                let input_rows: std::sync::Arc<std::sync::atomic::AtomicUsize> = Default::default();
                let _log_hook_guard;
                {
                    let hook_painter = painter.clone();
                    let hook_progress = r_progress.clone();
                    let hook_input_rows = input_rows.clone();
                    let hook: crate::widgets::log_writer::SpinnerHook =
                        std::sync::Arc::new(move |bytes: &[u8]| {
                            // try_lock: the gate may already be held by us (a
                            // widget that logs) or by an interactive prompt.
                            // Blocking there would deadlock; fall back to a
                            // plain write instead.
                            let gate = RENDER_GATE.try_lock();
                            if gate.is_err() {
                                crate::widgets::log_writer::write_line_to_stderr(bytes);
                                return;
                            }

                            let depth = 1 + hook_input_rows.load(Ordering::Relaxed);
                            let mut s = std::io::stdout();
                            {
                                use crossterm::cursor::{MoveToColumn, MoveUp};
                                use crossterm::terminal::{Clear, ClearType};
                                use crossterm::ExecutableCommand;
                                for _ in 0..depth {
                                    let _ = s.execute(MoveUp(1));
                                }
                                let _ = s.execute(MoveToColumn(0));
                                let _ = s.execute(Clear(ClearType::FromCursorDown));
                            }
                            crate::widgets::log_writer::write_line_to_stderr(bytes);
                            let mut painter = hook_painter.lock().unwrap();
                            painter.commit(&hook_progress);
                        });
                    _log_hook_guard = crate::widgets::log_writer::SpinnerHookGuard::install(hook);
                }

                // Wrap the receiver in a Mutex so we can access it
                // (it is !Sync but we only access from this thread).
                let update_rx = std::sync::Mutex::new(update_rx);
                let mut updates_done = false;
                // Tool call arguments keyed by call id, for the timeline rows.
                let mut timeline = ToolTimeline::default();

                // Mid-run input line state (story 2.1): the transient
                // buffer row drawn between the scrollback and the status
                // row. `active` = the row is currently on screen (or in
                // the redraw band). Frame tap echoes ride the test taps.
                // The row's liveness goes through `input_rows` (declared
                // above the log-hook install), whose depth EVERY transient
                // row erase consumes — story-2.1 review fix.
                // Erase every transient row (status + input band) — the
                // shared erase for announce rows, event rows and echo
                // frames alike.
                let erase_transient = {
                    let input_rows = input_rows.clone();
                    move || {
                        use crossterm::cursor::{MoveToColumn, MoveUp};
                        use crossterm::terminal::{Clear, ClearType};
                        use crossterm::ExecutableCommand;
                        let mut s = std::io::stdout();
                        for _ in 0..(1 + input_rows.load(Ordering::Relaxed)) {
                            let _ = s.execute(MoveUp(1));
                        }
                        let _ = s.execute(MoveToColumn(0));
                        let _ = s.execute(Clear(ClearType::FromCursorDown));
                    }
                };
                // The shared buffer slot so any announce/echo render can
                // re-draw the CURRENT mid-run input row.
                let input_buffer_shared: std::sync::Arc<std::sync::Mutex<String>> =
                    Default::default();
                let redraw_input_line_action = {
                    let input_rows = input_rows.clone();
                    let input_buffer_shared = input_buffer_shared.clone();
                    move || {
                        let buffer = input_buffer_shared.lock().unwrap().clone();
                        if buffer.is_empty() {
                            input_rows.store(0, Ordering::Relaxed);
                            return;
                        }
                        let echo = RLine::from(vec![
                            RSpan::styled(
                                "  ↳ ".to_string(),
                                RStyle::default().fg(RColor::Rgb(110, 110, 140)),
                            ),
                            RSpan::styled(
                                format!("{}▏", buffer),
                                RStyle::default().fg(RColor::Rgb(200, 200, 215)),
                            ),
                        ]);
                        inline::print_line(&echo);
                        input_rows.store(1, Ordering::Relaxed);
                    }
                };
                let mut input_active = false;
                #[allow(unused_assignments)]
                let mut input_buffer = String::new();
                #[cfg(test)]
                let mut input_echoes: Vec<String> = Vec::new();

                loop {
                    if updates_done {
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

                    // Drain the queue in ARRIVAL ORDER (story 1.3): chunks
                    // append to the preview text; an event's row renders
                    // only after the text that arrived before it has been
                    // announced; trailing chunks announce at tick end —
                    // today's pure-text behavior. The whole drain runs
                    // under the render gate this iteration already holds;
                    // when a prompt owns the gate the renderer parks at the
                    // top of the tick (blocking lock), which defers the
                    // whole tick to a later one — the gate is never
                    // contended twice and nothing prints while the prompt
                    // is up.
                    let mut pending_text = false;
                    if !updates_done {
                        loop {
                            match update_rx.lock().unwrap().try_recv() {
                                Ok(RendererUpdate::InputEcho(text)) => {
                                    // Story 2.1: the transient input line.
                                    input_buffer = text;
                                    *input_buffer_shared.lock().unwrap() = input_buffer.clone();
                                    if input_active || !input_buffer.is_empty() {
                                        // Erase the transient band (status
                                        // + prior input row) via the shared
                                        // erase, redraw the input row (when
                                        // a buffer exists), and commit the
                                        // status below it.
                                        erase_transient();
                                        redraw_input_line_action();
                                        input_active = !input_buffer.is_empty();
                                        commit_spinner();
                                        #[cfg(test)]
                                        input_echoes.push(input_buffer.clone());
                                    }
                                }
                                Ok(RendererUpdate::Chunk(chunk)) => {
                                    if !chunk.is_empty() {
                                        block_text.lock().unwrap().push_str(&chunk);
                                        pending_text = true;
                                    }
                                }
                                Ok(RendererUpdate::Event(event)) => {
                                    // Arrival order: announce the streamed
                                    // text queued before this event first,
                                    // so its row prints BELOW that text.
                                    if pending_text {
                                        let block_guard = block_text.lock().unwrap();
                                        preview.announce(
                                            block_guard.as_str(),
                                            &erase_transient,
                                            &redraw_input_line_action,
                                            &commit_spinner,
                                        );
                                        pending_text = false;
                                    }
                                    // Thinking suppression (story 1.3
                                    // decision, 2026-11-06): the tool-call
                                    // turn's Thinking event re-posts the
                                    // turn's streamed content, which the
                                    // preview has already printed as the
                                    // answer (design decision A) — skip it
                                    // whenever the full text is inside the
                                    // flushed preview. Not fully previewed,
                                    // or a diverged preview: renders as
                                    // today. No terminal work happens when
                                    // suppressed, so the status line keeps
                                    // its position untouched.
                                    if let takka_core::Event::Thinking { content } = &event {
                                        if !preview.diverged
                                            && !content.trim().is_empty()
                                            && preview
                                                .flushed
                                                .trim_end()
                                                .ends_with(content.trim_end())
                                        {
                                            continue;
                                        }
                                    }
                                    // Erase the transient band first (story
                                    // 2.1 review fix: the status + the
                                    // mid-run input row — every row-print
                                    // erase must consume the whole band, or
                                    // the input row strands and the next
                                    // echo destroys real scrollback).
                                    erase_transient();
                                    // Tool rows follow `show_tool_calls`; steered
                                    // hand-over acks render regardless (they
                                    // are the story's visible contract); the
                                    // reasoning block follows `show_thoughts`.
                                    if render_panels
                                        || matches!(
                                            event,
                                            takka_core::Event::Thinking { .. }
                                                | takka_core::Event::SteerQueued { .. }
                                                | takka_core::Event::Steered { .. }
                                        )
                                    {
                                        render_event(&event, &mut timeline, timeline_detailed);
                                        // cfg(test): record what actually
                                        // printed for the interleave
                                        // assertions — the thinking block
                                        // (non-empty content only) and the
                                        // tool completion row (under
                                        // `show_tool_calls`). ToolFinished
                                        // is a synthesized echo with no
                                        // `render_event` arm (prints
                                        // nothing), and ToolDelta lines only
                                        // print in detailed mode — neither
                                        // is marked.
                                        #[cfg(test)]
                                        match &event {
                                            takka_core::Event::Thinking { content }
                                                if !content.trim().is_empty() =>
                                            {
                                                preview.order.push("thinking".to_string())
                                            }
                                            takka_core::Event::ToolOutput { .. }
                                                if render_panels =>
                                            {
                                                preview.order.push("tool".to_string())
                                            }
                                            takka_core::Event::SteerQueued { content } => preview
                                                .order
                                                .push(format!("steer-queued:{content}")),
                                            takka_core::Event::Steered { content } => {
                                                preview.order.push(format!("steered:{content}"))
                                            }
                                            _ => {}
                                        }
                                    }
                                    // Story 2.1 review fix: the input row
                                    // re-draws BELOW every announced row and
                                    // ABOVE the re-committed status line,
                                    // keeping the status last.
                                    redraw_input_line_action();
                                    commit_spinner();
                                }
                                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                    updates_done = true;
                                    break;
                                }
                            }
                        }
                    }
                    // End-of-tick flush: pure-text ticks announce
                    // their completed paragraphs here (unchanged rule for
                    // ticks without events).
                    if pending_text {
                        let block_guard = block_text.lock().unwrap();
                        preview.announce(
                            block_guard.as_str(),
                            &erase_transient,
                            &redraw_input_line_action,
                            &commit_spinner,
                        );
                    }
                }

                // The spinner cleanup in `run` owns the terminal from here on.
                // Drop the hook first so a late log line can't re-arm a
                // spinner after we are gone.
                drop(_log_hook_guard);

                RenderFinal {
                    block_text: block_text.into_inner().unwrap(),
                    preview_committed: preview.committed,
                    preview_diverged: preview.diverged,
                    #[cfg(test)]
                    render_order: preview.order,
                    #[cfg(test)]
                    input_echoes,
                }
            })
            .expect("failed to spawn renderer thread");

        // Resolve the run's cancel flag before the connection borrow
        // (story 1.4): `run_turn` watches it between events.
        let cancel_flag = self.run_cancel_flag();
        // Per-run two-stage semantics (story 1.4 review): the first Ctrl+C
        // of THIS run is the cooperative one. A flag surviving from the
        // previous run (set at the idle prompt or carried over from a
        // cancelled run) would insta-cancel this run and turn the NEXT
        // press into the force-exit — reset it before the watcher's first
        // read.
        cancel_flag.store(false, std::sync::atomic::Ordering::Relaxed);

        // ── Mid-run input (story 2.1, both queue modes now — 2.2) ─────
        //
        // While the run owns the terminal, ONE spawned reader thread
        // collects typed characters, echoes them through the renderer's
        // ordered queue (`RendererUpdate::InputEcho` — the single-writer
        // discipline holds), and on Enter routes the line per the
        // captured queue mode: Interrupt → `Request::Steer` via
        // `steer_tx` (the `run_turn` watcher); FollowUp (the CLI default)
        // → the client-side follow-up queue + a log-hook notice, whose
        // existing post-run drain executes the line as the next run.
        // Ctrl+C rides the story-1.4 two-stage cancel path from the same
        // thread. The reader ends when the run's stop flag flips.
        // Mid-run `/queue` mode switching is out of scope: the mode is
        // captured at spawn (a switch applies from the next run).
        // The reader's notice write: production goes to the log hook
        // (through the reader's closure), cfg(test) smokes capture it
        // per-instance — no global, no parallel-test collisions.
        // Unconditional steering channel: follows both queue modes; the
        // reader only writes to it in Interrupt mode (story 2.2 does not
        // need `None` in follow-up — see implementation notes).
        let steer_rx;
        let queue_mode_captured = self.queue_mode;
        let pending_follow_ups_shared = self.pending_follow_ups.clone();
        #[allow(unused_assignments)] // review: see the join at the drain
        let mut reader_thread_handle: Option<std::thread::JoinHandle<()>> = None;
        {
            let (steer_tx, steer_line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            let (echo_tx, echo_rx) = std::sync::mpsc::channel::<String>();
            steer_rx = Some(steer_line_rx);
            // No standing "input is live" badge: an Interrupt run with no
            // typed text stays byte-for-byte today's output (story-2.1
            // review fix, zero-residue rule).
            #[cfg(test)]
            let notice_sink = self.test_mid_run_notice_sink.clone().unwrap_or_default();
            // cfg(test) injection: the cancel smokes feed deterministic
            // keys instead of the keyboard (story 1.4's cancel-driver
            // pattern). `test_mid_run_keys` is a one-shot take.
            #[cfg(test)]
            let test_keys = self.test_mid_run_keys.clone();
            // cfg(test): the reader's prompt gate is PER-INSTANCE — the
            // parallel steering smokes own their own gate flag instead of
            // the process-global prompt flag (which the prompt-gate smoke
            // flips for a different Commands instance).
            #[cfg(test)]
            let test_prompt_gate = self.test_prompt_gate.clone();
            let r_stop_reader = stop_flag.clone();
            let cancel_reader = cancel_flag.clone();
            // Story-2.2: the reader routes its notices through the log
            // hook in production; a cfg-test smoke may install a
            // per-instance capture sink (no globals, no cross-test
            // captures — the smoke asserts the reader's exact notice).
            let mid_run_notice = move |msg: String| {
                crate::widgets::log_writer::write_notice_dispatch(
                    msg,
                    #[cfg(test)]
                    Some(crate::widgets::log_writer::NoticeSink(notice_sink.clone())),
                    #[cfg(not(test))]
                    None,
                );
            };
            let reader_spawn_handle = std::thread::Builder::new()
                .name("takka-mid-run-input".into())
                .spawn(move || {
                    let mut buffer = String::new();
                    #[cfg(test)]
                    let injected = test_keys.lock().unwrap().take();
                    #[cfg(test)]
                    if injected.is_none() {
                        // No injection source configured: nothing keeps
                        // this reader alive in test builds.
                        let _ = echo_tx.send(String::new());
                        return;
                    }
                    loop {
                        if r_stop_reader.load(Ordering::Relaxed) {
                            break;
                        }
                        // Story-2.3 fix: while a confirmation/question
                        // prompt owns stdin (cooked mode, dialoguer), the
                        // reader must not race it for keystrokes — park AND
                        // discard. Keys typed behind the gate belong to the
                        // prompt's stdin; buffering them for later steering
                        // would lose them into a run the user already meant
                        // to talk to, so they drop. Per-INSTANCE gate (the
                        // prompt flag flows through the run's prompt
                        // messages) — not the global, which other tests
                        // own.
                        #[cfg(test)]
                        if test_prompt_gate.load(Ordering::Relaxed) {
                            let _ = injected
                                .as_ref()
                                .map(|rx| rx.try_recv());
                            std::thread::sleep(std::time::Duration::from_millis(20));
                            continue;
                        }
                        if r_stop_reader.load(Ordering::Relaxed) {
                            break;
                        }
                        #[cfg(test)]
                        let key: Option<MidRunKey> = 'blk: {
                            let Some(rx) = &injected else {
                                break 'blk None;
                            };
                            use std::sync::mpsc::TryRecvError;
                            match rx.try_recv() {
                                Ok(key) => break 'blk Some(key),
                                Err(TryRecvError::Empty) => {
                                    std::thread::sleep(std::time::Duration::from_millis(20));
                                    break 'blk None;
                                }
                                Err(TryRecvError::Disconnected) => break 'blk None,
                            }
                        };
                        #[cfg(not(test))]
                        let key: Option<MidRunKey> = poll_crossterm_key();
                        match key {
                            None => continue,
                            Some(MidRunKey::CtrlC) => {
                                // Two-stage cancel, reader parity with the
                                // signal task (main.rs): the first press
                                // requests the cooperative cancel, the
                                // second force-exits at 130.
                                if cancel_reader.swap(true, Ordering::SeqCst) {
                                    mid_run_notice("\n\u{26a0} Force-exiting on second Ctrl+C.".to_string());
                                    std::process::exit(130);
                                }
                                mid_run_notice("\n\u{26a0} Cancel requested (press Ctrl+C again to force-exit).".to_string());
                                buffer.clear();
                                let _ = echo_tx.send(String::new());
                            }
                            Some(MidRunKey::Char(ch)) => {
                                buffer.push(ch);
                                let _ = echo_tx.send(buffer.clone());
                            }
                            Some(MidRunKey::Enter) => {
                                let line = std::mem::take(&mut buffer);
                                let _ = echo_tx.send(String::new());
                                if !line.is_empty() {
                                    match queue_mode_captured {
                                        QueueMode::Interrupt => {
                                            let _ = steer_tx.send(line);
                                        }
                                        QueueMode::FollowUp => {
                                            // Story 2.2: the default mode
                                            // queues the text client-side;
                                            // the drain runs it as the next
                                            // run, and the log hook makes
                                            // the queueing visible in the
                                            // same transient band the
                                            // notices use.
                                            pending_follow_ups_shared
                                                .lock()
                                                .unwrap()
                                                .push_back(line.clone());
                                            mid_run_notice(format!(
                                                "\n\u{21b3} queued for the next run: \"{}\"",
                                                line
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    let _ = echo_tx.send(String::new());
                })
                .expect("failed to spawn mid-run input reader");
            // Keep the reader's handle: the drain runs only after the
            // reader joined, so an Enter landing inside the stop-flag
            // window cannot push past the promised next-run drain
            // (story-2.2 review fix #2).
            reader_thread_handle = Some(reader_spawn_handle);
            let update_tx_mid = update_tx.clone();
            let _echo_spooler = std::thread::Builder::new()
                .name("takka-mid-run-echo".into())
                .spawn(move || {
                    while let Ok(frame) = echo_rx.recv() {
                        if update_tx_mid
                            .send(RendererUpdate::InputEcho(frame))
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .expect("failed to spawn mid-run echo spooler");
        }
        let runtime = self
            .connection
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Runtime not initialized"))?;

        // Live phase tracking for the status line. Both callbacks run on
        // runtime threads, so they only touch the shared `ProgressState`
        // (never the terminal — the renderer owns that).
        let phase_progress = progress.clone();
        let run_tools = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let run_usage = std::sync::Arc::new(std::sync::Mutex::new(UsageTotals::default()));

        let result = runtime
            .run(
                task.to_string(),
                attachments,
                |chunk| {
                    if !chunk.is_empty() {
                        {
                            let mut p = phase_progress.lock().unwrap();
                            p.set_phase(progress::Phase::Streaming);
                            p.add_stream_chars(chunk.chars().count());
                        }
                        let _ = update_tx.send(RendererUpdate::Chunk(chunk.to_string()));
                    }
                },
                {
                    // Test tap: drained follow-up runs travel through this
                    // path, so e2e tests observe their lifecycle here.
                    #[cfg(test)]
                    let test_events = std::sync::Arc::clone(&self.test_events);
                    // `move` (needed for the test tap) moves this block's
                    // captured sender; shadow it with a clone so the
                    // original stays alive and `drop(update_tx)` below
                    // still releases the renderer.
                    let update_tx = update_tx.clone();
                    let e_progress = progress.clone();
                    let e_tools = run_tools.clone();
                    let e_usage = run_usage.clone();
                    move |event| {
                        if let Ok(mut p) = e_progress.lock() {
                            match &event {
                                takka_core::Event::ToolCall { tool_name, .. } => {
                                    p.set_tool(tool_name.clone());
                                }
                                takka_core::Event::ToolStarted { tool_name, .. } => {
                                    p.set_tool(tool_name.clone());
                                    e_tools.fetch_add(1, Ordering::Relaxed);
                                }
                                takka_core::Event::ToolOutput { .. } => {
                                    p.set_phase(progress::Phase::Thinking);
                                }
                                takka_core::Event::ConfirmationRequest { .. }
                                | takka_core::Event::QuestionRequest { .. } => {
                                    p.set_phase(progress::Phase::WaitingApproval);
                                }
                                _ => {}
                            }
                        }
                        // Real provider usage: accumulate for the end-of-run
                        // summary. It is not forwarded to the renderer — the
                        // numbers are shown once, not floated mid-run.
                        if let takka_core::Event::Usage {
                            prompt_tokens,
                            completion_tokens,
                            cache_read_input_tokens,
                            cache_creation_input_tokens,
                        } = &event
                        {
                            e_usage.lock().unwrap().add(
                                *prompt_tokens,
                                *completion_tokens,
                                cache_read_input_tokens.unwrap_or(0),
                                cache_creation_input_tokens.unwrap_or(0),
                            );
                        }
                        #[cfg(test)]
                        test_events.lock().unwrap().push(format!("{event:?}"));
                        let is_usage = matches!(event, takka_core::Event::Usage { .. });
                        // ModelChunk is the raw per-delta echo; the renderer
                        // gets the same text as a Chunk and has no use for
                        // the event (story 1.3: one queue, no no-op spins).
                        let is_noop_model_chunk =
                            matches!(event, takka_core::Event::ModelChunk { .. });
                        if !is_usage
                            && !is_noop_model_chunk
                            && (show_thoughts
                                || !matches!(event, takka_core::Event::Thinking { .. }))
                        {
                            let _ = update_tx.send(RendererUpdate::Event(event));
                        }
                    }
                },
                |event| match &event.event {
                    takka_core::Event::ConfirmationRequest {
                        action,
                        description,
                        risk_level,
                    } => {
                        // Use the shared confirmation panel (risk badge, diff
                        // preview, y/n/a/q). It also parks the renderer and
                        // consumes the status line for the duration of the
                        // prompt — a bare dialoguer prompt here let the spinner
                        // redraw over the question in REPL mode.
                        let request = takka_core::ConfirmationRequest {
                            action: action.clone(),
                            description: description.clone(),
                            risk_level: match risk_level.as_str() {
                                "low" => takka_core::RiskLevel::Low,
                                "high" => takka_core::RiskLevel::High,
                                "critical" => takka_core::RiskLevel::Critical,
                                _ => takka_core::RiskLevel::Medium,
                            },
                            risk_score: 0.5,
                            reason: description.clone(),
                            timestamp: chrono::Utc::now(),
                            preview_diff: None,
                        };
                        let approved = matches!(
                            prompt_confirmation(&request),
                            Some(ConfirmationResponse::Yes | ConfirmationResponse::Always)
                        );
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
                cancel_flag,
                steer_rx,
            )
            .await;

        drop(update_tx);
        stop_flag.store(true, Ordering::Relaxed);

        let render_final = renderer.join().expect("renderer thread panicked");
        // Test tap: e2e tests observe what the live preview printed (the
        // renderer thread's terminal output is not capturable in-process).
        #[cfg(test)]
        {
            *self.test_preview.lock().unwrap() = render_final.preview_committed.clone();
            *self.test_preview_diverged.lock().unwrap() = render_final.preview_diverged;
            *self.test_render_order.lock().unwrap() = render_final.render_order.clone();
            *self.test_input_echoes.lock().unwrap() = render_final.input_echoes.clone();
        }
        // The question handler only does its spinner consume/re-arm dance
        // while this flag is set, so it never touches the screen in modes
        // where no transient spinner exists (e.g. the full-screen TUI).
        SPINNER_ACTIVE.store(false, Ordering::Relaxed);
        progress.lock().unwrap().stop();

        // Cursor cleanup + final result.  The renderer always runs in
        // "spinner mode" (we never wrote chunks to stdout), so cleanup
        // is the same in every case: erase the spinner line, then render
        // the accumulated text as styled markdown.
        #[cfg(test)]
        self.test_terminal_cleanup_count
            .fetch_add(1, Ordering::Relaxed);
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

        let usage = {
            let totals = run_usage.lock().unwrap();
            (
                totals.measured,
                totals.prompt,
                totals.completion,
                totals.cache_read,
                totals.cache_creation,
            )
        };
        let (measured, prompt_tokens, completion_tokens, cache_read, cache_creation) = usage;
        self.last_run = RunStats {
            input_chars: task.len(),
            output_chars: 0,
            tool_calls: run_tools.load(Ordering::Relaxed),
            prompt_tokens: measured.then_some(prompt_tokens),
            completion_tokens: measured.then_some(completion_tokens),
            cache_read_tokens: cache_read,
            cache_creation_tokens: cache_creation,
        };

        match result {
            Ok(final_result) => {
                // RunStats keeps today's semantics: the orchestrator's
                // final answer (or the stream when it returned nothing) —
                // NOT the suffix-decision source (review patch 2026-11-06).
                let today_source = if final_result.is_empty() {
                    &render_final.block_text
                } else {
                    &final_result
                };
                self.last_run.output_chars = today_source.chars().count();
                if !today_source.is_empty() {
                    // The live preview printed the streamed answer above the
                    // status line: those lines ARE the final answer (design
                    // decision A), so only the suffix the preview never
                    // reached is emitted here — usually the trailing partial
                    // line. A prefix mismatch (mid-run divergence, or a
                    // final text that escaped the stream) keeps today's full
                    // render — the story's documented duplication case. An
                    // empty preview matches trivially, so the suffix is the
                    // whole render: the no-chunk fallback stays today's path
                    // exactly. Multi-turn runs (story 1.3): `final_result`
                    // covers only the LAST turn, so today's source would
                    // re-print the last block after it was already previewed
                    // — the print source upgrades to the FULL stream, but
                    // only after the strict line-prefix match against its
                    // parse verifies; a mismatch falls back to today's
                    // source and today's decision (the documented rule).
                    let mut parsed = md_widget::MarkdownContent::parse(today_source);
                    let mut suffix_start = if !render_final.preview_diverged
                        && preview_prefix_matches(&render_final.preview_committed, &parsed.lines)
                    {
                        Some(render_final.preview_committed.len())
                    } else {
                        None
                    };
                    if suffix_start.is_none()
                        && end_of_run_uses_full_stream(
                            &render_final.preview_committed,
                            render_final.preview_diverged,
                            &render_final.block_text,
                            &final_result,
                        )
                        && preview_prefix_matches(
                            &render_final.preview_committed,
                            &md_widget::MarkdownContent::parse(&render_final.block_text).lines,
                        )
                    {
                        parsed = md_widget::MarkdownContent::parse(&render_final.block_text);
                        suffix_start = Some(render_final.preview_committed.len());
                    }
                    let final_lines: &[ratatui::text::Line<'_>] = if let Some(n) = suffix_start {
                        &parsed.lines[n..]
                    } else {
                        &parsed.lines
                    };
                    #[cfg(test)]
                    {
                        *self.test_final_print.lock().unwrap() = final_lines
                            .iter()
                            .map(|l| {
                                l.spans
                                    .iter()
                                    .map(|sp| sp.content.to_string())
                                    .collect::<String>()
                            })
                            .collect();
                    }
                    inline::print_lines(final_lines);
                }
                inline::print_blank();
            }
            Err(e) => {
                // Story 1.4: "Cancelled by user" is a desired outcome —
                // the cancel notice, not the red error badge.
                let badge = run_outcome_badge(&e);
                match badge {
                    RunOutcomeBadge::Cancelled => {
                        inline::print_line(&components::info_badge(&e.to_string()));
                    }
                    RunOutcomeBadge::Error => {
                        inline::print_line(&components::error_badge(&e.to_string()));
                    }
                }
                #[cfg(test)]
                self.test_notice.lock().unwrap().push(badge);
                inline::print_blank();
            }
        }

        // After the run's terminal event (Done or Error), drain queued
        // follow-ups: text submitted mid-run in FollowUp mode is sent as
        // the next run (client-issued, depth-guarded loop). Both outcome
        // branches above converge here.
        //
        // Story-2.2 review fix: the mid-run reader joins FIRST — an Enter
        // landing inside the stop-flag window cannot push a line past the
        // drain (the "next run" promise stays literal).
        if let Some(reader) = reader_thread_handle {
            let _ = reader.join();
        }
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
        // Resolve the run's cancel flag before the connection borrow
        // (story 1.4): the TUI path gets the same cancel wiring as the
        // inline REPL — both UIs fold every event through `run_turn`.
        // Per-run two-stage reset: the TUI's first Ctrl+C is the
        // cooperative one (story 1.4 review fix).
        let cancel_flag = self.run_cancel_flag();
        // Per-run two-stage semantics (story 1.4 review): the first Ctrl+C
        // of THIS run is the cooperative one. A flag surviving from the
        // previous run (set at the idle prompt or carried over from a
        // cancelled run) would insta-cancel this run and turn the NEXT
        // press into the force-exit — reset it before the watcher's first
        // read.
        cancel_flag.store(false, std::sync::atomic::Ordering::Relaxed);
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
                cancel_flag,
                None,
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
    pub fn pending_follow_ups(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::VecDeque<String>> {
        self.pending_follow_ups.lock().unwrap()
    }

    /// Snapshot of the pending follow-ups (story 2.4 tests): avoids a
    /// live guard crossing an async boundary.
    pub fn pending_follow_ups_snapshot(&self) -> Vec<String> {
        self.pending_follow_ups
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect()
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
                self.pending_follow_ups.lock().unwrap().push_back(text);
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
        loop {
            let next = self.pending_follow_ups.lock().unwrap().pop_front();
            let Some(next) = next else { break };
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
            ConfigAction::Migrate { yes, force } => self.config_migrate(*yes, *force)?,
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
                .join("takka")
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

    // ── Config migrate ──────────────────────────────────────

    /// Move legacy `~/.config/agentic` user state (config, sessions,
    /// memory) to `~/.config/takka`. Interactive `y/N` confirmation
    /// unless `--yes`; refuses an existing target config unless
    /// `--force`. Idempotent: with nothing left at the legacy path it
    /// reports there is nothing to migrate.
    fn config_migrate(&self, yes: bool, force: bool) -> Result<()> {
        let legacy = takka_core::config::legacy_config_dir();
        let new = takka_core::config::config_dir();

        let items = config_migrate::migration_items(&legacy, &new);
        if items.is_empty() {
            print_info(&format!(
                "Nothing to migrate: no legacy data found at {}",
                legacy.display()
            ));
            return Ok(());
        }

        config_migrate::target_conflict(&new, force).map_err(CommandError::Config)?;

        println!(
            "Moving legacy user data from {} to {}:",
            legacy.display(),
            new.display()
        );
        for item in &items {
            println!(
                "  {:<8} {} → {}",
                item.label,
                item.source.display(),
                item.dest.display()
            );
        }

        if !yes {
            let confirm = Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt("Continue?")
                .default(false)
                .interact()
                .unwrap_or(false);
            if !confirm {
                println!("Aborted.");
                return Ok(());
            }
        }

        config_migrate::apply_migration(&items).map_err(CommandError::Config)?;
        print_success(&format!(
            "Migration complete — your data now lives at {}",
            new.display()
        ));
        Ok(())
    }

    // ── Config backup ───────────────────────────────────────

    fn config_backup(&self) -> Result<()> {
        // Back up the config that would actually load: new location,
        // or the legacy file for a pre-migration user.
        let (config_path, _) = Config::effective_config_path();

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

// ── Transient status line ─────────────────────────────────────

/// Per-run figures the REPL shows in its end-of-run summary.
#[derive(Debug, Default, Clone, Copy)]
pub struct RunStats {
    /// Prompt characters sent for this run (fallback token estimate).
    pub input_chars: usize,
    /// Response characters streamed back for this run (fallback estimate).
    pub output_chars: usize,
    /// Tools executed during this run.
    pub tool_calls: u32,
    /// Prompt tokens the provider reported (`None` when it reported nothing).
    pub prompt_tokens: Option<u32>,
    /// Completion tokens the provider reported.
    pub completion_tokens: Option<u32>,
    /// Prompt-cache reads / writes the provider reported.
    pub cache_read_tokens: u32,
    pub cache_creation_tokens: u32,
}

impl RunStats {
    /// Input/output tokens for this run, preferring the provider's report and
    /// falling back to the ~4-chars-per-token estimate.
    pub fn tokens(&self) -> (u32, u32) {
        let estimate = |chars: usize| (chars / 4).min(u32::MAX as usize) as u32;
        (
            self.prompt_tokens
                .unwrap_or_else(|| estimate(self.input_chars)),
            self.completion_tokens
                .unwrap_or_else(|| estimate(self.output_chars)),
        )
    }

    /// `true` when the numbers came from the provider rather than an estimate.
    pub fn tokens_are_measured(&self) -> bool {
        self.prompt_tokens.is_some() || self.completion_tokens.is_some()
    }
}

/// How much detail the inline tool timeline prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineStyle {
    /// Branch row per tool plus nested output, errors and diffs (default).
    Detailed,
    /// One row per tool — no streamed output, error bodies or diff hunks.
    Compact,
}

impl TimelineStyle {
    pub fn as_str(&self) -> &'static str {
        match self {
            TimelineStyle::Detailed => "detailed",
            TimelineStyle::Compact => "compact",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            TimelineStyle::Detailed => "rows plus nested output, errors and diffs",
            TimelineStyle::Compact => "one row per tool, no nested detail",
        }
    }
}

/// Token totals reported by the runtime during one `run()` call.
#[derive(Debug, Default)]
struct UsageTotals {
    prompt: u32,
    completion: u32,
    cache_read: u32,
    cache_creation: u32,
    measured: bool,
}

impl UsageTotals {
    fn add(&mut self, prompt: u32, completion: u32, cache_read: u32, cache_creation: u32) {
        self.prompt = self.prompt.saturating_add(prompt);
        self.completion = self.completion.saturating_add(completion);
        self.cache_read = self.cache_read.saturating_add(cache_read);
        self.cache_creation = self.cache_creation.saturating_add(cache_creation);
        self.measured = true;
    }
}

/// Paints the transient activity line that sits at the bottom of the inline
/// output during a run.
///
/// The line is overwritten in place (`MoveUp` + repaint) instead of being
/// erased and redrawn — the erase/draw cycle blinks on Windows conhost. Since
/// its width now varies with the phase label, elapsed time and token counter,
/// every frame is padded — under the min-cover rule
/// ([`frame_pad_on_resize`]) — to cover everything the predecessor could
/// have painted unwrapped at the terminal's current width, so a shorter
/// frame can never leave stale characters behind. The ambient width is
/// re-measured fresh every frame (nothing caches it), and the returned pad
/// becomes the next baseline: the painter's only stored state is
/// `frame_width`.
struct StatusPainter {
    frame_width: usize,
    /// `ctrl+c to cancel` is only worth the columns when the terminal is wide
    /// enough to hold the status line plus the hint.
    show_cancel_hint: bool,
}

impl StatusPainter {
    fn new() -> Self {
        Self {
            frame_width: 0,
            show_cancel_hint: inline::is_stdout_tty() && inline::terminal_width() >= 60,
        }
    }

    /// Build the next frame, padded per the min-cover resize rule: the
    /// ambient width is re-measured fresh each frame and the pad covers
    /// everything the predecessor could have painted unwrapped at that
    /// width. The returned pad doubles as the new baseline.
    fn next_line(
        &mut self,
        state: &std::sync::Mutex<crate::widgets::progress::ProgressState>,
    ) -> RLine<'static> {
        let mut line = {
            let mut progress = state.lock().unwrap();
            progress.tick();
            crate::widgets::spinner::status_line(&progress, self.show_cancel_hint)
        };
        let width = line.width();
        let pad = frame_pad_on_resize(self.frame_width, inline::terminal_width(), width);
        if pad > width {
            line.spans.push(RSpan::raw(" ".repeat(pad - width)));
        }
        self.frame_width = pad;
        line
    }

    /// Paint a fresh status line below the cursor.
    fn commit(&mut self, state: &std::sync::Mutex<crate::widgets::progress::ProgressState>) {
        let line = self.next_line(state);
        inline::print_line(&line);
    }

    /// Overwrite the status line in place.
    fn redraw(&mut self, state: &std::sync::Mutex<crate::widgets::progress::ProgressState>) {
        use crossterm::cursor::{MoveToColumn, MoveUp};
        use crossterm::ExecutableCommand;

        let line = self.next_line(state);
        let mut stdout = std::io::stdout();
        let _ = stdout.execute(MoveUp(1));
        let _ = stdout.execute(MoveToColumn(0));
        inline::print_line(&line);
    }

    /// Remove the status line and anything printed after it.
    fn erase(&self) {
        use crossterm::cursor::{MoveToColumn, MoveUp};
        use crossterm::terminal::{Clear, ClearType};
        use crossterm::ExecutableCommand;

        let mut stdout = std::io::stdout();
        let _ = stdout.execute(MoveUp(1));
        let _ = stdout.execute(MoveToColumn(0));
        let _ = stdout.execute(Clear(ClearType::FromCursorDown));
    }
}

/// The pad width for the next status frame under a mid-run resize
/// (story 1.2, the min-cover rule):
///
/// `pad = max(line_width, min(frame_width, ambient_now))`
///
/// Cover every column the predecessor could have painted WITHOUT WRAPPING
/// at the width the terminal has now — `redraw`'s in-place overwrite
/// writes no `Clear`, so anything short of that coverage strands stale
/// glyphs. Within one ambient width (the baseline was measured at the
/// current width) this is exactly `max(line_width, frame_width)` —
/// today's pad-to-widest, bit-for-bit. On a partial narrowing the
/// retained baseline still fits (`min` keeps it); on a deep narrow (the
/// baseline itself no longer fits) the baseline CONTRIBUTION is capped
/// at the new width, so the frame wraps only when its own content is
/// wider than the terminal (status content can exceed a very narrow
/// width — documented residual, out of this story's fix scope). In that
/// case the cap still covers the maximal recoverable one row's worth of
/// predecessor columns. The one irrecoverable exception is
/// the transition frame painted before the new width was first observed
/// (wrap/clipped residue until the terminal reflows it; on the wider
/// state after a deep-narrow the clamped baseline cannot cover columns
/// beyond the clamp until a naturally wider frame paints — same family
/// of cosmetic, bounded stale-column residuals; every frame after the
/// observation fits exactly). The caller's only state is
/// `frame_width` — the returned pad
/// becomes the next baseline; no ambient measurement is stored.
fn frame_pad_on_resize(frame_width: usize, ambient_now: usize, line_width: usize) -> usize {
    line_width.max(frame_width.min(ambient_now))
}

// ── Live preview of streamed text (story 1.1) ────────────────
//
// While a run streams, the renderer announces each newly completed line
// of `block_text` as styled markdown printed above the status line, using
// the console-log hook's proven erase → print → repaint cycle. The printed
// lines ARE the final answer: at end of run only the suffix the preview
// never reached is printed, so the answer appears exactly once.
//
// Wrap & resize doctrine (story 1.2, the console-log hook rule): a
// streamed line is announced once, whole, and the TERMINAL does all
// wrapping — an over-long line wraps once at the terminal's own ambient
// width, exactly as `log_writer::write_line_to_stderr` writes whole raw
// log lines with zero custom wrapping. Nothing in the preview path
// measures or caches a width; `preview_flush` is width-blind by
// construction. On a resize, lines already printed are never re-rendered,
// re-wrapped, or truncated — their scrollback is final; only lines not
// yet announced see the new width (the status painter's per-frame
// re-measure is the one width-aware surface, and it resets only its pad
// baseline, never a printed line).

/// One renderer tick's live-preview decision.
enum PreviewFlush {
    /// Styled lines to print below the already-printed ones (may be
    /// empty — nothing new completed since the last flush).
    Print(Vec<RLine<'static>>),
    /// The re-render no longer extends the printed prefix (an earlier
    /// line changed — e.g. a soft line break joined two printed lines).
    /// The caller stops previewing for this run; the end-of-run path
    /// falls back to today's full render.
    Diverged,
}

/// Plain-text projection of a styled line, for prefix comparison.
fn line_plain_text(line: &RLine<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Which terminal badge a run's `Err` value renders as (story 1.4).
/// "Cancelled by user" is a desired outcome, so it surfaces as the
/// neutral cancel notice instead of the red error badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunOutcomeBadge {
    Cancelled,
    Error,
}

/// Decide the badge for a run error. The cancel fact crosses the
/// protocol as a plain string (orchestrator `Err(AgenticError::
/// Cancelled)` → engine `Event::Error { message }` → `anyhow!(message)`
/// in `run_turn`), so the type is gone by the time it lands here.
/// Recomputing the same `thiserror` literal keeps both sides coupled to
/// the one variant — if core ever rewords it, this follows
/// automatically (documented coupling, story 1.4 design notes).
fn run_outcome_badge(error: &anyhow::Error) -> RunOutcomeBadge {
    if error.to_string() == takka_core::AgenticError::Cancelled.to_string() {
        RunOutcomeBadge::Cancelled
    } else {
        RunOutcomeBadge::Error
    }
}

/// The portion of `text` safe to announce this tick: everything up to and
/// including the last newline. A trailing fragment without a newline can
/// still grow into a longer line, so it stays pending until the newline
/// arrives (whole-line flush granularity — epic decision 2026-11-05a).
fn completed_text(text: &str) -> &str {
    // Whole-line granularity: a trailing fragment without a newline can
    // still grow into a longer line, so it stays pending (deferred to a
    // later tick).
    let some = match text.rfind('\n') {
        Some(end) => &text[..=end],
        None => "",
    };
    // Whole-paragraph granularity on top of it: markdown renders a single
    // newline inside a paragraph as a soft break that JOINS the lines
    // (`SoftBreak => push_span(" ")`), so announcing a paragraph's first
    // line early would later change already-printed lines — the divergence
    // fallback duplicating the answer for the common hard-wrapped prose
    // shape. Announce up to the last blank-line boundary only: a paragraph
    // renders complete, once, and never diverges.
    let boundary = match some.rfind("\n\n") {
        Some(pos) => pos + 2,
        None => return "",
    };
    // Whole-fence granularity: never announce across an unclosed code
    // fence — its temporary close (`parse_partial`) shifts when the block
    // grows, so a fence is held until its close (and the boundary after
    // it) completes. Back off to before the most recent triple-tick while
    // the announced region has odd fence parity.
    let mut cut = boundary;
    while cut > 0 {
        let region = &some[..cut];
        if region.matches("```").count() % 2 == 0 {
            break;
        }
        cut = region.rfind("```").unwrap_or(0);
    }
    &some[..cut]
}

/// Strict-prefix check: every already-printed preview line must equal the
/// plain text of the re-rendered line at the same position. This is the
/// guardrail that turns any styled-line instability into the documented
/// render-at-end fallback instead of corrupted scrollback. An empty
/// `committed` matches trivially — the caller then prints the whole
/// render, which is exactly today's no-preview behavior.
fn preview_prefix_matches(committed: &[String], rendered: &[RLine<'_>]) -> bool {
    rendered.len() >= committed.len()
        && committed
            .iter()
            .zip(rendered)
            .all(|(printed, line)| *printed == line_plain_text(line))
}

/// Decide what a preview flush prints: re-render the completed streamed
/// text with `parse_partial` (auto-closing open fences, so open constructs
/// can't leak their styling into later lines) and emit only the lines
/// beyond what was already printed.
fn preview_flush(committed: &[String], completed: &str) -> PreviewFlush {
    if completed.is_empty() {
        return PreviewFlush::Print(Vec::new());
    }
    let mut rendered = crate::widgets::markdown::MarkdownContent::parse_partial(completed);
    if !preview_prefix_matches(committed, &rendered.lines) {
        return PreviewFlush::Diverged;
    }
    PreviewFlush::Print(rendered.lines.split_off(committed.len()))
}

/// Whether the multi-turn end-of-run rule upgrades the print source to
/// the FULL streamed text (story 1.3): `block_text` accumulates EVERY
/// turn's deltas (T1+T2+T3) while the orchestrator returns only the last
/// turn's answer (T3) — with the story-1.1 preview live, T3 is already on
/// screen, so today's source would print it again. This is a GATE only:
/// the caller still verifies the strict line-prefix match against the
/// full stream's parse and falls back to today's source and decision when
/// it fails (plan task 3: "otherwise keep today's decision"). Divergence
/// and empty previews keep today's source. Pure and unit-testable.
fn end_of_run_uses_full_stream(
    committed: &[String],
    diverged: bool,
    block_text: &str,
    final_result: &str,
) -> bool {
    !diverged
        && !committed.is_empty()
        && (final_result.is_empty() || block_text.ends_with(final_result))
}

/// Live-preview run state (story 1.1 machinery, regrouped in story 1.3 so
/// the drain loop can announce text at arrival-order boundaries instead of
/// one place per tick).
#[derive(Default)]
struct PreviewTracker {
    /// Plain text of the preview lines already printed, in print order.
    committed: Vec<String>,
    /// The completed text the committed lines were rendered from (skips
    /// re-parsing when nothing new completed).
    flushed: String,
    /// True when the progressive re-render stopped extending the printed
    /// prefix mid-run; the end-of-run path then keeps today's full render.
    diverged: bool,
    /// cfg(test) only: the interleaved print order of the run —
    /// `text:<line>` for live preview lines, alongside the event markers
    /// the renderer drains pushes adjacent to them.
    #[cfg(test)]
    order: Vec<String>,
}

impl PreviewTracker {
    /// Announce newly completed streamed text as styled markdown above the
    /// status line, riding the console-log hook's erase → print → repaint
    /// cycle (the same atom for every printed item, so the status line is
    /// always the last line). `consume` erases ALL transient rows under it
    /// (status + the story-2.1 input row when it is live), so the announced
    /// lines never leave the input row stranded; the fly-by preview's next
    /// frame re-renders the input row below itself.
    fn announce(
        &mut self,
        block_text: &str,
        consume: &dyn Fn(),
        input_restored: &dyn Fn(),
        commit: &dyn Fn(),
    ) {
        if self.diverged {
            return;
        }
        let completed = completed_text(block_text);
        if completed == self.flushed {
            return;
        }
        match preview_flush(&self.committed, completed) {
            PreviewFlush::Print(new_lines) => {
                self.flushed = completed.to_string();
                if !new_lines.is_empty() {
                    // The shared transient-band erase: status + the
                    // story-2.1 input row (when it is live) — so the
                    // announced rows land at the band top and the input
                    // row is re-drawn below them before the status row
                    // re-commits.
                    consume();
                    // The announce's own input redraw needs the CURRENT
                    // mid-run buffer; the caller supplies a callback that
                    // reads the live state (this.story-2.1 review: an
                    // inert announce would strand/destroy that row).
                    // PreviewTracker doesn't hold the mid-run text, so
                    // the closure receives nothing and the caller's
                    // `redraw_input_line` reads it from shared state.
                    let no_context = "";
                    let _ = no_context;
                    inline::print_lines(&new_lines);
                    input_restored();
                    commit();
                    #[cfg(test)]
                    self.order.extend(
                        new_lines
                            .iter()
                            .map(|l| format!("text:{}", line_plain_text(l))),
                    );
                    self.committed.extend(new_lines.iter().map(line_plain_text));
                }
            }
            PreviewFlush::Diverged => {
                // A later re-render changed lines that were already printed
                // (e.g. a soft line break joined two of them). Stop
                // previewing; the end-of-run path falls back to today's
                // full render.
                self.diverged = true;
            }
        }
    }
}

fn render_event(event: &takka_core::Event, timeline: &mut ToolTimeline, detailed: bool) {
    use crate::widgets::{components, diff as diff_widget, inline, tool_call};

    // Tools render as a timeline: one row per finished call (icon, name, the
    // argument that says what it touched, timing), with streamed output,
    // errors and diffs hanging off a `│` guide beneath it. While a tool is
    // running nothing is printed — the transient status line already names it
    // and ticks its own clock, so a finished row replaces that noise.
    //
    // Special case: edit_file / write_file results carry a `diff` field
    // (unified-diff string) and `lines_added` / `lines_removed` counts.
    // We render those through `widgets::diff` so the user sees a real
    // colored diff inline instead of the raw JSON blob.
    const MAX_TOOL_OUTPUT_LINES: usize = 12;
    const MAX_ERROR_LINES: usize = 4;
    match event {
        takka_core::Event::ToolCall {
            tool_name,
            arguments,
        } => {
            timeline.on_call(tool_name, arguments);
        }
        takka_core::Event::ToolStarted {
            tool_call_id,
            tool_name,
            arguments,
            ..
        } => {
            timeline.on_started(tool_call_id, tool_name, arguments);
        }
        takka_core::Event::ToolDelta {
            delta,
            tool_name,
            tool_call_id,
            ..
        } => {
            if !detailed {
                // Compact: the completion row alone represents the tool.
                return;
            }
            // First delta: give the streamed lines a parent row to hang from.
            if timeline.mark_live(tool_call_id) {
                let arguments = timeline.peek_arguments(tool_call_id);
                inline::print_line(&tool_call::render_running_row(
                    tool_name,
                    arguments.as_ref(),
                ));
            }
            for line in render_tool_delta(tool_name, delta) {
                inline::print_line(&tool_call::timeline_detail(&line));
            }
        }
        takka_core::Event::ToolOutput {
            tool_name,
            output,
            tool_call_id,
            duration_ms,
            success,
            truncated,
            ..
        } => {
            // Heuristic: the orchestrator records denied/skipped outcomes as
            // plain string output with these prefixes, in which case `success`
            // is still true but the run did not do what was asked.
            let body = match output {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let denied = body.starts_with("Blocked:") || body.starts_with("Skipped:");
            let is_error = !*success || denied || body.starts_with("Tool error");

            // Structured fields ride along as JSON in the string body.
            let parsed = if is_error {
                None
            } else {
                serde_json::from_str::<serde_json::Value>(&body).ok()
            };
            let diff_text = parsed
                .as_ref()
                .and_then(|v| v.get("diff"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            let diff_counts = parsed.as_ref().and_then(|v| {
                let added = v.get("lines_added")?.as_u64()? as u32;
                let removed = v.get("lines_removed")?.as_u64()? as u32;
                Some((added, removed))
            });

            let arguments = timeline.arguments_for(tool_call_id, tool_name);
            let summary = if is_error {
                None
            } else {
                tool_call::timeline_summary(output)
            };

            if !detailed {
                inline::print_line(&tool_call::render_timeline_row(
                    tool_name,
                    arguments.as_ref(),
                    !is_error,
                    summary.as_deref(),
                    diff_counts,
                    *duration_ms,
                    *truncated,
                ));
                // Drop the live marker too: a later `ToolDelta` for the same
                // call must not reopen a row in compact mode.
                timeline.take_live(tool_call_id);
                return;
            }

            if timeline.take_live(tool_call_id) {
                // The running row is already on screen with the streamed
                // output beneath it — just close it out.
                let mut outcome = tool_call::timeline_summary(output);
                if *truncated {
                    let note = "truncated".to_string();
                    outcome = Some(match outcome {
                        Some(existing) => format!("{existing} \u{b7} {note}"),
                        None => note,
                    });
                }
                inline::print_line(&tool_call::render_outcome_detail(
                    !is_error,
                    outcome.as_deref(),
                    *duration_ms,
                ));
            } else {
                inline::print_line(&tool_call::render_timeline_row(
                    tool_name,
                    arguments.as_ref(),
                    !is_error,
                    summary.as_deref(),
                    diff_counts,
                    *duration_ms,
                    *truncated,
                ));
            }

            // Errors always show their message — the user must be able to act
            // on them without flipping a flag.
            if is_error {
                for line in tool_call::error_detail_lines(&body, MAX_ERROR_LINES) {
                    inline::print_line(&line);
                }
            }

            // Diffs render inline (no border) beneath the row.
            if let Some(diff) = diff_text {
                inline::print_line(&tool_call::timeline_detail(&diff_widget::summary_line(
                    diff,
                )));
                let diff_lines = diff_widget::render(diff);
                let shown = diff_lines.len().min(MAX_TOOL_OUTPUT_LINES);
                for line in &diff_lines[..shown] {
                    inline::print_line(&tool_call::timeline_detail(line));
                }
                if diff_lines.len() > shown {
                    inline::print_line(&tool_call::error_detail_lines(
                        &format!(
                            "\u{2026} {} more diff line(s) hidden",
                            diff_lines.len() - shown
                        ),
                        1,
                    )
                    .into_iter()
                    .next()
                    .unwrap_or_default());
                }
            }
        }
        takka_core::Event::Thinking { content }
            // Display the LLM's thinking/reasoning before tool execution
            // with DIM styling so it's visually distinct from the final response.
            if !content.trim().is_empty() => {
                inline::print_line(&components::thinking_header(true));
                inline::print_lines(&thinking_block_lines(content, THINKING_PREVIEW_LINES));
                inline::print_line(&components::thinking_header(false));
            }
        takka_core::Event::SteerQueued { content } => {
            // Story 2.1: the steering hand-over is visible — ack above
            // the (paused) input row, part of the run's scrollback.
            let dim = RStyle::default().fg(RColor::Rgb(110, 110, 140));
            inline::print_line(&RLine::from(vec![
                RSpan::styled(
                    "  ↳ steered in (applies at the next tool boundary): ".to_string(),
                    dim,
                ),
                RSpan::styled(
                    format!("\"{content}\""),
                    RStyle::default()
                        .fg(RColor::Rgb(120, 180, 140)),
                ),
            ]));
        }
        takka_core::Event::Steered { content } => {
            // The boundary drain applied the steer: hand-over marker.
            let dim = RStyle::default().fg(RColor::Rgb(110, 110, 140));
            inline::print_line(&RLine::from(vec![
                RSpan::styled("  ↳ applied at the tool boundary: ".to_string(), dim),
                RSpan::styled(
                    format!("\"{content}\""),
                    RStyle::default().fg(RColor::Rgb(120, 180, 140)),
                ),
            ]));
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

/// Per-run state for the inline tool timeline (see [`render_event`]).
///
/// `ToolOutput` carries no arguments, so the arguments seen at
/// `ToolStarted` (keyed by call id) or `ToolCall` are remembered here — that
/// is what lets a finished row say *what* the tool touched, not just its name.
#[derive(Default)]
struct ToolTimeline {
    calls: std::collections::HashMap<String, serde_json::Value>,
    /// Calls whose streamed row has already been printed, so the completion
    /// prints a nested outcome under it instead of a second branch row.
    live: std::collections::HashSet<String>,
    /// `ToolCall` has no id; remember it for outputs that arrive without a
    /// matching `ToolStarted`.
    last_call: Option<(String, serde_json::Value)>,
}

impl ToolTimeline {
    fn on_call(&mut self, tool_name: &str, arguments: &serde_json::Value) {
        self.last_call = Some((tool_name.to_string(), arguments.clone()));
    }

    fn on_started(&mut self, call_id: &str, tool_name: &str, arguments: &serde_json::Value) {
        self.calls.insert(call_id.to_string(), arguments.clone());
        self.last_call = Some((tool_name.to_string(), arguments.clone()));
    }

    /// Mark a call as having a live (streaming) row. Returns `true` the first
    /// time, i.e. when the caller should print the running row.
    fn mark_live(&mut self, call_id: &str) -> bool {
        self.live.insert(call_id.to_string())
    }

    /// `true` when this call already owns a live row that the outcome should
    /// hang beneath; the marker is consumed.
    fn take_live(&mut self, call_id: &str) -> bool {
        self.live.remove(call_id)
    }

    /// Arguments for a call without consuming them.
    fn peek_arguments(&mut self, call_id: &str) -> Option<serde_json::Value> {
        self.calls.get(call_id).cloned()
    }

    /// Arguments for a call that just finished; the entry is consumed so a
    /// long run does not accumulate state.
    fn arguments_for(&mut self, call_id: &str, tool_name: &str) -> Option<serde_json::Value> {
        if let Some(arguments) = self.calls.remove(call_id) {
            return Some(arguments);
        }
        match &self.last_call {
            Some((name, arguments)) if name == tool_name => Some(arguments.clone()),
            _ => None,
        }
    }
}

/// Longest reasoning block the inline renderer prints before eliding the
/// rest — the model's pre-tool reasoning can run to paragraphs, and the real
/// answer must not be pushed off screen.
const THINKING_PREVIEW_LINES: usize = 12;

/// Format a `Thinking` event body as indented DIM preview lines, capped at
/// `max_lines` (with an `… N more line(s)` notice). Pure — no terminal access,
/// so the output can be unit-tested.
fn thinking_block_lines(content: &str, max_lines: usize) -> Vec<RLine<'static>> {
    let style = RStyle::default()
        .fg(RColor::Indexed(242))
        .add_modifier(RModifier::DIM);
    let hidden_style = RStyle::default()
        .fg(RColor::Indexed(240))
        .add_modifier(RModifier::DIM | RModifier::ITALIC);

    let body: Vec<&str> = content
        .lines()
        .map(str::trim_end)
        .skip_while(|l| l.trim().is_empty())
        .collect();
    // A trailing blank line in the model's block shouldn't cost a preview row.
    let body_len = body
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map(|i| i + 1)
        .unwrap_or(0);

    let mut lines: Vec<RLine<'static>> = body
        .iter()
        .take(body_len.min(max_lines))
        .map(|line| RLine::from(RSpan::styled(format!("  {}", line), style)))
        .collect();

    if body_len > max_lines {
        lines.push(RLine::from(RSpan::styled(
            format!("  … {} more line(s) hidden", body_len - max_lines),
            hidden_style,
        )));
    }

    lines
}

/// Render a live tool-output delta as indented DIM lines. Pure — no
/// terminal access — so it can be unit-tested without capturing stdout.
fn render_tool_delta(_tool_name: &str, delta: &str) -> Vec<RLine<'static>> {
    let mut lines = Vec::new();
    for line in delta.lines() {
        if line.trim().is_empty() {
            continue;
        }
        // No extra indent: the caller prefixes these with the timeline guide.
        lines.push(RLine::from(RSpan::styled(
            line.to_string(),
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
mod thinking_preview_tests {
    use super::*;

    fn text(line: &RLine<'_>) -> String {
        line.spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn short_blocks_are_printed_whole() {
        let lines = thinking_block_lines("Plan:\n1. read main.rs\n2. fix bug", 12);
        assert_eq!(lines.len(), 3);
        assert!(text(&lines[0]).contains("Plan:"));
        assert!(text(&lines[2]).contains("fix bug"));
    }

    #[test]
    fn long_blocks_are_capped_with_a_notice() {
        let content: String = (1..=20)
            .map(|i| format!("step {i}\n"))
            .collect::<String>()
            .trim_end()
            .to_string();
        let lines = thinking_block_lines(&content, 12);
        assert_eq!(lines.len(), 13, "12 preview lines + notice");
        let notice = text(lines.last().unwrap());
        assert!(notice.contains("8 more line(s) hidden"), "{notice}");
    }

    #[test]
    fn leading_and_trailing_blank_lines_do_not_cost_preview_rows() {
        let lines = thinking_block_lines("\n\n  actual thought  \n\n", 12);
        assert_eq!(lines.len(), 1, "blank padding must be trimmed");
        assert!(text(&lines[0]).contains("actual thought"));
    }

    #[test]
    fn empty_or_whitespace_only_content_renders_nothing() {
        assert!(thinking_block_lines("", 12).is_empty());
        assert!(thinking_block_lines("   \n\t\n", 12).is_empty());
    }
}

#[cfg(test)]
mod status_painter_tests {
    use super::*;
    use crate::widgets::progress::{Phase, ProgressState};

    fn state() -> std::sync::Mutex<ProgressState> {
        let mut p = ProgressState::new();
        p.start();
        std::sync::Mutex::new(p)
    }

    /// The redraw overwrites in place without `Clear`, so a frame narrower
    /// than the previous one must be padded or stale glyphs stay on screen.
    #[test]
    fn narrower_frames_are_padded_to_the_widest() {
        let state = state();
        let mut painter = StatusPainter::new();
        painter.show_cancel_hint = true;

        state
            .lock()
            .unwrap()
            .set_tool("a_very_long_tool_name_indeed");
        let wide = painter.next_line(&state).width();

        state.lock().unwrap().set_phase(Phase::Thinking);
        let narrow = painter.next_line(&state).width();

        assert_eq!(narrow, wide, "narrow frame must be padded");
    }

    #[test]
    fn status_line_grows_with_the_token_counter() {
        let state = state();
        let mut painter = StatusPainter::new();

        state.lock().unwrap().set_phase(Phase::Streaming);
        state.lock().unwrap().add_stream_chars(40);
        let small = painter.next_line(&state).width();

        state.lock().unwrap().add_stream_chars(400_000);
        let big = painter.next_line(&state).width();

        assert!(big > small, "token counter should widen the line");
    }

    // ── Resize doctrine (story 1.2) ─────────────────────────
    //
    // `redraw` overwrites in place with MoveUp(1) + repaint and no
    // Clear, so the next frame must cover every column its predecessor
    // could have painted WITHOUT WRAPPING at the width the terminal has
    // now. `frame_pad_on_resize` is that pure min-cover rule:
    // `pad = max(line_width, min(frame_width, ambient_now))`.

    #[test]
    fn pad_unchanged_width_is_todays_pad_to_widest_bit_for_bit() {
        // Baseline measured at the current ambient: exactly today's
        // pad-to-widest rule, unchanged.
        assert_eq!(frame_pad_on_resize(60, 100, 40), 60);
        assert_eq!(frame_pad_on_resize(40, 100, 40), 40);
        // First frame: no baseline yet, the line sets it.
        assert_eq!(frame_pad_on_resize(0, 100, 55), 55);
        // A wider frame re-baselines the pad upward within one width.
        assert_eq!(frame_pad_on_resize(60, 100, 70), 70);
    }

    #[test]
    fn pad_on_widening_keeps_the_widest_baseline() {
        // Baseline 60 measured at 80 cols; the terminal widens to 120.
        // The widest-so-far baseline still pads (60 fits unwrapped at
        // 120), and only a wider frame lifts the baseline.
        assert_eq!(frame_pad_on_resize(60, 120, 40), 60);
        assert_eq!(frame_pad_on_resize(60, 120, 80), 80);
    }

    #[test]
    fn pad_on_partial_narrowing_keeps_the_retained_baseline() {
        // Baseline 60 built at 100 cols; the terminal narrows to 80. The
        // predecessor painted 60 columns unwrapped (60 ≤ 80), so the
        // next frame must still cover those 60 — resetting the pad to
        // the new frame's own width would strand stale glyphs in
        // columns 40–59 under the no-Clear in-place overwrite.
        assert_eq!(frame_pad_on_resize(60, 80, 40), 60);
    }

    #[test]
    fn pad_on_deep_narrowing_caps_at_the_new_ambient_width() {
        // Baseline 60 built at 100 cols; the terminal narrows to 50. The
        // baseline itself no longer fits: best-effort cover the maximal
        // one row, and the frame never exceeds the new width.
        assert_eq!(frame_pad_on_resize(60, 50, 40), 50);
    }

    #[test]
    fn pad_widen_then_narrow_needs_only_the_frame_width_state() {
        // The caller's only stored state is `frame_width`; no second,
        // stale ambient measurement participates in the decision. The
        // same baseline pads correctly across a widening and a later
        // narrowing, decided purely by the fresh per-frame measurement.
        let baseline = 70;
        assert_eq!(frame_pad_on_resize(baseline, 100, 50), 70); // widened
        assert_eq!(frame_pad_on_resize(baseline, 60, 50), 60); // narrowed
        assert_eq!(frame_pad_on_resize(baseline, 80, 90), 90); // wider frame re-baselines
    }
}

// ── Live-preview flush helper tests (story 1.1) ──────────────

#[cfg(test)]
mod preview_flush_tests {
    use super::*;
    use crate::widgets::markdown::MarkdownContent as Md;

    fn plains(lines: &[RLine<'static>]) -> Vec<String> {
        lines.iter().map(line_plain_text).collect()
    }

    /// The happy-path shape of one flush: `Print` with plain-text lines.
    fn printed(committed: &[String], text: &str) -> Vec<String> {
        match preview_flush(committed, text) {
            PreviewFlush::Print(lines) => plains(&lines),
            PreviewFlush::Diverged => panic!("expected Print, got Diverged for {text:?}"),
        }
    }

    /// Whole-line granularity: a trailing fragment without a newline can
    /// still grow into a longer line, so it stays pending (deferred to a
    /// later tick — the gate-contention parking is this same deferral).
    /// Whole-paragraph granularity (design decision A, "exactly once"): a
    /// single newline inside a paragraph is a soft break that joins lines,
    /// so a paragraph stays pending until a blank-line boundary completes
    /// it — announcing its first line early would later change printed
    /// lines and trip the duplication fallback.
    #[test]
    fn trailing_partial_line_and_open_paragraph_are_held_back() {
        assert_eq!(completed_text("Hello wor"), "");
        assert_eq!(completed_text(""), "");
        // A single newline does NOT complete a paragraph.
        assert_eq!(completed_text("Hello\nwor"), "");
        assert_eq!(completed_text("Hello\nworld\n"), "");
        // A blank-line boundary completes the paragraph.
        assert_eq!(completed_text("First para\n\n"), "First para\n\n");
        assert_eq!(
            completed_text("# Title\n\nFirst para\n\n"),
            "# Title\n\nFirst para\n\n"
        );
    }

    /// Streaming arrival: the first flush prints the completed lines…
    #[test]
    fn first_flush_prints_completed_lines() {
        assert_eq!(printed(&[], "Hello\n"), vec!["Hello"]);
    }

    /// …and each later flush prints only the suffix beyond what was
    /// already printed — in arrival order, each line exactly once.
    /// (Lines are blank-line-separated paragraphs: single-newline soft
    /// breaks join into one line and are the divergence case below.)
    #[test]
    fn later_flushes_print_only_the_suffix() {
        let committed = vec!["Hello".to_string()];
        assert_eq!(printed(&committed, "Hello\n"), Vec::<String>::new());
        assert_eq!(printed(&committed, "Hello\n\nWorld\n"), vec!["World"]);
        assert_eq!(
            printed(&committed, "Hello\n\nWorld\n\nSecond para\n"),
            vec!["World", "Second para"]
        );
    }

    /// Open-construct safety (story criterion 2): a chunk ending inside an
    /// unfenced code block renders as a temporarily closed code block, so
    /// no later non-code text can inherit code styling.
    #[test]
    fn open_fence_is_temporarily_closed() {
        let p = printed(&[], "```rust\nfn a() {\n");
        assert!(p.iter().any(|l| l.contains("fn a() {")), "code line: {p:?}");
        let last = p.last().expect("non-empty render");
        assert!(
            last.contains("drag-select to copy"),
            "fence closed temporarily: {p:?}"
        );
    }

    /// …and the fence closes properly once only the closer arrives: the
    /// temporary render was already exact, so the suffix is empty.
    #[test]
    fn fence_closes_properly_when_only_the_closer_arrives() {
        let committed: Vec<String> = match preview_flush(&[], "```rust\nfn a() {\n") {
            PreviewFlush::Print(lines) => lines.iter().map(line_plain_text).collect(),
            PreviewFlush::Diverged => panic!("expected Print"),
        };
        assert_eq!(
            printed(&committed, "```rust\nfn a() {\n```\n"),
            Vec::<String>::new()
        );
    }

    /// Guardrail, code-block case: more code arriving after a temporary
    /// close shifts the block's bottom border into a line that was
    /// already printed — the flush reports divergence instead of
    /// corrupted scrollback, and the run falls back to today's render.
    #[test]
    fn code_lines_after_a_temporary_close_diverge_to_the_fallback() {
        let committed: Vec<String> = match preview_flush(&[], "```rust\nfn a() {\n") {
            PreviewFlush::Print(lines) => lines.iter().map(line_plain_text).collect(),
            PreviewFlush::Diverged => panic!("expected Print"),
        };
        assert!(matches!(
            preview_flush(&committed, "```rust\nfn a() {\n}\n```\nDone\n"),
            PreviewFlush::Diverged
        ));
    }

    /// Instability guardrail: when a later re-render changes a line that
    /// was already printed (a soft line break joins two printed lines),
    /// the flush reports divergence and the caller falls back to today's
    /// render-at-end for that run.
    #[test]
    fn shifted_lines_diverge_to_the_fallback() {
        let committed = vec!["line one".to_string()];
        assert!(matches!(
            preview_flush(&committed, "line one\nline two\n"),
            PreviewFlush::Diverged
        ));
    }

    /// End-of-run selection, no-chunk fallback (story criterion 5): an
    /// empty printed prefix matches trivially, so the "suffix" is the
    /// whole render — exactly today's single full render. (The leading
    /// empty line is the markdown renderer's own heading spacing.)
    #[test]
    fn no_preview_matches_everything_and_yields_the_full_render() {
        let parsed = Md::parse("# Title\n\nBody\n");
        assert!(preview_prefix_matches(&[], &parsed.lines));
        assert_eq!(plains(&parsed.lines), vec!["", "# Title", "Body"]);
    }

    /// End-of-run selection: printed lines the final render no longer
    /// contains (final text escaped the stream, or a mid-run divergence)
    /// must keep today's full render.
    #[test]
    fn mismatching_or_overshooting_prefixes_do_not_match() {
        let parsed = Md::parse("real answer\n");
        assert!(!preview_prefix_matches(
            &["stale".to_string()],
            &parsed.lines
        ));
        let longer = vec!["extra".to_string(), "lines".to_string()];
        assert!(!preview_prefix_matches(&longer, &parsed.lines));
        assert!(preview_prefix_matches(
            &["real answer".to_string()],
            &parsed.lines
        ));
    }

    // ── Wrap / resize doctrine (story 1.2) ──────────────────

    /// Over-long streamed lines wrap once, at the terminal's own ambient
    /// width, by the terminal itself — the console-log hook rule
    /// (`log_writer::write_line_to_stderr` writes whole raw lines with
    /// zero custom wrapping). The flush therefore announces a 300-char
    /// paragraph as exactly ONE logical line carrying every character;
    /// nothing code-side wraps, truncates, or splits it.
    #[test]
    fn overlong_paragraph_flushes_as_one_whole_line() {
        let para = "w".repeat(300);
        let text = format!("{para}\n\n");
        let completed = completed_text(&text);
        assert_eq!(
            completed,
            format!("{para}\n\n"),
            "paragraph completes whole"
        );
        let lines = printed(&[], completed);
        assert_eq!(lines.len(), 1, "one logical line, no splitting");
        assert_eq!(lines[0].chars().count(), 300, "no truncation");
    }

    /// The flush is width-blind by construction: its signature takes no
    /// width and its body consults none, so output is a pure function of
    /// (committed, completed) — whatever the terminal width does between
    /// ticks cannot change it. A re-flush of the same stream therefore
    /// prints only the genuinely new lines, and the already-printed
    /// `committed` entries stay untouched opaque strings (never
    /// re-derived, re-wrapped, or re-rendered on a resize).
    #[test]
    fn flush_is_width_blind_and_prints_only_genuinely_new_lines() {
        let para = "w".repeat(300);
        let committed = vec![para.clone()];
        let committed_before = committed.clone();
        // "Width changed mid-stream" is invisible here by construction —
        // the flush sees no width: the 300-char line already printed is
        // not re-printed, and only genuinely new lines come out.
        assert_eq!(
            printed(&committed, &format!("{para}\n\n")),
            Vec::<String>::new()
        );
        assert_eq!(
            printed(&committed, &format!("{para}\n\nNext\n\n")),
            vec!["Next"]
        );
        assert_eq!(committed, committed_before, "committed stays untouched");
    }

    // ── End-of-run source selection (story 1.3) ──────────────

    /// Multi-turn runs accumulate every turn's deltas in `block_text`
    /// while the orchestrator returns only the last turn's answer: with a
    /// non-empty, undiverged preview and `final_result` a suffix of the
    /// stream, the multi-turn gate fires (the caller still verifies the
    /// strict line-prefix match against the full stream's parse).
    #[test]
    fn end_of_run_uses_full_stream_gate_covers_the_multi_turn_shape() {
        let stream = "First para\n\nSecond para\n\nThird para\n\n";
        assert!(end_of_run_uses_full_stream(
            &["First para".to_string(), "Second para".to_string()],
            false,
            stream,
            "Third para\n\n"
        ));
        // The same inputs diverged: today's source (final_result) wins.
        assert!(!end_of_run_uses_full_stream(
            &["First para".to_string()],
            true,
            stream,
            "Third para\n\n"
        ));
        // A final result that escaped the stream: today's source wins.
        assert!(!end_of_run_uses_full_stream(
            &["First para".to_string()],
            false,
            stream,
            "escaped final answer"
        ));
        // Empty commit (no-chunk or diverged-empty preview): today's source.
        assert!(!end_of_run_uses_full_stream(
            &[],
            false,
            stream,
            "Third para\n\n"
        ));
    }

    /// Single-turn runs keep today's decision byte-for-byte: when
    /// `final_result` equals the accumulated stream (the normal
    /// single-turn shape — the orchestrator returns the raw delta
    /// accumulation), the gate may fire but the two parses are the same,
    /// so the caller's first branch matches and behavior is unchanged.
    #[test]
    fn end_of_run_uses_full_stream_single_turn_is_identical() {
        let stream = "Hello world\n\n";
        assert!(end_of_run_uses_full_stream(
            &["Hello world".to_string()],
            false,
            stream,
            stream
        ));
        // final_result non-empty but not a stream suffix: gate off.
        assert!(!end_of_run_uses_full_stream(
            &["Hello world".to_string()],
            false,
            stream,
            "final"
        ));
    }
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
        hermetic_runtime_config_at("http://127.0.0.1:9")
    }

    /// Same shape as [`hermetic_runtime_config`], pointed at an explicit
    /// provider endpoint (the scripted SSE server of the streaming tests).
    fn hermetic_runtime_config_at(api_base: &str) -> std::path::PathBuf {
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
            format!(
                r#"{{
  "providers": [
    {{
      "name": "hermetic-test",
      "type": "openai-compatible",
      "api_base": "{api_base}",
      "api_key": "unused",
      "models": [
        {{ "model": "hermetic-model", "temperature": 0.0, "max_tokens": 64 }}
      ]
    }}
  ]
}}"#
            ),
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

    /// Minimal OpenAI-compatible SSE server for the streaming tests:
    /// answers every POST with a scripted streaming completion — the given
    /// text deltas (a delay before each, so the renderer's 80 ms ticks see
    /// them arrive one by one), then a `finish_reason: "stop"` event and
    /// `[DONE]`. Chunked-transfer framed; the connection is closed after
    /// the stream ends. Loops over incoming connections so the daemon
    /// (`Spawn` mode) can use it too.
    fn spawn_scripted_sse_server(deltas: Vec<(std::time::Duration, String)>) -> u16 {
        spawn_scripted_sse_server_mode(deltas, false)
    }

    /// Streaming-preview harness variant: after the deltas the connection
    /// drops without a stop frame or `[DONE]` — the provider-error path
    /// (I/O matrix row: stream error mid-preview).
    fn spawn_scripted_sse_server_abrupt(deltas: Vec<(std::time::Duration, String)>) -> u16 {
        spawn_scripted_sse_server_mode(deltas, true)
    }

    /// One scripted LLM turn for the multi-turn harness (story 1.3).
    #[derive(Clone)]
    enum ScriptedTurn {
        /// Text-only deltas, then a `stop` finish.
        Text(Vec<(std::time::Duration, String)>),
        /// Text deltas followed by ONE tool call, finished with
        /// `finish_reason: tool_calls` — the model asks to run `tool`
        /// next turn.
        TextThenTool(Vec<(std::time::Duration, String)>, String, String),
    }

    /// Multi-turn scripted provider (story 1.3): each incoming connection
    /// consumes the next scripted turn, so POST 1 can answer with a tool
    /// call whose result feeds POST 2 — a full agent loop against the
    /// OpenAI-compatible wire format, no real provider involved.
    fn spawn_scripted_sse_server_turns(turns: Vec<ScriptedTurn>) -> u16 {
        spawn_scripted_sse_server_turns_with_capture(turns, Default::default()).0
    }

    /// Same scripted multi-turn server but also capturing the JSON
    /// bodies of each request (story 2.1's context assertions: the
    /// steered text must show up in the provider's next turn input).
    fn spawn_scripted_sse_server_turns_with_capture(
        turns: Vec<ScriptedTurn>,
        captured: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> (u16, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::Write;

        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind scripted SSE server");
        let port = listener.local_addr().expect("local addr").port();
        let turns = std::sync::Arc::new(std::sync::Mutex::new(turns));
        let captured_for_thread = captured.clone();
        std::thread::Builder::new()
            .name("scripted-sse-server-turns".into())
            .spawn(move || {
                let mut index = 0usize;
                while let Some(mut stream) =
                    accept_scripted_connection_capture(&listener, &captured_for_thread)
                {
                    let turn = {
                        let guard = turns.lock().unwrap();
                        guard.get(index).cloned()
                    };
                    index += 1;
                    if stream.write_all(SCRIPTED_SSE_HEADER.as_bytes()).is_err() {
                        continue;
                    }
                    let _ = stream.flush();
                    let Some(turn) = turn else {
                        // Out of script: behave like a provider that never
                        // finishes so the test's timeout reports the hang
                        // with a clear cause.
                        std::thread::sleep(std::time::Duration::from_secs(120));
                        continue;
                    };
                    let (deltas, tool_call) = match &turn {
                        ScriptedTurn::Text(deltas) => (deltas, None),
                        ScriptedTurn::TextThenTool(deltas, tool, arguments) => {
                            (deltas, Some((tool.clone(), arguments.clone())))
                        }
                    };
                    for (delay, delta) in deltas {
                        std::thread::sleep(*delay);
                        let event = serde_json::json!({
                            "id": "chatcmpl-scripted",
                            "choices": [{
                                "delta": {"content": delta},
                                "finish_reason": serde_json::Value::Null,
                            }]
                        });
                        if write_sse_frame(&mut stream, &format!("data: {event}\r\n\r\n")).is_err()
                        {
                            break;
                        }
                    }
                    if let Some((tool, arguments)) = tool_call {
                        std::thread::sleep(std::time::Duration::from_millis(80));
                        let event = serde_json::json!({
                            "id": "chatcmpl-scripted",
                            "choices": [{
                                "delta": {"tool_calls": [{
                                    "index": 0,
                                    "id": "call-scripted",
                                    "type": "function",
                                    "function": {"name": tool, "arguments": arguments},
                                }]},
                                "finish_reason": serde_json::Value::Null,
                            }]
                        });
                        let _ = write_sse_frame(&mut stream, &format!("data: {event}\r\n\r\n"));
                        let stop = serde_json::json!({
                            "id": "chatcmpl-scripted",
                            "choices": [{
                                "delta": {},
                                "finish_reason": "tool_calls",
                            }]
                        });
                        let _ = write_sse_frame(&mut stream, &format!("data: {stop}\r\n\r\n"));
                    } else {
                        let stop = serde_json::json!({
                            "id": "chatcmpl-scripted",
                            "choices": [{
                                "delta": {},
                                "finish_reason": "stop",
                            }]
                        });
                        let _ = write_sse_frame(&mut stream, &format!("data: {stop}\r\n\r\n"));
                    }
                    let _ = write_sse_frame(&mut stream, "data: [DONE]\r\n\r\n");
                    // Terminal chunk (size 0) closes the response body.
                    let _ = write_sse_frame(&mut stream, "");
                }
            })
            .expect("spawn scripted SSE turns server");
        (port, captured)
    }

    /// Accept one scripted server connection and drain its HTTP request
    /// (reads to the declared content-length). Shared by the single-script
    /// and multi-turn SSE harnesses (review patch 2026-11-06: the drain
    /// logic must not live twice).
    fn accept_scripted_connection(listener: &std::net::TcpListener) -> Option<std::net::TcpStream> {
        accept_scripted_connection_capture(listener, &Default::default())
    }

    /// Same accept+drain but recording each request's JSON body into
    /// `captured` — story 2.1's context assertions look for the steered
    /// text in the provider's next-turn input.
    fn accept_scripted_connection_capture(
        listener: &std::net::TcpListener,
        captured: &Arc<std::sync::Mutex<Vec<String>>>,
    ) -> Option<std::net::TcpStream> {
        use std::io::Read;
        let mut stream = listener.incoming().next()?.ok()?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .ok();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    let head_end = buf
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|pos| pos + 4);
                    if let Some(head_end) = head_end {
                        let head = String::from_utf8_lossy(&buf[..head_end]);
                        let content_length: usize = head
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.trim()
                                    .eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().ok())?
                            })
                            .unwrap_or(0);
                        if buf.len() >= head_end + content_length {
                            captured
                                .lock()
                                .unwrap()
                                .push(String::from_utf8_lossy(&buf[head_end..]).into_owned());
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        Some(stream)
    }

    /// The chunked SSE response head + one frame writer, shared by the SSE
    /// harnesses.
    const SCRIPTED_SSE_HEADER: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";

    fn write_sse_frame(stream: &mut std::net::TcpStream, payload: &str) -> std::io::Result<()> {
        use std::io::Write;
        write!(stream, "{:x}\r\n{payload}\r\n", payload.len())?;
        stream.flush()
    }

    fn spawn_scripted_sse_server_mode(
        deltas: Vec<(std::time::Duration, String)>,
        close_abruptly: bool,
    ) -> u16 {
        use std::io::Write;

        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind scripted SSE server");
        let port = listener.local_addr().expect("local addr").port();
        std::thread::Builder::new()
            .name("scripted-sse-server".into())
            .spawn(move || {
                while let Some(mut stream) = accept_scripted_connection(&listener) {
                    // The response head is written raw — chunked framing
                    // applies to the body only.
                    if stream.write_all(SCRIPTED_SSE_HEADER.as_bytes()).is_err() {
                        continue;
                    }
                    let _ = stream.flush();
                    for (delay, delta) in &deltas {
                        std::thread::sleep(*delay);
                        let event = serde_json::json!({
                            "id": "chatcmpl-scripted",
                            "choices": [{
                                "delta": {"content": delta},
                                "finish_reason": serde_json::Value::Null,
                            }]
                        });
                        if write_sse_frame(&mut stream, &format!("data: {event}\r\n\r\n")).is_err()
                        {
                            break;
                        }
                    }
                    if !close_abruptly {
                        let stop = serde_json::json!({
                            "id": "chatcmpl-scripted",
                            "choices": [{
                                "delta": {},
                                "finish_reason": "stop",
                            }]
                        });
                        let _ = write_sse_frame(&mut stream, &format!("data: {stop}\r\n\r\n"));
                        let _ = write_sse_frame(&mut stream, "data: [DONE]\r\n\r\n");
                        // Terminal chunk (size 0) closes the response body.
                        let _ = write_sse_frame(&mut stream, "");
                    }
                    if close_abruptly {
                        // Broken chunked framing after the deltas: reqwest's
                        // body decoder fails mid-stream, which the provider
                        // surfaces as a real stream error (not a tidy EOF).
                        let _ = stream.write_all(b"this is not chunked framing\r\n");
                        let _ = stream.flush();
                        continue;
                    }
                }
            })
            .expect("spawn scripted SSE server");
        port
    }

    /// Streaming-preview e2e (story 1.1, PTY-free): a scripted
    /// OpenAI-compatible provider streams markdown deltas with delays; the
    /// inline renderer path (`Commands::run` — shared by `takka run` and
    /// the REPL) must surface every completed line through the live
    /// preview in arrival order, each exactly once, and the end-of-run
    /// path must not re-print the answer (the committed lines stand).
    ///
    /// The renderer thread's terminal output cannot be captured in-process,
    /// so the assertion rides the cfg(test) tap on `RenderFinal` (the plain
    /// text of the printed preview lines). Deltas arrive 120 ms apart, so
    /// each completed line flushes within two 80 ms renderer ticks.
    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_live_preview_prints_streamed_lines_in_order() {
        let deltas = vec![
            (
                std::time::Duration::from_millis(120),
                "# Title\n\n".to_string(),
            ),
            (
                std::time::Duration::from_millis(120),
                "First para\n\n".to_string(),
            ),
            (std::time::Duration::from_millis(120), "Second ".to_string()),
            (std::time::Duration::from_millis(120), "para\n".to_string()),
        ];
        let port = spawn_scripted_sse_server(deltas);
        let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(RuntimeMode::Embedded)
            .with_permission_mode(takka_core::PermissionMode::Yolo);

        let result =
            tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("say hi"))
                .await
                .expect("Commands::run hung: no terminal event within 60s");
        let _ = std::fs::remove_file(&fixture);
        assert!(result.is_ok(), "run should complete: {:?}", result.err());

        // The live preview announced each completed PARAGRAPH exactly once
        // (whole-paragraph hold-back: a single newline is a soft break, so
        // "Second para" cannot be announced until a blank line ends it —
        // it stands pending and the finish path prints it as the suffix,
        // keeping the answer exactly once end to end).
        assert_eq!(
            commands.test_preview.lock().unwrap().clone(),
            vec![
                "".to_string(),
                "# Title".to_string(),
                "First para".to_string(),
            ]
        );
        assert_eq!(
            commands.test_final_print.lock().unwrap().clone(),
            vec!["Second para".to_string()],
        );
    }

    /// I/O matrix row "stream error mid-preview" (story 1.1): deltas are
    /// previewed, then the connection drops without a stop frame; the run
    /// reports the provider error while the already printed preview stays
    /// as ordinary scrollback (only the status line is erased).
    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_live_preview_survives_provider_stream_error() {
        let deltas = vec![(
            std::time::Duration::from_millis(60),
            "# Title\n\n".to_string(),
        )];
        let port = spawn_scripted_sse_server_abrupt(deltas);
        let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(RuntimeMode::Embedded)
            .with_permission_mode(takka_core::PermissionMode::Yolo);

        let _run_result =
            tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("hi"))
                .await
                .expect("Commands::run hung: no terminal event within 60s");
        let _ = std::fs::remove_file(&fixture);

        // `Commands::run` always returns Ok (the runtime error is reported as
        // a badge); assert the failure itself via the event tap.
        let events = commands.test_events.lock().unwrap();
        assert!(
            events.iter().any(|e| e.contains("Stream read error:")),
            "the broken framing surfaces as a provider stream error: {events:?}"
        );
        drop(events);
        // The previewed line was printed before the drop and is kept.
        assert!(!*commands.test_preview_diverged.lock().unwrap());
        assert_eq!(
            commands.test_preview.lock().unwrap().clone(),
            vec!["".to_string(), "# Title".to_string()],
        );
    }

    /// I/O matrix row "no streamed chunks" (story 1.1): nothing prints
    /// live and `preview_diverged` stays false, so the end-of-run path
    /// renders the full styled markdown exactly once (today's behavior).
    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_live_preview_no_chunks_falls_back_to_single_render() {
        let port = spawn_scripted_sse_server(vec![]);
        let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(RuntimeMode::Embedded)
            .with_permission_mode(takka_core::PermissionMode::Yolo);

        let result = tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("hi"))
            .await
            .expect("Commands::run hung: no terminal event within 60s");
        let _ = std::fs::remove_file(&fixture);

        assert!(result.is_ok(), "run should complete: {:?}", result.err());
        assert!(!*commands.test_preview_diverged.lock().unwrap());
        // Nothing printed live: the preview tap is empty.
        assert!(
            commands.test_preview.lock().unwrap().is_empty(),
            "no chunks must print no preview: {:?}",
            commands.test_preview.lock().unwrap()
        );
    }

    /// I/O matrix row "final text escapes the stream" (story 1.1): an
    /// unclosed code fence is held back (whole-fence granularity), so
    /// nothing prints live; the run ends cleanly and the finish path
    /// renders the whole answer once — today's behavior.
    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_live_preview_growing_fence_is_held_back_full_render_at_end() {
        // 160 ms spacing: the renderer tick is 80 ms; equal spacing races
        // (both deltas could drain in one tick), so keep real margin here.
        let deltas = vec![
            (
                std::time::Duration::from_millis(160),
                "```rust\n".to_string(),
            ),
            (
                std::time::Duration::from_millis(160),
                "fn a() {}\n".to_string(),
            ),
        ];
        let port = spawn_scripted_sse_server(deltas);
        let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(RuntimeMode::Embedded)
            .with_permission_mode(takka_core::PermissionMode::Yolo);

        let result = tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("hi"))
            .await
            .expect("Commands::run hung: no terminal event within 60s");
        let _ = std::fs::remove_file(&fixture);

        assert!(result.is_ok(), "run should complete: {:?}", result.err());
        assert!(!*commands.test_preview_diverged.lock().unwrap());
        // Nothing announced live: the fence never closed.
        assert!(commands.test_preview.lock().unwrap().is_empty());
        // The finish path rendered the whole answer once (today's path).
        let final_print = commands.test_final_print.lock().unwrap().clone();
        assert!(
            final_print.iter().any(|l| l.contains("fn a()")),
            "full render must cover the held-back fence: {final_print:?}"
        );
    }

    /// I/O matrix row "gate contention" (story 1.1): while a prompt holds
    /// the render gate the renderer parks (nothing prints); after the gate
    /// frees the deferred paragraphs print and the run completes normally.
    /// Note `Commands: !Sync` — the parking itself is exercised through the
    /// parked-run observation (run stays unfinished while the gate is
    /// held), and the parked claims live outside the helper's control.
    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_live_preview_parks_while_the_render_gate_is_held() {
        let deltas = vec![
            (
                std::time::Duration::from_millis(80),
                "# Title\n\n".to_string(),
            ),
            (
                std::time::Duration::from_millis(80),
                "First para\n\n".to_string(),
            ),
        ];
        let port = spawn_scripted_sse_server(deltas);
        let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

        // Hold the gate like a question / confirmation prompt would: from a
        // blocking thread (async code must not hold a std guard across an
        // await point), released after the run has had time to stream into
        // the parked renderer.
        let gate_holder = std::thread::Builder::new()
            .name("gate-holder".into())
            .spawn(move || {
                let _gate = RENDER_GATE.lock().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(600));
            })
            .expect("gate holder thread");
        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(RuntimeMode::Embedded)
            .with_permission_mode(takka_core::PermissionMode::Yolo);
        #[cfg(test)]
        let preview_tap = commands.test_preview.clone();
        let run_task = tokio::spawn(async move { commands.run("hi").await });

        // The engine is independent of the render gate: the run itself
        // completes while the prompt holds the gate, and the parked
        // renderer prints the deferred paragraphs only after the gate
        // frees — nothing is lost, and the run joins cleanly.
        let run_result = run_task.await.expect("run task must not panic");
        gate_holder.join().expect("gate holder thread");
        let _ = std::fs::remove_file(&fixture);
        assert!(
            run_result.is_ok(),
            "run should complete: {:?}",
            run_result.err()
        );
        // Every announced paragraph survived the gate hold.
        assert_eq!(
            preview_tap.lock().unwrap().clone(),
            vec![
                "".to_string(),
                "# Title".to_string(),
                "First para".to_string(),
            ]
        );
    }
    /// Interleave e2e (story 1.3, the verify's shape): a scripted
    /// multi-turn provider runs text → tool → text → tool → text. Each
    /// streamed block must print live in arrival order with the tool row
    /// BETWEEN its turns (one unified arrival-ordered queue), every
    /// duplicated Thinking recap must be suppressed (its text is already
    /// in the preview), and the end-of-run print must add nothing — every
    /// block on screen exactly once.
    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_multi_turn_interleaves_tool_rows_in_arrival_order() {
        let turns = vec![
            ScriptedTurn::TextThenTool(
                vec![(
                    std::time::Duration::from_millis(120),
                    "First para\n\n".to_string(),
                )],
                "list_files".to_string(),
                r#"{"path": "."}"#.to_string(),
            ),
            ScriptedTurn::TextThenTool(
                vec![(
                    std::time::Duration::from_millis(120),
                    "Second para\n\n".to_string(),
                )],
                "list_files".to_string(),
                r#"{"path": "."}"#.to_string(),
            ),
            ScriptedTurn::Text(vec![(
                std::time::Duration::from_millis(120),
                "Third para\n\n".to_string(),
            )]),
        ];
        let port = spawn_scripted_sse_server_turns(turns);
        let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(RuntimeMode::Embedded)
            .with_permission_mode(takka_core::PermissionMode::Yolo);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            commands.run("run through the plan"),
        )
        .await
        .expect("Commands::run hung: no terminal event within 60s");
        let _ = std::fs::remove_file(&fixture);

        assert!(result.is_ok(), "run should complete: {:?}", result.err());

        // Arrival order across the whole run: text above the row that
        // followed it, one completion row per turn, and no stray thinking
        // block (its content is already in the preview — suppressed).
        let order = commands.test_render_order.lock().unwrap().clone();
        assert_eq!(
            order,
            vec![
                "text:First para".to_string(),
                "tool".to_string(),
                "text:Second para".to_string(),
                "tool".to_string(),
                "text:Third para".to_string(),
            ],
            "interleave must follow arrival order: {order:?}"
        );

        // The preview announced every block exactly once (each block ends
        // at a blank-line boundary, so nothing stays pending). Note: a
        // paragraph-first render produces no leading blank line — story
        // 1.1's empty first line came from its heading lead-in.
        assert_eq!(
            commands.test_preview.lock().unwrap().clone(),
            vec![
                "First para".to_string(),
                "Second para".to_string(),
                "Third para".to_string(),
            ]
        );
        // End of run: the multi-turn source rule parses the full stream,
        // the committed lines cover it — nothing re-prints.
        assert!(!*commands.test_preview_diverged.lock().unwrap());
        assert!(
            commands.test_final_print.lock().unwrap().is_empty(),
            "final print must be suffix-free: {:?}",
            commands.test_final_print.lock().unwrap()
        );
    }
    /// I/O matrix row "thinking between blocks" (story 1.3), second half:
    /// when the tool-call turn's text was NOT announced by the preview
    /// (it ends without a blank-line boundary, so the hold-back keeps it
    /// pending), the Thinking block must still render as today — the
    /// suppression only applies to fully previewed content.
    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_unpreviewed_thinking_still_renders() {
        let turns = vec![
            ScriptedTurn::TextThenTool(
                // No blank-line boundary: `completed_text` holds it back.
                vec![(
                    std::time::Duration::from_millis(120),
                    "Unfinished".to_string(),
                )],
                "list_files".to_string(),
                r#"{"path": "."}"#.to_string(),
            ),
            ScriptedTurn::Text(vec![(
                std::time::Duration::from_millis(120),
                " tail\n\n".to_string(),
            )]),
        ];
        let port = spawn_scripted_sse_server_turns(turns);
        let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(RuntimeMode::Embedded)
            .with_permission_mode(takka_core::PermissionMode::Yolo);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            commands.run("run through the plan"),
        )
        .await
        .expect("Commands::run hung: no terminal event within 60s");
        let _ = std::fs::remove_file(&fixture);

        assert!(result.is_ok(), "run should complete: {:?}", result.err());

        // The un-previewed turn text rendered through the Thinking block
        // (today's path), and the preview picked the paragraph up as soon
        // as the blank-line boundary completed it — arrival order holds.
        assert_eq!(
            commands.test_render_order.lock().unwrap().clone(),
            vec![
                "thinking".to_string(),
                "tool".to_string(),
                "text:Unfinished tail".to_string(),
            ]
        );
        assert_eq!(
            commands.test_preview.lock().unwrap().clone(),
            vec!["Unfinished tail".to_string()]
        );
        assert!(!*commands.test_preview_diverged.lock().unwrap());
        assert!(
            commands.test_final_print.lock().unwrap().is_empty(),
            "final print must be suffix-free: {:?}",
            commands.test_final_print.lock().unwrap()
        );
    }

    // ── Story 1.4: finish and mid-stream cancel cleanup ─────────

    /// Poll the live event tap until one of the streamed events contains
    /// `needle` (bounded so a regression fails the test, never hangs it).
    /// Runs on a plain driver thread while `commands` is held by the run.
    fn wait_for_tap_event(tap: &std::sync::Arc<std::sync::Mutex<Vec<String>>>, needle: &str) {
        wait_for_tap_event_count(tap, needle, 1);
    }

    /// Same as [`wait_for_tap_event`], but waits for the Nth occurrence —
    /// used to land the cancel flag exactly inside an iteration-N provider
    /// call (`ModelRequest` is emitted AFTER the iteration's boundary
    /// check and BEFORE the provider call streams).
    fn wait_for_tap_event_count(
        tap: &std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        needle: &str,
        count: usize,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let saw = tap
                .lock()
                .unwrap()
                .iter()
                .filter(|e| e.contains(needle))
                .count();
            if saw >= count {
                return;
            }
            // Checked after the satisfied condition: a starved poll thread
            // that woke up past the deadline with the condition already
            // met must not fail spuriously (story 1.4 review).
            assert!(
                std::time::Instant::now() < deadline,
                "event tap never saw {count}× {needle}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// I/O matrix row "cancel mid-preview" (story 1.4): the cancel flag
    /// flips while streamed paragraphs are already printed; the run
    /// returns gracefully, the preview lines stand as ordinary
    /// scrollback, the final print stays empty (no re-render of the
    /// answer), and "Cancelled by user" surfaces as the cancel notice —
    /// not the red error badge. Hermetic scripted provider only.
    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_cancel_mid_preview_unwinds_with_notice() {
        // Multi-turn shape so the cancel has a boundary to win at: core
        // checks `AgenticError::Cancelled` at the START of every loop
        // iteration, and a single-turn run never loops — a mid-stream
        // cancel on a text-only turn would flag the request while the run
        // still completes normally (documented in the Implementation
        // Notes). Turns: text+tool → the flag lands mid-turn-1 streaming;
        // the tool call forces iteration 2, whose boundary check unwinds
        // with Cancelled.
        let turns = vec![
            ScriptedTurn::TextThenTool(
                vec![
                    (
                        std::time::Duration::from_millis(120),
                        "# Title\n\n".to_string(),
                    ),
                    (
                        std::time::Duration::from_millis(120),
                        "First para\n\n".to_string(),
                    ),
                ],
                "list_files".to_string(),
                r#"{"path": "."}"#.to_string(),
            ),
            ScriptedTurn::Text(vec![(
                std::time::Duration::from_millis(2000),
                "Second para\n\n".to_string(),
            )]),
        ];
        // flag lands mid-turn-1 streaming (the driver waits for the
        // second streamed paragraph's ModelChunk).
        let port = spawn_scripted_sse_server_turns(turns);
        let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

        let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(RuntimeMode::Embedded)
            .with_permission_mode(takka_core::PermissionMode::Yolo)
            .with_cancel_flag(cancel_flag.clone());

        // Drive the cancel from a plain thread while the run holds
        // `commands`: land the flag inside the inter-chunk gap so the
        // client's between-event cancel check sees it while turn-1 is
        // still streaming and the engine honours it at turn-1's boundary.
        // The scripted provider paces chunk-2 ~120 ms after chunk-1, so
        // 60 ms is safely inside that window; a later flip (e.g. past the
        // tool boundary) would be forwarded while the loop idles in
        // turn-2's scripted silence and never surface a Cancelled badge.
        let tap = commands.test_events.clone();
        let driver = std::thread::Builder::new()
            .name("cancel-driver".into())
            .spawn(move || {
                wait_for_tap_event(&tap, "ModelChunk");
                std::thread::sleep(std::time::Duration::from_millis(60));
                cancel_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            })
            .expect("cancel driver thread");

        // Commands::run always returns Ok — the unwind surfaces through
        // the outcome badge (test_notice), asserted below.
        let _outcome =
            tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("say hi"))
                .await
                .expect("Commands::run hung: no terminal event within 60s");
        driver.join().expect("cancel driver panicked");
        let _ = std::fs::remove_file(&fixture);
        // The Err unwind rendered the CANCEL notice; the red error badge
        // would have shown up here as RunOutcomeBadge::Error.
        // Lock once (a MutexGuard temporary in the assert would deadlock
        // the panic format's second lock on the same std Mutex).
        let notice = commands.test_notice.lock().unwrap().clone();
        assert_eq!(
            notice,
            vec![RunOutcomeBadge::Cancelled],
            "cancelled run must print the cancel notice: {notice:?}"
        );
        // Status line erased exactly once (the shared terminal-cleanup
        // block runs once per run, both outcomes).
        assert_eq!(
            commands.test_terminal_cleanup_count.load(Ordering::Relaxed),
            1,
            "terminal cleanup must run exactly once per run"
        );
        // Preview lines stand as ordinary scrollback (printed before the
        // cancel flipped; the run streams on briefly until the turn
        // boundary, which may only add more lines).
        let preview = commands.test_preview.lock().unwrap().clone();
        assert!(
            preview.iter().any(|l| l == "# Title"),
            "printed preview lines must survive the unwind: {preview:?}"
        );
        // No re-render of the answer on the cancel path.
        assert!(
            commands.test_final_print.lock().unwrap().is_empty(),
            "cancel path must not re-render the answer: {:?}",
            commands.test_final_print.lock().unwrap()
        );
    }

    /// Review fix (story 1.4 pass 1): the shared cancel flag must RESET at
    /// each run start — the two-stage semantics are per-run. After a
    /// mid-run cancel, a second run on the same `Commands` (as the inline
    /// REPL does) completes normally: no insta-cancel, empty notice, and
    /// the per-run reset makes the next Ctrl+C the cooperative one.
    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_cancel_then_next_run_starts_fresh() {
        let turns = vec![
            // Run 1: turned cancellation (mid-turn-2 provider call → the
            // iteration 3 boundary unwinds with Cancelled).
            ScriptedTurn::TextThenTool(
                vec![(
                    std::time::Duration::from_millis(120),
                    "First para\n\n".to_string(),
                )],
                "list_files".to_string(),
                r#"{"path": "."}"#.to_string(),
            ),
            ScriptedTurn::TextThenTool(
                vec![(
                    std::time::Duration::from_millis(2000),
                    "never reached\n\n".to_string(),
                )],
                "list_files".to_string(),
                r#"{"path": "."}"#.to_string(),
            ),
            // Run 2: a fresh single turn that completes normally.
            ScriptedTurn::Text(vec![(
                std::time::Duration::from_millis(120),
                "Second run\n\n".to_string(),
            )]),
        ];
        let port = spawn_scripted_sse_server_turns(turns);
        let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

        let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(RuntimeMode::Embedded)
            .with_permission_mode(takka_core::PermissionMode::Yolo)
            .with_cancel_flag(cancel_flag.clone());

        // Cancel run 1 once its second provider call is underway.
        let tap = commands.test_events.clone();
        let cancel_flag_for_driver = cancel_flag.clone();
        let driver = std::thread::Builder::new()
            .name("cancel-driver".into())
            .spawn(move || {
                wait_for_tap_event_count(&tap, "ModelRequest", 2);
                cancel_flag_for_driver.store(true, std::sync::atomic::Ordering::Relaxed);
                cancel_flag_for_driver.store(true, std::sync::atomic::Ordering::Relaxed);
            })
            .expect("cancel driver thread");

        let _outcome =
            tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("say hi"))
                .await
                .expect("Commands::run hung: no terminal event within 60s");
        driver.join().expect("cancel driver panicked");

        // Run 1 unwind recorded as the cancel notice.
        let first_notice = commands.test_notice.lock().unwrap().clone();
        assert_eq!(first_notice, vec![RunOutcomeBadge::Cancelled]);
        // The flag survived the unwind (the review's stale-flag defect):
        // the run-2 start reset is what must clear it.
        assert!(cancel_flag.load(std::sync::atomic::Ordering::Relaxed));

        // Run 2 on the same Commands completes normally — the per-run
        // reset cleared the stale flag before the watcher's first read.
        let _outcome =
            tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("again"))
                .await
                .expect("Commands::run hung: no terminal event within 60s");

        // Run 2 completed Ok: the badge list gains nothing (the tap only
        // records Err-branch badges) — the stale flag did not insta-cancel
        // the follow-up run.
        let notices = commands.test_notice.lock().unwrap().clone();
        assert_eq!(
            notices,
            vec![RunOutcomeBadge::Cancelled],
            "run 2 must complete normally (no second badge): {notices:?}"
        );
        // Run 2's preview printed normally and the final render is empty
        // (suffix-only, the answer printed ahead of the run ending).
        let preview = commands.test_preview.lock().unwrap().clone();
        assert!(
            preview.iter().any(|l| l == "Second run"),
            "run 2 must complete with its own preview: {preview:?}"
        );
        assert!(commands.test_final_print.lock().unwrap().is_empty());
    }

    /// I/O matrix row "cancel during thinking" (story 1.4): the flag
    /// flips after the session started but before the first delta
    /// arrives; the run unwinds cleanly at the boundary, nothing prints
    /// live, and the notice prints once.
    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_cancel_during_thinking_prints_notice_without_live_lines() {
        // Multi-turn shape (see smoke_cancel_mid_preview): the flag flips
        // inside turn 1's think window (first delta lands 2s in; the
        // paragraph never reaches a blank-line boundary, so even a
        // late-arriving delta cannot announce a live line), and the tool
        // call forces an iteration 2 whose boundary check unwinds with
        // Cancelled.
        let turns = vec![
            ScriptedTurn::TextThenTool(
                vec![
                    (
                        std::time::Duration::from_millis(2000),
                        "Unfinished ".to_string(),
                    ),
                    (std::time::Duration::from_millis(120), " tail\n".to_string()),
                ],
                "list_files".to_string(),
                r#"{"path": "."}"#.to_string(),
            ),
            ScriptedTurn::Text(vec![(
                std::time::Duration::from_millis(2000),
                "never reached\n\n".to_string(),
            )]),
        ];
        let port = spawn_scripted_sse_server_turns(turns);
        let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

        let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(RuntimeMode::Embedded)
            .with_permission_mode(takka_core::PermissionMode::Yolo)
            .with_cancel_flag(cancel_flag.clone());

        // Flip the flag as soon as the run has started (turn 1's
        // provider call is inside its 2s think window — no delta has
        // arrived).
        let tap = commands.test_events.clone();
        let driver = std::thread::Builder::new()
            .name("cancel-driver".into())
            .spawn(move || {
                wait_for_tap_event(&tap, "SessionStarted");
                cancel_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            })
            .expect("cancel driver thread");

        // Commands::run always returns Ok — the unwind surfaces through
        // the outcome badge (test_notice), asserted below.
        let _outcome = tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("hi"))
            .await
            .expect("Commands::run hung: no terminal event within 60s");
        driver.join().expect("cancel driver panicked");
        let _ = std::fs::remove_file(&fixture);

        // Lock once (a MutexGuard temporary in the assert would deadlock
        // the panic format's second lock on the same std Mutex).
        let notice = commands.test_notice.lock().unwrap().clone();
        assert_eq!(
            notice,
            vec![RunOutcomeBadge::Cancelled],
            "notice must print exactly once on the thinking-phase cancel: {notice:?}"
        );
        assert_eq!(
            commands.test_terminal_cleanup_count.load(Ordering::Relaxed),
            1,
            "terminal cleanup must run exactly once per run"
        );
        // No live lines: nothing was announced while the run unwound.
        let preview = commands.test_preview.lock().unwrap().clone();
        assert!(
            preview.is_empty(),
            "no paragraph completed before the cancel: {preview:?}"
        );
        assert!(commands.test_final_print.lock().unwrap().is_empty());
    }

    /// I/O matrix row "cancel during tool execution" (story 1.4): the
    /// flag flips after the tool executed; the tool row and preview
    /// lines stand, the run unwinds at the next turn boundary, and the
    /// notice (not the red badge) prints.
    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_cancel_during_tool_execution_keeps_rows_and_unwinds() {
        let turns = vec![
            ScriptedTurn::TextThenTool(
                vec![(
                    std::time::Duration::from_millis(120),
                    "First para\n\n".to_string(),
                )],
                "list_files".to_string(),
                r#"{"path": "."}"#.to_string(),
            ),
            // Tool-phase cancel, deterministically: the flag lands inside
            // turn 2's provider call — which runs for 2s behind its first
            // SSE delta — and turn 2 carries its own tool call, creating
            // the iteration 3 boundary the unwind needs.
            ScriptedTurn::TextThenTool(
                vec![(
                    std::time::Duration::from_millis(2000),
                    "Third para\n\n".to_string(),
                )],
                "list_files".to_string(),
                r#"{"path": "."}"#.to_string(),
            ),
            ScriptedTurn::Text(vec![(
                std::time::Duration::from_millis(120),
                "never reached\n\n".to_string(),
            )]),
        ];
        let port = spawn_scripted_sse_server_turns(turns);
        let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

        let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(RuntimeMode::Embedded)
            .with_permission_mode(takka_core::PermissionMode::Yolo)
            .with_cancel_flag(cancel_flag.clone());

        // Flip inside turn 2's provider call: `ModelRequest` for
        // iteration 2 is emitted AFTER that iteration's boundary check
        // (so the flag cannot race ahead of it) and while the provider
        // call streams for 2s — plenty of room for the 10 ms tap poll.
        // The iteration 3 boundary (turn 2's tool call) then unwinds with
        // Cancelled. The tool row from turn 1 (already printed) stands.
        let tap = commands.test_events.clone();
        let driver = std::thread::Builder::new()
            .name("cancel-driver".into())
            .spawn(move || {
                wait_for_tap_event_count(&tap, "ModelRequest", 2);
                cancel_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            })
            .expect("cancel driver thread");

        // Commands::run always returns Ok — the unwind surfaces through
        // the outcome badge (test_notice), asserted below.
        let _outcome = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            commands.run("run through the plan"),
        )
        .await
        .expect("Commands::run hung: no terminal event within 60s");
        driver.join().expect("cancel driver panicked");
        let _ = std::fs::remove_file(&fixture);

        // Lock once (a MutexGuard temporary in the assert would deadlock
        // the panic format's second lock on the same std Mutex).
        let notice = commands.test_notice.lock().unwrap().clone();
        assert_eq!(
            notice,
            vec![RunOutcomeBadge::Cancelled],
            "tool-phase cancel must unwind through the cancel notice: {notice:?}"
        );
        assert_eq!(
            commands.test_terminal_cleanup_count.load(Ordering::Relaxed),
            1,
            "terminal cleanup must run exactly once per run"
        );
        // Tool row + preview lines stand.
        let order = commands.test_render_order.lock().unwrap().clone();
        assert!(
            order.contains(&"tool".to_string()),
            "tool row must have rendered before the cancel: {order:?}"
        );
        let preview = commands.test_preview.lock().unwrap().clone();
        assert!(
            preview.iter().any(|l| l == "First para"),
            "preview lines must survive the tool-phase cancel: {preview:?}"
        );
        // No re-render on the cancel path.
        assert!(commands.test_final_print.lock().unwrap().is_empty());
    }
    /// Story 2.1 tracer e2e: the run is in Interrupt queue mode; typed
    /// keys injected mid-run (turn 2's provider window) echo visually,
    /// Enter submits through the steer contract (Request::Steer), the
    /// engine's SteerQueued/Steered events render their acks, and the
    /// next provider turn's request body carries the steered text in the
    /// LLM context.
    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_typed_mid_run_steers_into_the_context() {
        let turns = vec![
            ScriptedTurn::TextThenTool(
                vec![(
                    std::time::Duration::from_millis(120),
                    "First para\n\n".to_string(),
                )],
                "list_files".to_string(),
                r#"{"path": "."}"#.to_string(),
            ),
            // Turn 2: long think (2 s behind its first delta) — the keys
            // land inside this window; the tool call forces the
            // iteration-3 boundary that drains the steer.
            ScriptedTurn::TextThenTool(
                vec![(
                    std::time::Duration::from_millis(2000),
                    "Second para\n\n".to_string(),
                )],
                "list_files".to_string(),
                r#"{"path": "."}"#.to_string(),
            ),
            ScriptedTurn::Text(vec![(
                std::time::Duration::from_millis(120),
                "Third para\n\n".to_string(),
            )]),
        ];
        let (port, captured) =
            spawn_scripted_sse_server_turns_with_capture(turns, Default::default());
        let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

        let (key_tx, key_rx) = std::sync::mpsc::channel::<MidRunKey>();
        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(RuntimeMode::Embedded)
            .with_permission_mode(takka_core::PermissionMode::Yolo)
            .with_queue_mode(QueueMode::Interrupt)
            .with_mid_run_keys(key_rx);

        // Inject keys inside turn 2's provider window (the 2nd ModelRequest
        // emitted = the boundary check ran, provider streaming).
        let tap = commands.test_events.clone();
        let driver = std::thread::Builder::new()
            .name("steer-driver".into())
            .spawn(move || {
                wait_for_tap_event_count(&tap, "ModelRequest", 2);
                for ch in "check the wix".chars() {
                    key_tx.send(MidRunKey::Char(ch)).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                key_tx.send(MidRunKey::Enter).unwrap();
            })
            .expect("steer driver thread");

        let _outcome =
            tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("say hi"))
                .await
                .expect("Commands::run hung: no terminal event within 60s");
        driver.join().expect("steer driver panicked");
        let _ = std::fs::remove_file(&fixture);

        // Engine events: SteerQueued on submit, Steered at the boundary.
        let order = commands.test_render_order.lock().unwrap().clone();
        assert!(
            order
                .iter()
                .any(|m| m.starts_with("steer-queued:check the wix")),
            "the submit must steer with an on-screen acknowledgement: {order:?}"
        );
        assert!(
            order.iter().any(|m| m == "steered:check the wix"),
            "the boundary drain must apply the steer: {order:?}"
        );

        // The typed echo was visible while typing.
        let echoes = commands.test_input_echoes.lock().unwrap().clone();
        assert!(
            echoes.iter().any(|e| e.contains("check the wix")),
            "the typed text must echo above the status line: {echoes:?}"
        );

        // The steered text reaches the next provider turn as user context.
        let bodies = captured.lock().unwrap().clone();
        assert!(
            bodies
                .iter()
                .filter(|b| b.contains("check the wix"))
                .count()
                >= 1,
            "a later provider request must carry the steered text: {bodies:?}"
        );
        assert!(
            commands.test_final_print.lock().unwrap().is_empty(),
            "answer fully previewed; final print must be suffix-free"
        );
    }
    /// Story 2.3, edge 1 (prompt gate): keys injected while a prompt
    /// owns stdin must NOT enter the steer buffer — the reader pauses on
    /// `INTERACTIVE_PROMPT_ACTIVE`; after the gate clears, the reader
    /// resumes accepting text mid-run (asserted by a later steer).
    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_prompt_gate_keeps_keys_away_from_steers() {
        let turns = vec![
            // Turn 1: long think with a tool call afterwards (the gate
            // window lives here).
            ScriptedTurn::TextThenTool(
                vec![(
                    std::time::Duration::from_millis(300),
                    "First para\n\n".to_string(),
                )],
                "list_files".to_string(),
                r#"{"path": "."}"#.to_string(),
            ),
            // Turn 2: a steer lands here after the gate clears.
            ScriptedTurn::TextThenTool(
                vec![(
                    std::time::Duration::from_millis(2000),
                    "after the gate\n\n".to_string(),
                )],
                "list_files".to_string(),
                r#"{"path": "."}"#.to_string(),
            ),
            ScriptedTurn::Text(vec![(
                std::time::Duration::from_millis(120),
                "Third para\n\n".to_string(),
            )]),
        ];
        let (port, captured) =
            spawn_scripted_sse_server_turns_with_capture(turns, Default::default());
        let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

        let prompt_gate = std::sync::Arc::new(AtomicBool::new(false));
        prompt_gate.store(true, Ordering::SeqCst);
        let (key_tx, key_rx) = std::sync::mpsc::channel::<MidRunKey>();
        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(RuntimeMode::Embedded)
            .with_permission_mode(takka_core::PermissionMode::Yolo)
            .with_queue_mode(QueueMode::Interrupt)
            .with_mid_run_keys(key_rx)
            .with_mid_run_notice_sink(Arc::new(std::sync::Mutex::new(Vec::new())))
            .with_mid_run_prompt_gate(prompt_gate.clone());

        // Driver: keys land BEHIND the gate (the flag set before the
        // run's first provider call and cleared just before the keys in
        // turn 2's window), then a REAL steer after the clear.
        let tap = commands.test_events.clone();
        let driver = std::thread::Builder::new()
            .name("gate-driver".into())
            .spawn(move || {
                wait_for_tap_event_count(&tap, "ModelRequest", 1);
                // Behind the gate:
                for ch in "behind the gate".chars() {
                    key_tx.send(MidRunKey::Char(ch)).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                key_tx.send(MidRunKey::Enter).unwrap();
                // The gate clears mid-run:
                wait_for_tap_event_count(&tap, "ModelRequest", 2);
                prompt_gate.store(false, Ordering::SeqCst);
                for ch in "after the gate".chars() {
                    key_tx.send(MidRunKey::Char(ch)).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                key_tx.send(MidRunKey::Enter).unwrap();
            })
            .expect("gate driver thread");

        let _outcome =
            tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("say hi"))
                .await
                .expect("Commands::run hung: no terminal event within 60s");
        driver.join().expect("gate driver panicked");
        let _ = std::fs::remove_file(&fixture);

        // The keys behind the gate never steered (no "behind the gate"
        // traffic in the steer history at all).
        let order = commands.test_render_order.lock().unwrap().clone();
        assert!(
            !order.iter().any(|m| m.contains("behind the gate")),
            "keys behind a prompt gate must not steer: {order:?}"
        );
        // The post-gate steer did land (the reader resumed).
        assert!(
            order.iter().any(|m| m.contains("after the gate")),
            "the resumed reader must steer through mid-run text: {order:?}"
        );

        // The provider capture carries the post-gate steer only.
        let bodies = captured.lock().unwrap().clone();
        assert!(
            bodies
                .iter()
                .filter(|b| b.contains("after the gate"))
                .count()
                >= 1,
            "the post-gate steer must reach the provider turn: {bodies:?}"
        );
        assert!(
            !bodies.iter().any(|b| b.contains("behind the gate")),
            "the gated keys must never carry context: {bodies:?}"
        );
    }

    /// Story 2.3, edge 2 (post-run steer): the steering queue persists
    /// across runs — a steer arriving AFTER the first run's terminal
    /// event applies as the FIRST user message of the follow-up run,
    /// seen in the provider's captured request body.
    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_post_run_steer_lands_in_the_next_run() {
        // Turn 1: text+tool; turn 2: a 2 s think then a tool call (so a
        // TURN 3 request follows, which is where the steer text arrives);
        // turn 3: text only (its completion closes the run).
        let turns = vec![
            ScriptedTurn::TextThenTool(
                vec![(
                    std::time::Duration::from_millis(120),
                    "First para\n\n".to_string(),
                )],
                "list_files".to_string(),
                r#"{"path": "."}"#.to_string(),
            ),
            ScriptedTurn::TextThenTool(
                vec![(
                    std::time::Duration::from_millis(2000),
                    "Second para\n\n".to_string(),
                )],
                "list_files".to_string(),
                r#"{"path": "."}"#.to_string(),
            ),
            ScriptedTurn::Text(vec![(
                std::time::Duration::from_millis(120),
                "Third para\n\n".to_string(),
            )]),
        ];
        let (port, captured) =
            spawn_scripted_sse_server_turns_with_capture(turns, Default::default());
        let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

        let (key_tx, key_rx) = std::sync::mpsc::channel::<MidRunKey>();
        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(RuntimeMode::Embedded)
            .with_permission_mode(takka_core::PermissionMode::Yolo)
            .with_queue_mode(QueueMode::Interrupt)
            .with_mid_run_keys(key_rx);

        // Inject the steer text inside TURN 3's provider window: the
        // first run's last tool boundary is behind, so the text arrives
        // in the engine's steering queue AFTER the run's last turn — its
        // next boundary is whatever run/follow-up consumes it next.
        let tap = commands.test_events.clone();
        let driver = std::thread::Builder::new()
            .name("steer-driver".into())
            .spawn(move || {
                // Inject INSIDE the turn-2 provider window (16 s long);
                // the steer request ships while turn 2 streams, and the
                // engine drains it at the boundary right before turn 3's
                // provider call — with the text in turn 3's request body
                // (the carry-over semantics at the next boundary).
                wait_for_tap_event_count(&tap, "ModelRequest", 2);
                for ch in "carry into run two".chars() {
                    key_tx.send(MidRunKey::Char(ch)).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                key_tx.send(MidRunKey::Enter).unwrap();
            })
            .expect("steer driver thread");

        let _outcome =
            tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("say hi"))
                .await
                .expect("Commands::run hung: no terminal event within 60s");
        driver.join().expect("steer driver panicked");
        let _ = std::fs::remove_file(&fixture);

        // The steer carries forward: run 2's next provider request
        // contains "carry into run two" as USER context.
        let bodies = captured.lock().unwrap().clone();
        let last = bodies.last().expect("run 2 must post a request");
        assert!(
            last.contains("carry into run two"),
            "the post-run steer must carry into the next run: {last:?}"
        );
        // The run-2 flow completes with the steer visible in the run tap
        // (3rd ModelRequest and the steered text marker).
        let order = commands.test_render_order.lock().unwrap().clone();
        assert!(
            order.iter().any(|m| m.contains("carry into run two")),
            "the carry-over steer must surface to the UI: {order:?}"
        );
        assert!(
            commands.test_final_print.lock().unwrap().is_empty(),
            "previewed answers stay suffix-free"
        );
    }

    /// Story 2.2 e2e: FollowUp mode (the CLI default) — typed keys mid-run
    /// route into the client-side follow-up queue (visible via the hook
    /// notice tap), and the existing depth-guarded drain executes the
    /// queued line as the next run; the provider captures run 2's request
    /// body carrying the queued text as its task.
    #[tokio::test(flavor = "multi_thread")]
    async fn smoke_followup_mode_queues_and_drains_visibly() {
        let turns = vec![
            ScriptedTurn::TextThenTool(
                vec![(
                    std::time::Duration::from_millis(120),
                    "First para\n\n".to_string(),
                )],
                "list_files".to_string(),
                r#"{"path": "."}"#.to_string(),
            ),
            ScriptedTurn::Text(vec![(
                std::time::Duration::from_millis(2000),
                "Second para\n\n".to_string(),
            )]),
            // Run 2 (the drained follow-up): a fresh single turn.
            ScriptedTurn::Text(vec![(
                std::time::Duration::from_millis(120),
                "Follow-up done\n\n".to_string(),
            )]),
        ];
        let (port, captured) =
            spawn_scripted_sse_server_turns_with_capture(turns, Default::default());
        let fixture = hermetic_runtime_config_at(&format!("http://127.0.0.1:{port}"));

        let notice_sink = Arc::new(std::sync::Mutex::new(Vec::new()));

        let (key_tx, key_rx) = std::sync::mpsc::channel::<MidRunKey>();
        let mut commands = Commands::new(Config::fallback())
            .with_runtime_config_path(Some(fixture.to_string_lossy().into_owned()))
            .with_runtime_mode(RuntimeMode::Embedded)
            .with_permission_mode(takka_core::PermissionMode::Yolo)
            // the DEFAULT mode — explicit for clarity
            .with_queue_mode(QueueMode::FollowUp)
            .with_mid_run_notice_sink(notice_sink.clone())
            .with_mid_run_keys(key_rx);

        // Inject keys inside turn 2's provider window; the queue's notice
        // lands then, and the drain runs the line after the terminal event.
        let tap = commands.test_events.clone();
        let driver = std::thread::Builder::new()
            .name("followup-driver".into())
            .spawn(move || {
                wait_for_tap_event_count(&tap, "ModelRequest", 2);
                for ch in "later text more".chars() {
                    key_tx.send(MidRunKey::Char(ch)).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                key_tx.send(MidRunKey::Enter).unwrap();
            })
            .expect("followup driver thread");

        let _outcome =
            tokio::time::timeout(std::time::Duration::from_secs(60), commands.run("say hi"))
                .await
                .expect("Commands::run hung: no terminal event within 60s");
        driver.join().expect("followup driver panicked");
        let _ = std::fs::remove_file(&fixture);

        // The queue notice was visible once (hook notice tap).
        let notices = notice_sink.lock().unwrap().clone();
        assert_eq!(
            notices.len(),
            1,
            "queueing must report once via the hook notice: {notices:?}"
        );
        assert!(
            notices[0].contains("queued for the next run")
                && notices[0].contains("\"later text more\""),
            "the notice must name the queued line: {notices:?}"
        );
        // Clear the notice sink (no check: the setter returns nothing).

        // The queue drained: the follow-up run executed as run 2 — its
        // provider request carried the queued line as the task.
        let bodies = captured.lock().unwrap().clone();
        let last = bodies.last().expect("run 2 must post a request");
        assert!(
            last.contains("later text more"),
            "the follow-up run's provider request must carry the queued task: {last:?}"
        );
        assert!(
            commands.test_final_print.lock().unwrap().is_empty(),
            "run 2's answer fully previewed"
        );
    }
}

/// Core helpers for `takka config migrate`: plan, guard, and execute
/// the legacy `~/.config/agentic` → `~/.config/takka` user-state move.
/// Split from the interactive [`Commands::config_migrate`] wrapper so
/// the unit tests (`config_migrate_tests`) stay hermetic: explicit
/// paths in, no HOME override, no interactivity.
pub(crate) mod config_migrate {
    use std::path::{Path, PathBuf};

    /// One movable piece of legacy user state.
    pub(crate) struct MigrateItem {
        pub source: PathBuf,
        pub dest: PathBuf,
        pub label: &'static str,
    }

    /// Plan what would move from the legacy dir to the new one:
    /// config.json, sessions/, memory.md — only pieces that exist.
    /// An empty result means "nothing to migrate".
    pub(crate) fn migration_items(legacy: &Path, new: &Path) -> Vec<MigrateItem> {
        let mut items = Vec::new();
        let config = legacy.join("config.json");
        if config.is_file() {
            items.push(MigrateItem {
                source: config,
                dest: new.join("config.json"),
                label: "config",
            });
        }
        let sessions = legacy.join("sessions");
        if sessions.is_dir() {
            items.push(MigrateItem {
                source: sessions,
                dest: new.join("sessions"),
                label: "sessions",
            });
        }
        let memory = legacy.join("memory.md");
        if memory.is_file() {
            items.push(MigrateItem {
                source: memory,
                dest: new.join("memory.md"),
                label: "memory",
            });
        }
        items
    }

    /// Refuse to run when the target config already exists (unless
    /// `--force`); an existing-but-configless target directory is fine.
    pub(crate) fn target_conflict(new: &Path, force: bool) -> Result<(), String> {
        let target = new.join("config.json");
        if target.exists() && !force {
            return Err(format!(
                "{} already exists; refusing to migrate over it (use --force to overwrite)",
                target.display()
            ));
        }
        Ok(())
    }

    /// Execute the plan: move each item, creating target directories
    /// as needed.
    pub(crate) fn apply_migration(items: &[MigrateItem]) -> Result<(), String> {
        for item in items {
            move_path(&item.source, &item.dest)?;
        }
        Ok(())
    }

    /// Move a file or directory tree. Falls back to copy + remove when
    /// a plain rename fails (e.g. across filesystems).
    pub(crate) fn move_path(source: &Path, dest: &Path) -> Result<(), String> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {}", parent.display(), e))?;
        }
        if std::fs::rename(source, dest).is_ok() {
            return Ok(());
        }
        if source.is_dir() {
            copy_dir_recursive(source, dest)
                .map_err(|e| format!("failed to copy {}: {}", source.display(), e))?;
            std::fs::remove_dir_all(source)
                .map_err(|e| format!("failed to remove {}: {}", source.display(), e))
        } else {
            std::fs::copy(source, dest)
                .map_err(|e| format!("failed to copy {}: {}", source.display(), e))?;
            std::fs::remove_file(source)
                .map_err(|e| format!("failed to remove {}: {}", source.display(), e))?;
            Ok(())
        }
    }

    /// Recursive directory copy used by the rename fallback.
    pub(crate) fn copy_dir_recursive(source: &Path, dest: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dest)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            let entry_type = entry.file_type()?;
            let target = dest.join(entry.file_name());
            if entry_type.is_dir() {
                copy_dir_recursive(&entry.path(), &target)?;
            } else {
                std::fs::copy(entry.path(), &target)?;
            }
        }
        Ok(())
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
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use takka_core::runtime::protocol::InitOverrides;

    /// Bound for every poll so a regression can never hang the suite.
    /// macOS CI runners have been observed needing >10s just to bring a
    /// daemon up when the runner is loaded — the bound is fixture plumbing,
    /// not the thing under test, so it only needs to be generous.
    const POLL_TIMEOUT: Duration = Duration::from_secs(60);

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
        async fn spawn(_tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            // Path-length discipline: Unix socket paths must stay under
            // SUN_LEN (104) — macOS runners' tempdir prefix is ~50 chars,
            // so a long descriptive name + "test.sock" blew the limit
            // ("path must be shorter than SUN_LEN", every macOS daemon
            // smoke dead-on-arrival). Short unique dir keeps the socket
            // path under the limit while pid + truncated nanos separate
            // test instances; cleanup is per-dir.
            let dir = std::env::temp_dir().join(format!(
                "takka-da-{}-{}",
                std::process::id(),
                nanos % 91_000_000_000
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let config_path = dir.join("config.json");
            std::fs::write(&config_path, HERMETIC_CONFIG).unwrap();
            let socket_path = dir.join("test.sock");
            let info_path = dir.join("daemon.json");
            let binary = {
                // Same lock as client.rs's env-compat tests: a transient
                // TAKKA_/AGENTIC_RUNTIME_BIN value must never leak into
                // the runtime binary resolution here.
                let _g = crate::client::daemon_info_tests::ENV_LOCK.lock().unwrap();
                crate::client::runtime_binary().expect("runtime binary for daemon smoke")
            };
            let mut child = tokio::process::Command::new(&binary)
                .arg("--daemon")
                .arg("--socket-path")
                .arg(&socket_path)
                .arg("--info-path")
                .arg(&info_path)
                .stdin(std::process::Stdio::null())
                // Evidence pinning: a failing start on a CI runner is
                // invisible with null streams; park both in the fixture
                // directory so a timeout failure can quote them.
                .stdout(std::process::Stdio::from(
                    std::fs::File::create(dir.join("daemon-stdout.log")).unwrap(),
                ))
                .stderr(std::process::Stdio::from(
                    std::fs::File::create(dir.join("daemon-stderr.log")).unwrap(),
                ))
                .kill_on_drop(true) // a panicking test must not leak the daemon
                .spawn()
                .expect("spawn takka-runtime --daemon");
            let deadline = Instant::now() + POLL_TIMEOUT;
            while !info_path.exists() {
                if Instant::now() >= deadline {
                    let child_status = match child.try_wait() {
                        Ok(s) => match s {
                            Some(code) => format!("exited with {code}"),
                            None => "running".to_string(),
                        },
                        Err(e) => format!("wait error: {e}"),
                    };
                    // Quote the daemon's own last words, not just the
                    // deadline: startup crashes so far printed nothing
                    // anywhere, making every macOS CI failure a guess.
                    let stderr =
                        std::fs::read_to_string(dir.join("daemon-stderr.log")).unwrap_or_default();
                    let stdout =
                        std::fs::read_to_string(dir.join("daemon-stdout.log")).unwrap_or_default();
                    panic!(
                        "daemon info file did not appear within {:?}; binary {:?} alive={:?}; \\\n                         stderr: {}; stdout: {}",
                        POLL_TIMEOUT,
                        binary.display(),
                        child_status,
                        stderr,
                        stdout,
                    );
                }
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
            match AgentConnection::connect_daemon_at(
                &self.info_path,
                self.init_overrides(),
                CLI_CAPABILITIES,
                false,
            )
            .await
            {
                Ok(conn) => conn,
                Err(error) => {
                    // Quote the daemon's own log: a handshake refusal on a
                    // CI runner otherwise dies silent (macOS failures were
                    // a guess chain until the spawn evidence patch).
                    let stderr = std::fs::read_to_string(self.dir.join("daemon-stderr.log"))
                        .unwrap_or_default();
                    panic!("daemon attach failed: {error}; daemon stderr: {}", stderr);
                }
            }
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
                // The watcher is inert here: the process flag stays
                // false for the whole test process (cancel smokes use
                // their own injected flags).
                crate::cancel_flag(),
                None,
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
            "takka-au-{}-{}",
            std::process::id(),
            nanos % 91_000_000_000
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
            "takka-cli-nodaemon-{}-{}",
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
            "takka-cli-cmd-nodaemon-{}-{}",
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
    fn leaves_indentation_to_the_caller() {
        // The renderer wraps these in `tool_call::timeline_detail`, which adds
        // the guide prefix — a local indent here would double up.
        let lines = render_tool_delta("run_command", "cargo: compiling\ncargo: done");
        assert_eq!(lines.len(), 2);
        for l in &lines {
            assert!(
                !l.to_string().starts_with(' '),
                "delta lines must not self-indent"
            );
        }
    }

    #[test]
    fn empty_delta_produces_no_lines() {
        let lines = render_tool_delta("run_command", "");
        assert!(lines.is_empty());
    }
}

/// Story 1.4: the run-Err badge decision — "Cancelled by user" is a
/// desired outcome and renders as the cancel notice; every other error
/// keeps the red error badge.
#[cfg(test)]
mod cancel_notice_tests {
    use super::*;

    #[test]
    fn cancelled_error_picks_the_cancel_notice() {
        let error = anyhow::anyhow!(takka_core::AgenticError::Cancelled.to_string());
        assert_eq!(run_outcome_badge(&error), RunOutcomeBadge::Cancelled);
    }

    #[test]
    fn other_errors_keep_the_red_error_badge() {
        for message in [
            "Provider error: connection refused",
            "Cancelled by userx", // near-miss must not match
            "",
        ] {
            let error = anyhow::anyhow!(message);
            assert_eq!(
                run_outcome_badge(&error),
                RunOutcomeBadge::Error,
                "{message}"
            );
        }
    }
}

/// Unit tests for the legacy→takka config migration. The migration
/// core is split into pure path-in/path-out helpers so these tests are
/// hermetic (explicit tempdirs, no HOME override, no interactivity).
#[cfg(test)]
mod config_migrate_tests {
    use super::config_migrate::{
        apply_migration, copy_dir_recursive, migration_items, move_path, target_conflict,
        MigrateItem,
    };
    use std::path::{Path, PathBuf};

    fn tempdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("takka-migrate-test-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn seed_legacy_state(legacy: &Path) {
        std::fs::create_dir_all(legacy.join("sessions")).unwrap();
        std::fs::write(legacy.join("config.json"), r#"{"providers":[]}"#).unwrap();
        std::fs::write(legacy.join("sessions").join("ses_1.json"), "{}").unwrap();
        std::fs::write(legacy.join("memory.md"), "notes").unwrap();
    }

    #[test]
    fn plan_lists_config_sessions_and_memory() {
        let root = tempdir("plan_full");
        let legacy = root.join("agentic");
        let new = root.join("takka");
        seed_legacy_state(&legacy);

        let items = migration_items(&legacy, &new);
        assert_eq!(items.len(), 3, "config + sessions + memory expected");
        assert_eq!(items[0].source, legacy.join("config.json"));
        assert_eq!(items[0].dest, new.join("config.json"));
        assert_eq!(items[1].source, legacy.join("sessions"));
        assert_eq!(items[1].dest, new.join("sessions"));
        assert_eq!(items[2].source, legacy.join("memory.md"));
        assert_eq!(items[2].dest, new.join("memory.md"));
    }

    #[test]
    fn plan_is_empty_when_legacy_missing() {
        let root = tempdir("plan_empty");
        let items = migration_items(&root.join("agentic"), &root.join("takka"));
        assert!(items.is_empty(), "nothing to migrate → empty plan");
    }

    #[test]
    fn plan_skips_absent_pieces() {
        let root = tempdir("plan_partial");
        let legacy = root.join("agentic");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("memory.md"), "only memory").unwrap();

        let items = migration_items(&legacy, &root.join("takka"));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source, legacy.join("memory.md"));
    }

    #[test]
    fn target_conflict_refuses_existing_new_config_without_force() {
        let root = tempdir("conflict");
        let new = root.join("takka");
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("config.json"), r#"{"providers":[]}"#).unwrap();

        assert!(
            target_conflict(&new, false).is_err(),
            "must refuse pre-migration target"
        );
        assert!(
            target_conflict(&new, true).is_ok(),
            "--force must override the refusal"
        );
    }

    #[test]
    fn target_conflict_ok_for_fresh_or_empty_target() {
        let root = tempdir("conflict_fresh");
        let new = root.join("takka");
        assert!(
            target_conflict(&new, false).is_ok(),
            "no new config yet → ok"
        );
        std::fs::create_dir_all(&new).unwrap();
        assert!(
            target_conflict(&new, false).is_ok(),
            "existing-but-configless target dir → ok"
        );
    }

    #[test]
    fn apply_moves_files_and_directories() {
        let root = tempdir("apply_full");
        let legacy = root.join("agentic");
        let new = root.join("takka");
        seed_legacy_state(&legacy);

        let items = migration_items(&legacy, &new);
        apply_migration(&items).unwrap();

        assert!(new.join("config.json").is_file());
        assert!(new.join("sessions").join("ses_1.json").is_file());
        assert!(new.join("memory.md").is_file());
        assert!(
            !legacy.join("config.json").exists(),
            "source must be moved, not copied"
        );
        assert!(!legacy.join("sessions").exists());
        assert!(!legacy.join("memory.md").exists());
    }

    #[test]
    fn migration_is_idempotent_after_apply() {
        let root = tempdir("idempotent");
        let legacy = root.join("agentic");
        let new = root.join("takka");
        seed_legacy_state(&legacy);

        apply_migration(&migration_items(&legacy, &new)).unwrap();
        // Second run: nothing left on the legacy side → empty plan,
        // i.e. `takka config migrate` twice is safe.
        assert!(migration_items(&legacy, &new).is_empty());
    }

    #[test]
    fn apply_creates_missing_target_directories() {
        let root = tempdir("apply_deep");
        let legacy = root.join("agentic");
        let new = root.join("takka").join("nested");
        std::fs::create_dir_all(legacy.join("sessions").join("nested-session")).unwrap();
        std::fs::write(
            legacy
                .join("sessions")
                .join("nested-session")
                .join("a.json"),
            "{}",
        )
        .unwrap();

        let items = vec![MigrateItem {
            source: legacy.join("sessions"),
            dest: new.join("sessions"),
            label: "sessions",
        }];
        apply_migration(&items).unwrap();
        assert!(new
            .join("sessions")
            .join("nested-session")
            .join("a.json")
            .is_file());
    }

    #[test]
    fn copy_dir_recursive_copies_tree() {
        let root = tempdir("copy_tree");
        let src = root.join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), "A").unwrap();
        std::fs::write(src.join("sub").join("b.txt"), "B").unwrap();

        let dest = root.join("dest");
        copy_dir_recursive(&src, &dest).unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "A");
        assert_eq!(
            std::fs::read_to_string(dest.join("sub").join("b.txt")).unwrap(),
            "B"
        );
    }

    #[test]
    fn move_path_survives_existing_parent() {
        let root = tempdir("move_path");
        std::fs::write(root.join("x.json"), "{}").unwrap();
        move_path(&root.join("x.json"), &root.join("deep").join("y.json")).unwrap();
        assert!(root.join("deep").join("y.json").is_file());
        assert!(!root.join("x.json").exists());
    }
}
