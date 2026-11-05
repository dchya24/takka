use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::io::Write;
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

    let mut body = vec![
        Line::from(vec![
            Span::styled("Risk Level:  ", label_style),
            Span::styled(risk_label.to_string(), risk_style),
        ]),
        Line::from(vec![
            Span::styled("Action:      ", label_style),
            Span::raw(request.action.clone()),
        ]),
    ];
    // Wrapped, not truncated: the operator must be able to read the whole
    // command before approving it.
    body.extend(wrap_field(
        "Description: ",
        &request.description,
        panel_inner_width(),
        label_style,
    ));
    body.push(Line::default());
    body.extend([Line::from(vec![
        Span::styled("[y]", label_style),
        Span::raw(" Yes  "),
        Span::styled("[n]", label_style),
        Span::raw(" No  "),
        Span::styled("[a]", label_style),
        Span::raw(" Always  "),
        Span::styled("[q]", label_style),
        Span::raw(" Quit"),
    ])]);

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

    // Raw mode (REPL / TUI): one keypress answers the prompt. `read_line` is
    // unusable there — Enter arrives as `\r`, echo is off, nothing is line
    // buffered — and switching the terminal back to cooked mode mid-run was
    // what made a typed `a` come back as "Invalid input": the REPL's own key
    // loop grabbed the letter first. So: keep raw mode, poll the shared
    // "prompt owns stdin" flag (the REPL honours it), and read keys directly.
    //
    // Cooked mode (`takka run`, piped input): a plain line read still applies.
    let raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);

    if raw {
        // No newline: the pressed key echoes right after the marker.
        let mut stdout = std::io::stdout();
        let _ = write!(stdout, "  > ");
        let _ = stdout.flush();
    } else {
        inline::print_line(&Line::from(Span::styled("  > ", dim)));
    }

    // Tell the REPL to stop reading stdin until this prompt returns.
    let _input_guard = crate::commands::InteractivePromptGuard::activate();

    loop {
        let answer = if raw {
            let answer = read_prompt_key()?;
            echo_prompt_key(&answer);
            answer
        } else {
            let mut input = String::new();
            if std::io::stdin().read_line(&mut input).is_err() {
                return None;
            }
            PromptAnswer::parse(&input)
        };

        let response = match answer {
            PromptAnswer::Yes => ConfirmationResponse::Yes,
            PromptAnswer::No => ConfirmationResponse::No,
            PromptAnswer::Always => ConfirmationResponse::Always,
            PromptAnswer::Quit => ConfirmationResponse::Quit,
            PromptAnswer::Invalid(text) => {
                invalid_answer(text.as_deref());
                continue;
            }
        };
        if crate::commands::SPINNER_ACTIVE.load(Ordering::Relaxed) {
            crate::commands::commit_spinner_placeholder();
        }
        return Some(response);
    }
}

/// One answer to the confirmation prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PromptAnswer {
    Yes,
    No,
    Always,
    Quit,
    /// Unrecognised answer, carrying what the user actually typed for the
    /// "invalid input" echo (the raw-mode key path has no text to show).
    Invalid(Option<String>),
}

impl PromptAnswer {
    /// Parse a cooked-mode line (`y`, `yes`, `a`, …).
    fn parse(text: &str) -> Self {
        match text.trim().to_lowercase().as_str() {
            "y" | "yes" => Self::Yes,
            "n" | "no" => Self::No,
            "a" | "always" => Self::Always,
            "q" | "quit" => Self::Quit,
            _ => Self::Invalid(Some(text.trim().to_string())),
        }
    }

    /// Map a single raw-mode keypress.
    fn from_key(code: crossterm::event::KeyCode) -> Self {
        use crossterm::event::KeyCode;
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Self::Yes,
            KeyCode::Char('n') | KeyCode::Char('N') => Self::No,
            KeyCode::Char('a') | KeyCode::Char('A') => Self::Always,
            KeyCode::Char('q') | KeyCode::Char('Q') => Self::Quit,
            // Enter takes the safe default rather than re-prompting forever.
            KeyCode::Enter => Self::No,
            KeyCode::Esc => Self::Quit,
            other => Self::Invalid(Some(format!("{other:?}"))),
        }
    }
}

/// Blocking single-key read for raw mode. Returns `None` when the event stream
/// ends (stdin closed), so the caller can bail out instead of looping.
fn read_prompt_key() -> Option<PromptAnswer> {
    use crossterm::event::{Event, KeyEventKind};
    loop {
        match crossterm::event::read() {
            Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                return Some(PromptAnswer::from_key(key.code));
            }
            // Mouse moves / resizes / focus events: keep waiting.
            Ok(_) => continue,
            Err(_) => return None,
        }
    }
}

