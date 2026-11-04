use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::sync::atomic::Ordering;
use takka_core::{ConfirmationRequest, RiskLevel};

use crate::widgets::components::{panel, BoxStyle};
use crate::widgets::diff as diff_widget;
use crate::widgets::inline;

pub enum ConfirmationResponse {
    Yes,
    No,
    Always,
    Quit,
}

pub fn prompt_confirmation(request: &ConfirmationRequest) -> Option<ConfirmationResponse> {
    // Pause the streaming renderer / TUI draw loop for the whole prompt
    // (same gate as the question handler). The confirmation panel must
    // not race the spinner's MoveUp/Clear redraws in REPL mode, and in
    // TUI mode the draw loop must stay parked so the panel survives on
    // the primary screen until the operator answers.
    let _render_gate = crate::commands::RENDER_GATE.lock().unwrap();
    let tui_active = crate::commands::TUI_ACTIVE.load(Ordering::Relaxed);
    if tui_active {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
    }
    if crate::commands::SPINNER_ACTIVE.load(Ordering::Relaxed) {
        crate::commands::consume_spinner_line();
    }

    // Re-enters the alternate screen when the TUI is active and flags a
    // full repaint so the next `Terminal::draw` rewrites everything.
    struct TuiScreenGuard(bool);
    impl Drop for TuiScreenGuard {
        fn drop(&mut self) {
            if self.0 {
                let _ = crossterm::execute!(
                    std::io::stdout(),
                    crossterm::terminal::EnterAlternateScreen
                );
                crate::commands::TUI_NEEDS_REDRAW.store(true, Ordering::Relaxed);
            }
        }
    }
    let _tui_guard = TuiScreenGuard(tui_active);

    // Risk → label + color. Color leaks through to the panel border so the
    // operator gets a visual cue at a glance.
    let (risk_label, risk_color) = match request.risk_level {
        RiskLevel::Low => ("LOW", Color::Rgb(46, 204, 113)),
        RiskLevel::Medium => ("MEDIUM", Color::Rgb(241, 196, 15)),
        RiskLevel::High => ("HIGH", Color::Rgb(230, 126, 34)),
        RiskLevel::Critical => ("CRITICAL", Color::Rgb(231, 76, 60)),
    };

    let label_style = Style::default().add_modifier(Modifier::BOLD);
    let risk_style = Style::default().fg(risk_color).add_modifier(Modifier::BOLD);
    let dim = Style::default().add_modifier(Modifier::DIM);

    let body = vec![
        Line::from(vec![
            Span::styled("Risk Level:  ", label_style),
            Span::styled(risk_label.to_string(), risk_style),
        ]),
        Line::from(vec![
            Span::styled("Action:      ", label_style),
            Span::raw(request.action.clone()),
        ]),
        Line::from(vec![
            Span::styled("Description: ", label_style),
            Span::raw(request.description.clone()),
        ]),
        Line::default(),
        Line::from(vec![
            Span::styled("[y]", label_style),
            Span::raw(" Yes  "),
            Span::styled("[n]", label_style),
            Span::raw(" No  "),
            Span::styled("[a]", label_style),
            Span::raw(" Always  "),
            Span::styled("[q]", label_style),
            Span::raw(" Quit"),
        ]),
    ];

    let lines = panel(
        "⚠ Confirmation Required",
        &body,
        BoxStyle::Rounded,
        risk_color,
    );

    inline::print_blank();
    inline::print_lines(&lines);

    // For state-changing file tools the orchestrator attaches a
    // preview of the unified diff to the confirmation request. Render
    // it through the shared diff widget so the operator sees the
    // exact change before approving.
    if let Some(ref diff_text) = request.preview_diff {
        const MAX_PREVIEW_LINES: usize = 60;
        inline::print_blank();
        inline::print_line(&diff_widget::summary_line(diff_text));
        let diff_lines = diff_widget::render(diff_text);
        if diff_lines.len() > MAX_PREVIEW_LINES {
            inline::print_lines(&diff_lines[..MAX_PREVIEW_LINES]);
            let remaining = diff_lines.len() - MAX_PREVIEW_LINES;
            inline::print_line(&Line::from(Span::styled(
                format!("    … {} more diff line(s) hidden", remaining),
                Style::default().add_modifier(Modifier::DIM),
            )));
        } else {
            inline::print_lines(&diff_lines);
        }
        inline::print_blank();
    }

    inline::print_line(&Line::from(Span::styled("  > ", dim)));

    // The REPL runs with crossterm raw mode enabled, where `read_line`
    // cannot work: Enter arrives as `\r` (not `\n`), echo is off, and
    // there is no line buffering. Flip back to cooked mode for the
    // duration of the prompt, then restore raw mode so the REPL keeps
    // working. Outside the REPL (raw mode already off) this is a no-op.
    let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
    if was_raw {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
    }

    /// Restores raw mode + cursor on scope exit (incl. early
    /// returns from the match arms below). The cursor is re-hidden for
    /// the REPL (which hides it in raw mode) but left visible for the
    /// TUI.
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

    loop {
        let mut input = String::new();
        if std::io::stdin().read_line(&mut input).is_err() {
            return None;
        }

        let input = input.trim().to_lowercase();
        match input.as_str() {
            "y" | "yes" => {
                if crate::commands::SPINNER_ACTIVE.load(Ordering::Relaxed) {
                    crate::commands::commit_spinner_placeholder();
                }
                return Some(ConfirmationResponse::Yes);
            }
            "n" | "no" => {
                if crate::commands::SPINNER_ACTIVE.load(Ordering::Relaxed) {
                    crate::commands::commit_spinner_placeholder();
                }
                return Some(ConfirmationResponse::No);
            }
            "a" | "always" => {
                if crate::commands::SPINNER_ACTIVE.load(Ordering::Relaxed) {
                    crate::commands::commit_spinner_placeholder();
                }
                return Some(ConfirmationResponse::Always);
            }
            "q" | "quit" => {
                if crate::commands::SPINNER_ACTIVE.load(Ordering::Relaxed) {
                    crate::commands::commit_spinner_placeholder();
                }
                return Some(ConfirmationResponse::Quit);
            }
            _ => {
                inline::print_line(&Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        "Invalid input. Enter (y/n/a/q): ",
                        Style::default().fg(Color::Rgb(241, 196, 15)),
                    ),
                ]));
            }
        }
    }
}
