use dialoguer::{theme::ColorfulTheme, Input, MultiSelect, Select};
use ratatui::style::{Color as RColor, Modifier as RModifier, Style as RStyle};
use ratatui::text::{Line as RLine, Span as RSpan};
use std::sync::atomic::Ordering;
use takka_core::sync;

use crate::widgets::{components, inline};

use super::*;

// ── Interactive tool handlers ───────────────────────────────

/// Handles `question` tool calls from the agent during interactive mode.
///
/// Renders each question with a styled prompt, presents options via
/// `dialoguer::Select` / `MultiSelect` when choices exist, and falls back
/// to free-text `Input` otherwise. The handler is synchronous (matches
/// the `Tool::execute` contract) and reads directly from stdin.
pub(crate) struct CliQuestionHandler;

impl takka_core::QuestionHandler for CliQuestionHandler {
    fn handle(&self, questions: &[takka_core::QuestionPrompt]) -> Vec<takka_core::QuestionAnswer> {
        // Pause the streaming renderer for the whole prompt sequence.
        // The spinner thread spawned by `Commands::run` redraws the
        // "Thinking…" line every 80ms with MoveUp+Clear+print; if it kept
        // writing here its cursor moves would land on top of the
        // dialoguer list (messy options), then erase the answer badge
        // after submission — and on Windows the two writers racing over
        // the console is what looks like a flickering dim lamp.
        let _render_gate = sync::lock(&RENDER_GATE);
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
pub(crate) fn free_text_fallback(question: &str) -> Vec<takka_core::QuestionAnswer> {
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
pub(crate) fn skip_answer(question: &str) -> Vec<takka_core::QuestionAnswer> {
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
pub(crate) fn render_answer(text: &str) {
    inline::print_blank();
    inline::print_line(&components::success_badge(&format!("Answered: {}", text)));
    inline::print_blank();
}

/// Renders todo list changes inline. Fires after every `todowrite` call.
/// Shows a compact progress summary so the user sees task progress even
/// in non-interactive `takka run` mode.
pub(crate) struct CliTodoRenderer;

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
pub(crate) struct ProviderPreset {
    pub(crate) name: &'static str,
    pub(crate) provider_type: &'static str,
    pub(crate) api_base: &'static str,
    pub(crate) models: &'static [(&'static str, &'static str)],
}

pub(crate) const PROVIDER_PRESETS: &[ProviderPreset] = &[
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

/// One poll of the terminal's key events, mapped to the reader's key
/// enum (story 2.1). Raw mode keeps SIGINT off; the reader is the
/// cancel source, so `Ctrl+C` maps via [`MidRunKey::CtrlC`]. Plain
/// characters and Enter drive the steer line; untracked keys fall
/// through.
#[cfg(not(test))]
pub(crate) fn poll_crossterm_key() -> Option<MidRunKey> {
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