/// Echo the pressed key on the `> ` line (raw mode does not echo for us).
fn echo_prompt_key(answer: &PromptAnswer) {
    let text = match answer {
        PromptAnswer::Yes => "y",
        PromptAnswer::No => "n",
        PromptAnswer::Always => "a",
        PromptAnswer::Quit => "q",
        PromptAnswer::Invalid(_) => "",
    };
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{text}");
    let _ = stdout.flush();
}

/// Re-prompt after an unusable answer.
fn invalid_answer(typed: Option<&str>) {
    inline::print_line(&Line::from(vec![
        Span::raw("  "),
        Span::styled(
            match typed.filter(|t| !t.is_empty()) {
                Some(typed) => format!("\"{typed}\" is not an answer. Press y, n, a or q: "),
                None => "Press y, n, a or q: ".to_string(),
            },
            Style::default().fg(Color::Rgb(241, 196, 15)),
        ),
    ]));
}

/// Usable line width inside `panel()` (border + padding removed).
fn panel_inner_width() -> usize {
    inline::terminal_width().saturating_sub(2).clamp(40, 100) - 4
}

/// Render `label` + `value`, wrapping `value` onto continuation lines that line
/// up under it. Long values (a full command line) used to be truncated by the
/// sender and would have overflowed the panel otherwise.
fn wrap_field(label: &str, value: &str, width: usize, label_style: Style) -> Vec<Line<'static>> {
    // Collapse newlines/tabs: the value may be a multi-line command.
    let flat = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let label_width = label.chars().count();
    let indent = " ".repeat(label_width);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current = String::new();
    let mut budget = width.saturating_sub(label_width).max(20);

    for word in flat.split(' ') {
        let extra = if current.is_empty() {
            word.chars().count()
        } else {
            word.chars().count() + 1
        };
        if !current.is_empty() && current.chars().count() + extra > budget {
            lines.push(finish_row(
                label,
                &indent,
                &current,
                &label_style,
                lines.is_empty(),
            ));
            current = word.to_string();
            budget = width.saturating_sub(label_width).max(20);
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }
    lines.push(finish_row(
        label,
        &indent,
        &current,
        &label_style,
        lines.is_empty(),
    ));
    lines
}

fn finish_row(
    label: &str,
    indent: &str,
    text: &str,
    label_style: &Style,
    first: bool,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            if first {
                label.to_string()
            } else {
                indent.to_string()
            },
            *label_style,
        ),
        Span::raw(text.to_string()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect()
    }

    #[test]
    fn short_description_stays_on_one_line() {
        let lines = wrap_field(
            "Description: ",
            "run_command(git status)",
            80,
            Style::default(),
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(flat(&lines)[0], "Description: run_command(git status)");
    }

    #[test]
    fn long_command_is_wrapped_not_truncated() {
        let command = format!("run_command(git add {})", "a/very/long/path.rs ".repeat(8));
        let lines = wrap_field("Description: ", &command, 60, Style::default());
        assert!(lines.len() > 1, "expected wrapping");
        for row in flat(&lines) {
            assert!(row.chars().count() <= 60, "row overflowed: {row}");
        }
        // Nothing is lost: the joined rows rebuild the original text.
        let joined = flat(&lines)
            .join(" ")
            .replace("Description: ", "")
            .replace("            ", "")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let expected = command.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(joined, expected);
    }

    #[test]
    fn multi_line_description_is_flattened() {
        let lines = wrap_field(
            "Description: ",
            "run_command(git commit)\n  body line",
            80,
            Style::default(),
        );
        assert_eq!(lines.len(), 1);
        assert!(!flat(&lines)[0].contains('\n'));
    }

    #[test]
    fn raw_mode_keys_map_to_answers() {
        use crossterm::event::KeyCode;
        assert_eq!(
            PromptAnswer::from_key(KeyCode::Char('y')),
            PromptAnswer::Yes
        );
        assert_eq!(
            PromptAnswer::from_key(KeyCode::Char('A')),
            PromptAnswer::Always
        );
        // Enter takes the safe default; Esc aborts.
        assert_eq!(PromptAnswer::from_key(KeyCode::Enter), PromptAnswer::No);
        assert_eq!(PromptAnswer::from_key(KeyCode::Esc), PromptAnswer::Quit);
        assert!(matches!(
            PromptAnswer::from_key(KeyCode::Char('z')),
            PromptAnswer::Invalid(_)
        ));
    }

    #[test]
    fn cooked_mode_lines_are_parsed_leniently() {
        assert_eq!(PromptAnswer::parse(" y \n"), PromptAnswer::Yes);
        assert_eq!(PromptAnswer::parse("Always"), PromptAnswer::Always);
        assert_eq!(
            PromptAnswer::parse("\r\n"),
            PromptAnswer::Invalid(Some(String::new()))
        );
    }
}
